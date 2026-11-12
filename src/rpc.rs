//! 最小 JSONL RPC 子集（对标 ww3 的 `protocol/jsonl` + `protocol/rpc`）。
//!
//! 当前阶段自实现，不依赖 ww3 内部 crate。后续可抽取为共享 `ww3-protocol` crate。
//!
//! ## 帧格式
//!
//! 每行一个 JSON 对象：
//! ```json
//! {"v":1,"id":"req-1","op":"invoke","method":"hook.stream","params":{...}}
//! {"v":1,"id":"req-1","op":"event","event":"reasoning.delta","data":{"content":"..."}}
//! {"v":1,"id":"req-1","op":"result","ok":true}
//! ```
//!
//! ## hook.stream 的双向流语义
//!
//! ```
//! core ──invoke(params)──▶ hook                 # 一次性下发 request/config/chain
//! core ──push(event,data)──▶ hook                # 逐事件推入（reasoning.delta / text.delta / ...）
//! core ──push(upstream.end,{ok})──▶ hook         # 结束信号，不转发给下游
//! hook ──event(event,data)──▶ core               # hook 处理后的事件（可改写/丢弃/合并）
//! hook ──result({ok:true})──▶ core               # hook 处理完毕，进程退出
//! ```
//!
//! hook 可在任意时刻返回 `result({"ok":false,"error":{...}})` 主动终止。
//! 终止事件由 core 统一发（W3-10：终止事件只出现一次）。

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

// ── 通用帧 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    #[serde(default)]
    pub v: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(rename = "op", default)]
    pub op: Op,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub method: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub event: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

/// 帧操作码
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Invoke,
    Push,      // core → hook：推送上游事件
    #[serde(alias = "event")]
    Event,     // hook → core：处理后的事件
    Result,
}

impl Default for Op {
    fn default() -> Self { Op::Invoke }
}

impl Frame {
    pub fn event(event: &str, data: impl Into<serde_json::Value>) -> Self {
        Self {
            v: 1,
            op: Op::Event,
            event: event.to_string(),
            data: data.into(),
            ..Default::default()
        }
    }

    pub fn result(ok: bool) -> Self {
        Self {
            v: 1,
            op: Op::Result,
            ok: Some(ok),
            ..Default::default()
        }
    }

    pub fn result_error(kind: &str, scope: &str, message: &str) -> Self {
        let mut f = Self::result(false);
        f.error = Some(serde_json::json!({
            "kind": kind,
            "scope": scope,
            "message": message,
        }));
        f
    }
}

// 缺省 frame 只填充版本号
impl Default for Frame {
    fn default() -> Self {
        Self {
            v: 1,
            id: String::new(),
            op: Op::default(),
            method: String::new(),
            params: serde_json::Value::Null,
            event: String::new(),
            data: serde_json::Value::Null,
            ok: None,
            error: None,
            result: None,
        }
    }
}

// ── IO ──

/// 读一封帧（不前置剥离 skip，因为首封一定是 invoke）。
pub fn read_frame(r: &mut impl BufRead) -> io::Result<Option<Frame>> {
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) => return Ok(None), // EOF
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed).map(Some).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("JSON 解析失败: {e}\n  行: {trimmed}"))
    })
}

/// 写一封帧。
pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()> {
    let line = serde_json::to_string(f).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("JSON 序列化失败: {e}"))
    })?;
    writeln!(w, "{line}")
}

// ── 服务入口 ──

