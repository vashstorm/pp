# pp

A minimal Pi-style coding agent in Rust: OpenAI-compatible Responses and Chat Completions via `async-openai`, real read/grep/find/edit/write/bash tools, and multi-turn terminal interaction. Single crate, Rust Edition 2024, macOS/Linux supported; bash must be on the PATH.

## Build and Run

```sh
cargo build --locked
./target/debug/pp --help
./target/debug/pp --version
```

配置统一保存在 `~/.pp/agent/settings.jsonc` 和 `~/.pp/agent/models.jsonc`，支持 `//`、`/* ... */` 注释及末尾逗号。启动目录中的同名文件不再读取。 `models.jsonc` is required: providers share credentials, base URL, and protocol, with multiple models under each provider. `settings.jsonc` is optional and controls app behavior itself. The default model is the first model of the first provider in the file:

```jsonc
// settings.jsonc
{
  // Optional; the first provider's first model is used when omitted or when the reference matches no provider/model
  "default_model": "gateway/secondary",
  // Optional, default "replace"; use "hashline" for anchored line edits
  "edit": "replace",
  // 启动默认思考级别：none（关闭思考）、low、medium、high、xhigh、max；省略时为 none
  "think_level": "high",
  // 是否显示通知右上角的倒计时；关闭后通知仍会按时淡出
  "notification_show_countdown": true,
  // 通知自动消失的总时长（正整数秒），默认 10；修改后重启生效
  "notification_timeout_seconds": 10,
  // skill 根目录；相对路径以 ~/.pp/agent 为基准，空数组关闭扫描
  "skill_dirs": ["~/.pp/agent/skills"],
  // 全局禁用的 skill 名称；新发现的 skill 默认启用
  "disabled_skills": [],
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
        {
          "id": "primary",
          "model": "vendor-model-one",
          // Optional: restrict available efforts and choose this model's default
          "thinking": {
            "efforts": ["low", "high", "xhigh"],
            "defaultLevel": "high",
          },
        },
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

配置文件名和目录固定；`settings.jsonc` 可以缺省，缺少 `models.jsonc` 时模型不可用。通过 HOME 定位用户目录，不读取项目配置。新会话的工具以启动目录为工作目录；恢复会话使用日志保存的原工作目录。旧版本用户需手动将配置迁移到 `~/.pp/agent/`；程序不会自动复制含密钥的文件。

```sh
chmod 600 ~/.pp/agent/models.jsonc
./target/debug/pp -p 'Read the current project and explain its structure; do not modify files.'
./target/debug/pp
```

**API 密钥以明文保存在 `~/.pp/agent/models.jsonc`。** The repository root `models.jsonc` is already in `.gitignore` to prevent a leftover personal key file on disk from accidentally entering version control. The tools have no sandbox and can still read configuration files; do not let the model read or output files containing keys. 如果 MCP 的命令参数或环境变量含凭证，`settings.jsonc` 也属于敏感配置，应限制权限并避免提交。

Validation and model-selection rules:

- 每模型 `thinking.efforts` 是严格白名单：非空时仅接受 `low`、`medium`、`high`、`xhigh`、`max`，不隐式附加 `none`，也不接受 `default`。重复值去重，菜单及循环始终按 `none → low → medium → high → xhigh → max` 的允许子集排序。省略 `thinking`、`thinking: {}`、省略 `efforts` 或 `efforts: []` 均不限制原有六档。`thinking.defaultLevel` 支持六档及 `default`→`none` 别名；有非空白名单时默认值必须在其中。拒绝显式 null、错误类型、大小写/空白变体及未知字段，包括未选中的模型。
- 普通新启动优先使用模型 `defaultLevel`，否则使用允许的全局 `settings.think_level`，全局值不允许时使用规范顺序中最低允许档。实际切入其他模型时，显式模型默认值总是优先；没有显式默认则保留目标支持的当前档位，否则回退到目标有效默认。切到当前模型引用是无操作。`/new` 保留当前模型和档位。切换模型、原生请求参数与保存档位原子提交；持久化失败不部分改变状态。
- 白名单贯穿 `/effort`（含 `/think` 和 `default` 别名）、TUI 菜单、Shift+Tab、会话恢复与 API 请求；不支持的输入在本地拒绝，不改变档位、请求或日志。Shift+Tab 仅循环允许档，单档时保持档位与草稿。恢复精确保留仍被允许的保存档位及原生请求，修改 `defaultLevel` 不覆盖它；收窄白名单排除保存档位时拒绝恢复，不改写日志或发送请求。Ctrl+S 仍只保存全局 `settings.think_level`，不写模型配置、不改变当前会话；显式模型默认值及菜单 `(default)` 标记仍优先于保存的全局值。

- `think_level` 设置启动时的默认思考级别，支持 `none`、`low`、`medium`、`high`、`xhigh`、`max`；省略字段或缺少配置文件时使用 `none`。界面将 `none` 显示为 `no think`。旧值 `default` 仍可读取，但作为 `none` 的兼容别名，含义变为显式关闭思考。仅接受这些小写字符串，拒绝 null 和其他值。Responses 请求显式发送 `reasoning.effort`，Chat Completions 请求显式发送 `reasoning_effort`；`none` 均传为字符串 `"none"`，Responses 同时省略 `reasoning.summary`，其他档位仍使用 `reasoning.summary: "auto"` 请求可读思考摘要。通过本地模型白名单校验的档位原样传给服务端；白名单不保证供应商实际支持，未配置白名单时仍由服务端决定，不自动降档或重试。参见 [OpenAI 官方思考级别说明](https://developers.openai.com/api/docs/guides/reasoning)。
- TUI 输入 `/effort `（命令后加空格）即在输入框下方展开档位菜单，左侧显示档位，右侧显示说明和“当前”标记，黄色突出选中项。↑/↓ 选择，Enter 确认，Esc 取消并保留输入；Ctrl+S 将当前高亮选中的档位保存为全局默认值（写入 `~/.pp/agent/settings.jsonc`，下次普通新启动生效），不改变当前会话档位。菜单底部显示快捷键提示；继续输入档位前缀会筛选候选，Tab 补全但不执行。菜单只包含当前模型允许的档位；未配置限制时包含 `no think` 和上述五档，关闭思考的补全值为 `none`，不显示固定 token 预算。输入 `/effort` 后按 Enter 也可展开。行模式用 `/effort` 查看当前级别及可选值；两种模式均可用 `/effort high` 直接切换、`/effort none` 关闭思考，兼容旧命令 `/think` 和旧值 `default`。切换保留历史，对后续请求生效；`/new` 保留当前级别，`/model` 按目标模型策略选择档位；未配置策略的模型仍保留当前级别。TUI 底部状态栏显示实际选择，运行中拒绝切换并保留输入草稿。命令及 Shift+Tab 不改写配置文件；已有活动 session 时会持久化选择，写入失败则保留原档位。普通新启动按模型默认值与全局 `think_level` 优先级选择，恢复 session 使用仍被允许的保存档位。
- 缺少 `settings.jsonc`、省略 `default_model` 或没有匹配的 provider/model 时，使用首个 provider 的首个模型。模型请求次数默认 20，模型请求超时和 bash 超时分别默认 120 秒；read 默认最多返回 65,536 源字节且不超过 2000 个完整行。限制值必须为正整数，拒绝 null、负数、小数、字符串和数值溢出。
- `edit` defaults to `"replace"` when settings or the field are absent. Only exact lowercase `"replace"` and `"hashline"` strings are accepted; null, other types, case variants, and surrounding whitespace are rejected. It is fixed at startup and survives `/model` (including cross-protocol switches) and `/new`. Existing anchored-edit users must explicitly select `"hashline"`.
- An explicit `default_model` must be `provider/model`, split at the first `/`; neither part may be blank, matching is case-sensitive and exact with no trimming. The provider id cannot contain `/`, but the model id can: `gateway/team/model` selects the model with id `team/model` under gateway. Bare model ids are not accepted, and there is no environment-variable fallback.
- `providers` and every provider's `models` must be non-empty. Provider ids are unique across the file; model ids only need to be unique within their provider, and different providers may reuse the same model alias. id, `api_key`, and the vendor `model` must all be non-blank. All providers and models are fully validated at load, including unselected ones.
- Unknown fields are rejected at the file top level, the provider level, and the model level. A model may contain only `id`, `model`, and optional `thinking` and cannot override the provider's credentials, base URL, or protocol; the old top-level flat `models` format is no longer accepted.
- Provider-level `protocol` selects the API: `"response"` uses `POST /responses` and is the default when omitted; `"completion"` uses `POST /chat/completions` (not the legacy `/completions` API). Only these exact lowercase strings are accepted. Null, wrong types, old `"openai"`, and `"anthropic"` invalidate the entire configuration, including on an unselected provider. To migrate an existing Chat Completions provider, replace `"openai"` with `"completion"`; omitting the field now selects Responses.
- Provider configuration is read only from these two JSONC files: no provider environment variables, no environment substitution or fallback, and no `.env` or OAuth loading. Errors name the offending file and never dump config values, URLs, or keys. Choose the API supported by the configured service and model; there is no automatic protocol fallback.

When running `pp` directly, an unusable configuration (`models.jsonc` missing, unreadable, encoding/JSONC/field/validation errors, or initialization errors such as an API key unusable in an HTTP header) never blocks entering interactive mode: the error is shown in the conversation area or the line terminal, and input remains editable. A missing `settings.jsonc` is not an error; defaults apply. With unusable configuration, submitting a task, `/new`, or `/model` shows the startup error without sending requests or running tools; `/help` remains available and `/quit` exits cleanly (exit code 0). Line mode also supports EOF. 模型及其他运行参数修复后需重启；skill 目录配置可以在 `/skills` 面板按 r 重扫。 `pp -p 'task'` instead exits non-zero on unusable configuration.

A provider's `base_url` is the API root. The existing path prefix is preserved; trailing `/` characters are stripped and the SDK appends `/responses` or `/chat/completions` according to `protocol`, with no automatic `/v1`. When omitted, `https://api.openai.com/v1` is used. Every explicit URL (including unselected providers) must be http/https with a host; userinfo, query, fragment, all-blank, and null are rejected. An explicit HTTP URL transmits credentials and sessions in plaintext — use it only with trusted compatible services. Only the selected key goes through HTTP header validation; unselected keys are merely checked for non-blankness.

