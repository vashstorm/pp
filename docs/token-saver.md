# pp 的省 token 方案

本文汇总当前实现里所有主动降低上下文占用（以及连带降低请求成本）的机制。数字都是默认值，行为以源码为准；改动这些机制时同步更新本文件。

范围：请求装配、单个工具结果、历史回收、前缀缓存、子 Agent 隔离与观测口径。计费与统计库细节见 `crates/pp-stats`，权限与沙盒不在此列。

## 1. 计量口径

- 估算口径固定：原生请求 UTF-8 JSON 字节数 ÷ 3 向上取整；每张内嵌图片计 4096 token（`pp_core::context::estimate_tokens`）。这是容量启发式，不是计费依据。
- 增量记账：`pp-runtime` 的 `SessionEstimate` 只序列化请求骨架与新增消息，避免每轮重算全量历史；整段替换历史（fallback／切换模型／裁剪／压缩／改写系统提示）必须显式 `invalidate()`。`Agent::context_usage()` 的全量重算保留为冷路径与唯一参考口径。
- 阈值校准：自动裁剪与自动压缩判定使用 `calibrated_context_tokens`，把最近一次服务端 `input_tokens` 与当时原始估算的差值外推到新估算；比例超出 `[0.4, 3.0]` 时回退原始估算，避免异常上报或测试桩误触发。
- 自动阈值只在当前模型**显式配置 `context_window`** 时生效，不猜容量。

## 2. 分层总览

| 层 | 机制 | 生效点 | 主要代码 |
|---|---|---|---|
| 请求装配 | 工具表裁剪、MCP 双工具代理、技能只给索引 | 每轮请求 | `crates/pp-tools/src/tools.rs`、`tools/mcp_proxy.rs`、`crates/pp-config/src/skills.rs` |
| 单个工具结果 | 分页、行/字节上限、落盘 `artifact://`、读取去重、展示字段剥离 | 每次工具调用 | `crates/pp-tools/src/tools/*`、`crates/pp-workspace/src/output.rs` |
| 历史 | 60% 无模型裁剪、80% 摘要压缩、超窗应急阶梯 | 每轮请求前、请求失败后 | `crates/pp-runtime/src/agent/prune.rs`、`agent/context.rs`、`agent/turn_loop.rs` |
| 前缀缓存 | 稳定 system／tools、`prompt_cache_key`、动态规则只追加尾部 | 每次请求 | `crates/pp-runtime/src/agent.rs`、`crates/pp-llm` |
| 子 Agent | 独立上下文、结果截断、请求/token/费用预算 | 派发与收尾 | `crates/pp-runtime/src/subagent/*` |

## 3. 请求装配：少发 schema 与正文

- `-p` 默认 `--allowed-tools "Read, Grep, Glob"`，且默认禁用子 Agent（显式包含 `Task` 才开启）。只有允许的工具定义进入 `tools`；`--disallowed-tools` 再减。
- Plan／Review／只读子 Agent 使用各自的白名单工具表，Review 与只读子 Agent 只拿只读授权工具。
- MCP 双工具代理：当前可见 MCP 工具数超过 `mcp.proxyThreshold`（默认 12，或 `mcp.proxy=always`）时，不再把每个 `mcp__*` schema 注入上下文，只固定暴露 `mcp_search` + `mcp_call`。`mcp_search` 默认返回 15 条紧凑条目（名称、简短说明、read_only、精简参数 schema）。工具表在会话期间恒定，同时保护前缀缓存。上限为 32 个服务、256 个工具。
- 提示词与 schema 分工：规则写在 `prompts/runtime.md`，参数与语法写在工具 schema，同一信息不重复占两处 token。
- 技能只注入启停项的名称、说明与编码 URI，正文不自动注入；模型用 `read skill://…` 按需读取（正文也不受普通 read 限额影响，但只在需要时付一次）。

## 4. 单个工具结果：先截断，再落盘

### 4.1 `read`

- 默认每页 ≤ `read_max_lines`（2000 行）且 ≤ `read_max_bytes`（65536 字节）；结果带 `truncated`／`next_offset`／`next_path`，让模型按需续读而不是一次取全量。
- 会话级去重：普通本地文本文件、正文 ≥ 1024 B 时参与；键为规范路径 + 选择器 + 编辑模式 + 读取限额，命中要求 `(dev, ino, len, mtime)` 指纹一致（Hashline 模式还要求对应快照仍激活）。命中返回 `deduplicated:true` 短桩并保留分页元信息，最多 256 条。写入/编辑按路径失效，裁剪或压缩后整体清空；`:raw`、URL、`skill://`／`artifact://`／`agent://`、图片与目录不参与。
- URL：Readability 失败回退整页 HTML 前先剥离 `<script>`／`<style>`／`<noscript>`／`<svg>`／`<nav>`／`<footer>`／`<aside>`／`<header>` 并压缩多余空行。

