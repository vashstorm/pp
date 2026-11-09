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
| **按需扩展** | 项目 `AGENTS.md`、技能、Markdown 提示词命令、本地 stdio 与远程 HTTP／SSE MCP 工具 |
| **用量可见** | 上下文估算与压缩，本地请求、词元、费用和耗时统计 |

前台 Bash 接受可选的 `timeout_seconds` 参数，范围为 0–3600 秒，0 表示不设超时。省略时使用 `bash_timeout_seconds`，默认 300 秒，已有显式配置仍生效。后台模式忽略此参数，沿用现有任务生命周期；不限时时仍可取消。

Bash 合并 stdout/stderr，默认返回最多 50 KiB 的头尾预览。超限或超长行被裁剪时保存为 `artifact://<id>`，模型可用 `read` 行选择器或 `grep` 继续查看；后台运行中的读取按每次调用冻结快照。资源由根会话与子 Agent 共享，恢复后仍可读取，删除会话时清理，`pp gc` 不按天数删除。日志默认上限 16 MiB，并明确报告中间省略和保存失败。配置与完整性边界见 [Bash 输出与 artifact 资源](docs/guide.md#bash-输出与-artifact-资源)。

`wait` 等待选中的后台 Bash 和子 Agent 全部进入终态后才返回；TUI 在同一个等待框持续更新状态、耗时和已结束数量。可通过 `task_ids`／`run_ids` 指定目标，目标集合在调用开始时冻结。固定等待上限为 120 分钟，超时后先终止并回收本批未完成任务及其派生工作，再返回结果。Steering 补充输入排队到批次结束后处理；Esc 取消当前主任务及其自有工作。旧调用中的整数 `timeout_ms` 仅兼容接收，数值被忽略，包括 `0`。

Bash 的模型结果还会在资源可用时附带含具体 `read` 示例的补读提示，或说明保存失败；提示区分完整日志、运行中快照和不完整头尾样本。

## 快速开始

### 1. 构建

需要支持 **2024 版语言规范**的稳定版 Rust 工具链，以及可在命令搜索路径中找到的 `bash`。所有 profile 均使用默认 LLVM 后端，无需 Cranelift 组件或 nightly 特性。

仓库内的 `.cargo/config.toml` 在 macOS ARM64（`aarch64-apple-darwin`）上选择命令搜索路径中的 `ld64.lld`，需安装与当前 macOS SDK 兼容的 LLD；Rust 自带版本可能尚不支持新版 SDK 格式。使用 glibc 的 Linux x86_64（`x86_64-unknown-linux-gnu`）在 Rust 1.90 及以上版本中默认使用自带 LLD。开发、发布构建与测试均生效；macOS 还需 Xcode Command Line Tools，Linux 需 C 工具链。这不提供交叉编译工具链或 SDK。设置 `RUSTFLAGS` 会覆盖配置中的目标参数。

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

创建 `~/.pp/agent/settings.yml`；省略此文件即可使用默认设置。默认模型、思考档位、收藏和七个模型 Role 由终端界面管理（`/model`、`/effort` 菜单的保存快捷键、Ctrl + F、`/Roles`），保存在 `~/.pp/agent/agent.db`，不再写入 YAML；其余设置仍为 `settings.yml` 中的声明式 YAML。

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
# 模型简写：本次运行用模糊匹配的 provider/model[:effort]（同档收藏优先，省略档位时用模型默认档），并启动即执行一条斜杠命令
pp --model deepseek/deepseek-flash:high /commit
```

> **权限与数据**：接口密钥明文保存在模型配置中。读取、命令执行、搜索与 MCP 工具按当前用户权限运行，没有文件系统沙箱。默认 YOLO 模式跳过全部工具审批，包括危险命令检查和显式 prompt/deny 规则；写入与编辑工具的保存层限制在工作目录内并拒绝符号链接，工作区位于 macOS 的 `/tmp`／`/private/tmp` 时两种拼写同样有效。会话与统计可能包含源码和对话，请按敏感数据保管。[查看完整权限说明 →](docs/guide.md#工具执行与权限)

## 日常使用

主聊天 TUI 支持 `@文件`、`@目录/` 和 `@图片路径`。`Ctrl + V` 可粘贴截图或文本，拖入／粘贴纯图片路径也可附图；图片显示为不可拆分的“图标 + #编号”，上方提供缩略图及悬停大图预览，支持整体删除、撤销和持久输入历史召回。加载后的图片使用固定快照；终端不支持图形协议时降级为字符色块。详见[输入指南](docs/guide.md#常用命令与快捷键)。

终端界面进入模型选择后清空主输入框，自动聚焦列表上方的独立搜索框。模型面板作为浮层紧贴主输入框顶部向上展开，覆盖 TODO 等内容而不改变底层布局，关闭后恢复显示。面板打开时按模型总数确定高度，后续筛选保持高度不变；整个面板最多 15 行（搜索框 3 行、列表最多 11 行、提示 1 行），超过后可通过方向键、翻页键或面板内滚轮滚动，并显示滚动条。搜索支持输入即筛选，忽略大小写并匹配 `provider/model` 或实际模型名的任意片段。Ctrl + F 收藏／取消收藏高亮模型，收藏项显示纯金黄色 Nerd Font 实心星标 `` 并始终排在其他模型之前；每组内按使用次数从多到少排列，次数相同时保持模型目录顺序。次数统计自升级以来实际尝试发送的逻辑模型 API 请求：同一请求的建连重试只计一次，打开面板或切换模型不计。收藏与次数保存在 `~/.pp/agent/agent.db`，重启后保留。Ctrl + S 保存默认模型。无匹配结果时显示提示，Enter、Tab 不执行操作。

`/help` 查看完整帮助，`/tips` 查看操作提示。以下斜杠命令在交互模式使用；`-p` 中的内容始终作为普通提示词发送。

主输入框可编辑时，补全始终可用，包括模型输出、上下文压缩、取消收尾和模型切换期间。斜杠命令、已支持的参数和文件引用均可补全，补全不会发送草稿。Esc 收起候选后可用 Tab 重新唤起；要求空闲的命令仍须等任务结束后才能执行。Markdown 解析、语法高亮与正文排版在后台线程完成，样式就绪前可能短暂显示纯文本预览。切换模型保留已提交上下文，不取消当前请求、不重复已完成任务，也不重启后台工作；历史内容无法兼容目标协议时拒绝切换并保留当前模型。

通过 `/Roles` 保存模型与思考档位后，绑定该角色的子 Agent 会在各自的下一次请求前应用最新配置。自定义角色使用派发时选定的模型与 thinking。禁用或清除角色后，正在运行的子 Agent 沿用最后一次应用的模型。角色变更在当前 pp 进程内生效。只改 effort 会保留请求前缀，改 model 会让绑定该角色的会话重新建立前缀缓存。

全部 13 个 `/test-*` 命令在 dev/test 构建中使用内嵌、仅监听 loopback 的 Mock AI Server，由 `#[cfg(debug_assertions)]` 编译隔离，与 `debug` 设置无关。命令通过编译内嵌 JSON 脚本驱动真实 Responses client、Agent、工具和子 Agent，不依赖真实 provider 或用户模型 Role；空 HOME、无 models 或无效配置 YAML 也能运行。每个场景使用全新的系统临时 workspace/config/sessions/locks 目录，不改变正常会话、模型、Role、cwd 或真实统计。预期工具失败继续，取消、初始化或清理失败则中止。

`/test-tools` 顺序运行 read → write → edit-replace → edit-hashline → grep → glob → bash → bash-task → wait → todo → ask，每场 fresh Agent；不串联 `/test-task` 或 `/test-ui`。TUI 在 Execute 模式、无 active/pending 输入时允许 Ready 和 Unavailable 启动；Esc 取消并保留运行中编辑的草稿，结束后恢复正常展示。`/test-ask` 使用真实 TUI 答题，CLI 非交互错误作为预期结果继续。release 不注册这些命令，也不启动 mock listener：行模式静默忽略未知命令，TUI 保留草稿，帮助和补全不列出这些名字。

| 命令 | 用途 |
| --- | --- |
| `/model`（`/models`）、`/effort` | 切换模型与思考档位，保留历史 |
| `/init` | 创建或更新当前项目的 `AGENTS.md` |
| `/plan [prompt]` | 切换 Plan 模式待命，可查看当前计划或附带任务立即发送，批准后执行（仅终端界面） |
| `/plan-list` | 管理已保存计划：预览、继续规划、执行、重命名、删除（仅终端界面） |
| `/test-task` | 仅 dev/test：三个真实 FAST／SLOW／REVIEW 子 Agent 使用独立本地 mock lane，不需要用户 Role 或真实 provider |
| `/test-tools`、`/test-read`、`/test-edit`、`/test-grep`、`/test-glob`、`/test-bash`、`/test-bash-task`、`/test-wait`、`/test-todo`、`/test-ask` | 仅 dev/test：运行全部或单个隔离真实工具场景；edit 覆盖 replace/hashline，ask 在 TUI 中答题 |
| `/test-write` | 仅 dev/test：12.5 mock token/s 的打字机回复；24/28 行正文以 240 mock token/s 写入 `/tmp/pp-test-write/demo.txt`，写入卡片可见尾部滚动；随后越界失败，最后用 bash 工具删除 |
| `/test-ui` | 仅 dev/test：以 12.5 mock token/s 输出原生 Responses SSE 思考与 Markdown／代码分片 |
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
| 换行 | Shift + Enter / Ctrl + J | Shift + Enter / Ctrl + J |
| 模型菜单 | ⌥ + P | Ctrl + P |
| 打开／关闭 Agent Hub | ⌥ + A | Alt + A |
| 收藏／取消收藏高亮模型 | Ctrl + F | Ctrl + F |
| 循环思考档位 | Shift + Tab | Shift + Tab |
| 展开／折叠工具输出 | Ctrl + O | Ctrl + O |
| 展开／收起固定 TODO 区 | ⌥ + T | Ctrl + T |
| 翻页浏览展开的 TODO 区 | ⌥ + PgUp/PgDn | Ctrl + PgUp/PgDn |
| 查看待处理输入 | ⌥ + Q | Ctrl + Q |
| 恢复暂停队列 | ⌥ + R | Ctrl + R |
| 取消当前任务 | Esc | Esc |
| 退出 | Ctrl + C | Ctrl + C |

`@` 打开项目文件路径补全。空闲且输入为空时，双击 Esc 打开会话树；回退历史不会回滚文件。输入框中 `Shift + Enter` 换行，并优先于补全菜单、待处理列表和发送；`Ctrl + J` 在任何位置换行。pp 启动时同时请求 Kitty 键盘协议与 xterm `modifyOtherKeys` mode 2，并把 tmux 默认的 `CSI 27;mod;code~` 编码翻译成 CSI-u；忽略这些请求的终端退化为 `Ctrl + J`。tmux 内需 `set -s extended-keys on`，外层终端 terminfo 未定义 `Eneks`（Ghostty）时再加 `set -as terminal-overrides 'xterm*:Eneks=\E[>1u'`；iTerm2 需保持 *Apps can change how keys are reported* 开启。[完整交互说明 →](docs/guide.md#终端交互)

`read` 仅接受 `path`，支持本地文本、图片、静态网页和已启用的 `skill://` 资源。系统在技能名称、说明和 URI 旁附加选择规则，自定义基础提示词也会保留：按任务及具体子任务主动匹配，在其他任务工具前读取适用技能，成功读取后简短说明用途。执行已批准计划也适用；选择仍由模型完成，正文不自动注入。裸技能 URI 读取完整 `SKILL.md`，追加 `/<path>` 访问资源，`/.` 列出目录。技能文本不受普通文本限额限制，结果带 `path` 和 `base_dir`，显式选择器仍生效。范围直接写入路径：`src/main.rs:20-60`、`notes.md:-30`、`file.rs:5-10,20-30`，严格读取指定行，截断后使用 `next_path` 续读。`:raw` 去掉前缀，网页则返回原始响应正文。PNG／JPEG／WebP／GIF 图片作为真实视觉输入发给模型，工具输出只显示元信息；恢复会话和切换协议保留图片。网页提取静态 HTML 正文，不执行 JavaScript，支持取消和有界会话缓存。[读取行为和限制 →](docs/guide.md#工具执行与权限)

`grep` 在进程内搜索本地文件，支持正则、路径 glob、文件行范围和按文件分页。Execute、Plan 和 Review 共用 `pattern`、`path`、`case`、`-i`、`gitignore`、`skip`、`context`、`-C`、`-A`、`-B`、`output_mode` 参数。`-i: true` 等价于 `case: false`，含义冲突时拒绝调用。`context` 与 `-C` 设置双侧上下文，两者不同值时报错；`-A`／`-B` 分别覆盖后文／前文，支持零行。未指定的侧使用 `settings.yml` 中的配置，默认 `grep: {contextBefore: 1, contextAfter: 3}`。

`output_mode` 默认 `content`，返回正文；`files_with_matches` 只返回路径；`count` 返回各文件计数与所有已搜索文件的总计。单行模式按命中行计数，多行模式按不重叠的匹配计数。上下文参数仅影响 `content`。所有模式保留 `skip` 按文件分页；计数总计包含当前页之外的文件，不受正文匹配条数上限限制，但仍遵守扫描范围限制，`details.countComplete: false` 表示统计不完整。仅 `content` 创建或更新 Hashline 编辑快照，路径与计数模式保留已有快照且不新增已读行。

`glob` 在 Execute、Plan 和 Review 中查找本地路径，接受必填匹配表达式 `pattern`、可选搜索目录 `path`（默认当前工作目录），以及 `hidden`、`gitignore` 和 `limit`；例如 `glob({"pattern":"**/*.rs","path":"src"})`。`path` 仅接受单个字面目录，不再承载 glob 或多个搜索范围；默认包含隐藏文件并遵守忽略规则。裸 `*.rs` 递归查找，`src/*.rs` 只查找直接子项。结果按修改时间排序，最多返回 200 个路径和 50 KiB；五秒协作式扫描超时或结果截断时应缩小目录。取消与超时在文件系统操作之间检查。`glob` 只读，但没有文件系统沙箱。

TODO 区固定在输入框正上方，使用灰色树形连线，不再绘制底部封口线；标题显示进度与展开／收起快捷键，窄窗口优先精简状态文字以保留快捷键。收起时保留可见任务的阶段行，空间允许便显示连续三项，进行项尽量居中，清单首尾用相邻项补齐；不足三项时全部显示。待办为灰色，进行中为亮黄色加粗，完成项保持绿色、仅勾选而不划线；选择框加宽为三列 `[ ]`／`[✓]`，阻塞项使用金色 `[!]`。展开后阶段与任务通过树枝相连，进行中阶段为亮黄色，已完成阶段为绿色，换行正文与任务文字对齐。macOS 使用 ⌥ + T、其他平台使用 Ctrl + T 原位向上展开，最多占终端高度的三分之一；展开和状态更新时尽量将当前任务滚动至中部，在清单内使用滚轮或同一功能修饰键配合 PgUp/PgDn 翻页。全部完成后保留用户选择的展开状态，收起时仍显示最后三项及绿色 ✔ Complete 标题；含放弃项时显示 Closed。小窗口按可用空间显示任务，空清单不占空间。完整清单仅在固定区展示，对话历史保留操作状态和错误；Ctrl + O 不控制 TODO。恢复和树跳转按最终任务状态重建固定区，并默认收起。

工具预览采用 [Oh My Pi](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0) 的 ANSI 显示规则。普通 read 默认仅显示本地生成的路径／范围摘要，采用深灰紫背景、白色圆点与 Read 标题、灰色计数和树形连线、金黄色路径；分组中的成功项省略 `ok` 后缀；在 `settings.yml` 设置 `read: {toolResultPreview: true}` 后启用三行正文预览。自动加载的 `AGENTS.md` 与实际 skill URI 读取使用独立的深色 Read 边框卡片，状态点位于标题前，Markdown 标题采用金色下划线、强调文字采用金色：默认预览前 10 行，保留展开提示，`Output` 分隔区展示记录的来源路径；普通 read 预览关闭时仍显示，Ctrl + O 展开完整保留正文。未变化的项目指令每会话展示一次，内容变化后追加新卡片；恢复按日志重放原始正文，不为展示重读文件。技能发现摘要不算正文加载卡片。Ctrl + O 全局切换已有及后续工具输出，并通知当前状态。搜索预览按列宽截断长行，shell 输出保留有限的屏幕尾部，各类工具使用独立额度；展开后的 grep 和文件列表仍按列宽截断，MCP 文本仍限制为 12 行。这些显示限制不改变模型接收的结果，会话树预览保持完整，TODO 清单除外。普通对话使用主屏幕，已结束内容随运行逐步提交到原生滚动历史，保留提交时的显示状态；退出只补交未提交内容，不重放整个会话，也不自动展开工具。翻历史、Ctrl + O 和各类面板临时使用备用屏幕，Esc 返回对话。`/new` 成功后清理屏幕与原生滚动历史；恢复会话和树回退按有效历史重建。[各工具预览额度 →](docs/guide.md#工具输出折叠)

配置 `edit: hashline` 后，本地 read 返回 `[path#TAG]` 文件版本头和 `N:text` 行号正文；通过 `edit({"input":"…"})` 提交单文件的 PUT 替换／插入或 CUT 删除。只能编辑已显示行，快照缺失或版本过期时必须重新读取；每次 edit、write 或 undo 成功后该文件快照失效。默认 replace 模式保持不变。


</details>

会话自动保存在 `~/.pp/agent/sessions/`。`pp -r` 在终端界面直接打开会话选择面板；传入完整标识则跳过面板直接恢复，或打开本地统计页面：

```sh
pp -r [SESSION_ID]
pp stats
```

`pp stats` 在交互终端自动使用默认浏览器打开统计页面，默认地址为 [127.0.0.1:3848](http://127.0.0.1:3848)；管道或非终端环境（脚本、CI、agent shell）只打印网址，不打开浏览器。使用 `pp stats --no-open` 可仅启动服务。需要局域网访问时，通过 `pp stats --host 192.168.1.20` 显式绑定本机局域网地址，并将示例地址替换为实际地址。服务没有认证，能够访问该地址的用户均可读取统计与请求详情，请仅在可信网络中使用。无需模型配置，运行时无需 Node 或 CDN。当前 session 通过 `~/.pp/run/sessions/SESSION_ID.lock` 由一个 pp 实例持续独占，包括等待输入期间。`pp gc [天数]` 可手动清理超过指定天数（默认 90 天）的日志、统计记录及未被占用的 session 锁文件，保留原始会话与配置。

`/resume` 采用单行紧凑列表，显示相对时间与固定配色标题，使用交替底色和整行蓝色选中。Ctrl + P 置顶或取消置顶所选会话；当前搜索和目录范围内，置顶项始终在前，两组内部均按最近更新时间排序。置顶状态保存在 `history.db`，重启或重建会话索引后保留。

终端界面输入原文按工作目录保存在 `~/.pp/agent/history.db`，重启后可用上方向键取回；其他目录和授权面板的输入不会出现。`/resume` 检索标题、预览、标识、路径以及 JSONL 会话中的全部 user prompt（包括不在当前 leaf 的旧分支），所有词按字面同时命中，短中文词也可直接搜索。会话索引可由日志重建，但输入原文不能：切勿删除 `history.db` 来修复索引，否则会永久丢失可回忆的输入和会话置顶状态。行模式与 `-p` 的 prompt 可从日志检索，但不会写成终端界面输入历史。详见[会话持久化与恢复](docs/guide.md#会话持久化与恢复)。

## 扩展能力

输入 `/Roles` 配置模型和 effort。主 Agent 用 DEFAULT 和 PLAN 分别选择启动模型和规划模型，计划审阅采用已启用的 REVIEW。派发子 Agent 时，每个任务都必须显式传入 `role`，可选已启用的 `fast`、`slow`、`vision`、`review`、`explorer` 或已加载的自定义名称。旧调用中的 `agent` 改为 `role`。缺失、未知或停用的角色会使整批任务在启动前被拒绝。`role: "review"` 用于只读审查，`role: "explorer"` 用于只读快速探索。[配置与使用 →](docs/subagents.md)

每个角色可配置一个 fallback，并独立设置 thinking。在 `/Roles` 中用 ↑/↓ 选择主模型或备用行，Enter 选模，`f` 设置备用，`x` 清除当前行，`t`（兼容 `e`）调整 thinking。清空主模型会禁用角色并保留 fallback；补齐后仍需手动启用。Delete／Backspace 删除整个角色配置。

模型错误（含流中断）从本轮请求的安全消息边界重试，不重复已完成的工具。每个模型最多发送 10 次，同时受 `rate_limit_retry_budget_seconds` 等待预算限制；fallback 独立获得相同额度。预算为 0 时不重试，仍允许发送一次备用请求。取消、本地配置、持久化和清理错误直接停止。备用模型沿用至本任务结束，下一独立任务恢复主模型；手动 `/model` 解除角色 fallback 绑定。失败输出保留可见标记，但不进入有效模型历史。已有角色配置通过事务迁移至 `agent.db` schema v4。


| 扩展 | 入口 | 说明 |
| --- | --- | --- |
| [项目指令](docs/guide.md#项目级-agentsmd) | 当前工作目录的 `AGENTS.md` | 每次模型请求前刷新项目规则 |
| [技能](docs/guide.md#技能支持) | `~/.pp/agent/skills/` | 按需加载 `SKILL.md`，通过 `/skills` 管理 |
| [系统提示词](docs/guide.md#系统提示词) | `cwd/.pp/` 或 `~/.pp/agent/` | `SYSTEM.md`／`SYSTEM_TEMPLATE.md` 替换基础指令，`APPEND_SYSTEM.md` 追加正文 |
| [提示词命令](docs/guide.md#markdown-提示词命令) | `~/.pp/agent/commands/*.md` | 将 Markdown 模板展开为 `/命令 参数` |
| [MCP 工具](docs/guide.md#mcp-工具) | `~/.pp/agent/mcp.json`、`/mcp` | 连接 stdio 与远程 HTTP／SSE 服务，支持 Context7 和 [Exa 搜索与网页读取](docs/guide.md#接入-exa)，可为 Plan、Review 和只读子 Agent 授权特定 MCP 工具并管理连接 |
| [语言服务器](docs/guide.md#lsp-语义导航与修改建议) | `~/.pp/agent/lsp.json`、`settings.yml` 的 `lsp:` | 语义导航、诊断和修改建议；AI 重新读取文件后通过 edit/write/bash 执行 |

## 开发

仓库是按功能拆分的 Cargo workspace，共七个包。根包 `pp` 只保留 `src/main.rs`（启动参数、日志初始化、工作目录恢复、模式分发与退出码）和根 `tests/`，并且是唯一的默认成员；业务实现由以下内部库承载：

| 包 | 职责 |
| --- | --- |
| `pp-core` | 共享契约：取消、上下文、事件、Plan、交互队列、日志、SQLite 打开，以及 Todo／LSP 报告类型 |
| `pp-config` | 设置与模型 YAML、模型目录、认证及认证 HTTP、agent 数据库与技能 |
| `pp-stats` | 请求快照、聚合，以及内嵌资源的只读统计服务 |
| `pp-llm` | 聊天补全与响应接口请求、重试、流式、解析和共享 HTTP 返回类型 |
| `pp-runtime` | Agent、工具、子 Agent、MCP／LSP、会话与历史、模型控制和提示词 |
| `pp-ui` | TUI、CLI、命令、快捷键与 tips |
| `pp`（根包） | 启动组装与根 `cli_agent` 集成测试 |

`cargo run`、`cargo build` 和不带包选择的测试只作用于根默认成员；跨全部 crate 验证须使用 `--workspace`，只验证单个库时使用 `-p <包名>`。

测试使用 [cargo-nextest](https://nexte.st/docs/running/)；未安装时先执行 `cargo install cargo-nextest --locked`。
日常修改先运行相关定向测试，全量测试默认并行；`--test-threads=1` 仅用于排查具体失败用例。

```sh
cargo nextest run --locked --workspace context_
cargo nextest run --locked --workspace
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --doc
cargo fmt --all --check
```

定向回归与测试边界见[开发与验证](docs/guide.md#开发与验证)。

---

灵感来自 [Pi](https://github.com/badlogic/pi-mono)，模型接口基于 [async-openai](https://github.com/64bit/async-openai)。

## 子 Agent

有子 Agent 正在运行时，终端界面会在 `tok/s` 前以金色图标显示实际数量。一个用单人图标，两个用双人图标，三个及以上用三人图标；没有正在运行的子 Agent 时隐藏。

使用 `/Roles` 配置模型角色，`/agents` 和 `/subagents`（也可用 `⌥ + A / Alt + A`）都能打开 Agent Hub，查看 Agents、完整 Activity、消耗与独立详情，并追问、取消或恢复任务。支持 Main 批量异步派发、独立上下文、结构化结果及可选 worktree 隔离，子任务沿用父审批规则。运行中子 Agent 数量上限默认 4，通过 `~/.pp/agent/settings.yml` 的 `task.max_concurrency` 配置为 1–32，重启生效；每个 Run 在初始化前获取名额，跨模型请求、工具和等待持续持有，结果收尾结束才释放，未获得名额时保持排队；Main 不占名额。所有 child 均无 `task` 工具和自动委托使用提示，不能递归派发。详见[子 Agent 使用说明](docs/subagents.md)。