## MCP 工具

在 `~/.pp/agent/settings.jsonc` 中添加 `mcp_servers`，连接本地 stdio MCP 服务：

```jsonc
{
  "mcp_servers": {
    "local": {
      "command": "python3",
      "args": ["/absolute/path/to/mcp_server.py"],
      "env": {"APP_MODE": "development"},
      "timeout_seconds": 30,
    },
  },
}
```

`command` 必填，按 PATH 查找程序；`args` 和 `env` 默认为空，超时默认 30 秒且必须为正整数。直接传递参数，不执行 shell 展开；需要 shell 时显式使用 `bash -c`。服务继承进程环境，`env` 覆盖指定变量，工作目录为当前会话的工作目录。省略 `mcp_servers` 或设为 `{}` 时不启动服务。所有配置层均拒绝未知字段；服务名只能包含 ASCII 字母、数字、下划线和连字符，最长 24 字节，不能含 `__`。

连接时按照 [MCP 初始化流程](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) 完成版本协商和 `notifications/initialized`，再分页调用 `tools/list`。支持协议版本 `2025-11-25`、`2025-06-18`、`2025-03-26` 和 `2024-11-05`。本次实现仅支持 stdio 工具发现及 `tools/call`，不支持远程 HTTP、resources、prompts、sampling、elicitation 或运行中更新工具列表；服务端 ping 会正常响应，其他服务端请求返回不支持的方法错误。

工具名通常为 `mcp__服务名__工具名`，例如 `mcp__local__search`；包含特殊字符或组合名超过 64 字节时，工具部分替换为确定性的 `tool_<hash>`，调用时仍发送原始名称。重名会拒绝启动。MCP 的输入 schema 原样传给模型（`strict: false`），两种模型协议均支持调用，CLI/TUI 使用现有工具事件显示状态。结果保留 `content` 和 `structuredContent` 等字段，包装为 `{"ok":true,"result":{...}}`；`isError:true` 转为 `ok:false`，同批工具仍按顺序继续执行。

初始化或发现失败时，当前配置的模型任务不可用；不完整连接会清理。每次请求的读写共享同一截止时间，通知不会延长超时；传输故障后关闭连接，后续调用返回错误，不自动重连或重试。仅 TUI 主动取消会停止并回收当前 MCP 服务进程组，下一任务真正调用该服务时才使用同一启动配置重新初始化和发现；不重放旧调用。完整工具集合（名称、描述与 schema）必须不变，定义变化或普通重建失败会拒绝该服务直到重启 pp，其他服务与本地工具不受影响。普通超时／EOF／协议故障仍不自动重连。服务进程在会话中复用，`/model`、`/new` 不重启；恢复保存的会话时按当前全局配置重新连接和发现工具，不恢复旧凭证或过期工具定义。正常退出、TUI 的 Ctrl-C 退出及连接故障时清理服务进程组。最多连接 32 个服务、发现 256 个工具，单条协议消息最多 1 MiB。

MCP 服务按当前用户权限执行，能够访问工作目录之外的资源。服务 stderr 被丢弃，协议错误不输出原始响应体、命令参数或环境变量；成功的工具结果按数据展示和回填模型，服务应自行避免在结果中返回凭证。

```sh
cargo test --locked --test cli_agent mcp_ -- --test-threads=1
```

## 运行诊断日志

所有运行模式在参数解析后自动向 `~/.pp/logs/pp.yyyy-mm-dd.number.log` 追加 JSON Lines；`--help`、`--version` 和参数错误不创建日志。HOME 必须非空且为绝对路径。新目录权限 0700，新日志和固定 `.lock` 文件权限 0600，不改已有父目录权限；日志目录、锁和日志文件不接受符号链接，文件必须是普通文件。

按本地日期和 **10 MiB（10 × 1024 × 1024 字节）**严格分卷，编号从 1 开始、按数字递增。重启继续追加当天未满的最大分卷；不同工作目录的进程通过同一 `.lock` 的短时阻塞 flock 协调完整事件的选卷和追加，不覆盖或截断已有日志。仅支持提供可靠 flock 语义的 macOS/Linux 本地文件系统；运行中不要手动替换 `.lock`。单事件超过上限时整条拒写，不截断 JSON。

保留今天及此前 29 个本地自然日，与 mtime 无关；未来日期暂留。每次启动和跨日后的首次写入清理严格匹配命名的过期普通文件，跳过其他名称、符号链接和目录；没有后台清理，也没有总容量上限。

诊断事件包括应用启停（pid、启动 cwd、prompt/line/tui 模式）、配置不可用、任务、Completion/Responses 请求和工具的开始/结束、受控状态及耗时。工具类别仅为 `load_skill`、`read`、`grep`、`find`、`edit`、`write`、`bash`，其他统一为 `external`。不记录 prompt、回答/思考、工具参数/结果/输出、工具文件路径、错误原文、配置、URL 或 API key；不启用依赖库日志，也不读取 `RUST_LOG`。这些运行诊断日志与下述包含完整交互的 session 文件相互独立。

初始化失败，或退出时发现写入/轮转/清理失败，仅在 stderr 输出一次固定 `warning: file logging unavailable`，不改变应用退出码；TUI 运行中不打印日志错误。事件同步交给操作系统，无后台队列，不逐条 fsync；强杀或断电不保证最后一条完整/持久化，也不保证存在 `app_stopped`。

## Terminal Interaction

未指定 `-p` 且标准输入、标准输出均为终端时，`pp` 自动进入 Ratatui TUI：启动页采用 omp 风格的居中像素字标、版本号和简短操作提示；发送消息后，上方切换为流式结构化对话记录，下方为多行输入框，使用复古绿色（`#7FA66A`）上下横线和 `❯` 提示符。图标采用 Nerd Font 字形，终端需使用支持这些字形的字体。输入框底部状态栏依次显示实际模型、思考级别、启动工作目录和 Git 信息。`no think` 表示显式请求关闭思考。较长的模型名、路径和分支名会自动缩略。Git 状态每两秒在后台刷新，不阻塞输入：`` 表示分支或分离头指针的提交，`*N` 表示已跟踪的改动条目数，`?N` 表示未跟踪条目数，配置上游时通过 `↑N` / `↓N` 显示领先和落后提交数。非 Git 目录或 Git 不可用时隐藏 Git 信息。运行状态及忙碌提示位于输入框上沿左侧；右半区预留为第二状态栏，目前保持空白。执行 `/new` 后恢复欢迎页；完整快捷键和工具安全说明可通过 `/help` 查看。模型文本及 bash/grep/find 标准输出、标准错误均在完成前实时显示，工具进度保留在对话记录中。暂不提供侧栏或 Markdown 渲染。

