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
pub fn build_management_area(chain: &[MessageRecord], budget: &WindowConfig) -> String {
    // 收集所有带 ec 字段的记录（压缩产物）
    let blocks: Vec<&EcProjection> = chain.iter()
        .filter_map(|r| r.ec.as_ref())
        .collect();

    if blocks.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("[EC_CONTEXT rev={}]", blocks.len())); // rev 简化：用块数表示

    // 目录区：index_only 块
    let directory: Vec<&EcProjection> = blocks.iter()
        .filter(|b| b.state == BlockState::IndexOnly)
        .copied()
        .collect();
    let omitted_count = blocks.iter().filter(|b| b.state == BlockState::Omitted).count();
    if !directory.is_empty() || omitted_count > 0 {
        lines.push("[DIRECTORY]".to_string());
        for b in &directory {
            let title = if b.title.is_empty() { "(无标题)" } else { &b.title };
            lines.push(format!("{} {}", block_id_of(b), title));
        }
        if omitted_count > 0 {
            lines.push(format!("({omitted_count} 个块已滚出，可用 ec-recall 召回)"));
        }
        lines.push("[/DIRECTORY]".to_string());
    }

    // 精华区：所有块的 highlights 合并去重
    let mut highlights: Vec<String> = Vec::new();
    for b in &blocks {
        for h in &b.highlights {
            if !h.is_empty() && !highlights.contains(h) {
                highlights.push(h.clone());
            }
        }
    }
    // 按精华区预算裁剪（5K token 预算，粗估）
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
    if !kept.is_empty() {
        lines.push("[HIGHLIGHTS]".to_string());
        for h in &kept {
            lines.push(format!("- {h}"));
        }
        lines.push("[/HIGHLIGHTS]".to_string());
    }

    // 摘要区：summary 块
    let summaries: Vec<&EcProjection> = blocks.iter()
        .filter(|b| b.state == BlockState::Summary && !b.summary.is_empty())
        .copied()
        .collect();
    if !summaries.is_empty() {
        lines.push("[SUMMARIES]".to_string());
        for b in &summaries {
            let title = if b.title.is_empty() { "(无标题)" } else { &b.title };
            lines.push(format!("[[EC_BLOCK id={} title={title}]]", block_id_of(b)));
            lines.push(b.summary.clone());
            lines.push("[[/EC_BLOCK]]".to_string());
        }
        lines.push("[/SUMMARIES]".to_string());
    }

    lines.push("[/EC_CONTEXT]".to_string());
    lines.join("\n")
}

/// 块的标识（代理版没有独立 block_id，用"记录 id + state"作稳定标识）。
fn block_id_of(b: &EcProjection) -> String {
    // 简化：用摘要前 12 字符的 hash 作 id（稳定可复现）
    let seed = format!("{}:{}", b.title, b.summary);
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(seed.as_bytes());
    hex::encode(&hasher.finalize()[..4])[..8].to_string()
}

/// 估算管理区的 token 数。
pub fn estimate_management_tokens(chain: &[MessageRecord], budget: &WindowConfig) -> u32 {
    let text = build_management_area(chain, budget);
    if text.is_empty() {
        return 0;
    }
    (text.len() as f64 / 3.0).ceil() as u32
}

/// 投影尾部提示（每轮变化，放末尾天然不破坏前缀缓存）。
///
/// 内容 = 当前叶子（最后一条原生消息的 parent_id），供 recall CLI 定位。
/// 实际上定位文件已经承载了叶子信息（见 store.rs update_leaf），
/// 这里返回的提示仅用于 LLM 可读的说明（当前实现可不注入，保留接口）。
pub fn leaf_hint(leaf_id: &str) -> String {
    format!("leaf={leaf_id}")
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
        assert!(build_management_area(&[], &w).is_empty());
    }

    #[test]
    fn summary_block_renders() {
        let chain = vec![
            rec_with_ec("a", BlockState::Summary, "需求评估", "用户要求评估可行性", vec!["凭据".into()]),
        ];
        let w = WindowConfig::for_window(180_000);
        let text = build_management_area(&chain, &w);
        assert!(text.contains("EC_CONTEXT"));
        assert!(text.contains("SUMMARIES"));
        assert!(text.contains("需求评估"));
        assert!(text.contains("HIGHLIGHTS"));
        assert!(text.contains("凭据"));
    }
}
