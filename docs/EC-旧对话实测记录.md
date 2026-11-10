# EC 代理版（ever-context-hook）旧对话实测记录

> 目的：拿一个真实的历史 pi 会话做蓝本，完整跑一遍 EC「建档 → 索引追赶（deepseek-flash）→ 投影 → 继续对话 → 召回」链路，
> 量化旧对话在 EC 中的实际表现与**上游前缀缓存利用率**。日期：2026-11-09。
> 一键复现：`bash temp/ec-live/run_all.sh [pi.jsonl] [data_dir] [trace_dir]`

## 0. 本次为复盘新增的 trace 桩位

| 桩位 | 位置 | 内容 |
|---|---|---|
| `[ec-prepare-trace]` INFO 日志 | `src/hook.rs`（handle_prepare 末尾） | `session=/leaf=/chain=/unsummarized_tokens=/projected=/soft=/hard=/management_chars=/over_hard=/tail_start=/tools=` |
| `--trace <file>` | `src/main.rs` catchup 子命令 + `src/ec/catchup.rs` | 每块一行 JSONL：range 文本/字符数/估算 token、请求 messages、上游 `usage`、parsed 摘要、耗时 |
| 上游 usage 采集 | `src/ec/core_client.rs`（新增 `UsageInfo`） | 解析 core 的 `usage` 事件：prompt/completion/total/cache.cached/cache_write/cache_reported/time_ms/first_token_ms |
| `prepare/投影` 全量落盘 | `tools/dump-prepare.py` | `.invoke.json/.response.json/.projection.txt/.messages.json/.stderr.txt` |

## 1. 蓝本会话与第一轮到完成的结果

- 蓝本：`C:/Users/rocks/.pi/agent/sessions/--D--work-pi-web--/2026-11-06T13-38-35-185Z_01a1b115-2431-7756-a413-30ac62552de4.jsonl`（48 行；主题为「pi 会话分叉的存储机制」，含大量 rg/ls 工具输出）
- 转换后：**40 条消息**，总字符 67 945，估算 ≈ 22 648 token（字节/3）
- `session_id = afec19748682ae8a`（首条 user 锚定区 SHA16，稳定）
- 产物：`temp/ec-live/traces/`

### 1.1 建档（bootstrap，is_new 分支）
- `store.import_messages` 幂等导入 40 条 → JSONL；`index_session` 按 target=10 000 规则分 **3 块**（title/summary 空，不调 LLM）
- 投影：40 → 40 条，`management_chars=0`（尚无摘要，管理区为空）
- trace：`unsummarized_tokens=22224 projected=22224 soft=100000 hard=103424 over_hard=false tools=1`

### 1.2 索引追赶（catchup，deepseek-flash，target=10 000，冷缓存）

| 块 | 段 | 终记录 | range 字符/~token | prompt | completion | cached | 耗时 |
|---|---|---|---|---|---|---|---|
| 0 | [0..22] | `1e8929db2735ac82` | 27 989 / 9 330 | 8 706 | 735 | **0** | 6 325 ms |
| 1 | [23..28] | `b2b673e765123abd` | 29 948 / 9 983 | 9 846 | 547 | **0** | 5 007 ms |
| 2 | [29..39] | `23d10ec0ff57e30b` | 8 426 / 2 809 | 3 224 | 635 | **0** | 5 068 ms |
| 合计 | | | | **21 776** | **1 917** | **0** | 16 400 ms |

写入 JSONL 的 `ec={state:"summary", title, summary, highlights}` 分别落在记录 22 / 28 / 39。

### 1.3 追赶后投影
- `p2-post-catchup.projection.txt`：40 → **41 条**（管理区 1 条 + 原始 40 条全部保留）
- 管理区 `[EC_CONTEXT rev=3]` = `[HIGHLIGHTS]`(9 条) + `[SUMMARIES]`(3 个 `[[EC_BLOCK id=...]]`)，共 **4 247 字符 ≈ 1 416 token**
- trace：`unsummarized_tokens=20991 projected=22224 soft=100000 hard=103424 management_chars=4247 over_hard=false tail_start=- tools=1`

### 1.4 继续对话（追加 1 条新 user 后重新 prepare）
- 原始 41 → 投影 42 条；`tools=['ec_recall']`（工具注入生效）
- JSONL：记录数 40→41，末条 role=user，**首条 id `cd16ac85a73feafe` 保持不变**（content-addressed 幂等正确）

### 1.5 召回校验（全部可用）
- `recall --directory` → `[EC_DIRECTORY]` 3 个块标题
- `recall --highlights` → 9 条精华
- `recall --query "分叉 branch_summary"` → 3 条命中（摘要全文）
- `recall --add-knowledge` / `--knowledge` → 写入并检索到 `knowledge:bf3706227803abb6`

