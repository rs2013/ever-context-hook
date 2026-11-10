//! core 客户端：spawn 独立 `ww3-core stdio` 子进程，走 stdio JSONL RPC 发 chat.generate。
//!
//! EC 的压缩/摘要请求用它——显式传 `--hooks none` 禁用所有拦截器（包括 EC 自己），
//! 避免压缩请求再经 ec hook 造成递归。其他参数（渠道/账号/模型）沿用默认配置文件。
//!
//! 协议（对齐 ww3 的 stdio JSONL RPC）：
//! ```text
//! 写: {"v":1,"id":"r1","op":"invoke","method":"chat.generate","params":{...}}
//! 读: {"v":1,"id":"r1","op":"event","event":"text.delta","data":{"content":"..."}}
//! 读: {"v":1,"id":"r1","op":"event","event":"finish_reason","data":{"reason":"stop"}}
//! 读: {"v":1,"id":"r1","op":"result","ok":true,"result":{...}}
//! ```

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// 一次 chat.generate 的上游 usage（对齐 protocol/chat/chat.go 的 Usage 帧）。
/// 缓存字段来自上游实报：cache_cached=命中、cache_write=写入；cache_reported 标记是否真报。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_cached: u64,
    pub cache_write: u64,
    /// 上游是否真报了缓存计量（false = 该渠道不给缓存信息，非 0 命中）。
    pub cache_reported: bool,
    pub time_ms: u64,
    pub first_token_ms: u64,
}

impl UsageInfo {
    /// 从 usage 事件 data 解析；缺字段按 0。
    pub fn from_event_data(d: &serde_json::Value) -> Self {
        let n = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_u64()).unwrap_or(0);
        let cache = d.get("cache");
        let cn = |k: &str| n(cache.and_then(|c| c.get(k)));
        Self {
            prompt_tokens: n(d.get("prompt_tokens")),
            completion_tokens: n(d.get("completion_tokens")),
            total_tokens: n(d.get("total_tokens")),
            cache_cached: cn("cached"),
            cache_write: cn("write"),
            cache_reported: cache.and_then(|c| c.get("cached_reported")).and_then(|v| v.as_bool()).unwrap_or(false),
            time_ms: n(d.get("time_ms")),
            first_token_ms: n(d.get("first_token_ms")),
        }
    }
}

/// 一次 chat.generate 的结果。
#[derive(Debug)]
pub struct ChatResult {
    /// 拼接后的正文（text.delta 累计）。
    pub text: String,
    /// finish_reason（stop / tool_calls / length ...）。
    pub finish_reason: String,
    /// 上游 usage（含缓存计量）；无 usage 帧则 None。
    pub usage: Option<UsageInfo>,
}

/// core stdio 客户端（一个进程可多次 chat）。
pub struct CoreClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    req_seq: u64,
}

