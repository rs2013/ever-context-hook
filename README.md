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
| `recall` | ec-recall CLI，供 LLM 通过 skill 注入调用以搜索会话历史 |
| `ping` | 自检 |

## 目录

```text
src/            Rust 源码（src/ec/* 为核心实现）
docs/           实施规划、设计文档、实测记录
tools/          数据分析与探针脚本（Python）
e2e/            ww3 沙箱配置与手动验证日志
```

## 构建

```bash
cargo build --release   # 产物 target/release/ever-context-hook.exe
cargo test              # 单元 + 集成测试（当前用例数见 docs/EC-代理版-实施规划.md）
```
