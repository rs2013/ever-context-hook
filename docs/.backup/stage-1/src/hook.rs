//! hook.stream 处理入口（协议胶水层）。
//!
//! ## 主流程（代理版）
//!
//! ```
//! 1. 解析 params → HookConfig（会话 ID = 首条 user SHA）
//! 2. 定位 JSONL → 读叶子 → 沿 parent_id 上溯祖先链
//! 3. 构建投影（管理区 = 祖先链的 ec 字段渲染）
//! 4. 事件透传（纯透传，零处理）
//! 5. upstream.end 时：append 新消息 → 更新叶子 → 压缩判断
//! ```
//!
//! ## 当前约束
//!
//! `params.request` 只读（`request.prepare` 未实现），因此管理区投影**暂不能注入**。
//! 当前实现：纯透传 + JSONL 维护（append + 叶子更新 + 压缩判断）。

use crate::ec::{self, HookConfig};
use crate::rpc::{self, Stream};

/// manifest.json（ww3 插件注册用）。
pub fn manifest_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "id": "ever-context",
        "name": "Ever-Context 滚动上下文管理器",
        "version": "0.2.0",
        "kind": "hook",
        "protocol": { "min": 1, "max": 1 },
        "execution": {
            "control": "call",
            "conversation": "call",
            "max_workers": 8
        },
        "capabilities": {
            "control": ["config.schema", "config.get", "ping", "health.check"],
            "conversation": ["hook.stream", "request.prepare"]
        },
        "icon": "database",
        "color": "#722ed1",
        "description": "在固定上下文预算内实现超长任务的不间断执行。双上下文 JSONL 持久化 + 管理区投影 + 树状分叉召回。"
    })).unwrap()
}

