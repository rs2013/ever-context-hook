# EC 与 ww3 插件契约的符合性 + 承诺兑现缺口

> 契约来源（只读参考仓 `ref/wild-work3`，未改动）：
> - `protocol/meta/meta.go:7`（`Manifest`）、`:47`（`Execution`）、`:55`（`Capabilities`）、`:34`（`ProtocolRng`）
> - `protocol/plugin/plugin.go:36-37`（`ModeCall`/`ModeStdio`）、`:92`（`Lifecycle`）、`:116-123`（`dispatch` 未知方法返回 `not_found`）、`:229`（`Spec`）
> - `core/internal/runtime/hook.go:86`（`hookParams`）、`:99`（`PrepareParams`）、`:115`（`PrepareResult`）、`:20-30`（失败策略）、`:424-431`（`spec()` → `Args: ["call","--data",dir]`）
> - `core/internal/route/route.go:523,533`（按 `capabilities.conversation` 筛 `hook.stream` / `request.prepare`）
> - `core/internal/config/config.go:108-110`（`hooks[].fail_open` 由 core 持有）
> - `protocol/errkind/errkind.go`（错误码全集）
> 本文以 EC 当前源码（`src/rpc.rs`、`src/hook.rs`、`src/main.rs`、`src/ec/config.rs`）为准。

---

## 一、manifest 输出与本 hook 的实际落实

`ec-hook manifest` 当前输出：

```json
{
  "id": "ever-context",
  "name": "Ever-Context 滚动上下文管理器",
  "version": "0.2.0",
  "kind": "hook",
  "protocol": { "min": 1, "max": 1 },
  "execution": { "control": "call", "conversation": "call", "max_workers": 8 },
  "capabilities": {
    "control": ["initialize", "capabilities.list", "ping", "health.check", "shutdown"],
    "conversation": ["hook.stream", "request.prepare"]
  },
  "icon": "database",
  "color": "#722ed1",
  "description": "在固定上下文预算内实现超长任务的不间断执行。双上下文 JSONL 持久化 + 管理区投影 + 树状分叉召回。"
}
```

### 1.1 逐字段：契约语义 → core 怎么用 → EC 怎么落实

| 字段 | 契约语义（meta.go） | core 实际使用 | EC 落实 |
| --- | --- | --- | --- |
| `id` | 插件唯一 ID，渠道实例命名空间前缀 | 路由 `hooks[].plugin_id` 匹配；`initialize` 回 `plugin_id` | `ever-context`；`initialize` 返回同值 ✅ |
| `name` | 展示名 | 面板/商店显示 | ✅ |
| `version` | 插件版本（字符串） | 安装/升级比对（`plugins/<id>/<version>/`） | `0.2.0`；注意 crate 版本是 `0.1.0`（见缺口 G-9） |
| `kind` | `channel` \| `hook` | `hook` → 进拦截器链，不当渠道 | `hook` ✅ |
| `protocol` | 兼容区间 `{min,max}` | 与 core 的协议版本 1 比对 | `{1,1}` ✅ |
| `execution.control` | 控制面进程形态 | （当前 core 用 `call` 拉控制方法） | `call` ✅ |
| `execution.conversation` | 会话面进程形态 | `plugin.Spec.Args = ["call","--data",<插件数据目录>]`（hook.go:428） | `call` ✅ 实测可被 `call --data` 驱动 |
| `execution.max_workers` | 并发上限提示（0=不限） | `Manager.acquire` 限流 | `8` ✅ |
| `execution.idle_timeout_seconds` | stdio 常连空闲回收 | 仅 stdio 形态用 | 未声明（EC 用 call，一请求一进程）— 合理 ✅ |
| `capabilities.control` | 控制面已实现方法 | 面板按此判断能否调；`plugin.dispatch` 未实现的方法回 `not_found` | 声明标准五项并已实现（见 1.2）✅ |
| `capabilities.conversation` | 会话面已实现方法 | `route.go:523` 用 `hook.stream` 筛层；`:533` 用 `request.prepare` 决定是否参与 prepare | 两项都声明且实现 ✅ |
| `default_channels` | 渠道插件预置实例 | 渠道安装时建实例 | 不适用（hook）✅ |
| `multi_channels` | 渠道由用户自建 | 渠道安装 | 不适用 ✅ |
| `icon` / `color` | 面板图标/主题色 | 前端展示 | ✅ |
| `description` | 描述 | 前端展示 | ✅ |