### 4.2 `rg` 与 `glob`

- `rg` 单行最多 512 B（命中居中裁剪）、每文件最多 20 条（单一目标文件时 200）、每页 20 个文件、正文总量 2000 条、输出正文 ≤ 50 KiB（为截断提示预留 2 KiB）。超过 4 MiB 的文件只搜索前 4 MiB 并给 `searched only the first 4 MiB` 警告；显式单文件与 multiline 走流式读取，命中数仍受上述上限约束。
- `rg` 的 `max_output_tokens` 按 token 预算裁剪并写入 `Output truncated by token limit; narrow paths or pattern.`；`max_count`／`skip`／`files_with_matches`／`count` 让模型先拿元数据再决定是否读正文。上下文行数默认 `rg.contextBefore=1`、`rg.contextAfter=3`。
- `glob` 最多 200 条、50 KiB，按 mtime 降序与路径升序排序；超出时给 `truncated`／`more` 警告，不静默丢弃。

### 4.3 `bash`／`eval`／`bash_task`

- 统一 `OutputSink`：内联窗口上限 = `artifactSpillThreshold`（默认 50 KiB，头 `artifactHeadBytes`=20 KiB + 尾 30 KiB）。超过阈值、或长行被 `outputMaxColumns`（768）截断时，完整输出写入会话 `artifact://`（单文件上限 `artifactMaxBytes`=16 MiB，超限明确省略），模型只看到头尾样本、`artifact`／`truncated` 元数据与恢复提示（`read artifact://N:-80`、`rg artifact://N`）。
- 归一化先去终端转义并折叠 `\r` 原地覆写的进度条帧，只保留可展示正文（CR 行缓冲上限 32 KiB）。
- 后台任务日志是有界累计快照，`wait`／`bash_task` 读快照，不重放全量 stdout。
- `eval` 单次 cell 最多附带 4 张图片。

### 4.4 全局落盘护栏

- 对未自带分页/artifact 的工具（`mcp__*`、`mcp_call`、`lsp`、`web_search`）在模型序列化之后统一加护栏：文本超过 `min(artifactSpillThreshold, 32 KiB)` 时自动落盘，返回有界预览加 `spilled:true`／`tool`／`truncated`；UI 与 journal 的 `ToolFinished` 保留原值。图片结果不重复包裹。

### 4.5 模型侧字段投影

`ModelProjection` 在序列化给模型前临时剥离 UI 展示与冗余副本，序列化后原样还原（UI/journal 不变）。`bash` 系列按条件剥离，其余只有结果里已内联 `content` 时才剥离：

| 工具 | 剥离字段 |
|---|---|
| `bash`／`bash_task`／`wait` | `shell` 展示行 |
| `write`／`edit` | 已内联进 `content` 的 `diagnostics` |
| `lsp` | 已内联的 `diagnostics`／`details` |
| `web_search` | `details` |
| `todo` | `phases` |
| `rg` | `files_with_matches`／`count` 模式下 `details.fileMatches` |

MCP 结果不参与该投影。

### 4.6 图片

- 单边超过 1568 px（`VISION_MAX_IMAGE_SIDE`）默认等比缩小：更高的分辨率只膨胀 vision token 与 base64 体积。
- 硬红线不放宽：输入 ≤ 20 MiB、≤ 3200 万像素、单边 ≤ 8000 px（供应商上限中最严者）。
- 有 alpha 优先 PNG，否则 PNG/JPEG 取体积更小者；结果同时给出实际尺寸与 `Resized from` 原始尺寸。

## 5. 历史层：append-only + 两级回收

### 5.1 平时不动历史

请求历史保持 append-only，不重写前缀，这是命中前缀缓存的前提。裁剪、压缩、树导航、跨协议切换与恢复是仅有的整段替换点；替换后必须重算估算并清空 `read` 去重。

### 5.2 水位 60%：无模型裁剪

`auto_prune`（默认 `true`，`auto_prune_threshold_percent`=60）在未达压缩阈值时触发，同一进度内最多执行一次：

- 保留最近 6 个完整 exchange（应急模式 2 个）。早于边界且结果文本 ≥ 2 KiB 的工具输出替换为短桩，保留 `ok`、`kind`、`source`、`artifact.uri`，并附工具名与 `path`／`command`／`pattern`／`query`／`url`／`action` 前 80 字节的目标提示；已带 `[Pruned tool output:` 标记的桩跳过（幂等）。
- 早于最近 2 个 exchange 的图片替换为 `[Image omitted; re-read if needed]`：Chat 删除整条图片附件消息，Responses 把函数输出折叠成文本。
- Responses 历史中早于工具输出裁剪边界（最近 6／2 个 exchange 之前）的 `Item::Reasoning` 直接移除（encrypted reasoning 体积大，只对当轮有价值）。
- 事务性提交：备份 session → `journal.replace_history` 原子落盘；畸形历史、没有净减少或落盘失败都回滚并放弃。成功后清空 `last_request` 与 `read` 去重缓存。

