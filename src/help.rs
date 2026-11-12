//! 命令行使用手册（`ec-hook --help` / `-h` / `help` 输出）。
//!
//! 手册内容必须与 `src/main.rs` 的分派、`src/rpc.rs` 的方法表保持一致；
//! 改动任一子命令或参数后同步更新本文件。

/// 完整使用手册（打印到 stdout）。
pub const USAGE: &str = r#"ec-hook — Ever-Context (EC) 滚动上下文管理器（ww3 拦截器插件）

用法:
  ec-hook [call|stdio] [--data <插件数据目录>]          默认子命令；ww3 调用入口（stdin JSONL RPC）
  ec-hook manifest [--out <path>]                      输出插件 manifest（ww3 插件注册用）
  ec-hook recall <模式> [参数...]                       检索会话历史 / 目录 / 精华 / 知识库（不调 LLM）
  ec-hook catchup --session <id> --core-data <dir>      旧对话离线追赶：逐段调 LLM 补摘要
  ec-hook ping                                         自检
  ec-hook --help | -h | help                           打印本手册
  ec-hook --version | -V                               打印版本

全局参数:
  --data <dir>   插件数据目录（所有子命令通用）。仅当环境变量 EC_DATA_ROOT 未设置时生效。
  --help, -h     打印本手册（放在任何位置都生效）

────────────────────────────────────────────────────────────────────────────
call | stdio   —— ww3 Hook 契约入口（每请求一进程，stdin/stdout 行式 JSON）
────────────────────────────────────────────────────────────────────────────
不带子命令时默认就是 call；stdio 与之等价（单请求，非常驻）。

首帧必须是 invoke：
  {"v":1,"id":"req-1","op":"invoke","method":"<method>","params":{...}}

支持的方法:
  hook.stream      双向流。core 用 push 帧喂事件，hook 用 event 帧回吐，
                   收到 upstream.end 后把 assistant 回复落盘 JSONL 并回 result。
                   识别事件: text.delta / reasoning.delta / tool_call.delta /
                             usage / finish_reason / upstream.end(upstream.done)，
                   其余事件原样透传。
  request.prepare  一次性。改写入站请求（增量落盘 + 管理区注入 + 硬预算降级 +
                   注入 ec_recall 工具定义），result = {"request": <改写后的完整请求>}。
  initialize       返回 {protocol, plugin_id, manifest, capabilities}
  capabilities.list 返回 {control:[...], conversation:[...]}
  ping             返回 "pong"
  health.check     返回 {"state":"ok"}
  shutdown         返回 {"ok":true}
  其他方法         返回 error.kind=not_found（core 据此判定可选能力未实现）

退出码: 0 正常 / 1 处理失败

示例（request.prepare）:
  echo '{"v":1,"id":"r1","op":"invoke","method":"request.prepare","params":{
    "hook_id":"ec-1","chain":{"index":0,"total":1},"config":{"enabled":true},
    "request":{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"你好"}]}}}' \
  | ec-hook call --data ./data

示例（hook.stream）:
  printf '%s\n%s\n%s\n' \
   '{"v":1,"id":"r2","op":"invoke","method":"hook.stream","params":{"request":{"model":"m","messages":[]},"config":{}}}' \
   '{"v":1,"id":"r2","op":"push","event":"text.delta","data":{"content":"回答"}}' \
   '{"v":1,"id":"r2","op":"push","event":"upstream.end","data":{"ok":true}}' \
  | ec-hook call

────────────────────────────────────────────────────────────────────────────
manifest  —— 输出插件元数据（ww3 插件注册/商店用）
────────────────────────────────────────────────────────────────────────────
  ec-hook manifest                 # pretty JSON 到 stdout
  ec-hook manifest --out m.json    # 写文件

