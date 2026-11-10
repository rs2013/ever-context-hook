//! Ever-Context (EC) 滚动上下文管理器 - ww3 拦截器插件（代理版）。
//!
//! ## 架构
//!
//! ```text
//! stdin JSONL ──▶ [RPC layer] ──▶ [hook::handle_stream]
//!                                      │
//!                    ┌─────────────────┼──────────────────┐
//!                    │ 请求前处理        │ 事件透传          │ 响应后处理
//!                    │ · 会话定位        │ · 透传所有事件     │ · append 新消息
//!                    │ · 祖先链回溯      │ · 收集 usage       │ · 更新叶子
//!                    │ · 投影构建        │                    │ · 压缩判断
//!                    └─────────────────┼──────────────────┘
//! ```
//!
//! ## 数据存储
//!
//! 会话 JSONL 存于 `<data>/sessions/<session_id>.jsonl`，
//! 双侧上下文（agent 侧原文 + 投影产物）并存于同一文件，
//! 通过 message_id + parent_id 支持树状分叉。
//!
//! ## 命令
//!
//! - `call`：ww3 hook.stream 的一次调用（core 每请求 spawn 一次）
//! - `manifest`：输出 manifest.json
//! - `recall`：ec-recall CLI（LLM 通过 skill 注入调用，搜索会话历史）
//! - `ping`：自检

mod ec;
mod hook;
mod rpc;

use std::env;
use std::process;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("call");

    // 解析 --data 参数（ww3 call 模式会传插件数据目录）
    parse_data_arg(&args);

    match cmd {
        "call" | "stdio" => {
            let exit_code = rpc::serve_once(hook::handle_stream, hook::handle_prepare);
            process::exit(exit_code);
        }
        "manifest" => {
            let out_path = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1));
            let m = hook::manifest_json();
            match out_path {
                Some(path) => {
                    std::fs::write(path, m).unwrap_or_else(|e| {
                        eprintln!("写 manifest 失败: {e}");
                        process::exit(1);
                    });
                }
                None => println!("{m}"),
            }
        }
        "recall" => {
            // ec-recall CLI：LLM 通过 skill 注入调用
            // 用法: ever-context-hook recall --session <id> [--query <q>] [--limit <n>]
            let session = arg_value(&args, "--session").unwrap_or_default();
            let query = arg_value(&args, "--query").unwrap_or_default();
            let limit = arg_value(&args, "--limit").and_then(|v| v.parse().ok()).unwrap_or(5);
            let data_root = env::var("EC_DATA_ROOT").unwrap_or_default();
            if session.is_empty() {
                eprintln!("用法: ever-context-hook recall --session <id> [--query <q>] [--limit <n>]");
                process::exit(2);
            }
            let hits = ec::recall::search(&session, &query, limit, &data_root);
            println!("{}", ec::recall::format_hits(&hits));
        }
        "catchup" => {
            // 索引追赶：后台逐段调 LLM 补摘要，直到追平
            // 用法: ever-context-hook catchup --session <id> --data <dir> --core <core路径> --core-data <core数据目录> --plugins <插件目录> --model <摘要模型>
            let cfg = ec::catchup::CatchupConfig {
                session_id: arg_value(&args, "--session").unwrap_or_default(),
                data_root: arg_value(&args, "--data").unwrap_or_else(|| env::var("EC_DATA_ROOT").unwrap_or_default()),
                core_exe: arg_value(&args, "--core").unwrap_or_else(|| "ww3-core".to_string()),
                core_data: arg_value(&args, "--core-data").unwrap_or_default(),
                plugins_dir: arg_value(&args, "--plugins").unwrap_or_default(),
                model: arg_value(&args, "--model").unwrap_or_default(),
                target_tokens: arg_value(&args, "--target-tokens").and_then(|v| v.parse().ok()).unwrap_or(20_000),
                gap_ms: arg_value(&args, "--gap-ms").and_then(|v| v.parse().ok()).unwrap_or(200),
            };
            if cfg.session_id.is_empty() || cfg.core_data.is_empty() {
                eprintln!("用法: ever-context-hook catchup --session <id> --core-data <core数据目录> [--core <路径>] [--plugins <目录>] [--model <模型>] [--target-tokens <n>] [--gap-ms <n>]");
                process::exit(2);
            }
            match ec::catchup::run(&cfg) {
                Ok(n) => {
                    log::info!("[ec-catchup] 完成，折叠 {} 块", n);
                    process::exit(0);
                }
                Err(e) => {
                    eprintln!("[ec-catchup] 失败: {e}");
                    process::exit(1);
                }
            }
        }
        "ping" => {
            eprintln!("[ec-hook] ping: ok");
        }
        other => {
            eprintln!("未知命令: {other}");
            process::exit(2);
        }
    }
}

/// 解析 --data 参数并写入 EC_DATA_ROOT 环境变量。
fn parse_data_arg(args: &[String]) {
    if let Some(v) = arg_value(args, "--data") {
        if env::var("EC_DATA_ROOT").is_err() {
            // 不覆盖显式设置的 EC_DATA_ROOT
            unsafe { env::set_var("EC_DATA_ROOT", v); }
        }
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}
