<h1 align="center">✈ pp</h1>

<p align="center"><strong>English</strong> | <a href="README.zh-CN.md">Simplified Chinese</a></p>

<p align="center"><strong>paper plane · Your coding agent in the terminal</strong></p>
<p align="center">Rust · macOS / Linux · OpenAI-compatible Responses / Chat Completions</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#everyday-use">Everyday use</a> ·
  <a href="#extensions">Extensions</a> ·
  <a href="docs/guide.md">Full guide (Chinese)</a>
</p>

---

pp is a Pi-style terminal coding agent written in Rust. It lets models read projects, edit files, search code, and run commands across multiple turns, directly in your terminal.

## Features

| Feature | Description |
| --- | --- |
| **Terminal interaction** | Streaming text, reasoning, and tool output in a TUI, one-shot tasks, or piped input |
| **Planning and execution** | Plan research, review, and explicit approval, with an ask panel for key decisions |
| **Task control** | Add instructions while a task runs, queue follow-up tasks, and press Esc to cancel |
| **Sessions and changes** | Automatic saving and restoration, session tree navigation, and recent-task diffs with safe undo |
| **Extensions on demand** | Project `AGENTS.md`, skills, Markdown prompt commands, and local stdio MCP tools |
| **Usage visibility** | Context estimates and compaction, plus local request, token, cost, and latency statistics |

## Quick start

### 1. Build

You need a Rust toolchain that supports **Edition 2024**, and `bash` on your PATH.

```sh
cargo build --release --locked
mkdir -p ~/.pp/agent
```

Install `rg` and `fd` for faster searches; pp falls back to the system `grep` and `find` when they are unavailable. TUI icons require a Nerd Font. On macOS, enable Alt/Meta in your terminal to use Option shortcuts.

### 2. Configure a model

Create `~/.pp/agent/models.yml` and replace the API key and actual model ID:

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

- `protocol`: `response` uses the Responses API (default); `completion` uses the Chat Completions API.
- `base_url`: the provider's API root. pp appends the endpoint for the selected protocol and does not add `/v1` automatically.
- Multiple providers and models are supported. Without a configured default, pp uses the first model in the file.

<details>
<summary><strong>Optional: application settings, multiple models, and reasoning levels</strong></summary>

Create `~/.pp/agent/settings.yml`, or omit it to use the defaults.

```yaml
default_model: openai/main
favorite_models: []
think_level: none
edit: replace
max_model_requests: 20
auto_compact: true
auto_compact_threshold_percent: 80
```

Automatic compaction is enabled only when the model has an explicit `context_window`. Reasoning levels must match the provider's capabilities. Use `thinking.efforts` and `thinking.defaultLevel` in the model configuration to restrict available levels and set a default.

