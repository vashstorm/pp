# OMP wait 调研与 pp 适配

调研日期：2026-11-02。源码固定为 oh-my-pi 提交 `256b3211bca7458391866a8bcc9369902664be55`。

## 返回结果

OMP 的 `wait({})` 等待调用者拥有的任务、服务或消息，任一事件到达即返回，安全等待上限为 30 分钟。它不接受按 ID 选择和自定义超时。

结果包含两个部分：

- `content` 的文本正文：已结束任务的 ID、类型、状态、输出或错误，以及仍运行目标的列表。结构化子 Agent 结果提供资源入口，避免在正文重复完整数据。
- `details: { op: "wait", jobs: [...] }`：结构化任务快照，含标签、耗时、可选退出码、结果文本、错误和结构化结果元数据。运行中子 Agent 还可带请求数、费用、上下文和自报进度。

显式等待与自动投递共享结果消费机制，避免相同结果随后再次自动注入。收到 Steering 等工具中断时，等待正常返回 `interrupted` 状态，其他取消继续传播。

来源：[等待入口](https://github.com/can1357/oh-my-pi/blob/256b3211bca7458391866a8bcc9369902664be55/packages/coding-agent/src/tools/wait.ts)、[结果组装](https://github.com/can1357/oh-my-pi/blob/256b3211bca7458391866a8bcc9369902664be55/packages/coding-agent/src/async/job-control.ts)。

## 消息框与进度

OMP 每 500ms 推送一次任务快照。界面显示运行数量、成功／失败／取消数量、任务动态状态和耗时；具备额外数据时可展示百分比进度环、数值条或图表。百分比来自任务自身上报，并非按耗时估算。

来源：[等待渲染](https://github.com/can1357/oh-my-pi/blob/256b3211bca7458391866a8bcc9369902664be55/packages/tui/src/tools/wait.ts)、[任务组件](https://github.com/can1357/oh-my-pi/blob/256b3211bca7458391866a8bcc9369902664be55/packages/tui/src/tools/job-widget.ts)。

## pp 的取舍

- 保留既有 `run_ids`／`timeout_ms`，新增 `task_ids`，将默认及最大等待时长扩展为 30 分钟；省略 ID 时等待当前调用者尚未交付的目标。
- 保留子 Agent 的 `delivery_marker`／`run` 信封，后台 Bash 复用现有最终快照和 `artifact://`，不另建 OMP 的消息总线、服务控制或图表协议。
- 结果预留覆盖同一原生工具批次，持久化成功后才确认消费；子 Agent 继续与自动投递去重。后台 Bash 不新增自动结果注入。
- 复用只驻留内存的 `ToolOutputSnapshot` 通道传递 JSON 等待快照，由等待消息框解码；CLI 忽略这些进度帧，最终工具结果正常持久化。
- 首版采用动态运行标识、耗时、状态列表和已结束数量进度条，不新增任务内部百分比上报。

完整接口见[子 Agent 使用说明](subagents.md)中的 `wait`。
