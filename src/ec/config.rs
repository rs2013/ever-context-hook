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
    /// 当前对话请求的思考开关（只有明确传入时才存在）。
    pub request_thinking: Option<bool>,
    /// 会话 ID（首条 user 锚定区 SHA）。
    pub session_id: String,
    /// 请求消息（入站原文，含 system/tool 等所有角色）。
    pub messages: Vec<ChatMessage>,
    /// 模型上下文窗口配置。
    pub window: WindowConfig,
    /// 本 hook 专属配置。
    pub ec: EcConfig,
    /// 数据根目录（JSONL 存储）。
    pub data_root: String,
}

/// EC hook 专属配置（来自 core.json hooks[].config）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EcConfig {
    /// 是否启用（false 时纯透传）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 上下文窗口覆盖（0 = 按模型名推断 infer_context_window，1 以上为显式 token 数）。
    #[serde(default)]
    pub context_window: u32,
    /// 硬预算门覆盖（0 = 自动从模型上下文窗口推算）。
    #[serde(default)]
    pub hard_budget: u32,
    /// 软阈值覆盖（0 = 用 deriveBudget 公式）。
    #[serde(default)]
    pub soft_threshold: u32,
    /// 降级目标比例（首次尾部贴合降级时保留软阈值的比例，默认 0.6）。
    #[serde(default = "default_degrade_ratio")]
    pub degrade_target_ratio: f64,
    /// fail_open：启动失败时是否跳过（观测模式为 true，门模式为 false）。
    // TODO(未接线)：当前失败一律 pass-through 兜底，此开关待实现。
    #[allow(dead_code)]
    #[serde(default)]
    pub fail_open: bool,
    /// 单块目标 token（增量压缩/索引分段的块大小，默认 10_000）。
    #[serde(default = "default_target_tokens")]
    pub target_tokens: u32,
    /// 正常对话增量压缩；未指定模型时使用本轮对话模型。
    #[serde(default)]
    pub compact: SummaryLlmConfig,
    /// 旧对话追赶；未指定模型时使用会话最后一条 assistant 的模型。
    // TODO(未接线)：prepare 触发的 spawn 目前一律用 compact 模型，手工追赶走 CLI --model。
    #[allow(dead_code)]
    #[serde(default)]
    pub catchup: SummaryLlmConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_models_are_independently_configurable() {
        let params = serde_json::json!({
            "request": {"model": "dialog-model", "thinking": true, "messages": [{"role": "user", "content": "hi"}]},
            "config": {"compact": {"model": "small-model", "thinking": false},
                       "catchup": {"model": "archive-model", "thinking": true}}
        });
        let cfg = HookConfig::from_params(&params);
        assert_eq!(cfg.model, "dialog-model");
        assert_eq!(cfg.request_thinking, Some(true));
        assert_eq!(cfg.ec.compact.model.as_deref(), Some("small-model"));
        assert_eq!(cfg.ec.compact.thinking, Some(false));
        assert_eq!(cfg.ec.catchup.model.as_deref(), Some("archive-model"));
        assert_eq!(cfg.ec.catchup.thinking, Some(true));
        let default = HookConfig::from_params(&serde_json::json!({"request":{"model":"dialog-model"}}));
        assert!(default.ec.compact.model.is_none() && default.ec.catchup.model.is_none());
    }

    #[test]
    fn context_window_override_beats_model_inference() {
        let explicit = HookConfig::from_params(&serde_json::json!({
            "request": {"model": "some-custom-model", "messages": [{"role":"user","content":"hi"}]},
            "config": {"context_window": 32_000}
        }));
        assert_eq!(explicit.window.context_window, 32_000);
        // 未配置时可被模型名覆盖；custom 模型落到保守默认 128K
        let inferred = HookConfig::from_params(&serde_json::json!({
            "request": {"model": "some-custom-model"}
        }));
        assert_eq!(inferred.window.context_window, 128_000);
    }

    #[test]
    fn max_tokens_overrides_output_reserve() {
        let params = serde_json::json!({
            "request": {"model": "gpt-4", "max_tokens": 2048, "messages": [{"role":"user","content":"hi"}]}
        });
        let cfg = HookConfig::from_params(&params);
        assert_eq!(cfg.window.max_output_tokens, 2048);
        assert_eq!(
            cfg.window.hard_budget,
            cfg.window.context_window - cfg.window.budget.reserve_tokens - 2048
        );
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SummaryLlmConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<bool>,
}

fn default_true() -> bool { true }
fn default_degrade_ratio() -> f64 { 0.6 }
fn default_target_tokens() -> u32 { 10_000 }

impl Default for EcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_window: 0,
            hard_budget: 0,
            soft_threshold: 0,
            degrade_target_ratio: 0.6,
            fail_open: false,
            target_tokens: 10_000,
            compact: SummaryLlmConfig::default(),
            catchup: SummaryLlmConfig::default(),
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
        let request_thinking = request.and_then(|r| r.get("thinking")).and_then(|v| v.as_bool());
        let max_tokens = request.and_then(|r| r.get("max_tokens"))
            .and_then(|v| v.as_u64()).map(|v| v as u32);

        // 解析 messages
        let messages: Vec<ChatMessage> = request
            .and_then(|r| r.get("messages"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| match serde_json::from_value(m.clone()) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            // 不再静默丢弃：把失败原文记入日志，便于发现 content:null/多模态等解析缺口。
                            log::warn!("[ec-config] 消息解析失败已跳过: {e}; raw={}", m);
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 会话 ID = 首条 user 锚定区（首个 assistant 之前的所有 user 拼接）SHA
        let session_id = crate::ec::identity::session_id(&messages);

        // EC 专属配置
        let ec: EcConfig = params.get("config")
            .and_then(|c| serde_json::from_value(c.clone()).ok())
            .unwrap_or_default();

        // 上下文窗口：显式配置 > 模型名推断，硬预算/软阈值可用配置覆盖
        let context_window = if ec.context_window > 0 {
            ec.context_window
        } else {
            infer_context_window(&model)
        };
        let mut window = WindowConfig::for_window(context_window);
        // 降级比例可配置（默认 0.6）：非默认值时按新比例重算派生预算。
        if (ec.degrade_target_ratio - 0.6).abs() > f64::EPSILON {
            window.budget = crate::ec::types::DerivedBudget::derive_with_ratio(context_window, ec.degrade_target_ratio);
        }
        // 请求声明的 max_tokens 优先作为输出预留，替换 for_window 的固定 8192。
        if let Some(mt) = max_tokens {
            window.max_output_tokens = mt;
            window.hard_budget = window
                .context_window
                .saturating_sub(window.budget.reserve_tokens)
                .saturating_sub(mt);
        }
        if ec.hard_budget > 0 {
            window.hard_budget = ec.hard_budget;
        }
        if ec.soft_threshold > 0 {
            window.budget.soft_threshold = ec.soft_threshold;
        }
        if ec.target_tokens > 0 {
            window.budget.target_tokens = ec.target_tokens;
        }

        // 数据根：环境变量 EC_DATA_ROOT > ww3 的 --data 参数（由 main 传入）
        // 这里先用环境变量，main 里解析 --data 后覆盖。
        let data_root = std::env::var("EC_DATA_ROOT")
            .unwrap_or_else(|_| std::env::var("WW3_DATA").unwrap_or_else(|_| ".".to_string()));

        Self {
            hook_id,
            chain_index,
            chain_total,
            model,
            request_thinking,
            session_id,
            messages,
            window,
            ec,
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
