//! 索引追赶：对新导入的长对话，后台逐段调用 LLM 补 title/summary/highlights。
//!
//! ## 流程（对齐设计文档 §7 超长旧对话处理）
//!
//! ```
//! catchup 子进程（独立，由 hook 在响应后 detach）
//!   循环直到追平:
//!     1. 读祖先链，找游标（最后一个带 ec 字段的记录；无则从根开始）
//!     2. 从游标之后取下一段未折叠前缀（≤ target_tokens）
//!     3. 调 ww3-core（--hooks none）发压缩请求，关思考
//!     4. 解析摘要（title/summary/highlights）
//!     5. 写回该段最后一条记录的 ec 字段 → 游标前移
//!     6. sleep 200ms（让位主对话）
//!   追平 → 退出（下一轮请求的 prepare 自动用新摘要构建管理区）
//! ```
//!
//! ## 锁
//!
//! `<session>.lock` 文件：同一会话只允许一个追赶进程（防并发重复压缩）。

use crate::ec::core_client::CoreClient;
use crate::ec::lock;
use crate::ec::index::{self, EcBlockDoc};
use crate::ec::store::Store;
use crate::ec::types::{BlockState, EcProjection, MessageRecord};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// 追赶进程配置。
#[derive(Debug, Clone)]
pub struct CatchupConfig {
    pub session_id: String,
    pub data_root: String,
    pub core_exe: String,
    pub core_data: String,
    pub plugins_dir: String,
    pub model: String,
    /// 单块目标 token（摘要一个片段的预算）。
    pub target_tokens: u32,
    /// 每块之间的间隔（让位主对话）。
    pub gap_ms: u64,
}

impl Default for CatchupConfig {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            data_root: String::new(),
            core_exe: "ww3-core".to_string(),
            core_data: String::new(),
            plugins_dir: String::new(),
            model: String::new(),
            target_tokens: 20_000,
            gap_ms: 200,
        }
    }
}

/// 摘要结果（LLM 返回）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub highlights: Vec<String>,
}



/// 找游标：祖先链里最后一个带 ec 字段的记录的下标；无则返回 None。
pub fn cursor_index(chain: &[MessageRecord]) -> Option<usize> {
    chain.iter().rposition(|r| r.ec.is_some())
}

/// 从祖先链里选下一段未折叠前缀（游标之后，按 target_tokens 累计）。
///
/// 返回 (start_idx, end_idx)（含），end 是这段最后一条。None 表示无可压（已追平）。
/// 简化版切分：只按 token 累计，不处理工具链闭合（后续补）。
pub fn next_range(chain: &[MessageRecord], cursor: Option<usize>, target_tokens: u32) -> Option<(usize, usize)> {
    let start = cursor.map(|c| c + 1).unwrap_or(0);
    if start >= chain.len() {
        return None;
    }
    let mut acc = 0u32;
    let mut end = start;
    for i in start..chain.len() {
        let cost = estimate_record_tokens(&chain[i]);
        if acc + cost > target_tokens && i > start {
            break; // 至少包含一条
        }
        acc += cost;
        end = i;
    }
    Some((start, end))
}

/// 估算单条 MessageRecord 的 token（正文 + 工具调用，粗估 utf8/3）。
fn estimate_record_tokens(r: &MessageRecord) -> u32 {
    if r.name.as_deref() == Some("ec_recall_result") { return 0; }
    let mut t = 4u32;
    t += (r.content.len() as f64 / 3.0).ceil() as u32;
    for tc in &r.tool_calls {
        t += 10;
        t += (tc.function.arguments.len() as f64 / 3.0).ceil() as u32;
    }
    t
}

/// 把祖先链的一段转成给压缩 LLM 的文本（带角色前缀，对齐 rangeText）。
pub fn range_text(chain: &[MessageRecord], start: usize, end: usize) -> String {
    let mut out = String::new();
    for i in start..=end {
        let r = &chain[i];
        out.push_str(&format!("[{}] {}\n", r.role, r.content));
        for tc in &r.tool_calls {
            out.push_str(&format!("[tool_call:{}] {}\n", tc.function.name, tc.function.arguments));
        }
    }
    out
}

/// 解析 LLM 摘要响应（容忍 markdown 围栏）。
pub fn parse_summary(text: &str) -> Option<Summary> {
    // 先尝试直接 JSON
    if let Ok(s) = serde_json::from_str::<Summary>(text.trim()) {
        return Some(s);
    }
    // 容忍 ```json ... ``` 围栏
    if let Some(inner) = extract_fenced_json(text) {
        if let Ok(s) = serde_json::from_str::<Summary>(inner) {
            return Some(s);
        }
    }
    None
}

fn extract_fenced_json(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    // 跳过可能的 "json" 语言标记
    let after = after.strip_prefix("json").unwrap_or(after);
    let end = after.find("```")?;
    Some(after[..end].trim())
}

