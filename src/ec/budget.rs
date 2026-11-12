//! 预算检查与 token 估算（对齐 Pi 插件版 `ec-budget.ts` + 代理版增量规则）。
//!
//! ## 触发层次
//!
//! 1. **硬预算门**（请求前）：projected_tokens > hard_budget → 拒绝/降级。
//! 2. **软阈值**（请求后）：projected_tokens > soft_threshold → 触发压缩。
//!
//! ## 关键规则：召回内容不计入增量
//!
//! recall 召回的内容（正文含 `[EC_RECALL_RESULT]` 标记）**必须跳过**，
//! 否则全文召回一次 +10K token 会立刻触发又一次压缩（无新内容，浪费 + 死循环）。

use crate::ec::identity::is_recall_result;
use crate::ec::types::ChatMessage;

/// 估算 prompt 的 token 数（粗估：字符数 ÷ 3）。
///
/// **跳过 recall 结果消息**（正文含 `[EC_RECALL_RESULT]` 标记）——它们的 token 不计入增量。
pub fn estimate_prompt_tokens(messages: &[ChatMessage]) -> u32 {
    let mut total = 0u32;
    for m in messages {
        // 跳过 recall 结果：不计入增量估算，避免召回内容触发压缩死循环
        if is_recall_result(m) {
            continue;
        }
        // 每条消息的基础开销（role + 格式）：约 4 token
        total += 4;
        total += m.content.estimated_tokens();
        for tc in &m.tool_calls {
            total += 10; // 工具调用的结构开销
            total += (tc.function.arguments.len() as f64 / 3.0).ceil() as u32;
        }
    }
    total
}

/// 锚定区 + 尾部贴合：返回 (锚定区结束下标, 尾部窗口起点)。
///
/// 首条 user 锚定区 [sys_end, anchor_end) 是 session_id 的基石，整段保留；
/// 锚定区成本先从预算中扣除，再对锚定区之后的消息做尾部贴合。
/// 两段拼接后即为降级后的消息序列（中间可能不连续）。
pub fn anchor_and_tail_start(messages: &[ChatMessage], sys_end: usize, budget_tokens: u32) -> (usize, usize) {
    let anchor_end = crate::ec::identity::leading_user_end(messages).max(sys_end);
    let anchor_tokens: u32 = messages[sys_end..anchor_end].iter().map(estimate_message_tokens).sum();
    let tail_room = budget_tokens.saturating_sub(anchor_tokens);
    let tail_start = tail_fit_start(&messages[anchor_end..], tail_room) + anchor_end;
    (anchor_end, tail_start)
}

/// 单条消息的 token 估算（含 4 token 固定开销）。
/// 跳过 recall 结果（不计入预算，避免召回内容触发压缩）。
pub fn estimate_message_tokens(m: &ChatMessage) -> u32 {
    if is_recall_result(m) {
        return 0;
    }
    let mut t = 4u32;
    t += m.content.estimated_tokens();
    for tc in &m.tool_calls {
        t += 10;
        t += (tc.function.arguments.len() as f64 / 3.0).ceil() as u32;
    }
    t
}

/// 估算单条 MessageRecord 的 token（正文 + 工具调用，粗估 utf8/3）。
///
/// 统一的持久化记录估算（index.rs / catchup.rs 共用，DRY）。
/// 跳过 recall 结果（正文含 `[EC_RECALL_RESULT]` 标记）。
pub fn estimate_record_tokens(r: &crate::ec::types::MessageRecord) -> u32 {
    if r.name.as_deref() == Some("ec_recall_result") || r.content.contains(crate::ec::identity::EC_RECALL_MARKER) {
        return 0;
    }
    let mut t = 4u32;
    t += (r.content.len() as f64 / 3.0).ceil() as u32;
    for tc in &r.tool_calls {
        t += 10;
        t += (tc.function.arguments.len() as f64 / 3.0).ceil() as u32;
    }
    t
}

/// 统一分段：把记录序列按 target_tokens 切成连续段（DRY，供 index_session / catchup 共用）。
///
/// 返回每段的 (start, end)（含）。规则：
/// - 从 start 向后累计 token，超过 target_tokens 且段内至少一条时收口；
/// - 段内至少一条（单条超预算也单独成段）；
/// - recall 结果 token 计 0（不计入切分预算）。
///
/// `records` 可为 `&[MessageRecord]` 的任意切片（全链或游标之后）。
/// 返回的下标相对于传入切片。空切片返回空。
pub fn segment_records<D: std::borrow::Borrow<crate::ec::types::MessageRecord>>(
    records: &[D],
    target_tokens: u32,
) -> Vec<(usize, usize)> {
    if records.is_empty() {
        return Vec::new();
    }
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut acc = 0u32;
    for i in 0..records.len() {
        let cost = estimate_record_tokens(records[i].borrow());
        if acc + cost > target_tokens && i > start {
            segments.push((start, i - 1));
            start = i;
            acc = 0;
        }
        acc += cost;
    }
    segments.push((start, records.len() - 1));
    segments
}

/// 尾部贴合：从消息数组尾部向前累积 token，直到预算用尽，返回窗口起点下标（含）。
///
/// 预算不足放任何一条时返回 len（空窗口，调用方只保留前置 system + 管理区 + 提示）。
/// 对齐 Pi 版 `ec-blocks.ts` 的 `tailFitStartIndex`：
/// - 从 to 向前累加，单条超预算就停（宁可空窗也不超线）
/// - 工具链对齐（opener 与 closer 被窗口切开时继续丢最旧）由调用方负责，这里只做控量
pub fn tail_fit_start(messages: &[ChatMessage], budget_tokens: u32) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut start = messages.len();
    let mut acc = 0u32;
    for i in (0..messages.len()).rev() {
        let cost = estimate_message_tokens(&messages[i]);
        if acc + cost > budget_tokens {
            break;
        }
        acc += cost;
        start = i;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::types::MessageContent;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: MessageContent::Text(content.to_string()),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }
    }

    fn recall_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool_result".to_string(),
            content: MessageContent::Text(content.to_string()),
            tool_calls: vec![],
            tool_call_id: Some("call_x".to_string()),
            name: None,
        }
    }

    #[test]
    fn test_estimate_skips_recall() {
        // 新形态：bash 调 recall 的产出，靠正文标记识别（不再依赖 name 字段）
        let msgs = vec![
            msg("user", "帮我写函数"),
            recall_msg("[EC_RECALL_RESULT]\n找到 1 条……\n[/EC_RECALL_RESULT]"),
        ];
        let tokens = estimate_prompt_tokens(&msgs);
        // recall 消息被跳过，只剩 user 消息（4 + content/3）
        assert_eq!(tokens, 4 + msg("user", "帮我写函数").content.estimated_tokens());
    }

    #[test]
    fn test_anchor_and_tail_start_keeps_anchor() {
        let msgs = vec![
            msg("system", "sys"),
            msg("user", "首条用户问题，作为 session 锚定区"),
            msg("assistant", "a1"),
            msg("user", "u2"),
            msg("assistant", "a2"),
            msg("user", "u3"),
        ];
        // sys_end=1；预算 8 连锚定区都放不下，但锚定区必须整段保留。
        let (anchor_end, tail_start) = anchor_and_tail_start(&msgs, 1, 8);
        assert_eq!(anchor_end, 2, "锚定区应结束于首条 user 之后");
        assert_eq!(msgs[1].role, "user");
        assert!(tail_start >= anchor_end, "尾部窗口不得回退进锚定区");
    }
}