结论：**manifest 的每个字段现在都对应真实实现或明确不适用**。此前 `config.schema` / `config.get` 属于「声明了但没实现」，已从 `capabilities.control` 移除（与 loopguard 的声明式写法不同，EC 改为如实声明）。

### 1.2 RPC 方法落实情况

| 方法 | 契约 | EC 实现 | 实测 |
| --- | --- | --- | --- |
| `initialize` | 回 `{protocol, plugin_id, manifest, capabilities}` | `rpc.rs` | ✅ |
| `capabilities.list` | 回 `{control, conversation}` | 同上 | ✅ |
| `ping` | 回 `"pong"` | 同上 | ✅ |
| `health.check` | 回 `{"state":"ok"}` | 同上 | ✅ |
| `shutdown` | 回 `{"ok":true}` | 同上 | ✅ |
| `request.prepare` | 一次性，`result.request` 整包替换（`hook.go:115`） | `handle_prepare` | ✅ 返回 `{"request": <完整请求>}` |
| `hook.stream` | 双向流，`upstream.end` 收尾（`hook.go:86`） | `handle_stream` | ✅ 透传 + 落盘 |
| 未知方法 | `error.kind=not_found`（`plugin.go:123`，core 靠它判可选能力未实现） | `reply_err("not_found", ...)`，exit 0 | ✅ 修正前是 stderr + exit 1 |

### 1.3 契约参数结构对照（EC 读取的入站字段）

| 契约结构 | 字段 | EC 是否读取 |
| --- | --- | --- |
| `hookParams` | `request` | ✅ `HookConfig::from_params` 读 `request.model/max_tokens/thinking/messages` |
| | `config` | ✅ `EcConfig`（enabled/context_window/hard_budget/soft_threshold/degrade_target_ratio/target_tokens/compact/catchup） |
| | `hook_id` | ✅ 仅日志 |
| | `chain.index/total` | ✅ 仅日志 |
| `upstream.end` 帧 | `{ok, kind?, message?}` | 只读 `ok`；`kind`/`message` **未使用**（见缺口 G-6） |
| `push` 事件 | `text.delta` | ✅ 累积正文 |
| | `reasoning.delta` | ✅ 累积思考正文 |
| | `tool_call.delta` | ✅ 按 `index` 聚合 `id/type/name/arguments_fragment` |
| | `usage` | ✅ 原样落盘 + 取 prompt/completion_tokens |
| | `finish_reason` | ✅ 取 `reason` |
| | 其他 | ✅ 原样透传 |

---

## 二、承诺兑现缺口清单

> 口径：以「用户已明确要求」或「manifest/文档已承诺」为准，不含纯设计设想。P0 = 影响正确性，P1 = 影响能力闭环，P2 = 工程质量。

### P0 — 正确性

| # | 承诺 / 预期 | 现状 | 缺口 |
| --- | --- | --- | --- |
| G-1 | 会话历史在并发（hook 与 catchup 子进程）下不丢数据 | `append`/`import_messages`/`update_leaf` 未持有会话写锁；`set_ec` 走「读全量→写临时→rename」，与并发 append 存在覆盖窗口；`update_leaf` 用固定 `.leaf.tmp` 名 | 三次读改写需合并为一把会话锁下的原子事务；锁改 RAII |
| G-2 | 原始消息无损（双上下文完整性） | `content:null` 已修；但 `ContentPart` 仍只认 `text`/`image_url`，`input_audio` 等未知 part 会整条消息解析失败并被 `log::warn` 跳过（不再静默，但仍丢） | 权威存储改为「保留 `raw` Value + 规范化字段」，未知 part 不丢原文 |
| G-3 | 请求被下游裁剪后仍能正确接续会话树 | `import_messages` 假设入站 messages 从根开始；若下游只发尾部，会把截断首条当新根；`imported==0` 时 prepare 不重读 leaf（已读取则磁盘/内存 leaf 可能不一致） | 每轮校验请求尾部与持久化树的对应关系，而非无条件信任 `.leaf` |

### P1 — 能力闭环

