# ever-context-hook

Ever-Context（EC）滚动上下文管理器的 **ww3 拦截器插件形态（代理版）**：纯 Rust 单二进制，内嵌 Tantivy 全文索引，无 sidecar 进程。

目标与 `pi-ever-context`（Pi 插件版）一致 —— **在固定上下文预算内实现超长任务的不间断执行**：发给模型的历史按块单向滚动 `RAW → SUMMARY → INDEX_ONLY → OMITTED`，原始 JSONL 与用户可见完整历史一律保留，滚出内容通过本地全文索引按需召回。两个形态互不同步，本仓库只承载代理版。

## 架构

```text
stdin JSONL ──▶ [RPC layer] ──▶ [hook::handle_stream]
                                     │
                   ┌─────────────────┼──────────────────┐
                   │ 请求前处理       │ 事件透传          │ 响应后处理
                   │ · 会话定位       │ · 透传所有事件     │ · append 新消息
                   │ · 祖先链回溯     │ · 收集 usage      │ · 更新叶子
                   │ · 投影构建       │                   │ · 压缩判断
                   └─────────────────┼──────────────────┘
```

## 数据存储

- 会话 JSONL：`<data>/sessions/<session_id>.jsonl`，双侧上下文（agent 侧原文 + 投影产物）并存于同一文件，通过 `message_id` + `parent_id` 支持树状分叉。
- 全文索引：`<data>/index/`（Tantivy，一请求一进程 `open → 读写 → commit → exit`，底层 mmap + OS 页缓存，无常驻内存）。

## 命令

| 命令 | 说明 |
|---|---|
| `call` | ww3 `hook.stream` 的一次调用（core 每请求 spawn 一次） |
| `manifest` | 输出 `manifest.json` |
| `recall` | ec-recall CLI，供 LLM 通过 skill 注入调用以搜索会话历史（不调用 LLM） |
| `catchup` | 旧会话离线追赶压缩；可传 `--model`、`--thinking true|false` |
| `ping` | 自检 |

## 目录

```text
src/            Rust 源码（src/ec/* 为核心实现）
docs/           实施规划、设计文档、实测记录
tools/          数据分析与探针脚本（Python）
e2e/            ww3 沙箱配置与手动验证日志
```

## 摘要模型与思考开关

正常对话触发的增量压缩使用 ww3 hook 配置（`hooks[].config`）：

```json
{"compact":{"model":"可选模型 ID","thinking":true},"catchup":{"model":"可选模型 ID","thinking":false}}
```

`compact.model` 留空时取当前 `request.model`；`compact.thinking` 留空时跟随请求中的 `thinking`（请求未明确传入时为 `false`）。当前请求触发的后台增量压缩虽然调用同一 `catchup` 子命令，仍按 **compact** 配置执行；`catchup` 配置用于独立旧会话追赶。运行旧会话 CLI 时可用 `--model`、`--thinking` 显式指定，或者通过环境变量 `EC_CATCHUP_CONFIG='{"model":"...","thinking":false}'` 指定；未设置时从最后一条已落盘 assistant 的 `model` / `thinking` 读取，缺少模型则报错（历史记录没有模型时请手动指定）。未提供思考记录时按 `false` 执行。`rag` 模型和检索增强回答暂不实现。

新生成的 assistant JSONL 记录保存请求模型、请求思考开关及 `reasoning_effort`、流式推理正文、工具调用、最后一帧 `usage`、结束原因和 EC 观察到的时间与耗时；旧数据不推测缺失字段。渠道只有 `params.upstream.channel_id` 明确提供时记录，**不能把请求的路由意图冒充实际命中渠道**。JSONL 仍是 EC 自有格式，非 Pi JSONL 同构；`usage` 原样保留 ww3 事件字段，不伪造 Pi 的 `cost` / `reasoning` token。

若希望完整对齐 Pi 示例，需 ww3 在 `hook.stream` 契约中透传**实际**命中的渠道 / provider、上游返回的模型 ID、响应 ID、原始停止原因、准确的上游响应时间戳及可用的 cache / reasoning tokens 与费用；当前 EC 只能记录 request.model 和流中可见的 usage / finish_reason，hook 自行计时并非上游时间。建议这些字段作为可选的 `upstream` 元信息或事件提供，保留旧渠道不支持时的空值。

## 构建

```bash
cargo build --release   # 产物 target/release/ever-context-hook.exe
cargo test              # 单元 + 集成测试（当前用例数见 docs/EC-代理版-实施规划.md）
```