/// hook.stream 的主入口。
///
/// 返回退出码（0 正常，1 错误）。
pub fn handle_stream(s: &mut Stream) -> i32 {
    let cfg = HookConfig::from_params(&s.params);
    log::info!("[ec-hook] hook_id={} chain={}/{} model={} session={}",
        cfg.hook_id, cfg.chain_index, cfg.chain_total, cfg.model, cfg.session_id);

    // 禁用时纯透传
    if !cfg.ec.enabled {
        return pass_through(s);
    }

    // ── 第 1 步：定位 JSONL，上溯祖先链 ──
    let store = ec::store::Store::new(&cfg.data_root);
    let session_id = &cfg.session_id;

    // 读叶子（定位文件）
    let leaf = store.read_leaf(session_id);
    let chain = match &leaf {
        Some(l) => store.load_chain(session_id, l).unwrap_or_default(),
        None => Vec::new(), // 新会话
    };
    log::info!("[ec-hook] session={session_id} leaf={:?} chain_len={}", leaf, chain.len());

    // ── 第 2 步：构建投影（等 request.prepare 就绪后注入）──
    let management = ec::projection::build_management_area(&chain, &cfg.window);
    if !management.is_empty() {
        log::info!("[ec-hook] management area: {} chars", management.len());
        // TODO: request.prepare 就绪后，把 management 注入到 messages 开头
    }

    // ── 第 3 步：事件透传循环 ──
    // 收集响应中的 assistant 消息（用于 append 到 JSONL）
    let mut response_text = String::new();
    let response_tool_calls: Vec<crate::ec::types::ToolCall> = Vec::new();
    let mut last_prompt_tokens: u32 = 0;
    let mut last_completion_tokens: u32 = 0;

    let exit_code = loop {
        match s.next() {
            Ok(Some(ev)) => {
                match ev.event.as_str() {
                    "upstream.end" | "upstream.done" => {
                        log::info!("[ec-hook] upstream end");
                        break 0;
                    }
                    "text.delta" => {
                        if let Some(content) = ev.data.get("content").and_then(|v| v.as_str()) {
                            response_text.push_str(content);
                        }
                        if let Err(e) = s.event(&ev.event, ev.data) {
                            log::error!("[ec-hook] 写下游事件失败: {e}");
                            let _ = s.terminate(rpc::errkind::CONN, "request", &format!("写下游事件失败: {e}"));
                            break 1;
                        }
                    }
                    "tool_call.delta" => {
                        // 收集 tool_call（首片带 id/name，后续带 arguments_fragment）
                        if let Err(e) = s.event(&ev.event, ev.data) {
                            log::error!("[ec-hook] 写下游事件失败: {e}");
                            let _ = s.terminate(rpc::errkind::CONN, "request", &format!("写下游事件失败: {e}"));
                            break 1;
                        }
                    }
                    "usage" => {
                        if let Some(pt) = ev.data.get("prompt_tokens").and_then(|v| v.as_u64()) {
                            last_prompt_tokens = pt as u32;
                        }
                        if let Some(ct) = ev.data.get("completion_tokens").and_then(|v| v.as_u64()) {
                            last_completion_tokens = ct as u32;
                        }
                        if let Err(e) = s.event(&ev.event, ev.data) {
                            log::error!("[ec-hook] 写下游事件失败: {e}");
                            break 1;
                        }
                    }
                    _ => {
                        // 其余事件原样透传
                        if let Err(e) = s.event(&ev.event, ev.data) {
                            log::error!("[ec-hook] 写下游事件失败: {e}");
                            let _ = s.terminate(rpc::errkind::CONN, "request", &format!("写下游事件失败: {e}"));
                            break 1;
                        }
                    }
                }
            }
            Ok(None) => {
                log::info!("[ec-hook] core 关闭流");
                break 0;
            }
            Err(e) => {
                log::error!("[ec-hook] 读取事件失败: {e}");
                break 1;
            }
        }
    };

    // ── 第 4 步：append 新消息 + 更新叶子 + 压缩判断 ──
    if exit_code == 0 && !response_text.is_empty() {
        // 计算新消息的 parent_id：当前 leaf（定位文件里的），或请求最后一条原生消息
        let parent = leaf.clone().or_else(|| {
            // 无 leaf（新会话）：从请求最后一条原生消息算
            cfg.messages.iter()
                .rev()
                .find(|m| !ec::identity::is_projection_message(m))
                .map(|m| ec::identity::message_id(&ec::identity::feature_text(m), None))
        });

        // 构造 assistant 响应记录
        let response_rec = crate::ec::types::MessageRecord {
            id: ec::identity::message_id(
                &format!("assistant:{}", response_text),
                parent.as_deref(),
            ),
            parent_id: parent.clone(),
            role: "assistant".to_string(),
            content: response_text.clone(),
            tool_calls: response_tool_calls.iter().map(|tc| crate::ec::types::ToolCallRecord {
                id: tc.id.clone(),
                type_: tc.type_.clone(),
                function: crate::ec::types::ToolCallFunction {
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                },
            }).collect(),
            tool_call_id: None,
            tool_name: None,
            name: None,
            ec: None,
        };

        let _ = store.append(session_id, &[response_rec.clone()]);
        let _ = store.update_leaf(session_id, &response_rec.id);
        log::info!("[ec-hook] appended assistant response, leaf={}", response_rec.id);

        // 压缩判断（用估算 token，跳过 recall 内容已在 estimate 内处理）
        let projected = ec::budget::estimate_prompt_tokens(&cfg.messages);
        let _ = ec::compress::decide(
            projected as u64,
            chain.iter().filter(|r| r.ec.is_some()).count() as u64 * 20_000, // 粗略的 last_compaction_tokens
            cfg.window.hard_budget,
            cfg.window.budget.soft_threshold,
            cfg.ec.growth_nudge,
            cfg.ec.pressure_threshold,
        );
        // TODO: 决策为 Soft/Hard 时，触发 LLM 摘要（需旁路调用通道）
        let _ = last_prompt_tokens; // 预留：后续用真实 usage 校准
        let _ = last_completion_tokens;
    }

    exit_code
}