应用自有的 TUI 文案统一使用英文，包括帮助、菜单、通知、skill 管理与加载卡片，以及这些界面接收的配置保存错误和 skill 诊断。用户输入、模型回复、文件／命令输出、外部 skill 元数据与正文，以及名称和路径保持原文；不限制 Unicode，也不强制模型使用英语。已有 session 中保存的历史工具错误忠实回放，不改写旧日志。共享生产者的配置保存错误和 skill 诊断在 CLI 中也使用英文。

用户输入继续使用蓝色左竖线和加粗标签，助手回答与 read/write 数据使用终端默认文字色。bash 与 think 使用下述独立样式；其他工具保留淡色正文与运行／完成／失败状态。真实 `grep` 调用使用青色卡片，`find` 使用普通数据卡片；标题显示工具名与紧凑 JSON argv，保留空格、引用和控制字符边界。搜索结果直接显示 stdout/stderr，不使用 bash 专属折叠或计时 footer。真实 `load_skill` 调用使用品红色 skill 卡片，不按文件路径或 bash 命令猜测类别。提示为灰色，错误为红色；颜色输出仍遵循 `NO_COLOR`。

- 临时消息以非模态圆角弹框显示在输入框上方靠右，左右至少留一格，不挤占布局、不改变草稿、光标或历史滚动位置。普通消息为绿色，重要提示／警告为黄色，错误为红色；通过边框、标题和正文的颜色亮度实现 200ms 淡入与最后 400ms 淡出，默认总时长 10 秒，不是窗口透明度。右上角显示剩余秒数（向上取整），无需输入也会更新并自动消失；`notification_show_countdown` 默认 `true`，关闭仅隐藏秒数，`notification_timeout_seconds` 设置总时长，默认 `10`，必须为正整数且不超出计时范围。通知配置在启动时读取，即使模型不可用也生效；设置缺失或无效时使用默认通知行为，无效设置仍显示启动诊断，修改配置后需重启。鼠标停留在弹框内（含边框）时隐藏倒计时并保持完全显示，移出后重新从配置时长倒数；新消息替换旧消息，不堆叠。使用终端原生选区复制：先将鼠标移入弹框使其保持显示，再按终端要求的修饰键拖选，使用终端自身的复制快捷键；程序保留鼠标捕获以支持悬停与滚轮，不提供复制按钮或应用内选区。终端选区只能复制屏幕上可见的文字，可能包含边框；不会取得截断后未显示的内容。中文、emoji 和多行正文自动折行，最多八行，过长内容只显示开头；空间不足时隐藏弹框或倒计时，恢复尺寸后仅显示尚未到期的消息，倒计时不会覆盖标题和边框。skill 全屏管理面板中的弹框采用相同规则。
- 界面已体现的成功变化不重复通知：模型和当前思考档位切换只更新状态栏，`/new` 直接恢复欢迎页并清空旧通知，skill 面板保存成功直接关闭。Ctrl+S 保存下次启动的默认思考档位以绿色弹框确认，不改变当前档位；本地命令、切换和保存的临时错误使用红框。启动 skill 扫描的非致命诊断汇总为黄色提示，完整详情保留在 `/skills`。任务／工具失败和执行不可用仍保留原有持久错误位置；通知不进入对话记录、session 或退出滚动历史，CLI／管道输出不变。
- Enter 发送非空草稿，Ctrl-J 插入换行；粘贴保留多行且不会自动发送。输入框使用终端原生光标，为拼音等输入法提供插入位置。菜单关闭时，↑/↓ 优先在输入框的显示行间移动（包括自动折行）；到达首行后再按 ↑、到达末行后再按 ↓，才浏览本次进程中已提交的历史输入，支持继续编辑；↓ 越过最新一条会恢复浏览前的草稿和光标，历史两端不循环。运行中也可浏览输入历史，忙碌时被拒绝的提交不计入历史；`/new` 重置模型会话后仍保留本次进程的输入历史；恢复 session 时从已保存用户消息恢复输入历史，不恢复草稿或历史浏览位置。菜单展开时，↑/↓ 仍用于选择菜单项。
- Shift+Tab 按 `no think → low → medium → high → xhigh → max → no think` 循环切换思考级别，后台确认后更新状态栏，保留草稿、光标和输入历史浏览位置。选择菜单展开时不切档。Ctrl+P 打开模型菜单，临时输入 `/model ` 供筛选和 Tab 补全；Esc 取消或 Enter 确认后恢复原草稿和光标，切换失败也保留。忙碌时拒绝切档和打开模型菜单，不排队。F1 显示与 `/help` 相同的帮助，忙碌时也可使用且保留草稿。三个快捷操作均忽略长按重复事件；skill 面板优先处理自身按键。Ctrl+N、Ctrl+U、Ctrl+W 沿用编辑器既有行为。
- 在光标所在的空白分隔 token 中输入 `@`，立即在输入框下方按目录层级浏览项目文件：只显示当前目录的直接子项，目录在前并以 `/` 结尾，Enter 或 Tab 进入目录，选择 `../` 返回上级；进入／返回目录时同步将当前 `@` 引用补全为对应的目录路径（如 `@src/tui/`），光标停在路径末尾，不添加空格、不发送消息，也不重新扫盘；返回根目录时恢复为 `@`。继续输入按当前子树内的相对路径做大小写敏感的子串筛选，匹配到深层文件时仍逐级显示其祖先目录，不再平铺后代文件。↑/↓ 选择，Enter 或 Tab 选中文件后**只插入完整的 `@文件路径`，不发送消息、不读取文件正文、不上传附件**，随后再次 Enter 才发送编辑好的原始文本。含空白、双引号或反斜线的路径使用 JSON 引号，例如 `@"docs/my file.md"`；补全目录时光标停在闭引号前，仍可继续输入筛选或选择子项。补全只替换当前引用，保留多行草稿与其他引用。邮箱、`@@foo` 和光标位于闭引号之后的 `@"…"` 不触发菜单。候选根为启动／恢复会话的工作目录，不向上扩展到 Git 根；后台枚举遵守项目内 `.ignore`、`.gitignore` 和 `.git/info/exclude`，包含未被忽略的隐藏常规文件，不列出 `.git` 或符号链接。每次重新激活时重新扫描，同一次选择中的筛选不重复扫盘；扫描期间 Enter/Tab 不提交，根目录无可选文件或无匹配时 Enter 按普通提交处理；子目录无匹配时保留 `../` 供返回上级，只有包含可选文件的目录才会列出。Esc 收起菜单并保留草稿，迟到的扫描结果不会重新打开；普通行模式和 `-p` 不提供文件菜单，`@` 文本原样发送。
- Ctrl+A / Ctrl+E 分别跳到当前行首 / 行尾；Ctrl+K 删除光标到当前行尾的内容，在行尾或空行按下不删除换行。左右键、Home/End、Backspace/Delete 继续支持中文和 Unicode 文本编辑。
- The input box grows with explicit lines, up to six; long lines soft-wrap, and the editing viewport scrolls past its height. Narrow screens crop the bottom bar; short screens keep the input and status first; restoring size never loses the draft.
- bash 使用暗色圆角边框卡片：顶部为 `$ command`，`Output` 横线分隔命令和灰色输出，底部显示耗时与退出码；运行中显示 running，失败保留错误和退出信息。超过 10 行时默认显示末尾 10 行及省略提示，Ctrl+O 展开或收起最近一次 bash 的完整输出，不改变输入草稿；退出时完整输出写入终端滚动历史。think 默认以无标题、无边框的灰色文本显示在独立暗色背景上，与最终回答分开。是否返回可读思考取决于服务端和模型；`/effort` 仍只控制思考级别。行模式和单次任务将思考文本写入 stderr，stdout 保持为回答正文，均不添加 ANSI 控制序列。Responses 摘要使用 [官方流式事件](https://developers.openai.com/api/reference/resources/responses/streaming-events)。
- Mouse wheel scrolls the transcript by three visual lines per event; PgUp/PgDn scroll by a page. Both work without moving the input cursor or changing the draft, even with the pointer over the input box. Output follows the bottom by default; scrolling up leaves follow mode, so incoming chunks do not steal the viewport. Scrolling down to the bottom restores following. Drafts stay editable and scrollable while running, but Enter does not queue or clear the draft; submission resumes after completion or an error.
- 命令、模型、思考级别和文件菜单展开时，滚轮仍滚动历史，不改变菜单选择。Clicks, drags, pointer movement, and horizontal wheel events are ignored. The TUI captures the mouse; any modifier needed to bypass capture for native terminal selection or scrollback depends on the terminal emulator.
- Single-line slash commands share one registry with line mode: `/help` lists commands; `/model` opens a model picker; `/model provider/model` switches directly; `/new` resets history while keeping the selected model; `/resume` opens the fullscreen saved-session picker (TUI only); `/quit` exits. Unknown names and invalid built-in arguments are local errors, never model tasks. Names are case-sensitive; leading/trailing whitespace is ignored, but the entire remaining `/model` argument preserves internal spaces and `/`. Multi-line slash drafts remain ordinary tasks.
- 输入框下方的选择菜单（`/` 命令补全、`/model `、`/effort `、`@` 文件选择）统一使用 `src/tui/menu.rs` 中的 `SelectionMenu` 组件，业务层只提供选项数据和确认动作。统一在输入框下方显示无外框的两列列表，左列占 46%、右列占 54%；选中行以黄色文字和箭头突出，其余说明与图标为灰色，当前值显示“（当前）”，输入框使用紫色分隔线。最多显示八行，窄屏截断说明，短屏随选择滚动。↑/↓、Home/End、PgUp/PgDn 导航且不循环，Tab 补全或打开目录但不提交，Enter 确认当前项；Esc 收起列表并保留草稿，下次编辑时重新显示。输入 `/` 展开命令菜单，继续输入会按命令前缀筛选，左列保留命令图标。
- 输入 `/model `（命令后加空格）会在输入框下方展开与 `/effort ` 一致的两列菜单：左侧为 `provider/model`，右侧为实际模型名、`completion`/`response` 及“当前”标记，黄色文字和箭头突出选中项。`/model` 后按 Enter 也能展开，默认选中当前模型。按启动配置顺序排列，最多显示八行，长列表随选择滚动；↑/↓、Home/End、PgUp/PgDn 导航，Enter 确认，Esc 收起并保留输入。继续输入按引用或实际模型名的前缀筛选，Tab 补全引用但不切换；多行粘贴仍作为普通任务。后台完成切换后才更新状态栏，历史和确认后新编辑的草稿均保留；忙碌时拒绝切换、不排队、不清空草稿。`/new` 仍在后台确认后清空历史和草稿。
- 运行中按无修饰键的 Esc 取消当前任务，不退出 pp；已有面板或菜单优先消费 Esc。收尾期间显示 `Cancelling...`，仍可编辑草稿、滚动、F1/help、Ctrl-O 或退出，但不能提交／排队新任务、切换模型／思考级别或 `/new`。收尾完成后保留草稿、光标、输入历史、已完成的工具结果和文件修改，并追加 `Task cancelled. File changes were not rolled back.`；不重试、不回滚。每个任务使用独立取消 token，已提交的最终回答不会被迟到 Esc 改成取消。行模式和 `-p` 不提供取消入口。
- `/quit` 或 Ctrl-C 立即退出整个 pp，正常退出码为 0。恢复终端后，全部对话记录按退出时的终端宽度写入普通终端的滚动历史，保留原有颜色、缩进、分隔线、流式片段、工具输出及错误，末尾保留输入框、未提交的草稿、菜单和状态栏；可以向上滚动查看超出一屏的历史。保留长度受终端自身的滚动历史容量限制。退出不等待阻塞中的模型请求或工具，也不保证终止 bash 子进程；需要任务级受控清理时应先按 Esc 等待收尾。文件修改不会回滚。
- `-p/--prompt` runs one task and never parses slash commands: `pp -p '/model'` sends `/model` as a task. It can follow `-r ID` to append one task to saved history; combining it with `-c` is rejected. If either stdin or stdout is not a terminal, line mode supports the same commands without raw mode or an alternate screen. `/model` lists configured models and marks the current one; use `/model provider/model` to switch. Stdout streams **all readable assistant text**, including text before tools, with one terminating newline per nonempty turn and no replay at completion. Refusal is printed only if no non-blank content wins. Help, command responses, errors, and `tool <name>: running/ok/failed` go to stderr; live process data uses `[{name} stdout]` / `[{name} stderr]` on pp's stderr, with the actual `bash`, `grep`, or `find` tool name. Non-TUI output adds no ANSI control sequences. A display write failure is returned only after the Agent finishes recording the task's history/tool results.

## Markdown 提示词命令

将 UTF-8 Markdown 文件放在 `~/.pp/agent/commands/`，重启 pp 后即可在 TUI 和行模式使用 `/文件名 参数`。例如 `commands/review.md`：

```markdown
---
description: Review source
---
Review $1. Focus: $@[2:].
```

输入 `/review auth "error handling" tests`，实际发送正文为 `Review auth. Focus: error handling tests.`，并保留文件正文末尾的换行。TUI 的 `/rev` 菜单显示 `review` 和说明；Tab 只补全名称，Enter 提交，Esc 收起并保留草稿。`/help` 列出成功加载的命令与说明，不展示模板正文。命令带参数后不提供额外参数菜单。

- 仅扫描 `commands/` 的直接子项，不递归；只接受非隐藏、扩展名恰为 `.md` 的普通文件或指向普通文件的符号链接。目录自身也可为符号链接。名称取文件名去掉最后的 `.md`，大小写敏感，允许 Unicode 和标点，但不能含空白／控制字符。内置命令名（包括隐藏别名 `/think`）不能覆盖；重复名称保留按路径排序的首项，最终菜单／帮助按名称排序。
- 目录缺失时不创建目录，不报错。不可读文件、无效 UTF-8、断链、非法名称、内置冲突、未闭合／非法 YAML 和空白正文会被跳过，其他命令继续可用。行模式诊断以 `command:` 写入 stderr；TUI 启动显示扫描问题摘要，`/help` 可查看完整诊断。
- YAML frontmatter 可省略，支持文件开头 BOM 和 CRLF；只读取可选字符串 `description`，并从模型提示词中剥离整个 frontmatter。缺少、null 或空白说明时，使用正文第一条非空行的前 60 个 Unicode 字符，确有截断才追加 `...`；显式说明折叠连续空白。`name`、`argument-hint`、`model`、`allowed-tools` 等其他键均忽略，不改名、不切模型、不授权工具。
- 参数仅以空格和 tab 分隔；单／双引号分组并去掉定界符，同一参数内的引号段可与普通文字拼接。另一种引号和反斜杠为普通字符，没有转义；未闭合引号接受到末尾，空引用参数丢弃。聚合参数以单空格拼接。
- 支持 `$1`、`$2` 等任意连续数字的位置索引（从 1 开始），`$@`／`$ARGUMENTS` 表示聚合参数；`$@[start]` 和 `$@[start:]` 取从第 start 个到末尾，`$@[start:length]` 最多取 length 个。缺失参数、0 或越界索引、0 长度均为空；超大长度截到末尾。替换只扫描原正文一次，参数里的 `$1` 等不会再次展开。未知占位符原样保留；非法 slice 仍替换 `$@` 前缀，如 `$@[x]` 变成聚合参数加 `[x]`。`$ARGUMENTS` 不要求单词边界，`$` 没有转义语法。
- 正文没有已识别占位符且参数非空时，去掉正文末尾空白后追加一个空行和聚合参数；无参数则保留原正文。展开为空白时报本地错误，不发送模型或修改历史。正文即使为 `/quit` 也只是模型提示词，不会递归执行命令。`{{…}}` 保持字面文本；没有 Handlebars、脚本、环境变量展开或项目级命令发现。
- 每次交互启动只加载一次；修改、新增或删除模板需要重启，`/new` 不重载。运行中 Enter 不排队、不回显、不清草稿；配置不可用时仍可查看候选和帮助，但不能执行模板。未知命令仍是本地错误，多行输入仍作为普通任务；`-p` 不扫描命令目录，始终原样发送。
- 本次进程的 ↑ 输入历史保留原始 slash 输入；对话记录和 session 保存实际发送的展开正文。`-r` 恢复后的新命令使用本次启动的模板，旧历史不随文件修改而改变；`-c` 原样重放保存的展开内容，不重新展开。

## Session 持久化与恢复

TUI 空闲时提交 `/resume`，打开 omp 风格的全屏 `Resume session` 面板。默认只显示当前工作目录的会话，→/← 或 Tab 切换当前目录和全部项目。顶部输入搜索词，按标题、回答预览、ID 和目录筛选，支持粘贴、Backspace 删除字符和 Ctrl+U 清空查询。每条会话展示首条用户消息摘要、首条非空助手文本预览、相对更新时间、文件大小及 `current`／`✓ done`／`■ aborted` 标记。摘要折叠空白并最多保留 80 个 Unicode 字符，预览最多 200 个，超长追加 `…`。完成状态根据最后用户／助手记录及错误判断，不调用模型生成标题。

会话按文件最后修改时间倒序（相同时间按完整 ID 倒序），默认选中第一项。↑/↓、Home/End、PgUp/PgDn 导航，滚轮移动选择，Enter 恢复，Esc 取消。筛选或切换范围后重新选中第一项；窗口缩放保留选择。Delete 或空查询时的 Backspace 请求删除，面板内 Enter/y 确认、Esc/n 取消；当前会话及持锁会话不可删除。扫描、恢复和删除均在 worker 中执行。单个坏文件跳过并提示数量，无用户消息的日志不展示，选中时才做完整恢复校验。浏览和搜索不加恢复锁、不改变日志字节或 mtime。

加载时取消会保留旧界面，但扫描结束前 worker 仍忙碌，迟到回复不会重新打开列表；恢复或删除提交后不能取消或重复确认。选择当前 ID 仅关闭列表，不重加锁、不重放历史。

`/resume` 只恢复对话历史：按原始日志顺序替换可见对话、模型上下文和用户输入历史，保留当前模型、协议、思考级别、工具限制、edit 模式、工作目录及 MCP 连接。保存时的模型被删除或修改、原目录已不存在，也可以恢复；跨模型或跨协议时转换历史，去除绑定旧模型的 Responses 状态，同一模型则保留原生推理历史。后续输入继续追加原 JSONL，不创建分叉、不重跑旧工具。失败时仍可在旧会话继续输入。行模式仅提示 `/resume requires the TUI; use pp --resume SESSION_ID.`，不支持 `/resume ID`；按 ID 启动恢复仍使用启动参数。

| 命令 | 启动行为 | 任务后行为 |
|---|---|---|
| `pp` | 新的惰性 session，不请求模型 | 原有 TUI / 行模式 |
| `pp -p TEXT` | 发送一次新任务 | 退出 |
| `pp -r ID` | 精确恢复，不发送 HTTP | 等待输入；行模式可 EOF 退出 |
| `pp -r ID -p TEXT` | 恢复后追加一次 TEXT | 退出 |
| `pp -c` | 选择全局最近修改的 session，立即发送完整已保存请求 | 继续交互；行模式 stdin EOF 退出 |
| `pp -c ID` | 精确恢复并立即发送完整已保存请求 | 同上 |

`-c` 不附加“请继续”、不重复最后一条用户消息、不回滚助手回答，也不重放旧工具调用。只能使用完整 ID：`YYYY-MM-DDTHH-MM-SS-<32位小写hex>`，例如 `2026-10-17T01-02-03-0123456789abcdef0123456789abcdef`；日期时间使用创建时的本机时区，精确到秒，随机后缀避免同秒冲突。不支持前缀或文件路径，旧的 Unix 毫秒格式 ID 不再接受。`-r` 缺 ID、`-r` 与 `-c` 同用、`-c` 与 `-p` 同用均报错。

会话日志唯一位置为 `~/.pp/agent/sessions/<ID>.jsonl`。启动、草稿和本地命令不创建 session 目录或文件（运行诊断日志独立创建）；只有真实 HTTP stream 打开调用返回（收到 headers 或请求失败）后才创建。因此请求仍在等待首次 headers 时被强杀，不保证已有日志。主动 Esc 取消是例外：即使 headers 尚未到达或任务尚未发送 HTTP，也同步保存已接受的用户输入和取消终态，供后续恢复。ID 使用 16 字节 OS 随机数；新目录权限 0700，新文件 0600，不改已有目录权限。日志是敏感明文，包含完整对话和工具结果，不保存 Config、API key 或 HTTP headers；不要提交或随意共享。

启动参数 `-r` / `-c` 使用原工作目录、edit 模式、工具限制、模型引用、思考级别、原生 system/instructions 和完整历史，并继续追加原文件，不分叉。原目录缺失或不可进入时报错，不回退到启动目录。模型必须仍存在于当前 catalog 且实际 model/protocol 未改变；凭据、base URL、HTTP 超时和每任务请求上限使用当前配置。启动恢复失败是非零退出，不进入禁用模型的交互兜底；运行中的 `/resume` 恢复失败保留旧会话、工作目录与列表选择，不发送模型请求。

TUI 恢复可见消息、工具结果、错误和用户输入历史，输入框为空；不恢复草稿、菜单、滚动位置或布局缓存。恢复的 bash 卡片仅显示保存的退出状态，不伪造耗时。未完成展示事件显示 `Previous request was interrupted.`，不加入模型上下文。行模式不重印旧回答。恢复 ID 与首次创建 ID 分别通过 stderr / TUI Note 提示 `Resumed session: <ID>`、`Session: <ID>`；stdout 仍只有模型回答。

“最新”按文件修改时间选择，时间相同按 ID 字典序降序；只查看恢复不会提高排序，不按项目过滤。session 文件必须是普通文件，不跟随符号链接；进程持有排他锁，另一进程恢复同一 ID 会报 `session is already in use`。最新文件损坏或已锁时直接报错，不换另一个文件。

JSONL 使用 oh-my-pi 风格容器：首行是 `type:"session"`、`version:3` 的 header，保存现有完整 session ID、原工作目录和 UTC RFC3339 毫秒创建时间；其余条目都有 `id`、`parentId`、`timestamp`，形成线性链。正文 ID 是当前日志内唯一的 8 位小写随机 hex，首条 parent 为 null。支持的正文类型只有 `message`、`model_change`、`thinking_level_change`、`custom`；普通消息提供 `user`、`assistant`、`toolResult` 及 text/thinking/toolCall 内容，供通用阅读，不虚构 usage/cost。

pp 的无损恢复使用 `custom` 扩展：`pp.session_state`（独立原生版本 1）、`pp.native_append`、`pp.instructions` 保存协议原生请求及增量；`pp.event`、`pp.error` 保存展示事件与错误；`pp.cancelled`（`data:{}`）同步保存主动取消终态，恢复时显示取消 Note，不误报中断，也不进入模型上下文。取消后的列表状态为 aborted，后续真实任务完成可再次标为 done。普通 assistant/toolResult 消息不反推模型上下文，也不重复渲染；TUI 仍从独立展示事件恢复。`model_change` 仅作审计，后续完整 native state 才提交模型切换。这是结构对齐，不支持直接互读上游会话文件。旧 `checkpoint` 格式明确报 `unsupported legacy session format`，不迁移、不改写；最新文件为旧格式时也直接失败，不跳过。

首个 `pp.session_state`、用户原生输入、每个完整模型/工具批次及设置更改均同步落盘；流式展示事件不逐 token fsync。原生历史与展示日志分离，Responses item ID、encrypted reasoning、call_id 及工具原始结果均保留。完整工具批次在下一次请求前提交；网络错误不撤销已执行工具。写入失败锁存首次 `persisting session` 错误，当前已接受工具批次仍执行完毕，停止后续 HTTP；`/new` 或重启后才可重新发送。模型/思考级别设置写入失败保留旧内存选择。

仅允许忽略末尾未换行的残缺记录；恢复时提示，首次真正追加前持锁截去残尾。完整坏行、未知版本/记录、ID 不符或协议不符均报错，不覆盖旧日志。正常退出和完整批次可恢复；强杀期间的未完成回答不提交到模型历史，工具副作用不提供事务、回滚或 exactly-once 保证。

`/new` 保持当前模型和思考级别，清空原生历史并释放旧日志锁，旧文件保留；下次真实模型请求才创建新 ID。

## Skill 支持

配置中的多个 `skill_dirs` 按顺序递归扫描，每个 skill 是包含 `SKILL.md` 的目录；发现该文件后不再扫描其附属目录。支持符号链接，并按规范路径去重、防止循环；同名定义按目录顺序及路径排序取首个，其他定义显示冲突提示。缺失目录、无效文件和元数据错误会显示诊断并跳过，不影响其他 skill。

```markdown
---
name: rust
description: Rust 开发与审查指导
---
在修改前阅读相关实现，并运行与修改范围匹配的检查。
```

`name` 和 `description` 必须为非空字符串，支持 YAML 多行说明和其他元数据。名称作为全局启停标识，需要唯一。默认根目录为 `~/.pp/agent/skills`；`~/` 展开为用户目录，普通相对路径以 `~/.pp/agent` 为基准。

TUI 输入 `/skills` 打开全局管理面板：↑/↓ 选择、Space 勾选、Enter 保存、Esc 放弃本次选择，r 重读目录配置并扫描（会重置未保存选择），PgUp/PgDn 滚动详情及诊断。面板显示名称、说明、来源和启用状态；配置中的目录通过编辑 settings 修改。保存仅更新 `disabled_skills`，保留其他字段和 JSONC 注释，使用临时文件原子提交；失败保留原运行状态与面板选择。运行任务时拒绝管理并保留草稿；缺少或无效模型配置时仍可管理 skill。行模式支持 `/skills` 查看列表，以及 `/skills enable 名称`、`/skills disable 名称`。

模型只收到已启用 skill 的名称与说明；通过 `load_skill({"name":"rust"})` 按需读取全文，结果包含 `content`、`path` 和资源基准 `base_dir`。加载工具只接受已发现且启用的名称，不会执行 skill 内的脚本。TUI 用品红色完整边框显示加载过程，左上角加粗标题为 `skill · 名称`，并显示来源及成功／失败状态。正文默认预览前 10 行，Ctrl-O 展开／收起最近的 bash 或 skill 卡片；模型接收全文，退出回放也保留全文。

启停和重扫对后续请求生效，跨模型及协议切换保留。已加载的内容仍在当前会话历史中，`/new` 清除会话历史并保留全局启用选择。配置不会被其他运行实例自动刷新，需要重扫或重启。skill 加载日志写入 stderr，行模式 stdout 仍只有助手输出。

## Execution and Permissions

**write/edit 仅修改启动目录内文件，按单次调用原子提交。macOS/Linux 的 bash、grep、find 均无文件权限隔离，仍可修改当前用户有权限访问的目录外文件；没有逐次确认。请在可信或临时目录中运行。**

- 启动时固定规范化工作目录。`read` 允许目录外读取及符号链接；`write/edit` 接受目录内的相对或绝对路径，拒绝越界路径以及写入路径上的符号链接（包括指向项目内的链接）。父目录通过目录句柄逐级打开，保存不重新按完整路径追踪链接。允许不越出工作目录的 `..`，不自动扫描项目规则文件；skill 仅扫描全局配置指定的目录。
- `read(path, offset?, limit?)`：`offset` 是从 1 开始的起始行，默认 1；`limit` 默认 `read_max_lines`，必须为正整数且不超过该配置。读取普通文件，使用有界缓冲跳过前面的行；每页同时受 `read_max_bytes` 和行数限制。返回 `ok`、`content`、`truncated`、`lines_read`、`next_offset`；有后续内容时使用 `next_offset` 继续，文件结尾为 null。超出文件末尾返回空页。两种模式均只返回完整行，超长单行无法放入空页时返回错误；增大配置或使用 bash 检查有界片段。默认 replace 返回保留原始换行、无前缀的 lossy UTF-8；字节上限计算源文件内容，不包括 hashline 前缀及 JSON 编码。不会解析图片或二进制格式。
- Default `edit({path, edits:[{oldText,newText}]})`: changes an existing UTF-8 file. Every item matches the same original snapshot, not earlier replacements. Nonempty `oldText` must be unique even after loose normalization; overlapping or duplicate targets are rejected, adjacent targets are allowed. Use minimal unique context; insert by repeating neighboring context in `newText`, or delete with `newText:""`. Empty arrays, missing matches, and whole-batch no-ops fail without writing; mixed no-op and changing items are allowed. On failure, read again.
- Replace matching tries exact LF-normalized text first, then NFKC → per-line ECMAScript `trimEnd` → smart quote/dash/Unicode-space normalization. It does not ignore case, indentation, or repeated inline spaces. If loose matching is needed, touched line blocks use normalized text while untouched line bodies retain their original content by line number. A leading UTF-8 BOM is preserved; CRLF and bare CR are normalized internally, then all output newlines use the source's first LF style (CRLF if preceded by CR, otherwise LF; LF when absent). No final newline is forced.
- 设置 `"edit":"hashline"` 后，read 显示 `N:hash|text`：N 为从 1 开始的真实文件行号，分页不重新编号；hash 为原始行正文（不含 LF/CRLF）的 16 位小写十六进制 FNV-1a 64 位哈希。显示分隔符为 LF，只为完整行生成锚点，空文件没有锚点。edit 仅接受带锚点的 `replace`（包含首尾行，`lines:[]` 表示删除）、`insert_before`、`insert_after`。新行字符串不能包含 CR/LF 或显示前缀。操作均引用原始快照；拒绝重叠替换、重复插入位置及触及替换边界的插入。锚点失效时重新读取；只检查端点，不校验内部行或整文件版本。未触及的字节保持原样，新分隔符采用源文件首个换行风格，并保留被替换末行是否带换行的状态。
- 两种 edit 模式均先对同一份原始快照校验全部修改，再调用共用原子保存流程。空文件、缺失文件、目录、符号链接和非 UTF-8 文件不能编辑；edit 不创建文件或目录。成功返回 `edits_applied` 和最终 UTF-8 `bytes_written`。保留严格且分模式的参数校验，不增加 diff/patch 字段。
- `write(path, content)`：创建缺失父目录，并创建或完整覆盖 UTF-8 文件，允许空内容。write/edit 均在目标所在目录排他创建临时文件，完整写入、保留原文件权限（含可执行位）、同步临时文件后，以一次原子重命名提交。创建、部分写入、同步或重命名失败均保留原文件；新建失败不会留下残缺目标，已创建的空父目录允许保留。重命名是提交点，之后父目录同步失败返回 `ok:true` 并附 `warning`，不能当作尚未修改。失败路径清理临时文件；不提供成功后的撤销、备份、跨文件事务、跨进程并发隔离或断电恢复保证。
- `grep({"args":[...]})` 搜索文件内容，`find({"args":[...]})` 查找路径。每个 Tools 实例构造时按 PATH 固定选择 `rg → grep`、`fd → find`，工具 description 根据实际后端提供原生参数语法；优先程序可位于后面的 PATH 项。相对／空 PATH 项相对启动目录解析，只选择普通文件且有执行位的候选（允许可执行 symlink）。不运行版本探测，不自动安装，不使用 `fdfind` 别名；运行时程序删除或权限变化返回真实错误，不拿相同 argv 静默换后端。新启动／恢复构造的 Tools 会重新探测，模型切换和历史恢复通过实时 schema 提供当前后端说明。
- 搜索只接受必填的字符串数组 `args`，不传程序名，不拼接 shell、不做展开／管道或跨后端参数转换；空数组、空字符串原样传给程序，NUL 和额外字段被拒绝。rg 示例：`{"args":["-n","--color","never","-e","PATTERN","src"]}`；system grep 示例：`{"args":["-R","-n","-E","-e","PATTERN","--","src"]}`；fd 示例：`{"args":["--color","never","--type","f","PATTERN","src"]}`；system find 示例：`{"args":["src","-type","f","-name","*.rs","-print"]}`。rg/fd 默认遵守 ignore 且跳过隐藏文件，可用 `--hidden`／`--no-ignore` 扩大范围；系统 grep/find 不自动遵守 gitignore，不能照搬快速后端的 flags。find 的路径在表达式前，`-name` 的 glob 保持单个 argv；fd 的 pattern 在目录前，默认正则，`--glob` 改用 glob。
- 搜索复用 bash 的受控执行层：启动 cwd、关闭 stdin、独立进程组、超时／取消／正常结束后的组清理、项目内 scratch/cache、UTF-8 流式输出与下述 500 行／每流 65,536 字节限额均相同，超时沿用 `bash_timeout_seconds`。结果保留 `ok`、`exit_code`、`timed_out`、`cancelled`、两流输出及截断标记；grep 正常 exit 1 表示无匹配，返回 `ok:true` 并保留 `exit_code:1`，其他非零退出、find 非零退出、超时／取消均失败。后端缺失不阻断启动和文件工具，schema 仍存在并标明 unavailable，调用返回 `grep requires rg or grep on PATH` 或 `find requires fd or find on PATH`。原生参数可产生副作用（如 find 的 `-exec`／`-delete`），没有参数白名单或文件系统沙箱。
- `bash(command)`：以启动目录为工作目录执行 `bash -c`，关闭 stdin，并建立独立进程组。stdout/stderr 无需等待换行或进程退出即可实时显示；保留跨分块的 UTF-8 字符，无效字节采用 lossy 解码。两流合计最多保留 500 行，同时每流最多保留 65,536 个原始字节，先达到的限制生效；按 `\n` 分行，非空且没有末尾换行的片段也计一行。仅在确有超出内容时标记对应流截断；实时显示与返回输出使用相同限制，多余内容持续排空，命令继续执行。仅保证各流内部顺序，不保证跨流顺序；500 行额度按接收分块的顺序分配。返回退出码、输出、截断标记和 `timed_out`，信号退出时退出码为 null。
- Arguments are strictly validated: missing or extra fields, invalid JSON, blank path/command, unknown tools, and file/process errors all return `ok: false` to the model, allowing it to correct itself. A non-zero bash exit likewise returns `ok: false` with the output preserved.
- bash 的 `bash_timeout_seconds` 默认 120，与模型请求超时独立，模型不能通过工具参数延长。独立监督线程在超时时向进程组发送 TERM，最多等待 1 秒后发送 KILL 并回收直接子进程；可停止的非阻塞管道读取避免后代持有管道导致无限等待。超时返回 `ok:false, timed_out:true`，保留已捕获输出，不回滚 shell 修改。正常结束也清理同一进程组的残留后台任务；主动脱离进程组的进程不受保证，仍只支持有限前台命令。TUI Esc 取消同样通过 TERM→1 秒宽限→KILL／wait 停止受控进程组，返回 `ok:false, cancelled:true, timed_out:false` 并保留输出；正常／超时结果的 `cancelled` 为 false。Ctrl-C 仍立即退出整个 pp，不提供 bash 子进程清理保证。
- bash 默认将 `TMPDIR`、`TMP`、`TEMP` 设置为项目内每次调用独立的 `.pp-tmp-*` 目录，结束后清理；`XDG_CACHE_HOME` 设置为项目内 `.pp-cache` 并保留缓存，二者均已加入忽略规则。这是默认位置而非权限隔离；不遵守这些变量的程序仍可能使用其他位置。清理失败通过结果中的 `warning` 报告。
- Esc 取消会关闭本地模型连接及 SDK 后台网络任务；下一任务惰性创建新的 runtime／HTTP 连接池。流式半成品只保留展示，不进入原生历史或执行未完成工具调用。已开始的同步 read/edit/write/load_skill 操作允许完成并保留真实结果；同批剩余调用不执行，但仍按 call_id 追加取消结果。只有终止／回收成功且原生批次与终态同步保存成功才显示取消；真实清理失败或持久化错误优先报告错误。只能保证本地连接及同进程组受控工作停止，不能撤销供应商远端计算、计费、外部副作用或主动脱离进程组的进程。
- Tools execute serially; dependencies within a batch such as write → read become visible in order. The complete model output is saved first, then tool results are appended one by one with matching call IDs; a tool failure never skips the remaining calls in the batch.
- `read_max_bytes`、`read_max_lines`、`bash_timeout_seconds` 均要求正整数，启动时读取并固定；`/model`、`/new` 不重新读取或放宽这些限制。
- Each user task allows at most `max_model_requests` (default 20) model requests. The final batch at the limit still executes and returns into in-memory history before the limit error is raised; no next request is sent and no completion is claimed. Each new task restarts the count.
- The full native session is kept without context trimming and persisted as JSONL after an actual model request attempt. Network/protocol errors do not roll back real file changes and never retry automatically; previously completed history and tool results remain recoverable. See session recovery below.
- `/model` selects only from the fully validated startup catalog; it neither reloads nor writes configuration. Selecting the current reference is a no-op with confirmation. Failed resolution/client initialization/history conversion preserves the current model and history. Switching does not probe HTTP, execute tools, retry, or fall back; service compatibility is checked by the next actual request.
- Switching preserves user/assistant text, system instructions, tool-call IDs/arguments, and tool results. Completion-to-Completion keeps native messages; Responses within the same configured provider keeps native output, including encrypted reasoning. Cross-protocol or cross-provider Responses switching converts readable history and strips provider item IDs, replay metadata, and opaque encrypted reasoning. Readable reasoning summaries/content become assistant text; interleaved Responses assistant messages/calls are batched for Completion. Unsupported non-text or unknown history aborts the switch rather than losing records. Encrypted vendor state is not portable.

取消回归通过真实 loopback HTTP、PTY 和进程组验证：两协议 headers 前／SSE 空闲取消在下一任务前关闭旧连接，write→bash→write 保留已完成结果并补齐未执行调用，MCP 仅在下一任务调用时重建并拒绝 schema 变化，重建中 Ctrl-C 仍回收替换进程；取消终态可恢复为 aborted，后续完成可变为 done，持久化失败不误报取消。独立 100×30 实际 TUI smoke 使用临时 Python loopback fixture：TERM-resistant bash 两个受控 PID 均停止，中文草稿和光标保留，下一任务收到 `cancelled:true, timed_out:false` 与原 stdout 并回复 `smoke recovered`，共两次 HTTP，最后 Ctrl-C 以 0 退出并恢复终端属性。未使用真实供应商，Linux 未在本次环境实机验证。

## HTTP Protocol

`async-openai` creates and consumes typed SSE streams for both APIs; streaming is the only execution path, with no non-streaming fallback or extra request on failure. A Tokio runtime bridges SDK calls to the synchronous Agent and TUI worker; model requests never run on the TUI drawing thread. Each request uses `request_timeout_seconds` (default 120); automatic redirects and retries are forbidden. SDK environment-derived credentials, API base, organization, and project are explicitly overridden; configuration remains file-only.

- **`response` (default):** sends `model`, `instructions`, full local `input` history, function `tools`, `tool_choice: auto`, `stream: true`, `store: false`, and `include: ["reasoning.encrypted_content"]`. It does not use `previous_response_id` or server-side conversations. 正文和拒绝文本按输出项及内容顺序显示；可读思考摘要和思考文本默认单独显示，摘要的 delta、done 和 completed 不重复输出。参数分片不显示，加密思考内容不显示。 `response.completed.response` is the sole source of committed output and calls; it releases the turn immediately without waiting for HTTP EOF or `[DONE]`. Response/message/reasoning/function-call statuses must satisfy completed validation. Message, reasoning (including encrypted content), and calls are retained in native order; results use matching `call_id`. `/new` clears input and retains instructions.
- **`completion`:** sends `model`, `messages`, function `tools`, `tool_choice: auto`, and `stream: true`. Chunks use the SDK schema (`id`, `object`, `created`, `model`, indexed choices); only choice index 0 is consumed, with usage-only empty choices ignored. An actual assistant role and a `stop` or `tool_calls` finish reason are required. Tool fragments are accumulated by index and validated only after the stream ends without error. Truncation, filtering, and EOF before finish are failures. The SDK consumes `[DONE]` internally; a legal finish plus clean stream termination is accepted even if the server omitted `[DONE]`. `/new` keeps only the system message.

Both APIs validate the entire batch before saving assistant output or executing tools: only function calls with non-blank IDs and names are accepted, and call IDs cannot repeat within a batch. A bad batch never executes even valid writes from that response. Neither readable text nor calls is a protocol error. All text parts are concatenated even in tool turns; content takes priority over refusal. Final Responses text may add a missing suffix, but conflicting streamed non-blank content fails the turn before tool execution. A broken stream retains visible partial text, marks the TUI entry failed, and reports a redacted error without committing incomplete assistant/tool history; the submitted user message and earlier completed tools remain. Tool arguments decode at execution time per tool type. Legacy Chat `function_call` 和托管工具仍不支持；Chat 额外读取 choice 0 的 `delta.reasoning_content` 或 `delta.reasoning` 字符串，仅用于显示，不写入回答正文。 Errors never dump provider messages, response bodies, or keys.

### Responses 故障排查

- 部分模型始终启用思考，不接受 `reasoning.effort: "none"`。旧配置 `"think_level": "default"` 在当前版本是 `none` 的别名，**不是服务端默认值**。可在该模型配置 `thinking.efforts` 排除 `none`，并用 `thinking.defaultLevel` 指定支持的启动档位。若服务端仍拒绝请求，在交互模式执行允许的 `/effort low` 后重新提交任务；启动默认值在 `~/.pp/agent/settings.jsonc` 中改为 `"think_level": "low"`，或选择服务端支持的其他档位。已恢复的 session 保留其保存的档位，需要执行 `/effort low`；仅修改启动设置不会覆盖已恢复的档位。
- 流式终止错误区分 `response.failed`、`response.incomplete` 与 `error`，仅展示白名单内的错误码／未完成原因；服务端原始 message、param、未知 code/reason 均不回显。`invalid_request` 提示检查模型支持的思考档位和请求参数；`max_output_tokens` 表示输出截断，`content_filter` 表示内容过滤。失败不自动重试、不降级协议、不提交未完成的回答，也不执行该批次工具。

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
cargo test --locked --test cli_agent session:: -- --nocapture
cargo test --locked --test cli_agent tui_session_ -- --nocapture
cargo test --locked slash_
cargo test --locked file_command_ -- --nocapture
cargo test --locked --test cli_agent tui::tui_slash_file_command_menu_and_submission -- --nocapture
cargo test --locked --test cli_agent session::session_file_command_expansion_survives_resume -- --nocapture
cargo test --locked model_switch
cargo test --locked model_thinking -- --nocapture
cargo test --locked llm::
cargo test --locked --test cli_agent default_reasoning_rejection_allows_manual_effort_recovery_without_committing_failure
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

新增搜索工具另经真实构建产物与临时 loopback provider 验证：全后端、仅系统 fallback、rg/find、grep/fd、两组均缺失五种 PATH 组合分别运行 Completion/Responses，确认动态 description、原生命令结果、无匹配成功、非法正则 stderr、带空格路径与 CLI 标签一致。另两次 bash smoke 保留原标签、500 行截断及超时行为。实际 100×30 PTY 观察到 grep 青色／find 普通卡片、紧凑 JSON argv 标题、结束前实时输出及非法正则失败状态；Ctrl-C 退出为 0 并恢复 termios。全部使用隔离 HOME／配置／工作目录，未访问个人凭证；临时脚本已删除。

回归测试使用回环 HTTP、真实子进程、独立临时 HOME 下的全局 JSONC 配置及临时工作目录； they do not modify the test process environment or access external model services. Both APIs cover ordered tool dependencies, cross-turn history, tool-error correction, invalid batches without partial writes, request limits, timeouts, error redaction, and no automatic retries. Responses-specific cases cover omitted/explicit protocol selection, text aggregation/refusal, encrypted reasoning and complete output replay, optional function-call status, reset preserving instructions, rejected response/item statuses, and recovery without invalid output entering history. Configuration cases cover composite provider/model routing, default ordering, model aliases containing `/`, scoped uniqueness, strict protocol/URL/field types, full validation of unselected providers, removed protocol rejection, and no environment fallback. HTTP regressions include non-JSON 302/401/429/500 errors without redirects or exposed bodies.

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

Session 新格式已在 macOS arm64 验证：13 项 session 行为测试、3 项专用真实 PTY 测试、9 项 persistence 单测通过；`cargo test --locked -- --test-threads=1` 全套 236 项通过，`cargo build --locked` 通过。默认并发全套曾有 1 项 session 惰性创建用例在 5 秒内未收到 CLI 请求；串行全套通过，未放宽 deadline 或断言。覆盖原工作目录、保存的 edit/read/bash 限制优先级、完整原生上下文、链/版本/旧格式拒绝、残尾/锁/符号链接边界，以及真实文件大小限制注入的持久化失败。PTY 还覆盖未结束 SSE 文本 delta 后强杀并 `-r`：保留部分文本、显示中断提示、无恢复 HTTP、无部分原生 assistant 提交。Linux 尚未实机验证。

新格式另经一次性 Python 标准库 SSE 服务与构建后的 CLI 独立冒烟：Completion/Responses 分别使用隔离 HOME、项目 A 与启动目录 B，验证 HTTP 打开前不创建 session、stdout 只有回答、header/version 3 与线性链正确、普通消息内容正确；`-r` 无 HTTP 且日志字节不变，`-c` 精确发送保存的原生请求、不加用户消息、保留首次 assistant，并以原 cwd 连接旧链追加同一个文件。每个协议共 2 次 HTTP。此验证不代表真实供应商兼容性。

日期时间格式 ID 已通过独立 CLI 冒烟：临时 HOME 与本地 SSE 服务下分别设置 `TZ=UTC`、`TZ=Asia/Shanghai`，确认 ID 时间与 checkpoint 创建时间一致、文件名与记录 ID 一致，并可用完整 ID 执行 `-r`（无 HTTP）和 `-c`（继续原会话）。

Session recovery was independently smoke-tested against a disposable Python standard-library SSE fixture and the built `target/debug/pp`, using isolated HOME, project A, and launch directory B. For both protocols: empty startup left no sessions directory; `-p` returned `ack smoke-marker` and an ID; `-r` in a real 100×30 PTY showed saved dialogue without HTTP, then retained it on new input; `-c` sent the exact fully reconstructed native request without a keypress or extra user item. A new write tool created its proof file only in A, not B. JSONL first/final records parsed successfully, all successful process exits were 0, and termios/alternate-screen/paste/cursor restoration was observed. Responses preserved the opaque reasoning field and item ID. A separate restored start-only event rendered `Previous request was interrupted.` with zero HTTP. Temporary fixture files were removed; no real HOME or provider was accessed.

当前配置的 `glm-5.3-flash` 已通过真实 Responses CLI 冒烟：`think_level: low`、临时 HOME 和工作目录、无工具调用，回答 `OK`、退出码 0；同一服务端明确拒绝 `none`。这不代表其他模型、档位或真实工具调用已验证。After filling in a real vendor configuration supporting function tools and selecting its API via `protocol`, 将全局配置复制到临时 HOME 中运行（配置文件仅从 HOME 下的 `.pp/agent/` 读取）：

```sh
BIN="$PWD/target/debug/pp"
WORK="$(mktemp -d)"
mkdir -p "$WORK/home/.pp/agent"
cp ~/.pp/agent/settings.jsonc ~/.pp/agent/models.jsonc "$WORK/home/.pp/agent/"
(
  cd "$WORK" || exit 1
  export HOME="$WORK/home"
  "$BIN" -p 'Create hello.txt containing "hello from rust agent" plus a newline; read it, verify the line count with wc -l via bash, then report the file content and line count.'
  cat hello.txt
  wc -l < hello.txt
  "$BIN"
)
```

The expected file contains `hello from rust agent` plus a newline, one line. In the interactive session, first ask for the file content, then follow up asking for the file name, checking context continuity; keep the temporary directory for inspection.

References: [Pi](https://github.com/badlogic/pi-mono), [tool loop](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md), [async-openai](https://github.com/64bit/async-openai), [Responses](https://platform.openai.com/docs/api-reference/responses), [Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create). Vendor-specific extension protocols and upstream session files are unsupported.
