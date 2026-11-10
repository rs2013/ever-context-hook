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
//! ## 当前状态
//!
//! 压缩执行依赖「调用 LLM 生成摘要」，在 ww3 hook 形态下需要旁路调用
//! （类似 Pi 版的 `modelRegistry.streamSimple`）。**当前为骨架**，
//! 实际调用链等 request.prepare 契约与旁路调用通道就绪后接入。

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
}