/// request.prepare 的处理入口（一次性方法，无事件流）。
///
/// 输入：params = `{request, config, hook_id, chain}`。
/// 输出：改写后的 request（注入管理区 + 工具）或错误（硬预算门拒绝）。
///
/// 与 hook.stream 不同：prepare 阶段在渠道调用之前，只有 request 没有响应流。
/// EC 在这里：
/// 1. 定位 JSONL，回溯祖先链
/// 2. 构建管理区（压缩块摘要）
/// 3. 硬预算门检查（超线 → 拒绝，返回 prompt_too_long）
/// 4. 改写 messages：前置 system + 管理区 + 尾部原生
pub fn handle_prepare(req_id: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    let cfg = HookConfig::from_params(params);
    log::info!("[ec-prepare] hook_id={} chain={}/{} model={} session={}",
        cfg.hook_id, cfg.chain_index, cfg.chain_total, cfg.model, cfg.session_id);

    if !cfg.ec.enabled {
        return Ok(serde_json::json!({})); // 禁用：不改写
    }

    // 1. 定位 JSONL，回溯祖先链
    let store = ec::store::Store::new(&cfg.data_root);
    let mut leaf = store.read_leaf(&cfg.session_id);
    // 新对话（首条 user 找不到 JSONL）：用请求 messages 创建完整 JSONL 树（纯规则，不调 LLM）
    let is_new = leaf.is_none() && !store.session_exists(&cfg.session_id);
    if is_new {
        let imported = store.import_messages(&cfg.session_id, &cfg.messages).unwrap_or(0);
        log::info!("[ec-prepare] 新对话：导入 {} 条消息到 JSONL", imported);
        leaf = store.read_leaf(&cfg.session_id);
        // 纯规则分段 + 快速建索引（title/summary 空，不调 LLM，不阻塞对话）
        match ec::index::open_index(std::path::Path::new(&format!("{}/index", cfg.data_root))) {
            Ok(index) => {
                match ec::index::index_session(&index, &store, &cfg.session_id, cfg.window.budget.target_tokens) {
                    Ok(n) => log::info!("[ec-prepare] 分段建索引 {} 块", n),
                    Err(e) => log::warn!("[ec-prepare] 分段建索引失败（fail-open）: {e}"),
                }
            }
            Err(e) => log::warn!("[ec-prepare] 打开索引失败（fail-open）: {e}"),
        }
    }
    let chain = match &leaf {
        Some(l) => store.load_chain(&cfg.session_id, l).unwrap_or_default(),
        None => Vec::new(),
    };

    // 新对话 + 无压缩块 + 投影超软阈值 → 触发索引追赶（后台子进程，detach）
    if is_new && chain.iter().all(|r| r.ec.is_none()) {
        let projected = ec::budget::estimate_prompt_tokens(&cfg.messages);
        if projected as u64 > cfg.window.budget.soft_threshold as u64 {
            spawn_catchup(&cfg);
        }
    }

    // 2. 构建管理区
    let management = ec::projection::build_management_area(&chain, &cfg.window);

    // 3. 预算检查：估算全量 prompt token
    let projected = ec::budget::estimate_prompt_tokens(&cfg.messages);
    let hard_budget = cfg.window.hard_budget;
    let degrade_target = cfg.window.budget.degrade_target;
    let over_hard = projected > hard_budget;
    if over_hard {
        log::info!("[ec-prepare] 超硬线：估算 {} token > 硬预算 {}，降级为尾部贴合（目标={} = 软阈值 {} × {:.0}%）",
            projected, hard_budget, degrade_target, cfg.window.budget.soft_threshold,
            cfg.window.budget.degrade_target_ratio * 100.0);
    }

    // 4. 改写 messages
    // 前置 system 原样保留；管理区注入；尾部按预算贴合。
    let msgs = &cfg.messages;
    let mut new_msgs: Vec<serde_json::Value> = Vec::new();

    // 4a. 前置 system（开头连续 system 消息原样保留）
    let mut sys_end = 0usize;
    while sys_end < msgs.len() && (msgs[sys_end].role == "system" || msgs[sys_end].role == "developer") {
        sys_end += 1;
    }
    for m in &msgs[..sys_end] {
        new_msgs.push(serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
    }

    // 4b. 管理区（有压缩块时注入）
    if !management.is_empty() {
        new_msgs.push(serde_json::json!({
            "role": "system",
            "content": management,
            "name": "ec_management"
        }));
    }

    // 4c. 尾部原生消息
    if over_hard {
        // 超硬线：尾部贴合——只保留能塞进降级目标的最近消息。
        // degrade_target = soft_threshold × degrade_target_ratio（默认 0.6），比硬预算更激进，
        // 避免每轮新内容都重新触发降级（留出余量让日常增长交回压缩维护）。
        let management_tokens = ec::projection::estimate_management_tokens(&chain, &cfg.window);
        let notice = HARD_LINE_NOTICE;
        let notice_tokens = (notice.len() as f64 / 3.0).ceil() as u32 + 4;
        let sys_tokens: u32 = msgs[..sys_end].iter().map(ec::budget::estimate_message_tokens).sum();
        let room = degrade_target
            .saturating_sub(sys_tokens)
            .saturating_sub(management_tokens)
            .saturating_sub(notice_tokens);
        let tail_start = ec::budget::tail_fit_start(&msgs[sys_end..], room) + sys_end;
        let kept_tokens: u32 = msgs[tail_start..].iter().map(ec::budget::estimate_message_tokens).sum();
        log::info!("[ec-prepare] 硬线降级 room={} tail_start={} kept={}条 kept_tokens={}",
            room, tail_start, msgs.len() - tail_start, kept_tokens);
        for m in &msgs[tail_start..] {
            new_msgs.push(serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
        }
        // 硬线提示（放最后，告诉模型有历史滚出）
        new_msgs.push(serde_json::json!({
            "role": "user",
            "content": notice,
            "name": "ec_hard_line_notice"
        }));
    } else {
        // 未超线：全量保留
        for m in &msgs[sys_end..] {
            new_msgs.push(serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
        }
    }

    // 构造改写后的 request：复制原 request，替换 messages
    let mut new_req = params.get("request")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if new_req.is_null() {
        return Err("params 缺少 request 字段".to_string());
    }
    new_req["messages"] = serde_json::Value::Array(new_msgs);

    log::info!("[ec-prepare] 投影完成：原始 {} 条 -> 投影 {} 条（硬线降级={}）",
        msgs.len(), new_req["messages"].as_array().map(|a| a.len()).unwrap_or(0), over_hard);
    // 契约：result 返回 {"request": 改写后的完整请求}（与 ww3 PrepareResult 对齐）
    Ok(serde_json::json!({ "request": new_req }))
}

/// 硬线降级提示（对齐 Pi 版 EC_HARD_LINE_NOTICE）：告诉模型有历史滚出，可用 ec_recall 召回。
pub const HARD_LINE_NOTICE: &str = "[EC 提示] 本对话历史较长，早期内容已从当前上下文滚出（仍保留在本地索引中）。如需查找早先的决策、约束或结果，请使用 ec_recall 工具检索，不要假定这些内容仍然可见。";

/// spawn 一个 detach 的索引追赶子进程（ec-hook 自身 catchup 子命令）。
///
/// 子进程独立于当前 hook 进程存活（不随 hook 退出而终止），后台逐段补摘要。
/// 核心路径/数据目录/插件目录从环境变量读取（部署方设置），缺省用相对 data_root 反推。
fn spawn_catchup(cfg: &HookConfig) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[ec-hook] 取自身路径失败，无法 spawn catchup: {e}");
            return;
        }
    };
    // core 路径：环境变量 WW3_CORE_EXE 优先
    let core_exe = std::env::var("WW3_CORE_EXE").unwrap_or_else(|_| "ww3-core".to_string());
    // core 数据目录 = EC 插件数据目录上溯两级（<data>/core/plugins/ever-context → <data>/core）
    let core_data = std::env::var("WW3_CORE_DATA").unwrap_or_else(|_| {
        let p = std::path::Path::new(&cfg.data_root);
        p.parent().and_then(|p| p.parent()).map(|p| p.display().to_string()).unwrap_or_default()
    });
    // 插件目录 = EC 插件数据目录上溯一级（→ <data>/core/plugins）
    let plugins_dir = std::env::var("WW3_PLUGINS_DIR").unwrap_or_else(|_| {
        std::path::Path::new(&cfg.data_root).parent().map(|p| p.display().to_string()).unwrap_or_default()
    });

    let child = std::process::Command::new(&exe)
        .args([
            "catchup",
            "--session", &cfg.session_id,
            "--data", &cfg.data_root,
            "--core", &core_exe,
            "--core-data", &core_data,
            "--plugins", &plugins_dir,
            "--model", &cfg.model,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(_) => log::info!("[ec-hook] 已 spawn 索引追赶子进程（session={}, core={}, core_data={}）",
            cfg.session_id, core_exe, core_data),
        Err(e) => log::warn!("[ec-hook] spawn catchup 失败: {e}"),
    }
}

/// 禁用时纯透传。
fn pass_through(s: &mut Stream) -> i32 {
    loop {
        match s.next() {
            Ok(Some(ev)) => {
                if ev.event == "upstream.end" || ev.event == "upstream.done" {
                    return 0;
                }
                if let Err(e) = s.event(&ev.event, ev.data) {
                    log::error!("[ec-hook] 写下游事件失败: {e}");
                    return 1;
                }
            }
            Ok(None) | Err(_) => return 0,
        }
    }
}
