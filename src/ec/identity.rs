//! 会话寻址与消息身份（EC 代理版的核心算法）。
//!
//! ## 设计要点（对齐 `双上下文持久化与召回设计.md`）
//!
//! - **会话 ID** = 首条 user 全文的 SHA-256 截断（16 hex）。不信任下游传来的任何 ID。
//! - **message_id** = SHA-256(feature_text + parent_id) 截断（16 hex）。
//!   parent_id 纳入哈希，从根上消除"重复内容碰撞"（如连续两个 "继续"）。
//! - **feature_text** = 按角色拼接的规范化文本，是哈希的内容来源。
//! - 树状分叉：每个消息的 parent_id 指向前一条消息的 message_id。
//!
//! ## 与 BC 的差异
//!
//! BC 的 `deriveMessageId` 哈希 `role|contentType|toolCallId|toolName|text`（不含 parent），
//! 重复内容靠实例重编号（_1/_2）解决。EC 把 parent_id 纳入哈希，更简单且天然支持树。

use crate::ec::types::{ChatMessage, MessageRecord};
use sha2::{Digest, Sha256};

/// 会话 ID 的 hex 截断长度。
pub const SESSION_ID_HEX: usize = 16;
/// message_id 的 hex 截断长度。
pub const MESSAGE_ID_HEX: usize = 16;

/// 计算「首条 user 锚定区」的结束下标（不含）。
///
/// 首条 user = 首个 assistant 之前的所有 user（含宿主注入的 agents.md/技能列表等以 user 形态出现的提示词）。
/// 实现：从 0 开始，`system`/`developer`/`user` 都计入锚定区，直到首个其它角色
/// （assistant / tool / tool_result 等）为止。无 user 消息时返回 0。
pub fn leading_user_end(messages: &[ChatMessage]) -> usize {
    let mut end = 0usize;
    for m in messages {
        match m.role.as_str() {
            "system" | "developer" | "user" => end += 1,
            _ => break,
        }
    }
    end
}

