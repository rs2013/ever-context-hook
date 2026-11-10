//! 索引追赶：对新导入的长对话，后台逐段调用 LLM 补 title/summary/highlights。
//!
//! ## 流程（对齐设计文档 §7 超长旧对话处理）
//!
//! ```
//! catchup 子进程（独立，由 hook 在响应后 detach）
//!   循环直到追平:
//!     1. 读祖先链，找游标（最后一个带 ec 字段的记录；无则从根开始）
//!     2. 从游标之后取下一段未折叠前缀（≤ target_tokens）
//!     3. 构造摘要请求（`compress::build_summary_call`，与正常对话/维护共用同一算法）
//!     4. 调 ww3-core（--hooks none）发压缩请求，显式关思考；解析 title/summary/highlights
//!     5. 写回该段最后一条记录的 ec 字段 → 游标前移
//!     6. sleep 200ms（让位主对话）
//!   追平 → 退出（下一轮请求的 prepare 自动用新摘要构建管理区）
//! ```
//!
//! ## 锁
//!
//! `<session>.lock` 文件：同一会话只允许一个追赶进程（防并发重复压缩）。

use crate::ec::compress::{self, Summary, SummaryMode};
use crate::ec::core_client::CoreClient;
use crate::ec::lock;
use crate::ec::index::{self, EcBlockDoc};
use crate::ec::store::Store;
use crate::ec::types::{BlockState, EcProjection, MessageRecord};
use std::fs;
use std::io::Write;
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
    /// 摘要输入模式（fragment / full_context），与正常对话共用（默认 full_context）。
    pub summary_mode: SummaryMode,
    /// full_context 模式的前缀 token 上限，超出自动降级 fragment。
    pub full_context_max_tokens: u32,
    /// 可选 trace 输出：每块追加一条 JSON（含完整 prompt + 上游 usage），供事后复盘。
    pub trace_path: Option<String>,
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
            target_tokens: 10_000,
            gap_ms: 200,
            summary_mode: SummaryMode::default(),
            full_context_max_tokens: 60_000,
            trace_path: None,
        }
    }
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
    // 统一分段（DRY）：只取游标之后的第一段
    crate::ec::budget::segment_records(&chain[start..], target_tokens)
        .first()
        .map(|(s, e)| (s + start, e + start))
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
    // 四态推进用预算（与主对话同窗口，summary/directory 预算一致）
    let window = crate::ec::types::WindowConfig::default();
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

        // 3. 构造摘要请求（与正常对话增量压缩/维护共用同一算法，DRY）
        let call = compress::build_summary_call(
            &chain, start, end, config.summary_mode, config.full_context_max_tokens,
        );
        let t0 = std::time::Instant::now();
        // 4. 调 core 摘要（显式关思考）+ 解析
        let (res, parsed) = match compress::run_summary_call(&mut client, &config.model, &call) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("[ec-catchup] 摘要调用失败（跳过本段，下轮重试）: {e}");
                // 失败也落一条 trace，便于复盘失败现场
                append_trace(config, serde_json::json!({
                    "block": folded, "cursor": cursor, "range": [start, end],
                    "end_id": chain[end].id, "ok": false, "error": e,
                    "effective_mode": call.effective_mode.as_str(),
                    "range_text_chars": call.user_content.len(),
                    "range_text_est_tokens": est_tokens(&call.user_content),
                    "request": call.messages,
                }));
                // 失败不推进游标，下轮重试；但避免死循环，break
                break;
            }
        };
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        let summary = match parsed {
            Some(s) => s,
            None => {
                let p: String = res.text.chars().take(200).collect();
                log::warn!("[ec-catchup] 摘要解析失败，原文前 200 字: {p}");
                append_trace(config, serde_json::json!({
                    "block": folded, "cursor": cursor, "range": [start, end],
                    "end_id": chain[end].id, "ok": false, "error": "parse_summary_failed",
                    "effective_mode": call.effective_mode.as_str(),
                    "range_text_chars": call.user_content.len(),
                    "range_text_est_tokens": est_tokens(&call.user_content),
                    "request": call.messages, "response_text": res.text,
                    "finish_reason": res.finish_reason, "usage": res.usage, "elapsed_ms": elapsed_ms,
                }));
                break;
            }
        };

        // trace桩：完整 prompt + 上游 usage（缓存计量）+ 解析结果，供复盘
        append_trace(config, serde_json::json!({
            "block": folded, "cursor": cursor, "range": [start, end], "end_id": chain[end].id,
            "ok": true, "effective_mode": call.effective_mode.as_str(),
            "range_text_chars": call.user_content.len(),
            "range_text_est_tokens": est_tokens(&call.user_content),
            "request": call.messages, "response_text": res.text,
            "finish_reason": res.finish_reason, "usage": res.usage, "elapsed_ms": elapsed_ms,
            "parsed": { "title": summary.title, "summary": summary.summary, "highlights": summary.highlights },
        }));

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

        // 5.6 四态推进（bug ③ 修复）：摘要/目录区超预算时降最旧块
        advance_states(&store, &config.session_id, &window);

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

/// token 粗估：utf8 字节 / 3（对齐 budget 的估算口径）。
fn est_tokens(s: &str) -> u64 {
    (s.len() as f64 / 3.0).ceil() as u64
}

