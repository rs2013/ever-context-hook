# EC 工具 CLI 使用说明（ec-hook）

> 适用版本：`Cargo.toml` 0.1.0 / `manifest` 声明 0.2.0（产物：`target/release/ec-hook.exe`）
> 本文以源码为准（`src/main.rs`、`src/rpc.rs`、`src/hook.rs`、`src/ec/recall.rs`、`src/ec/catchup.rs`），非设计稿。

---

## 0. 结论先行：它是不是"符合 ww3 hook 契约的 EC 工具"

**是。** 它就是一个单文件二进制，同时承担两件事：

| 身份 | 入口 | 谁调用 |
| --- | --- | --- |
| **ww3 Hook 插件**（主身份） | `ec-hook call --data <插件数据目录>`（`call` 可省略，默认就是它） | ww3-core 每请求 spawn 一次，走 stdin/stdout 的 JSONL RPC |
| **辅助 CLI**（同一二进制） | `manifest` / `recall` / `catchup` / `ping` | 人工、LLM 通过 bash、或 hook 自己 detach 子进程 |

**`--help` / `-h` / `help` 打印完整命令行手册，`--version` / `-V` 打印版本**（均 exit 0）。未列出的子命令走：

```
未知命令: <x>
运行 `ec-hook --help` 查看完整用法。
exit 2
```

### 契约符合度（对标 `ref/wild-work3/protocol/meta/meta.go` + `protocol/plugin/plugin.go` + `core/internal/runtime/hook.go`）

| 契约项 | 状态 |
| --- | --- |
| `manifest` 字段齐全（`id/name/version/kind/protocol/execution/capabilities/icon/color/description`） | ✅ |
| `execution.control/conversation="call"`（core 以 `["call","--data",dir]` 拉起） | ✅ 与 `route.go`/`hook.go` 的 `plugin.Spec` 一致 |
| RPC 方法 `request.prepare`（`PrepareResult.Request` 整包替换） | ✅ 返回 `{"request": <完整请求>}` |
| RPC 方法 `hook.stream`（双向流，`upstream.end` 收尾） | ✅ |
| 生命周期 `initialize` / `capabilities.list` / `ping` / `health.check` / `shutdown` | ✅ 已实现，与 `capabilities` 声明一致 |
| 未知方法返回 `error.kind=not_found`（core 靠它判定可选能力未实现） | ✅ 已实现，exit 0 |
| `config.schema` / `config.get` | ⚠️ 未实现（与 loopguard 现状相同）；已从 `capabilities.control` 移除，调用返回 `not_found` |
| `--help` / `-h` / `help` / `--version` / `-V` | ✅ 已实现 |

---

## 1. 全局约定

### 1.1 全局参数

| 参数 | 作用域 | 说明 |
| --- | --- | --- |
| `--data <dir>` | **所有子命令**（在 dispatch 之前解析） | 设置插件数据目录。**仅当环境变量 `EC_DATA_ROOT` 未设置时**才写入 `EC_DATA_ROOT`（不覆盖显式环境变量） |

### 1.2 环境变量

| 变量 | 用途 | 默认 |
| --- | --- | --- |
| `EC_DATA_ROOT` | 数据根目录（JSONL + 索引） | 回退 `WW3_DATA`，再回退 `.` |
| `WW3_DATA` | 同上，ww3 风格命名 | `.` |
| `EC_CATCHUP_CONFIG` | 只有 `catchup` 用：`{"model":"...","thinking":false}` 的 JSON，作为 `--model/--thinking` 的兜底 | 无 |
| `EC_RAG_CONFIG` | 只有 `rag` 用：`{"model":"...","thinking":false,"top_k":5}`，作为 `--model/--thinking/--top-k` 的兜底 | 无 |
| `WW3_CORE_EXE` | hook 自动 spawn 追赶时用：ww3-core 可执行文件 | `ww3-core` |
| `WW3_CORE_DATA` | hook 自动 spawn 追赶时用：core 数据目录 | `<data_root>/../../` |
| `WW3_PLUGINS_DIR` | hook 自动 spawn 追赶时用：插件目录 | `<data_root>/../` |
| `RUST_LOG` | 日志级别（env_logger），日志一律走 **stderr** | `info` |

