//! Hook 配置解析（从 params 提取）。
//!
//! params 结构（参见 HOOK_PROTOCOL §3.1）：
//! ```json
//! {
//!   "request": { /* chat.GenerateRequest */ },
//!   "config": { /* hook 专属配置 */ },
//!   "hook_id": "ec-1",
//!   "chain": { "index": 0, "total": 1 },
//!   "upstream": { "ok": true }
//! }
//! ```

use crate::ec::types::{ChatMessage, WindowConfig};

/// 从 params 提取的 hook 配置。
#[derive(Debug, Clone)]
pub struct HookConfig {
    pub hook_id: String,
    pub chain_index: usize,
    pub chain_total: usize,
    pub model: String,
    /// 请求声明的 max_tokens（用于计算可用预算）。
    pub max_tokens: Option<u32>,
    /// 会话 ID（首条 user 锚定区 SHA）。
    pub session_id: String,
    /// 请求消息（入站原文，含 system/tool 等所有角色）。
    pub messages: Vec<ChatMessage>,
    /// 模型上下文窗口配置。
    pub window: WindowConfig,
    /// 本 hook 专属配置。
    pub ec: EcConfig,
    /// upstream 摘要。
    pub upstream_ok: bool,
    /// 数据根目录（JSONL 存储）。
    pub data_root: String,
}

/// EC hook 专属配置（来自 core.json hooks[].config）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EcConfig {
    /// 是否启用（false 时纯透传）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 硬预算门覆盖（0 = 自动从模型上下文窗口推算）。
    #[serde(default)]
    pub hard_budget: u32,
    /// 软阈值覆盖（0 = 用 deriveBudget 公式）。
    #[serde(default)]
    pub soft_threshold: u32,
    /// growth nudge 步长（token），每次压缩后预算增量。
    #[serde(default = "default_growth_nudge")]
    pub growth_nudge: u32,
    /// 压力阈值（0-1），token 占比超过此值触发压缩。
    #[serde(default = "default_pressure")]
    pub pressure_threshold: f64,
    /// 降级目标比例（首次尾部贴合降级时保留软阈值的比例，默认 0.6）。
    #[serde(default = "default_degrade_ratio")]
    pub degrade_target_ratio: f64,
    /// fail_open：启动失败时是否跳过（观测模式为 true，门模式为 false）。
    #[serde(default)]
    pub fail_open: bool,
    /// 单块目标 token（增量压缩/索引分段的块大小，默认 10_000）。
    #[serde(default = "default_target_tokens")]
    pub target_tokens: u32,
}

fn default_true() -> bool { true }
fn default_growth_nudge() -> u32 { 50_000 }
fn default_pressure() -> f64 { 0.75 }
fn default_degrade_ratio() -> f64 { 0.6 }
fn default_target_tokens() -> u32 { 10_000 }

impl Default for EcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hard_budget: 0,
            soft_threshold: 0,
            growth_nudge: 50_000,
            pressure_threshold: 0.75,
            degrade_target_ratio: 0.6,
            fail_open: false,
            target_tokens: 10_000,
        }
    }
}

impl HookConfig {
    /// 从 hook.stream 的 params JSON 提取配置。
    pub fn from_params(params: &serde_json::Value) -> Self {
        let hook_id = params.get("hook_id").and_then(|v| v.as_str()).unwrap_or("ec-0").to_string();
        let chain = params.get("chain").and_then(|v| v.as_object());
        let chain_index = chain.and_then(|c| c.get("index")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let chain_total = chain.and_then(|c| c.get("total")).and_then(|v| v.as_u64()).unwrap_or(1) as usize;

        let request = params.get("request");
        let model = request.and_then(|r| r.get("model"))
            .and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let max_tokens = request.and_then(|r| r.get("max_tokens"))
            .and_then(|v| v.as_u64()).map(|v| v as u32);

        // 解析 messages
        let messages: Vec<ChatMessage> = request
            .and_then(|r| r.get("messages"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| serde_json::from_value(m.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        // 会话 ID = 首条 user 锚定区（首个 assistant 之前的所有 user 拼接）SHA
        let session_id = crate::ec::identity::session_id(&messages);

        // EC 专属配置
        let ec: EcConfig = params.get("config")
            .and_then(|c| serde_json::from_value(c.clone()).ok())
            .unwrap_or_default();

        // 上下文窗口：从模型名推断，硬预算/软阈值可用配置覆盖
        let context_window = infer_context_window(&model);
        let mut window = WindowConfig::for_window(context_window);
        if ec.hard_budget > 0 {
            window.hard_budget = ec.hard_budget;
        }
        if ec.soft_threshold > 0 {
            window.budget.soft_threshold = ec.soft_threshold;
        }
        if ec.target_tokens > 0 {
            window.budget.target_tokens = ec.target_tokens;
        }

        let upstream_ok = params.get("upstream")
            .and_then(|u| u.get("ok"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // 数据根：环境变量 EC_DATA_ROOT > ww3 的 --data 参数（由 main 传入）
        // 这里先用环境变量，main 里解析 --data 后覆盖。
        let data_root = std::env::var("EC_DATA_ROOT")
            .unwrap_or_else(|_| std::env::var("WW3_DATA").unwrap_or_else(|_| ".".to_string()));

        Self {
            hook_id,
            chain_index,
            chain_total,
            model,
            max_tokens,
            session_id,
            messages,
            window,
            ec,
            upstream_ok,
            data_root,
        }
    }
}

/// 从模型名推断上下文窗口大小（后续可扩展为查表）。
fn infer_context_window(model: &str) -> u32 {
    let lower = model.to_lowercase();
    if lower.contains("gpt-5") || lower.contains("gpt-4.5") { 270_000 }
    else if lower.contains("gpt-4") { 128_000 }
    else if lower.contains("claude") { 200_000 }
    else if lower.contains("gemini") { 1_000_000 }
    else if lower.contains("deepseek") { 128_000 }
    else if lower.contains("qwen") { 128_000 }
    else { 128_000 } // 保守默认值
}