/// 计算「首条 user 锚定区」的拼接全文（用于会话 ID）。
///
/// 即 [0, leading_user_end) 区间内所有 role == "user" 消息的正文，用 "\n" 拼接。
/// 若无 user 消息（理论上不会），返回空串。
pub fn leading_user_anchor(messages: &[ChatMessage]) -> String {
    messages[..leading_user_end(messages)]
        .iter()
        // 排除 EC 注入的投影消息（ec_management/ec_meta），
        // 否则下游把改写后的 request 回传时会污染锚定区、导致 session_id 漂移。
        .filter(|m| m.role == "user" && !is_projection_message(m))
        .map(|m| m.content.extract_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 计算会话 ID = 首条 user 锚定区的 SHA-256 截断。
///
/// 首条 user 定义为「首个 assistant 之前的所有 user」的拼接（含 dsh 注入的
/// agents.md/技能列表等 user 形态提示词），而非仅第一条 user。
/// 若无 user 消息（理论上不会），返回空串。
pub fn session_id(messages: &[ChatMessage]) -> String {
    let anchor = leading_user_anchor(messages);
    if anchor.is_empty() {
        String::new()
    } else {
        sha16(anchor.as_bytes())
    }
}

/// 计算单条消息的 message_id = SHA-256(feature_text + parent_id)。
///
/// `parent_id` 为 None 表示根消息（首条 user）。
pub fn message_id(feature_text: &str, parent_id: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(feature_text.as_bytes());
    // 用分隔符区分 "feature_text=A, parent=B" 与 "feature_text=A+B, parent=''"
    hasher.update(b"\x00");
    hasher.update(parent_id.unwrap_or("").as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8])[..MESSAGE_ID_HEX].to_string()
}

/// 计算 feature_text（规范化文本，用于 message_id 哈希）。
///
/// 规则（对齐设计文档 §2.1）：
/// - user：`"user:" + content`
/// - assistant 无工具调用：`"assistant:" + content`
/// - assistant 有工具调用：`"assistant:" + tool_calls[].name + arguments`（多个用 \x1f 分隔）
/// - tool_result：`"tool_result:" + tool_call_id + ":" + content`
pub fn feature_text(m: &ChatMessage) -> String {
    match m.role.as_str() {
        "user" => format!("user:{}", m.content.extract_text()),
        "assistant" => {
            if m.tool_calls.is_empty() {
                format!("assistant:{}", m.content.extract_text())
            } else {
                let calls: Vec<String> = m.tool_calls.iter()
                    .map(|tc| format!("{}|{}", tc.function.name, tc.function.arguments))
                    .collect();
                format!("assistant:{}", calls.join("\x1f"))
            }
        }
        "tool" => {
            // OpenAI 的 tool 角色（tool_result 的 wire 形式）
            let call_id = m.tool_call_id.clone().unwrap_or_default();
            format!("tool_result:{}:{}", call_id, m.content.extract_text())
        }
        "tool_result" => {
            let call_id = m.tool_call_id.clone().unwrap_or_default();
            format!("tool_result:{}:{}", call_id, m.content.extract_text())
        }
        _ => format!("{}:{}", m.role, m.content.extract_text()),
    }
}

/// 从 MessageRecord 重建 feature_text（用于定位时匹配 JSONL 行）。
///
/// MessageRecord 是持久化形态，字段名与 ChatMessage 略不同（tool_calls 是 ToolCallRecord）。
pub fn feature_text_of_record(r: &MessageRecord) -> String {
    match r.role.as_str() {
        "user" => format!("user:{}", r.content),
        "assistant" => {
            if r.tool_calls.is_empty() {
                format!("assistant:{}", r.content)
            } else {
                let calls: Vec<String> = r.tool_calls.iter()
                    .map(|tc| format!("{}|{}", tc.function.name, tc.function.arguments))
                    .collect();
                format!("assistant:{}", calls.join("\x1f"))
            }
        }
        "tool" | "tool_result" => {
            let call_id = r.tool_call_id.clone().unwrap_or_default();
            format!("tool_result:{}:{}", call_id, r.content)
        }
        _ => format!("{}:{}", r.role, r.content),
    }
}

/// 判断消息是否为"原生消息"（不是 EC 注入的投影消息）。
///
/// EC 注入的投影消息带 name 字段标记（ec_meta / ec_management）。
pub fn is_projection_message(m: &ChatMessage) -> bool {
    matches!(m.name.as_deref(), Some("ec_meta") | Some("ec_management"))
}

/// 判断消息是否为 recall 结果（增量估算与压缩时需跳过）。
///
/// 两种形态都认：
/// - 旧形态：`name == "ec_recall_result"`（早期 function calling 残留，向后兼容）；
/// - 新形态：正文含 `[EC_RECALL_RESULT]` 标记（模型通过 bash 调 `ec-hook recall` 的产出）。
pub fn is_recall_result(m: &ChatMessage) -> bool {
    m.name.as_deref() == Some("ec_recall_result") || m.content.extract_text().contains(EC_RECALL_MARKER)
}

/// recall 输出的包裹标记（与 `recall::format_hits` 一致）。
pub const EC_RECALL_MARKER: &str = "[EC_RECALL_RESULT]";

/// 从 ChatMessage 构造 MessageRecord（用于 append 到 JSONL）。
pub fn to_record(m: &ChatMessage, parent_id: Option<&str>) -> MessageRecord {
    let ft = feature_text(m);
    let id = message_id(&ft, parent_id);
    let (tool_calls, tool_call_id, tool_name) = match m.role.as_str() {
        "assistant" => (
            m.tool_calls.iter().map(|tc| crate::ec::types::ToolCallRecord {
                id: tc.id.clone(),
                type_: tc.type_.clone(),
                function: crate::ec::types::ToolCallFunction {
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                },
            }).collect(),
            None,
            None,
        ),
        "tool" | "tool_result" => (
            vec![],
            m.tool_call_id.clone(),
            // OpenAI tool 消息没有 name 字段承载工具名；tool_result wire 形式也一样。
            // 工具名暂从调用方传入（见 to_record_with_tool_name）。
            None,
        ),
        _ => (vec![], None, None),
    };
    MessageRecord {
        id,
        parent_id: parent_id.map(|s| s.to_string()),
        role: m.role.clone(),
        timestamp: None,
        model: None,
        channel: None,
        thinking: None,
        reasoning_effort: None,
        usage: None,
        stop_reason: None,
        reasoning: None,
        duration_ms: None,
        content: m.content.extract_text(),
        tool_calls,
        tool_call_id,
        tool_name,
        name: m.name.clone(),
        ec: None,
    }
}

fn sha16(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(&result[..8])[..SESSION_ID_HEX].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::types::{MessageContent, ToolCall, ToolCallFunction};

    fn user(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Text(content.to_string()),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }
    }

    fn assistant(content: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: MessageContent::Text(content.to_string()),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn session_id_is_first_user_sha() {
        let msgs = vec![
            ChatMessage { role: "system".to_string(), content: MessageContent::Text("sys".into()), tool_calls: vec![], tool_call_id: None, name: None },
            user("帮我写个函数"),
            assistant("好的"),
        ];
        // 首条 user = "帮我写个函数"
        let sid = session_id(&msgs);
        assert_eq!(sid, sha16("帮我写个函数".as_bytes()));
        assert_eq!(sid.len(), 16);
    }

    #[test]
    fn session_id_uses_all_users_before_first_assistant() {
        // dsh 形态：[system, user "hi", user "agents.md", user "技能列表", assistant, ...]
        let msgs = vec![
            ChatMessage { role: "system".to_string(), content: MessageContent::Text("sys".into()), tool_calls: vec![], tool_call_id: None, name: None },
            user("hi"),
            user("agents.md 项目规范"),
            user("技能列表"),
            assistant("好的"),
            user("继续"),
        ];
        // 首条 user 锚定区 = 首个 assistant 之前的 3 条 user 拼接
        assert_eq!(leading_user_end(&msgs), 4);
        assert_eq!(leading_user_anchor(&msgs), "hi\nagents.md 项目规范\n技能列表");
        assert_eq!(session_id(&msgs), sha16("hi\nagents.md 项目规范\n技能列表".as_bytes()));
    }

    #[test]
    fn leading_user_end_stops_at_first_non_system_user() {
        let msgs = vec![
            ChatMessage { role: "developer".to_string(), content: MessageContent::Text("dev".into()), tool_calls: vec![], tool_call_id: None, name: None },
            user("hi"),
            ChatMessage { role: "tool_result".to_string(), content: MessageContent::Text("out".into()), tool_calls: vec![], tool_call_id: Some("c1".into()), name: None },
            user("later"),
        ];
        // 首个非 system/developer/user 是 tool_result，锚定区止于它之前
        assert_eq!(leading_user_end(&msgs), 2);
        assert_eq!(leading_user_anchor(&msgs), "hi");
    }

    #[test]
    fn session_id_ignores_projection_messages() {
        // ec_management 等投影 user 消息不得进入锚定区，否则下游回传改写请求时 session_id 漂移。
        let plain = vec![user("hi"), user("second")];
        let injected = vec![
            ChatMessage { role: "user".to_string(), content: MessageContent::Text("[EC_CONTEXT ...]".into()), tool_calls: vec![], tool_call_id: None, name: Some("ec_management".into()) },
            user("hi"),
            user("second"),
        ];
        assert_eq!(session_id(&plain), session_id(&injected));
    }

    #[test]
    fn message_id_differs_for_duplicate_content() {
        // 两个内容相同的 user，但 parent 不同 → id 不同
        let a = message_id("user:继续", None);
        let b = message_id("user:继续", Some("some-parent-id"));
        assert_ne!(a, b);
    }

    #[test]
    fn message_id_stable_for_same_inputs() {
        let a = message_id("user:继续", Some("p1"));
        let b = message_id("user:继续", Some("p1"));
        assert_eq!(a, b);
    }

    #[test]
    fn feature_text_distinguishes_roles() {
        assert_eq!(feature_text(&user("hi")), "user:hi");
        assert_eq!(feature_text(&assistant("hi")), "assistant:hi");
    }

    #[test]
    fn feature_text_for_tool_calls() {
        let m = ChatMessage {
            role: "assistant".to_string(),
            content: MessageContent::Text(String::new()),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                type_: "function".to_string(),
                function: ToolCallFunction { name: "bash".to_string(), arguments: "{\"cmd\":\"ls\"}".to_string() },
            }],
            tool_call_id: None,
            name: None,
        };
        assert_eq!(feature_text(&m), "assistant:bash|{\"cmd\":\"ls\"}");
    }

    #[test]
    fn is_projection_message_detects_ec_meta() {
        let mut m = user("x");
        assert!(!is_projection_message(&m));
        m.name = Some("ec_meta".to_string());
        assert!(is_projection_message(&m));
    }
}
