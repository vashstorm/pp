# pp

A minimal Pi-style coding agent in Rust: OpenAI-compatible Responses and Chat Completions via `async-openai`, real read/edit/write/bash tools, and multi-turn terminal interaction. Single crate, Rust Edition 2024, macOS/Linux supported; bash must be on the PATH.

## Build and Run

```sh
cargo build --locked
./target/debug/pp --help
./target/debug/pp --version
```

Prepare JSONC configuration in the startup directory; both files support `//`, `/* ... */` comments and trailing commas. `models.jsonc` is required: providers share credentials, base URL, and protocol, with multiple models under each provider. `settings.jsonc` is optional and controls app behavior itself. The default model is the first model of the first provider in the file:

```jsonc
// settings.jsonc
{
  // Optional; the first provider's first model is used when omitted or when the reference matches no provider/model
  "default_model": "gateway/secondary",
  // Optional, default "replace"; use "hashline" for anchored line edits
  "edit": "replace",
  // 思考级别：default（服务端默认）、low、medium、high、xhigh、max
  "think_level": "default",
  // Optional, default 20: maximum model requests per user task
  "max_model_requests": 20,
  // Optional, default 120: model request timeout in seconds, for either API
  "request_timeout_seconds": 120,
  // read 单次返回的源字节数与完整行数上限
  "read_max_bytes": 65536,
  "read_max_lines": 2000,
  // bash 超时秒数，与模型请求超时独立
  "bash_timeout_seconds": 120,
}
```

```jsonc
// models.jsonc
{
  "providers": [
    {
      // Provider alias, unique across the file, must not contain /
      "id": "openai",
      // protocol omitted: defaults to "response" (Responses API)
      "api_key": "your-key",
      // Optional for OpenAI, default https://api.openai.com/v1
      "base_url": "https://api.openai.com/v1",
      "models": [
        // id is an alias within the provider; model is the raw model name sent to the vendor
        {"id": "primary", "model": "vendor-model-one"},
        {"id": "secondary", "model": "vendor-model-two"},
      ],
    },
    {
      "id": "gateway",
      "protocol": "completion", // Chat Completions API
      "api_key": "another-key",
      "base_url": "https://example.com/custom/v1",
      "models": [
        {"id": "primary", "model": "gateway-model-one"},
        {"id": "secondary", "model": "gateway-model-two"},
      ],
    },
  ],
}
```

File names are fixed: `settings.jsonc` and `models.jsonc` are always read from the startup directory (the former may be absent; a missing latter fails at startup). Custom paths or renames are not supported. The tools' working directory is the startup cwd:

```sh
chmod 600 models.jsonc
./target/debug/pp -p 'Read the current project and explain its structure; do not modify files.'
./target/debug/pp
```

**API keys are stored in plaintext in models.jsonc.** The repository root `models.jsonc` is already in `.gitignore` to prevent a leftover personal key file on disk from accidentally entering version control. The tools have no sandbox and can still read configuration files; do not let the model read or output files containing keys. `settings.jsonc` holds no credentials and needs no ignore entry.

Validation and model-selection rules:

