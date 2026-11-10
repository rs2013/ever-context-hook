//! ec-recall CLI 的搜索逻辑（代理版）。
//!
//! ## 搜索范围：祖先链（叶子到根）
//!
//! 从定位文件读叶子 → 沿 parent_id 上溯得到祖先链 → 在链上搜索。
//! **不搜兄弟分支**（那是不同时间线）。
//!
//! ## 实现（方案 B：Tantivy 粗筛 + 内存过滤）
//!
//! 1. Tantivy 全文粗筛：session_id + doc_type + 自由文本（title/summary）。
//! 2. 内存精确过滤：祖先链 message_id 集合，只保留当前分支上的段。
//!
//! 索引目录：`<data_root>/index`。

use crate::ec::index::{self};
use crate::ec::store::Store;

/// 召回命中（过滤后）。
#[derive(Debug, Clone)]
pub struct RecallHit {
    pub block_id: String,
    pub title: String,
    pub summary: String,
    pub score: f32,
}

/// 在会话的祖先链上搜索（Tantivy 粗筛 + 分支过滤）。
///
/// `session_id` 定位 JSONL 与索引，从定位文件读叶子，沿 parent_id 上溯祖先链。
/// 返回按分数降序的命中列表（最多 limit 条）。
pub fn search(session_id: &str, query: &str, limit: usize, data_root: &str) -> Vec<RecallHit> {
    let store = Store::new(data_root);
    let leaf = match store.read_leaf(session_id) {
        Some(l) => l,
        None => return Vec::new(), // 无叶子：新会话或数据缺失
    };
    let chain = match store.load_chain(session_id, &leaf) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // 祖先链 message_id 集合（分支过滤白名单）
    let chain_ids: std::collections::HashSet<String> = chain.iter().map(|r| r.id.clone()).collect();

    // Tantivy 粗筛
    let index_dir = std::path::Path::new(data_root).join("index");
    let index = match index::open_index(&index_dir) {
        Ok(i) => i,
        Err(_) => return Vec::new(), // 索引不可用：无结果（fail-open，不阻断）
    };
    let hits = match index::search(&index, session_id, "ec_block", query, limit * 3) {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };

    // 内存过滤：只保留当前分支上的段
    hits.into_iter()
        .filter(|h| chain_ids.contains(&h.message_id))
        .take(limit)
        .map(|h| RecallHit {
            block_id: h.message_id,
            title: if h.title.is_empty() { "(无标题)".to_string() } else { h.title },
            summary: h.summary,
            score: h.score,
        })
        .collect()
}

/// 在给定的祖先链上做纯内存搜索（降级路径：索引不可用时）。
pub fn search_chain(chain: &[crate::ec::types::MessageRecord], query: &str, limit: usize) -> Vec<RecallHit> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<RecallHit> = Vec::new();
    for rec in chain {
        if let Some(ec) = &rec.ec {
            let text = format!("{} {} {}", ec.title, ec.summary, ec.highlights.join(" "));
            let score = score_text(&text, query);
            if score > 0.0 {
                hits.push(RecallHit {
                    block_id: rec.id.clone(),
                    title: if ec.title.is_empty() { "(无标题)".to_string() } else { ec.title.clone() },
                    summary: text,
                    score,
                });
            }
        }
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit.max(1));
    hits
}

/// 简单评分：query 的字符在 text 中的覆盖率 + 词命中率。
fn score_text(text: &str, query: &str) -> f32 {
    let text_lower = text.to_lowercase();
    let query_lower = query.to_lowercase();
    let query_chars: Vec<char> = query_lower.chars().filter(|c| !c.is_whitespace()).collect();
    if query_chars.is_empty() {
        return 0.0;
    }
    let mut matched = 0;
    for c in &query_chars {
        if text_lower.contains(*c) {
            matched += 1;
        }
    }
    let char_ratio = matched as f32 / query_chars.len() as f32;
    let word_hits: u32 = query_lower
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_ascii_alphabetic()))
        .filter(|w| text_lower.contains(w))
        .count() as u32;
    char_ratio + word_hits as f32 * 0.1
}

fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().take(max_chars).collect();
    let mut out: String = chars.into_iter().collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// 格式化命中为 CLI 输出文本。
pub fn format_hits(hits: &[RecallHit]) -> String {
    if hits.is_empty() {
        return "未找到相关历史内容。".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!("找到 {} 条相关上下文：\n\n", hits.len()));
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("### {} ({})\n{}\n\n", h.title, h.block_id, h.summary));
        if i >= 4 {
            out.push_str("...\n");
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::types::{BlockState, EcProjection, MessageRecord};

    fn rec(id: &str, content: &str) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            parent_id: None,
            role: "user".to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            name: None,
            ec: None,
        }
    }

    fn rec_with_ec(id: &str, title: &str, summary: &str) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            parent_id: None,
            role: "tool_result".to_string(),
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            name: None,
            ec: Some(EcProjection {
                state: BlockState::Summary,
                title: title.to_string(),
                summary: summary.to_string(),
                highlights: vec![],
            }),
        }
    }

    #[test]
    fn search_finds_summary() {
        let chain = vec![
            rec("root", "帮我写个函数"),
            rec_with_ec("b1", "凭据配置", "认证凭据是 ghp_xxx"),
        ];
        let hits = search_chain(&chain, "凭据", 5);
        assert!(!hits.is_empty());
        assert!(hits[0].summary.contains("ghp_xxx"));
    }

    #[test]
    fn search_scores_cjk() {
        let chain = vec![rec_with_ec("root", "滚动上下文管理器", "管理超长任务的上下文")];
        let hits = search_chain(&chain, "上下文", 5);
        assert!(!hits.is_empty());
    }
}