impl CoreClient {
    /// spawn 一个 `ww3-core stdio` 子进程，禁用所有 hook。
    ///
    /// `core_exe`：ww3-core 可执行文件路径。
    /// `data`：core 数据目录（<data>/core）。
    /// `plugins`：插件目录。
    pub fn spawn(core_exe: &str, data: &str, plugins: &str) -> Result<Self, String> {
        let mut child = Command::new(core_exe)
            .args(["stdio", "--data", data, "--plugins", plugins, "--hooks", "none"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn core 失败: {e}"))?;
        let stdin = child.stdin.take().ok_or("取 core stdin 失败")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("取 core stdout 失败")?);
        Ok(Self { child, stdin, stdout, req_seq: 0 })
    }

    /// 发一次 chat.generate，阻塞到 result，返回拼接正文 + finish_reason。
    ///
    /// `messages`：OpenAI Chat 消息数组（serde_json::Value）。
    /// `thinking`：是否开启思考（压缩用 false，提升效率）。
    pub fn chat(&mut self, model: &str, messages: &[serde_json::Value], thinking: bool) -> Result<ChatResult, String> {
        self.req_seq += 1;
        let id = format!("ec-compress-{}", self.req_seq);
        let params = serde_json::json!({
            "model": model,
            "stream": true,
            "messages": messages,
            "thinking": thinking,
        });
        // 写 invoke 帧
        let invoke = serde_json::json!({
            "v": 1, "id": id, "op": "invoke", "method": "chat.generate", "params": params,
        });
        let line = serde_json::to_string(&invoke).map_err(|e| format!("序列化 invoke 失败: {e}"))?;
        self.stdin.write_all(line.as_bytes()).map_err(|e| format!("写 invoke 失败: {e}"))?;
        self.stdin.write_all(b"\n").map_err(|e| format!("写换行失败: {e}"))?;
        self.stdin.flush().map_err(|e| format!("flush 失败: {e}"))?;

        // 读 event 流直到 result
        let mut text = String::new();
        let mut finish_reason = String::new();
        let mut usage: Option<UsageInfo> = None;
        loop {
            let mut l = String::new();
            match self.stdout.read_line(&mut l) {
                Ok(0) => return Err("core 流在 result 前关闭".to_string()),
                Ok(_) => {}
                Err(e) => return Err(format!("读 core 响应失败: {e}")),
            }
            let trimmed = l.trim();
            if trimmed.is_empty() {
                continue;
            }
            let frame: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("解析 core 帧失败: {e}\n  {trimmed}"))?;
            match frame.get("op").and_then(|v| v.as_str()) {
                Some("event") => {
                    let event = frame.get("event").and_then(|v| v.as_str()).unwrap_or("");
                    match event {
                        "text.delta" => {
                            if let Some(c) = frame.get("data").and_then(|d| d.get("content")).and_then(|c| c.as_str()) {
                                text.push_str(c);
                            }
                        }
                        "finish_reason" => {
                            if let Some(r) = frame.get("data").and_then(|d| d.get("reason")).and_then(|r| r.as_str()) {
                                finish_reason = r.to_string();
                            }
                        }
                        "usage" => {
                            if let Some(d) = frame.get("data") {
                                usage = Some(UsageInfo::from_event_data(d));
                            }
                        }
                        "error" => {
                            let msg = frame.get("data").and_then(|d| d.get("message")).and_then(|m| m.as_str()).unwrap_or("unknown");
                            return Err(format!("core 报错: {msg}"));
                        }
                        _ => {}
                    }
                }
                Some("result") => {
                    let ok = frame.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if !ok {
                        let msg = frame.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).unwrap_or("unknown error");
                        return Err(format!("core result 失败: {msg}"));
                    }
                    return Ok(ChatResult { text, finish_reason, usage });
                }
                _ => {}
            }
        }
    }

    /// 优雅关闭（发 shutdown 或直接 kill）。
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for CoreClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_result_text_accumulates() {
        // 纯结构测试：不需要真 core
        let r = ChatResult { text: "a".to_string(), finish_reason: "stop".to_string(), usage: None };
        assert_eq!(r.text, "a");
        assert_eq!(r.finish_reason, "stop");
    }

    #[test]
    fn usage_info_parses_cache_event() {
        // 对齐 core 实测帧：{"prompt_tokens":36,...,"cache":{"cached":0,"cached_reported":true}}
        let d = serde_json::json!({
            "prompt_tokens": 36, "completion_tokens": 11, "total_tokens": 47,
            "cache": {"cached": 0, "cached_reported": true},
            "time_ms": 954, "first_token_ms": 954
        });
        let u = UsageInfo::from_event_data(&d);
        assert_eq!(u.prompt_tokens, 36);
        assert_eq!(u.total_tokens, 47);
        assert!(u.cache_reported);
        assert_eq!(u.cache_cached, 0);
        // 缺 cache 段：reported=false
        let u2 = UsageInfo::from_event_data(&serde_json::json!({"prompt_tokens": 5}));
        assert_eq!(u2.prompt_tokens, 5);
        assert!(!u2.cache_reported);
    }
}
