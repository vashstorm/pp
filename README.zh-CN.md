<h1 align="center">✈ pp</h1>

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

## 功能一览

| 功能 | 说明 |
| --- | --- |
| **终端交互** | 流式正文、思考与工具输出，支持终端界面、单次任务和管道输入 |
| **规划与执行** | 计划调研、审阅与显式批准，通过询问面板收集关键选择 |
| **任务控制** | 运行中补充需求、排队后续任务，Esc 取消当前任务 |
| **会话与变更** | 自动保存与恢复、会话树回退、最近任务差异与安全撤销 |
| **按需扩展** | 项目 `AGENTS.md`、技能、Markdown 提示词命令、基于本地标准输入输出的 MCP 工具 |
| **用量可见** | 上下文估算与压缩，本地请求、词元、费用和耗时统计 |

## 快速开始

### 1. 构建

需要支持 **2024 版语言规范**的 Rust 工具链，以及可在命令搜索路径中找到的 `bash`。

```sh
cargo build --release --locked
mkdir -p ~/.pp/agent
```

搜索推荐安装 `rg` 和 `fd`，缺失时回退到系统 `grep` 和 `find`。终端界面图标需要 Nerd Font 字体；macOS 使用 Option 快捷键时，终端需启用 Alt/Meta。

### 2. 配置模型

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

创建 `~/.pp/agent/settings.yml`；省略此文件即可使用默认设置。

```yaml
default_model: openai/main
think_level: none
edit: replace
max_model_requests: 20
auto_compact: true
auto_compact_threshold_percent: 80
```

自动压缩仅在模型显式配置 `context_window` 时启用。思考档位须与供应商实际能力匹配，可在模型配置中使用 `thinking.efforts` 和 `thinking.defaultLevel` 限定可选档位与默认值。

完整示例与校验规则见[配置指南](docs/guide.md#配置)。模型与应用设置只从 `~/.pp/agent/` 加载，不读取项目同名文件或供应商环境变量；[系统提示词](docs/guide.md#系统提示词)也可从启动目录的 `.pp/` 加载。

</details>

### 3. 启动

```sh
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

终端界面进入模型选择后清空主输入框，自动聚焦列表上方的独立搜索框。面板打开时按模型总数确定高度，后续筛选保持高度不变；整个面板最多 15 行（搜索框 3 行、列表最多 11 行、提示 1 行），超过后可通过方向键、翻页键或面板内滚轮滚动，并显示滚动条。搜索支持输入即筛选，忽略大小写并匹配 `provider/model` 或实际模型名的任意片段。Ctrl + F 收藏／取消收藏高亮模型，收藏项显示纯金黄色 Nerd Font 实心星标 `` 并优先排列，同组保持配置顺序；收藏保存到全局 `settings.yml` 的 `favorite_models` 列表，重启后保留。Ctrl + S 保存默认模型。无匹配结果时显示提示，Enter、Tab 不执行操作。

`/help` 查看完整帮助，`/tips` 查看操作提示。以下斜杠命令在交互模式使用；`-p` 中的内容始终作为普通提示词发送。

| 命令 | 用途 |
| --- | --- |
| `/model`、`/effort` | 切换模型与思考档位，保留历史 |
| `/init` | 创建或更新当前项目的 `AGENTS.md` |
| `/plan` | 调研、审阅计划，批准后执行（仅终端界面） |
| `/compact`、`/new` | 压缩上下文，或开始新会话 |
| `/resume`、`/tree` | 恢复会话，或回退当前历史（仅终端界面） |
| `/changes` | 查看最近任务的编辑与写入变更并逐文件撤销（仅终端界面） |
| `/skills`、`/quit` | 管理技能，或退出 |

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
| 查看待处理输入 | ⌥ + Q | Ctrl + Q |
| 恢复暂停队列 | ⌥ + R | Ctrl + R |
| 取消当前任务 | Esc | Esc |
| 退出 | Ctrl + C | Ctrl + C |

`@` 打开项目文件路径补全。空闲且输入为空时，双击 Esc 打开会话树；回退历史不会回滚文件。[完整交互说明 →](docs/guide.md#终端交互)

`read` 仅接受 `path`，支持本地文本、图片、静态网页和已启用的 `skill://` 资源。系统列出技能名称、说明和 URI，匹配技能必须先读取，不自动注入正文；裸技能 URI 读取完整 `SKILL.md`，追加 `/<path>` 访问资源，`/.` 列出目录。技能文本不受普通文本限额限制，结果带 `path` 和 `base_dir`，显式选择器仍生效。范围直接写入路径：`src/main.rs:20-60`、`notes.md:-30`、`file.rs:5-10,20-30`，严格读取指定行，截断后使用 `next_path` 续读。`:raw` 去掉前缀，网页则返回原始响应正文。PNG／JPEG／WebP／GIF 图片作为真实视觉输入发给模型，终端只显示元信息；恢复会话和切换协议保留图片。网页提取静态 HTML 正文，不执行 JavaScript，支持取消和有界会话缓存。[读取行为和限制 →](docs/guide.md#工具执行与权限)

工具预览采用 [Oh My Pi](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0) 的 ANSI 显示规则。普通 read 默认仅显示本地生成的路径／范围摘要；在 `settings.yml` 设置 `read: {toolResultPreview: true}` 后启用三行正文预览。skill URI 始终默认显示三行正文预览，Ctrl + O 展开。Ctrl + O 全局切换已有及后续工具输出，并通知当前状态。搜索预览按列宽截断长行，shell 输出保留有限的屏幕尾部，各类工具使用独立额度；展开后的 grep 和文件列表仍按列宽截断，MCP 文本仍限制为 12 行。这些显示限制不改变模型接收的结果，会话树预览与退出时写入终端历史的内容保持完整。[各工具预览额度 →](docs/guide.md#工具输出折叠)

配置 `edit: hashline` 后，本地 read 返回 `[path#TAG]` 文件版本头和 `N:text` 行号正文；通过 `edit({"input":"…"})` 提交单文件的 PUT 替换／插入或 CUT 删除。只能编辑已显示行，快照缺失或版本过期时必须重新读取；每次 edit、write 或 undo 成功后该文件快照失效。默认 replace 模式保持不变。


</details>

会话自动保存在 `~/.pp/agent/sessions/`。按完整标识恢复，或打开本地统计页面：

```sh
pp -r SESSION_ID
pp stats
```

统计默认地址为 [127.0.0.1:3848](http://127.0.0.1:3848)，无需模型配置，运行时无需 Node 或 CDN。`pp gc` 可手动清理超过 90 天的日志和统计记录，保留原始会话与配置。

## 扩展能力

| 扩展 | 入口 | 说明 |
| --- | --- | --- |
| [项目指令](docs/guide.md#项目级-agentsmd) | 当前工作目录的 `AGENTS.md` | 每次模型请求前刷新项目规则 |
| [技能](docs/guide.md#技能支持) | `~/.pp/agent/skills/` | 按需加载 `SKILL.md`，通过 `/skills` 管理 |
| [系统提示词](docs/guide.md#系统提示词) | `cwd/.pp/` 或 `~/.pp/agent/` | `SYSTEM.md`／`SYSTEM_TEMPLATE.md` 替换基础指令，`APPEND_SYSTEM.md` 追加正文 |
| [提示词命令](docs/guide.md#markdown-提示词命令) | `~/.pp/agent/commands/*.md` | 将 Markdown 模板展开为 `/命令 参数` |
| [MCP 工具](docs/guide.md#mcp-工具) | `settings.yml` 的 `mcp_servers` | 连接基于本地标准输入输出的服务并发现工具 |

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