> 日志与诊断信息全部写 stderr，**stdout 只承载 RPC 帧或命令正文**，可安全管道。

### 1.3 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 运行期失败（RPC 处理失败、索引/IO 错误、manifest 写失败、catchup 失败） |
| 2 | 用法错误（未知命令、缺必填参数、`--thinking`/`--summary-mode` 取值非法） |

### 1.4 数据布局

```
<EC_DATA_ROOT>/
├── sessions/
│   ├── <session_id>.jsonl         # 权威数据：消息树（id + parent_id + ec 投影）
│   ├── <session_id>.leaf          # 当前叶子指针
│   ├── <session_id>.jsonl.lock    # JSONL 写锁（短持锁）
│   └── <session_id>.lock          # catchup 会话锁（长持锁，30min 过期）
├── index/                         # 内嵌 Tantivy（派生缓存，可从 JSONL 重建）
└── ...
```

`session_id` = 首条 user 锚定区（首个 assistant 之前的全部 user 内容）的 SHA256 前 16 hex，**由 EC 自己算，不依赖下游 Agent 的 conversation_id**。

---

## 2. `call`（默认）— ww3 Hook 入口

```
ec-hook [call|stdio] [--data <插件数据目录>]
```

`call` 与 `stdio` 完全等价。**不带任何子命令时默认就是 `call`**，所以 ww3 侧配置 `execution=call` + 参数 `["--data", "<dir>"]` 即可。

### 2.1 帧格式（每行一个 JSON）

```jsonc
// core → hook：握手，必须是第一帧
{"v":1,"id":"req-1","op":"invoke","method":"hook.stream","params":{...}}

// core → hook：逐事件推入（仅 hook.stream）
{"v":1,"id":"req-1","op":"push","event":"text.delta","data":{"content":"..."}}

// hook → core：处理后的事件（可改写/丢弃/合并）
{"v":1,"id":"req-1","op":"event","event":"text.delta","data":{"content":"..."}}

// hook → core：结束
{"v":1,"id":"req-1","op":"result","ok":true}
```

`invoke.method` 只认两个值，其他一律 `未知方法: <m>` + exit 1：

| method | 语义 | 返回 |
| --- | --- | --- |
| `request.prepare` | **一次性**：改写入站请求（落盘增量 + 注入管理区 + 硬预算降级；不注入 tools） | `result` 帧，`result = {"request": <改写后的完整请求>}`；失败 `error.kind=bad_params` |
| `hook.stream` | **双向流**：透传上游事件，收集 assistant 回复并在 `upstream.end` 后落盘 | `result` 帧，`ok=true/false` |
| `initialize` | 握手 | `{protocol:1, plugin_id, manifest, capabilities}` |
| `capabilities.list` | 能力清单 | `{control:[...], conversation:[...]}` |
| `ping` | 存活探针 | `"pong"` |
| `health.check` | 健康检查 | `{"state":"ok"}` |
| `shutdown` | 优雅退出 | `{"ok":true}` |
| 其他 | 未实现的可选能力 | `error.kind=not_found`，exit 0（契约：core 据此判定可选能力未实现，非崩溃） |

### 2.2 `request.prepare` 示例

```bash
echo '{"v":1,"id":"r1","op":"invoke","method":"request.prepare","params":{}}' >/dev/null  # 仅示意

echo '{"v":1,"id":"r1","op":"invoke","method":"request.prepare","params":{
  "hook_id":"ec-1",
  "chain":{"index":0,"total":1},
  "config":{"enabled":true},
  "request":{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"你好"}]}
}}' | ./ec-hook call --data ./e2e/data/core/plugins/ever-context
```

预期 stdout（单行）：

