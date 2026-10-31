<h1 align="center"><img src="docs/assets/logo.svg" alt="pp" width="40" height="40" align="middle" /> pp</h1>

<p align="center"><a href="README.md">英文</a> | <strong>简体中文</strong></p>

<p align="center"><strong>纸飞机 · 终端里的编程助手</strong></p>
<p align="center">Rust · macOS / Linux · 兼容 OpenAI 的响应接口与聊天补全接口</p>

<p align="center">
  <a href="#快速开始">快速开始</a> ·
  <a href="#日常使用">日常使用</a> ·
  <a href="#扩展能力">扩展能力</a> ·
  <a href="docs/guide.md">完整指南</a>
</p>

---

pp 是用 Rust 编写的 Pi 风格终端编程助手。让模型直接阅读项目、编辑文件、搜索代码和执行命令，在终端中完成多轮开发任务。

TUI 默认最大帧率为 60 FPS。可在 `~/.pp/agent/settings.yml` 中将 `tui.max_fps` 设为 1 到 120 的整数，重启后生效。输入和取消独立于绘制处理，流式文本按帧合并显示。

## 功能一览

| 功能 | 说明 |
| --- | --- |
| **终端交互** | 流式正文、思考与工具输出，支持终端界面、单次任务和管道输入 |
| **规划与执行** | 计划调研、审阅与显式批准，通过询问面板收集关键选择；`/plan-list` 管理 drafts、plans、done 三类已保存计划 |
| **任务清单** | 输入框上方固定进度、执行阶段可展开清单、批准计划的 checkbox 双向同步，以及有次数上限的完成提醒 |
| **任务控制** | 运行中补充需求、排队后续任务，Esc 取消当前任务 |
| **会话与变更** | 自动保存与恢复、会话树回退、按工作目录持久回忆输入、`/resume` 检索全部 prompt、最近任务差异与安全撤销 |
| **按需扩展** | 项目 `AGENTS.md`、技能、Markdown 提示词命令、基于本地标准输入输出的 MCP 工具 |
| **用量可见** | 上下文估算与压缩，本地请求、词元、费用和耗时统计 |

## 快速开始

### 1. 构建

需要支持 **2024 版语言规范**的 Rust 工具链，以及可在命令搜索路径中找到的 `bash`。

```sh
cargo build --release --locked
mkdir -p ~/.pp/agent
```

`grep` 和 `glob` 在进程内搜索，不依赖外部搜索程序。终端界面图标需要 Nerd Font 字体；macOS 使用 Option 快捷键时，终端需启用 Alt/Meta。

### 2. 配置模型

没有模型配置时，也可以先启动终端界面，再用 `/login` 选择 ChatGPT、GitHub Copilot、Kimi Code、GLM、Z.AI 或 Command Code：

```sh
./target/release/pp
```

选择服务商后自动打开授权页面。ChatGPT、Copilot 和 Kimi 使用设备码授权；GLM、Z.AI 与 Command Code 使用遮蔽的 API key 输入。凭证以明文保存在 `~/.pp/agent/agent.db`，目录权限为 `0700`、数据库文件为 `0600`，不提供系统钥匙串或加密保证。发现的订阅模型仍单独保存到 `~/.pp/agent/models-sub.yml`，该文件保持原有格式。发现失败时保留授权并提供重试，不补造默认模型。升级前请退出所有运行中的 pp：首次启动时会把 `settings.yml` 的旧偏好字段和 `auth.yml` 一次性导入 `agent.db`，随后从 YAML 中移除授权及这些字段。

