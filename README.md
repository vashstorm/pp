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
| **Planning and execution** | Plan research, review, and explicit approval, with an ask panel for key decisions; `/plan-list` manages saved plans across drafts, plans, and done |
| **Todo tracking** | Fixed progress above the input box, expandable task lists during execution, approved-plan checkbox synchronization, and bounded completion reminders |
| **Task control** | Add instructions while a task runs, queue follow-up tasks, and press Esc to cancel |
| **Sessions and changes** | Automatic saving and restoration, session tree navigation, persistent per-directory input recall, full-prompt `/resume` search, and recent-task diffs with safe undo |
| **Extensions on demand** | Project `AGENTS.md`, skills, Markdown prompt commands, and local stdio MCP tools |
| **Usage visibility** | Context estimates and compaction, plus local request, token, cost, and latency statistics |

## Quick start

### 1. Build

You need a Rust toolchain that supports **Edition 2024**, and `bash` on your PATH.

```sh
cargo build --release --locked
mkdir -p ~/.pp/agent
```

`grep` and `glob` run in process without external search programs. TUI icons require a Nerd Font. On macOS, enable Alt/Meta in your terminal to use Option shortcuts.

### 2. Configure a model

Start the terminal UI even without model configuration, then use `/login` to choose ChatGPT, GitHub Copilot, Kimi Code, GLM, Z.AI, or Command Code:

```sh
./target/release/pp
```

`/login` opens the authorization page in your browser. ChatGPT, Copilot, and Kimi use device authorization; GLM, Z.AI, and Command Code use masked API key input. Credentials are stored in plain text in `~/.pp/agent/agent.db`, with directory mode `0700` and database file mode `0600`; no system keychain or encryption is provided. Discovered subscription models remain in `~/.pp/agent/models-sub.yml`, which keeps its existing format. Discovery failures keep the authorization and offer a retry without adding a guessed default model. Quit every running pp process before upgrading: on first start pp imports the old preference fields in `settings.yml` and `auth.yml` into `agent.db` exactly once, then removes the authorization and those fields from YAML.

Use `/model` to select a model immediately. It reloads both model files; `models.yml` takes priority for identical `provider/model` references. `/logout` removes a provider's credentials from `agent.db` and its subscription models. These authorization commands require the TUI; the old `pp auth` command and `auth.json` are no longer used. Kimi and GLM currently use non-thinking mode. See [subscription authentication](docs/guide.md#订阅认证) for details.

For a regular API key, use the configuration below.

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