/// serve_once 处理一次 `call` 调用：读取 invoke → 执行业务 → 写 result → 退出。
///
/// 返回退出码（0 成功，1 失败）。
///
/// 与 Go 侧 `plugin.ServeOnce` 对齐：handler 返回后必须写一条 `op: result` 帧，
/// 否则调用方（core）会读到 EOF 而误报「插件调用失败」。
/// serve_once 处理一次 `call` 调用：读取 invoke → 按 method 分发 → 写 result → 退出。
///
/// 支持两种方法（与 Go 侧 `plugin.Registry` 对齐）：
/// - `hook.stream`：双向流，stream_handler 循环处理 push 事件。
/// - `request.prepare`：一次性，prepare_handler 改写请求后直接返回 result。
///
/// 返回退出码（0 成功，1 失败）。
/// handler 返回后必须写一条 `op: result` 帧，否则调用方（core）会读到 EOF 而误报。
pub fn serve_once(
    stream_handler: fn(&mut Stream) -> i32,
    prepare_handler: fn(&str, &serde_json::Value) -> Result<serde_json::Value, String>,
) -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let reader = io::BufReader::new(stdin.lock());
    let writer = io::BufWriter::new(stdout);

    let mut stream = Stream {
        params: serde_json::Value::Null,
        reader,
        writer,
        req_id: String::new(),
    };

    // 读 invoke（通过 Stream 的 reader，预读数据不会丢）
    let invoke = match read_frame(&mut stream.reader) {
        Ok(Some(f)) if f.op == Op::Invoke => f,
        Ok(Some(f)) => {
            eprintln!("[ec-hook] 期待 invoke，收到 {:?}", f.op);
            return 1;
        }
        Ok(None) => {
            eprintln!("[ec-hook] stdin 在 invoke 前关闭");
            return 1;
        }
        Err(e) => {
            eprintln!("[ec-hook] 读取 invoke 失败: {e}");
            return 1;
        }
    };
    stream.params = invoke.params.clone();
    stream.req_id = invoke.id.clone();
    let req_id = invoke.id.clone();

    let exit_code = match invoke.method.as_str() {
        "hook.stream" => stream_handler(&mut stream),
        "request.prepare" => {
            // 一次性方法：直接调用 prepare handler，写 result
            match prepare_handler(&req_id, &invoke.params) {
                Ok(result_json) => {
                    let mut frame = Frame::result(true);
                    frame.id = req_id.clone();
                    // result 帧的 result 字段携带改写后的 request
                    frame.result = Some(result_json);
                    let _ = write_frame(&mut stream.writer, &frame);
                    let _ = stream.writer.flush();
                    0
                }
                Err(msg) => {
                    let mut frame = Frame::result_error("prompt_too_long", "request", &msg);
                    frame.id = req_id.clone();
                    let _ = write_frame(&mut stream.writer, &frame);
                    let _ = stream.writer.flush();
                    1
                }
            }
        }
        other => {
            eprintln!("[ec-hook] 未知方法: {other}");
            return 1;
        }
    };

    // hook.stream 的 result 帧在 handler 返回后写（request.prepare 已在上方写过）
    if invoke.method == "hook.stream" {
        let mut result = if exit_code == 0 {
            Frame::result(true)
        } else {
            Frame::result_error("server", "request", "hook 处理失败")
        };
        result.id = req_id;
        let _ = write_frame(&mut stream.writer, &result);
        let _ = stream.writer.flush();
    }
    exit_code
}

// ── Stream ──

/// hook.stream 双向流的抽象。
///
/// hook 调用 `next()` 读取 core 推送的事件帧，调用 `event()` 将处理后事件写回。
pub struct Stream {
    pub params: serde_json::Value,
    reader: io::BufReader<io::StdinLock<'static>>,
    writer: io::BufWriter<io::Stdout>,
    req_id: String,
}

/// core 推送的归一化事件（payload 已解析为字段）。
#[derive(Debug, Clone)]
pub struct PushEvent {
    pub event: String,      // reasoning.delta / text.delta / tool_call.delta / ... / upstream.end
    pub data: serde_json::Value,
}

impl Stream {
    /// 读取下一帧 core 推送的事件（阻塞）。
    ///
    /// 返回 `None` 表示 core 已关闭流（正常收尾）。
    /// 复用 Stream 持有的 reader——预读的数据不会丢（BufReader 的缓冲区跨调用保留）。
    pub fn next(&mut self) -> io::Result<Option<PushEvent>> {
        // 读前先冲刷 stdout buffer，确保 hook 已写的事件帧送达 core（半双工）
        self.writer.flush()?;
        match read_frame(&mut self.reader) {
            Ok(Some(f)) if f.op == Op::Push => {
                Ok(Some(PushEvent {
                    event: f.event.clone(),
                    data: f.data.clone(),
                }))
            }
            Ok(Some(f)) => {
                // 非 Push 帧：可能是 core 提前关流
                log::warn!("[ec-hook] next() 收到非 Push 帧: {:?}", f.op);
                Ok(None)
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 发一帧处理后的事件给 core。
    pub fn event(&mut self, event: &str, data: impl Into<serde_json::Value>) -> io::Result<()> {
        let mut frame = Frame::event(event, data);
        frame.id = self.req_id.clone(); // event 帧必须带 invoke 的 id（与 Go 侧 Emit 对齐）
        write_frame(&mut self.writer, &frame)
    }

    /// 发终止错误帧并结束 hook 处理（用于硬预算门超线等场景）。
    pub fn terminate(&mut self, kind: &str, scope: &str, message: &str) -> io::Result<i32> {
        write_frame(&mut self.writer, &Frame::result_error(kind, scope, message))?;
        // 不补 [DONE]——由 core 统一构造终止事件（W3-10 / R36）
        Ok(0)
    }
}

// ── 类型标记（与 ww3 一致） ──

/// errkind 子集（与 ww3 共享，见 IPC_CHAT §4.1）
pub mod errkind {
    pub const CONN: &str = "conn";
}