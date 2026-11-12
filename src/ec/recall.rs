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
    /// 命中段原文（verbatim JSONL 行，回读自 path/start_line/end_line）。
    /// 未回读（降级路径、目录/精华召回）时为空。
    pub raw: String,
    /// 命中段在 JSONL 的行区间（原文回读来源）。
    pub start_line: u64,
    pub end_line: u64,
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

    // Tantivy 粗筛：候选量迭代放大。兄弟分支的命中会被白名单过滤，
    // 固定 limit*3 可能在多分支场景下把当前分支结果挤掉，故按需倍增。
    let index_dir = std::path::Path::new(data_root).join("index");
    let mut hits: Vec<RecallHit> = Vec::new();
    if let Ok(index) = index::open_index(&index_dir) {
        let mut fetch = limit.max(1).saturating_mul(3);
        loop {
            let fetched = match index::search(&index, session_id, "ec_block", query, fetch) {
                Ok(h) => h,
                Err(_) => break, // 索引查询失败：退回内存降级路径
            };
            let got = fetched.len();
            // 命中后回读 JSONL 祖先链取权威 title/summary（索引侧只索引不 STORED）
            hits = fetched.into_iter()
                .filter(|h| chain_ids.contains(&h.message_id))
                .filter_map(|h| {
                    let rec = chain.iter().find(|r| r.id == h.message_id)?;
                    let ec = rec.ec.as_ref()?;
                    Some(RecallHit {
                        block_id: h.message_id.clone(),
                        title: if ec.title.is_empty() { "(无标题)".to_string() } else { ec.title.clone() },
                        summary: ec.summary.clone(),
                        raw: read_raw_block(&store, &h),
                        start_line: h.start_line,
                        end_line: h.end_line,
                        score: h.score,
                    })
                })
                .take(limit)
                .collect();
            if hits.len() >= limit || got < fetch || fetch >= 100_000 {
                break;
            }
            fetch = fetch.saturating_mul(2);
        }
    }
    // 索引不可用或无命中：走纯内存降级（search_chain 已实现，接入主流程）。
    if hits.is_empty() {
        return search_chain(&chain, query, limit);
    }
    hits
}

/// 召回完整标题目录（祖先链上所有压缩块的 block_id + title，不限状态）。
///
/// 相对投影里被目录预算裁剪的目录区，这里返回**完整**目录，
/// 供汰出后由 LLM 通过 `ec_recall directory` 找回。
pub fn directory(session_id: &str, data_root: &str) -> Vec<RecallHit> {
    let store = Store::new(data_root);
    let leaf = match store.read_leaf(session_id) {
        Some(l) => l,
        None => return Vec::new(),
    };
    let chain = match store.load_chain(session_id, &leaf) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    chain.iter()
        .filter_map(|r| r.ec.as_ref().map(|ec| (r.id.clone(), ec)))
        .map(|(id, ec)| RecallHit {
            block_id: id,
            title: if ec.title.is_empty() { "(无标题)".to_string() } else { ec.title.clone() },
            summary: String::new(),
            raw: String::new(),
            start_line: 0,
            end_line: 0,
            score: 0.0,
        })
        .collect()
}

/// 召回完整精华区（祖先链上所有块的 highlights 合并去重，不裁剪）。
pub fn highlights(session_id: &str, data_root: &str) -> Vec<String> {
    let store = Store::new(data_root);
    let leaf = match store.read_leaf(session_id) {
        Some(l) => l,
        None => return Vec::new(),
    };
    let chain = match store.load_chain(session_id, &leaf) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for r in &chain {
        if let Some(ec) = &r.ec {
            for h in &ec.highlights {
                if !h.is_empty() && !out.contains(h) {
                    out.push(h.clone());
                }
            }
        }
    }
    out
}

