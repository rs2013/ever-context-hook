# EC 代理版（ever-context-hook）实施规划

> 状态：**全部实施完成**（阶段 1-5），31 测试通过 + prepare/tool_calls/recall 冒烟验证通过。
> 适用范围：`ever-context-hook`（EC 的 ww3 拦截器形态，纯 Rust 单二进制，内嵌 Tantivy 无 sidecar）。
> 对照基线：用户 10 条需求（m00264）+ 首条 user 锚定（m00350）+ 上轮 7 个 bug（m00263）。
> 注意：本次实施**只改代理版**，不同步 pi 插件版（pi-ever-context）。

## 实施完成记录

| 阶段 | 内容 | 结果 |
|---|---|---|
| 1 | 会话身份锚定（leading_user_end/leading_user_anchor）+ 首条 user 保护 | ✅ 测试通过 |
| 2 | target 10K 可配置 + 统一分段 DRY（segment_records/estimate_record_tokens）+ 增量落盘 + 日常压缩触发 | ✅ 测试通过 |
| 3 | recall 起止标记 `[EC_RECALL_RESULT]` + directory/highlights/knowledge 命令 + 目录/精华区汰出提示 | ✅ 冒烟验证通过 |
| 4 | 冗余评估落地：schema v2 title/summary 只索引不 STORED + doc_text（knowledge STORED）+ 召回回读 JSONL | ✅ 测试通过 |
| 5 | 7 bug 全修：user 落盘 / locate 接线 / 四态推进（advance_states）/ tool_calls 收集 / ec_recall 注入 tools / block_id 稳定化 / 管理区 role=user | ✅ 测试通过 |

## 0. 现状对齐结论（已逐行核对）

| 需求 | 状态 | 关键证据 |
|---|---|---|
| ① target 20K→10K 可配置 | ❌ | `types.rs:225` / `catchup.rs:54` / `main.rs:89` 硬编码 20_000；`EcConfig`（config.rs:41）七字段无 target_tokens |
| ② 增量算法统一 DRY + 扣召回 | ❌ | 三套估算并存：`budget.rs` estimate_message_tokens / `index.rs:288` estimate_record_tokens / `catchup.rs:80` next_range；`hook.rs:182` `decide` 结果 `let _=` 丢弃，日常增长无增量压缩入口 |
| ③ 召回内容起止标记 | ❌ | `recall.rs:127` `format_hits` 输出 `### 标题 (id)\n摘要`，无边界标记 |
| ④ recall directory/highlights 命令 + 汰出提示 | ❌ | `main.rs` recall 仅 `--session/--query/--limit` |
| ⑤ recall 读磁盘 leaf_id 避跨树枝 | ✅ | `recall.rs:33` `read_leaf` → 祖先链 message_id 白名单 |
| ⑥ knowledge 跨会话/全局检索 | ❌ | `index.rs` doc_type 只有 `ec_block`，`knowledge` 仅注释，无写入/检索入口 |
| ⑦ JSONL+Tantivy 双份摘要冗余 | ⚠️ | 需评估；现状 title/summary 双份（JSONL 权威 + Tantivy STORED） |
| ⑧ 无 JSONL 时保护首条 user + 裁剪提示词 | ❌ | `tail_fit_start` 从尾保留可裁掉首条 user；`identity.rs:28` session_id 只取第一条 user |
| ⑨ tantivy 无常驻内存、磁盘检索 | ✅ | 一请求一进程 open→读写→commit→退出（底层 mmap，OS 页缓存） |
| ⑩ 上轮 7 bug | ❌ | 见 §0.1 |

### 0.1 上轮 7 个 bug（m00263，逐行核对过）

1. **新 user 消息永不落盘**：`store.append` 仅两处 = `import_messages`（仅 is_new）+ `hook.rs:176` append assistant 回复；后续轮 user/tool 消息不入库，树退化 assistant→assistant 直链。
2. **`Store::locate` 未接线**：`store.rs:294` 已实现但 `hook.rs`/`catchup.rs`/`recall.rs` 全只用 `read_leaf`，未用 tail-features 反向链校验定位。
3. **四态只渲染不推进**：`catchup` 只写 `BlockState::Summary`，IndexOnly/Omitted 无任何代码写入；`projection.rs` 目录区/精华区永远空。
4. **tool_calls 全程丢失**：`hook.rs:79` `response_tool_calls` 声明后从未 push；`tool_call.delta` 分支只透传不收集。
5. **ec_recall 未作为 tool 注入 request.tools**：全仓无 tools 写入。
6. **`block_id_of` 内容 hash 跨轮不稳定**：`projection.rs` 用 sha256(title:summary) 截 8 hex，标题/摘要变则 id 变，`rev=blocks.len` 冒充版本。
7. **管理区 role=system**：`hook.rs` 注入 `role:system`，Pi 版默认 user（注释警告 system 改变 provider 语义）。