| # | 承诺 / 预期 | 现状 | 缺口 |
| --- | --- | --- | --- |
| G-4 | 三个可配置 LLM：`compact` / `catchup` / `rag` | `compact` 已接线；`catchup` 配置项存在但**无消费者**（`spawn_catchup` 一律用 compact，手工 CLI 只认 `--model` / `EC_CATCHUP_CONFIG`）；`rag` 未实现 | 需先定义「哪条触发路径算旧对话追赶」；建议：`spawn_catchup` 传入 `EC_CATCHUP_CONFIG`（由 `cfg.ec.catchup` 序列化），并让 `--model` 仅在 compact 显式配置时才传 |
| G-5 | 检索增强生成（RAG）：检索 + LLM 生成答案 | ✅ **已实现** `ec-hook rag --session --query --top-k --core-data`：`ec/rag.rs` 取相关度最高 top-k（默认 5，可 `--top-k` 或 `EC_RAG_CONFIG.top_k` 覆盖），组编号片段 prompt 交 LLM，要求只依据片段并给出 `[1][2]` 引用；无命中不调 LLM | 剩余：跨会话 knowledge 的生成式回答未接（只有 `recall --knowledge` 纯检索） |
| G-6 | 上游失败信息可用于决策 | `upstream.end` 的 `kind`/`message` 未读；hook 只记 `ok` | 落盘/日志带上上游错误分类，便于 fail-open 判定与复盘 |
| G-7 | 模型能用上检索能力 | ✅ **已修正**：移除 function calling 工具注入（相关定义已删），改为在管理区汰出提示 / 硬线降级提示里注入**本机二进制真实路径**的 CLI 用法（`ec::bin_hint()`），并在 `docs/EC-CLI使用说明.md §10.1` 给出 skill 文本模板 | 术语统一：只用 `recall` / `rag`，不再出现工具名；历史 JSONL 的旧 `name` 值仅作向后兼容识别 |
| G-8 | 四态推进随实际模型窗口 | `catchup` 用 `WindowConfig::default()` | 改为按会话模型/`ec.context_window` 推导 |

### P2 — 工程质量

| # | 项 | 现状 |
| --- | --- | --- |
| G-9 | 版本一致性 | `Cargo.toml` 0.1.0 / manifest 硬编码 0.2.0 / e2e 插件目录 `ever-context/0.1.0` |
| G-10 | `core_client` 超时 | 无请求超时，上游卡住会长期占会话锁 |
| G-11 | 锁过期 | 30 分钟硬过期、无心跳，长追赶可能被误判失效 |
| G-12 | `config.schema` / `config.get` | 未实现（返回 `not_found`，与 loopguard 一致；如需面板渲染配置表单再补） |
| G-13 | 文档与代码一致性 | README 架构图、`docs/EC-代理版-实施规划.md` 中「31 个测试」等旧数据待同步（当前 40 个测试） |

---

## 三、本次已闭环的契约项（相对上一版）

1. `--help` / `-h` / `help` → 打印完整命令行手册；`--version` / `-V` → 版本（`src/help.rs`）。
2. 二进制更名 `ever-context-hook` → **`ec-hook`**（`Cargo.toml [[bin]]`）。
3. 五个标准生命周期方法补齐（`initialize` / `capabilities.list` / `ping` / `health.check` / `shutdown`）。
4. 未知方法改为 `error.kind=not_found` + exit 0（契约要求，core 据此判断可选能力未实现）。
5. `request.prepare` 失败错误码从硬编码 `prompt_too_long` 改为 `bad_params`。
6. `capabilities.control` 改为如实声明（移除未实现的 `config.schema` / `config.get`）。
7. 删除冗余配置 `fail_open`（契约里由 core 的 `hooks[].fail_open` 决定，插件侧无权自决）。
8. 死代码清零：`cargo build` 0 warning；脚本扫描「仅测试内使用 / 仅定义」的顶层函数为 0。
9. **RAG 生成能力**（P1 G-5）：新增 `ec-hook rag`，`top-k` 可配（`--top-k` / `EC_RAG_CONFIG.top_k`，默认 5），无命中不调 LLM。
10. **检索术语归一**（P1 G-7）：删除 function calling 工具注入，模型侧改为 bash 调 CLI；提示文本注入二进制真实路径；全部文档统一用 `recall` / `rag`，工具形态术语废弃（代码仅留向后兼容识别）。
11. 附带：删除失效配置键 `growth_nudge` / `pressure_threshold`（e2e 配置同步）；默认日志过滤调为 `info,tantivy=warn`，避免索引内部 INFO 污染 stderr。
