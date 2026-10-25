<h1 align="center">✈ pp</h1>

<p align="center"><a href="README.md">English</a> | <strong>简体中文</strong></p>

<p align="center"><strong>paper plane · 终端里的编程助手</strong></p>
<p align="center">Rust · macOS / Linux · OpenAI-compatible Responses / Chat Completions</p>

<p align="center">
  <a href="#快速开始">快速开始</a> ·
  <a href="#日常使用">日常使用</a> ·
  <a href="#扩展能力">扩展能力</a> ·
  <a href="docs/guide.md">完整指南</a>
</p>

---

pp 是用 Rust 编写的 Pi 风格终端 coding agent。让模型直接阅读项目、编辑文件、搜索代码和执行命令，在终端中完成多轮开发任务。

## 功能一览

| 功能 | 说明 |
| --- | --- |
| **终端交互** | 流式正文、思考与工具输出，支持 TUI、单次任务和管道输入 |
| **规划与执行** | Plan 调研、计划审阅与显式批准，ask 面板收集关键选择 |
| **任务控制** | 运行中补充需求、排队后续任务，Esc 取消当前任务 |
| **会话与变更** | 自动保存与恢复、会话树回退、最近任务 diff 与安全撤销 |
| **按需扩展** | 项目 `AGENTS.md`、技能、Markdown 提示词命令、本地 stdio MCP |
| **用量可见** | 上下文估算与压缩，本地请求、Token、费用和耗时统计 |

## 快速开始

### 1. 构建

需要支持 **Edition 2024** 的 Rust 工具链，以及 PATH 中的 `bash`。

```sh
cargo build --release --locked
mkdir -p ~/.pp/agent
```

搜索推荐安装 `rg` 和 `fd`，缺失时回退到系统 `grep` 和 `find`。TUI 图标需要 Nerd Font；macOS 使用 Option 快捷键时，终端需启用 Alt/Meta。

### 2. 配置模型

创建 `~/.pp/agent/models.yml`，替换密钥和实际模型 ID：

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

- `protocol`：`response` 使用 Responses API（默认）；`completion` 使用 Chat Completions API。
- `base_url`：填写供应商的 API 根地址，程序按协议追加端点，不自动补 `/v1`。
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

完整示例与校验规则见[配置指南](docs/guide.md#配置)。配置只从 `~/.pp/agent/` 加载，不读取项目同名文件或供应商环境变量。

</details>

### 3. 启动

```sh
chmod 600 ~/.pp/agent/models.yml
./target/release/pp
```

标准输入和输出均为终端时自动进入 TUI，否则使用行模式。将构建产物放入 PATH 后，可直接使用下文的 `pp`。

```sh
pp -p '阅读当前项目并说明结构，不修改文件。'
printf '说明当前项目的构建方式\n' | pp
```

> **权限与数据**：API 密钥明文保存在模型配置中。read、shell、搜索与 MCP 按当前用户权限运行，没有文件系统沙箱或逐次操作确认；write/edit 的保存层限制在工作目录内并拒绝符号链接。会话与统计可能包含源码和对话，请按敏感数据保管。[查看完整权限说明 →](docs/guide.md#工具执行与权限)

## 日常使用

`/help` 查看完整帮助，`/tips` 查看操作提示。以下 slash 命令在交互模式使用；`-p` 中的内容始终作为普通提示词发送。

| 命令 | 用途 |
| --- | --- |
| `/model`、`/effort` | 切换模型与思考档位，保留历史 |
| `/init` | 创建或更新当前项目的 `AGENTS.md` |
| `/plan` | 调研、审阅计划，批准后执行（仅 TUI） |
| `/compact`、`/new` | 压缩上下文，或开始新会话 |
| `/resume`、`/tree` | 恢复会话，或回退当前历史（仅 TUI） |
| `/changes` | 查看最近任务的 edit/write 变更并逐文件撤销（仅 TUI） |
| `/skills`、`/quit` | 管理技能，或退出 |

<details>
<summary><strong>TUI 常用快捷键</strong></summary>

| 操作 | macOS | Linux |
| --- | --- | --- |
| 发送／运行中补充需求 | Enter | Enter |
| 排队为独立后续任务 | ⌥ + Enter | Alt + Enter |
| 换行 | Ctrl + J | Ctrl + J |
| 模型菜单 | ⌥ + P | Ctrl + P |
| 循环思考档位 | Shift + Tab | Shift + Tab |
| 展开／折叠工具输出 | Ctrl + O | Ctrl + O |
| 查看待处理输入 | ⌥ + Q | Ctrl + Q |
| 恢复暂停队列 | ⌥ + R | Ctrl + R |
| 取消当前任务 | Esc | Esc |
| 退出 | Ctrl + C | Ctrl + C |

`@` 打开项目文件路径补全。空闲且输入为空时，双击 Esc 打开会话树；回退历史不会回滚文件。[完整交互说明 →](docs/guide.md#终端交互)

</details>

会话自动保存在 `~/.pp/agent/sessions/`。按完整 ID 恢复，或打开本地统计页面：

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
| [提示词命令](docs/guide.md#markdown-提示词命令) | `~/.pp/agent/commands/*.md` | 将 Markdown 模板展开为 `/命令 参数` |
| [MCP 工具](docs/guide.md#mcp-工具) | `settings.yml` 的 `mcp_servers` | 连接本地 stdio 服务并发现工具 |

## 开发

测试使用 [cargo-nextest](https://nexte.st/docs/running/)；未安装时先执行 `cargo install cargo-nextest --locked`。

```sh
cargo nextest run --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

定向回归与测试边界见[开发与验证](docs/guide.md#开发与验证)。

---

灵感来自 [Pi](https://github.com/badlogic/pi-mono)，模型接口基于 [async-openai](https://github.com/64bit/async-openai)。