## 1. 实施阶段

### 阶段 1：会话身份锚定 + 首条 user 保护（m00350 + 需求⑧）

**目标**：`session_id` 与 JSONL 定位锚点改用「首个 assistant 之前的所有 user」；首条 user 区在压缩/降级中永不汰出。

**改动**：
1. `identity.rs` 新增 `leading_user_anchor(messages) -> String`：跳过前导 system/developer 后，拼接所有 role==user 的消息直到首个非 user（首个 assistant/tool），返回拼接全文（用 `\n` 分隔）。`session_id()` 改用它（原只用第一条 user 全文）。
2. `budget.rs` `tail_fit_start` 增加首条 user 保护：尾部贴合起点不得越过「首个 assistant 之前」的位置——即 `leading_user_end = 首个非 user 下标`，`tail_start = max(tail_fit_start(...), leading_user_end)`。保证降级后首条 user 区仍在。
3. 无 JSONL 时裁剪提示词（需求⑧②）：`hook.rs handle_prepare` 超硬线降级分支，若裁掉了前缀（tail_start > leading_user_end），在管理区后追加 `EC_DEGRADED_NOTICE`（含 ec_recall 使用指引 few-shot），话术如下：
   ```
   [EC 提示] 对话前段（约 N 条消息）已从上下文裁剪，当前尚无可用摘要。
   如需查找早先的内容，可调用 ec_recall 关键词检索，例如：ec_recall query <关键词> --session <session_id>
   ```

**验收**：
- `cargo test` 新增：`session_id_uses_all_users_before_first_assistant`（两 user + assistant 时 session_id 含两条 user 拼接）；`tail_fit_start_preserves_first_user`。
- e2e：构造 dsh 形态消息（[system, user "hi", user "agents.md", user "技能列表", assistant, ...]），降级后首条 user 区仍在投影里。

### 阶段 2：target 可配置 + 增量算法统一 DRY（需求①②）

**目标**：target_tokens 默认 10K 且进 `EcConfig`；统一 token 估算与分段；日常增长触发增量压缩（不再只靠 catchup）。

**改动**：
1. `types.rs:225` `target_tokens: 20_000` → `10_000`（`DerivedBudget`）；`config.rs EcConfig` 加 `target_tokens: u32`（默认 10_000），`from_params` 解析；`catchup.rs:54` / `main.rs:89` 默认改 10_000 并优先读 `EcConfig.target_tokens`。
2. 抽取统一分段器：新增 `ec::chunk` 模块（或并入 `budget.rs`），提供 `segment_chain(chain, cursor, target_tokens) -> Vec<(usize,usize)>`，内部用唯一一套 `estimate_record_tokens`（跳过 recall 结果）。`index.rs index_session` 与 `catchup.rs next_range` 都改用它，删掉各自私有估算函数。
3. 日常增量压缩入口：`hook.rs` 第 4 步 `compress::decide` 结果不再丢弃——SoftTrigger 时 spawn catchup 子进程（复用现有 `spawn_catchup`，同锁保证单进程）；`decide` 的 `last_compaction_tokens` 改为「祖先链最后一个带 ec 记录之后累计的 token」（用 chain 上 ec 游标精确计算，不用 `块数*20_000` 粗略值）。
4. 增量估算扣召回：确认 `estimate_record_tokens` 跳过 `name=="ec_recall_result"`（已有），并在 `estimate_prompt_tokens` 层同样跳过（核对 `budget.rs`）。

**验收**：
- `cargo test`：`segment_chain` 按 target 切分、跳过 recall；`EcConfig` 解析 target_tokens。
- e2e：`catchup --target-tokens 10000` 分段粒度变小；正常多轮对话在增长超 nudge 后自动 spawn catchup。

### 阶段 3：recall 起止标记 + directory/highlights 命令 + knowledge（需求③④⑥）

**目标**：召回结果带边界标记；recall 增加 directory/highlights 子命令；knowledge 跨会话检索落地。

