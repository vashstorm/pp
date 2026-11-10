<h1 align="center"><img src="docs/assets/logo.svg" alt="pp" width="40" height="40" align="middle" /> pp</h1>

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

TUI rendering defaults to a maximum of 60 FPS. Set `tui.max_fps` in `~/.pp/agent/settings.yml` to an integer from 1 to 120 and restart to change it. Input and cancellation are processed independently of rendering; streamed text is batched for display.

## Features

| Feature | Description |
| --- | --- |
| **Terminal interaction** | Streaming text, reasoning, and tool output in a TUI, one-shot tasks, or piped input |
| **Planning and execution** | Plan research, review, and explicit approval, with an ask panel for key decisions; `/plan-list` manages saved plans across drafts, plans, and done |
| **Todo tracking** | Fixed progress above the input box, expandable task lists during execution, approved-plan checkbox synchronization, and bounded completion reminders |
| **Task control** | Add instructions while a task runs, queue follow-up tasks, and press Esc to cancel |
| **Sessions and changes** | Automatic saving and restoration, session tree navigation, persistent per-directory input recall, full-prompt `/resume` search, and recent-task diffs with safe undo |
| **Extensions on demand** | Project `AGENTS.md`, skills, Markdown prompt commands, and stdio and remote HTTP/SSE MCP tools |
| **Usage visibility** | Context estimates and compaction, plus local request, token, cost, and latency statistics |

