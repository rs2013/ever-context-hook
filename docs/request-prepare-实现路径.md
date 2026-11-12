# request.prepare 能力缺失——实现路径建议

> 写给 ww3 维护者：EC hook 需要改写请求（messages/tools/system），当前 `params.request` 只读是阻塞项。

## EC 需要改什么

在 hook.stream 的 invoke 之后、core 将请求发给渠道之前，EC 需要对请求做三处修改：

| 修改点 | 内容 | 大小 |
|--------|------|------|
| `messages` 前插入一条 system 消息 | 管理区投影（长期契约 + 历史块摘要 + recall 提示） | ~800–2000 token |
| `tools` 数组追加 | `recall` 函数定义 | ~200 token |
| 首条 system 消息增强 | 追加 EC 指令（"对话已压缩，用 recall 召回"） | ~100 token |

合计约 1.1–2.3K token，远小于上下文窗口，但**对 EC 机制是必需的**——没有管理区投影，模型看不到历史摘要；没有 recall 工具，模型无法主动召回。

## 三种实现路径

### 路径 A：同一进程两阶段（推荐）

**协议变更**：`hook.stream` 的 params.request 仍只读下发，但 hook 在事件循环开始前用一帧 `request` 回写改写后的请求。

```
core ──invoke(hook.stream, {config, request, ...})──▶ hook
hook ──request({messages: [...], tools: [...]})────▶ core   # ⬅ 新增帧：改写请求（可选）
core ──push(text.delta, ...)───────────────────────▶ hook   # 事件循环开始（同现在）
hook ──event(text.delta, ...)──────────────────────▶ core
...
core ──push(upstream.end, ...)─────────────────────▶ hook
hook ──result({ok: true})──────────────────────────▶ core
```

**帧格式**：

```json
// hook → core（在第一个 push 之前，紧接 invoke 之后）
{"op": "request", "messages": [...], "tools": [...], "system": "..."}

// 也可以只传增量（推荐——避免 hook 拼接完整 request）
{"op": "request", "patch": {
  "messages.prepend": [{"role": "system", "content": "管理区..."}],
  "tools.append": [{"type": "function", "function": {...}}],
  "system.landing": "使用 recall 工具搜索已压缩的历史内容。"
}}

// core 回复（确认改写已应用）
{"op": "ack", "ok": true}
```

**优点**：
- 不上额外的进程 spawn（request.prepare 和 hook.stream 共用同一进程，零额外延迟）
- Hook 可以在看了 request 内容后再决定改写（例如不同 model 用不同的管理区模板）
- 向后兼容：不实现此帧的 hook 跳过，core 视为无改写
- 增量 patch 语义：hook 只传自己要改的部分，core 做合并，hook 不需要"理解完整 request"

**缺点**：
- hook.stream 的双向流增加了一个帧类型（op=request），破坏了当前"invoke → push loop → result"的简洁性
- 链上有多个 hook 时，改写需要**正向传播**（H0 的改写 → H1 看到改写后的 request），与当前事件**反向传播**方向相反

### 路径 B：独立 `request.prepare` 方法

在 manifest 中声明新能力 `request.prepare`，core 在 `hook.stream` 之前调用：

```
core ──invoke(request.prepare, {config, request})──▶ hook  # 独立进程（或不——见下）
hook ──result({request: {...}})────────────────────▶ core
```

**关于进程模型**：
- B1（独立 spawn）：每次对话 spawn 两次（prepare + stream），额外 12–70ms 延迟
- B2（同一进程）：core 先调 `request.prepare` 再调 `hook.stream`，进程内状态机串行，但需要 `execution.conversation = "call"` 支持"一进程两方法"

**B2 的时序**：

```
core spawns hook 进程
core ──invoke(request.prepare, {request})──▶ hook
hook ──result({request: modified})─────────▶ core
core ──invoke(hook.stream, {request: modified, ...})──▶ hook  # 同一进程
hook ──event(...)──▶ core
...
```

**优点**：
- 语义清晰：`request.prepare` 是"改写请求"的专用方法
- 便于测试：可单独调 `request.prepare` 而不走完整对话
- 链上正向传播自然：H0.prepare → H1.prepare → ... → H0.stream → H1.stream

**缺点**：
- 同一进程两轮 invoke ⇒ 需要 hook 实现两个方法，或用一个状态机
- 比路径 A 多一次 invoke→result 往返（但仍在同一进程内，仅 JSONL 序列化开销）

### 路径 C：放宽 params.request 只读约束（最简，不推荐）

直接把 `params.request` 改成可变的：hook 在 `hook.stream` 开始前修改 `params.request` 对象，core 在 hook 返回 result 后读取。