**改动**：
1. `recall.rs format_hits`：每条命中前后加标记行 `<<<EC_RECALL_HIT i>>>` / `<<<EC_RECALL_END>>>`，命中汇总头加 `<<<EC_RECALL_BEGIN total=N>>>`；压缩提示词 `COMPRESS_SYSTEM_PROMPT` 已要求忽略 recall 结果，保持。
2. `main.rs recall` 增加 `--command <query|directory|highlights>`（缺省 query）。`recall.rs` 新增：
   - `directory(session_id, data_root) -> String`：渲染祖先链全部块（含 index_only/omitted）的 `block_id title` 列表。
   - `highlights(session_id, data_root) -> String`：渲染全部块 highlights 去重合并。
3. `projection.rs` 目录区/精华区汰出提示：精华区按 budget 裁剪后有剩余时，末尾加 `(还有 N 条精华已汰出，可用 ec_recall highlights --session xxx 获取)`；目录区 omitted 提示已有，补充 directory 命令指引。
4. knowledge 落地：`index.rs` 新增 `index_knowledge` + `search_knowledge`（跨 session，doc_type=`knowledge`）；`main.rs` 加 `knowledge-add`/`knowledge-search` 子命令（或并入 recall `--command knowledge --query`）。索引 doc_type 过滤参数化：`search` 的 `doc_type` 参数已支持，knowledge 检索传 `"knowledge"` 且 session_id 传空（不过滤会话）。

**验收**：
- `cargo test`：format_hits 含边界标记；directory/highlights 渲染正确；knowledge 跨会话检索命中且不受 session 过滤。

### 阶段 4：数据冗余评估落地（需求⑦）

**目标**：JSONL 权威，Tantivy 侧 title/summary 从「STORED 全文」改为「只索引不 STORED」，召回命中按 doc_key→message_id 回读 JSONL 取摘要/标题，消除双份。

**改动**（评估结论落地）：
- `index.rs build_schema`：title/summary 的 `searchable` 去掉 `.set_stored()`（只索引）；`SearchHit` 删除 title/summary 字段（改为回读）。
- `recall.rs search`：命中后按 `message_id` 从 `load_chain` 找到对应 record 的 `ec` 字段回读 title/summary/highlights。
- 若回读成本/复杂度不划算（命中需逐条查链），**降级方案**：保持 STORED，仅在文档记录「JSONL 为权威、Tantivy 为派生缓存、schema 变更自动重建」的约束，不追求消除冗余。

**验收**：`cargo test` 召回结果 title/summary 正确；JSONL 改动后召回结果一致。

### 阶段 5：7 个 bug 修复（需求⑩）

**目标**：逐个修掉 §0.1 的 7 个 bug。

**改动**：
1. **user 落盘**：`hook.rs handle_prepare`（或第 4 步）在每次请求后把请求中的**新增原生消息**（user/tool_result/assistant）append 进 JSONL——用 `locate` 定位当前叶子，把「JSONL 里没有的尾部消息」补入（幂等 append）。assistant 回复照旧在 stream 结束时 append。
2. **`locate` 接线**：`handle_prepare` 用 `store.locate(session_id, tail_features)` 替代「仅 read_leaf」定位（leaf 为空或 locate 更精确时）；`recall.rs`/`catchup.rs` 保持 read_leaf（有 leaf 文件时足够）。
3. **四态推进**：新增 `ec::roll` 逻辑（或在 `catchup` 补块后调用）：摘要区超 summary_budget 时把最旧 summary 块降 index_only，目录区超 directory_budget 时把最旧 index_only 降 omitted。写回 `set_ec`。
4. **tool_calls 收集**：`hook.rs` `tool_call.delta` 分支按 id 聚合 name/arguments 进 `response_tool_calls`；assistant 记录带 tool_calls 落盘。
5. **ec_recall 注入 tools**：`handle_prepare` 改写 request 时把 `tools` 数组追加 `ec_recall` 函数定义（若 request 无 tools 则新建）。
6. **block_id 稳定化**：`projection.rs` 改用「记录 id（message_id）」作 block_id（锚定 ec 挂载点，跨轮稳定），`rev` 用「带 ec 记录的 message_id 集合」派生稳定值或直接用 `blocks.len` 但注释说明语义。
7. **管理区 role 改 user**：`hook.rs` 管理区注入消息 role 从 `system` 改 `user`（对齐 Pi 版，避免 provider 语义问题）；硬线提示已是 user。

