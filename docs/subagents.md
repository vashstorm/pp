# 子 Agent

pp 可以把独立工作交给多个子 Agent，并在主会话继续工作。子 Agent 使用自己的模型历史、工具状态和日志；任务结束后仍可追问，空闲后释放运行实例，需要时从已保存上下文恢复。

## 日常使用

直接要求主 Agent 拆分独立工作，例如：

> 并行调查运行循环与持久化，分别报告证据，再汇总实现建议。

- `/agents`：查看可用系统 Role、自定义 Agent 及其配置来源。
- `/subagents`：打开 Agent Hub 的 Agents 页；主输入框上方显示子任务摘要。
- `⌥ + A`（macOS）／`Alt + A`（Linux）：在主视图与 Agent Hub 之间切换，补全菜单展开时也可使用；保留主输入草稿，其他模态面板仍独占输入。未获得子 Agent 绑定时显示空态而不是报错。
- Hub 使用蓝色外框、左右分栏线及顶部汇总／底部快捷键分隔线；窄终端保留单栏切换。
- `/test-task`：dev/test 构建专用的固定自检（其他构建由 `#[cfg(debug_assertions)]` 编译排除，命令名不注册，按未注册命令静默忽略），要求 FAST／SLOW／REVIEW 已启用，派发 FAST、SLOW、REVIEW 三个只读子 Agent 并等待结果；详见[常用命令](guide.md#常用命令与快捷键)。

Hub 分两页，`1`／`2` 切换：

- Agents：列表与 inspector。`j`/`k` 或方向键选择，`t` 切换父子树，`/` 过滤 id 与名称（树中保留匹配项的祖先），`Enter` 打开详情，`e` 打开详情并聚焦输入，`c` 取消本次运行、`x` 立即停止 Agent、`r` 在 parked／aborted 上恢复，`F5` 重新扫描。宽屏右侧 inspector 显示身份、模型、任务、当前活动、用量与上下文、lineage、变更归属和最近活动；窄屏用 `Tab` 在列表与 inspector 之间切换，`Esc` 先清过滤、再退出 inspector、最后关闭 Hub。
- Activity：完整活动流，`f` 切换 All／Errors／Responses／Tools，`s` 切换 All agents／Selected agent／Subtree，`/` 搜索，默认跟随最新；手动移动会暂停跟随并固定选中活动，`Space` 恢复跟随。`Enter` 打开对应 Agent 的详情并定位到该条活动。
- 详情是全屏独立视图：正文用 child 自己的 transcript，底部是独立输入框。普通字符与方向键都进入 child 输入框，Enter 发送原文，Ctrl+J 换行，Ctrl+A／E／K 沿用主编辑器语义，PgUp／PgDn 与滚轮滚动正文，Ctrl+Home／End 跳顶／回底，Ctrl+O 只切换 child 的工具展开。输入不执行 slash、本地 shell、`@` 文件展开或主会话历史。
- 详情消息区复用主 Agent 的 Markdown、代码高亮、工具卡片、diff 和运行状态显示，并继承 read 预览与 bash 超时配置；正文、滚动位置和工具展开状态独立，重开详情恢复默认折叠。运行计时从日志中的开始时间恢复，完成耗时使用已保存的任务耗时；缺失的时间显示 `—`，历史工具不按导入速度生成 `Wall`。
- Esc 从详情回到来源页并保留 child 草稿，再 Esc 关闭 Hub；关闭 Hub 不取消任何 child。主视图中 Esc 取消根任务及其拥有的子任务。
- 人工追问按普通文本发送，`/model` 等文本不会在 child 中执行本地命令。发送成功才清掉未修改的草稿，失败保留草稿并提示；父方在成功后按既有规则取回结果。
- 详情与 Activity 使用与主对话相同的观察日志，历史超过 64 KiB 也能从头浏览；旧日志缺少时间戳时显示 `—`，不推算当前时间或费用。

子 Agent 可直接选择系统 Role，也可选择已加载的自定义 Agent。系统 Role 包括：DEFAULT、FAST、SLOW、VISION、REVIEW；PLAN 仅用于主 Agent 进入计划模式。REVIEW 始终只读，其他 Role 可用 `readonly: true` 限制为只读调查。Plan 中的所有子 Agent 强制只读。只读限制同时作用于工具清单和执行入口，禁止写入、shell 与继续委托。只读子 Agent 可使用父级开放、用户标为只读且在自身工具集合中的 MCP 工具。

## 配置

全局 `~/.pp/agent/settings.yml` 可增加以下设置；省略时采用示例中的默认值：

```yaml
task:
  max_concurrency: 4
  max_recursion_depth: 2
  idle_ttl_ms: 420000
  request_budget: 200
  disabled_agents: []
  agent_models: {}
```

`max_concurrency` 是整棵子任务树共享的运行中子 Agent 数量上限，默认 4，取值 1–32，Main 不占名额。每个 Run 在初始化及准备隔离工作区前获取名额，进入 `running`，直到成功、失败或取消后的结果收尾结束才释放；模型轮次、工具调用、等待结果和人工交互期间都持续占用。不限制已创建或空闲的 Agent 总数；未获得名额的任务保持 `queued`，可以取消，已有 Agent 的后续 Run 也必须重新排队获取名额。配置修改后重启 `pp` 生效。

只有 Main 可以派发子 Agent；所有 child（包括恢复的旧任务）均隐藏并拒绝 `task`，不生成该工具的 schema、description 或自动委托使用提示。`max_recursion_depth` 保留默认值 2 和 0–8 的解析范围：0 禁用 Main 派发，1–8 最多允许 Main 派发一层，不能开启递归。旧任务树仍可观察、取消和恢复已有任务。

空闲 7 分钟后进入 `parked`，保留身份、结果、历史与隔离工作区；`idle_ttl_ms: 0` 关闭正常的空闲驻留计时。`request_budget: 0` 关闭请求预算；默认在 200 次请求时提示收尾，达到 300 次停止运行并保留已保存成果。

使用 `/Roles` 为系统角色配置主模型、effort 和可选备用模型。每个任务项通过 `role` 选择角色，例如 `{"role":"fast","readonly":true,"task":"定位实现"}`；可同时用 `agent` 选择实际存在的自定义定义。`agent` 的工具枚举只列已发现且启用的名称；未知、停用或不存在的名称在整批派发前拒绝，不自动补出 `scout`、`task`、`reviewer` 等定义。

选模优先级为显式 `role`、自定义 Agent 的模型配置、DEFAULT。显式 Role 同时决定 model、effort 和 fallback；未显式指定 Role 时，`task.agent_models` 按自定义名称覆盖定义中的 `model`，`thinking` 采用定义值，缺省采用 DEFAULT 的有效 effort（DEFAULT 不可用时为 none）。自定义 Agent 没有模型配置时使用 DEFAULT；无 `agent` 的普通任务省略 `role` 也使用 DEFAULT。所有路径都不继承父 Agent 当前的 PLAN 模型。

DEFAULT 尚未配置时使用全局默认模型及其有效默认 effort；显式停用 DEFAULT 后，需选择其他已启用 Role，或使用配置了独立模型的自定义 Agent。其他未配置或停用的系统 Role 不进入工具可选项，显式调用在整批派发前报错。既无可用系统 Role、也无自定义定义时隐藏 `task` 工具。

PLAN 始终不进入子 Agent 的模型 Role 可选项，显式指定 `role: "plan"` 会在整批派发前报错。派发时冻结完整主备路由；后续 `/Roles` 更新仍遵循安全边界应用规则。详见[模型 Roles](guide.md#模型-roles)。

## 自定义 Agent

发现顺序为最近的项目 `.pp/agents/*.md`、用户 `~/.pp/agent/agents/*.md`；同名精确匹配，项目定义优先。无效文件记录诊断后跳过，`task.disabled_agents` 仅过滤自定义 Agent。定义必须真实存在且成功加载；模型不能仅通过 `agent` 参数或 `agent_models` 中的名称创建新角色。

````markdown
---
name: api-auditor
description: 审阅接口契约和兼容性
model: provider/review-model
thinking: high
tools: [read, grep, glob, lsp]
spawns: []
blocking: false
---

Review the assigned API contract. Report concrete compatibility issues and file references.
````

文件加载后可调用 `{"agent":"api-auditor","task":"审阅接口兼容性"}`；显式附加 `"role":"fast"` 会使用 FAST 的模型和 effort，同时保留该自定义 Agent 的提示词、工具和输出约束。

`tools` 省略时采用通用工具集，显式 `[]` 禁用普通工具；仅含 read／grep／glob／lsp 的工具集自动应用只读限制。无论默认工具、自定义 `tools` 还是旧记录，child 的有效工具集都会剔除 `task`。`spawns` 字段保留解析与存储兼容，但即使设为 `["*"]` 也不能让 child 继续派发。子 Agent 能力仍受工作模式限制。`blocking: true` 在批次全部启动后等待该自定义任务，默认异步；普通异步任务可用 `wait` 等待结果。`output` 可定义结构化结果约束，任务项的 `output_schema` 优先。

普通 child 不复制父对话，只接收系统 Role 对应的任务约束、任务、共享背景、适用的 `AGENTS.md`、可用 Skill 与相关批准计划；不注入 Main 的 Plan／批准执行委托规则。父 TODO 和计划状态仍由 Main 管理。自定义提示词、任务背景、项目规则和批准计划中的原文不会被字符串清洗，普通任务说明、结果约定及 `yield` 提示保持不变。

## 模型工具接口

`task` 默认异步，支持同批多个任务：

```json
{
  "context": "Inspect the current repository and report file references.",
  "tasks": [
    {"name": "Runtime", "role": "fast", "readonly": true, "task": "Explain the execution loop."},
    {"name": "Persistence", "role": "review", "task": "Review persistence failure modes."}
  ]
}
```

每项返回 `agent_id`、`run_id` 和 `output_uri`。使用返回的身份，不根据显示名猜测 URI。

| 接口 | 行为 |
| --- | --- |
| `wait` | 等待选中的自有子 Agent 与后台 Bash 全部进入终态；支持 `run_ids`／`task_ids`，固定上限 120 分钟，超时后终止并回收本批未完成工作 |
| `read agent://<id>` | 查看当前 Run 与最近已发布结果；旧结果标记 `superseded` |
| `write agent://<id>` | 通过 `content` 补充运行中指令，或开始后续 Run |
| `read history://` | 列出当前根会话可访问的子 Agent |
| `read history://<id>` | 返回最近完整历史记录，最多 64 KiB，并提供完整日志路径 |
| `read proc://<run_id>` | 查看状态，不消费结果 |
| `write proc://<run_id>/kill` | 取消该 Run，保留 Agent 上下文 |

后台结果自动回到直接 owner。`wait` 与自动投递共用消费标识，父原生上下文提交成功后才确认消费；普通读取不会消费结果。父模型结束输出但仍有自有子任务时等待该批全部结束，再交付结果并汇总；隐式等待同样使用 120 分钟上限，Steering 不会提前放行。

`wait({})` 自动选择当前调用者拥有且尚未交付结果的目标；任一 ID 列表非空时仅等待显式指定目标，两个列表可以混用。目标集合在调用开始时冻结，新启动任务不会加入本次等待。工具 schema 不再提供 `timeout_ms`；旧调用中的非负整数仍可解析，但数值完全忽略，包括 `0`。未知、其他 owner 或上一用户任务的后台 ID 整体报错，不消费其他有效结果。

返回值保留 `ok`、`results` 和 `pending`，并增加 `background_results`、`jobs`、`timed_out`、`interrupted`。`results` 保留子 Agent 的 `delivery_marker`／`run` 信封；`background_results` 保存后台 Bash 终态、退出码、有限输出及 artifact 元数据；`jobs` 为本次目标的 ID、类型、标题、状态、耗时与交付状态。`ok` 表示等待调用成功，任务自身仍可能失败或取消。已交付目标再次等待只返回状态，查看正文使用不消费结果的读取接口。后台 Bash 仍只在显式等待时交付，不增加自动注入。

Steering 与 FollowUp 在等待期间保留队列，批次全部结束并成功提交后才按原规则领取。某个目标提前成功、失败或取消都不会放行其余目标。120 分钟期限到达后，先取消本批未完成目标及其派生工作，并等待真实回收，再返回 `ok:false, timed_out:true, pending:false`，保留已完成结果和可用的部分输出；清理失败升级 `CleanupError` 并停止父 Agent。Esc 取消整个当前任务，回收后恢复输入，取消不提前确认结果消费。TUI 每 500ms 更新同一个框中的运行标识、耗时和任务状态，进度条表示已结束数量／目标总数，不表示任务内部的完成百分比。默认显示 8 项，Ctrl + O 展开全部目标；等待框始终只显示任务概况，不显示 Result 正文，子 Agent 完整结果在 Agent Hub（`/agents` 或 `/subagents`）查看，后台 Bash 输出保留在原 Bash 卡片。整批结束后冻结最终快照，后续 wait 新建框。历史恢复展示持久化的最终快照，不重新查询当前任务。参见 [OMP wait 调研与适配](wait-research.md)。

默认使用最后一条 assistant 消息作为结果。任务项可以提供 `output_schema`，也可采用自定义定义中的 `output`；任务项优先。结构化 child 获得 `yield`，必须以 `{"data": ...}` 提交符合 JSON Schema 的结果；无效结果不会被标成成功。Schema 外部 HTTP/文件引用解析未开启。

完整结果独立保存为 JSON 文件。较长文本自动回传预览与 `output_path`，可使用普通 `read` 的范围分页读取完整文件。

## 工作区与权限

默认共享当前工作目录，edit/write 修改进入根任务 `/changes`，显示来源 Agent。相互重叠的工作应选择隔离：

```json
{
  "context": "Implement the assigned change and report validation.",
  "tasks": [{"role": "default", "task": "Implement the parser change.", "isolated": true}]
}
```

隔离需要已有 Git 提交，使用独立 git worktree，带入派发时的受版本控制修改及未忽略的新文件；忽略的构建产物不复制。嵌套仓库保存独立基线和补丁。成功后检查并应用相对基线的修改，父索引保持原样；冲突保留补丁和工作区。取消时不自动合并修改。现阶段采用 patch 回收，不自动创建合并提交。

共享写入与隔离合并统一记录来源。正常 edit/write 和可验证的隔离文件修改沿用条件 undo；shell 的任意修改、隔离删除等无法保证撤销的情况会明确禁用 undo。工作区隔离不提供 OS 沙箱。

子 Agent 继承父审批模式、显式 allow/deny/prompt 和根会话授权。YOLO 模式跳过全部工具审批，包括显式 prompt/deny 规则与 bash 危险命令确认；其他模式继续应用这些规则。审批与 `ask` 带子 Agent 身份进入同一条 FIFO 交互队列。无 TUI 时，需要交互的调用返回错误，不读取后续 stdin 充当答复。

Plan 可派发只读 child，禁止隔离、修改和再次派发。MCP 连接由父方管理，child 使用代理；取消排队请求不会在之后启动该请求，取消已发出的调用不会由 child 关闭同伴连接。

Plan 提示词要求将复杂任务拆成独立调研问题，通过同批 `task` 派发多个只读子 Agent；主 Agent 同时推进其他调研，汇总并核实结果后保存计划。实施任务须写明修改范围、前置依赖、交付物和验收条件，标明可并发的任务与集成责任。本地批准后，执行提示词要求按依赖并发派发已就绪的独立任务；共享目录下的写入范围应互不重叠，冲突修改串行处理或使用受支持的隔离并核对回收结果。主 Agent 统一维护计划进度并验证集成结果。简单任务不强制增加子 Agent，实际并发受当前工具、角色及运行时上限约束。

执行提示词将多 Agent 并发列为默认策略：有两个及以上就绪的独立任务且容量允许时，必须同批派发，不能派发一个就等待再派发下一个；每次结果返回后重新检查依赖，及时启动新就绪任务，无需等待无关任务全部结束。主 Agent 同时推进其他独立工作，仅在没有可推进工作时等待；串行执行需说明具体依赖、修改冲突或工具／容量限制。

## 保存、恢复和边界

子 Agent 登记、原生上下文、事件与结果存放在根会话对应的 `<session-id>.subagents/` 目录，包含任务正文和项目内容，应按会话数据保护。恢复根会话会展示历史子 Agent；未完成 Run 标为中断，不自动重放工具。显式追问或恢复操作后才重新建立实例。

切换会话、工作模式或历史分支前先停止旧运行；旧历史分支的待投递结果不会自动进入新分支。清理失败会阻止权限或会话转换。成功结果与清理状态分别处理，清理问题不会把已接受结果追溯改成取消。

本次实现覆盖核心运行时与终端控制。workpool、历史咨询、推测启动、advisor、平台原生隔离后端及外部 RPC 属于后续路线。
