//! Ever-Context 核心模块（代理版）。
//!
//! 模块映射（对齐设计文档 `双上下文持久化与召回设计.md`）：
//! - `types.rs`     ↔ 核心类型：MessageRecord（JSONL 记录）+ 投影产物 + 预算派生
//! - `identity.rs`  ↔ 会话寻址（首条 user SHA）+ 消息身份（message_id 含 parent_id）
//! - `store.rs`     ↔ JSONL 存储层：读/写/定位/回溯/叶子定位文件
//! - `config.rs`    ↔ hook 配置解析（params → HookConfig）
//! - `budget.rs`    ↔ 预算检查（硬门/软阈值/growth nudge，跳过 recall 内容）
//! - `projection.rs`↔ 管理区投影构建（从祖先链的 ec 字段渲染）
//! - `recall.rs`    ↔ ec-recall CLI 的搜索逻辑（祖先链范围）
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
pub mod recall;
pub mod store;
pub mod types;

pub use config::HookConfig;