**验收**：`cargo test` 逐 bug 补单测；e2e 多轮对话 JSONL 里 user/tool_result/assistant 都在、tool_calls 非空、管理区 role=user、block_id 跨轮稳定。

## 2. 顺序与依赖

```
阶段1（身份锚定）──┐
阶段2（增量+target）─┤ 可并行，均为基础层
阶段3（召回命令）  ──┤ 依赖 2 的估算统一
阶段4（冗余）     ──┘ 依赖 3 的 recall 回读
阶段5（7 bug）    ── 依赖 1/2 的落盘与定位（bug1/2 尤其）
```

建议按 1→2→3→4→5 顺序实施，每阶段跑 `cargo test` + 相关 e2e 后再进入下一阶段。

## 3. 失败回退

- 每阶段改动集中在独立模块/函数，`git` 无版本控制（本仓库未 init）——**每阶段开始前 `cp -r` 备份 `src/` 到 `docs/.backup/stage-N/`**，失败可整体回退。
- 行为开关：`EcConfig.enabled` 已是 fail-open 总开关；新增能力（ec_recall 注入、增量压缩 spawn）失败时记日志不阻断主流程（保持 fail-open 原则）。

## 4. 不在本次范围

- pi 插件版（pi-ever-context）的任何改动。
- 并发 recall 叶子错乱（已知限制，设计文档 §10 已裁定暂接受）。
- LLM 显式传 `--leaf`（设计文档 §5 已裁定留后续）。
- `checkpointThreshold` 迁移（代理版无 Pi checkpoint 机制，用 growthNudge，已裁定）。

---

## 阶段 6：压缩算法统一（DRY，对齐 Pi `summarizeOne`）

**背景**：原实现把「追赶索引」与「正常对话增量压缩」写成两套逻辑——`catchup.rs` 自带
`range_text/parse_summary/compress_messages`，只喂**互不重叠**的片段，导致各块 prompt 无公共
前缀、上游缓存 0 命中。用户裁定：追赶与正常对话增量压缩**必须共用同一份压缩算法**（DRY），差异
仅有两点——追赶用主会话同模型但显式关思考、连续串行执行（不靠 tool_result/user prompt 触发）。
注意：**维护（四态升降）是纯规则、不调 LLM，不属于本算法**。

**改动**（压缩本体集中到 `src/ec/compress.rs`，`catchup.rs` 退化为纯驱动）：

| 符号 | 说明 |
| --- | --- |
| `SummaryMode{Fragment,FullContext}` | 输入模式；默认 `FullContext`（对齐 Pi `EcExtensionOptions.summaryMode`，extension.ts:139） |
| `build_summary_call(chain,start,end,mode,full_max)` | 唯一请求构造入口。FullContext＝喂 `[0..end]` 完整前缀 + 本段首条原文的唯一锚点（`unique_boundary_quote`），模型只压锚点之后；锚点不唯一或前缀超 `full_max` 自动降级 Fragment |
| `run_summary_call(client,model,call)` | 唯一执行入口，`chat(..., false)` 显式关思考；返回上游 `ChatResult` + 解析产物 |
| `unique_quote_in / unique_boundary_quote` | 对齐 Pi `ec-boundary.ts:145/163`（最短唯一前缀，min 48 / max 512 字节，UTF-8 边界对齐） |
| `SUMMARY_SYSTEM_PROMPT[_FULLCONTEXT]` | 片段 / 全上下文两版提示词（后者要求「只压缩锚点之后」并忽略 `ec_recall_result`） |

`catchup::CatchupConfig` 新增 `summary_mode`（默认 full_context）与 `full_context_max_tokens`
（默认 60000）；CLI 新增 `--summary-mode fragment|full_context`、`--full-context-max-tokens`。
`hook.rs::spawn_catchup` 已传 `--model cfg.model`（主会话同模型）、算法内 `thinking=false`，故
「正常对话增量压缩」与「追赶」自动共用同一份实现。

**验收（实测，deepseek-flash，40 条旧会话 3 块折叠）**：`effective_mode=full_context`；前缀嵌套
⇒ 缓存命中率 block0 `0/8778=0%`（首块冷）、block1 `8704/18413=47.3%`、block2
`18304/21421=85.4%`（旧 fragment 实现三块全 0%）。`cargo test --release` 35 passed（+3 单测：
全前缀含锚点、超限降级、最短唯一前缀）。trace 增记 `effective_mode`。
