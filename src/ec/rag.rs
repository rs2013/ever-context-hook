//! recall 的 RAG 生成层：检索 top-k → 组 prompt → 调 LLM 生成带引用的回答。
//!
//! 分层：
//! - `recall.rs` 负责纯检索（Tantivy 召回 + 分支过滤 + 原文回读），不调 LLM；
//! - 本模块负责「取相关度最高的 top_k 条 → 交给 LLM 生成回答」。
//!
//! 与 catchup 共用 [`CoreClient`]：spawn 独立 `ww3-core stdio --hooks none` 子进程发
//! `chat.generate`，避免 RAG 请求再经 EC hook 造成递归。

use crate::ec::core_client::CoreClient;
use crate::ec::recall::{self, RecallHit};

/// 单条命中注入 prompt 的原文上限（字符）。超出截断，防止一块原文挤爆上下文。
pub const RAW_CHAR_CAP: usize = 4_000;

/// RAG 系统提示词：只依据片段回答，必须带序号引用，不足时明说。
pub const RAG_SYSTEM_PROMPT: &str = r#"你是会话历史问答助手。规则：
- 只依据下面给出的检索片段回答，不得编造片段中没有的事实。
- 片段用序号 [1] [2] 标注，回答时用同样序号注明依据。
- 片段不足以回答时，直接说明「现有历史片段不足以回答」，并指出还缺什么信息。
- 用中文回答，先给结论，再给依据；不要复述整段原文。"#;

/// RAG 运行配置（CLI 解析后传入）。
#[derive(Debug, Clone)]
pub struct RagConfig {
    pub session_id: String,
    pub query: String,
    /// 只把相关度最高的 top_k 条交给 LLM。
    pub top_k: usize,
    pub data_root: String,
    pub model: String,
    pub thinking: bool,
    pub core_exe: String,
    pub core_data: String,
    pub plugins_dir: String,
}

/// `EC_RAG_CONFIG` 环境变量形态（供 hook/skill 侧免改命令行地覆盖默认值）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RagEnvConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<bool>,
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// 按字符数截断（UTF-8 安全），超长补省略号。
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 组装 chat.generate 的 messages：system 规则 + user（问题 + 编号片段）。
pub fn build_messages(hits: &[RecallHit], query: &str) -> Vec<serde_json::Value> {
    let mut ctx = String::new();
    for (i, h) in hits.iter().enumerate() {
        ctx.push_str(&format!("[{}] {} ({}, 行 {}-{})\n", i + 1, h.title, h.block_id, h.start_line, h.end_line));
        if !h.summary.is_empty() {
            ctx.push_str(&format!("摘要：{}\n", h.summary));
        }
        if !h.raw.is_empty() {
            ctx.push_str(&format!("原文：\n{}\n", cap_chars(&h.raw, RAW_CHAR_CAP)));
        }
        ctx.push('\n');
    }
    vec![
        serde_json::json!({ "role": "system", "content": RAG_SYSTEM_PROMPT }),
        serde_json::json!({
            "role": "user",
            "content": format!("问题：{query}\n\n检索片段（按相关度降序，共 {} 条）：\n{ctx}", hits.len()),
        }),
    ]
}

/// 执行一次 RAG：检索 top_k → 组 prompt → 调 LLM。
///
/// 无命中时**不调用 LLM**（直接返回确定性文案，省一次上游调用）。
pub fn run(cfg: &RagConfig) -> Result<String, String> {
    let hits = recall::search(&cfg.session_id, &cfg.query, cfg.top_k.max(1), &cfg.data_root);
    if hits.is_empty() {
        return Ok("未在当前会话历史中找到与问题相关的内容。".to_string());
    }
    // 只在确有命中时才要求模型（无命中不调 LLM，也就不需要 --model）。
    if cfg.model.trim().is_empty() || cfg.model == "unknown" {
        return Err("缺少生成模型：指定 --model 或 EC_RAG_CONFIG.model（会话记录中也没有可用模型）".to_string());
    }
    let msgs = build_messages(&hits, &cfg.query);
    let mut client = CoreClient::spawn(&cfg.core_exe, &cfg.core_data, &cfg.plugins_dir)?;
    let res = client.chat(&cfg.model, &msgs, cfg.thinking)?;
    if res.text.trim().is_empty() {
        return Err(format!("模型返回空回答（finish_reason={}）", res.finish_reason));
    }
    Ok(res.text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, raw: &str) -> RecallHit {
        RecallHit {
            block_id: "m1".to_string(),
            title: title.to_string(),
            summary: "摘要内容".to_string(),
            raw: raw.to_string(),
            start_line: 3,
            end_line: 9,
            score: 1.0,
        }
    }

    #[test]
    fn build_messages_numbers_hits_in_order() {
        let hits = vec![hit("第一块", "原文A"), hit("第二块", "原文B")];
        let msgs = build_messages(&hits, "接口为什么超时？");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        let user = msgs[1]["content"].as_str().unwrap();
        assert!(user.contains("接口为什么超时？"));
        assert!(user.contains("[1] 第一块"));
        assert!(user.contains("[2] 第二块"));
        assert!(user.contains("原文A") && user.contains("原文B"));
        assert!(user.contains("共 2 条"));
    }

    #[test]
    fn build_messages_caps_long_raw() {
        let long = "汉".repeat(RAW_CHAR_CAP + 100);
        let msgs = build_messages(&[hit("长块", &long)], "q");
        let user = msgs[1]["content"].as_str().unwrap();
        assert!(user.contains('…'));
        // 截断后总长度应接近上限，而不是原长
        assert!(user.chars().count() < RAW_CHAR_CAP + 500);
    }

    #[test]
    fn cap_chars_keeps_short_text() {
        assert_eq!(cap_chars("短文本", 10), "短文本");
        assert_eq!(cap_chars("一二三四五", 3), "一二三…");
    }
}