Bash merges stdout/stderr and returns a bounded head/tail preview (50 KiB by default). Oversized or column-truncated output is saved as `artifact://<id>`; use `read` with line selectors or `rg` with that URI to inspect it. Background logs can be read while running, using a snapshot per call. The `wait` tool blocks until every selected background Bash task and subagent run has settled, updating one TUI card with live status, elapsed time, and settled-task progress. It accepts optional `task_ids` / `run_ids`; targets are frozen when the call starts. The fixed limit is 120 minutes: unfinished selected jobs and their descendants are stopped and reaped before timeout results return. Steering stays queued until the batch finishes; Esc cancels the parent task and its owned work. Legacy integer `timeout_ms` arguments are accepted but ignored, including `0`. Artifacts are shared by the root session and its subagents, survive resume, and are removed when that session is deleted; `pp gc` does not expire them. Saved logs default to a 16 MiB cap and explicitly report omitted bytes or capture failures. See [Bash artifacts](docs/guide.md#bash-输出与-artifact-资源) for configuration and completeness limits.

Foreground Bash accepts optional `timeout_seconds` from 0 to 3600 seconds; 0 disables the timeout. Omit it to use `bash_timeout_seconds`, which defaults to 300 seconds; explicit settings still apply. Background mode ignores this argument and keeps its existing task lifecycle. Cancellation remains available with no timeout.

Model-facing Bash results also include a recovery notice with a concrete `read` example when an artifact is available, or explain when capture failed. Notices distinguish complete logs, growing snapshots, and incomplete head/tail samples.

## Quick start

### 1. Build

You need a stable Rust toolchain that supports **Edition 2024**, and `bash` on your PATH. All profiles use the default LLVM backend; no Cranelift component or nightly features are required.

The checked-in `.cargo/config.toml` selects `ld64.lld` from your PATH on macOS ARM64 (`aarch64-apple-darwin`); install an LLD version compatible with your macOS SDK. Rust's bundled LLD may lag behind newer SDK formats. Linux x86_64 with glibc (`x86_64-unknown-linux-gnu`) uses the bundled LLD by default with Rust 1.90 or newer. This applies to development, release, and test builds. macOS also requires the Xcode Command Line Tools, and Linux requires a C toolchain. This does not provide a cross-compilation toolchain or SDK. Setting `RUSTFLAGS` overrides the configured target flags.

```sh
cargo build --release --locked
mkdir -p ~/.pp/agent
```

`rg` and `glob` run in process without external search programs. TUI icons require a Nerd Font. On macOS, enable Alt/Meta in your terminal to use Option shortcuts.

### 2. Configure a model

Start the terminal UI even without model configuration, then use `/login` to choose ChatGPT, GitHub Copilot, Kimi Code, GLM, Z.AI, or Command Code:

```sh
./target/release/pp
```

`/login` opens the authorization page in your browser. ChatGPT, Copilot, and Kimi use device authorization; GLM, Z.AI, and Command Code use masked API key input. Credentials are stored in plain text in `~/.pp/agent/agent.db`, with directory mode `0700` and database file mode `0600`; no system keychain or encryption is provided. Discovered subscription models remain in `~/.pp/agent/models-sub.yml`, which keeps its existing format. Discovery failures keep the authorization and offer a retry without adding a guessed default model. Quit every running pp process before upgrading: on first start pp imports the old preference fields in `settings.yml` and `auth.yml` into `agent.db` exactly once, then removes the authorization and those fields from YAML.

Use `/model` (alias `/models`) to select a model. An idle session switches immediately; a running session switches after its current response and complete tool batch have been committed. It reloads both model files; `models.yml` takes priority for identical `provider/model` references. `/logout` removes a provider's credentials from `agent.db` and its subscription models. These authorization commands require the TUI; the old `pp auth` command and `auth.json` are no longer used. Kimi and GLM currently use non-thinking mode. See [subscription authentication](docs/guide.md#订阅认证) for details.

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
# Model shorthand: fuzzy provider/model[:effort] for this run only (favorites win ties, the default effort applies when omitted), then run a startup slash command
pp --model deepseek/deepseek-flash:high /commit
```

> **Permissions and data**: API keys are stored in plain text in the model configuration. Read, shell, search, and MCP tools run with your user permissions, without a filesystem sandbox. The default YOLO mode bypasses all tool approvals, including dangerous-command checks and explicit prompt/deny policies. The write/edit save layer restricts paths to the working directory and rejects symbolic links; on macOS, `/tmp` and `/private/tmp` spellings of the same directory are both accepted when the workspace lives there. Sessions and statistics may contain source code and conversations; treat them as sensitive data. [Full permissions guide (Chinese) →](docs/guide.md#工具执行与权限)

## Everyday use

The main TUI supports `@file`, `@directory/`, and `@image-path` references. Use `Ctrl + V` for clipboard images or text, or paste image paths. Images appear as atomic icon-and-number tokens with thumbnails and hover previews; deletion, undo, and persistent input recall preserve attachment bindings. Loaded images are frozen snapshots. Graphics protocols fall back to Unicode half-blocks when unavailable. See the [input guide (Chinese)](docs/guide.md#常用命令与快捷键).

Opening the model picker in the TUI clears the main input and focuses a separate search field above the list. The picker opens upward from the top of the main input as an overlay, covering TODO and other content without changing their layout. Closing it restores the covered content. The panel height is set from the total model count when opened and stays fixed while filtering. The panel uses at most 15 rows: 3 for search, up to 11 for the list, and 1 for hints. Longer lists show a scrollbar and support arrow keys, page keys, and mouse-wheel scrolling inside the panel. Search filters as you type, matching any part of `provider/model` or the actual model name, case-insensitively. Ctrl + F toggles the highlighted model's favorite status. Favorites show a solid gold Nerd Font star `` and always appear before other models; within each group, models are ordered by usage, with more logical requests first and equal counts keeping catalog order. Usage counts logical model API requests actually attempted since the upgrade: connection retries of one request count once, while opening the panel or switching models does not count. Favorites and usage are stored in `~/.pp/agent/agent.db` and persist across restarts. Ctrl + S saves the default model. When no models match, the panel shows a message, and Enter and Tab do nothing.

Use `/help` for full help and `/tips` for usage tips. The following slash commands work in interactive modes; content passed through `-p` is always sent as a regular prompt.

Completion stays available whenever the main input is editable, including during streaming, compaction, cancellation, and model changes. Slash commands, supported arguments, and file references can be completed without sending the draft. Tab reopens dismissed suggestions and fills in the full value when there is one match, or extends the typed prefix as far as all matches agree. Commands that require an idle session can only execute after the task finishes. Markdown parsing, syntax highlighting, and text layout run on a background thread; a plain-text preview may appear briefly before the styled result. Selecting another model keeps committed context and does not cancel the current request, repeat a completed task, or restart background work. Unsupported history rejects the switch and keeps the current model.

Use `/Roles` to save role-specific models and effort. Every `task` item requires an explicit `role`. Choose an enabled built-in role (`fast`, `slow`, `vision`, `review`, or `explorer`) or the name of a loaded custom role from `.pp/agents/*.md` or `~/.pp/agent/agents/*.md`. For example, use `{"role":"api-auditor","task":"Review the API contract"}` for a custom role named `api-auditor`. Replace the former `agent` field with `role` in custom calls. DEFAULT and PLAN are reserved for the main session. REVIEW and EXPLORER are always read-only, and other roles can request `readonly: true`. Built-in roles use the configured model, effort, and fallback. Custom roles keep the model and thinking selected at dispatch. Changes to an enabled built-in role apply to its bound subagents at their next request boundary in the same pp process. Disabling or clearing a role leaves running subagents on the last model they applied. Changing only effort preserves the request prefix (`messages`/`input`, `tools`, `instructions`), while changing a model cold-starts prompt caching. See the [subagent guide](docs/subagents.md).

Only Main can dispatch subagents. Children, including restored ones and custom definitions with `spawns: ["*"]`, have no `task` tool or automatically generated delegation guidance and cannot delegate recursively. Subagent runs share a default concurrency limit of 4; set `task.max_concurrency` to 1–32 in `~/.pp/agent/settings.yml` and restart `pp` to change it. Each run acquires a slot before initialization and holds it across model requests, tools, and waits until its result cleanup finishes; runs waiting for a slot stay queued. Main does not occupy a slot. See the [subagent guide](docs/subagents.md).

Open Agent Hub with `/agents` or `/subagents`. Configure model roles with `/Roles`. The TUI shows the number of running subagents in gold before `tok/s`. The icon shows one person for one subagent, two for two, and three for any larger count. The badge disappears when no subagents are running.

Each role supports one fallback with its own thinking level. In `/Roles`, use ↑/↓ to select primary or fallback rows, Enter to pick a model, `f` to set a fallback, `x` to clear the selected row, and `t` (`e` also works) to adjust thinking. Clearing the primary disables the role while retaining its fallback; selecting a new primary keeps it disabled until you enable it. Delete/Backspace removes the whole role.

Model failures, including interrupted streams, retry from the current request boundary without repeating completed tools. Each model allows at most 10 sends, bounded by `rate_limit_retry_budget_seconds`; the fallback gets an independent allowance. Zero disables retries but still permits one fallback send. Cancellation and local configuration, persistence, or cleanup failures stop immediately. A successful fallback remains active for the current task; the next independent task restores the primary. Manual `/model` selection detaches the role fallback. Failed partial output remains visibly marked and is excluded from valid model history. Existing role settings migrate transactionally to `agent.db` schema v4.


All 13 `/test-*` commands use an embedded loopback-only Mock AI Server in dev/test builds (`#[cfg(debug_assertions)]`), independently of the `debug` setting. They run the real Responses client, Agent, tools, and subagents from compiled-in JSON scripts, without a configured provider or user model roles; even an empty HOME or invalid model/settings YAML does not block them. Every scenario has fresh system-temporary workspace/config/session/lock directories. They do not change the normal session, model, roles, working directory, or usage statistics. Expected tool failures continue; cancellation and setup/cleanup failures stop the command.

`/test-tools` runs read → write → edit-replace → edit-hashline → rg → glob → bash → bash-task → wait → todo → ask, with a fresh Agent per scenario; `/test-task` and `/test-ui` are separate. The TUI permits these commands while Ready or configuration-Unavailable, in Execute mode with no active/pending work; Esc cancels and preserves the current draft, then the normal display is restored. `/test-ask` uses the real TUI dialog (its noninteractive CLI failure is an expected result). Release builds do not register these commands or start the server: line mode silently ignores an unknown command, the TUI preserves its draft, and help/completion omit the names.

| Command | Purpose |
| --- | --- |
| `/model` (`/models`), `/effort` | Switch models and reasoning levels while keeping history |
| `/init` | Create or update the current project's `AGENTS.md` |
| `/plan [prompt]` | Toggle Plan mode standby; view the plan or send a planning task immediately; execute after approval (TUI only) |
| `/plan-list` | Manage saved plans: preview, continue, execute, rename, delete (TUI only) |
| `/test-task` | Dev/test only: run three real FAST/SLOW/REVIEW subagents against independent local mock lanes; no user roles or provider required |
| `/test-tools`, `/test-read`, `/test-edit`, `/test-rg`, `/test-glob`, `/test-bash`, `/test-bash-task`, `/test-wait`, `/test-todo`, `/test-ask` | Dev/test only: all or one isolated real-tool scenario; edit covers replace and hashline; ask is interactive in the TUI |
| `/test-write` | Dev/test only: typewriter reply at 12.5 mock tokens/s; the two writes of a 24/28-line `/tmp/pp-test-write/demo.txt` stream at 240 mock tokens/s so the write preview scrolls its tail, then an escape failure and bash cleanup |
| `/test-ui` | Dev/test only: native Responses SSE reasoning and Markdown/code chunks at 12.5 mock tokens/s |
| `/mcp [list\|enable <name>\|disable <name>\|reconnect <name>\|reload\|help]` | Manage MCP servers without restarting |
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
| Insert a newline | Shift + Enter / Ctrl + J | Shift + Enter / Ctrl + J |
| Open the model menu | ⌥ + P | Ctrl + P |
| Toggle Agent Hub | ⌥ + A | Alt + A |
| Toggle favorite for the highlighted model | Ctrl + F | Ctrl + F |
| Cycle reasoning levels | Shift + Tab | Shift + Tab |
| Expand / collapse tool output | Ctrl + O | Ctrl + O |
| Expand / collapse the fixed TODO area | ⌥ + T | Ctrl + T |
| Page through expanded TODO tasks | ⌥ + PgUp/PgDn | Ctrl + PgUp/PgDn |
| View pending input | ⌥ + Q | Ctrl + Q |
| Resume the paused queue | ⌥ + R | Ctrl + R |
| Cancel the current task | Esc | Esc |
| Exit | Ctrl + C | Ctrl + C |

Type `@` for project file path completion. When idle with an empty input, press Esc twice to open the session tree. Navigating back through history does not roll back files. In the input box, `Shift + Enter` inserts a newline and outranks the completion and pending-input menus as well as send; `Ctrl + J` inserts a newline everywhere. pp requests both the Kitty keyboard protocol and xterm `modifyOtherKeys` mode 2 at startup and translates tmux's default `CSI 27;mod;code~` encoding, and terminals that ignore those requests simply fall back to `Ctrl + J`. Inside tmux set `set -s extended-keys on`, and when the outer terminal's terminfo has no `Eneks` (Ghostty) also `set -as terminal-overrides 'xterm*:Eneks=\E[>1u'`; keep iTerm2's *Apps can change how keys are reported* enabled. [Full interaction guide (Chinese) →](docs/guide.md#终端交互)

`read` accepts only `path` for local text, images, static web pages, and enabled `skill://` resources. The system lists skill names, descriptions, and URIs alongside selection instructions, including with custom base prompts: match the task and concrete subtasks, read applicable skills before other task tools, and briefly report their use after successful reads. This also applies to approved plans; selection remains model-driven, without automatic body injection. A bare skill URI reads the full `SKILL.md`; append `/<path>` for resources or `/.` for a directory listing. Skill text bypasses ordinary text limits and returns `path` and `base_dir`; explicit selectors still apply. Put exact ranges in the path: `src/main.rs:20-60`, `notes.md:-30`, or `file.rs:5-10,20-30`; use `next_path` to continue. `:raw` disables prefixes (and returns the original body for web pages). PNG/JPEG/WebP/GIF images reach the model as visual input; the read tool output shows metadata only. Images persist across resume and protocol switches. Web reads extract static HTML text without JavaScript, support cancellation, and use a bounded session cache. [Read behavior and limits (Chinese) →](docs/guide.md#工具执行与权限)

`rg` searches local files in process with regex, path globs, file ranges and file-based pagination across Execute, Plan and Review. It accepts `pattern`, `path`, `case`, `-i`, `gitignore`, `skip`, `context`, `-C`, `-A`, `-B`, `output_mode` and the ripgrep-style `-F`, `-w`, `-t`, `-g`, `glob` and `max_count`. `-i: true` means `case: false`; contradictory values are rejected. The boolean `-n` flag is optional and accepted for compatibility. Content always includes line numbers. `context` and `-C` set both context sides (different values are rejected); `-A`/`-B` override after/before, including zero. Otherwise, `rg: {contextBefore: 1, contextAfter: 3}` in `settings.yml` supplies the defaults.

`-F: true` treats metacharacters literally and skips the lenient compile chain and the PCRE2 fallback; `\n` then means a backslash followed by `n`, and only a real newline enables multiline matching. `-w: true` requires whole-word matches and combines with `-F`. `-t` takes a file type or a list of them (`!name` excludes, `all` is valid, unknown types are rejected), and `-g` (plain-name alias `glob`) takes an override glob or list (`!` excludes, ranked above ignore files, and a pattern without `/` matches basenames at any depth); both filter directory traversal only, so files named explicitly in `path` are always searched. `max_count` caps matches per file like `rg -m`; `0` performs no search and returns `No matches found` with `details.fileCount: 0`.

The tool is named `rg` throughout: `settings.yml` uses the top-level `rg:` key (the former `grep:` key fails strict parsing now), and `tools.approval` entries use `rg`.

`output_mode` defaults to `content`; `files_with_matches` returns paths only, and `count` returns per-file counts plus a total across all searched files. Single-line patterns count matching lines; multiline patterns count non-overlapping matches. Context options only affect `content`. All modes retain file-based `skip` pagination; count totals include files outside the displayed page and bypass content match limits. Existing scan limits still apply, and `details.countComplete: false` marks partial counts. Only `content` creates or updates Hashline edit snapshots; path and count modes leave existing snapshots untouched.

`glob` finds local paths in Execute, Plan and Review with required `pattern`, optional search directory `path` (defaults to cwd), and `hidden`, `gitignore` and `limit`. For example: `glob({"pattern":"**/*.rs","path":"src"})`. Put the glob expression in `pattern`; `path` accepts one literal directory, not a glob or a list of scopes. It includes hidden files and respects ignore rules by default. Bare `*.rs` searches recursively, while `src/*.rs` searches only direct children. Results are ranked by modification time and capped at 200 paths and 50 KiB; narrow the directory if the five-second cooperative scan times out or results are truncated. Cancellation and timeout are checked between filesystem operations. `glob` is read-only but has no filesystem sandbox.

The TODO area stays directly above the input box, with gray tree connectors and no bottom footer. The title shows progress and the expand/collapse shortcut; narrow windows shorten status text first to retain the shortcut. The collapsed view keeps the phase rows of the visible tasks and shows three consecutive tasks when space permits, centering the active task whenever possible and filling from neighboring tasks at either end. Lists with fewer than three tasks show every item. Pending tasks are gray, active tasks are bold bright yellow, and completed tasks remain green without strikethrough. Larger three-column checkboxes use `[ ]` and `[✓]`; blocked tasks use a gold `[!]`. Expanded tasks connect beneath their phase, with wrapped text aligned to the task content; active phases are bright yellow and completed phases are green. Use ⌥ + T on macOS or Ctrl + T elsewhere to expand upward, up to one third of the terminal height. Expansion and state updates center the current task when possible; scroll inside the area with the mouse wheel or use the same modifier with PgUp/PgDn. Completion preserves the chosen expanded state; the collapsed view keeps the last three tasks and a green ✔ Complete title. Lists containing abandoned tasks show Closed instead. Small windows show as many tasks as space permits, and empty lists take no space. Task checklists appear only in this fixed area; conversation history keeps operation status and errors. Ctrl + O does not affect TODO. Resume and tree navigation restore the final task state with the area collapsed.

Tool previews follow the ANSI policies of [Oh My Pi](https://github.com/can1357/oh-my-pi/tree/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0). Ordinary file reads show local path/range summaries by default, with a white dot and Read title, gray counts and tree branches, and gold paths on a dark gray-purple background. Grouped successful reads omit the `ok` suffix; set `read: {toolResultPreview: true}` in `settings.yml` to enable three-line content previews. Automatically loaded `AGENTS.md` and actual skill URI reads appear as separate dark bordered Read cards with a leading status dot, gold underlined Markdown headings, gold emphasis, ten-line previews, an expansion hint, and the recorded source path under an Output divider. These instruction previews remain visible when ordinary read previews are disabled; Ctrl + O shows their complete retained text. Unchanged project instructions are reported once per session, changes produce a new card, and resume replays the recorded bodies without rereading them for display. Skill discovery summaries are not body-load cards. Ctrl + O toggles existing and future tool output and reports the state. Search previews clip long rows, shell output keeps a bounded visual tail, and other tools have separate limits. Expanded rg and file lists remain width-clipped; MCP text still has a 12-line limit. These display limits do not change model-facing results. Session-tree previews retain complete output except for TODO checklists. Normal conversation uses the main screen and commits completed content incrementally; terminal history keeps the display state at commit time. Exiting only finishes uncommitted content and does not replay the session or automatically expand tools. History browsing, Ctrl + O, and panels temporarily use the alternate screen; Esc returns to the conversation. A successful `/new` clears the screen and native scrollback; resume and tree rewind rebuild them from the effective history. [Preview limits (Chinese) →](docs/guide.md#工具输出折叠)

With `edit: hashline`, local reads and rg results provide a `[path#TAG]` file-version header and `N:text` lines. Submit one file through `edit({"input":"…"})` using `PUT` replacements/insertions or `CUT` deletions. Only displayed lines may be edited; stale or missing snapshots require a fresh read. Each successful edit, write, or undo invalidates that file's snapshot. The default `replace` mode is unchanged.


</details>

Sessions are saved automatically in `~/.pp/agent/sessions/`. `pp -r` opens the resume session picker directly in the TUI; pass a full ID to restore that session without the picker, or open the local statistics page:

```sh
pp -r [SESSION_ID]
pp stats
```

`pp stats` automatically opens the statistics page in your default browser, served at [127.0.0.1:3848](http://127.0.0.1:3848) by default; piped or non-terminal contexts (scripts, CI, agent shells) only print the URL. Use `pp stats --no-open` to only start the server. To allow LAN access, explicitly bind a local LAN address with `pp stats --host 192.168.1.20`; replace the example with your own address. The server has no authentication, so anyone who can reach it can read statistics and request details; use it only on trusted networks. No model configuration, Node, or CDN is required at runtime. Each active session is held exclusively by one pp process through `~/.pp/run/sessions/SESSION_ID.lock`, including while waiting for input. Run `pp gc [DAYS]` to manually remove logs, statistics, and unused session lock files older than the given number of days (default 90); original sessions and configuration are kept.

`/resume` uses compact single-line rows with relative times, stable title colors, alternating backgrounds, and a blue selection bar. Press Ctrl + P to pin or unpin the selected session. Pinned sessions stay first within the active search and folder scope; each group is sorted by latest update. Pins persist in `history.db` across restarts and session-index rebuilds.

TUI input history is stored per working directory in `~/.pp/agent/history.db` and can be recalled with the up arrow after a restart; other directories and authorization-panel input never appear there. `/resume` searches titles, previews, IDs, paths, and every user prompt in the JSONL session logs, including branches outside the active leaf; all query words must match literally, and short Chinese words work without being treated as query syntax. The session index can be rebuilt from the logs, but the input history cannot: never delete `history.db` to rebuild the index, because that permanently loses recalled input and session pins. Line mode and `-p` prompts are searchable from the logs and are not added to TUI input history. See [session persistence](docs/guide.md#会话持久化与恢复) for details.

## Extensions

The linked guides below are in Chinese.

| Extension | Location | Description |
| --- | --- | --- |
| [Project instructions](docs/guide.md#项目级-agentsmd) | `AGENTS.md` in the working directory | Refresh project rules before each model request |
| [Skills](docs/guide.md#技能支持) | `~/.pp/agent/skills/` | Load `SKILL.md` on demand and manage skills with `/skills` |
| [System prompts](docs/guide.md#系统提示词) | `cwd/.pp/` or `~/.pp/agent/` | Replace base instructions with `SYSTEM.md` or `SYSTEM_TEMPLATE.md`, or append with `APPEND_SYSTEM.md` |
| [Prompt commands](docs/guide.md#markdown-提示词命令) | `~/.pp/agent/commands/*.md` | Expand Markdown templates with `/command arguments` |
| [MCP tools](docs/guide.md#mcp-工具) | `~/.pp/agent/mcp.json`, `/mcp` | Connect stdio and remote HTTP/SSE servers; configure Context7 or [Exa search and page fetching](docs/guide.md#接入-exa), grant specific MCP tools to Plan, Review and read-only subagents, and manage connections |
| [Language servers](docs/guide.md#lsp-语义导航与修改建议) | `~/.pp/agent/lsp.json`, `settings.yml` `lsp:` | Semantic navigation, diagnostics, and modification suggestions; the AI applies changes with edit/write/bash |

## Development

The repository is a Cargo workspace with seven packages. The root `pp` package keeps `src/main.rs` (startup arguments, logging, working-directory restore, mode dispatch, and exit code) plus the root `tests/`; it is the only default member and assembles the internal libraries below.

| Package | Responsibility |
| --- | --- |
| `pp-core` | Shared contracts: cancellation, context, events, plan, interaction queue, logging, SQLite opening, todo and LSP report types |
| `pp-config` | Settings and model YAML, model catalog, authentication with its HTTP client, agent store, and skills |
| `pp-stats` | Request snapshots, aggregation, and the read-only statistics server with embedded assets |
| `pp-llm` | Chat Completions and Responses requests, retry, streaming, parsing, and the shared HTTP response type |
| `pp-runtime` | Agent, tools, subagents, MCP/LSP, sessions and history, model control, and prompts |
| `pp-ui` | TUI, CLI, commands, shortcuts, and tips |
| `pp` (root) | Startup assembly and the root `cli_agent` integration tests |

`cargo run`, `cargo build`, and tests without a package selector apply only to the root default member. Use `--workspace` for full verification across every crate, and `-p <package>` for one library.

Tests use [cargo-nextest](https://nexte.st/docs/running/). Install it with `cargo install cargo-nextest --locked` if needed.
For local changes, select the relevant tests first; the full suite runs in parallel by default. Tests named with a `slow_` prefix form the serial `slow_test` set (real PTY, real binary, real subprocesses); the default profile filters them out, so full verification is the default profile plus `--profile slow_test`. Use `--test-threads=1` only to investigate specific failures.

```sh
cargo nextest run --locked --workspace context_
cargo nextest run --locked --workspace
cargo nextest run --locked --workspace --profile slow_test
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --doc
cargo fmt --all --check
```

See [development and validation (Chinese)](docs/guide.md#开发与验证) for targeted regression commands and test coverage limits.

---

Inspired by [Pi](https://github.com/badlogic/pi-mono), with model API support provided by [async-openai](https://github.com/64bit/async-openai).
