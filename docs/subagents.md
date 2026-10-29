# 子 Agent

pp 可以把独立工作交给多个子 Agent，并在主会话继续工作。子 Agent 使用自己的模型历史、工具状态和日志；任务结束后仍可追问，空闲后释放运行实例，需要时从已保存上下文恢复。

## 日常使用

直接要求主 Agent 拆分独立工作，例如：

> 并行调查运行循环与持久化，分别报告证据，再汇总实现建议。

- `/agents`：查看角色定义、描述和生效来源。
- `/subagents`：打开运行面板；主输入框上方显示子任务摘要。
- 面板中用方向键选择、Enter 查看历史、`e` 追问、`c` 取消本次运行、`x` 停止 Agent、`r` 恢复会话。
- Esc 关闭历史或面板，不会取消 child。主视图中 Esc 取消根任务及其拥有的子任务。
- 人工追问按普通文本发送，`/model` 等文本不会在 child 中执行本地命令。

内置角色：`task` 可执行多步读写并继续委托，`scout` 只读调查，`reviewer` 只读审阅。只读角色从工具清单和执行入口同时限制写入、shell、MCP 与继续委托。

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

`max_concurrency` 是整棵子任务树共享的活跃执行上限，取值 1–32，Main 不占名额。等待结果和人工交互时让出名额；排队任务可以取消。根深度为 0；默认最多两层 child，达到上限后隐藏并拒绝 `task`。深度配置上限为 8。

空闲 7 分钟后进入 `parked`，保留身份、结果、历史与隔离工作区；`idle_ttl_ms: 0` 关闭正常的空闲驻留计时。`request_budget: 0` 关闭请求预算；默认在 200 次请求时提示收尾，达到 300 次停止运行并保留已保存成果。

`agent_models` 按角色名称覆盖模型，例如 `reviewer: provider/review-model`。模型优先级为角色配置覆盖、角色定义、派发时的父模型，模型引用及 thinking 必须在 pp 模型配置中有效。全局配置沿用 pp 的加载与模型重载时机。

## 自定义角色

发现顺序为最近的项目 `.pp/agents/*.md`、用户 `~/.pp/agent/agents/*.md`、内置定义；同名精确匹配，先发现者优先。无效自定义文件记录诊断后跳过。以下定义允许只读研究：

````markdown
---
name: api-reviewer
description: 审阅接口契约和兼容性
model: provider/review-model
thinking: high
tools: [read, grep, glob]
spawns: []
blocking: false
---

Review the assigned API contract. Report concrete compatibility issues and file references.
````

`tools` 省略时采用通用工具集，显式 `[]` 禁用普通工具。`spawns` 列出允许派发的角色，`["*"]` 允许所有角色，省略或空列表禁止继续派发。child 能力还受父能力和工作模式限制。`blocking: true` 使该角色在派发调用中同步返回，其他项仍可后台运行。

普通 child 不复制父对话，只接收角色、任务、共享背景、适用的 `AGENTS.md`、可用 Skill 与相关批准计划。父 TODO 和计划状态仍由 Main 管理。

## 模型工具接口

`task` 默认异步，支持同批多个任务：

```json
{
  "context": "Inspect the current repository and report file references.",
  "tasks": [
    {"name": "Runtime", "agent": "scout", "task": "Explain the execution loop."},
    {"name": "Persistence", "agent": "reviewer", "task": "Review persistence failure modes."}
  ]
}
```

每项返回 `agent_id`、`run_id` 和 `output_uri`。使用返回的身份，不根据显示名猜测 URI。

| 接口 | 行为 |
| --- | --- |
| `wait` | 等待调用者拥有的 Run；可传 `run_ids` 和最多 60000 的 `timeout_ms`；超时不取消 |
| `read agent://<id>` | 查看当前 Run 与最近已发布结果；旧结果标记 `superseded` |
| `write agent://<id>` | 通过 `content` 补充运行中指令，或开始后续 Run |
| `read history://` | 列出当前根会话可访问的子 Agent |
| `read history://<id>` | 返回最近完整历史记录，最多 64 KiB，并提供完整日志路径 |
| `read proc://<run_id>` | 查看状态，不消费结果 |
| `write proc://<run_id>/kill` | 取消该 Run，保留 Agent 上下文 |

后台结果自动回到直接 owner。`wait` 与自动投递共用消费标识，父原生上下文提交成功后才确认消费；普通读取不会消费结果。父模型结束输出但仍有自有子任务时继续等待，结果到达后再汇总。

默认使用最后一条 assistant 消息作为结果。任务项可以提供 `output_schema`，角色也可声明 `output`；调用项优先。结构化 child 获得 `yield`，必须以 `{"data": ...}` 提交符合 JSON Schema 的结果；无效结果不会被标成成功。Schema 外部 HTTP/文件引用解析未开启。

完整结果独立保存为 JSON 文件。较长文本自动回传预览与 `output_path`，可使用普通 `read` 的范围分页读取完整文件。

## 工作区与权限

默认共享当前工作目录，edit/write 修改进入根任务 `/changes`，显示来源 Agent。相互重叠的工作应选择隔离：

```json
{
  "context": "Implement the assigned change and report validation.",
  "tasks": [{"agent": "task", "task": "Implement the parser change.", "isolated": true}]
}
```

隔离需要已有 Git 提交，使用独立 git worktree，带入派发时的受版本控制修改及未忽略的新文件；忽略的构建产物不复制。嵌套仓库保存独立基线和补丁。成功后检查并应用相对基线的修改，父索引保持原样；冲突保留补丁和工作区。取消时不自动合并修改。现阶段采用 patch 回收，不自动创建合并提交。

共享写入与隔离合并统一记录来源。正常 edit/write 和可验证的隔离文件修改沿用条件 undo；shell 的任意修改、隔离删除等无法保证撤销的情况会明确禁用 undo。工作区隔离不提供 OS 沙箱。

子 Agent 继承父审批模式、显式 allow/deny/prompt 和根会话授权。审批与 `ask` 带子 Agent 身份进入同一条 FIFO 交互队列。无 TUI 时，需要交互的调用返回错误，不读取后续 stdin 充当答复。

Plan 可派发只读 child，禁止隔离、修改、MCP 和再次派发。MCP 连接由父方管理，child 使用代理；取消排队请求不会在之后启动该请求，取消已发出的调用不会由 child 关闭同伴连接。

## 保存、恢复和边界

子 Agent 登记、原生上下文、事件与结果存放在根会话对应的 `<session-id>.subagents/` 目录，包含任务正文和项目内容，应按会话数据保护。恢复根会话会展示历史子 Agent；未完成 Run 标为中断，不自动重放工具。显式追问或恢复操作后才重新建立实例。

切换会话、工作模式或历史分支前先停止旧运行；旧历史分支的待投递结果不会自动进入新分支。清理失败会阻止权限或会话转换。成功结果与清理状态分别处理，清理问题不会把已接受结果追溯改成取消。

本次实现覆盖核心运行时与终端控制。workpool、历史咨询、推测启动、advisor、平台原生隔离后端及外部 RPC 属于后续路线。