Create `~/.pp/agent/settings.yml`, or omit it to use the defaults. The default model, reasoning effort, favorites, and the six model roles are managed in the TUI (`/model`, `/effort`, the menus' save shortcuts, Ctrl + F, `/Roles`) and stored in `~/.pp/agent/agent.db`; they are not configured in YAML. The remaining settings stay declarative YAML.

```yaml
edit: replace
auto_compact: true
auto_compact_threshold_percent: 80
```

Model requests have no total time limit. `request_timeout_seconds` defaults to 300 seconds and limits connection setup and idle reads; incoming text, reasoning, and SSE heartbeats reset the idle timer. Explicitly configured values still apply.

Automatic compaction is enabled only when the model has an explicit `context_window`. Reasoning levels must match the provider's capabilities. Use `thinking.efforts` and `thinking.defaultLevel` in the model configuration to restrict available levels and set a default.

See the [configuration guide (Chinese)](docs/guide.md#配置) for full examples and validation rules. Model and application settings are loaded only from `~/.pp/agent/`; project files with the same names and provider environment variables are not read. [System prompt overrides](docs/guide.md#系统提示词) can also be loaded from the startup directory's `.pp/`.

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

Opening the model picker in the TUI clears the main input and focuses a separate search field above the list. The panel height is set from the total model count when opened and stays fixed while filtering. The panel uses at most 15 rows: 3 for search, up to 11 for the list, and 1 for hints. Longer lists show a scrollbar and support arrow keys, page keys, and mouse-wheel scrolling inside the panel. Search filters as you type, matching any part of `provider/model` or the actual model name, case-insensitively. Ctrl + F toggles the highlighted model's favorite status. Favorites show a solid gold Nerd Font star `` and always appear before other models; within each group, models are ordered by usage, with more logical requests first and equal counts keeping catalog order. Usage counts logical model API requests actually attempted since the upgrade: connection retries of one request count once, while opening the panel or switching models does not count. Favorites and usage are stored in `~/.pp/agent/agent.db` and persist across restarts. Ctrl + S saves the default model. When no models match, the panel shows a message, and Enter and Tab do nothing.

Use `/help` for full help and `/tips` for usage tips. The following slash commands work in interactive modes; content passed through `-p` is always sent as a regular prompt.

| Command | Purpose |
| --- | --- |
| `/model`, `/effort` | Switch models and reasoning levels while keeping history |
| `/init` | Create or update the current project's `AGENTS.md` |
| `/plan [prompt]` | Toggle Plan mode standby; view the plan or send a planning task immediately; execute after approval (TUI only) |
| `/plan-list` | Manage saved plans: preview, continue, execute, rename, delete (TUI only) |
| `/compact`, `/new` | Compact context or start a new session |
| `/resume`, `/tree` | Restore a session or navigate back through current history (TUI only) |
| `/changes` | View recent-task edit/write changes and undo individual files (TUI only) |
| `/skills`, `/quit` | Manage skills or exit |

Enter `!command` to run a local, non-interactive bash command in the project directory. Output is shown locally, not sent to the model or saved in session history. Requires a configured agent and Execute mode; commands run with your user permissions, without a sandbox. Each command starts a fresh shell. In the TUI, the first `!` enters shell mode without appearing in the editor; subsequent `!` characters belong to the command. Esc leaves shell mode while editing; Backspace on an empty editor also leaves it. While a command is running, Esc cancels it; wait for cleanup before continuing.

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
| Expand / collapse the fixed TODO area | ⌥ + T | Ctrl + T |
| Page through expanded TODO tasks | ⌥ + PgUp/PgDn | Ctrl + PgUp/PgDn |
| View pending input | ⌥ + Q | Ctrl + Q |
| Resume the paused queue | ⌥ + R | Ctrl + R |
| Cancel the current task | Esc | Esc |
| Exit | Ctrl + C | Ctrl + C |

Type `@` for project file path completion. When idle with an empty input, press Esc twice to open the session tree. Navigating back through history does not roll back files. [Full interaction guide (Chinese) →](docs/guide.md#终端交互)

`read` accepts only `path` for local text, images, static web pages, and enabled `skill://` resources. The system lists skill names, descriptions, and URIs alongside selection instructions, including with custom base prompts: match the task and concrete subtasks, read applicable skills before other task tools, and briefly report their use after successful reads. This also applies to approved plans; selection remains model-driven, without automatic body injection. A bare skill URI reads the full `SKILL.md`; append `/<path>` for resources or `/.` for a directory listing. Skill text bypasses ordinary text limits and returns `path` and `base_dir`; explicit selectors still apply. Put exact ranges in the path: `src/main.rs:20-60`, `notes.md:-30`, or `file.rs:5-10,20-30`; use `next_path` to continue. `:raw` disables prefixes (and returns the original body for web pages). PNG/JPEG/WebP/GIF images reach the model as visual input; the terminal shows metadata only. Images persist across resume and protocol switches. Web reads extract static HTML text without JavaScript, support cancellation, and use a bounded session cache. [Read behavior and limits (Chinese) →](docs/guide.md#工具执行与权限)

`grep` searches local files in process with regex, path globs, file ranges and file-based pagination. Its `pattern`, `path`, `case`, `gitignore` and `skip` parameters are shared across Execute, Plan and Review. Set `grep: {contextBefore: 1, contextAfter: 3}` in `settings.yml` to control surrounding lines.

`glob` finds local paths in Execute, Plan and Review with `path`, `hidden`, `gitignore` and `limit`. It includes hidden files and respects ignore rules by default. Bare `*.rs` searches recursively, while `src/*.rs` searches only direct children. Results are ranked by modification time and capped at 200 paths and 50 KiB; narrow the directory if the five-second cooperative scan times out or results are truncated. Cancellation and timeout are checked between filesystem operations. `glob` is read-only but has no filesystem sandbox.

The TODO area stays directly above the input box and uses a subtle gray rounded border, with progress on the left and a muted shortcut on the right; narrow windows hide the shortcut first. Current and next pending tasks are indented inside (four rows in total). Active tasks are bold light blue, pending text is light gray, and blocked tasks are gold. Completed tasks use green checkmarks with muted, struck-through text; the next pending task stays readable. Expanded tasks are indented beneath their phase, with wrapped text aligned to the task content. The active phase is light blue and closed phases are muted. Use ⌥ + T on macOS or Ctrl + T elsewhere to expand it upward, up to one third of the terminal height. Scroll inside the area with the mouse wheel or use the same modifier with PgUp/PgDn. Fully completed lists display a green ✔ marker and green border around the summary (two rows); closed lists containing abandoned tasks show Closed instead. Small windows fall back to a one-line summary, and empty lists take no space. Task checklists appear only in this fixed area; conversation history keeps operation status and errors. Ctrl + O does not affect TODO. Resume and tree navigation restore the final task state with the area collapsed.

Tool previews follow the ANSI policies of [Oh My Pi](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0). Ordinary file reads show local path/range summaries by default, with a white dot and Read title, gray counts and tree branches, and gold paths on a dark gray-purple background. Grouped successful reads omit the `ok` suffix; set `read: {toolResultPreview: true}` in `settings.yml` to enable three-line content previews. Automatically loaded `AGENTS.md` and actual skill URI reads appear as separate dark bordered Read cards with a leading status dot, gold underlined Markdown headings, gold emphasis, ten-line previews, an expansion hint, and the recorded source path under an Output divider. These instruction previews remain visible when ordinary read previews are disabled; Ctrl + O shows their complete retained text. Unchanged project instructions are reported once per session, changes produce a new card, and resume replays the recorded bodies without rereading them for display. Skill discovery summaries are not body-load cards. Ctrl + O toggles existing and future tool output and reports the state. Search previews clip long rows, shell output keeps a bounded visual tail, and other tools have separate limits. Expanded grep and file lists remain width-clipped; MCP text still has a 12-line limit. These display limits do not change model-facing results. Session-tree previews retain complete output except for TODO checklists. Normal conversation uses the main screen and commits completed content incrementally; terminal history keeps the display state at commit time. Exiting only finishes uncommitted content and does not replay the session or automatically expand tools. History browsing, Ctrl + O, and panels temporarily use the alternate screen; Esc returns to the conversation. A successful `/new` clears the screen and native scrollback; resume and tree rewind rebuild them from the effective history. [Preview limits (Chinese) →](docs/guide.md#工具输出折叠)

With `edit: hashline`, local reads and grep results provide a `[path#TAG]` file-version header and `N:text` lines. Submit one file through `edit({"input":"…"})` using `PUT` replacements/insertions or `CUT` deletions. Only displayed lines may be edited; stale or missing snapshots require a fresh read. Each successful edit, write, or undo invalidates that file's snapshot. The default `replace` mode is unchanged.


</details>

Sessions are saved automatically in `~/.pp/agent/sessions/`. Restore one by its full ID, or open the local statistics page:

```sh
pp -r SESSION_ID
pp stats
```

`pp stats` automatically opens the statistics page in your default browser, served at [127.0.0.1:3848](http://127.0.0.1:3848) by default; piped or non-terminal contexts (scripts, CI, agent shells) only print the URL. Use `pp stats --no-open` to only start the server. No model configuration, Node, or CDN is required at runtime. Each active session is held exclusively by one pp process through `~/.pp/run/sessions/SESSION_ID.lock`, including while waiting for input. Run `pp gc [DAYS]` to manually remove logs, statistics, and unused session lock files older than the given number of days (default 90); original sessions and configuration are kept.

TUI input history is stored per working directory in `~/.pp/agent/history.db` and can be recalled with the up arrow after a restart; other directories and authorization-panel input never appear there. `/resume` searches titles, previews, IDs, paths, and every user prompt in the JSONL session logs, including branches outside the active leaf; all query words must match literally, and short Chinese words work without being treated as query syntax. The session index can be rebuilt from the logs, but the input history cannot: never delete `history.db` to rebuild the index, because that permanently loses recalled input. Line mode and `-p` prompts are searchable from the logs and are not added to TUI input history. See [session persistence](docs/guide.md#会话持久化与恢复) for details.

## Extensions

The linked guides below are in Chinese.

| Extension | Location | Description |
| --- | --- | --- |
| [Project instructions](docs/guide.md#项目级-agentsmd) | `AGENTS.md` in the working directory | Refresh project rules before each model request |
| [Skills](docs/guide.md#技能支持) | `~/.pp/agent/skills/` | Load `SKILL.md` on demand and manage skills with `/skills` |
| [System prompts](docs/guide.md#系统提示词) | `cwd/.pp/` or `~/.pp/agent/` | Replace base instructions with `SYSTEM.md` or `SYSTEM_TEMPLATE.md`, or append with `APPEND_SYSTEM.md` |
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
