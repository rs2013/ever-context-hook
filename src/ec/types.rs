//! EC 代理版核心类型。
//!
//! 与 Pi 插件版的 `ec-types.ts` 不同：代理版没有 Pi 的 JSONL 与分支 API，
//! 核心持久化单元是**自建的会话 JSONL 文件**，每行一条消息记录，
//! 双侧上下文（agent 侧原文 + 投影产物）并存于同一文件。

use serde::{Deserialize, Serialize};

/// 会话 JSONL 文件中一条消息记录。
///
/// 结构参考 Pi 的会话记录，但 message_id 由内容哈希派生（见 `identity.rs`），
/// parent_id 指向前一条消息的 message_id，天然支持树状分叉。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// message_id = sha256(feature_text + parent_id) 前 16 hex。
    pub id: String,
    /// 前一条消息的 message_id；根消息为 null。
    pub parent_id: Option<String>,
    /// user | assistant | tool_result
    pub role: String,
    /// EC 观察到响应开始的时间（Unix 毫秒）；导入历史时不冒充原始创建时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    /// 对话模型；历史请求未携带模型时保持空值，不推测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本轮上游响应的渠道标识（若 ww3 显式提供）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// 请求中的 thinking / reasoning_effort；不是实际推理发生与否的证明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// ww3 的 usage 事件原样保存（非 Pi usage，字段及计费口径不同）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 文本正文（user/assistant 有正文；tool_result 的正文是工具输出）。
    #[serde(default)]
    pub content: String,
    /// assistant 的工具调用（可选）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRecord>,
    /// tool_result 所应答的工具调用 ID（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// tool_result 的工具名（可选，用于 feature_text 与防篡改审计）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 消息名（OpenAI 的 name 字段；投影注入消息用它标记 ec_meta / ec_recall_result）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 投影产物（可选）：压缩结果挂在与触发压缩同一行的记录上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ec: Option<EcProjection>,
}

/// assistant 消息里的工具调用记录（OpenAI 形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub type_: String,
    pub function: ToolCallFunction,
}

fn default_tool_type() -> String { "function".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// 投影产物（压缩块的持久化形态）。
///
/// 四态单向滚动：raw -> summary -> index_only -> omitted，只前进不回退。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcProjection {
    /// raw | summary | index_only | omitted
    pub state: BlockState,
    /// 块标题（≤40 字）。
    #[serde(default)]
    pub title: String,
    /// 摘要全文（state == summary 时非空）。
    #[serde(default)]
    pub summary: String,
    /// 精华：跨段长期有效的高价值信息（凭据/术语/核心契约），≤3 条每条 ≤40 字。
    #[serde(default)]
    pub highlights: Vec<String>,
}

/// 块状态（四态单向）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockState {
    Raw = 0,
    Summary = 1,
    IndexOnly = 2,
    Omitted = 3,
}

impl BlockState {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "raw" => Some(Self::Raw),
            "summary" => Some(Self::Summary),
            "index_only" => Some(Self::IndexOnly),
            "omitted" => Some(Self::Omitted),
            _ => None,
        }
    }
}

// ── OpenAI 请求消息（从 params.request.messages 解析） ──

/// OpenAI Chat 消息（入站解析用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: MessageContent,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
    /// 显式 JSON null（OpenAI 工具调用 assistant 消息的常见形态）：
    /// 用 unit 变体承接，避免整条消息反序列化失败被静默丢弃。
    Null,
}

impl Default for MessageContent {
    fn default() -> Self { MessageContent::Text(String::new()) }
}