## 2. 上游前缀缓存测量（关键结论）

### 2.1 catchup 路径
- **冷缓存：0 命中**（见上表三块 cached 全 0）。
- 复现实验（第二会话 `--D--work-bjev-kms_api--/…_01a0d184-….jsonl`，37 条/≈13 030 token，2 块）：prompt 11 217 + 2 720，completion 1 339 + 942，**cached 全 0，命中率 0.0%**。
- 同一会话**重跑**（prompt 完全相同）：cached 8 572 / 9 600 / 3 072，**命中率 97.6%** —— 说明命中只来自「块级 prompt 完全重复」，而非块间共享。

原因：catchup 每块的 prompt = `[system 381 字][user: 固定引导语 + 本块 range 文本]`，块与块之间**只有 381 字的 system 共享**（≈250 token），低于 DeepSeek 前缀缓存有效阈值。

### 2.2 阈值扫描（`temp/ec-live/probe_cache3.py`，共享 system 前缀、user 正文各不相同）

| 共享 system | prompt | cached（第二轮） |
|---|---|---|
| 600 字 | 2 164 | 128 |
| 1 200 字 | 2 494 | 512 |
| 2 400 字 | 3 154 | 1 152 |
| 4 800 字 | 4 474 | 2 432 |

⇒ 共享前缀 ≈600 字才开始命中（128 token，即 64 的整数倍），EC 的 381 字 system 命中 0。

### 2.3 会话路径对照（`temp/ec-live/sim2_continue_cache.py`）
把 p2 投影（管理区 + 40 原生消息）原样发给核心，再发「上轮前缀 + 助手回复 + 新 user」：

| 轮次 | prompt | cached | 命中率 |
|---|---|---|---|
| turn-N（首轮） | 22 266 | 0 | 0.0% |
| turn-N+1（前缀扩展） | 22 306 | 22 144 | **99.3%** |
| turn-N+2（同前缀再来） | 22 306 | 22 144 | 99.3% |

⇒ 真实聊天链路的 prompt 是逐轮前缀扩展，缓存近乎全命中；**cache 收益几乎被聊天路径吃掉，索引追赶路径完全吃不到**。

## 3. 旧对话在 EC 中的表现小结

1. **机制全部可用**：建档导入、规则分段建索引、LLM 逐段摘要、管理区注入（目录/精华/摘要）、四态记录、`ec_recall` 工具注入与召回（query/directory/highlights/knowledge）、增量落盘与 leaf 持久化，均实测通过。
2. **小对话不省 token，反而略增**：22.6K token 远低于 soft=100K / hard=103.4K，投影只加不减——管理区 +1 416 token（+6.4%），40 条原生消息**全部保留**。EC 的裁剪只在超硬线时才发生，四态推进在本例中一次都没触发（3 块始终停在 `summary`）。
3. **catchup 冷缓存收益为 0**（两会话实测），会话路径约 99.3%。若要降低索引追赶成本，需要让块间 prompt 构成前缀扩展（例如把「滚动累计上下文/上一块摘要」放在 system 之后的稳定前缀位置，再拼接本块 range），而不是每块独立成 prompt。
4. **疑似缺陷（本轮新发现）**：`unsummarized_tokens` 按「记录是否有 `ec` 字段」统计，而 catchup 只给每块**末条**记录盖 `ec` 戳，因此追赶完成后仍有 20 991 token、远超 target=10 000 → 每轮 prepare 只要投影 > soft 就会 `spawn_catchup`（子进程起来发现无新段后立即退出，幂等但空跑）。建议改为统计「最后一次带 `ec` 的记录之后的尾部」，或直接按未摘要块计。

## 4. 产物索引（供复盘）

```
temp/ec-live/traces/
  p1-bootstrap.{invoke,response,messages}.json   # 建档入站/出站/投影
  p1-bootstrap.projection.txt / .stderr.txt      # 人类可读投影 + hook 日志(trace)
  catchup-trace.jsonl                            # 逐块完整 trace（range/请求/响应/usage/耗时）
  catchup.log                                    # core + tantivy 全量日志
  p2-post-catchup.{response,messages}.json / .projection.txt / .stderr.txt
  p3-continue.{response,messages}.json / .projection.txt
temp/ec-live2/traces/                            # 第二会话（冷缓存复现）
temp/ec-live/probe_cache{,2,3}.py                # 缓存控制/阈值扫描
temp/ec-live/sim2_continue_cache.py              # 继续对话 + 会话路径缓存测算
temp/ec-live/run_all.sh                          # 一键复现
```