登录后可使用 `/model`（别名 `/models`）选模型，无需重启。空闲会话立即切换；运行中则等当前回复与整批工具结果完整提交后自动切换。它重新读取两个模型文件，同名 `provider/model` 以 `models.yml` 为准。`/logout` 从 `agent.db` 删除目标服务商凭证，并删除其订阅模型。授权命令仅支持终端界面，不再提供 `pp auth` 参数入口，也不读取或迁移旧 `auth.json`。Kimi／GLM 当前使用非思考模式。详见[订阅认证](docs/guide.md#订阅认证)。

使用普通 API key 时，按以下方式配置。

创建 `~/.pp/agent/models.yml`，替换密钥和实际模型标识：

```yaml
providers:
  openai:
    api_key: your-api-key
    base_url: https://api.openai.com/v1
    protocol: response
    models:
      main:
        model: your-model-id
```

- `protocol`：`response` 使用响应接口（默认）；`completion` 使用聊天补全接口。
- `base_url`：填写供应商的接口根地址，程序按协议追加端点，不自动补 `/v1`。
- 支持多个供应商与模型；未设置默认模型时，使用文件中的第一个模型。

<details>
<summary><strong>可选：应用设置、多模型与思考档位</strong></summary>

创建 `~/.pp/agent/settings.yml`；省略此文件即可使用默认设置。默认模型、思考档位、收藏和六个模型 Role 由终端界面管理（`/model`、`/effort` 菜单的保存快捷键、Ctrl + F、`/Roles`），保存在 `~/.pp/agent/agent.db`，不再写入 YAML；其余设置仍为 `settings.yml` 中的声明式 YAML。

```yaml
edit: replace
auto_compact: true
auto_compact_threshold_percent: 80
```

模型请求不限制总耗时。`request_timeout_seconds` 默认 300 秒，限制建连及连续无数据的闲置等待；收到正文、思考或 SSE 心跳后重新计时，已有显式配置值仍生效。

自动压缩仅在模型显式配置 `context_window` 时启用。思考档位须与供应商实际能力匹配，可在模型配置中使用 `thinking.efforts` 和 `thinking.defaultLevel` 限定可选档位与默认值。

完整示例与校验规则见[配置指南](docs/guide.md#配置)。模型与应用设置只从 `~/.pp/agent/` 加载，不读取项目同名文件或供应商环境变量；[系统提示词](docs/guide.md#系统提示词)也可从启动目录的 `.pp/` 加载。

</details>

### 3. 启动

```sh
# 使用 models.yml 时限制文件权限
chmod 600 ~/.pp/agent/models.yml
./target/release/pp
```

标准输入和输出均为终端时自动进入终端界面，否则使用行模式。将构建产物放入命令搜索路径后，可直接使用下文的 `pp`。

```sh
pp -p '阅读当前项目并说明结构，不修改文件。'
printf '说明当前项目的构建方式\n' | pp
```

> **权限与数据**：接口密钥明文保存在模型配置中。读取、命令执行、搜索与 MCP 工具按当前用户权限运行，没有文件系统沙箱或逐次操作确认；写入与编辑工具的保存层限制在工作目录内并拒绝符号链接。会话与统计可能包含源码和对话，请按敏感数据保管。[查看完整权限说明 →](docs/guide.md#工具执行与权限)

## 日常使用

终端界面进入模型选择后清空主输入框，自动聚焦列表上方的独立搜索框。面板打开时按模型总数确定高度，后续筛选保持高度不变；整个面板最多 15 行（搜索框 3 行、列表最多 11 行、提示 1 行），超过后可通过方向键、翻页键或面板内滚轮滚动，并显示滚动条。搜索支持输入即筛选，忽略大小写并匹配 `provider/model` 或实际模型名的任意片段。Ctrl + F 收藏／取消收藏高亮模型，收藏项显示纯金黄色 Nerd Font 实心星标 `` 并始终排在其他模型之前；每组内按使用次数从多到少排列，次数相同时保持模型目录顺序。次数统计自升级以来实际尝试发送的逻辑模型 API 请求：同一请求的建连重试只计一次，打开面板或切换模型不计。收藏与次数保存在 `~/.pp/agent/agent.db`，重启后保留。Ctrl + S 保存默认模型。无匹配结果时显示提示，Enter、Tab 不执行操作。

`/help` 查看完整帮助，`/tips` 查看操作提示。以下斜杠命令在交互模式使用；`-p` 中的内容始终作为普通提示词发送。

模型持续输出时仍可打字、粘贴和打开模型面板。Markdown 解析、语法高亮与正文排版在后台线程完成，样式就绪前可能短暂显示纯文本预览。切换模型保留已提交上下文，不取消当前请求、不重复已完成任务，也不重启后台工作；历史内容无法兼容目标协议时拒绝切换并保留当前模型。

通过 `/Roles` 保存模型与思考档位后，Default 会让主 Agent 在下一个安全边界切换，不影响已有子 Agent；其他角色的变更由绑定该角色的子 Agent 各自在安全边界应用，包括嵌套子 Agent。没有角色绑定、仅在创建时继承模型的子 Agent 保持原配置。禁用或清除角色不会取消或重新绑定已有子 Agent；不监听其他 pp 进程的配置热更新。

`/task-test` 只在 cargo debug 构建中提供。它要求已启用 FAST、SLOW、REVIEW 三个模型 Role，保持当前模型，并发送一个固定的只读批次：`scout`（role 为 `fast`）、`scout`（role 为 `slow`）与 `reviewer`，等待三份结果后汇总；不会修改仓库文件。release 构建下该命令在本地报错，且不出现在 `/help` 与 `/` 命令菜单中。

| 命令 | 用途 |
| --- | --- |
| `/model`（`/models`）、`/effort` | 切换模型与思考档位，保留历史 |
| `/init` | 创建或更新当前项目的 `AGENTS.md` |
| `/plan [prompt]` | 切换 Plan 模式待命，可查看当前计划或附带任务立即发送，批准后执行（仅终端界面） |
| `/plan-list` | 管理已保存计划：预览、继续规划、执行、重命名、删除（仅终端界面） |
| `/task-test` | 仅 debug 构建：按固定场景派发 FAST／SLOW／REVIEW 三个只读子 Agent 并等待三份结果 |
| `/mcp [list\|enable <name>\|disable <name>\|reconnect <name>\|reload\|help]` | 查看和管理 MCP 服务，无需重启 |
| `/compact`、`/new` | 压缩上下文，或开始新会话 |
| `/resume`、`/tree` | 恢复会话，或回退当前历史（仅终端界面） |
| `/changes` | 查看最近任务的编辑与写入变更并逐文件撤销（仅终端界面） |
| `/skills`、`/quit` | 管理技能，或退出 |

输入 `!命令` 可在项目目录运行本地非交互 bash；输出仅在当前界面显示，不发送给模型，也不写入会话历史。需要有效的 Agent 配置和 Execute 模式；命令以当前用户权限运行，没有沙箱，每次启动独立 shell。终端界面中，首个 `!` 进入 shell 模式且不显示在输入框中，后续 `!` 保留为命令正文。编辑时按 Esc 退出 shell 模式，空输入框按 Backspace 也可退出；命令运行时按 Esc 取消，等待清理完成后再继续。

<details>
<summary><strong>终端界面常用快捷键</strong></summary>

| 操作 | macOS | Linux |
| --- | --- | --- |
| 发送／运行中补充需求 | Enter | Enter |
| 排队为独立后续任务 | ⌥ + Enter | Alt + Enter |
| 换行 | Ctrl + J | Ctrl + J |
| 模型菜单 | ⌥ + P | Ctrl + P |
| 收藏／取消收藏高亮模型 | Ctrl + F | Ctrl + F |
| 循环思考档位 | Shift + Tab | Shift + Tab |
| 展开／折叠工具输出 | Ctrl + O | Ctrl + O |
| 展开／收起固定 TODO 区 | ⌥ + T | Ctrl + T |
| 翻页浏览展开的 TODO 区 | ⌥ + PgUp/PgDn | Ctrl + PgUp/PgDn |
| 查看待处理输入 | ⌥ + Q | Ctrl + Q |
| 恢复暂停队列 | ⌥ + R | Ctrl + R |
| 取消当前任务 | Esc | Esc |
| 退出 | Ctrl + C | Ctrl + C |

`@` 打开项目文件路径补全。空闲且输入为空时，双击 Esc 打开会话树；回退历史不会回滚文件。[完整交互说明 →](docs/guide.md#终端交互)

`read` 仅接受 `path`，支持本地文本、图片、静态网页和已启用的 `skill://` 资源。系统在技能名称、说明和 URI 旁附加选择规则，自定义基础提示词也会保留：按任务及具体子任务主动匹配，在其他任务工具前读取适用技能，成功读取后简短说明用途。执行已批准计划也适用；选择仍由模型完成，正文不自动注入。裸技能 URI 读取完整 `SKILL.md`，追加 `/<path>` 访问资源，`/.` 列出目录。技能文本不受普通文本限额限制，结果带 `path` 和 `base_dir`，显式选择器仍生效。范围直接写入路径：`src/main.rs:20-60`、`notes.md:-30`、`file.rs:5-10,20-30`，严格读取指定行，截断后使用 `next_path` 续读。`:raw` 去掉前缀，网页则返回原始响应正文。PNG／JPEG／WebP／GIF 图片作为真实视觉输入发给模型，终端只显示元信息；恢复会话和切换协议保留图片。网页提取静态 HTML 正文，不执行 JavaScript，支持取消和有界会话缓存。[读取行为和限制 →](docs/guide.md#工具执行与权限)

`grep` 在进程内搜索本地文件，支持正则、路径 glob、文件行范围和按文件分页。Execute、Plan 和 Review 共用 `pattern`、`path`、`case`、`gitignore`、`skip` 参数；可在 `settings.yml` 设置 `grep: {contextBefore: 1, contextAfter: 3}` 调整上下文。

`glob` 在 Execute、Plan 和 Review 中查找本地路径，接受 `path`、`hidden`、`gitignore` 和 `limit`；默认包含隐藏文件并遵守忽略规则。裸 `*.rs` 递归查找，`src/*.rs` 只查找直接子项。结果按修改时间排序，最多返回 200 个路径和 50 KiB；五秒协作式扫描超时或结果截断时应缩小目录。取消与超时在文件系统操作之间检查。`glob` 只读，但没有文件系统沙箱。

TODO 区固定在输入框正上方，使用灰色 `├─ / │ / └─` 树形连线，标题左侧显示进度、右侧显示展开／折叠快捷键；窄窗口优先精简状态文字以保留快捷键，树枝显示当前任务和下一项待办，默认共四行。进行中任务为浅蓝加粗、待办正文为浅灰色、阻塞为金色；已完成项使用绿色勾选与绿色划线正文，下一项待办保持清晰可读。展开后阶段与任务通过树枝相连，当前阶段为浅蓝色、已完成阶段为绿色，换行正文与任务文字对齐。macOS 使用 ⌥ + T、其他平台使用 Ctrl + T 原位向上展开，最多占终端高度的三分之一；在清单内使用滚轮，或用同一功能修饰键配合 PgUp/PgDn 翻页。全部完成后保留两行绿色树形摘要，并显示绿色 ✔ 标识；含放弃项时显示 Closed。小窗口退化为单行摘要，空清单不占空间。完整清单仅在固定区展示，对话历史保留操作状态和错误；Ctrl + O 不控制 TODO。恢复和树跳转按最终任务状态重建固定区，并默认收起。

工具预览采用 [Oh My Pi](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0) 的 ANSI 显示规则。普通 read 默认仅显示本地生成的路径／范围摘要，采用深灰紫背景、白色圆点与 Read 标题、灰色计数和树形连线、金黄色路径；分组中的成功项省略 `ok` 后缀；在 `settings.yml` 设置 `read: {toolResultPreview: true}` 后启用三行正文预览。自动加载的 `AGENTS.md` 与实际 skill URI 读取使用独立的深色 Read 边框卡片，状态点位于标题前，Markdown 标题采用金色下划线、强调文字采用金色：默认预览前 10 行，保留展开提示，`Output` 分隔区展示记录的来源路径；普通 read 预览关闭时仍显示，Ctrl + O 展开完整保留正文。未变化的项目指令每会话展示一次，内容变化后追加新卡片；恢复按日志重放原始正文，不为展示重读文件。技能发现摘要不算正文加载卡片。Ctrl + O 全局切换已有及后续工具输出，并通知当前状态。搜索预览按列宽截断长行，shell 输出保留有限的屏幕尾部，各类工具使用独立额度；展开后的 grep 和文件列表仍按列宽截断，MCP 文本仍限制为 12 行。这些显示限制不改变模型接收的结果，会话树预览保持完整，TODO 清单除外。普通对话使用主屏幕，已结束内容随运行逐步提交到原生滚动历史，保留提交时的显示状态；退出只补交未提交内容，不重放整个会话，也不自动展开工具。翻历史、Ctrl + O 和各类面板临时使用备用屏幕，Esc 返回对话。`/new` 成功后清理屏幕与原生滚动历史；恢复会话和树回退按有效历史重建。[各工具预览额度 →](docs/guide.md#工具输出折叠)

配置 `edit: hashline` 后，本地 read 返回 `[path#TAG]` 文件版本头和 `N:text` 行号正文；通过 `edit({"input":"…"})` 提交单文件的 PUT 替换／插入或 CUT 删除。只能编辑已显示行，快照缺失或版本过期时必须重新读取；每次 edit、write 或 undo 成功后该文件快照失效。默认 replace 模式保持不变。


</details>

会话自动保存在 `~/.pp/agent/sessions/`。按完整标识恢复，或打开本地统计页面：

```sh
pp -r SESSION_ID
pp stats
```

`pp stats` 在交互终端自动使用默认浏览器打开统计页面，默认地址为 [127.0.0.1:3848](http://127.0.0.1:3848)；管道或非终端环境（脚本、CI、agent shell）只打印网址，不打开浏览器。使用 `pp stats --no-open` 可仅启动服务。无需模型配置，运行时无需 Node 或 CDN。当前 session 通过 `~/.pp/run/sessions/SESSION_ID.lock` 由一个 pp 实例持续独占，包括等待输入期间。`pp gc [天数]` 可手动清理超过指定天数（默认 90 天）的日志、统计记录及未被占用的 session 锁文件，保留原始会话与配置。

终端界面输入原文按工作目录保存在 `~/.pp/agent/history.db`，重启后可用上方向键取回；其他目录和授权面板的输入不会出现。`/resume` 检索标题、预览、标识、路径以及 JSONL 会话中的全部 user prompt（包括不在当前 leaf 的旧分支），所有词按字面同时命中，短中文词也可直接搜索。会话索引可由日志重建，但输入原文不能：切勿删除 `history.db` 来修复索引，否则会永久丢失可回忆的输入。行模式与 `-p` 的 prompt 可从日志检索，但不会写成终端界面输入历史。详见[会话持久化与恢复](docs/guide.md#会话持久化与恢复)。

## 扩展能力

输入 `/Roles` 配置 DEFAULT、FAST、SLOW、VISION、PLAN、REVIEW 各自的 model 和 effort；`/model` 只切换当前模型。圆点表示启用状态，Space 或点击圆点切换，未选中的 Role 灰显且不会进入 prompt。进入 Plan 默认采用已启用的 PLAN，计划审阅及 reviewer 子任务默认采用已启用的 REVIEW，主 Agent 也可在子任务中显式指定模型 Role。[配置与使用 →](docs/guide.md#模型-roles)

| 扩展 | 入口 | 说明 |
| --- | --- | --- |
| [项目指令](docs/guide.md#项目级-agentsmd) | 当前工作目录的 `AGENTS.md` | 每次模型请求前刷新项目规则 |
| [技能](docs/guide.md#技能支持) | `~/.pp/agent/skills/` | 按需加载 `SKILL.md`，通过 `/skills` 管理 |
| [系统提示词](docs/guide.md#系统提示词) | `cwd/.pp/` 或 `~/.pp/agent/` | `SYSTEM.md`／`SYSTEM_TEMPLATE.md` 替换基础指令，`APPEND_SYSTEM.md` 追加正文 |
| [提示词命令](docs/guide.md#markdown-提示词命令) | `~/.pp/agent/commands/*.md` | 将 Markdown 模板展开为 `/命令 参数` |
| [MCP 工具](docs/guide.md#mcp-工具) | `~/.pp/agent/mcp.json`、`/mcp` | 发现 Claude／Codex 用户级 stdio 服务，查看状态、启停、重连和重载 |

## 开发

测试使用 [cargo-nextest](https://nexte.st/docs/running/)；未安装时先执行 `cargo install cargo-nextest --locked`。
日常修改先运行相关定向测试，全量测试默认并行；`--test-threads=1` 仅用于排查具体失败用例。

```sh
cargo nextest run --locked context_
cargo nextest run --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

定向回归与测试边界见[开发与验证](docs/guide.md#开发与验证)。

---

灵感来自 [Pi](https://github.com/badlogic/pi-mono)，模型接口基于 [async-openai](https://github.com/64bit/async-openai)。

## 子 Agent

使用 `/agents` 查看角色，`/subagents` 查看状态、历史、消耗并追问、取消或恢复任务。支持批量异步派发、独立上下文、两层递归、结构化结果及可选 worktree 隔离，子任务沿用父审批规则。详见[子 Agent 使用说明](docs/subagents.md)。