impl MessageContent {
    /// 提取纯文本（用于指纹计算与 token 估算）。
    pub fn extract_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => {
                parts.iter()
                    .filter_map(|p| {
                        if let ContentPart::Text { text, .. } = p { Some(text.as_str()) } else { None }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            MessageContent::Null => String::new(),
        }
    }

    /// 粗略 token 估算（utf8 字节数 ÷ 3，对齐 Pi 版 `utf8ByteLength/3`）。
    /// ASCII 约 1 字节 ≈ 0.33 token（略高估）；CJK 3 字节 ≈ 1 token（贴合）。
    pub fn estimated_tokens(&self) -> u32 {
        let s = self.extract_text();
        if s.is_empty() {
            0
        } else {
            ((s.len() as f64) / 3.0).ceil() as u32 // s.len() = utf8 字节数
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContentPart {
    Text { text: String, #[serde(rename = "type")] type_: String },
    ImageUrl { image_url: std::collections::HashMap<String, String>, #[serde(rename = "type")] type_: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub type_: String,
    pub function: ToolCallFunction,
}

// ── 上下文窗口配置（复用 Pi 插件版 deriveBudget 公式） ──

/// 派生预算（对齐 `ec-budget.ts` 的 `DerivedBudget`）。
#[derive(Debug, Clone, Copy)]
pub struct DerivedBudget {
    /// 硬安全线预留：max(16384, round(W×0.10))。
    pub reserve_tokens: u32,
    /// 软阈值：clamp(round(W×0.50), 100K, 500K)。
    pub soft_threshold: u32,
    /// 降级目标比例（首次尾部贴合降级时的保留比例，默认 0.6）。
    /// degrade_target = soft_threshold × degrade_target_ratio。
    pub degrade_target_ratio: f64,
    /// 降级目标 token（实际降级时保留的 token 数）。
    pub degrade_target: u32,
    /// 目录区 token 预算（固定 3K）。
    pub directory_budget: u32,
    /// 精华区 token 预算（固定 5K）。
    pub highlights_budget: u32,
    /// 摘要区 token 预算 = round(softThreshold × 0.30)。
    pub summary_budget: u32,
    /// 原文区预算 = softThreshold − 三区。
    pub raw_budget: u32,
    /// 单块目标 token（10K，可被 EcConfig.target_tokens 覆盖）。
    pub target_tokens: u32,
    /// 过度合并护栏（80K）。
    pub max_block_tokens: u32,
}

impl DerivedBudget {
    /// 按 contextWindow 派生全部阈值（对齐 ec-budget.ts deriveBudget 公式）。
    /// degrade_target_ratio 默认 0.6（首次降级保留软阈值 60%）。
    pub fn derive(context_window: u32) -> Self {
        Self::derive_with_ratio(context_window, 0.6)
    }

    /// 指定降级比例的派生。
    pub fn derive_with_ratio(context_window: u32, degrade_target_ratio: f64) -> Self {
        let soft_threshold = ((context_window as f64 * 0.50).round() as u32)
            .clamp(100_000, 500_000);
        let directory_budget = 3_000;
        let highlights_budget = 5_000;
        let summary_budget = (soft_threshold as f64 * 0.30).round() as u32;
        let raw_budget = soft_threshold
            .saturating_sub(directory_budget)
            .saturating_sub(highlights_budget)
            .saturating_sub(summary_budget);
        let degrade_target = (soft_threshold as f64 * degrade_target_ratio).round() as u32;
        Self {
            reserve_tokens: ((context_window as f64 * 0.10).round() as u32).max(16_384),
            soft_threshold,
            degrade_target_ratio,
            degrade_target,
            directory_budget,
            highlights_budget,
            summary_budget,
            raw_budget,
            target_tokens: 10_000,
            max_block_tokens: 80_000,
        }
    }
}

/// 上下文窗口配置。
#[derive(Debug, Clone, Copy)]
pub struct WindowConfig {
    pub context_window: u32,
    pub max_output_tokens: u32,
    /// 硬预算线（token）= context_window − reserve_tokens − max_output_tokens。
    pub hard_budget: u32,
    /// 派生预算。
    pub budget: DerivedBudget,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self::for_window(180_000)
    }
}

impl WindowConfig {
    pub fn for_window(context_window: u32) -> Self {
        let budget = DerivedBudget::derive(context_window);
        let max_output_tokens = 8_192;
        let hard_budget = context_window
            .saturating_sub(budget.reserve_tokens)
            .saturating_sub(max_output_tokens);
        Self { context_window, max_output_tokens, hard_budget, budget }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_null_parses_with_tool_calls() {
        // OpenAI 工具调用 assistant 消息常见 content:null，修复前会整条解析失败被丢弃。
        let raw = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]
        });
        let m: ChatMessage = serde_json::from_value(raw).expect("content:null 应能解析");
        assert_eq!(m.role, "assistant");
        assert_eq!(m.tool_calls.len(), 1);
        assert_eq!(m.content.extract_text(), "");
    }
}
