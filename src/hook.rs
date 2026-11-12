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
//! ## 两个入口
//!
//! - `handle_prepare`（request.prepare）：请求前改写——增量落盘 + 管理区注入 + 硬预算门降级。
//! - `handle_stream`（hook.stream）：响应流透传 + 收集 assistant 回复 append 回 JSONL + 更新叶子。

use crate::ec::{self, HookConfig};
use crate::rpc::{self, Stream};

/// manifest（ww3 插件注册用，PLUGIN_PROTOCOL §3）。
pub fn manifest_value() -> serde_json::Value {
    serde_json::json!({
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
        "capabilities": capabilities_value(),
        "icon": "database",
        "color": "#722ed1",
        "description": "在固定上下文预算内实现超长任务的不间断执行。双上下文 JSONL 持久化 + 管理区投影 + 树状分叉召回。"
    })
}

/// 能力清单：control 为标准生命周期五项，conversation 为本 hook 的两个方法。
pub fn capabilities_value() -> serde_json::Value {
    serde_json::json!({
        "control": ["initialize", "capabilities.list", "ping", "health.check", "shutdown"],
        "conversation": ["hook.stream", "request.prepare"]
    })
}

/// manifest.json 文本（`ec-hook manifest` 子命令输出）。
pub fn manifest_json() -> String {
    serde_json::to_string_pretty(&manifest_value()).unwrap()
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

    // ── 第 2 步：构建投影（仅供日志观察；真正注入在 handle_prepare 完成）──
    let management = ec::projection::build_management_area(&chain, &cfg.window, session_id);
    if !management.is_empty() {
        log::info!("[ec-hook] management area: {} chars（注入由 handle_prepare 完成）", management.len());
    }

    // ── 第 3 步：事件透传循环 ──
    // 收集响应中的 assistant 消息（用于 append 到 JSONL）
    let started = std::time::Instant::now();
    let response_started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_millis() as u64);
    let mut response_text = String::new();
    let mut response_reasoning = String::new();
    let mut response_usage: Option<serde_json::Value> = None;
    let mut stop_reason: Option<String> = None;
    let mut upstream_ok = false;
    let mut response_tool_calls: Vec<crate::ec::types::ToolCall> = Vec::new();
    let mut last_prompt_tokens: u32 = 0;
    let mut last_completion_tokens: u32 = 0;

    let exit_code = loop {
        match s.next() {
            Ok(Some(ev)) => {
                match ev.event.as_str() {
                    "upstream.end" | "upstream.done" => {
                        upstream_ok = ev.data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                        log::info!("[ec-hook] upstream end ok={upstream_ok}");
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
                        // 收集 tool_call（bug ④ 修复）：按 index 分组聚合，
                        // 首片带 id/type/name，后续带 arguments_fragment。
                        let idx = ev.data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        while response_tool_calls.len() <= idx {
                            response_tool_calls.push(crate::ec::types::ToolCall {
                                id: String::new(),
                                type_: "function".to_string(),
                                function: crate::ec::types::ToolCallFunction { name: String::new(), arguments: String::new() },
                            });
                        }
                        let tc = &mut response_tool_calls[idx];
                        if let Some(v) = ev.data.get("id").and_then(|v| v.as_str()) { tc.id = v.to_string(); }
                        if let Some(v) = ev.data.get("type").and_then(|v| v.as_str()) { tc.type_ = v.to_string(); }
                        if let Some(v) = ev.data.get("name").and_then(|v| v.as_str()) { tc.function.name = v.to_string(); }
                        if let Some(v) = ev.data.get("arguments_fragment").and_then(|v| v.as_str()) {
                            tc.function.arguments.push_str(v);
                        }
                        if let Err(e) = s.event(&ev.event, ev.data) {
                            log::error!("[ec-hook] 写下游事件失败: {e}");
                            let _ = s.terminate(rpc::errkind::CONN, "request", &format!("写下游事件失败: {e}"));
                            break 1;
                        }
                    }
                    "usage" => {
                        response_usage = Some(ev.data.clone());
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
                    "reasoning.delta" | "finish_reason" => {
                        if ev.event == "reasoning.delta" {
                            if let Some(c) = ev.data.get("content").and_then(|v| v.as_str()) { response_reasoning.push_str(c); }
                        } else {
                            stop_reason = ev.data.get("reason").and_then(|v| v.as_str()).map(str::to_owned);
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
    // 有正文或有工具调用都落盘（bug ④ 修复：纯工具调用的 assistant 回复也要落盘）
    if exit_code == 0 && upstream_ok && (!response_text.is_empty() || !response_tool_calls.is_empty() || !response_reasoning.is_empty()) {
        // 计算新消息的 parent_id：当前 leaf（定位文件里的），或请求最后一条原生消息
        let parent = leaf.clone().or_else(|| {
            // 无 leaf（新会话）：从请求最后一条原生消息算
            cfg.messages.iter()
                .rev()
                .find(|m| !ec::identity::is_projection_message(m))
                .map(|m| ec::identity::message_id(&ec::identity::feature_text(m), None))
        });

        // feature_text 按 assistant 规则：有工具调用时用 tool_calls 拼接（与 identity::feature_text 一致）
        let feature = if response_tool_calls.is_empty() {
            format!("assistant:{}", response_text)
        } else {
            let calls: Vec<String> = response_tool_calls.iter()
                .map(|tc| format!("{}|{}", tc.function.name, tc.function.arguments))
                .collect();
            format!("assistant:{}", calls.join("\x1f"))
        };

        // 构造 assistant 响应记录
        let response_rec = crate::ec::types::MessageRecord {
            id: ec::identity::message_id(&feature, parent.as_deref()),
            parent_id: parent.clone(),
            role: "assistant".to_string(),
            timestamp: response_started,
            model: (cfg.model != "unknown").then(|| cfg.model.clone()),
            channel: s.params.get("upstream").and_then(|r| r.get("channel_id")).and_then(|v| v.as_str())
                .map(str::to_owned),
            thinking: s.params.get("request").and_then(|r| r.get("thinking")).and_then(|v| v.as_bool()),
            reasoning_effort: s.params.get("request").and_then(|r| r.get("reasoning_effort")).and_then(|v| v.as_str()).map(str::to_owned),
            usage: response_usage,
            stop_reason,
            reasoning: (!response_reasoning.is_empty()).then_some(response_reasoning),
            duration_ms: Some(started.elapsed().as_millis() as u64),
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

        match store.append(session_id, &[response_rec.clone()]) {
            Ok(_) => {
                if let Err(e) = store.update_leaf(session_id, &response_rec.id) { log::warn!("[ec-hook] 更新 leaf 失败: {e}"); }
                log::info!("[ec-hook] appended assistant response, leaf={}", response_rec.id);
            }
            Err(e) => log::warn!("[ec-hook] assistant 落盘失败: {e}"),
        }

        // 压缩触发已移到 prepare 阶段（那里有完整 messages 且可 spawn catchup）。
        // 这里只记录 usage，供后续估算校准用。
        log::info!("[ec-hook] usage prompt={} completion={}", last_prompt_tokens, last_completion_tokens);
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
pub fn handle_prepare(_req_id: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    let cfg = HookConfig::from_params(params);
    log::info!("[ec-prepare] hook_id={} chain={}/{} model={} session={}",
        cfg.hook_id, cfg.chain_index, cfg.chain_total, cfg.model, cfg.session_id);

    if !cfg.ec.enabled {
        return Ok(serde_json::json!({})); // 禁用：不改写
    }

    // 1. 定位 JSONL，回溯祖先链
    let store = ec::store::Store::new(&cfg.data_root);
    let mut leaf = store.read_leaf(&cfg.session_id);
    // 定位文件失效但会话存在：用请求尾部在 JSONL 中重新定位（bug ② 修复，防分叉/恢复后丢失祖先链）
    if leaf.is_none() && store.session_exists(&cfg.session_id) {
        let native: Vec<&crate::ec::types::ChatMessage> = cfg.messages.iter()
            .filter(|m| !ec::identity::is_projection_message(m))
            .collect();
        let tail: Vec<String> = native.iter()
            .rev().take(16).rev() // 尾部最多 16 条（旧→新），足够精确定位
            .map(|m| ec::identity::feature_text(m))
            .collect();
        match store.locate(&cfg.session_id, &tail) {
            Ok(Some(l)) => {
                let _ = store.update_leaf(&cfg.session_id, &l);
                leaf = Some(l);
                log::info!("[ec-prepare] leaf 定位文件失效，已用 locate 重新定位");
            }
            Ok(None) => log::warn!("[ec-prepare] locate 未命中，保持 leaf=None（按新对话处理）"),
            Err(e) => log::warn!("[ec-prepare] locate 失败: {e}"),
        }
    }
    // 新对话（首条 user 找不到 JSONL）
    let is_new = leaf.is_none() && !store.session_exists(&cfg.session_id);

    // 增量落盘（幂等）：把请求里的原生消息同步到 JSONL。
    // 修复 bug ①「新 user 消息永不落盘」：每轮 prepare 都把本轮 user/assistant
    // 追加进 JSONL（append 按 id 去重，旧消息跳过），树不再退化成 assistant→assistant 直链。
    let imported = store.import_messages(&cfg.session_id, &cfg.messages).unwrap_or(0);
    if imported > 0 {
        log::info!("[ec-prepare] {} 落盘 {} 条新消息", if is_new { "新对话" } else { "增量" }, imported);
        leaf = store.read_leaf(&cfg.session_id);
    }

    if is_new {
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

    // 日常增量压缩触发：未摘要 token 攒够一块（≥ target_tokens）且投影超软阈值
    // → 后台追赶子进程补摘要（detach）。不只在 is_new：长会话每轮新增内容也会攒段。
    // 增量值 = target_tokens（对齐需求 ②：增量计算用 target 且统一算法）。
    // 未摘要 token = 游标之后（尚未折叠）的记录；不能按「记录是否有 ec 字段」统计，
    // 因为 catchup 只在每块末条盖 ec，块内前 N-1 条永远 ec.is_none()，会被重复计入，
    // 导致每轮 prepare 空跑 spawn_catchup（见 docs/EC-旧对话实测记录.md 的 20,991）。
    let cursor = ec::catchup::cursor_index(&chain);
    let unsummarized_tokens: u32 = chain[cursor.map(|c| c + 1).unwrap_or(0)..].iter()
        .map(ec::budget::estimate_record_tokens)
        .sum();
    if unsummarized_tokens >= cfg.window.budget.target_tokens {
        let projected = ec::budget::estimate_prompt_tokens(&cfg.messages);
        if projected as u64 > cfg.window.budget.soft_threshold as u64 {
            log::info!("[ec-prepare] 未摘要 {} token ≥ 目标 {} 且投影 {} > 软阈值 {}，spawn 追赶",
                unsummarized_tokens, cfg.window.budget.target_tokens, projected, cfg.window.budget.soft_threshold);
            spawn_catchup(&cfg);
        }
    }

    // 2. 构建管理区
    let management = ec::projection::build_management_area(&chain, &cfg.window, &cfg.session_id);
    let mgmt_chars = management.len(); // 先取长度：下面 json! 会移动 management
    // 管理区占用 prompt 预算；估算必须计入，否则实际 prompt 可能超硬线。
    let management_tokens = ec::projection::estimate_management_tokens(&chain, &cfg.window, &cfg.session_id);

    // 3. 预算检查：估算全量 prompt token（原生 messages + 管理区）
    let projected = ec::budget::estimate_prompt_tokens(&cfg.messages)
        .saturating_add(management_tokens);
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

    // 4b. 管理区（有压缩块时注入）。
    // role=user 而非 system（bug ⑧ 修复）：system 角色会改变部分 provider 的语义
    // （如 o1 不支持 system），且管理区是"历史投影"而非系统指令，用 user + name 标记更安全。
    if !management.is_empty() {
        new_msgs.push(serde_json::json!({
            "role": "user",
            "content": management,
            "name": "ec_management"
        }));
    }

    // 4c. 尾部原生消息
    let mut dbg_tail_start: Option<usize> = None; // 仅用于 trace 日志
    if over_hard {
        // 超硬线：尾部贴合——只保留能塞进降级目标的最近消息。
        // degrade_target = soft_threshold × degrade_target_ratio（默认 0.6），比硬预算更激进，
        // 避免每轮新内容都重新触发降级（留出余量让日常增长交回压缩维护）。
        // 无摘要块时用降级提示词（带 few-shot），有摘要块时用硬线提示词
        let notice = if management.is_empty() {
            degraded_notice(&cfg.session_id)
        } else {
            hard_line_notice(&cfg.session_id)
        };
        let notice_tokens = (notice.len() as f64 / 3.0).ceil() as u32 + 4;
        let sys_tokens: u32 = msgs[..sys_end].iter().map(ec::budget::estimate_message_tokens).sum();
        let room = degrade_target
            .saturating_sub(sys_tokens)
            .saturating_sub(management_tokens)
            .saturating_sub(notice_tokens);
        // 首条 user 锚定区是会话身份（session_id）的基石，必须整段保留；
        // 它不是「可裁剪的旧历史」，不能与尾部窗口用 min/max 混在一个窗口里算。
        // 正确做法：锚定区独立保留并先扣其成本，再对锚定区之后的消息做尾部贴合，两段拼接。
        let (anchor_end, tail_start) = ec::budget::anchor_and_tail_start(msgs, sys_end, room);
        let anchor = &msgs[sys_end..anchor_end];
        dbg_tail_start = Some(tail_start);
        log::info!("[ec-prepare] 硬线降级 room={} anchor={}条 tail_start={} anchor_end={} kept={}条",
            room, anchor.len(), tail_start, anchor_end, msgs.len() - tail_start);
        for m in anchor {
            new_msgs.push(serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
        }
        let kept_tokens: u32 = msgs[tail_start..].iter().map(ec::budget::estimate_message_tokens).sum();
        log::info!("[ec-prepare] 硬线降级 kept_tokens={}", kept_tokens);
        for m in &msgs[tail_start..] {
            new_msgs.push(serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
        }
        // 降级提示（放最后，告诉模型有历史滚出）
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

    // 不再注入 function calling 工具定义：
    // 下游/ww3 并无该工具的执行器，注入只会让模型“看得见调不动”。
    // 改为在管理区/降级提示中给出可执行的 CLI 路径（见 ec::bin_hint）。

    // 复盘 trace（INFO）：一轮 request.prepare 的完整决策量，便于事后回放 EC 行为。
    log::info!("[ec-prepare-trace] session={} leaf={} chain={} unsummarized_tokens={} projected={} soft={} hard={} management_chars={} over_hard={} tail_start={} tools={}",
        cfg.session_id, leaf.as_deref().unwrap_or("-"), chain.len(), unsummarized_tokens, projected,
        cfg.window.budget.soft_threshold, hard_budget, mgmt_chars, over_hard,
        dbg_tail_start.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
        new_req["tools"].as_array().map(|a| a.len()).unwrap_or(0));    log::info!("[ec-prepare] 投影完成：原始 {} 条 -> 投影 {} 条（硬线降级={}）",
        msgs.len(), new_req["messages"].as_array().map(|a| a.len()).unwrap_or(0), over_hard);
    // 契约：result 返回 {"request": 改写后的完整请求}（与 ww3 PrepareResult 对齐）
    Ok(serde_json::json!({ "request": new_req }))
}

/// 硬线降级提示：历史已滚出，给出**可执行**的 CLI 路径（不假定下游有对应工具）。
pub fn hard_line_notice(session_id: &str) -> String {
    let bin = ec::bin_hint();
    format!(
        "[EC 提示] 本对话历史较长，早期内容已从当前上下文滚出（仍保留在本地索引中）。\
如需查找早先的决策、约束或结果，可用 bash 执行：\n  {bin} recall --session {session_id} --query <关键词>\n\
  {bin} rag --session {session_id} --query <问题> --core-data <core数据目录>\n\
不要假定这些内容仍然可见。"
    )
}

/// 降级提示词（无摘要块时用）：历史已裁剪且暂无摘要，给出检索样本。
pub fn degraded_notice(session_id: &str) -> String {
    let bin = ec::bin_hint();
    format!(
        "[EC 提示] 本对话历史较长，早期内容已从当前上下文裁剪，且当前暂无可用摘要（压缩维护尚未完成）。\
如需查找早先内容，可用 bash 执行：\n  {bin} recall --session {session_id} --query <关键词>\n\
不要假定这些内容仍然可见。"
    )
}

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

    // 此处是正常对话触发的增量压缩；手工旧会话追赶走 catchup CLI。
    let compact = &cfg.ec.compact;
    let model = compact.model.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or(&cfg.model);
    let thinking = compact.thinking.unwrap_or_else(|| cfg.request_thinking.unwrap_or(false));
    if model == "unknown" { log::warn!("[ec-hook] 会话缺少模型，跳过后台压缩"); return; }
    let child = std::process::Command::new(&exe)
        .args([
            "catchup",
            "--session", &cfg.session_id,
            "--data", &cfg.data_root,
            "--core", &core_exe,
            "--core-data", &core_data,
            "--plugins", &plugins_dir,
            "--model", model,
            "--thinking", if thinking { "true" } else { "false" },
            "--target-tokens", &cfg.window.budget.target_tokens.to_string(),
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