```json
{"v":1,"id":"r1","op":"result","ok":true,"result":{"request":{"model":"deepseek-v4-pro","messages":[...],"tools":[...]}}}
```

失败时（已修正）：错误码为 `bad_params`（prepare 唯一错误是入站请求缺字段），scope `request`，exit 1。硬预算超线在内部降级，不走错误通道。

### 2.3 `hook.stream` 示例

```bash
printf '%s\n%s\n%s\n' \
 '{"v":1,"id":"r2","op":"invoke","method":"hook.stream","params":{"request":{"model":"m","messages":[]},"config":{}}}' \
 '{"v":1,"id":"r2","op":"push","event":"text.delta","data":{"content":"回答"}}' \
 '{"v":1,"id":"r2","op":"push","event":"upstream.end","data":{"ok":true}}' \
 | ./ec-hook call
```

识别并特殊处理的事件：

| 事件 | hook 行为 |
| --- | --- |
| `text.delta` | 累积正文 → 透传 |
| `reasoning.delta` | 累积思考内容 → 透传 |
| `tool_call.delta` | 按 `index` 聚合 tool_calls（`id`/`type`/`name`/`arguments_fragment`）→ 透传 |
| `usage` | 记录 `prompt_tokens`/`completion_tokens` → 透传 |
| `finish_reason` | 记录 `reason` → 透传 |
| `upstream.end` / `upstream.done` | 结束信号，**不转发**，`ok` 决定是否落盘 |
| 其他任意事件 | 原样透传 |

落盘条件：`exit_code==0 && upstream_ok && 正文/工具调用/思考非空`。落盘内容 = assistant 记录（含 model/channel/thinking/usage/stop_reason/reasoning/duration_ms）+ 更新 `.leaf`。

---

## 3. `manifest`

```
ec-hook manifest [--out <path>]
```

- 无 `--out`：pretty JSON 打到 stdout。
- 有 `--out`：写文件；写失败 exit 1。

```bash
ec-hook manifest --out ./e2e/plugins/ever-context/0.1.0/manifest.json
```

manifest 关键字段：`id=ever-context`、`kind=hook`、`version=0.2.0`、`execution.control/conversation=call`、`max_workers=8`、`capabilities.conversation=[hook.stream, request.prepare]`。

---

## 4. `recall` — 不调 LLM 的直接检索（LLM 通过 bash 调用）

```
ec-hook recall --session <id> [--query <q>] [--limit <n>]
ec-hook recall --directory  --session <id>
ec-hook recall --highlights --session <id>
ec-hook recall --knowledge --query <q> [--limit <n>]
ec-hook recall --add-knowledge --title <t> --summary <s>
```

**模式优先级（从高到低，命中即返回）**：`--add-knowledge` → `--knowledge` → `--directory` → `--highlights` → 默认检索。

| 参数 | 适用模式 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `--session <id>` | 默认 / `--directory` / `--highlights` | 是 | — | 缺则 exit 2 并打印用法 |
| `--query <q>` | 默认 / `--knowledge` | 默认模式可空；`--knowledge` 必填 | `""` | 关键词（Tantivy 检索时与索引同一 jieba 分词） |
| `--limit <n>` | 默认 / `--knowledge` | 否 | `5` | 返回条数上限 |
| `--title` / `--summary` | `--add-knowledge` | 是 | — | 缺任一 exit 2 |

### 4.1 各模式行为

| 模式 | 输出（stdout） | 数据来源 |
| --- | --- | --- |
| 默认检索 | `[EC_RECALL_RESULT]` … `[/EC_RECALL_RESULT]`，每条含 `### 标题 (block_id)` + 摘要 + `--- 原文 (行 a-b)` + **JSONL 原文 verbatim** | Tantivy 召回 → 按 `path/start_line/end_line` 回读 JSONL；索引不可用时退化为内存 `search_chain` |
| `--directory` | `[EC_DIRECTORY]` + 每行 `<block_id> <title>` | 祖先链上全部压缩块（不受目录预算裁剪） |
| `--highlights` | `[EC_HIGHLIGHTS]` + 每行 `- <highlight>` | 祖先链上全部块 highlights 合并去重 |
| `--knowledge` | 同 `[EC_RECALL_RESULT]` | 全局 `doc_type=knowledge`（不受 session 限制） |
| `--add-knowledge` | `已写入知识：<title>` | 写索引 `doc_key=knowledge:<sha8(title)>` |

