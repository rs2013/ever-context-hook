//! 管理区投影构建（代理版）。
//!
//! 与 Pi 插件版的 `ec-projection.ts` 不同：代理版的投影产物**挂在祖先链的记录上**
//! （`MessageRecord.ec` 字段），本模块从祖先链渲染管理区。
//!
//! ## 投影结构
//!
//! ```
//! [EC_CONTEXT session=<id> rev=<n>]
//! [DIRECTORY]
//! <block_id> <title>
//! [/DIRECTORY]
//! [HIGHLIGHTS]
//! - <精华>
//! [/HIGHLIGHTS]
//! [SUMMARIES]
//! [[EC_BLOCK id=<id> title=<title>]]
//! <summary>
//! [[/EC_BLOCK]]
//! [/SUMMARIES]
//! [/EC_CONTEXT]
//! ```
//!
//! 其中：
//! - `[DIRECTORY]` = index_only 块（一行 = 索引 ID + 标题）
//! - `[HIGHLIGHTS]` = 所有块的精华合并去重
//! - `[SUMMARIES]` = summary 块（标题 + 摘要全文）
//! - omitted 块不进管理区，但会有一条"已滚出"提示

use crate::ec::types::{BlockState, EcProjection, MessageRecord, WindowConfig};

/// 从祖先链渲染管理区文本。
///
/// `chain` = 从根到当前叶子的消息记录（含 `ec` 字段）。
/// 返回空串表示无需管理区（没有压缩块）。
pub fn build_management_area(chain: &[MessageRecord], budget: &WindowConfig, session_id: &str) -> String {
    // 收集所有带 ec 字段的记录（压缩产物）：(记录 id, 投影)。
    // block_id 直接用记录 id（message_id，内容寻址稳定），不再用内容 hash（bug ⑥ 修复）。
    let blocks: Vec<(&str, &EcProjection)> = chain.iter()
        .filter_map(|r| r.ec.as_ref().map(|ec| (r.id.as_str(), ec)))
        .collect();

    if blocks.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("[EC_CONTEXT session={session_id} rev={}]", blocks.len())); // session 稳定，可安全进头部

    // 目录区：index_only 块，按目录预算裁剪（汰出最旧，保留最新）
    let directory: Vec<(&str, &EcProjection)> = blocks.iter()
        .filter(|(_, b)| b.state == BlockState::IndexOnly)
        .map(|(id, b)| (*id, *b))
        .collect();
    let omitted_count = blocks.iter().filter(|(_, b)| b.state == BlockState::Omitted).count();
    let directory_budget = budget.budget.directory_budget;
    let mut dir_kept: Vec<(&str, &EcProjection)> = Vec::new();
    let mut dir_acc = 0u32;
    for (id, b) in directory.iter().rev() {
        let title = if b.title.is_empty() { "(无标题)" } else { &b.title };
        let cost = (title.len() as f64 / 3.0).ceil() as u32 + 12; // 标题 + id + 行开销
        if dir_acc + cost <= directory_budget {
            dir_acc += cost;
            dir_kept.push((*id, *b));
        }
    }
    dir_kept.reverse();
    let dir_omitted = directory.len() - dir_kept.len();
    if !dir_kept.is_empty() || omitted_count > 0 || dir_omitted > 0 {
        lines.push("[DIRECTORY]".to_string());
        for (id, b) in &dir_kept {
            let title = if b.title.is_empty() { "(无标题)" } else { &b.title };
            lines.push(format!("{id} {title}"));
        }
        if dir_omitted > 0 {
            lines.push(format!("(部分标题已汰出，可调用 ec_recall directory --session {session_id} 获取完整目录)"));
        }
        if omitted_count > 0 {
            lines.push(format!("({omitted_count} 个块已滚出，可用 ec-recall 召回)"));
        }
        lines.push("[/DIRECTORY]".to_string());
    }

    // 精华区：summary + index_only 块的 highlights 合并去重（omitted 完全滚出投影，不进管理区）
    let mut highlights: Vec<String> = Vec::new();
    for (_, b) in &blocks {
        if b.state == BlockState::Omitted {
            continue; // 仅可召回，不进管理区精华区（精华仍在 JSONL 可 recall highlights 找回）
        }
        for h in &b.highlights {
            if !h.is_empty() && !highlights.contains(h) {
                highlights.push(h.clone());
            }
        }
    }
    // 按精华区预算裁剪（5K token 预算，粗估，汰出最旧保留最新）
    let highlights_budget = budget.budget.highlights_budget;
    let mut kept: Vec<String> = Vec::new();
    let mut acc = 0u32;
    for h in highlights.iter().rev() {
        let cost = (h.len() as f64 / 3.0).ceil() as u32 + 4;
        if acc + cost <= highlights_budget {
            acc += cost;
            kept.push(h.clone());
        }
    }
    kept.reverse();
    let highlights_omitted = highlights.len() - kept.len();
    if !kept.is_empty() {
        lines.push("[HIGHLIGHTS]".to_string());
        for h in &kept {
            lines.push(format!("- {h}"));
        }
        if highlights_omitted > 0 {
            lines.push(format!("(部分精华已汰出，可调用 ec_recall highlights --session {session_id} 获取完整精华区)"));
        }
        lines.push("[/HIGHLIGHTS]".to_string());
    }

    // 摘要区：summary 块
    let summaries: Vec<(&str, &EcProjection)> = blocks.iter()
        .filter(|(_, b)| b.state == BlockState::Summary && !b.summary.is_empty())
        .map(|(id, b)| (*id, *b))
        .collect();
    if !summaries.is_empty() {
        lines.push("[SUMMARIES]".to_string());
        for (id, b) in &summaries {
            let title = if b.title.is_empty() { "(无标题)" } else { &b.title };
            lines.push(format!("[[EC_BLOCK id={id} title={title}]]"));
            lines.push(b.summary.clone());
            lines.push("[[/EC_BLOCK]]".to_string());
        }
        lines.push("[/SUMMARIES]".to_string());
    }

    lines.push("[/EC_CONTEXT]".to_string());
    lines.join("\n")
}

/// 估算管理区的 token 数。
pub fn estimate_management_tokens(chain: &[MessageRecord], budget: &WindowConfig, session_id: &str) -> u32 {
    let text = build_management_area(chain, budget, session_id);
    if text.is_empty() {
        return 0;
    }
    (text.len() as f64 / 3.0).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::types::MessageRecord;

    fn rec_with_ec(id: &str, state: BlockState, title: &str, summary: &str, highlights: Vec<String>) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            parent_id: None,
            role: "tool_result".to_string(),
            timestamp: None, model: None, channel: None, thinking: None, reasoning_effort: None,
            usage: None, stop_reason: None, reasoning: None, duration_ms: None,
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            name: None,
            ec: Some(EcProjection { state, title: title.to_string(), summary: summary.to_string(), highlights }),
        }
    }

    #[test]
    fn empty_chain_no_management() {
        let w = WindowConfig::for_window(180_000);
        assert!(build_management_area(&[], &w, "sess").is_empty());
    }

    #[test]
    fn summary_block_renders() {
        let chain = vec![
            rec_with_ec("a", BlockState::Summary, "需求评估", "用户要求评估可行性", vec!["凭据".into()]),
        ];
        let w = WindowConfig::for_window(180_000);
        let text = build_management_area(&chain, &w, "sess");
        assert!(text.contains("EC_CONTEXT"));
        assert!(text.contains("SUMMARIES"));
        assert!(text.contains("需求评估"));
        assert!(text.contains("HIGHLIGHTS"));
        assert!(text.contains("凭据"));
    }
}