/// 跨会话知识检索（doc_type="knowledge"，不受 session_id 限制）。
///
/// 需求 ⑥：knowledge 用于检索其它对话和全局知识库，不受 active=true 限制。
/// 代理版无 active 字段，knowledge 即全局命名空间：session_id 传空，
/// 检索所有会话写入的知识条目。
pub fn search_knowledge(query: &str, limit: usize, data_root: &str) -> Vec<RecallHit> {
    let index_dir = std::path::Path::new(data_root).join("index");
    let index = match index::open_index(&index_dir) {
        Ok(i) => i,
        Err(_) => return Vec::new(),
    };
    let hits = match index::search(&index, "", "knowledge", query, limit) {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    hits.into_iter()
        .map(|h| {
            // knowledge 正文从 doc_text 拆（title\nsummary），doc_key 作标识
            let (title, summary) = match h.doc_text.split_once('\n') {
                Some((t, s)) => (t.to_string(), s.to_string()),
                None => (h.doc_text.clone(), String::new()),
            };
            RecallHit {
                block_id: if h.doc_key.is_empty() { h.message_id } else { h.doc_key },
                title: if title.is_empty() { "(无标题)".to_string() } else { title },
                summary,
                raw: String::new(),
                start_line: 0,
                end_line: 0,
                score: h.score,
            }
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
                    raw: String::new(),
                    start_line: 0,
                    end_line: 0,
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

/// 按 path/start_line/end_line 回读命中段的 JSONL 原文（verbatim）。
/// 读失败（文件不存在/行越界）时返回空串，不影响召回主流程。
fn read_raw_block(store: &Store, h: &crate::ec::index::SearchHit) -> String {
    if h.path.is_empty() || h.end_line == 0 {
        return String::new();
    }
    match store.read_lines_range(&h.path, h.start_line, h.end_line) {
        Ok(lines) => lines.join("\n"),
        Err(_) => String::new(),
    }
}

/// 格式化命中为 CLI 输出文本。
///
/// 起止标记 `[EC_RECALL_RESULT]` / `[/EC_RECALL_RESULT]` 包裹召回内容，
/// 供：① 增量计算识别并扣除（双保险，估算层已按 name 跳过）；
/// ② 摘要 prompt 约束 LLM 压缩时排除该区间。
pub fn format_hits(hits: &[RecallHit]) -> String {
    if hits.is_empty() {
        return "未找到相关历史内容。".to_string();
    }
    let mut out = String::from("[EC_RECALL_RESULT]\n");
    out.push_str(&format!("找到 {} 条相关上下文：\n\n", hits.len()));
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("[{}] ### {} ({})\n{}", i + 1, h.title, h.block_id, h.summary));
        if !h.raw.is_empty() {
            // 命中段原文回读注入，供 LLM 精确检索历史工具参数/错误/用户约束
            out.push_str(&format!("\n--- 原文 (行 {}-{})\n{}", h.start_line, h.end_line, h.raw));
        }
        out.push_str("\n\n");
    }
    out.push_str("[/EC_RECALL_RESULT]");
    out
}

/// 格式化完整标题目录。
pub fn format_directory(hits: &[RecallHit]) -> String {
    if hits.is_empty() {
        return "本会话暂无压缩块。".to_string();
    }
    let mut out = String::from("[EC_DIRECTORY]\n");
    for h in hits {
        out.push_str(&format!("{} {}\n", h.block_id, h.title));
    }
    out.push_str("[/EC_DIRECTORY]");
    out
}

/// 格式化完整精华区。
pub fn format_highlights(highlights: &[String]) -> String {
    if highlights.is_empty() {
        return "本会话暂无精华内容。".to_string();
    }
    let mut out = String::from("[EC_HIGHLIGHTS]\n");
    for h in highlights {
        out.push_str(&format!("- {h}\n"));
    }
    out.push_str("[/EC_HIGHLIGHTS]");
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
            timestamp: None, model: None, channel: None, thinking: None, reasoning_effort: None,
            usage: None, stop_reason: None, reasoning: None, duration_ms: None,
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
            timestamp: None, model: None, channel: None, thinking: None, reasoning_effort: None,
            usage: None, stop_reason: None, reasoning: None, duration_ms: None,
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