无命中时输出人类可读提示：`未找到相关历史内容。` / `本会话暂无压缩块。` / `本会话暂无精华内容。`

### 4.2 示例

```bash
# 本会话关键词召回（含原文回读）
ec-hook recall --session 3f2a9c1b7d4e5061 --query "接口超时" --limit 3

# 完整目录 / 完整精华
ec-hook recall --session 3f2a9c1b7d4e5061 --directory
ec-hook recall --session 3f2a9c1b7d4e5061 --highlights

# 跨会话知识
ec-hook recall --knowledge --query "部署口令"
ec-hook recall --add-knowledge --title "ww3 端口" --summary "core 默认监听 127.0.0.1:7864"
```

> `--data` 也可用（全局参数），等价于提前设置 `EC_DATA_ROOT`。

---

## 5. `rag` — 检索 + LLM 生成（只把 top-k 条交给模型）

```
ec-hook rag --session <id> --query <问题> --core-data <dir>
            [--top-k <n>] [--model <m>] [--thinking true|false]
            [--core <path>] [--plugins <dir>] [--data <dir>]
```

| 参数 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `--session <id>` | ✅ | — | 会话 ID |
| `--query <问题>` | ✅ | — | 问题原文（先做关键词检索，不做 LLM 意图拆解） |
| `--core-data <dir>` | ✅ | — | core 数据目录（生成阶段 `ww3-core stdio --hooks none` 用） |
| `--top-k <n>` | 否 | `5` | **只取相关度最高的 n 条片段**交给 LLM |
| `--model <m>` | 否 | 见下 | 生成模型 |
| `--thinking <bool>` | 否 | 见下 | 生成阶段思考开关 |
| `--core <path>` | 否 | `ww3-core` | core 可执行文件 |
| `--plugins <dir>` | 否 | `""` | 透传给 core |
| `--data <dir>` | 否 | `EC_DATA_ROOT` | EC 数据目录 |

兜底顺序：

```
--top-k   → EC_RAG_CONFIG.top_k → 5
--model   → EC_RAG_CONFIG.model → 会话末条 assistant 的 model
--thinking→ 显式值 → EC_RAG_CONFIG.thinking → false
```

行为：

1. 用与 `recall` **同一套检索**（Tantivy 召回 + 分支过滤 + 按 `path/start_line/end_line` 回读原文）取 top-k；
2. **无命中时不调 LLM**，直接输出「未在当前会话历史中找到与问题相关的内容。」；
3. 有命中时把编号片段（`[1] 标题 (block_id, 行 a-b)` + 摘要 + 原文，单条原文上限 4000 字符）交给 LLM，
   system 规则要求：只依据片段回答、用 `[1][2]` 标注依据、片段不足时明说；
4. 模型缺失且确有命中 → 报错 `缺少生成模型`，exit 1。

示例：

```bash
ec-hook rag --session 3f2a9c1b7d4e5061 --query "上次为什么改用 jieba？"   --top-k 3 --core ./ww3-core.exe --core-data ./data
```

> 注：索引不可用时 `recall` 会退化到内存检索，此时命中只有摘要、**没有原文行号**（`行 0-0`），
> RAG 的片段质量会相应下降；建索引由 `request.prepare` 自动完成。

## 6. `catchup` — 旧对话离线追赶（后台逐段调 LLM 补摘要）

```
ec-hook catchup --session <id> --core-data <core数据目录>
                          [--data <插件数据目录>] [--core <core可执行文件>] [--plugins <插件目录>]
                          [--model <模型>] [--thinking true|false]
                          [--target-tokens <n>] [--gap-ms <n>]
                          [--summary-mode fragment|full_context] [--full-context-max-tokens <n>]
                          [--trace <trace.jsonl>]
```