See the [configuration guide (Chinese)](docs/guide.md#配置) for full examples and validation rules. Configuration is loaded only from `~/.pp/agent/`; project files with the same names and provider environment variables are not read.

</details>

### 3. Run

```sh
chmod 600 ~/.pp/agent/models.yml
./target/release/pp
```

pp enters the TUI when both stdin and stdout are terminals; otherwise, it uses line mode. Add the binary to your PATH to use `pp` as shown below.

```sh
pp -p 'Read the current project and explain its structure without changing files.'
printf 'Explain how to build the current project\n' | pp
```

> **Permissions and data**: API keys are stored in plain text in the model configuration. Read, shell, search, and MCP tools run with your user permissions, without a filesystem sandbox or per-action confirmation. The write/edit save layer restricts paths to the working directory and rejects symbolic links. Sessions and statistics may contain source code and conversations; treat them as sensitive data. [Full permissions guide (Chinese) →](docs/guide.md#工具执行与权限)

## Everyday use

Opening the model picker in the TUI clears the main input and focuses a separate search field above the list. The panel height is set from the total model count when opened and stays fixed while filtering. The panel uses at most 15 rows: 3 for search, up to 11 for the list, and 1 for hints. Longer lists show a scrollbar and support arrow keys, page keys, and mouse-wheel scrolling inside the panel. Search filters as you type, matching any part of `provider/model` or the actual model name, case-insensitively. Ctrl + F toggles the highlighted model's favorite status. Favorites show a solid gold Nerd Font star `` and appear first, with configuration order preserved within each group. Favorites are saved in the global `settings.yml` under `favorite_models` and persist across restarts. Ctrl + S saves the default model. When no models match, the panel shows a message, and Enter and Tab do nothing.

Use `/help` for full help and `/tips` for usage tips. The following slash commands work in interactive modes; content passed through `-p` is always sent as a regular prompt.

| Command | Purpose |
| --- | --- |
| `/model`, `/effort` | Switch models and reasoning levels while keeping history |
| `/init` | Create or update the current project's `AGENTS.md` |
| `/plan` | Research and review a plan, then execute after approval (TUI only) |
| `/compact`, `/new` | Compact context or start a new session |
| `/resume`, `/tree` | Restore a session or navigate back through current history (TUI only) |
| `/changes` | View recent-task edit/write changes and undo individual files (TUI only) |
| `/skills`, `/quit` | Manage skills or exit |

<details>
<summary><strong>Common TUI shortcuts</strong></summary>

| Action | macOS | Linux |
| --- | --- | --- |
| Send / add instructions to the running task | Enter | Enter |
| Queue an independent follow-up task | ⌥ + Enter | Alt + Enter |
| Insert a newline | Ctrl + J | Ctrl + J |
| Open the model menu | ⌥ + P | Ctrl + P |
| Toggle favorite for the highlighted model | Ctrl + F | Ctrl + F |
| Cycle reasoning levels | Shift + Tab | Shift + Tab |
| Expand / collapse tool output | Ctrl + O | Ctrl + O |
| View pending input | ⌥ + Q | Ctrl + Q |
| Resume the paused queue | ⌥ + R | Ctrl + R |
| Cancel the current task | Esc | Esc |
| Exit | Ctrl + C | Ctrl + C |

Type `@` for project file path completion. When idle with an empty input, press Esc twice to open the session tree. Navigating back through history does not roll back files. [Full interaction guide (Chinese) →](docs/guide.md#终端交互)

Tool previews follow the ANSI policies of [Oh My Pi](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0). File reads show local path/range summaries by default; set `read: {toolResultPreview: true}` in `settings.yml` to enable three-line content previews. Ctrl + O toggles existing and future tool output and reports the state. Search previews clip long rows, shell output keeps a bounded visual tail, and other tools have separate limits. Expanded grep and file lists remain width-clipped; MCP text still has a 12-line limit. These display limits do not change model-facing results. Session-tree previews and terminal history written on exit include the complete retained output. [Preview limits (Chinese) →](docs/guide.md#工具输出折叠)

</details>

Sessions are saved automatically in `~/.pp/agent/sessions/`. Restore one by its full ID, or open the local statistics page:

```sh
pp -r SESSION_ID
pp stats
```

Statistics are served at [127.0.0.1:3848](http://127.0.0.1:3848) by default, without model configuration, Node, or a CDN at runtime. Run `pp gc` to manually remove logs and statistics older than 90 days; original sessions and configuration are kept.

## Extensions

The linked guides below are in Chinese.

| Extension | Location | Description |
| --- | --- | --- |
| [Project instructions](docs/guide.md#项目级-agentsmd) | `AGENTS.md` in the working directory | Refresh project rules before each model request |
| [Skills](docs/guide.md#技能支持) | `~/.pp/agent/skills/` | Load `SKILL.md` on demand and manage skills with `/skills` |
| [Prompt commands](docs/guide.md#markdown-提示词命令) | `~/.pp/agent/commands/*.md` | Expand Markdown templates with `/command arguments` |
| [MCP tools](docs/guide.md#mcp-工具) | `mcp_servers` in `settings.yml` | Connect to local stdio servers and discover tools |

## Development

Tests use [cargo-nextest](https://nexte.st/docs/running/). Install it with `cargo install cargo-nextest --locked` if needed.
For local changes, select the relevant tests first; the full suite runs in parallel by default. Use `--test-threads=1` only to investigate specific failures.

```sh
cargo nextest run --locked context_
cargo nextest run --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

See [development and validation (Chinese)](docs/guide.md#开发与验证) for targeted regression commands and test coverage limits.

---

Inspired by [Pi](https://github.com/badlogic/pi-mono), with model API support provided by [async-openai](https://github.com/64bit/async-openai).
