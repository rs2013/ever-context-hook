//! 压缩触发与执行（代理版）。
//!
//! ## 触发条件（对齐设计文档 §6）
//!
//! 1. **Growth Nudge**：自上次压缩以来累计增量 ≥ growth_nudge。
//! 2. **Pressure**：投影 token 占比 ≥ pressure_threshold。
//!
//! ## 关键规则
//!
//! - **增量估算跳过 recall 内容**（`name == "ec_recall_result"`），防死循环。
//! - **压缩提示词明确要求 LLM 忽略 recall 内容**，避免同样内容反复进入索引。
//!
//! ## 执行（统一算法，DRY）
//!
//! 压缩本体（`build_summary_call` / `run_summary_call`）**只有一份**，被两处共用：
//! 存量追赶（`catchup` 串行驱动）与正常对话增量压缩（由请求触发）。对齐 Pi 版
//! `pi-ever-context/src/extension.ts:626` 的 `summarizeOne`。
//!
//! **维护是纯规则、不调 LLM**（`catchup::advance_states` 按阈值做四态升降），不属于本算法。
//!
//! 差异仅在驱动方式与推理开关：追赶用主会话同模型 + 显式关思考、连续串行执行
//! （不靠 tool_result / user prompt 触发）；正常对话增量压缩由请求触发。

use crate::ec::core_client::{ChatResult, CoreClient};
use crate::ec::identity::is_recall_result;
use crate::ec::types::{BlockState, ChatMessage, MessageRecord};

/// 压缩决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressDecision {
    /// 不压缩。
    None,
    /// 软阈值触发（growth nudge 或 pressure）。
    SoftTrigger { reason: CompressReason },
    /// 硬预算超线（必须压缩才能继续）。
    HardTrigger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressReason {
    GrowthNudge,
    Pressure,
}

/// 评估是否需要压缩。
///
/// - `current_tokens`：当前投影 token（**已跳过 recall 内容**）。
/// - `last_compaction_tokens`：上次压缩时的累计 token。
/// - `window`：上下文窗口配置。
/// - `growth_nudge`：growth nudge 阈值。
/// - `pressure_threshold`：压力阈值（0-1）。
pub fn decide(
    current_tokens: u64,
    last_compaction_tokens: u64,
    hard_budget: u32,
    soft_threshold: u32,
    growth_nudge: u32,
    pressure_threshold: f64,
) -> CompressDecision {
    if current_tokens > hard_budget as u64 {
        return CompressDecision::HardTrigger;
    }
    if crate::ec::budget::should_growth_nudge(current_tokens, last_compaction_tokens, growth_nudge) {
        return CompressDecision::SoftTrigger { reason: CompressReason::GrowthNudge };
    }
    if (current_tokens as f64) >= (soft_threshold as f64) * pressure_threshold {
        return CompressDecision::SoftTrigger { reason: CompressReason::Pressure };
    }
    CompressDecision::None
}

/// 压缩提示词（给 LLM 的 system prompt 片段）。
///
/// 明确要求 LLM 忽略 recall 内容，避免重复摘要。
pub const COMPRESS_SYSTEM_PROMPT: &str = r#"你是一个滚动上下文维护器。你被给了一段对话的时序片段，请输出一个 JSON 对象（不要输出其他内容）。

要求：
- title: 字符串，块标题（≤40 字），概括本段内容
- summary: 字符串，摘要（≤600 字），保留关键决策、结论、约束
- highlights: 字符串数组，最多 3 条，每条 ≤40 字，只记录长期有效的关键信息（凭据/术语/核心契约）

**重要：忽略召回内容**
如果对话中出现 tool_result 类型为 ec_recall_result 的内容（即 ec-recall 工具的搜索结果），
这些是对已压缩历史的检索结果，不要将其内容纳入摘要。
它们已经在历史块中，再摘要一遍是重复。

输出格式：
{"title": "...", "summary": "...", "highlights": ["..."]}"#;

/// 判断一段消息序列中哪些是"可压缩的原文"（排除 recall 结果与投影消息）。
pub fn compressible_messages(messages: &[ChatMessage]) -> Vec<&ChatMessage> {
    messages.iter()
        .filter(|m| !is_recall_result(m) && !crate::ec::identity::is_projection_message(m))
        .collect()
}

/// 判断祖先链中哪些记录是"可压缩的原文"（排除 ec 投影产物与 recall 结果）。
pub fn compressible_records(chain: &[MessageRecord]) -> Vec<&MessageRecord> {
    chain.iter()
        .filter(|r| r.ec.is_none())
        .filter(|r| r.name.as_deref() != Some("ec_recall_result"))
        .collect()
}

/// 判断某条记录是否处于"未压缩"状态（无 ec 字段，或 ec.state == raw）。
pub fn is_uncompressed(r: &MessageRecord) -> bool {
    r.ec.as_ref().map(|ec| ec.state == BlockState::Raw).unwrap_or(true)
}

// ────────────────────────────────────────────────────────────────────────────
// 统一压缩算法（唯一一份）：追赶 / 正常对话增量压缩 / 维护共用
// ────────────────────────────────────────────────────────────────────────────