────────────────────────────────────────────────────────────────────────────
recall  —— 检索（纯本地，不调用 LLM；供人或 LLM 经 bash 调用）
────────────────────────────────────────────────────────────────────────────
模式（优先级从高到低，命中即返回）:
  --add-knowledge --title <t> --summary <s>     写入全局知识条目
  --knowledge --query <q> [--limit <n>]         跨会话知识检索
  --directory --session <id>                    本会话完整标题目录
  --highlights --session <id>                    本会话完整精华区
  --session <id> [--query <q>] [--limit <n>]    默认：关键词召回（含 JSONL 原文回读）

参数:
  --session <id>   会话 ID（默认模式 / --directory / --highlights 必填）
  --query <q>      关键词（--knowledge 必填；默认模式可空）
  --limit <n>      返回条数上限，默认 5

输出标记:
  默认/--knowledge  [EC_RECALL_RESULT] ... [/EC_RECALL_RESULT]
  --directory       [EC_DIRECTORY] ... [/EC_DIRECTORY]
  --highlights      [EC_HIGHLIGHTS] ... [/EC_HIGHLIGHTS]

示例:
  ec-hook recall --session 3f2a9c1b7d4e5061 --query "接口超时" --limit 3
  ec-hook recall --session 3f2a9c1b7d4e5061 --directory
  ec-hook recall --knowledge --query "部署口令"
  ec-hook recall --add-knowledge --title "ww3 端口" --summary "core 默认 127.0.0.1:7864"

────────────────────────────────────────────────────────────────────────────
catchup  —— 旧对话离线追赶（后台逐段调 LLM 补摘要，推进四态）
────────────────────────────────────────────────────────────────────────────
必填:
  --session <id>        目标会话
  --core-data <dir>     core 数据目录

可选:
  --data <dir>                    EC 数据目录（默认取 EC_DATA_ROOT）
  --core <path>                   core 可执行文件（默认 ww3-core）
  --plugins <dir>                 透传给 core 的插件目录
  --model <m>                     摘要模型（默认：EC_CATCHUP_CONFIG.model → 会话末条 assistant 的 model）
  --thinking true|false           摘要请求是否开思考（默认：显式值 → EC_CATCHUP_CONFIG.thinking → 末条 assistant → false）
  --target-tokens <n>             单块目标 token（默认 10000）
  --gap-ms <n>                    块间节流毫秒（默认 200）
  --summary-mode fragment|full_context   摘要输入模式（默认 full_context）
  --full-context-max-tokens <n>   full_context 前缀上限（默认 60000，超出退 fragment）
  --trace <path>                  每块决策/耗时写 JSONL 追踪

示例:
  ec-hook catchup --session 3f2a9c1b7d4e5061 \
    --data ./data --core ./ww3-core.exe --core-data ./data --plugins ./data/plugins \
    --model deepseek-v4-pro --thinking false

────────────────────────────────────────────────────────────────────────────
环境变量
────────────────────────────────────────────────────────────────────────────
  EC_DATA_ROOT       数据根目录（回退 WW3_DATA，再回退 .）
  WW3_DATA           同上（ww3 风格命名）
  EC_CATCHUP_CONFIG  catchup 用的 {"model":"...","thinking":false}
  WW3_CORE_EXE       hook 自动 spawn 追赶时用的 core 可执行文件（默认 ww3-core）
  WW3_CORE_DATA      hook 自动 spawn 追赶时用的 core 数据目录
  WW3_PLUGINS_DIR    hook 自动 spawn 追赶时用的插件目录
  RUST_LOG           日志级别（默认 info）；日志一律写 stderr

退出码: 0 成功 / 1 运行期失败 / 2 用法错误

数据布局:
  <EC_DATA_ROOT>/sessions/<session_id>.jsonl   权威消息树（含 ec 投影）
  <EC_DATA_ROOT>/sessions/<session_id>.leaf    当前叶子
  <EC_DATA_ROOT>/index/                        内嵌 Tantivy 索引（可从 JSONL 重建）

详见 docs/EC-CLI使用说明.md
"#;

/// 打印完整手册。
pub fn print_usage() {
    print!("{USAGE}");
}