| 参数 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `--session <id>` | ✅ | — | 目标会话 |
| `--core-data <dir>` | ✅ | — | core 数据目录（缺则 exit 2） |
| `--data <dir>` | 否 | `EC_DATA_ROOT` | EC 数据目录 |
| `--core <path>` | 否 | `ww3-core` | core 可执行文件，内部以 `stdio --data <core-data> --plugins <plugins> --hooks none` 启动 |
| `--plugins <dir>` | 否 | `""` | 透传给 core |
| `--model <m>` | 否* | 见下 | 摘要模型 |
| `--thinking <bool>` | 否 | 见下 | 摘要请求是否开思考 |
| `--target-tokens <n>` | 否 | `10000` | 单块目标 token |
| `--gap-ms <n>` | 否 | `200` | 每块之间的节流间隔 |
| `--summary-mode <m>` | 否 | `full_context` | `fragment` \| `full_context`（`full_context` 提供完整前缀 + 唯一锚点，吃上游前缀缓存） |
| `--full-context-max-tokens <n>` | 否 | `60000` | full_context 前缀上限，超了自动退 fragment |
| `--trace <path>` | 否 | 无 | 每块决策/耗时写 JSONL 追踪 |

`--model` / `--thinking` 的兜底顺序：

```
--model        → EC_CATCHUP_CONFIG.model  → 会话最后一条 assistant 的 model    → 报错"缺少摘要模型"
--thinking     → --thinking 显式值        → EC_CATCHUP_CONFIG.thinking        → 最后一条 assistant 的 thinking → false
```

示例：

```bash
ec-hook catchup \
  --session 3f2a9c1b7d4e5061 \
  --data ./e2e/data/core/plugins/ever-context \
  --core ./e2e/ww3-core.exe \
  --core-data ./e2e/data/core \
  --plugins ./e2e/data/core/plugins \
  --model deepseek-v4-pro --thinking false \
  --summary-mode full_context --trace ./temp/catchup-trace.jsonl
```

> **谁会自动调它**：`request.prepare` 发现「未摘要 token ≥ target_tokens 且投影 > 软阈值」时，会用 `compact` 配置 detach 一个本命令的子进程（`--data/--core/--core-data/--plugins` 由 `WW3_*` 环境变量或 data_root 反推）。手工追赶才需要显式传参。

---

## 7. `ping`

```
ec-hook ping
```

向 **stderr** 打 `[ec-hook] ping: ok`，exit 0。仅自检用；RPC 层的 `ping` 方法未实现。

---

## 8. 插件配置（`hooks[].config`，由 ww3 下发）

`EcConfig` 实际接受的键（`serde` 默认忽略未知键，写错不报错）：

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | `false` 时 `hook.stream` 纯透传 |
| `context_window` | u32 | `0` | `>0` 时覆盖按模型名推断的窗口 |
| `hard_budget` | u32 | `0` | `>0` 时覆盖硬预算线 |
| `soft_threshold` | u32 | `0` | `>0` 时覆盖软阈值 |
| `degrade_target_ratio` | f64 | `0.6` | 硬线降级目标 = 软阈值 × 该比例（非 0.6 时重算整份预算） |
| `target_tokens` | u32 | `10000` | 单块目标 token |
| `compact.model` / `compact.thinking` | string/bool | 空 | **正常对话增量压缩**；模型缺省取本轮 `request.model`，thinking 缺省跟随请求 |
| `catchup.model` / `catchup.thinking` | string/bool | 空 | 旧对话追赶（**尚未接线**，见 §8） |

> ⚠️ 旧配置里的 `growth_nudge`、`pressure_threshold` 已在死代码清理中移除（对应 `compress::decide` 链路），保留在 JSON 里不会报错但**不再生效**。`e2e/data/config.json` 里仍有这两个键，可一并删除。
> ⚠️ `fail_open` 不属于插件配置：ww3 的 `hooks[].fail_open` 由 **core** 读取并决定启动/收尾失败时是否跳过本层，插件侧无权自决，故已从 `EcConfig` 删除。