/// 构建压缩请求的 messages。
pub fn compress_messages(range_text: &str) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "role": "system",
            "content": crate::ec::compress::COMPRESS_SYSTEM_PROMPT,
        }),
        serde_json::json!({
            "role": "user",
            "content": format!("以下是要摘要的对话片段：\n\n{range_text}"),
        }),
    ]
}

/// 主循环：追赶直到追平。
///
/// 返回追赶的块数。
pub fn run(config: &CatchupConfig) -> Result<u32, String> {
    // 锁
    let lock_path = lock::session_lock_path(&config.data_root, &config.session_id);
    let (_lock_guard, _, _) = lock::try_acquire(&lock_path)?;
    log::info!("[ec-catchup] session={} 获取锁，开始追赶", config.session_id);

    let store = Store::new(&config.data_root);
    let mut client = CoreClient::spawn(&config.core_exe, &config.core_data, &config.plugins_dir)?;
    log::info!("[ec-catchup] core 已启动，model={}", config.model);

    let mut folded = 0u32;
    loop {
        // 1. 读链 + 游标
        let leaf = store.read_leaf(&config.session_id);
        let chain = match &leaf {
            Some(l) => store.load_chain(&config.session_id, &l).unwrap_or_default(),
            None => {
                log::info!("[ec-catchup] 会话无 leaf，退出");
                break;
            }
        };
        let cursor = cursor_index(&chain);

        // 2. 选下一段
        let (start, end) = match next_range(&chain, cursor, config.target_tokens) {
            Some(r) => r,
            None => {
                log::info!("[ec-catchup] 已追平（cursor={:?}，共 {} 条），共折叠 {} 块", cursor, chain.len(), folded);
                break;
            }
        };

        // 3. 调 core 摘要
        let text = range_text(&chain, start, end);
        let msgs = compress_messages(&text);
        let res = match client.chat(&config.model, &msgs, false) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("[ec-catchup] 摘要调用失败（跳过本段，下轮重试）: {e}");
                // 失败不推进游标，下轮重试；但避免死循环，break
                break;
            }
        };

        // 4. 解析摘要
        let summary = match parse_summary(&res.text) {
            Some(s) => s,
            None => {
                log::warn!("[ec-catchup] 摘要解析失败，原文前 200 字: {}", &res.text[..res.text.len().min(200)]);
                break;
            }
        };

        // 5. 写回 ec 字段（该段最后一条记录）
        let end_id = chain[end].id.clone();
        let title_preview = summary_title_preview(&summary);
        if let Err(e) = store.set_ec(&config.session_id, &end_id, &EcProjection {
            state: BlockState::Summary,
            title: summary.title.clone(),
            summary: summary.summary.clone(),
            highlights: summary.highlights.clone(),
        }) {
            log::warn!("[ec-catchup] 写 ec 失败: {e}");
            break;
        }

        // 5.5 更新索引文档（title/summary 从空补上，保留原定位行号）
        update_index(&store, &config, &chain, start, end, &summary);

        folded += 1;
        log::info!("[ec-catchup] 折叠块 {}: 段[{start}..{end}] \"{}\"（累计 {} 块）",
            end_id, title_preview, folded);

        // 6. 让位
        std::thread::sleep(Duration::from_millis(config.gap_ms));
    }

    lock::release(&lock_path);
    log::info!("[ec-catchup] 结束，共折叠 {} 块", folded);
    Ok(folded)
}

fn summary_title_preview(s: &Summary) -> String {
    if s.title.is_empty() { "(无标题)".to_string() } else { s.title.chars().take(30).collect() }
}

/// 更新索引文档：补上 title/summary，保留分段建索引时的定位行号。
///
/// 失败只记日志（fail-open）：索引可下次追赶/重建时补，不阻断追赶主流程。
fn update_index(
    store: &Store,
    config: &CatchupConfig,
    chain: &[MessageRecord],
    start: usize,
    end: usize,
    summary: &Summary,
) {
    // 打开索引
    let index_dir = std::path::Path::new(&config.data_root).join("index");
    let index = match index::open_index(&index_dir) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("[ec-catchup] 打开索引失败（跳过更新）: {e}");
            return;
        }
    };
    // 查行号（end_id 与段首记录 id → JSONL 行号）
    let rows = match store.read_session_with_lines(&config.session_id) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[ec-catchup] 读行号失败（跳过更新）: {e}");
            return;
        }
    };
    let line_of = |id: &str| -> u64 {
        rows.iter().find(|(_, r)| r.id == id).map(|(ln, _)| *ln).unwrap_or(0)
    };
    let end_id = &chain[end].id;
    let doc = EcBlockDoc {
        session_id: config.session_id.clone(),
        message_id: end_id.clone(),
        doc_key: format!("{}:{}", config.session_id, end_id),
        doc_type: "ec_block".to_string(),
        title: summary.title.clone(),
        summary: summary.summary.clone(),
        path: store.session_file_path(&config.session_id).display().to_string(),
        start_line: line_of(&chain[start].id),
        end_line: line_of(end_id),
    };
    if let Err(e) = index::index_block(&index, &doc) {
        log::warn!("[ec-catchup] 更新索引失败（fail-open）: {e}");
    }
}