/// 追加一条 trace（JSONL）。未配置 trace_path 时无操作；写失败只记日志（fail-open）。
fn append_trace(config: &CatchupConfig, rec: serde_json::Value) {
    let Some(path) = &config.trace_path else { return };
    let line = match serde_json::to_string(&rec) {
        Ok(l) => l,
        Err(e) => { log::warn!("[ec-catchup] trace 序列化失败: {e}"); return; }
    };
    if let Err(e) = fs::OpenOptions::new().create(true).append(true).open(path)
        .and_then(|mut f| writeln!(f, "{line}"))
    {
        log::warn!("[ec-catchup] trace 写入失败 {path}: {e}");
    }
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
        doc_text: String::new(), // ec_block 无 STORED 正文（原文权威在 JSONL）
        path: store.session_file_path(&config.session_id).display().to_string(),
        start_line: line_of(&chain[start].id),
        end_line: line_of(end_id),
    };
    if let Err(e) = index::index_block(&index, &doc) {
        log::warn!("[ec-catchup] 更新索引失败（fail-open）: {e}");
    }
}

/// 四态推进（bug ③ 修复）：摘要区超 summary_budget 时最旧 summary→index_only；
/// 目录区超 directory_budget 时最旧 index_only→omitted。
///
/// 状态推进规则：
/// - summary→index_only：summary 清空，title/highlights 保留（精华跨段长期有效）。
/// - index_only→omitted：title/highlights 保留（仅可召回，见 design §3），只从投影渲染。
///
/// 每次最多降两块（一块 summary、一块 index_only），由 catchup 每轮循环反复驱动到收敛。
pub fn advance_states(store: &Store, session_id: &str, budget: &crate::ec::types::WindowConfig) -> usize {
    let leaf = match store.read_leaf(session_id) { Some(l) => l, None => return 0 };
    let chain = match store.load_chain(session_id, &leaf) { Ok(c) => c, Err(_) => return 0 };
    let mut advanced = 0;

    // 摘要区超预算：降最旧 summary → index_only（链为根→叶，first 即最旧）
    let summary_tokens: u32 = chain.iter()
        .filter(|r| r.ec.as_ref().map(|ec| ec.state == BlockState::Summary).unwrap_or(false))
        .map(|r| r.ec.as_ref().map(|ec| (ec.summary.len() as f64 / 3.0).ceil() as u32).unwrap_or(0))
        .sum();
    if summary_tokens > budget.budget.summary_budget {
        if let Some(r) = chain.iter().find(|r| r.ec.as_ref().map(|ec| ec.state == BlockState::Summary).unwrap_or(false)) {
            if let Some(ec) = &r.ec {
                let new = EcProjection {
                    state: BlockState::IndexOnly,
                    title: ec.title.clone(),
                    summary: String::new(),
                    highlights: ec.highlights.clone(),
                };
                if store.set_ec(session_id, &r.id, &new).is_ok() {
                    log::info!("[ec-catchup] 四态推进：{} summary→index_only（摘要区 {} token 超 {}）",
                        r.id, summary_tokens, budget.budget.summary_budget);
                    advanced += 1;
                }
            }
        }
    }

    // 目录区超预算：降最旧 index_only → omitted（标题保留供召回，仅从投影滚出）
    let dir_tokens: u32 = chain.iter()
        .filter(|r| r.ec.as_ref().map(|ec| ec.state == BlockState::IndexOnly).unwrap_or(false))
        .map(|r| r.ec.as_ref().map(|ec| (ec.title.len() as f64 / 3.0).ceil() as u32 + 12).unwrap_or(0))
        .sum();
    if dir_tokens > budget.budget.directory_budget {
        if let Some(r) = chain.iter().find(|r| r.ec.as_ref().map(|ec| ec.state == BlockState::IndexOnly).unwrap_or(false)) {
            if let Some(ec) = &r.ec {
                let new = EcProjection {
                    state: BlockState::Omitted,
                    title: ec.title.clone(),
                    summary: String::new(),
                    highlights: ec.highlights.clone(),
                };
                if store.set_ec(session_id, &r.id, &new).is_ok() {
                    log::info!("[ec-catchup] 四态推进：{} index_only→omitted（目录区 {} token 超 {}）",
                        r.id, dir_tokens, budget.budget.directory_budget);
                    advanced += 1;
                }
            }
        }
    }

    advanced
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::store::Store;
    use crate::ec::types::WindowConfig;

    fn summary_rec(id: &str, parent: Option<&str>, summary_chars: usize) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            role: "tool_result".to_string(),
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
            name: None,
            ec: Some(EcProjection {
                state: BlockState::Summary,
                title: "块".to_string(),
                summary: "很".repeat(summary_chars),
                highlights: vec![],
            }),
        }
    }

    #[test]
    fn advance_summary_to_index_only_when_over_budget() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target").join(format!("ec-catchup-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(dir.clone());

        let records = vec![
            summary_rec("b1", None, 200),
            summary_rec("b2", Some("b1"), 200),
        ];
        store.append("s1", &records).unwrap();
        store.update_leaf("s1", "b2").unwrap();

        let mut w = WindowConfig::default();
        w.budget.summary_budget = 10; // 极低预算，触发推进
        let advanced = advance_states(&store, "s1", &w);
        assert!(advanced >= 1);

        let chain = store.load_chain("s1", "b2").unwrap();
        // 最旧 summary（b1）→ index_only
        assert_eq!(chain[0].ec.as_ref().unwrap().state, BlockState::IndexOnly);
        assert!(chain[0].ec.as_ref().unwrap().summary.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
