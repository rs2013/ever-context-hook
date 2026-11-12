//! Ever-Context 核心模块（代理版）。
//!
//! 模块映射（对齐设计文档 `双上下文持久化与召回设计.md`）：
//! - `types.rs`     ↔ 核心类型：MessageRecord（JSONL 记录）+ 投影产物 + 预算派生
//! - `identity.rs`  ↔ 会话寻址（首条 user SHA）+ 消息身份（message_id 含 parent_id）
//! - `store.rs`     ↔ JSONL 存储层：读/写/定位/回溯/叶子定位文件
//! - `config.rs`    ↔ hook 配置解析（params → HookConfig）
//! - `budget.rs`    ↔ 预算检查（硬门/软阈值/growth nudge，跳过 recall 内容）
//! - `projection.rs`↔ 管理区投影构建（从祖先链的 ec 字段渲染）
//! - `recall.rs`    ↔ recall CLI 的搜索逻辑（祖先链范围）
//! - `rag.rs`       ↔ recall 的 RAG 生成层（top-k 检索 → LLM 生成带引用回答）
//! - `compress.rs`  ↔ 压缩触发与执行（增量估算 + 提示词）

pub mod budget;
pub mod catchup;
pub mod compress;
pub mod config;
pub mod core_client;
pub mod identity;
pub mod index;
pub mod lock;
pub mod projection;
pub mod rag;
pub mod recall;
pub mod store;
pub mod types;

pub use config::HookConfig;

/// 当前可执行文件路径。
///
/// 注入给模型的提示文本（管理区汰出提示 / 降级提示 / skill 文本）要直接给出真实路径，
/// 模型才能用 bash 调 `recall` / `rag`；取不到时回退到命令名。
pub fn bin_hint() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "ec-hook".to_string())
}