### 5.3 水位 80%：模型摘要压缩

`auto_compact`（默认 `true`，`auto_compact_threshold_percent`=80）在每次普通请求前判定；`/compact` 手动入口不受阈值限制，空会话直接返回 `Unchanged`（不写 journal、不发请求）。

- 保留后缀预算 `min(16384, window / 5)`：二分选出仍能落在预算内的最早边界（尽量多留近期 exchange），保留原生项，不做有损跨协议转换。
- 摘要预算 `min(2048, window / 16)`；摘要请求 `max_output_tokens = 2 × 预算`；提示词 `prompts/compact.md` 只要求四类信息（请求与约束、进展与证据、待办、下一步），并要求合并既有 `[Context summary]`、省略可恢复内容。
- 摘要请求复用主会话原样的 `system`／`instructions` 与 `tools`，设 `tool_choice: none` 并降到模型允许的最低推理档，所以能命中同一前缀缓存；只有窄窗口下携带完整 system+tools 会超窗时，才回退到移除工具 + 精简系统提示。
- 摘要流不展示、不写 journal。候选必须严格小于原请求（自动触发还必须落到阈值以下），`journal.replace_history` 成功后才替换；任何失败路径恢复旧历史与被暂存的请求控制项。
- 压缩后丢弃后缀里过时的动态 `AGENTS.md` developer 消息，下一轮重新合并进新前缀。

### 5.4 超窗应急阶梯

主循环捕获 `pp_llm::retry::is_context_overflow` 后：先激进裁剪（保留 2 个 exchange），仍超窗或无法缩减再升级自动压缩；两级各最多一次。

## 6. 前缀缓存

- `models.yml` 的 `prompt_cache_key` 默认 `true`：`Session::apply_cache_policy` 用 `pp-<session-id>`（清洗为 ASCII 字母数字与 `-`/`_`，≤48 字符）作 key，无会话 id 时按进程内序号与时间派生；关闭时清除 key。`prompt_cache_retention` 可选 `in_memory`／`24h`。
- 该字段只对接受的供应商有意义：Kimi／GLM／Zai 与 Codex 及非 OpenAI 订阅源在 `wire_request` 中被剥离。
- 稳定前缀的四个做法：首轮冻结基础系统提示词；工具表稳定（含 MCP 代理）；会话中新增或变化的 `AGENTS.md` 只以 `[pp-project-instructions]` developer 消息追加到历史尾部，压缩/新建/恢复/切模型时才重新并入静态前缀；压缩请求复用 system/tools。
- 子 Agent 在启用 `prompt_cache_key` 时按 `(definition, shared_context, plan, model)` 派生 `pp-sub-<hash>` 批次键，同一批并发任务与后续请求共享前缀；子 Agent 各自持有空覆盖的模型状态，不继承主会话的手选模型。

## 7. 子 Agent 与辅助请求

- 子 Agent 有独立上下文、独立会话目录，主 Agent 只看到最终结果：`output` 超过 11500 字符时截为 5000 字符预览并标记 `truncated:true`，同时给出 `output_path`，完整内容按需用 `agent://`／文件读取取回。
- 预算熔断：`task.request_budget`（默认 200）到达时注入一次收尾提示，1.5× 硬停；`task.token_budget`／`task.cost_budget_usd` 同理 1.0× 提示、1.5× 硬停。`task.max_concurrency` 默认 4，排队不占执行名额。
- 摘要压缩的摘要请求与 Plan review 都在主历史之外进行：摘要请求不写 journal、不展示，Plan review 使用独立临时原生 session；两者都不进入主会话历史。

## 8. 观测与调参

- `/context`（别名 `/ctx`）面板按 10 类拆分上下文：System Prompt、Instruction Files、Skills、Built-in Tools、Custom Tools、User / Developer Messages、Assistant Messages、Assistant Thinking、Tool Calls、Tool Output，并按各分类估算份额切分 `Used`。超过 64 KiB 的正文只显示 `<N bytes>` 摘要，但 `bytes`／`tokens` 仍按原始正文计入。面板可直接调整 `auto_compact` 与阈值。
- 每轮请求前发布 `ContextUpdated`（估算值 + 最近一次真实用量 + 累计输出）；恢复会话从 `stats.db` 回填最近请求用量。
- 计费与工具统计在 `pp-stats`：未知缓存读量、缺费率或不可计价的请求整段保持 unknown，不补零。