/// 摘要输入模式（对齐 Pi `EcExtensionOptions.summaryMode`，extension.ts:139）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryMode {
    /// 只喂本段原文（块间无前缀共享，缓存不命中）。
    Fragment,
    /// 喂 [0..本段末] 完整前缀 + 唯一边界锚点，模型只压锚点之后。
    /// 连续块天然共享前缀 → 上游前缀缓存可命中（须串行执行）。
    FullContext,
}

impl Default for SummaryMode {
    fn default() -> Self { SummaryMode::FullContext }
}

impl SummaryMode {
    pub fn as_str(&self) -> &'static str {
        match self { SummaryMode::Fragment => "fragment", SummaryMode::FullContext => "full_context" }
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

/// 片段模式 system prompt（只喂本段）。
pub const SUMMARY_SYSTEM_PROMPT: &str = COMPRESS_SYSTEM_PROMPT;

/// 全上下文模式 system prompt（对齐 Pi `EC_SUMMARY_SYSTEM_PROMPT_FULLCONTEXT`，extension.ts:82）。
pub const SUMMARY_SYSTEM_PROMPT_FULLCONTEXT: &str = r#"你是一个滚动上下文维护器。下面是从会话开始到某一时点的完整上下文，以及一个在输入中只出现一次的精确边界锚点。
你的任务：只压缩「从锚点开始到输入末尾」的部分；锚点之前的历史只是背景，不要摘要。
只输出一个 JSON 对象，不要输出其他内容：
{"title":"...","summary":"...","highlights":[...]}
- title：字符串，≤40 字，概括锚点之后那部分内容。
- summary：字符串，≤600 字，保留关键事实、决策及其理由、文件路径、精确错误信息、用户意图与约束；丢弃冗长日志、重复内容与已完成的探索。
- highlights：字符串数组，默认 []，最多 3 条，每条 ≤40 字且自洽；只记录长期有效的高价值信息（凭据/术语/核心契约）；宁缺毋滥，不确定返回 []。
- 忽略历史中的 ec_recall_result 召回内容（它们已在历史块中，重复摘要无意义）。
- 只输出 JSON：不要 markdown 围栏，不要解释。"#;

/// 把祖先链的一段渲染成给压缩 LLM 的文本（带角色前缀，对齐 Pi rangeText）。
pub fn range_text(chain: &[MessageRecord], start: usize, end: usize) -> String {
    let mut out = String::new();
    for r in chain.iter().take(end + 1).skip(start) {
        out.push_str(&format!("[{}] {}\n", r.role, r.content));
        for tc in &r.tool_calls {
            out.push_str(&format!("[tool_call:{}] {}\n", tc.function.name, tc.function.arguments));
        }
    }
    out
}

/// 在 `full` 中找 `seed` 的最短唯一前缀（对齐 Pi uniqueQuoteIn，ec-boundary.ts:145）。
pub fn unique_quote_in(full: &str, seed: &str) -> Option<String> {
    const MIN: usize = 48;
    const MAX: usize = 512;
    if seed.is_empty() { return None; }
    let mut end = MIN.min(seed.len());
    while end <= seed.len().min(MAX) {
        while end < seed.len() && !seed.is_char_boundary(end) { end += 1; } // UTF-8 边界对齐
        let q = &seed[..end];
        if full.matches(q).count() == 1 { return Some(q.to_string()); }
        end += 1;
    }
    None
}

/// 取本段首条原文在「全文」中唯一出现的边界锚点（对齐 Pi uniqueBoundaryQuote，ec-boundary.ts:163）。
pub fn unique_boundary_quote(chain: &[MessageRecord], start: usize) -> Option<String> {
    if start >= chain.len() { return None; }
    let boundary = range_text(chain, start, start);
    if boundary.is_empty() { return None; }
    unique_quote_in(&range_text(chain, 0, chain.len() - 1), &boundary)
}

/// 一次摘要调用的请求构造结果（供 trace 复用完整 prompt）。
pub struct SummaryCall {
    pub effective_mode: SummaryMode,
    /// 完整 messages（system + user），与发给 core 的完全一致。
    pub messages: Vec<serde_json::Value>,
    /// user 正文（用于 token 粗估）。
    pub user_content: String,
}

/// 构造摘要请求（唯一入口，追赶/正常对话增量压缩/维护共用）。
///
/// `FullContext`：喂 [0..end] 完整前缀 + 本段首条原文的唯一锚点；若锚点不唯一或
/// 前缀超 `full_context_max_tokens` 则降级 `Fragment`（旧对话全前缀可能喂不进模型）。
pub fn build_summary_call(
    chain: &[MessageRecord],
    start: usize,
    end: usize,
    mode: SummaryMode,
    full_context_max_tokens: u32,
) -> SummaryCall {
    if mode == SummaryMode::FullContext {
        let prefix = range_text(chain, 0, end);
        let est = (prefix.len() as f64 / 3.0).ceil() as u64;
        if est <= full_context_max_tokens as u64 {
            if let Some(q) = unique_boundary_quote(chain, start) {
                let content = format!("{prefix}\n\n---\n从下面这个精确引用开始，压缩到输入末尾：\n\"\"\"\n{q}\n\"\"\"");
                let messages = vec![
                    serde_json::json!({"role": "system", "content": SUMMARY_SYSTEM_PROMPT_FULLCONTEXT}),
                    serde_json::json!({"role": "user", "content": content}),
                ];
                return SummaryCall { effective_mode: SummaryMode::FullContext, messages, user_content: content };
            }
        }
    }
    let content = format!("以下是要摘要的对话片段：\n\n{}", range_text(chain, start, end));
    let messages = vec![
        serde_json::json!({"role": "system", "content": SUMMARY_SYSTEM_PROMPT}),
        serde_json::json!({"role": "user", "content": content}),
    ];
    SummaryCall { effective_mode: SummaryMode::Fragment, messages, user_content: content }
}

/// 执行一次摘要请求：显式关思考（压缩不需要推理），返回上游结果与解析产物。
pub fn run_summary_call(
    client: &mut CoreClient,
    model: &str,
    call: &SummaryCall,
) -> Result<(ChatResult, Option<Summary>), String> {
    let res = client.chat(model, &call.messages, false)?; // thinking=false：显式关思考
    let parsed = parse_summary(&res.text);
    Ok((res, parsed))
}

/// 解析 LLM 摘要响应（容忍 markdown 围栏）。
pub fn parse_summary(text: &str) -> Option<Summary> {
    if let Ok(s) = serde_json::from_str::<Summary>(text.trim()) { return Some(s); }
    if let Some(inner) = extract_fenced_json(text) {
        if let Ok(s) = serde_json::from_str::<Summary>(inner) { return Some(s); }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_none_when_under_thresholds() {
        assert_eq!(
            decide(10_000, 0, 160_000, 100_000, 50_000, 0.75),
            CompressDecision::None
        );
    }

    #[test]
    fn decide_growth_nudge() {
        assert_eq!(
            decide(60_000, 0, 160_000, 100_000, 50_000, 0.75),
            CompressDecision::SoftTrigger { reason: CompressReason::GrowthNudge }
        );
    }

    #[test]
    fn decide_pressure() {
        // 90K > 100K * 0.75 = 75K，但 < growth_nudge(50K 增量)
        // 用 last_compaction_tokens = 80K，增量 10K < 50K，但压力触发
        assert_eq!(
            decide(90_000, 80_000, 160_000, 100_000, 50_000, 0.75),
            CompressDecision::SoftTrigger { reason: CompressReason::Pressure }
        );
    }

    #[test]
    fn decide_hard() {
        assert_eq!(
            decide(170_000, 0, 160_000, 100_000, 50_000, 0.75),
            CompressDecision::HardTrigger
        );
    }

    fn rec(id: &str, role: &str, content: &str) -> MessageRecord {
        serde_json::from_value(serde_json::json!({
            "id": id, "parent_id": null, "role": role, "content": content,
        })).unwrap()
    }

    // full_context 请求必须包含 [0..end] 完整前缀 + 唯一的边界锚点
    #[test]
    fn full_context_prompt_contains_prefix_and_anchor() {
        let chain = vec![
            rec("a", "user", "第一段用户问题：会话文件如何分叉？"),
            rec("b", "assistant", "用 parent_id 构建树。"),
            rec("c", "user", "第二段：缓存为何为 0？"),
        ];
        let call = build_summary_call(&chain, 2, 2, SummaryMode::FullContext, 60_000);
        assert_eq!(call.effective_mode, SummaryMode::FullContext);
        assert!(call.user_content.contains("第一段用户问题")); // 前缀包含历史
        assert!(call.user_content.contains("第二段：缓存为何为 0？")); // 含本段
        assert!(call.user_content.contains("从下面这个精确引用开始")); // 锚点指令
        let sys = call.messages[0]["content"].as_str().unwrap();
        assert!(sys.contains("只压缩"));
    }

    // 前缀超上限时降级 fragment
    #[test]
    fn full_context_falls_back_to_fragment_when_prefix_too_big() {
        let chain = vec![
            rec("a", "user", "很长的历史正文aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            rec("b", "user", "第二段"),
        ];
        let call = build_summary_call(&chain, 1, 1, SummaryMode::FullContext, 1);
        assert_eq!(call.effective_mode, SummaryMode::Fragment);
        assert!(!call.user_content.contains("很长的历史正文"));
    }

    // unique_quote_in：取最短唯一前缀；整段出现两次则 None
    #[test]
    fn unique_quote_in_takes_shortest_unique_prefix() {
        let boundary = "这是一个足够长的边界片段用于测试唯一引用"; // 63 字节
        let twice = format!("前导{boundary}中间{boundary}尾部");
        assert!(unique_quote_in(&twice, boundary).is_none()); // 出现两次，整段也无法唯一
        let once = format!("前导{boundary}尾部");
        let q = unique_quote_in(&once, boundary).unwrap();
        assert_eq!(once.matches(&q).count(), 1);
        assert_eq!(q.len(), 48); // 最短唯一前缀 = MIN 长度
    }
}