- `think_level` 可设置为 `low`、`medium`、`high`、`xhigh` 或 `max`；省略时为 `default`，也可显式设为 `"default"`。仅接受这些小写字符串，拒绝 null 和其他值。Responses 请求使用 `reasoning.effort`，Chat Completions 请求使用 `reasoning_effort`；`default` 会省略该参数。档位原样传给服务端，具体模型的支持范围由服务端决定，不自动降档或重试。参见 [OpenAI 官方思考级别说明](https://developers.openai.com/api/docs/guides/reasoning)。
- TUI 输入 `/effort `（命令后加空格）即在输入框下方展开档位菜单，左侧显示档位，右侧显示说明和“当前”标记，黄色突出选中项。↑/↓ 选择，Enter 确认，Esc 取消并保留输入；继续输入档位前缀会筛选候选，Tab 补全但不执行。菜单包含 `default` 和上述五档，不显示固定 token 预算。输入 `/effort` 后按 Enter 也可展开。行模式用 `/effort` 查看当前级别及可选值；两种模式均可用 `/effort high` 直接切换、`/effort default` 恢复服务端默认，兼容旧命令 `/think`。切换保留历史，对后续请求生效；`/model` 和 `/new` 均保留当前级别。TUI 底部状态栏显示实际选择，运行中拒绝切换并保留输入草稿。命令仅改变本次进程，不改写配置文件；重启后使用配置值。
- 缺少 `settings.jsonc`、省略 `default_model` 或没有匹配的 provider/model 时，使用首个 provider 的首个模型。模型请求次数默认 20，模型请求超时和 bash 超时分别默认 120 秒；read 默认最多返回 65,536 源字节且不超过 2000 个完整行。限制值必须为正整数，拒绝 null、负数、小数、字符串和数值溢出。
- `edit` defaults to `"replace"` when settings or the field are absent. Only exact lowercase `"replace"` and `"hashline"` strings are accepted; null, other types, case variants, and surrounding whitespace are rejected. It is fixed at startup and survives `/model` (including cross-protocol switches) and `/new`. Existing anchored-edit users must explicitly select `"hashline"`.
- An explicit `default_model` must be `provider/model`, split at the first `/`; neither part may be blank, matching is case-sensitive and exact with no trimming. The provider id cannot contain `/`, but the model id can: `gateway/team/model` selects the model with id `team/model` under gateway. Bare model ids are not accepted, and there is no environment-variable fallback.
- `providers` and every provider's `models` must be non-empty. Provider ids are unique across the file; model ids only need to be unique within their provider, and different providers may reuse the same model alias. id, `api_key`, and the vendor `model` must all be non-blank. All providers and models are fully validated at load, including unselected ones.
- Unknown fields are rejected at the file top level, the provider level, and the model level. A model may contain only `id` and `model` and cannot override the provider's credentials, base URL, or protocol; the old top-level flat `models` format is no longer accepted.
- Provider-level `protocol` selects the API: `"response"` uses `POST /responses` and is the default when omitted; `"completion"` uses `POST /chat/completions` (not the legacy `/completions` API). Only these exact lowercase strings are accepted. Null, wrong types, old `"openai"`, and `"anthropic"` invalidate the entire configuration, including on an unselected provider. To migrate an existing Chat Completions provider, replace `"openai"` with `"completion"`; omitting the field now selects Responses.
- Provider configuration is read only from these two JSONC files: no provider environment variables, no environment substitution or fallback, and no `.env` or OAuth loading. Errors name the offending file and never dump config values, URLs, or keys. Choose the API supported by the configured service and model; there is no automatic protocol fallback.

When running `pp` directly, an unusable configuration (`models.jsonc` missing, unreadable, encoding/JSONC/field/validation errors, or initialization errors such as an API key unusable in an HTTP header) never blocks entering interactive mode: the error is shown in the conversation area or the line terminal, and input remains editable. A missing `settings.jsonc` is not an error; defaults apply. With unusable configuration, submitting a task, `/new`, or `/model` shows the startup error without sending requests or running tools; `/help` remains available and `/quit` exits cleanly (exit code 0). Line mode also supports EOF. Restart after fixing configuration — there is no reload. `pp -p 'task'` instead exits non-zero on unusable configuration.

A provider's `base_url` is the API root. The existing path prefix is preserved; trailing `/` characters are stripped and the SDK appends `/responses` or `/chat/completions` according to `protocol`, with no automatic `/v1`. When omitted, `https://api.openai.com/v1` is used. Every explicit URL (including unselected providers) must be http/https with a host; userinfo, query, fragment, all-blank, and null are rejected. An explicit HTTP URL transmits credentials and sessions in plaintext — use it only with trusted compatible services. Only the selected key goes through HTTP header validation; unselected keys are merely checked for non-blankness.

## Terminal Interaction

未指定 `-p` 且标准输入、标准输出均为终端时，`pp` 自动进入 Ratatui TUI：启动页采用 omp 风格的居中像素字标、版本号和简短操作提示；发送消息后，上方切换为流式结构化对话记录，下方为多行输入框，使用复古绿色（`#7FA66A`）上下横线和 `❯` 提示符。图标采用 Nerd Font 字形，终端需使用支持这些字形的字体。输入框底部状态栏依次显示实际模型、思考级别、启动工作目录和 Git 信息。`default` 表示请求未显式指定思考级别，使用服务端默认值。较长的模型名、路径和分支名会自动缩略。Git 状态每两秒在后台刷新，不阻塞输入：`` 表示分支或分离头指针的提交，`*N` 表示已跟踪的改动条目数，`?N` 表示未跟踪条目数，配置上游时通过 `↑N` / `↓N` 显示领先和落后提交数。非 Git 目录或 Git 不可用时隐藏 Git 信息。运行状态及忙碌提示位于输入框上沿左侧；右半区预留为第二状态栏，目前保持空白。执行 `/new` 后恢复欢迎页；完整快捷键和工具安全说明可通过 `/help` 查看。模型文本及 bash 标准输出、标准错误均在完成前实时显示，工具进度保留在对话记录中。暂不提供侧栏或 Markdown 渲染。

Entries use a left rail and bold label: blue `You`, yellow `bash · $ command`, and terminal-default `Assistant`/read/write data. Tool bodies are dimmed, with separate stdout/stderr labels, running/ok/failed status, exit codes, and truncation notices. Cyan `grep` and magenta `skill` are reserved display categories only: no new tools, skill discovery, or command/path-based classification. Notes are gray and errors red; the terminal background is unchanged. ANSI color output respects `NO_COLOR`.

- Enter 发送非空草稿，Ctrl-J 插入换行；粘贴保留多行且不会自动发送。菜单关闭时，↑/↓ 将本次进程中已提交的历史输入复制到输入框，支持继续编辑；↓ 越过最新一条会恢复浏览前的草稿和光标，历史两端不循环。运行中也可浏览输入历史，忙碌时被拒绝的提交不计入历史；`/new` 重置模型会话后仍保留输入历史，退出后不保存。菜单展开时，↑/↓ 仍用于选择菜单项。
- Ctrl+A / Ctrl+E 分别跳到当前行首 / 行尾；Ctrl+K 删除光标到当前行尾的内容，在行尾或空行按下不删除换行。左右键、Home/End、Backspace/Delete 继续支持中文和 Unicode 文本编辑。
- The input box grows with explicit lines, up to six; long lines soft-wrap, and the editing viewport scrolls past its height. Narrow screens crop the bottom bar; short screens keep the input and status first; restoring size never loses the draft.
- Mouse wheel scrolls the transcript by three visual lines per event; PgUp/PgDn scroll by a page. Both work without moving the input cursor or changing the draft, even with the pointer over the input box. Output follows the bottom by default; scrolling up leaves follow mode, so incoming chunks do not steal the viewport. Scrolling down to the bottom restores following. Drafts stay editable and scrollable while running, but Enter does not queue or clear the draft; submission resumes after completion or an error.
- 命令、模型和思考级别菜单展开时，滚轮仍滚动历史，不改变菜单选择。Clicks, drags, pointer movement, and horizontal wheel events are ignored. The TUI captures the mouse; any modifier needed to bypass capture for native terminal selection or scrollback depends on the terminal emulator.
- Single-line slash commands share one registry with line mode: `/help` lists commands; `/model` opens a model picker; `/model provider/model` switches directly; `/new` resets history while keeping the selected model; `/quit` exits. Unknown names and extra arguments are local errors, never model tasks. Names are case-sensitive; leading/trailing whitespace is ignored, but the entire remaining `/model` argument preserves internal spaces and `/`. Multi-line slash drafts remain ordinary tasks.
- 所有选择菜单（`/` 命令补全、`/model `、`/effort `）统一使用 `src/tui/menu.rs` 中的 `SelectionMenu` 组件，业务层只提供选项数据和确认动作。统一在输入框下方显示无外框的两列列表，左列占 46%、右列占 54%；选中行以黄色文字和箭头突出，其余说明与图标为灰色，当前值显示“（当前）”，输入框使用紫色分隔线。最多显示八行，窄屏截断说明，短屏随选择滚动。↑/↓、Home/End、PgUp/PgDn 导航且不循环，Tab 补全但不执行，Enter 确认一次；Esc 收起列表并保留草稿，下次编辑时重新显示。输入 `/` 展开命令菜单，继续输入会按命令前缀筛选，左列保留命令图标。
- 输入 `/model `（命令后加空格）会在输入框下方展开与 `/effort ` 一致的两列菜单：左侧为 `provider/model`，右侧为实际模型名、`completion`/`response` 及“当前”标记，黄色文字和箭头突出选中项。`/model` 后按 Enter 也能展开，默认选中当前模型。按启动配置顺序排列，最多显示八行，长列表随选择滚动；↑/↓、Home/End、PgUp/PgDn 导航，Enter 确认，Esc 收起并保留输入。继续输入按引用或实际模型名的前缀筛选，Tab 补全引用但不切换；多行粘贴仍作为普通任务。后台完成切换后才更新状态栏，历史和确认后新编辑的草稿均保留；忙碌时拒绝切换、不排队、不清空草稿。`/new` 仍在后台确认后清空历史和草稿。
- `/quit` 或 Ctrl-C 立即退出整个 pp，正常退出码为 0。恢复终端后，全部对话记录按退出时的终端宽度写入普通终端的滚动历史，保留原有颜色、缩进、分隔线、流式片段、工具输出及错误，末尾保留输入框、未提交的草稿、菜单和状态栏；可以向上滚动查看超出一屏的历史。保留长度受终端自身的滚动历史容量限制。退出不等待阻塞中的模型请求或工具，不提供任务级取消，也不保证终止工具子进程，文件修改不会回滚。
- `-p/--prompt` always takes precedence and never parses slash commands: `pp -p '/model'` sends `/model` as a task. If either stdin or stdout is not a terminal, line mode supports the same commands without raw mode or an alternate screen. `/model` lists configured models and marks the current one; use `/model provider/model` to switch. Stdout streams **all readable assistant text**, including text before tools, with one terminating newline per nonempty turn and no replay at completion. Refusal is printed only if no non-blank content wins. Help, command responses, errors, and `tool <name>: running/ok/failed` go to stderr; live bash data uses `[bash stdout]` / `[bash stderr]` on pp's stderr. Non-TUI output adds no ANSI control sequences. A display write failure is returned only after the Agent finishes recording the task's history/tool results.

## Execution and Permissions

**write/edit 仅修改启动目录内文件，按单次调用原子提交。macOS/Linux 的 bash 均无文件权限隔离，仍可修改当前用户有权限访问的目录外文件；没有逐次确认。请在可信或临时目录中运行。**

- 启动时固定规范化工作目录。`read` 允许目录外读取及符号链接；`write/edit` 接受目录内的相对或绝对路径，拒绝越界路径以及写入路径上的符号链接（包括指向项目内的链接）。父目录通过目录句柄逐级打开，保存不重新按完整路径追踪链接。允许不越出工作目录的 `..`，不扫描项目规则或技能文件。
- `read(path, offset?, limit?)`：`offset` 是从 1 开始的起始行，默认 1；`limit` 默认 `read_max_lines`，必须为正整数且不超过该配置。读取普通文件，使用有界缓冲跳过前面的行；每页同时受 `read_max_bytes` 和行数限制。返回 `ok`、`content`、`truncated`、`lines_read`、`next_offset`；有后续内容时使用 `next_offset` 继续，文件结尾为 null。超出文件末尾返回空页。两种模式均只返回完整行，超长单行无法放入空页时返回错误；增大配置或使用 bash 检查有界片段。默认 replace 返回保留原始换行、无前缀的 lossy UTF-8；字节上限计算源文件内容，不包括 hashline 前缀及 JSON 编码。不会解析图片或二进制格式。
- Default `edit({path, edits:[{oldText,newText}]})`: changes an existing UTF-8 file. Every item matches the same original snapshot, not earlier replacements. Nonempty `oldText` must be unique even after loose normalization; overlapping or duplicate targets are rejected, adjacent targets are allowed. Use minimal unique context; insert by repeating neighboring context in `newText`, or delete with `newText:""`. Empty arrays, missing matches, and whole-batch no-ops fail without writing; mixed no-op and changing items are allowed. On failure, read again.
- Replace matching tries exact LF-normalized text first, then NFKC → per-line ECMAScript `trimEnd` → smart quote/dash/Unicode-space normalization. It does not ignore case, indentation, or repeated inline spaces. If loose matching is needed, touched line blocks use normalized text while untouched line bodies retain their original content by line number. A leading UTF-8 BOM is preserved; CRLF and bare CR are normalized internally, then all output newlines use the source's first LF style (CRLF if preceded by CR, otherwise LF; LF when absent). No final newline is forced.
- 设置 `"edit":"hashline"` 后，read 显示 `N:hash|text`：N 为从 1 开始的真实文件行号，分页不重新编号；hash 为原始行正文（不含 LF/CRLF）的 16 位小写十六进制 FNV-1a 64 位哈希。显示分隔符为 LF，只为完整行生成锚点，空文件没有锚点。edit 仅接受带锚点的 `replace`（包含首尾行，`lines:[]` 表示删除）、`insert_before`、`insert_after`。新行字符串不能包含 CR/LF 或显示前缀。操作均引用原始快照；拒绝重叠替换、重复插入位置及触及替换边界的插入。锚点失效时重新读取；只检查端点，不校验内部行或整文件版本。未触及的字节保持原样，新分隔符采用源文件首个换行风格，并保留被替换末行是否带换行的状态。
- 两种 edit 模式均先对同一份原始快照校验全部修改，再调用共用原子保存流程。空文件、缺失文件、目录、符号链接和非 UTF-8 文件不能编辑；edit 不创建文件或目录。成功返回 `edits_applied` 和最终 UTF-8 `bytes_written`。保留严格且分模式的参数校验，不增加 diff/patch 字段。
- `write(path, content)`：创建缺失父目录，并创建或完整覆盖 UTF-8 文件，允许空内容。write/edit 均在目标所在目录排他创建临时文件，完整写入、保留原文件权限（含可执行位）、同步临时文件后，以一次原子重命名提交。创建、部分写入、同步或重命名失败均保留原文件；新建失败不会留下残缺目标，已创建的空父目录允许保留。重命名是提交点，之后父目录同步失败返回 `ok:true` 并附 `warning`，不能当作尚未修改。失败路径清理临时文件；不提供成功后的撤销、备份、跨文件事务、跨进程并发隔离或断电恢复保证。
- `bash(command)`：以启动目录为工作目录执行 `bash -c`，关闭 stdin，并建立独立进程组。stdout/stderr 无需等待换行或进程退出即可实时显示；保留跨分块的 UTF-8 字符，无效字节采用 lossy 解码。两流合计最多保留 500 行，同时每流最多保留 65,536 个原始字节，先达到的限制生效；按 `\n` 分行，非空且没有末尾换行的片段也计一行。仅在确有超出内容时标记对应流截断；实时显示与返回输出使用相同限制，多余内容持续排空，命令继续执行。仅保证各流内部顺序，不保证跨流顺序；500 行额度按接收分块的顺序分配。返回退出码、输出、截断标记和 `timed_out`，信号退出时退出码为 null。
- Arguments are strictly validated: missing or extra fields, invalid JSON, blank path/command, unknown tools, and file/process errors all return `ok: false` to the model, allowing it to correct itself. A non-zero bash exit likewise returns `ok: false` with the output preserved.
- bash 的 `bash_timeout_seconds` 默认 120，与模型请求超时独立，模型不能通过工具参数延长。独立监督线程在超时时向进程组发送 TERM，最多等待 1 秒后发送 KILL 并回收直接子进程；可停止的非阻塞管道读取避免后代持有管道导致无限等待。超时返回 `ok:false, timed_out:true`，保留已捕获输出，不回滚 shell 修改。正常结束也清理同一进程组的残留后台任务；主动脱离进程组的进程不受保证，仍只支持有限前台命令。Ctrl-C 立即退出整个 pp，不提供任务级取消或子进程清理保证。
- bash 默认将 `TMPDIR`、`TMP`、`TEMP` 设置为项目内每次调用独立的 `.pp-tmp-*` 目录，结束后清理；`XDG_CACHE_HOME` 设置为项目内 `.pp-cache` 并保留缓存，二者均已加入忽略规则。这是默认位置而非权限隔离；不遵守这些变量的程序仍可能使用其他位置。清理失败通过结果中的 `warning` 报告。
- Tools execute serially; dependencies within a batch such as write → read become visible in order. The complete model output is saved first, then tool results are appended one by one with matching call IDs; a tool failure never skips the remaining calls in the batch.
- `read_max_bytes`、`read_max_lines`、`bash_timeout_seconds` 均要求正整数，启动时读取并固定；`/model`、`/new` 不重新读取或放宽这些限制。
- Each user task allows at most `max_model_requests` (default 20) model requests. The final batch at the limit still executes and returns into in-memory history before the limit error is raised; no next request is sent and no completion is claimed. Each new task restarts the count.
- The full in-memory session is kept, with no persistence or context trimming. Network/protocol errors do not roll back real file changes and never retry automatically; previously completed history is kept.
- `/model` selects only from the fully validated startup catalog; it neither reloads nor writes configuration. Selecting the current reference is a no-op with confirmation. Failed resolution/client initialization/history conversion preserves the current model and history. Switching does not probe HTTP, execute tools, retry, or fall back; service compatibility is checked by the next actual request.
- Switching preserves user/assistant text, system instructions, tool-call IDs/arguments, and tool results. Completion-to-Completion keeps native messages; Responses within the same configured provider keeps native output, including encrypted reasoning. Cross-protocol or cross-provider Responses switching converts readable history and strips provider item IDs, replay metadata, and opaque encrypted reasoning. Readable reasoning summaries/content become assistant text; interleaved Responses assistant messages/calls are batched for Completion. Unsupported non-text or unknown history aborts the switch rather than losing records. Encrypted vendor state is not portable.

## HTTP Protocol

`async-openai` creates and consumes typed SSE streams for both APIs; streaming is the only execution path, with no non-streaming fallback or extra request on failure. A Tokio runtime bridges SDK calls to the synchronous Agent and TUI worker; model requests never run on the TUI drawing thread. Each request uses `request_timeout_seconds` (default 120); automatic redirects and retries are forbidden. SDK environment-derived credentials, API base, organization, and project are explicitly overridden; configuration remains file-only.

- **`response` (default):** sends `model`, `instructions`, full local `input` history, function `tools`, `tool_choice: auto`, `stream: true`, `store: false`, and `include: ["reasoning.encrypted_content"]`. It does not use `previous_response_id` or server-side conversations. Only text/refusal deltas are displayed, ordered by output item/content part; argument deltas and reasoning remain hidden. `response.completed.response` is the sole source of committed output and calls; it releases the turn immediately without waiting for HTTP EOF or `[DONE]`. Response/message/reasoning/function-call statuses must satisfy completed validation. Message, reasoning (including encrypted content), and calls are retained in native order; results use matching `call_id`. `/new` clears input and retains instructions.
- **`completion`:** sends `model`, `messages`, function `tools`, `tool_choice: auto`, and `stream: true`. Chunks use the SDK schema (`id`, `object`, `created`, `model`, indexed choices); only choice index 0 is consumed, with usage-only empty choices ignored. An actual assistant role and a `stop` or `tool_calls` finish reason are required. Tool fragments are accumulated by index and validated only after the stream ends without error. Truncation, filtering, and EOF before finish are failures. The SDK consumes `[DONE]` internally; a legal finish plus clean stream termination is accepted even if the server omitted `[DONE]`. `/new` keeps only the system message.

Both APIs validate the entire batch before saving assistant output or executing tools: only function calls with non-blank IDs and names are accepted, and call IDs cannot repeat within a batch. A bad batch never executes even valid writes from that response. Neither readable text nor calls is a protocol error. All text parts are concatenated even in tool turns; content takes priority over refusal. Final Responses text may add a missing suffix, but conflicting streamed non-blank content fails the turn before tool execution. A broken stream retains visible partial text, marks the TUI entry failed, and reports a redacted error without committing incomplete assistant/tool history; the submitted user message and earlier completed tools remain. Tool arguments decode at execution time per tool type. Legacy Chat `function_call`, hosted tools, and vendor-specific extensions remain unsupported. Errors never dump provider messages, response bodies, or keys.

## Verification

Default replace example after reading `hello`:

```json
{"path":"demo.txt","edits":[{"oldText":"hello","newText":"edited"}]}
```

With `"edit":"hashline"`, after reading `1:a430d84680aabd0b|hello`:

```json
{"path":"demo.txt","edits":[{"op":"replace","start":"1:a430d84680aabd0b","end":"1:a430d84680aabd0b","lines":["edited"]}]}
```

```sh
cargo test --locked slash_
cargo test --locked model_switch
cargo test --locked streaming_ -- --nocapture
cargo test --locked replace_ -- --nocapture
cargo test --locked --test cli_agent edit_mode_survives_model_switch -- --nocapture
cargo test --locked hashline_ -- --nocapture
cargo test --locked drains_both_large_pipes_without_deadlock
cargo test --locked responses_invalid_batches_never_write_and_repl_recovers
cargo test --locked --test cli_agent tui:: -- --nocapture
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

工具加固回归包括：读取字节/行数边界、完整行分页和真实 hashline 行号，原子保存部分写入/同步/提交失败注入、权限保持、越界/符号链接拒绝和硬链接对端保留，以及静默/持续输出/忽略 TERM/后代持有管道的超时。两种协议验证工具失败后的历史记录、恢复和继续执行，跨模型切换及 `/new` 验证启动限制快照。

本次工具加固已在 macOS arm64 验证：104 项单元测试、55 项集成测试（含双协议与 PTY）、构建、Clippy 和格式检查通过。Linux 尚未实机运行；本地夹具测试不代表真实服务商兼容性。

Regressions use loopback HTTP fixtures and real subprocesses with independent JSONC configs and temporary working directories; they do not modify the test process environment or access external model services. Both APIs cover ordered tool dependencies, cross-turn history, tool-error correction, invalid batches without partial writes, request limits, timeouts, error redaction, and no automatic retries. Responses-specific cases cover omitted/explicit protocol selection, text aggregation/refusal, encrypted reasoning and complete output replay, optional function-call status, reset preserving instructions, rejected response/item statuses, and recovery without invalid output entering history. Configuration cases cover composite provider/model routing, default ordering, model aliases containing `/`, scoped uniqueness, strict protocol/URL/field types, full validation of unselected providers, removed protocol rejection, and no environment fallback. HTTP regressions include non-JSON 302/401/429/500 errors without redirects or exposed bodies.

Unix regressions additionally drive the TUI through a real PTY (100×30, `TERM=xterm-256color`), restoring the visible screen via vt100: multi-line input, drafts while waiting, paste without sending, real write/read, session history, protocol-error recovery, confirmed reset, unavailable configuration, and Ctrl-C while busy. After exit, termios flags, alternate screen, bracketed paste, mouse protocol mode/encoding, and cursor restoration are checked; the original pipe regressions remain. An SGR mouse-wheel scenario verifies scrolling through a 60-line streamed answer, stable history and CJK draft during incoming output, bottom clamping, and restored following on the next response. Physical mouse wheels and touchpads in specific terminal emulators have not been manually verified. Run alone: `cargo test --locked --test cli_agent tui:: -- --nocapture`.

Streaming regressions gate SSE completion on observed child output, covering both APIs' first deltas, uncompleted call fragments, atomic invalid batches, completed-only text/refusal precedence, missing-tail reconciliation, failed streams and redaction. Bash gates prove both pipes are visible before newline/exit; unit cases cover fragmented/invalid UTF-8 and exact byte caps. Ratatui TestBackend covers all five display categories, while real PTY cells verify User/Bash/Data colors. Paging while streaming preserves the old viewport and draft; resizing and Ctrl-C preserve the terminal restoration contract.

Hashline regressions cover source-byte truncation, strict anchors, snapshot multi-edits, line endings, rejection without partial writes, and stale-anchor recovery through both protocols. A standalone disposable Rust loopback driver also launched the built `pp` for each protocol: read → edit → read produced `edited\n` and `ok:true, edits_applied:1, bytes_written:7`; a stale multi-edit was rejected without changing external bytes, then a fresh read enabled recovery. Both processes returned exit 0 with stdout exactly `hashline-ok\n`. The temporary driver was removed; no personal config or real provider was accessed, and fixture success does not establish vendor compatibility.

Replace regressions cover snapshot batches, Unicode/whitespace boundaries, conservative uniqueness, overlap rejection, BOM/newlines/deletion, unchanged-line preservation, strict mode arguments, and source-byte read limits. Real subprocess tests exercise both APIs with omitted/explicit replace settings and both modes across Completion → Responses → Completion switches. An independent disposable Rust standard-library loopback driver served five requests to the built CLI: plain read → two-item exact/loose edit (27 bytes) → rejected valid+missing batch with unchanged bytes → recovery edit (32 bytes) → final answer. Observed results were `ok:true, edits_applied:2, bytes_written:27`, `ok:false`, then `ok:true, edits_applied:1, bytes_written:32`; exit 0 and stdout exactly `replace-smoke-ok\n`. Only temporary dummy-key configuration and loopback HTTP were used; no real provider compatibility is implied.

A separate Python standard-library loopback fixture exercised the built binary in all four combinations: Completion/Responses × pipe/TUI. Each showed `smoke-first-中` before releasing the remaining model text and both bash channels before creating the command's release file; returned tool JSON was exactly stdout `out-firstout-last`, stderr `err-first`, exit code 0. Pipe stdout contained only assistant text, without duplicates or ANSI. TUI observation confirmed distinct User/Bash/Data foregrounds, draft editing, paging across 60 lines, 100×30 → 20×4 → 100×30 resize, and immediate Ctrl-C during a pending request with original termios/alternate-screen/paste/cursor restored. Only disposable fixture configuration was used; no real provider compatibility is implied.

An actual PTY smoke test with the local fixture has been run: CJK/emoji editing, multi-line paste, soft-wrapping of indented code and long paths, paging through a 60-line answer, scrolling while running, resizing 100×30 → 40×10 → 20×4 → 100×30 with draft preservation, and a real tool writing and reading back `tui-ok\n` in `proof.txt` with no tool log breaking through the screen. Ctrl-C while busy returned to the shell with working echo and cursor, the file still present. Also verified: `-p` inside a terminal does not switch screens, and one-shot and pipe-mode stdout contain only the fixture's final answer and a newline. No external network or personal model configuration was touched.

The dual-API CLI was smoke-tested against a standalone Python standard-library loopback fixture in temporary working directories. Omitting `protocol` and explicitly setting `"response"` both used `/responses`; `"completion"` used `/chat/completions`. All three modes preserved an encoded custom URL prefix, ignored environment-provided SDK configuration, and completed a real write/read/bash batch in exactly two requests. `proof.txt` contained `dual-api-ok\n`, bash counted one line, tool results carried matching call IDs, and stdout was `dual-api-ok: 1 line` plus a newline, exit code 0. Responses additionally replayed encrypted reasoning and aggregated multiple text parts. No personal configuration or external service was used; temporary files were removed.

A separate default-protocol PTY smoke launched the actual TUI at 100×30: the task used `/v1/responses`, the screen rendered `responses-tui-ok`, and Ctrl-C exited with code 0 while restoring the alternate screen and original termios settings.

Slash/model regressions exercise local commands without HTTP, alias routing and credentials, complete tool histories across bidirectional protocol switches, native reasoning replay versus cross-provider portability, atomic initialization failure, read-only startup snapshots, and one-shot slash prompts. PTY cases cover command completion, picker confirmation/cancellation, retained answers and drafts, busy rejection, switch failure, 100×30 → 20×4 → 100×30 resizing, and terminal restoration when exiting in a picker or during a request.

A standalone Python standard-library fixture also exercised the actual binary through Completion A → Responses B → Completion A. All three requests carried the expected model/authorization and complete prior dialogue; slash commands made no requests. Stdout was exactly `alpha`, `alpha via b`, and `alpha via a`, one per line, exit code 0; both configuration files were byte-identical afterward. No external service or personal configuration was used.

**Real vendor calls are unverified.** After filling in a real vendor configuration supporting function tools and selecting its API via `protocol`, copy both configs to a temporary directory and run (file names are fixed and only read from the startup directory):

```sh
BIN="$PWD/target/debug/pp"
WORK="$(mktemp -d)"
cp settings.jsonc models.jsonc "$WORK"/
(
  cd "$WORK" || exit 1
  "$BIN" -p 'Create hello.txt containing "hello from rust agent" plus a newline; read it, verify the line count with wc -l via bash, then report the file content and line count.'
  cat hello.txt
  wc -l < hello.txt
  "$BIN"
)
```

The expected file contains `hello from rust agent` plus a newline, one line. In the interactive session, first ask for the file content, then follow up asking for the file name, checking context continuity; keep the temporary directory for inspection.

References: [Pi](https://github.com/badlogic/pi-mono), [tool loop](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md), [async-openai](https://github.com/64bit/async-openai), [Responses](https://platform.openai.com/docs/api-reference/responses), [Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create). Vendor-specific extension protocols and upstream session files are unsupported.