## 9. 配置速查

| 键 | 默认 | 作用 |
|---|---|---|
| `read_max_bytes` | 65536 | `read` 单次字节上限 |
| `read_max_lines` | 2000 | `read` 单次行数上限 |
| `auto_prune` | `true` | 是否启用无模型裁剪 |
| `auto_prune_threshold_percent` | 60 | 裁剪水位（占 `context_window`） |
| `auto_compact` | `true` | 是否启用摘要压缩 |
| `auto_compact_threshold_percent` | 80 | 压缩水位（1..=99） |
| `tools.artifactSpillThreshold` | 51200 | 内联窗口与落盘阈值 |
| `tools.artifactHeadBytes` | 20480 | 落盘后保留的头字节数 |
| `tools.artifactMaxBytes` | 16777216 | 单个 artifact 字节上限 |
| `tools.outputMaxColumns` | 768 | 单行展示列上限（超限裁剪并落盘） |
| `rg.contextBefore` / `rg.contextAfter` | 1 / 3 | `rg` 上下文行数 |
| `mcp.proxy` / `mcp.proxyThreshold` | `auto` / 12 | MCP 双工具代理开关与阈值 |
| `task.request_budget` | 200 | 子 Agent 请求数预算 |
| `task.token_budget` / `task.cost_budget_usd` | 未设置 | 子 Agent token／费用预算 |
| `task.max_concurrency` | 4 | 子 Agent 并发上限 |
| `models.yml` `prompt_cache_key` | `true` | 是否发送前缀缓存键 |
| `models.yml` `prompt_cache_retention` | 未设置 | `in_memory` / `24h` |

## 10. 边界与已知取舍

- 估算是启发式，不用于计费；口径变化必须同时改 `estimate_tokens` 的调用方与 `/context` 展示。
- 自动裁剪/压缩只在模型显式配置 `context_window` 时生效；未知容量就退化为手动 `/compact`。
- 压缩有损：只保证“摘要 + 预算内近期 exchange”，不保证跨协议完整还原，也不保留被摘要掉的工具原文。
- 裁剪只把旧输出换成短桩；正文能否取回取决于当时是否已落盘为 `artifact://`。artifact 随所属会话删除，`pp gc` 按会话日志的最后修改时间判断整个会话是否过期。
- `read` 去重指纹不含内容哈希：同 inode、同长度、同 mtime 的覆盖写在极端情况下可能误命中；`:raw` 可强制重读，写入路径另有快照与失效校验。
- 字段投影只影响模型可见 JSON；`ToolFinished`、journal 与终端卡片保留完整结果。
- 前缀缓存是否命中的最终决定权在供应商；字段被剥离的供应商没有命中保证，不要用命中率断言正确性。
- 32 KiB 全局落盘护栏只覆盖 `mcp__*`／`mcp_call`／`lsp`／`web_search`；其余内置工具依赖各自的分页与 `OutputSink`。

## 11. 源码索引

| 关注点 | 位置 |
|---|---|
| 估算口径、10 类诊断 | `crates/pp-core/src/context.rs`、`crates/pp-runtime/src/agent/context_diagnosis.rs` |
| 增量记账、校准 | `crates/pp-runtime/src/agent/usage.rs` |
| 无模型裁剪 | `crates/pp-runtime/src/agent/prune.rs` |
| 事务压缩与摘要预算 | `crates/pp-runtime/src/agent/context.rs`、`crates/pp-runtime/src/prompts/compact.md` |
| 阈值判定与超窗阶梯 | `crates/pp-runtime/src/agent/turn_loop.rs` |
| 缓存键与批次键 | `crates/pp-runtime/src/agent.rs`、`agent/session_lifecycle.rs`、`agent/subagents.rs` |
| 去重、分页、URL 清洗 | `crates/pp-tools/src/tools/read/` |
| 搜索/路径结果上限 | `crates/pp-tools/src/tools/rg.rs`、`tools/glob.rs` |
| 展示字段投影、全局护栏 | `crates/pp-tools/src/tools/shell_fields.rs`、`tools/lifecycle.rs` |
| 输出窗口、落盘、归一化、恢复提示 | `crates/pp-workspace/src/output/` |
| 图片降采样 | `crates/pp-workspace/src/image.rs` |
| MCP 双工具代理 | `crates/pp-tools/src/tools/mcp_proxy.rs` |
| 子 Agent 结果截断与预算 | `crates/pp-runtime/src/subagent/worker.rs`、`agent/turn_loop.rs` |

相关回归面包括 `cargo test --locked -p pp-runtime --lib context`（含 `prune`／`calibrated` 用例）、`cargo nextest run --locked -p pp-tools --lib artifact`（含后台产物用例）和根 `tests/cli_agent` 的 `context_` 端到端用例。