---

## 9. 已知限制清单（按影响排序）

1. `catchup.*` 插件配置尚未接线：自动 spawn 一律用 `compact` 模型，手工追赶用 CLI `--model`；`rag.*` 同理，目前只认 CLI 参数与 `EC_RAG_CONFIG`。
2. `rag` 仅检索**本会话**（祖先链）；跨会话知识库的生成式回答尚未接入（`--knowledge` 只有纯检索）。
3. `catchup` 四态推进目前用 `WindowConfig::default()`，不随实际模型窗口变化。
4. 并发写保护：`append`/`import_messages`/`update_leaf` 仍未纳入同一把会话写锁（评审 P0 项，未修）。
5. `core_client` 无请求超时；锁 30 分钟过期且无心跳。
6. `config.schema` / `config.get` 未实现（返回 `not_found`，与 loopguard 现状一致）。

---

## 10. 与你预期核对用的自检清单

- [ ] 二进制名 `ec-hook`（`target/release/ec-hook.exe`），ww3 侧只需 `["call","--data","<插件数据目录>"]`
- [ ] `--help` / `-h` / `help` 打印手册，`--version` / `-V` 打印版本
- [ ] 两个 RPC 方法：`request.prepare`（结果在 `result.request`）、`hook.stream`（透传 + 落盘）
- [ ] 五个生命周期方法：`initialize` / `capabilities.list` / `ping` / `health.check` / `shutdown`
- [ ] 未知方法返回 `not_found` 而非崩溃
- [ ] 五个子命令：`call`(默认) / `manifest` / `recall` / `catchup` / `ping`
- [ ] `recall` 五模式与优先级：`--add-knowledge` > `--knowledge` > `--directory` > `--highlights` > 默认
- [ ] `rag` 只把 top-k 条交给 LLM；无命中不调 LLM；顶层 `--top-k` / `EC_RAG_CONFIG.top_k` 可覆盖
- [ ] `recall` 默认与 `--knowledge` 需要 `--query`；`--directory`/`--highlights`/默认 需要 `--session`
- [ ] `catchup` 必填 `--session` + `--core-data`，模型兜底链三段
- [ ] 退出码 0/1/2 语义
- [ ] stdout 只出 RPC 帧或正文，日志全在 stderr
- [ ] 数据落在 `<EC_DATA_ROOT>/sessions/<session_id>.jsonl` + `<EC_DATA_ROOT>/index/`

### 10.1 模型侧使用提示（skill 文本模板）

给模型看的说明里**直接写 `ec-hook` 的真实绝对路径**（hook 注入的管理区/降级提示就是这么做的，
见 `ec::bin_hint()`），不要写工具名：

```markdown
## 历史检索

需要查找已经滚出上下文的早先内容时，用 bash 执行（路径以本机安装为准）：

- 关键词检索：`<EC_HOOK_PATH> recall --session <session_id> --query "<关键词>"`
- 完整目录：  `<EC_HOOK_PATH> recall --session <session_id> --directory`
- 完整精华：  `<EC_HOOK_PATH> recall --session <session_id> --highlights`
- 直接问答：  `<EC_HOOK_PATH> rag --session <session_id> --query "<问题>" --core-data <core数据目录>`

`recall` 只检索、结果包在 `[EC_RECALL_RESULT]` … `[/EC_RECALL_RESULT]` 里（这类内容不计入压缩预算）；
`rag` 会先检索 top-k 再让模型给出带 `[1][2]` 引用的回答。
```

> 术语约定：本仓库只用 **`recall`** 指代「检索」，用 **`rag`** 指代「检索 + 生成」，
> 不引入任何"工具名"概念。早期工具形态的命名已废弃；代码里只保留对历史 JSONL 的向后兼容识别
> （按正文 `[EC_RECALL_RESULT]` 标记判断召回内容）。