**不推荐理由**：JSONL 是单向流，core 无法"等 hook 改完再读"——hook 不返回 request 帧，core 不知道该不该等、等多久。

## 推荐：路径 A（同一进程两阶段，增量 patch）

理由：
1. **零额外 spawn**：EC 已经是每请求一个进程，不想再翻倍
2. **增量语义安全**：hook 只说自己要加什么，不承担"构造完整 request"的责任——不会意外丢失其他 hook 的修改
3. **检查点明确**：core 在处理 `request` 帧（带 patch）后发 `ack`，hook 知道改写已生效，再开始事件循环

### 链上正向传播的解决方案

当前 hook 链的 push/event 是反向的（响应从链尾往链头），但 request 改写是正向的（H0 先改写，H1 看到改写后的）。对于路径 A：

```
core 同时启动所有 hook 进程（或顺序启动，取决于实现）
for i in 0..n:
    hook[i] ──request(patch)──▶ core
core 按链顺序合并 patch（H0 → H1 → ... → Hn）
core 把最终 merged request 下发给渠道
core 开始 push 事件到链尾 Hn（反向传播，同现在）
```

或者更简单的实现：先让 hook 返回 `request` 帧，core 收集所有 hook 的 request 帧后合并，再开始 push 事件。hook 的 `request` 帧是可选的——不发的 hook 视为无改写。

### ww3 core 需要改的地方

`runtime/hook.go` 的 `RunHooks`：

```go
// 1. 所有 hook 启动后，先收一轮 request 帧（正向，H0 → Hn-1）
mergedRequest := req  // 初始为原始 request
for i := 0; i < n; i++ {
    c := calls[i]
    if c == nil { continue }
    // 发一个特殊的 "prepare" push，hook 用 request 帧回复
    // 或者：hook 在 invoke 响应后、第一个 push 前主动发 request 帧
    // ...
    // core 收到 hook 的 request 帧后合并
    mergedRequest = applyPatch(mergedRequest, patch)
}

// 2. 用合并后的 request 发给渠道（ChatStream 已有的逻辑）

// 3. 开始事件循环（push 到链尾，反向，同现在）
```

`HOOK_PROTOCOL.md` 新增帧类型：

```markdown
### request 帧（hook → core，可选）

在 invoke 之后、第一个 push 之前，hook **可以**发一帧 `request` 来改写请求。
core 按链顺序（H0→Hn）收集所有 `request` 帧，逐层合并 patch，最后发给渠道。

帧格式：

```json
{"op": "request", "patch": {
  "messages.prepend": [{"role": "system", "content": "..."}],
  "tools.append": [{"type": "function", "function": {...}}],
  "system.append": "额外的系统指令文本"
}}
```

core 收到后回复 `{"op": "ack", "ok": true}`，然后开始 push 事件。
不发的 hook 视为无改写。
```

### 在 EC hook 中的对应改动

`hook.rs` 的 `handle_stream`：

```rust
// ── Phase 1: 请求前处理 ──
// 1. 计算管理区投影
let mgmt = ec::projection::build_management_area(session.ec_state.as_ref());
let recall_tool = ec::projection::recall_tool_def();

// 2. 发 request 帧给 core
let patch = serde_json::json!({
    "messages.prepend": [{
        "role": "system",
        "content": mgmt,
        "name": "ever_context"  // 标记以便后续识别
    }],
    "tools.append": [recall_tool],
});
s.request_patch(&patch)?;

// 3. 等 core ack
s.wait_ack()?;

// ── Phase 2: 事件透传（同现在）──
// ...
```

## 小结

| 维度 | 路径 A（同进程两阶段） | 路径 B（独立方法） | 路径 C（放宽只读） |
|------|----------------------|-------------------|-------------------|
| 新增 spawn | 0 | 0 (B2) 或 1 (B1) | 0 |
| 协议改动量 | +1 帧类型 | +1 方法 | 语义变动 |
| 正向传播实现 | 简单（收集帧） | 简单（逐层调） | N/A |
| 向后兼容 | 高（可选帧） | 高（可选能力） | 低（破环契约） |
| EC 实现复杂度 | 低 | 低 | N/A |

**建议落地步骤**：
1. 在 `HOOK_PROTOCOL.md` 写入 `request` 帧规范（路径 A）
2. 在 `runtime/hook.go` 的 `RunHooks` 新增 Phase 0（收 request 帧 → 合并 → ack）
3. 用 loopguard-hook 验证：不做改写，确保不实现此帧的 hook 正常工作（向后兼容）
4. EC hook 实现 request 帧的发包逻辑，功能打通