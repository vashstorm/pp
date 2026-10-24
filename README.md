# pp

A minimal Pi-style coding agent in Rust: OpenAI-compatible Responses and Chat Completions via `async-openai`, real read/grep/find/edit/write/bash tools, and multi-turn terminal interaction. Single crate, Rust Edition 2024, macOS/Linux supported; bash must be on the PATH.

## 请求统计

所有新发起的模型调用都会自动记录到 `~/.pp/stats/stats.db`，包括工具循环及手动／自动摘要；统计页面是否运行不影响采集。不会导入此前的 pp 或 omp 日志。

```sh
pp stats
pp stats --port 4000
```

默认访问 [http://127.0.0.1:3848/#/overview?range=24h](http://127.0.0.1:3848/#/overview?range=24h)。服务在前台运行，打印网址而不自动打开浏览器，Ctrl+C 退出；无需模型配置。`stats` 与 `-p`、`--resume`、`--continue` 互斥，`--port 0` 可让系统分配可用端口。服务仅监听本机回环地址。

Overview 展示当前时间范围内的请求、Token、缓存命中和耗时；右上角支持 1h、24h、7d、30d、90d、All。All requests 跳到保留范围的 Requests，点击请求行可查看详情、展开及复制 JSON。Requests 默认加载最新 500 条、每次显示 100 条，可扩大为最新 2,000 条；摘要针对已加载集合，搜索／状态筛选及排序也只作用于此集合。Models 展示模型请求占比、费用、缓存率、错误率、吞吐和 TTFT；点击行可展开效率、延迟、Token 及可切换的性能趋势。Providers 展示供应商总量、可展开的 Token／模型明细、可切换指标的消耗趋势和本地小时峰值。Costs 展示每日费用（按模型／计费项切换）、费用构成和可展开的模型单价明细。三个页面使用范围内全部请求，图例可点击隐藏／恢复系列，自动刷新保留展开状态。

Errors 按归一化的错误签名聚合失败请求，签名可展开查看完整消息并筛选失败列表，模型支持交叉筛选；点击失败行打开原有请求详情。

各页表格、趋势线、柱状／堆叠图、Token／费用构成及会话时间线支持悬停详情；表格单元格和图表时间桶也可通过键盘聚焦查看，Esc 隐藏提示。详情保留完整数值、时间或路径，堆叠图展示当前可见系列及合计，零值时间桶也可查看；切换页面、滚动或刷新会关闭旧提示。

Traces 按会话展示请求数量、模型、失败数、Token 和费用，点击会话查看完整已记录会话的请求时间线；时间线色块与请求项均可点击展开，正文、思考、工具参数、原生输出及会话条目可继续逐项展开，支持全部展开／收起。请求详情的 Trace 链接可直接定位并展开对应请求。Errors 汇总覆盖范围内全部失败，Traces 列表指标覆盖范围内请求；两者默认加载最新 500 条失败／会话，可扩大为 2,000，搜索只作用于已加载列表。单个会话详情不受列表范围限制，自动刷新保留展开状态。无会话记录的请求独立展示，不合并为同一会话。Traces 仅使用统计库快照，不读取原始会话文件；用户输入、工具结果／耗时及子代理跨度未采集，页面不推算这些数据。Tools 和 Projects 提供独立统计页面（见下）；Frustration 和 Gain 暂为占位。支持明暗／系统主题、数字 1–6 切换范围、G 加导航字母跳转。页面每 5 秒刷新，点击 Live 立即刷新。

Tools 展示模型发出的工具调用数量、趋势、参数字符数及归属 Token／费用；工具行和模型行可逐层展开，最近调用请求可打开原有详情，也支持搜索、按工具筛选和图例开关。调用优先从已提交会话快照提取，缺失时回退到 Completion／Responses 原生输出，同一请求不重复计数。每次模型请求的 Token／费用均分到该请求发出的全部工具调用，仅表示归属估算，不是工具独立费用。失败／取消请求的已保存调用可能不完整或未执行；工具执行结果、错误率和耗时仍为未采集。

Projects 按完整 cwd 聚合，保留同名不同路径的独立目录，展示费用／请求排行、搜索和排序；点击目录行或排行可展开 Token、会话计数、模型用量和范围内最近 10 条请求，模型行还可继续展开延迟和 Token 明细。两个页面的汇总覆盖范围内全部请求，自动刷新保留搜索、筛选及展开状态，行支持 Enter／空格展开；请求正文和工具参数原文只在原有请求详情中按需加载。

统计口径：以请求开始时间筛选，UTC 毫秒落盘、本地时间展示；总输入 Token 已包含缓存读取和写入，Conversation tokens 为总输入加输出，Uncached input 为总输入减缓存读取和写入。缺失的 usage／缓存明细保持未知，不用估算值填充。TTFT 是发起调用至首个非空正文、思考、拒绝或工具参数增量的时间；Tokens/s 为输出 Token 除以请求总耗时。用户取消记为 Aborted，服务或协议错误记为 Failed；发送前取消和本地校验失败不计入。

缓存写入从 Chat 的 `usage.prompt_tokens_details.cache_write_tokens`、Responses 的 `usage.input_tokens_details.cache_write_tokens` 采集，也兼容 usage 顶层的 `cache_write_tokens`／`cache_creation_input_tokens` 扩展；遵循这两种协议的总输入已包含缓存读写的口径，不额外叠加缓存 Token。不支持原生 Anthropic Messages 协议。未上报写入量保持未知，明确上报 `0` 才表示无写入。SQLite 自动升级到 v2，旧记录的写入量保留 NULL，不回填估算。Premium requests 和订阅额度没有采集数据，保持未知。

费用直接使用 `~/.pp/agent/models.yml` 中各模型的 `cost` 配置，按调用时保存的 `model_reference`（`provider/model` 配置引用，并非供应商模型名）精确匹配。无需另建价格文件。`cost` 可省略，省略时该模型未定价；配置时 `input`、`cacheRead`、`output` 必填，新增 `cacheWrite` 可省略以兼容旧配置；所有已配置单价都必须为非负有限数，单位为 **USD／百万 Token**，`0` 表示已知免费。例如在 `providers.<provider_id>.models.<model_id>` 的模型条目中填写：

```yaml
model: vendor-model-one
cost:
  input: 2.0
  cacheRead: 0.2
  cacheWrite: 2.5
  output: 8.0
```

费用公式为 `(总输入 - 缓存读取 - 缓存写入) × cost.input + 缓存读取 × cost.cacheRead + 缓存写入 × cost.cacheWrite + 输出 × cost.output`，最后除以一百万。每次统计查询重新读取模型配置，修改后历史估算按当前单价重算，不代表历史账单或订阅实际支出。缺少价格、完整 usage 或缓存读写数量，以及缓存读写之和超过总输入等异常的请求都计入 Unpriced 并从估算中排除；写入量大于 0 时必须配置 `cacheWrite`；明确为 0 时无需写入单价。旧数据写入量未知也计入 Unpriced。全未定价显示 `—`，混合数据只合计已定价部分并标示覆盖数量。空范围的费用为 `$0`。模型配置不存在时全部未定价；文件损坏、字段错误或无法读取时，页面提示问题并保留其余统计，不返回配置原文或密钥。统计服务仍可在没有模型配置时启动。Overview、Requests 及请求详情沿用相同计价规则。每日费用按 UTC 日聚合，Average per day 以有请求的 UTC 日数为分母。模型按完整配置引用分组，同名跨供应商或同供应商不同别名不会混在一起。

数据库保存模型输出（含可获得的思考、工具参数）、请求指标和已提交的对应会话条目快照；不额外保存完整输入上下文、请求头或认证配置。摘要和未提交输出没有会话条目时明确显示未记录。统计文件可能包含代码和模型输出，应按个人会话数据保管；不会自动清理，可运行 `pp gc` 删除超过 90 天的记录。目录权限为 `0700`、数据库为 `0600`，SQLite 使用 WAL 支持多进程写入；存储不可用只提示，不中断模型任务。

前端及 Inter 字体内嵌在二进制中，运行无需 Node 或 CDN；字体许可证见 `src/stats/fonts/LICENSE.txt`。只读接口为 `/api/stats/overview?range=24h`、`/api/stats/requests?range=24h&limit=500`、`/api/stats/models?range=24h`、`/api/stats/providers?range=24h`、`/api/stats/costs?range=24h`、`/api/stats/tools?range=24h`、`/api/stats/projects?range=24h`、`/api/stats/errors?range=24h&limit=500`、`/api/stats/traces?range=24h&limit=500`、`/api/stats/traces/{request_id}` 和 `/api/stats/requests/{id}`；详情正文按需加载，不在列表接口中返回。

## 清理过期数据

```sh
pp gc
```

无需模型配置，手动清理超过 **90 天**的数据。输出分为 `Logs`（日志文件数）和 `SQLite requests`（请求记录数）两行，每行报告清理前总量 `total`、本次清理数量 `removed` 和本批次剩余数量 `remaining`（`total - removed`）。日志总量仅统计下述三类普通文件，不含 `.lock`、无关文件、目录或符号链接；SQLite 按请求计数，关联快照不重复计数。不存在的存储各项均显示 `0`。

清理范围：

- `~/.pp/logs/`：仅删除严格匹配 `pp.yyyy-mm-dd.number.log`、`debug.yyyy-mm-dd.xxxx`、`prompt-debug.SESSION_ID` 的过期普通文件，按文件名中的本地日期判断，不使用 mtime；保留截止日期当天和未来日期，跳过其他文件名、符号链接和目录。使用日志已有的 `.lock` 跨进程锁。
- `~/.pp/stats/stats.db`：按请求开始时间（UTC 毫秒）删除早于执行时刻减 90 天的请求，在同一事务中先删除关联输出及会话条目快照，再删除请求；发生删除后执行 `VACUUM` 回收数据库空间。数据库或日志目录不存在时跳过，不创建空存储。

不会删除 `~/.pp/agent/sessions/` 中的原始会话或任何配置。操作不可撤销；清理后统计页面不再包含这些请求。数据库写锁竞争或文件清理失败会返回非零退出码；数据库与日志清理不属于同一个事务，已完成的清理不会回滚。`gc` 与 `-p`、`--resume`、`--continue` 互斥。

普通、debug 和 prompt-debug 日志均不自动删除；启动、写入和跨日只负责追加或分卷。仅显式运行 `pp gc` 时清理超过 **90 天**的数据，保留截止日期当天的日志。`pp gc` 不初始化运行诊断日志，不会为本次命令生成新日志或影响清理总量。

## Build and Run

```sh
cargo build --locked
./target/debug/pp --help
./target/debug/pp --version
```

发布构建使用体积优化配置，生成 `target/release/pp`：

```sh
cargo build --release --locked
./target/release/pp --help
```

`release` 使用 `opt-level = "z"`、完整 LTO、单代码生成单元并剥离符号。此配置优先减小可执行文件体积，代价是更长的编译时间及潜在的运行性能取舍；保留默认 panic 展开行为。符号剥离会减少回溯信息，排查问题时可用 `CARGO_PROFILE_RELEASE_STRIP=none cargo build --release --locked` 保留符号。

配置统一保存在 `~/.pp/agent/settings.yml` 和 `~/.pp/agent/models.yml`，使用 YAML 块映射、列表和 `#` 注释。启动目录中的同名文件不再读取。 `models.yml` 为必需文件：同一供应商的多个模型共享凭证、接口地址和协议。`settings.yml` 为可选的应用设置；默认使用文件中首个供应商的首个模型：

```yaml
# settings.yml
# 省略或找不到匹配模型时，使用首个供应商的首个模型
default_model: gateway/secondary
# 默认关闭；开启后记录运行状态和每个 session 的初始完整 prompt
debug: false
# 可选 replace 或 hashline
edit: replace
# 可选 none、low、medium、high、xhigh、max；默认 none
think_level: high
# 通知倒计时及自动消失时长，修改后重启生效
notification_show_countdown: true
notification_timeout_seconds: 10
# 相对路径以 ~/.pp/agent 为基准；空列表关闭扫描
skill_dirs:
  - ~/.pp/agent/skills
disabled_skills: []
# 每次用户任务最多请求数，包含自动摘要请求
max_model_requests: 20
# 仅当前模型显式配置 context_window 时生效
auto_compact: true
auto_compact_threshold_percent: 80 # 严格限制 1..=99
request_timeout_seconds: 120
# 建连阶段 429 限流的总重试等待预算（秒）；0 表示不重试
rate_limit_retry_budget_seconds: 300
read_max_bytes: 65536
read_max_lines: 2000
bash_timeout_seconds: 120
```

```yaml
# models.yml
providers:
  openai: # 供应商别名，全局唯一且不能含 /
    # 省略 protocol 时默认使用 response（Responses API）
    api_key: your-key
    base_url: https://api.openai.com/v1 # 可省略，默认使用此地址
    models:
      primary: # 供应商内的模型别名
        model: vendor-model-one # 发送给供应商的实际模型名
        context_window: 32768 # 仅为示例，应按实际容量填写或省略
        thinking:
          efforts: [low, high, xhigh]
          defaultLevel: high
      secondary:
        model: vendor-model-two
  gateway:
    protocol: completion # Chat Completions API
    api_key: another-key
    base_url: https://example.com/custom/v1
    models:
      primary:
        model: gateway-model-one
      secondary:
        model: gateway-model-two
```

`providers` 和每个供应商的 `models` 都是 map，key 分别是供应商 ID 和模型配置 ID；条目内部不再填写 `id`。`model` 保留为发送给供应商的实际模型 ID。两层均按文件书写顺序排列，用于模型菜单及默认模型选择。旧数组格式不再接受：迁移时将每个 `- id: name` 改成 `name:`，保留其余字段及原有顺序；`provider/model` 引用不变。

配置文件名和目录固定；`settings.yml` 可以缺省，缺少 `models.yml` 时模型不可用。通过 HOME 定位用户目录，不读取项目配置。新会话的工具以启动目录为工作目录；恢复会话使用日志保存的原工作目录。旧版本用户需将 JSONC 内容转换为 YAML，保存为上述两个 `.yml` 文件；仅重命名含 `//` 或 `/* ... */` 注释的文件无效。程序不再读取旧文件，也不会自动迁移含密钥的配置。

```sh
chmod 600 ~/.pp/agent/models.yml
./target/debug/pp -p 'Read the current project and explain its structure; do not modify files.'
./target/debug/pp
```

**API 密钥以明文保存在 `~/.pp/agent/models.yml`。** 仓库根目录的 `models.yml`、`settings.yml` 及旧版 `models.jsonc` 已加入 `.gitignore`，避免误提交个人配置。 The tools have no sandbox and can still read configuration files; do not let the model read or output files containing keys. 如果 MCP 的命令参数或环境变量含凭证，`settings.yml` 也属于敏感配置，应限制权限并避免提交。

Validation and model-selection rules:

- 每模型 `thinking.efforts` 是严格白名单：非空时仅接受 `low`、`medium`、`high`、`xhigh`、`max`，不隐式附加 `none`，也不接受 `default`。重复值去重，菜单及循环始终按 `none → low → medium → high → xhigh → max` 的允许子集排序。省略 `thinking`、`thinking: {}`、省略 `efforts` 或 `efforts: []` 均不限制原有六档。`thinking.defaultLevel` 支持六档及 `default`→`none` 别名；有非空白名单时默认值必须在其中。拒绝显式 null、错误类型、大小写/空白变体及未知字段，包括未选中的模型。
- 普通新启动优先使用模型 `defaultLevel`，否则使用允许的全局 `settings.think_level`，全局值不允许时使用规范顺序中最低允许档。实际切入其他模型时，显式模型默认值总是优先；没有显式默认则保留目标支持的当前档位，否则回退到目标有效默认。切到当前模型引用是无操作。`/new` 保留当前模型和档位。切换模型、原生请求参数与保存档位原子提交；持久化失败不部分改变状态。
- 白名单贯穿 `/effort`（含 `/think` 和 `default` 别名）、TUI 菜单、Shift+Tab、会话恢复与 API 请求；不支持的输入在本地拒绝，不改变档位、请求或日志。Shift+Tab 仅循环允许档，单档时保持档位与草稿。恢复精确保留仍被允许的保存档位及原生请求，修改 `defaultLevel` 不覆盖它；收窄白名单排除保存档位时拒绝恢复，不改写日志或发送请求。Ctrl+S 仍只保存全局 `settings.think_level`，不写模型配置、不改变当前会话；显式模型默认值及菜单 `(default)` 标记仍优先于保存的全局值。

- `think_level` 设置启动时的默认思考级别，支持 `none`、`low`、`medium`、`high`、`xhigh`、`max`；省略字段或缺少配置文件时使用 `none`。界面将 `none` 显示为 `no think`。旧值 `default` 仍可读取，但作为 `none` 的兼容别名，含义变为显式关闭思考。仅接受这些小写字符串，拒绝 null 和其他值。Responses 请求显式发送 `reasoning.effort`，Chat Completions 请求显式发送 `reasoning_effort`；`none` 均传为字符串 `"none"`，Responses 同时省略 `reasoning.summary`，其他档位仍使用 `reasoning.summary: "auto"` 请求可读思考摘要。通过本地模型白名单校验的档位原样传给服务端；白名单不保证供应商实际支持，未配置白名单时仍由服务端决定，不自动降档或重试。参见 [OpenAI 官方思考级别说明](https://developers.openai.com/api/docs/guides/reasoning)。
- TUI 输入 `/effort `（命令后加空格）即在输入框下方展开档位菜单，左侧显示档位，右侧显示说明和“当前”标记，黄色突出选中项。↑/↓ 选择，Enter 确认，Esc 取消并保留输入；Ctrl+S 将当前高亮选中的档位保存为全局默认值（写入 `~/.pp/agent/settings.yml`，下次普通新启动生效），不改变当前会话档位。菜单底部显示快捷键提示；继续输入档位前缀会筛选候选，Tab 补全但不执行。菜单只包含当前模型允许的档位；未配置限制时包含 `no think` 和上述五档，关闭思考的补全值为 `none`，不显示固定 token 预算。输入 `/effort` 后按 Enter 也可展开。行模式用 `/effort` 查看当前级别及可选值；两种模式均可用 `/effort high` 直接切换、`/effort none` 关闭思考，兼容旧命令 `/think` 和旧值 `default`。切换保留历史，对后续请求生效；`/new` 保留当前级别，`/model` 按目标模型策略选择档位；未配置策略的模型仍保留当前级别。TUI 底部状态栏显示实际选择，运行中拒绝切换并保留输入草稿。命令及 Shift+Tab 不改写配置文件；已有活动 session 时会持久化选择，写入失败则保留原档位。普通新启动按模型默认值与全局 `think_level` 优先级选择，恢复 session 使用仍被允许的保存档位。
- 缺少 `settings.yml`、省略 `default_model` 或没有匹配的 provider/model 时，使用首个 provider 的首个模型。模型请求次数默认 20，模型请求超时和前台 bash 超时分别默认 120 秒；后台 bash 不设固定 deadline，按本次用户任务生命周期收尾；read 默认最多返回 65,536 源字节且不超过 2000 个完整行。限制值必须为正整数，拒绝 null、负数、小数、字符串和数值溢出。
- `edit` defaults to `"replace"` when settings or the field are absent. Only exact lowercase `"replace"` and `"hashline"` strings are accepted; null, other types, case variants, and surrounding whitespace are rejected. It is fixed at startup and survives `/model` (including cross-protocol switches) and `/new`. Existing anchored-edit users must explicitly select `"hashline"`.
- An explicit `default_model` must be `provider/model`, split at the first `/`; neither part may be blank, matching is case-sensitive and exact with no trimming. The provider id cannot contain `/`, but the model id can: `gateway/team/model` selects the model with id `team/model` under gateway. Bare model ids are not accepted, and there is no environment-variable fallback.
- `providers` and every provider's `models` must be non-empty. Provider ids are unique across the file; model ids only need to be unique within their provider, and different providers may reuse the same model alias. id, `api_key`, and the vendor `model` must all be non-blank. All providers and models are fully validated at load, including unselected ones.
- Unknown fields are rejected at the file top level, the provider level, and the model level. A model may contain only `id`, `model`, and optional `thinking` / `context_window` and cannot override the provider's credentials, base URL, or protocol; the old top-level flat `models` format is no longer accepted.
- Provider-level `protocol` selects the API: `"response"` uses `POST /responses` and is the default when omitted; `"completion"` uses `POST /chat/completions` (not the legacy `/completions` API). Only these exact lowercase strings are accepted. Null, wrong types, old `"openai"`, and `"anthropic"` invalidate the entire configuration, including on an unselected provider. To migrate an existing Chat Completions provider, replace `"openai"` with `"completion"`; omitting the field now selects Responses.
- Responses 流优先直接解码为 SDK 类型；仅在报告缺少 `annotations` 或 `arguments` 时构造 JSON 树，为缺少注释的 `output_text` 补 `annotations: []`，并仅为 `response.output_item.added` 中的 `function_call` 补缺失的 `arguments: ""`，随后仍由 SDK 校验。已有值（包括 `null` 或错误类型）不会被覆盖；调用结束与最终响应中的工具参数仍须完整，工具只在最终批次校验通过后执行。
- Provider configuration is read only from these two YAML files: no provider environment variables, no environment substitution or fallback, and no `.env` or OAuth loading. Errors name the offending file and never dump config values, URLs, or keys. Choose the API supported by the configured service and model; there is no automatic protocol fallback.

When running `pp` directly, an unusable configuration (`models.yml` missing, unreadable, encoding/YAML/field/validation errors, or initialization errors such as an API key unusable in an HTTP header) never blocks entering interactive mode: the error is shown in the conversation area or the line terminal, and input remains editable. A missing `settings.yml` is not an error; defaults apply. With unusable configuration, submitting a task, `/compact`, `/new`, or `/model` shows the startup error without sending requests or running tools; `/help` remains available and `/quit` exits cleanly (exit code 0). Line mode also supports EOF. 模型及其他运行参数修复后需重启；skill 目录配置可以在 `/skills` 面板按 r 重扫。 `pp -p 'task'` instead exits non-zero on unusable configuration.

A provider's `base_url` is the API root. The existing path prefix is preserved; trailing `/` characters are stripped and the SDK appends `/responses` or `/chat/completions` according to `protocol`, with no automatic `/v1`. When omitted, `https://api.openai.com/v1` is used. Every explicit URL (including unselected providers) must be http/https with a host; userinfo, query, fragment, all-blank, and null are rejected. An explicit HTTP URL transmits credentials and sessions in plaintext — use it only with trusted compatible services. Only the selected key goes through HTTP header validation; unselected keys are merely checked for non-blankness.

## MCP 工具

在 `~/.pp/agent/settings.yml` 中添加 `mcp_servers`，连接本地 stdio MCP 服务：

```yaml
mcp_servers:
  local:
    command: python3
    args: [/absolute/path/to/mcp_server.py]
    env:
      APP_MODE: development
    timeout_seconds: 30
```

`command` 必填，按 PATH 查找程序；`args` 和 `env` 默认为空，超时默认 30 秒且必须为正整数。直接传递参数，不执行 shell 展开；需要 shell 时显式使用 `bash -c`。服务继承进程环境，`env` 覆盖指定变量，工作目录为当前会话的工作目录。省略 `mcp_servers` 或设为 `{}` 时不启动服务。所有配置层均拒绝未知字段；服务名只能包含 ASCII 字母、数字、下划线和连字符，最长 24 字节，不能含 `__`。

连接时按照 [MCP 初始化流程](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle) 完成版本协商和 `notifications/initialized`，再分页调用 `tools/list`。支持协议版本 `2025-11-25`、`2025-06-18`、`2025-03-26` 和 `2024-11-05`。本次实现仅支持 stdio 工具发现及 `tools/call`，不支持远程 HTTP、resources、prompts、sampling、elicitation 或运行中更新工具列表；服务端 ping 会正常响应，其他服务端请求返回不支持的方法错误。

工具名通常为 `mcp__服务名__工具名`，例如 `mcp__local__search`；包含特殊字符或组合名超过 64 字节时，工具部分替换为确定性的 `tool_<hash>`，调用时仍发送原始名称。重名会拒绝启动。MCP 的输入 schema 原样传给模型（`strict: false`），两种模型协议均支持调用，CLI/TUI 使用现有工具事件显示状态。结果保留 `content` 和 `structuredContent` 等字段，包装为 `{"ok":true,"result":{...}}`；`isError:true` 转为 `ok:false`，同批工具仍按顺序继续执行。

初始化或发现失败时，当前配置的模型任务不可用；不完整连接会清理。每次请求的读写共享同一截止时间，通知不会延长超时；传输故障后关闭连接，后续调用返回错误，不自动重连或重试。TUI 主动取消会停止并回收当前 MCP 服务进程组，下一任务真正调用该服务时才使用同一启动配置重新初始化和发现；不重放旧调用。完整工具集合（名称、描述与 schema）必须不变，定义变化或普通重建失败会拒绝该服务直到重启 pp，其他服务与本地工具不受影响。普通超时／EOF／协议故障仍不自动重连。服务进程在会话中复用，`/model`、`/new` 不重启；恢复保存的会话时按当前全局配置重新连接和发现工具，不恢复旧凭证或过期工具定义。正常退出、TUI 的 Ctrl-C 退出及连接故障时清理服务进程组。最多连接 32 个服务、发现 256 个工具，单条协议消息最多 1 MiB。

MCP 服务按当前用户权限执行，能够访问工作目录之外的资源。服务 stderr 被丢弃，协议错误不输出原始响应体、命令参数或环境变量；成功的工具结果按数据展示和回填模型，服务应自行避免在结果中返回凭证。

```sh
cargo nextest run --locked --test cli_agent mcp_ --test-threads=1
```

## 运行诊断日志

除 `pp gc` 外，所有运行模式在参数解析后自动向 `~/.pp/logs/pp.yyyy-mm-dd.number.log` 追加 JSON Lines；`--help`、`--version` 和参数错误不创建日志。HOME 必须非空且为绝对路径。新目录权限 0700，新日志和固定 `.lock` 文件权限 0600，不改已有父目录权限；日志目录、锁和日志文件不接受符号链接，文件必须是普通文件。

按本地日期和 **10 MiB（10 × 1024 × 1024 字节）**严格分卷，编号从 1 开始、按数字递增。重启继续追加当天未满的最大分卷；不同工作目录的进程通过同一 `.lock` 的短时阻塞 flock 协调完整事件的选卷和追加，不覆盖或截断已有日志。仅支持提供可靠 flock 语义的 macOS/Linux 本地文件系统；运行中不要手动替换 `.lock`。单事件超过上限时整条拒写，不截断 JSON。

不会按时间自动删除日志；启动、写入、跨日和重启均保留旧日志，没有后台清理或总容量上限。需要释放空间时运行 `pp gc`，按文件名日期清理超过 90 天的普通日志文件，不使用 mtime，并跳过其他名称、符号链接和目录。

诊断事件包括应用启停（pid、启动 cwd、prompt/line/tui 模式）、配置不可用、任务、Completion/Responses 请求和工具的开始/结束、受控状态及耗时。工具类别仅为 `ask`、`load_skill`、`read`、`grep`、`find`、`edit`、`write`、`bash`、`bash_task`，其他统一为 `external`。不记录 prompt、回答/思考、工具参数/结果/输出、工具文件路径、错误原文、配置、URL 或 API key；不启用依赖库日志，也不读取 `RUST_LOG`。这些运行诊断日志与下述包含完整交互的 session 文件相互独立。

初始化失败，或退出时发现写入/轮转失败，仅在 stderr 输出一次固定 `warning: file logging unavailable`，不改变应用退出码；TUI 运行中不打印日志错误。事件同步交给操作系统，无后台队列，不逐条 fsync；强杀或断电不保证最后一条完整/持久化，也不保证存在 `app_stopped`。

### Debug 日志

在 `~/.pp/agent/settings.yml` 中设置 `debug: true` 后，启用以下两类独立的分段文本日志（不是 JSONL）；默认 `false`，省略字段或配置文件时均不开启，只接受布尔值，修改后重启生效。只启动程序、不发送模型请求不会创建这两类日志文件。

- **`~/.pp/logs/prompt-debug.SESSION_ID`**：每个新 session 一个文件，仅记录首次普通模型请求的完整原生合成 prompt（`llm_first_prompt`），包括 system/instructions、初始用户输入、工具定义、模型参数，以及对应的 session ID。后续消息、工具轮次和切模型均不追加；`/new` 后的新 session 会生成另一个文件。重启后通过 `-r`、`-c` 或 TUI `/resume` 恢复同一 session，不覆盖、不追加，也不补录中途 prompt。请求发送并完成初始连接处理、session 持久化后写入，消费响应流之前完成；供应商拒绝请求时也保留初始 prompt，临时压缩摘要请求不记录。
- **`~/.pp/logs/debug.yyyy-mm-dd.xxxx`**：保留其他调试信息，目前为每次普通请求前的 `harness_state` / `request_ready` 快照。包含 pid、session ID（首次持久化前为 null）、provider/model 引用、协议、cwd、思考级别、edit 模式、工具限制、当前任务请求序号/上限、历史项数和工具数；不再记录 prompt 正文、回答或工具内容。`xxxx` 是从 `0001` 开始的四位最小宽度分卷编号，不带 `.log` 后缀。

Prompt 文本以事件名和时间戳为标题，按「请求参数 → 系统指令 → 对话消息／输入历史 → 工具定义」分区；Completion 与 Responses 均支持。正文用 `字段: |` 独立成块，原样保留换行、空行、代码缩进、引号和反斜杠，不折行、不截断；工具参数 schema 使用格式化 JSON，其他嵌套字段缩进展开，未知字段同样保留。

两类日志沿用普通日志的文件权限和跨进程锁，均不自动删除，只有 `pp gc` 会按文件名中的本地日期清理超过 90 天的文件。`debug` 按本地日期及 10 MiB 整条事件分卷；`prompt-debug` 不分卷、不追加，单个 prompt 上限为 10 MiB，新 session 首次写入不会清理旧文件。单条超限整条拒写，不截断。初始化、序列化或写入失败时停止本次进程的 debug 与 prompt-debug 记录，向普通诊断日志写入固定 `debug_logging_unavailable` 事件，不中断任务、不向 TUI 打印日志错误。

**`prompt-debug` 包含完整初始 prompt，可能含敏感源码或用户数据。** 不主动序列化配置、API key、HTTP 认证头或供应商错误体；但已进入 prompt 的敏感内容会原样记录。仅在需要排查时开启，分享日志前自行检查；普通 `pp.*.log` 的内容隔离规则不变。旧 `debug.*` 文件不会迁移或改写，可能仍含此前记录的 prompt。

## Terminal Interaction

未指定 `-p` 且标准输入、标准输出均为终端时，`pp` 自动进入 Ratatui TUI：启动页采用 omp 风格的居中像素字标、版本号和简短操作提示；发送消息后，上方切换为流式结构化对话记录，下方为多行输入框，使用复古绿色（`#7FA66A`）上下横线和 `❯` 提示符。图标采用 Nerd Font 字形，终端需使用支持这些字形的字体。输入框底部状态栏显示估算上下文用量、实际模型、思考级别、仅在 Plan 模式显示的叠放方框图标与 Plan 标识（窄屏缩为 P）、启动工作目录和 Git 信息。`no think` 表示显式请求关闭思考。较长的模型名、路径和分支名会自动缩略。Git 状态每两秒在后台刷新，不阻塞输入：`` 表示分支或分离头指针的提交，`*N` 表示已跟踪的改动条目数，`?N` 表示未跟踪条目数，配置上游时通过 `↑N` / `↓N` 显示领先和落后提交数。非 Git 目录或 Git 不可用时隐藏 Git 信息。运行状态及忙碌提示位于输入框上沿左侧；手动压缩时在输入框上沿显示 `Compacting context...` 或 `Cancelling context...`。执行 `/new` 后恢复欢迎页；完整快捷键和工具安全说明可通过 `/help` 查看。模型文本及 bash/grep/find 标准输出、标准错误均在完成前实时显示，工具进度保留在对话记录中。暂不提供侧栏。助手正文支持流式 Markdown，edit/write 与 ask 使用独立内容块；这些块共用原有 transcript 的滚动、恢复与退出 scrollback。

应用自有的 TUI 文案统一使用英文，包括帮助、菜单、通知、skill 管理与加载卡片，以及这些界面接收的配置保存错误和 skill 诊断。用户输入、模型回复、文件／命令输出、外部 skill 元数据与正文，以及名称和路径保持原文；不限制 Unicode，也不强制模型使用英语。已有 session 中保存的历史工具错误忠实回放，不改写旧日志。共享生产者的配置保存错误和 skill 诊断在 CLI 中也使用英文。

用户与助手保留既有角色背景；助手正文按 Markdown 渲染，read 等工具正文保持字面文本，edit/write 与 ask 使用下面说明的内容块。bash 与 think 使用下述独立样式；其他工具保留淡色正文与运行／完成／失败状态。真实 `grep` 调用使用青色卡片，`find` 使用普通数据卡片；标题显示工具名与紧凑 JSON argv，保留空格、引用和控制字符边界。搜索结果直接显示 stdout/stderr，不使用 bash 专属折叠或计时 footer。真实 `load_skill` 调用使用品红色 skill 卡片，不按文件路径或 bash 命令猜测类别。提示为灰色，错误为红色；颜色输出仍遵循 `NO_COLOR`。

- 仅助手正文（包括实际可见的 refusal）解析 Markdown：标题、粗体／斜体／删除线、链接、列表／任务项、引用、代码 frame 和表格随流显示；未闭合 fence 的已到达代码立即可见。图片只显示 ALT 与 URL，HTML 按字面显示，不执行、不下载或打开链接。表格按终端列宽缩列，过窄时改为 `header: value` 竖排；中文、组合字符、emoji 与续行前缀按 grapheme 布局。用户输入、think、read/bash/搜索/MCP 输出、错误和 Note 不解析 Markdown。resize 只重新布局已解析文档，用户上翻时新内容不会拉回底部。
- edit/write 的 diff 是**保存成功后展示，不是写入前审批**：header、`+N -N`、真实行号、红色删除／绿色新增与三行上下文反映 worker 读取的旧快照和实际提交内容，不执行 git、不在 UI 线程读取文件。默认显示前 10 条逻辑 diff 行，Ctrl+O 切换最近一个可折叠的 bash/skill/diff；退出时全部展开写入 scrollback，恢复时重新采用默认折叠。相同内容、新建空文件、缺末尾换行、LF/CRLF 与 BOM 均有明确展示。write 的旧快照不可读或非 UTF-8 时仍可按原权限保存，只显示 diff unavailable；目录 sync warning 属于已提交成功。原有启动目录／symlink 限制不变，外部并发修改仍沿用原保存语义。diff 只进入 `pp.event` 和显示记录，不改变模型工具结果 JSON；旧日志缺少 FileChanged 时仅显示原摘要，不重建 diff，CLI 不额外打印 diff。
- 新内容块仅在显示层将除 LF/TAB 外的 C0 字符及 DEL 替换为 `�`，TAB 按四列 tab stop 展开；原始助手正文、事件、参数和用户答案保持不变。
- 临时消息以非模态圆角弹框显示在输入框上方靠右，左右至少留一格，不挤占布局、不改变草稿、光标或历史滚动位置。普通消息为绿色，重要提示／警告为黄色，错误为红色；通过边框、标题和正文的颜色亮度实现 200ms 淡入与最后 400ms 淡出，默认总时长 10 秒，不是窗口透明度。右上角显示剩余秒数（向上取整），无需输入也会更新并自动消失；`notification_show_countdown` 默认 `true`，关闭仅隐藏秒数，`notification_timeout_seconds` 设置总时长，默认 `10`，必须为正整数且不超出计时范围。通知配置在启动时读取，即使模型不可用也生效；设置缺失或无效时使用默认通知行为，无效设置仍显示启动诊断，修改配置后需重启。鼠标停留在弹框内（含边框）时隐藏倒计时并保持完全显示，移出后重新从配置时长倒数；新消息替换旧消息，不堆叠。使用终端原生选区复制：先将鼠标移入弹框使其保持显示，再按终端要求的修饰键拖选，使用终端自身的复制快捷键；程序保留鼠标捕获以支持悬停与滚轮，不提供复制按钮或应用内选区。终端选区只能复制屏幕上可见的文字，可能包含边框；不会取得截断后未显示的内容。中文、emoji 和多行正文自动折行，最多八行，过长内容只显示开头；空间不足时隐藏弹框或倒计时，恢复尺寸后仅显示尚未到期的消息，倒计时不会覆盖标题和边框。skill 全屏管理面板中的弹框采用相同规则。
- 界面已体现的成功变化不重复通知：模型和当前思考档位切换只更新状态栏，`/new` 直接恢复欢迎页并清空旧通知，skill 面板保存成功直接关闭。Ctrl+S 保存下次启动的默认模型或思考档位以绿色弹框确认，不改变当前会话模型或档位；本地命令、切换和保存的临时错误使用红框。启动 skill 扫描的非致命诊断汇总为黄色提示，完整详情保留在 `/skills`。任务／工具失败和执行不可用仍保留原有持久错误位置；通知不进入对话记录、session 或退出滚动历史，CLI／管道输出不变。
- 空闲时 Enter／Alt+Enter 立即发送非空草稿；运行中 Enter 补充当前任务，Alt+Enter 排队为后续独立任务。Ctrl-J 插入换行；粘贴保留多行且不会自动发送。输入框使用终端原生光标，为拼音等输入法提供插入位置。菜单关闭时，↑/↓ 优先在输入框的显示行间移动（包括自动折行）；到达首行后再按 ↑、到达末行后再按 ↓，才浏览本次进程中已提交的历史输入，支持继续编辑；↓ 越过最新一条会恢复浏览前的草稿和光标，历史两端不循环。运行中也可浏览输入历史，忙碌时被拒绝的提交不计入历史；`/new` 重置模型会话后仍保留本次进程的输入历史；恢复 session 时从已保存用户消息恢复输入历史，不恢复草稿或历史浏览位置。菜单展开时，↑/↓ 仍用于选择菜单项。
- Shift+Tab 按 `no think → low → medium → high → xhigh → max → no think` 循环切换思考级别，后台确认后更新状态栏，保留草稿、光标和输入历史浏览位置。选择菜单展开时不切档。Ctrl+P 打开模型菜单，临时输入 `/model ` 供筛选和 Tab 补全；Esc 取消或 Enter 确认后恢复原草稿和光标，切换失败也保留。忙碌时拒绝切档和打开模型菜单，不排队。F1 显示与 `/help` 相同的帮助，忙碌时也可使用且保留草稿。三个快捷操作均忽略长按重复事件；skill 面板优先处理自身按键。Ctrl+N、Ctrl+U、Ctrl+W 沿用编辑器既有行为。
- 在光标所在的空白分隔 token 中输入 `@`，立即在输入框下方按目录层级浏览项目文件：只显示当前目录的直接子项，目录在前并以 `/` 结尾，Enter 或 Tab 进入目录，选择 `../` 返回上级；进入／返回目录时同步将当前 `@` 引用补全为对应的目录路径（如 `@src/tui/`），光标停在路径末尾，不添加空格、不发送消息，也不重新扫盘；返回根目录时恢复为 `@`。继续输入按当前子树内的相对路径做大小写敏感的子串筛选，匹配到深层文件时仍逐级显示其祖先目录，不再平铺后代文件。↑/↓ 选择，Enter 或 Tab 选中文件后**只插入完整的 `@文件路径`，不发送消息、不读取文件正文、不上传附件**，随后再次 Enter 才发送编辑好的原始文本。含空白、双引号或反斜线的路径使用 JSON 引号，例如 `@"docs/my file.md"`；补全目录时光标停在闭引号前，仍可继续输入筛选或选择子项。补全只替换当前引用，保留多行草稿与其他引用。邮箱、`@@foo` 和光标位于闭引号之后的 `@"…"` 不触发菜单。候选根为启动／恢复会话的工作目录，不向上扩展到 Git 根；后台枚举遵守项目内 `.ignore`、`.gitignore` 和 `.git/info/exclude`，包含未被忽略的隐藏常规文件，不列出 `.git` 或符号链接。每次重新激活时重新扫描，同一次选择中的筛选不重复扫盘；扫描期间 Enter/Tab 不提交，根目录无可选文件或无匹配时 Enter 按普通提交处理；子目录无匹配时保留 `../` 供返回上级，只有包含可选文件的目录才会列出。Esc 收起菜单并保留草稿，迟到的扫描结果不会重新打开；普通行模式和 `-p` 不提供文件菜单，`@` 文本原样发送。
- 运行中补充（Steering）仅在首次普通请求前或完整模型回复／整批工具结果提交后应用，不打断 SSE、工具批次、ask 或临时摘要；每个边界领取固定批次，后到消息等下一边界。补充沿用当前任务的请求额度、activity 和后台服务；FollowUp 每条独立启动任务，有新额度和取消 token。同类型保持 FIFO，Steering 优先于尚未开始的 FollowUp，但不抢占已开始的任务。入队成功才清草稿并记录一次原始输入历史；对话只在真正应用后回显。Alt+Enter 需终端发送可区分的 ALT-Enter／ESC+CR；不支持的终端不会自动改键或猜测两次独立按键。
- Ctrl+Q 查看／关闭待处理列表，标签为 `Steer #ID`／`Queued #ID`，预览只显示原输入首行（最多 120 个 Unicode 字符）。↑/↓、Home/End、PgUp/PgDn 选择；Enter 取回原输入到编辑器末尾，要求编辑器每一行都完全为空（空格也不允许），不记录新历史、不发送、不重新展开模板。已被 worker 领取的项不能取回；Tab 不取回，Esc 只关列表，编辑／粘贴先关闭列表再修改草稿。ask、skill、session、tree 和临时模型面板优先消费按键。输入栏显示 `Steering: N · Queued: M`；窄屏优先保留 context、临时状态、队列摘要，再显示可选 token 指标。
- 取消或失败会保留未应用队列并暂停；即使最终回复赢得与 Esc 的竞争，取消意图也不会自动启动积压任务。仅空闲时 Ctrl+R 显式恢复，保留正在编辑的草稿；暂停期间新消息继续入队，不绕过旧队列。收尾期间拒绝新模型输入，但仍可查看／取回。存在待处理消息时，`/new`、`/resume`、`/tree` 和空输入双 Esc 均被阻止，须先取回消息。空闲时模型／思考档位仍可切换，后续任务使用当前配置。队列只存在内存中，退出不执行／等待它；恢复只重放已应用的普通用户历史，不重发旧队列或草稿。行模式与 `-p` 不增加并发输入或这些快捷键。
- Ctrl+A / Ctrl+E 分别跳到当前行首 / 行尾；Ctrl+K 删除光标到当前行尾的内容，在行尾或空行按下不删除换行。左右键、Home/End、Backspace/Delete 继续支持中文和 Unicode 文本编辑。
- The input box grows with explicit lines, up to six; long lines soft-wrap, and the editing viewport scrolls past its height. Narrow screens shorten cwd, hide Git, then shorten the model; context usage has priority and is clipped by display columns only as a last resort. Short screens keep input and status first; restoring size never loses the draft.
- bash 使用暗色圆角边框卡片：顶部为 `$ command`（管理调用为 `bash_task · action · task_id`），`Output` 横线分隔命令和灰色输出，底部前台显示耗时与退出码、后台快照显示 `Task bg-N: status`；运行中显示 running，失败保留错误和退出信息。后台卡片完成仅表示 start／status／stop 调用返回，服务可能仍在 running。超过 10 行时默认显示末尾 10 行及省略提示，Ctrl+O 展开或收起最近一个可折叠的 bash/skill/diff 块，不改变输入草稿；退出时完整输出写入终端滚动历史。think 默认以无标题、无边框的灰色文本显示在独立暗色背景上，与最终回答分开。是否返回可读思考取决于服务端和模型；`/effort` 仍只控制思考级别。行模式和单次任务将思考文本写入 stderr，stdout 保持为回答正文，均不添加 ANSI 控制序列。Responses 摘要使用 [官方流式事件](https://developers.openai.com/api/reference/resources/responses/streaming-events)。
- Mouse wheel scrolls the transcript by three visual lines per event; PgUp/PgDn scroll by a page. Both work without moving the input cursor or changing the draft, even with the pointer over the input box. Output follows the bottom by default; scrolling up leaves follow mode, so incoming chunks do not steal the viewport. Scrolling down to the bottom restores following. Drafts stay editable and scrollable while running; queued dispatch preserves the new draft and its cursor.
- 命令、模型、思考级别和文件菜单展开时，滚轮仍滚动历史，不改变菜单选择。Clicks, drags, pointer movement, and horizontal wheel events are ignored. The TUI captures the mouse; any modifier needed to bypass capture for native terminal selection or scrollback depends on the terminal emulator.
- Single-line slash commands share one registry with line mode: `/help` lists commands; `/model` opens a model picker; `/model provider/model` switches directly; `/new` resets history while keeping the selected model; `/compact` summarizes older context without clearing the visible conversation; `/resume` opens the fullscreen saved-session picker (TUI only); `/plan [view|new|off]` controls the TUI-only planning workflow; `/quit` exits. Unknown names and invalid built-in arguments are local errors, never model tasks. Names are case-sensitive; leading/trailing whitespace is ignored, but the entire remaining `/model` argument preserves internal spaces and `/`. Multi-line slash drafts remain ordinary tasks.
- 退出时仅在内存中生成最终画面，切回主屏幕与完整会话回放使用同一次终端同步更新（DEC 2026）；支持该协议的终端会隐藏中间画面，减少闪烁。退出后仍保留完整历史、草稿和状态栏；不支持同步更新的终端按原有方式显示回放。
- 输入框下方的选择菜单（`/` 命令补全、`/model `、`/effort `、`@` 文件选择、Ctrl+Q 待处理消息）统一使用 `src/tui/menu.rs` 中的 `SelectionMenu` 组件，业务层只提供选项数据和确认动作。统一在输入框下方显示无外框的两列列表，左列占 46%、右列占 54%；选中行以黄色文字和箭头突出，其余说明与图标为灰色，当前值显示“（当前）”，输入框使用紫色分隔线。最多显示八行，窄屏截断说明，短屏随选择滚动。↑/↓、Home/End、PgUp/PgDn 导航且不循环，Tab 补全或打开目录但不提交（待处理消息列表中无操作），Enter 确认当前项；Esc 收起列表并保留草稿，普通补全菜单下次编辑时重新显示，队列列表须 Ctrl+Q 显式打开。输入 `/` 展开命令菜单，继续输入会按命令前缀筛选，左列保留命令图标。
- 输入 `/model `（命令后加空格）会在输入框下方展开与 `/effort ` 一致的两列菜单：左侧为 `provider/model`，实际启动默认模型附带 `(default)`；右侧为实际模型名、`completion`/`response` 及“当前”标记，默认与当前可分别标在不同模型上。Ctrl+S 保存时保持菜单、筛选内容和高亮选择，成功后更新 default 标记，失败保持原标记。黄色文字和箭头突出选中项。`/model` 后按 Enter 也能展开，默认选中当前模型。按启动配置顺序排列，最多显示八行，长列表随选择滚动；↑/↓、Home/End、PgUp/PgDn 导航，Enter 确认，Esc 收起并保留输入。继续输入按引用或实际模型名的前缀筛选，Tab 补全引用但不切换；Ctrl+S 将高亮模型保存到 `~/.pp/agent/settings.yml` 的 `default_model`，下次普通新启动生效，不切换当前会话模型、不改写 `models.yml`。保存沿用 YAML 原子更新，保留注释与其他设置，缺少设置文件时自动创建；失败显示错误并保留原配置。菜单底部显示快捷键提示；通过 Ctrl+P 打开时，保存后仍保留菜单，按 Esc 恢复原草稿和光标。多行粘贴仍作为普通任务。后台完成切换后才更新状态栏，历史和确认后新编辑的草稿均保留；忙碌时拒绝切换、不排队、不清空草稿。`/new` 仍在后台确认后清空历史和草稿。
- 运行中按无修饰键的 Esc 取消当前任务，不退出 pp；已有面板或菜单优先消费 Esc。收尾期间显示 `Cancelling...`，仍可编辑草稿、滚动、F1/help、Ctrl-O 或退出，但不能提交／排队新任务、切换模型／思考级别或 `/new`。收尾完成后保留草稿、光标、输入历史、已完成的工具结果和文件修改，并追加 `Task cancelled. File changes were not rolled back.`；不重试、不回滚。每个任务使用独立取消 token，已提交的最终回答不会被迟到 Esc 改成取消。行模式和 `-p` 不提供取消入口。
- `/quit` 或 Ctrl-C 退出整个 pp：先停止受管理后台进程组并等待它们收尾，正常退出码为 0；真实清理失败返回错误。恢复终端后，全部对话记录按退出时的终端宽度写入普通终端的滚动历史，保留原有颜色、缩进、分隔线、流式片段、工具输出及错误，末尾保留输入框、未提交的草稿、菜单和状态栏；可以向上滚动查看超出一屏的历史。保留长度受终端自身的滚动历史容量限制。退出不等待整个 agent worker、阻塞中的模型请求或其他前台工具；受管理后台清理不依赖 UI 事件回调。其他前台 bash 子进程仍无 Ctrl-C 清理保证，需要任务级前台受控清理时应先按 Esc 等待收尾。文件修改不会回滚。
- `-p/--prompt` runs one task and never parses slash commands: `pp -p '/model'` sends `/model` as a task. It can follow `-r ID` to append one task to saved history; combining it with `-c` is rejected. If either stdin or stdout is not a terminal, line mode supports the same commands without raw mode or an alternate screen. `/model` lists configured models and marks the current one; use `/model provider/model` to switch. Stdout streams **all readable assistant text**, including text before tools, with one terminating newline per nonempty turn and no replay at completion. Refusal is printed only if no non-blank content wins. Help, command responses, errors, and `tool <name>: running/ok/failed` go to stderr; live process data uses `[{name} stdout]` / `[{name} stderr]` on pp's stderr, with the actual `bash`, `grep`, or `find` tool name. Non-TUI output adds no ANSI control sequences. A display write failure is returned only after the Agent finishes recording the task's history/tool results.

### Plan（仅 TUI）

普通任务默认 **EXEC**。空闲时输入 `/plan` 显式进入 **PLAN**，然后提交调研任务；命令本身不请求模型。模型只能调研、询问关键产品取舍并保存计划，不能切换模式。`/plan` 再次打开当前计划；`/plan view` 只查看，未保存时显示 `No saved plan.`。`/plan new` 开始另一份计划，未执行正文存在时先本地确认；`/plan off` 本地确认后放弃计划并回 Execute，不请求模型。确认默认取消，旧 Markdown 不删除。忙碌或有 pending 输入时不能切换／批准；命令不排队成后续执行动作。

Plan 的执行层白名单只有 `read`、`load_skill`、TUI `ask`、`plan_search`、`plan_find`、`plan_write`、`plan_submit`；即使模型返回隐藏的 shell、write/edit、原生 grep/find 或任意 MCP 调用也会拒绝。进入 Plan 显式停止受管理 MCP，恢复 Plan 不连接 MCP；清理失败保持 Plan 门禁并阻止模型请求，`/plan` 可重试清理。批准或 off 的可取消执行准备完成后，UI 接管新的退出清理句柄才继续。

`plan_search` 是区分大小写的**字面行子串搜索**，`plan_find` 按搜索根的相对文件路径子串匹配，空 query 可枚举。均在进程内使用项目 ignore 规则，不运行 rg/fd/grep/find，不解析 argv，也不创建 `.pp-cache`。支持 `path`、`hidden`、`no_ignore`、从 0 开始的 `offset` 和 `limit`（默认 200，1–500）；结果有 `next_offset`、截断和跳过文件的 warnings，受 65,536 字节输出上限、读取行大小、取消与工具超时约束。read/skill 仍沿用现有读取权限，**不是凭据访问沙箱**。

计划正文保存在项目 `.pp/plans/<随机ID>.md`，首次保存才创建目录／文件；不自动修改 `.gitignore`。正文须为非空 UTF-8、无 NUL、最多 262,144 字节，实施步骤使用标准 `- [ ]`／`- [x]` Markdown task list；代码 fence 中的 checkbox 不算任务。保存使用目录句柄、NOFOLLOW 和原子提交，拒绝符号链接／特殊文件；同名首次创建不覆盖。修订写入要求 expected revision 和已登记磁盘字节都匹配，避免覆盖外部编辑。

模型 `plan_submit` 成功后补齐整批 call-ID/results、同步 native history 和 Ready workflow，然后停在审阅边界，不再发总结请求。审阅正文可 PgUp/PgDn／滚轮滚动，Tab 有意切到操作区后默认选择第 2 项；打开面板时焦点在正文，Enter 不默认执行。四项严格按以下顺序显示：

1. **Execute (plan only)**：Fresh，新 session；只发送完整批准计划及固定实施／更新 TODO 规则，正常加载系统、当前项目与技能规则，不带原始需求、调研聊天、旧调用或 reviewer 报告。
2. **Execute (keep context)**：默认 Keep，保留当前历史并追加一次完整批准快照；计划与重要历史用户要求冲突时先澄清。
3. **Continue discussing Plan**：独立编辑器，Enter 发送、Ctrl-J 换行、Esc 丢弃未发送问题；发送后降 Draft，保留计划／TODO，重新提交才能批准。不强制立即重写文件；仅针对当前修订的 reviewer 报告随问题作为辅助材料带回。
4. **Review Plan**：当前模型的独立只读上下文，仅原始需求＋当前计划；可读取代码和安全搜索，不可 ask／写入／提交／实施。报告按严重程度给出计划条目、代码证据、影响和建议，明确静态审查局限；不会修改计划或勾选 TODO，也不自动批准。无需先 review 或消除报告问题才可执行。

Esc 只关闭审阅，不退出 Plan。外部编辑后再次安全读取可直接登记新修订并进入 Ready（须有真实 TODO），不强制模型复核；无 TODO 的正文只能 Draft 查看。批准携带 session／plan／revision／精确正文 token，worker 在准备前后再次核对文件及队列。字节变化会刷新新修订并要求重审；文件缺失、非 UTF-8、过大或不安全时绝不用日志旧正文回填。报告绑定审查修订和模型，计划改变后标记 `outdated`；审查结束不会自动实施。

两种批准方式之后都保持 **EXEC**，包括成功、失败和取消；再次规划必须用户 `/plan`。`Executed` 表示已经交接，不表示全部完成。执行通过普通 edit 更新**同一份 Markdown**：只有动作及完成条件实际验证后才勾选，失败／取消／阻塞项保持未勾选；不能重写批准的目标、接口和方案或用后来新增 TODO 扩大范围。执行中 `/plan view` 显示最近成功同步的只读进度，Esc 只关闭视图，不取消任务；批次提交及收尾安全刷新正文，不监听外部文件、不自动勾选剩余项。批准时的原始快照永久保留在实施消息／checkpoint，进度不会改写审批记录。

`/new` 保持模式但清掉绑定，下一次 Plan 普通任务建立新计划；切模型、跨协议及手动／自动 compact 保留 workflow。`/tree` 保持当前模式和最新计划文件，Ready 降 Draft，不恢复历史 Execute 权限。Plan 的 `-r`／`/resume` 不重放审批、review 或执行；打开审阅须重新读磁盘，`-c` 在 Draft 等输入、Ready 打开审阅，不自动续跑模型。非 TUI 恢复 Plan 明确失败，所有 `/plan` 子命令在行模式只给 TUI 提示；没有 `--plan` 或管道批准接口，`-p` 内容仍是字面 prompt。

这些保证限于 pp 的工具、版本、队列、日志和托管进程生命周期；不是 OS 沙箱、逐操作授权或对实现范围的语义证明，不能停止用户独立启动／故意脱管的程序。

独立实际 TUI smoke 使用临时 HOME／项目和单独 loopback provider 进程：观察 ask、批准前无 hello.txt、外部编辑使旧审批失效、Keep 写入 `hello from reviewed plan`、同文件进度 1/2、Fresh 新 session 写入 second.txt、原规划会话仍可恢复、review 不改第三份计划，以及讨论／重新提交仍需明确批准；退出后 `-r` 查看原修订报告没有新增 HTTP 或第三个文件。这只证明本地 fixture 路径，不声称真实供应商兼容。

### ask（仅 TUI）

模型通过显式 `ask` 工具发起一批问题，不从普通助手文字猜测选项。仅在确实需要用户决定时提问；推荐项只是初始焦点和 `(Recommended)` 标记，不会自动选中，也不授权文件写入。问题使用唯一 `id`、非空 `question` 与 `options`，选项保留原始 `label`，可带 `description`／Markdown `preview`；`header` 为可选题目 chip，`multi` 默认 false，`recommended` 是从零开始的选项下标。空 options 可自由回答；重复 id／label、空白必填文字、未知字段或越界推荐项返回普通工具错误，不打开面板。

`recommended` 推荐使用题目层级的零基下标，例如 `{"questions":[{"id":"choice","question":"选哪个？","options":[{"label":"A"},{"label":"B"}],"recommended":0}]}`；也支持选项上的 `recommended:true`／`false`，true 表示推荐该选项。为兼容 provider 的参数差异，两种协议共用的入口还接受题目／选项上的数字字符串下标、旧式选项内数字下标（按数值解释，不按所在位置猜测）、纯字符串选项、`multi:null`（等同缺省 false），以及额外编码一层的 JSON 参数字符串。缺省或 null 推荐值表示不提供提示；多个有效推荐值必须指向同一选项，否则报错。归一化只处理这些明确格式，保留原始问题／选项文字、顺序及调用 ID，继续拒绝未知／重复字段、冲突或越界推荐项和畸形 JSON；错误不回显原始参数。运行中与会话恢复共用这一解析路径，答案格式保持一致。

面板锚定输入框上方，不改主草稿、光标或输入历史。↑/↓、Home/End、PgUp/PgDn 导航；Tab/Shift-Tab、左右键切问题／Review。单选 Space 选中焦点项并停留当前题，不前进或发送；Enter 确认焦点项并前进，单题单选直接提交。多选 Space 勾选／取消，Enter 只前进，不顺便选中焦点项。多题和单题多选须在 Review 再按 Enter 一次提交；未答单选会定位回该题，空多选可显式提交。每题自动有 `Other (type your own)`，Enter 打开独立编辑器，支持 Unicode、粘贴、多行和 Ctrl-J；非空白输入才可接受。单选的 custom 与选项互斥，多选可共存。

面板采用深灰背景、青色圆角边框与分隔线，顶部固定显示 `Ask`，下方标签栏展示各题 header（缺省为 id）；多题或多选另有 `Submit` 标签进入 Review，当前标签使用灰色底色。选择面板只显示选项名称（最多两行）、推荐标记与选择状态，不重复问题正文、说明或预览；Review 只显示题目标签与答案摘要。焦点以黄色 `❯` 和黄色名称表示，单选使用轮廓更大的单字形 `◯`／绿色 `⦿`，多选使用 `☐`／绿色 `☑`；标记加粗，无额外括号，焦点与已选状态独立。底部独立分隔快捷键提示。单个字符的实际大小由终端字体决定。

消息区的 ask 卡片完整显示问题正文、各选项说明和 Markdown 预览，沿用 transcript 滚动；提交答案、缩放及恢复后保留详情，选中项在消息中标记为 `●`。ask 不提供 Ctrl+O 详情展开。面板高度在打开／resize 时确定，最多屏幕 70%；短屏进入紧凑模式。滚轮在面板内导航，面板外继续滚 transcript。面板活跃时全局菜单、F1、切档、输入历史和主编辑器不会接收它的按键／粘贴，只有 custom 编辑器显示插入光标。custom 编辑器的 Esc 只丢弃本次编辑；picker／Review 的 Esc 取消整个任务，不提交部分答案，同批后续工具按既有取消规则跳过。Ctrl-C 仍立即退出并恢复终端，不等待回复线程。

成功工具结果精确使用 `{"ok":true,"answers":[{"id":"storage","selected_options":["Postgres"],"custom_input":null}]}`：题目和多选答案分别按原题／原选项顺序排列，不包含推荐徽标；custom 有值时为原始字符串。问题与答案沿用 ToolStarted／ToolFinished 日志，恢复只重放卡片，不重开表单、不把答案放进主输入历史；中断请求显示既有 interrupted 状态。`/new`、跨协议 `/model` 和 `/resume` 后 TUI 仍提供 ask。`-p` 和管道行模式不提供 ask schema；服务端仍返回 ask 调用时固定报错 `ask requires interactive TUI mode`，不读取 stdin，不消耗下一条任务。

## 项目级 AGENTS.md

pp 在每次模型请求前完整读取启动目录的 UTF-8 `AGENTS.md`，并将正文、来源路径和适用目录加入系统提示词；不搜索父目录、Git 根目录或子目录。文件不存在或仅含空白时不追加项目规则，也不会自动创建文件。拒绝符号链接、目录及其他特殊文件；读取失败或 UTF-8 无效时阻止本次请求并报错，修复文件后可在当前交互会话重新提交任务。

刷新覆盖普通请求、工具执行后的下一轮请求和手动／自动压缩。文档修改或删除在下一次请求生效，不重复追加旧正文。启动参数恢复会话时从恢复后的工作目录加载；TUI `/resume` 保持当前工作目录并加载该目录的规则。恢复日志里的旧规则不会覆盖当前文件。项目规则计入完整请求的上下文估算；新建空会话和 `/new` 后仍显示 `ctx 0`。

TUI 和行模式支持不带参数的 `/init`：使用当前模型和现有工具检查项目，创建或更新启动目录的 `AGENTS.md`。已有文档保留有效规则，修正过时的项目描述、命令、路径和流程。命令沿用当前会话、请求上限和工具保存限制；TUI 显示 `/init`，Esc 可取消任务，已落盘修改不回滚。`pp -p '/init'` 仍原样发送普通提示词，不解析命令。

系统提示词固定要求模型在代码变更导致 AGENTS.md 过时时同步修正文档，保留仍有效的项目约束，不得仅为掩盖违反规则而改写规则。`/init` 的实际分析和修改由当前模型执行；程序不会替代模型推断项目事实或保证识别所有文档冲突。

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
- 每次交互启动只加载一次；修改、新增或删除模板需要重启，`/new` 不重载。运行中模板按所选消息类型入队，提交时只展开一次，应用后才回显展开正文（`/init` 仍回显原命令）；取回保留原 slash，重新提交才重新展开。配置不可用时仍可查看候选和帮助，但不能执行模板。未知命令仍是本地错误，多行输入仍作为普通任务；`-p` 不扫描命令目录，始终原样发送。
- 本次进程的 ↑ 输入历史保留原始 slash 输入；对话记录和 session 保存实际发送的展开正文。`-r` 恢复后的新命令使用本次启动的模板，旧历史不随文件修改而改变；`-c` 原样重放保存的展开内容，不重新展开。

## 上下文用量与压缩

- **用量可见，不增加查询命令。** TUI 在输入栏上沿右端显示 `ctx N/W`，未知容量时只显示 `ctx N`；ctx、in、out、cached 小于 1000 时显示完整整数；这些字段和 tok/s 达到 1000 时使用 k，达到 100 万时使用 m，换算后保留一位小数（如 `999`、`1.0k`、`12.3k`、`1.0m`），目前只支持 k 和 m。tok/s 未换算时也保留一位小数。普通用量为蓝色，达到配置阈值为黄色，达到容量为红色。CLI 每个任务结束（含失败）向 stderr 输出当前模型、完整整数的 `Context: ~N / W tokens (P%, estimated)`、最近普通请求的 input/output 和自动策略；stdout 仍只有模型回答。显示用量不发网络请求。
- **实时请求指标。** TUI 在 `ctx` 左侧依次显示带 Nerd Font 图标的 speed（金色）、in（蓝色）、out（绿色）、cached（紫色）。流中速度使用约 2 秒滑动窗口内的输出 UTF-8 字节增量除以 3 估算，从首个输出开始采样，排除首 token 等待，每 250ms 更新一次显示快照，速度数值不带 `~` 前缀；正文刷新不会改变该快照。工具执行、请求间隙及下一请求等待期间保留上一笔速度，直到新请求产生有效采样，避免频繁闪回 `—`；停流时估速随窗口降至 0；计入可读思考、回答、拒绝及工具参数 delta，不计完成事件的重复全文、工具名或 opaque reasoning。成功完成后以服务端 output tokens / 完整请求耗时校正为平均速率并持续保留；没有 usage 时按本轮累计字节与完整请求耗时保留最终估速，没有流式字节则保留上一笔速度（无历史时为未知）。in/out/cached 是最近一次已完成的普通请求，不累计任务或会话；新请求生成期间保留上一请求计数，成功但没有 usage 时变回 `—`，明确的 0 仍显示为 0。input 包含 cached，cached 只是其明细，不加减 input；output 包含供应商计入的 reasoning。失败或 Esc 取消清除速度但保留最近成功计数。`/new`、切模型／effort、恢复、会话树跳转和成功压缩清空指标；失败操作保留完成态。摘要流隔离，不覆盖普通请求指标。窄屏优先保留最右的 `ctx` 和至多半行临时状态，依次整块隐藏 cached、in、out、speed；不拆开图标。指标仅运行时使用，不写 session journal，不改变 CLI 输出协议。
- **估算不是 tokenizer。** 新建空会话及 `/new` 后显示 `ctx 0`，未发送的输入草稿不计入用量。有对话历史后，`~N` 是整份当前原生请求序列化为 UTF-8 JSON 后的字节数向上除以 3，包括 system/instructions、工具 schema、参数、结果及原生／加密 reasoning 的 JSON 开销；不保证准确率或一定不会超限。百分比向下取整，允许超过 100%。最近请求的服务端 usage 独立显示，不累计为上下文占用，也不重复计算 cached/reasoning 明细；没有 usage 时 CLI 显示 `unavailable`，TUI 计数字段显示 `—`。初始化、恢复、`/new`、切模型／effort、会话树跳转和压缩成功后清空最近 usage，摘要请求不覆盖它。
- **容量必须显式配置。** 每模型 `context_window` 为可选的正整数 token 数；省略表示未知，拒绝 null、0、负数、小数、字符串及溢出，未选中的模型也校验。不猜测模型名，不远程探测容量。`auto_compact` 默认 true，`auto_compact_threshold_percent` 默认 80 且必须为 1..=99 的整数；修改后重启生效，无运行时开关命令。当前模型没有容量时显示 `Auto compact: inactive (context_window not configured)`，不显示虚构的百分比；明确关闭则显示 `off`。切模型只用目标模型的容量，恢复只用当前模型目录配置，不继承旧模型或日志中的容量。
- **主动 `/compact`，自动在每次普通请求前检查。** `/compact` 不接受参数，支持 TUI 和行模式；`-p '/compact'` 仍是普通提示词。自动检查包含刚接受的用户输入及同一任务内完整工具批次的增长；用量达到阈值时先摘要，再继续任务。手动压缩不要求已知容量；空历史、只有待回复用户输入或只有一个完整 exchange 时返回 `Nothing to compact.`，不发请求、不创建空日志。TUI 仅空闲时接受命令，运行中草稿可编辑，Esc 等待实际取消终态后恢复；自动压缩仍属于当前任务，手动压缩不产生虚假的任务 Completed 或 `You: /compact`。
- **当前模型总结旧前缀，近期交互原生保留。** 完整 assistant 回复或 assistant calls 与该批全部 tool results 构成安全 exchange；Responses 的 reasoning 与输出／calls/results 不拆开。至少保留最后一个完整 exchange 和其后待回复用户输入；后缀预算为 `min(16384, context_window / 5)`，容量未知为 16384 tokens，最后一个 exchange 超预算也整体保留。旧摘要参与再次摘要，不叠加摘要链。摘要只发送当前 system/instructions、旧原生前缀和固定摘要请求，不发送近期后缀，不配置或执行工具，不改变当前 effort。摘要目标为 `min(2048, max(1, context_window / 16))` tokens（未知容量为 2048），输出上限为目标的两倍；候选为一个 `[Context summary]` 用户消息加原生后缀。可见对话和原始日志不删除，也不减少日志磁盘占用。
- **有限失败，不重试或裁掉最新 exchange。** 已知容量时，摘要请求的估算输入加输出上限必须不超过容量；不做分块摘要。空白、拒绝、工具调用或截断的摘要不能提交。手动候选未变小则保持原历史；自动候选还必须严格低于触发阈值，否则报 `cannot compact: retained context exceeds automatic threshold` 并停止当前任务。系统／工具 schema、当前输入或最新 exchange 太大而无可压前缀时同样有限失败。网络失败、取消或 state 写失败均恢复原生历史，不执行摘要工具，不继续普通请求，不重放已执行工具。手动取消不改写历史任务的 done/aborted 状态；自动失败／取消走当前任务的终态处理。

上下文回归覆盖双协议 usage-only／completed usage、手动与任务内自动压缩、容量与额度门槛、摘要失败矩阵、连续恢复的 native append cursor、真实 PTY 的 Esc／草稿／阈值颜色／60 与 30 列 resize，以及 `ulimit -f` 残尾恢复。独立临时 Python 标准库 runner 在 loopback HTTP + 100×30 PTY 上验证两协议的手动压缩、恢复和两个只读批次后的自动摘要；未使用个人配置或真实远程供应商，Linux 未在本次环境实机验证。

## 会话树与 Esc

运行中按一次 Esc 中断当前任务，等待连接／受控进程回收和历史保存完成后恢复输入。空闲、输入完全为空且没有其他面板时，500ms 内连续两次 Esc 打开 `/tree`；其他输入、粘贴或任务状态变化会重置双击。关闭菜单、取消 ask 或中断任务的 Esc 不计入双击；Ctrl+C 仍退出程序。

TUI 空闲时提交无参数 `/tree`，在原消息区域浏览当前有效历史的完整消息卡，复用普通对话的 Markdown、工具、diff 与 ask 渲染，不另开弹窗或搜索栏。回退后放弃的消息永久退出有效历史，恢复会话和后续 `/tree` 均不可找回；`b` 不执行任何操作。进入时所有消息左右各预留一列；移动候选只移动外围虚线框，不改变正文宽度、换行或组高。标签保留当前路径圆点与角色，状态行显示 `[current]`。回退到首条问题之前时，有效历史为空，再打开 `/tree` 通知 `No entries found in session tree.` 并关闭预览。加载期间保留普通消息区域，在原输入区显示加载状态与提示。

底部提示为 `↑/↓ step  ←/→ user turns  f filter  󰌑 rewind  󱊷 cancel`。↑/↓ 前后选择安全节点，首尾不循环；←/→ 按有效历史顺序跳到前后用户节点；Home/End 选首尾。`f` 循环全部／隐藏工具／仅用户，尽量保留节点身份；候选被隐藏时选择其后的可见节点，否则选最后一个。PgUp/PgDn 和滚轮只滚动消息物理行，不改变候选；后续方向键导航会重新显示候选。Enter 跳转，Esc 一次退出；普通输入、Ctrl+O 与粘贴只消费，不搜索也不改草稿。工具说明、整批结果与 diff 作为一个安全节点整体框选，不能在调用和结果之间截断。超高消息仅绘制真实边界，裁切处不伪造闭合边框；窄屏裁切提示而不换行，宽度不足三列时省略外框。取消后恢复原消息 viewport、草稿与输入历史；退出时只把当前分支的普通对话写入 scrollback，不写入全树预览、虚线框或提示。

选择用户消息会回到该问题之前，并在输入框为空时回填问题；选择助手回复或工具批次会保留该节点的完整上下文。选中当前叶子只关闭面板。回退不请求模型、不重跑工具、不回滚项目文件或 bash 副作用。模型、思考级别、当前工作目录与工具配置保持当前值；跨协议复用历史转换，不支持转换时拒绝回退并保留当前状态。项目指令按当前文件刷新。标签和历史 ask 重新作答暂未提供。

JSONL 采用逻辑失效，不删除旧行，也不添加逐条 tombstone。回退同步追加带 `data.tree.leaf` 和完整目标原生 session 的现有 `pp.session_state`；只有提交成功才替换内存历史和画面，失败保留此前历史和草稿。原始消息、native append、显示事件和旧 state 保留原字节以供审计，不承诺秘密擦除或节省磁盘；物理 `parentId` 仍连接追加末尾，不表示消息仍有效。例如 A → B → C 回退到 B 助手后，即使立即退出，恢复、回放、列表摘要及 `/tree` 也只使用 A/B；新输入 D 后是 A → B → D，C 的 ID 不可再选择。回退到首条用户消息之前提交 `leaf:null` 和空对话，列表仍保留该 `Untitled session`。恢复原生上下文使用最新完整 state 加后续 native append，不从旧普通消息补齐。

树完整解析和校验旧日志后，仅输出最终已提交的根到叶路径，避免读取废弃节点的展示区间。旧 v3 线性日志及含合法历史跳转的旧日志均无需迁移；升级后只保留最终有效路径，不提供找回废弃分支的入口，也不支持手工编辑 JSONL 改变有效性。读取不改写文件字节或 mtime。行模式提示 `/tree requires the TUI`；`pp -p '/tree'` 仍是普通模型任务。

## Session 持久化与恢复

TUI 空闲时提交 `/resume`，打开 omp 风格的全屏 `Resume session` 面板。默认只显示当前工作目录的会话，→/← 或 Tab 切换当前目录和全部项目。顶部输入搜索词，按标题、回答预览、ID 和目录筛选，支持粘贴、Backspace 删除字符和 Ctrl+U 清空查询。每条会话展示首条用户消息摘要、首条非空助手文本预览、相对更新时间、文件大小及 `current`／`✓ done`／`■ aborted` 标记。摘要折叠空白并最多保留 80 个 Unicode 字符，预览最多 200 个，超长追加 `…`。完成状态根据最后用户／助手记录及错误判断，不调用模型生成标题。

会话按文件最后修改时间倒序（相同时间按完整 ID 倒序），默认选中第一项。↑/↓、Home/End、PgUp/PgDn 导航，滚轮移动选择，Enter 恢复，Esc 取消。筛选或切换范围后重新选中第一项；窗口缩放保留选择。Delete 或空查询时的 Backspace 请求删除，面板内 Enter/y 确认、Esc/n 取消；当前会话及持锁会话不可删除。扫描、恢复和删除均在 worker 中执行。单个坏文件跳过并提示数量，未产生用户消息的日志不展示；已提交根回退的空历史仍以 `Untitled session` 展示，选中时才做完整恢复校验。浏览和搜索不加恢复锁、不改变日志字节或 mtime。

加载时取消会保留旧界面，但扫描结束前 worker 仍忙碌，迟到回复不会重新打开列表；恢复或删除提交后不能取消或重复确认。选择当前 ID 仅关闭列表，不重加锁、不重放历史。

`/resume` 恢复所选会话：按当前叶子的逻辑路径替换可见对话、模型上下文和用户输入历史，保留当前模型、协议、思考级别、工具限制、edit 模式和工作目录；恢复目标 workflow，按目标模式停止或重建 MCP，并在 UI 接管新清理句柄后完成交接。保存时的模型被删除或修改、原目录已不存在，也可以恢复；跨模型或跨协议时转换历史，去除绑定旧模型的 Responses 状态，同一模型则保留原生推理历史。后续输入继续追加原 JSONL 和当前分支，不重跑旧工具。失败时仍可在旧会话继续输入。行模式仅提示 `/resume requires the TUI; use pp --resume SESSION_ID.`，不支持 `/resume ID`；按 ID 启动恢复仍使用启动参数。

下表的自动续跑行为适用于 Execute；Plan session 的 `-r`／`-c` 例外见上方 Plan 节。

| 命令 | 启动行为 | 任务后行为 |
|---|---|---|
| `pp` | 新的惰性 session，不请求模型 | 原有 TUI / 行模式 |
| `pp -p TEXT` | 发送一次新任务 | 退出 |
| `pp -r ID` | 精确恢复，不发送 HTTP | 等待输入；行模式可 EOF 退出 |
| `pp -r ID -p TEXT` | 恢复后追加一次 TEXT | 退出 |
| `pp -c` | 选择全局最近修改的 session，刷新项目规则和技能摘要后发送已保存历史 | 继续交互；行模式 stdin EOF 退出 |
| `pp -c ID` | 精确恢复，刷新项目规则和技能摘要后发送已保存历史 | 同上 |

`-c` 不附加“请继续”、不重复最后一条用户消息、不回滚助手回答，也不重放旧工具调用。只能使用完整 ID：`YYYY-MM-DDTHH-MM-SS-<32位小写hex>`，例如 `2026-10-17T01-02-03-0123456789abcdef0123456789abcdef`；日期时间使用创建时的本机时区，精确到秒，随机后缀避免同秒冲突。不支持前缀或文件路径，旧的 Unix 毫秒格式 ID 不再接受。`-r` 缺 ID、`-r` 与 `-c` 同用、`-c` 与 `-p` 同用均报错。

会话日志唯一位置为 `~/.pp/agent/sessions/<ID>.jsonl`。启动、草稿和本地命令不创建 session 目录或文件（运行诊断日志独立创建）；只有真实 HTTP stream 打开调用返回（收到 headers 或请求失败）后才创建。因此请求仍在等待首次 headers 时被强杀，不保证已有日志。主动 Esc 取消是例外：即使 headers 尚未到达或任务尚未发送 HTTP，也同步保存已接受的用户输入和取消终态，供后续恢复。ID 使用 16 字节 OS 随机数；新目录权限 0700，新文件 0600，不改已有目录权限。日志是敏感明文，包含完整对话和工具结果，不保存 Config、API key 或 HTTP headers；不要提交或随意共享。

启动参数 `-r` / `-c` 使用原工作目录、edit 模式、工具限制、模型引用、思考级别、基础系统提示词和完整历史；请求前按当前文件刷新项目规则及技能摘要，并继续追加原文件，不分叉。原目录缺失或不可进入时报错，不回退到启动目录。模型必须仍存在于当前 catalog 且实际 model/protocol 未改变；凭据、base URL、HTTP 超时和每任务请求上限使用当前配置。启动恢复失败是非零退出，不进入禁用模型的交互兜底；运行中的 `/resume` 恢复失败保留旧会话、工作目录与列表选择，不发送模型请求。

TUI 恢复可见消息、工具结果、错误和用户输入历史，输入框为空；不恢复草稿、菜单、滚动位置或布局缓存。恢复的 bash 卡片仅显示保存的退出状态，不伪造耗时。未完成展示事件显示 `Previous request was interrupted.`，不加入模型上下文。行模式不重印旧回答。恢复 ID 与首次创建 ID 分别通过 stderr / TUI Note 提示 `Resumed session: <ID>`、`Session: <ID>`；stdout 仍只有模型回答。

“最新”按文件修改时间选择，时间相同按 ID 字典序降序；只查看恢复不会提高排序，不按项目过滤。session 文件必须是普通文件，不跟随符号链接；进程持有排他锁，另一进程恢复同一 ID 会报 `session is already in use`。最新文件损坏或已锁时直接报错，不换另一个文件。

JSONL 使用 oh-my-pi 风格容器：首行是 `type:"session"`、`version:3` 的 header，保存现有完整 session ID、原工作目录和 UTC RFC3339 毫秒创建时间；其余条目都有 `id`、`parentId`、`timestamp`，形成线性链。正文 ID 是当前日志内唯一的 8 位小写随机 hex，首条 parent 为 null。支持的正文类型只有 `message`、`model_change`、`thinking_level_change`、`custom`；普通消息提供 `user`、`assistant`、`toolResult` 及 text/thinking/toolCall 内容，供通用阅读，不虚构 usage/cost。

pp 的无损恢复使用 `custom` 扩展：`pp.session_state`（独立原生版本 1）、`pp.native_append`、`pp.instructions` 保存协议原生请求及增量；`pp.workflow`（version 1）与 native state 同步提交模式、绑定计划和审查报告，checkpoint 同样携带 workflow。缺少 workflow 的旧 v3 session 默认 Execute；未知字段／版本、非法 ID／修订／状态组合明确失败。审批与状态提交失败会截断并同步回滚到原提交边界，不先在内存发布执行权限；`pp.event`、`pp.error` 保存展示事件与错误；`pp.cancelled`（`data:{}`）同步保存主动取消终态，恢复时显示取消 Note，不误报中断，也不进入模型上下文。取消后的列表状态为 aborted，后续真实任务完成可再次标为 done。普通 assistant/toolResult 消息不反推模型上下文，也不重复渲染；TUI 仍从独立展示事件恢复。`model_change` 仅作审计，后续完整 native state 才提交模型切换。这是结构对齐，不支持直接互读上游会话文件。旧 `checkpoint` 格式明确报 `unsupported legacy session format`，不迁移、不改写；最新文件为旧格式时也直接失败，不跳过。

首个 `pp.session_state`、用户原生输入、每个完整模型/工具批次及设置更改均同步落盘；流式展示事件不逐 token fsync。原生历史与展示日志分离，Responses item ID、encrypted reasoning、call_id 及工具原始结果均保留。完整工具批次在下一次请求前提交；网络错误不撤销已执行工具。写入失败锁存首次 `persisting session` 错误，当前已接受工具批次仍执行完毕，停止后续 HTTP；`/new` 或重启后才可重新发送。模型/思考级别设置写入失败保留旧内存选择。

压缩通过同步追加新的完整 `pp.session_state` 提交替换后的原生历史，同时更新增量游标；不虚构 `model_change`，成功后立即退出也能恢复摘要与近期后缀。`ContextUpdated`、请求开始／输出进度／完成指标、压缩阶段／结果事件及摘要调用的 assistant/thinking 流不写 `pp.event`；恢复重新估算，不恢复旧 usage。逻辑提交点是完整 state 记录，不是文件 rename，也不承诺断电绝对原子性。

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

TUI 输入 `/skills` 打开全局管理面板：↑/↓ 选择、Space 勾选、Enter 保存、Esc 放弃本次选择，r 重读目录配置并扫描（会重置未保存选择），PgUp/PgDn 滚动详情及诊断。面板显示名称、说明、来源和启用状态；配置中的目录通过编辑 settings 修改。保存仅更新 `disabled_skills`，保留其他字段；常规块映射尽量保留原有注释，流式映射等特殊格式会重新排版为 YAML 块映射，使用临时文件原子提交；失败保留原运行状态与面板选择。运行任务时拒绝管理并保留草稿；缺少或无效模型配置时仍可管理 skill。行模式支持 `/skills` 查看列表，以及 `/skills enable 名称`、`/skills disable 名称`。

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
- 默认前台 `bash({"command":"..."})`：以启动目录为工作目录执行 `bash -c`，关闭 stdin，并建立独立进程组。stdout/stderr 无需等待换行或进程退出即可实时显示；保留跨分块的 UTF-8 字符，无效字节采用 lossy 解码。两流合计最多保留 500 行，同时每流最多保留 65,536 个原始字节，先达到的限制生效；按 `\n` 分行，非空且没有末尾换行的片段也计一行。仅在确有超出内容时标记对应流截断；实时显示与返回输出使用相同限制，多余内容持续排空，命令继续执行。仅保证各流内部顺序，不保证跨流顺序；500 行额度按接收分块的顺序分配。返回退出码、输出、截断标记和 `timed_out`，信号退出时退出码为 null。
- Arguments are strictly validated: missing or extra fields, invalid JSON, blank path/command, unknown tools, and file/process errors all return `ok: false` to the model, allowing it to correct itself. A non-zero bash exit likewise returns `ok: false` with the output preserved.
- 前台 bash 的 `bash_timeout_seconds` 默认 120，与模型请求超时独立，模型不能通过工具参数延长。`{"command":"..."}` 和显式 `"mode":"foreground"` 保持相同契约。独立监督线程在超时时向进程组发送 TERM，等待 1 秒后发送 KILL 并回收直接子进程；可停止的非阻塞管道读取避免后代持有管道导致无限等待。超时返回 `ok:false, timed_out:true`，保留已捕获输出，不回滚 shell 修改。正常结束也清理同一进程组的残留后代；主动脱离进程组的进程不受保证。TUI Esc 取消通过 TERM→1 秒宽限→KILL／wait 停止受控进程组，前台结果为 `ok:false, cancelled:true, timed_out:false`；正常／超时前台结果的 `cancelled` 为 false。
- 跨工具调用验证服务时使用 `bash({"command":"exec server ...","mode":"background"})`，保存返回的 `task_id`（如 `bg-1`）。后台没有固定执行超时，但只属于当前用户任务：最终回答、拒绝、请求预算耗尽、错误、Esc 取消或 TUI 受控退出前自动停止并清理。运行中补充沿用当前任务的后台服务；独立 FollowUp 结束时单独收尾。不会跨独立任务、会话恢复或应用重启保留；历史 ID 不会复活进程。服务应保持在受管进程组内，优先 `exec server`，不要 daemonize／setsid。shell `&` 不是受管理后台模式。
- `bash_task({"task_id":"bg-1","action":"status"})` 立即返回当前累计日志／状态，不等待就绪；`"action":"stop"` 发出本任务的停止请求并等待 TERM／KILL／wait、worker 和 scratch 收尾，不取消整个用户任务。验证完成后显式 stop；已终结任务的 status／stop 保留原终态，stop 幂等。仅接受本管理器当前任务的 ID，不接受任意 PID；上一用户任务的 ID 已清除，新 ID 不复用。mode/action 仅接受上述小写枚举，拒绝 null、额外字段和空白 command／task_id。
- 后台 start／status／stop 共用快照：`ok`、`task_id`、`status`、真实 `exit_code`（或 null）、`stdout`／`stderr` 和两流 `*_truncated`。状态为 `running|exited|stopped|cancelled|failed`；前三者 `ok:true`，后两者 `ok:false`。自然 exit 0 为 exited，非零／信号退出为 failed；显式停止为 stopped，用户取消为 cancelled。仅实际出错时附 error／warning，无前台 timed_out 字段。真实进程清理失败会停止 agent，普通命令非零退出不会被误当作清理失败。
- `running` 仅表示进程受管理，不表示服务 ready；通过 status 取得日志，再以后续实际 HTTP／功能请求验证，不自动探测端口、不重启。后台日志沿用共用 500 行和每流 65,536 原始字节限额，超过后持续读取并丢弃，不阻塞服务。运行中暂不显示不完整 UTF-8 尾字符，终态 lossy 解码剩余尾字节。后台 worker 不主动推送日志；每次 start／status／stop 用该次 call_id 展示累计快照，因此重复 status 可重复展示同一日志。CLI 将日志与 `task bg-N: status` 写到 stderr，stdout 仍只包含模型回答。TUI 卡片显示 `Task bg-N: status`，卡片完成仅表示本次工具调用完成，不表示服务退出。
- bash 默认将 `TMPDIR`、`TMP`、`TEMP` 设置为项目内每个命令独立的 `.pp-tmp-*` 目录；前台调用结束后清理，后台随任务存活直到进程收尾后清理；`XDG_CACHE_HOME` 设置为项目内 `.pp-cache` 并保留缓存，二者均已加入忽略规则。这是默认位置而非权限隔离；不遵守这些变量的程序仍可能使用其他位置。清理失败通过结果中的 `warning` 报告。
- Esc 取消会关闭本地模型连接及 SDK 后台网络任务；下一任务惰性创建新的 runtime／HTTP 连接池。流式半成品只保留展示，不进入原生历史或执行未完成工具调用。已开始的同步 read/edit/write/load_skill 操作允许完成并保留真实结果；同批剩余调用不执行，但仍按 call_id 追加取消结果。只有终止／回收成功且原生批次与终态同步保存成功才显示取消；真实清理失败或持久化错误优先报告错误。只能保证本地连接及同进程组受控工作停止，不能撤销供应商远端计算、计费、外部副作用或主动脱离进程组的进程。
- Tools execute serially; dependencies within a batch such as write → read become visible in order. The complete model output is saved first, then tool results are appended one by one with matching call IDs; a tool failure never skips the remaining calls in the batch.
- `read_max_bytes`、`read_max_lines`、`bash_timeout_seconds` 均要求正整数，启动时读取并固定；`/model`、`/new` 不重新读取或放宽这些限制。
- Each user task allows at most `max_model_requests` (default 20) model requests, including automatic summary requests; automatic compaction requires two remaining slots for the summary and the following ordinary request. Manual `/compact` is independent of the next task's budget. The final batch at the limit still executes and returns into in-memory history before the limit error is raised; no next request is sent and no completion is claimed. Pending steering at an otherwise final reply also reports the limit and remains queued for explicit resume. Each independent task restarts the count; steering never resets it.
- Ordinary requests use the current native context, which may contain a compacted summary; the append-only JSONL and visible conversation retain the original messages and events. Accepted input is saved before automatic compaction can fail locally. Network/protocol errors do not roll back real file changes and never retry automatically — except an opening-phase HTTP 429, which is retried within `rate_limit_retry_budget_seconds`; previously completed history and tool results remain recoverable. See session recovery below.
- `/model` selects only from the fully validated startup catalog; it neither reloads nor writes configuration. Selecting the current reference is a no-op with confirmation. Failed resolution/client initialization/history conversion preserves the current model and history. Switching does not probe HTTP, execute tools, retry, or fall back; service compatibility is checked by the next actual request.
- Switching preserves user/assistant text, system instructions, tool-call IDs/arguments, and tool results. Completion-to-Completion keeps native messages; Responses within the same configured provider keeps native output, including encrypted reasoning. Cross-protocol or cross-provider Responses switching converts readable history and strips provider item IDs, replay metadata, and opaque encrypted reasoning. Readable reasoning summaries/content become assistant text; interleaved Responses assistant messages/calls are batched for Completion. Unsupported non-text or unknown history aborts the switch rather than losing records. Encrypted vendor state is not portable.

取消回归通过真实 loopback HTTP、PTY 和进程组验证：两协议 headers 前／SSE 空闲取消在下一任务前关闭旧连接，write→bash→write 保留已完成结果并补齐未执行调用，MCP 仅在下一任务调用时重建并拒绝 schema 变化，重建中 Ctrl-C 仍回收替换进程；取消终态可恢复为 aborted，后续完成可变为 done，持久化失败不误报取消。独立 100×30 实际 TUI smoke 使用临时 Python loopback fixture：TERM-resistant bash 两个受控 PID 均停止，中文草稿和光标保留，下一任务收到 `cancelled:true, timed_out:false` 与原 stdout 并回复 `smoke recovered`，共两次 HTTP，最后 Ctrl-C 以 0 退出并恢复终端属性。未使用真实供应商，Linux 未在本次环境实机验证。

## HTTP Protocol

The request shapes below describe ordinary task requests; dedicated summary requests omit tools and tool choice. `async-openai` creates and consumes typed SSE streams for both APIs; streaming is the only execution path, with no non-streaming fallback or extra request on failure. A Tokio runtime bridges SDK calls to the synchronous Agent and TUI worker; model requests never run on the TUI drawing thread. Each request uses `request_timeout_seconds` (default 120); automatic redirects are forbidden, and the only retry is an opening-phase HTTP 429: retried within `rate_limit_retry_budget_seconds` (default 300, 0 disables) using integer `Retry-After` seconds when present (0 retries at once) and exponential backoff otherwise, clipped to the remaining budget, capped at ten attempts, and cancellable while waiting; mid-stream rate-limit errors never retry. SDK environment-derived credentials, API base, organization, and project are explicitly overridden; configuration remains file-only.

- **`response` (default):** sends `model`, `instructions`, current local `input` history, function `tools`, `tool_choice: auto`, `stream: true`, `store: false`, and `include: ["reasoning.encrypted_content"]`. It does not use `previous_response_id` or server-side conversations. 正文和拒绝文本按输出项及内容顺序显示；可读思考摘要和思考文本默认单独显示，摘要的 delta、done 和 completed 不重复输出。参数分片不显示，加密思考内容不显示。 `response.completed.response` is the sole source of committed output and calls; it releases the turn immediately without waiting for HTTP EOF or `[DONE]`. Response/message/reasoning/function-call statuses must satisfy completed validation. Message, reasoning (including encrypted content), and calls are retained in native order; results use matching `call_id`. `/new` clears input and retains instructions.
- **`completion`:** sends `model`, `messages`, function `tools`, `tool_choice: auto`, `stream: true`, and `stream_options.include_usage: true`. Chunks use the SDK schema (`id`, `object`, `created`, `model`, indexed choices); only choice index 0 is consumed, while the last non-empty usage is retained even in a usage-only chunk with empty choices. Missing usage is accepted without retry or protocol fallback. An actual assistant role and a `stop` or `tool_calls` finish reason are required. Tool fragments are accumulated by index and validated only after the stream ends without error. Truncation, filtering, and EOF before finish are failures. The SDK consumes `[DONE]` internally; a legal finish plus clean stream termination is accepted even if the server omitted `[DONE]`. `/new` keeps only the system message.

Both APIs validate the entire batch before saving assistant output or executing tools: only function calls with non-blank IDs and names are accepted, and call IDs cannot repeat within a batch. A bad batch never executes even valid writes from that response. Neither readable text nor calls is a protocol error. All text parts are concatenated even in tool turns; content takes priority over refusal. Final Responses text may add a missing suffix, but conflicting streamed non-blank content fails the turn before tool execution. A broken stream retains visible partial text, marks the TUI entry failed, and reports a redacted error without committing incomplete assistant/tool history; the submitted user message and earlier completed tools remain. Tool arguments decode at execution time per tool type. Legacy Chat `function_call` 和托管工具仍不支持；Chat 额外读取 choice 0 的 `delta.reasoning_content` 或 `delta.reasoning` 字符串，仅用于显示，不写入回答正文。 Errors never dump provider messages, response bodies, or keys.

### Responses 故障排查

- 部分模型始终启用思考，不接受 `reasoning.effort: "none"`。旧配置 `think_level: default` 在当前版本是 `none` 的别名，**不是服务端默认值**。可在该模型配置 `thinking.efforts` 排除 `none`，并用 `thinking.defaultLevel` 指定支持的启动档位。若服务端仍拒绝请求，在交互模式执行允许的 `/effort low` 后重新提交任务；启动默认值在 `~/.pp/agent/settings.yml` 中改为 `think_level: low`，或选择服务端支持的其他档位。已恢复的 session 保留其保存的档位，需要执行 `/effort low`；仅修改启动设置不会覆盖已恢复的档位。
- 流式终止错误区分 `response.failed`、`response.incomplete` 与 `error`，仅展示白名单内的错误码／未完成原因；服务端原始 message、param、未知 code/reason 均不回显。`invalid_request` 提示检查模型支持的思考档位和请求参数；`max_output_tokens` 表示输出截断，`content_filter` 表示内容过滤。失败不自动重试、不降级协议、不提交未完成的回答，也不执行该批次工具。

## Verification

本地开发与未来 CI 的默认测试运行器为 [cargo-nextest](https://nexte.st/docs/running/)。未安装时先执行 `cargo install cargo-nextest --locked`，不回退到 `cargo test`。

```sh
cargo nextest run --locked
```

`.config/nextest.toml` 的默认 profile 设置 `fail-fast = false`，失败后继续运行其他测试并统一报告；保留 nextest 默认的 CPU 数并发与零重试，不排除已有失败。`--test-threads=1` 直接传给 nextest，不放在 `--` 后。需要查看成功测试输出时使用 `--success-output immediate`；`--no-capture` 会强制串行，不能用它衡量默认并发收益。

nextest 不运行 doctest；当前 crate 只有 binary target，没有 library doctest。以后新增 library target 时，须另行运行 `cargo test --locked --doc`，不能用 nextest 替代。仓库目前没有 CI 配置或独立测试脚本，默认入口由本节与 `AGENTS.md` 约定，不修改 Cargo 内置的 `cargo test` 行为。

### cargo-nextest 切换实测

2026-10-22，macOS arm64、14 个逻辑 CPU、24 GiB 内存，rustc/cargo 1.99.1、cargo-nextest 0.9.149。先完成一次构建预热，再交替执行三轮；每次完整运行同一组 679 项测试（433 单元 + 246 集成、2 个 binary），不改测试源码、不跳过、不重试，使用默认并发。旧入口原样执行会在单元测试失败后停止，因此公平基线使用 `cargo test --locked --no-fail-fast`，切换后使用 `cargo nextest run --locked`。时间来自 `/usr/bin/time`，包含 Cargo/runner 启动与热缓存构建检查，不包含首次编译成本；两种运行器未同时运行。

| 指标 | Cargo 基线 | nextest 默认入口 |
|---|---:|---:|
| 第 1 轮墙钟 | 51.27 s | 40.99 s |
| 第 2 轮墙钟 | 49.33 s | 41.06 s |
| 第 3 轮墙钟 | 49.81 s | 41.48 s |
| 墙钟中位数 | 49.81 s | 41.06 s |
| CPU 时间中位数（user + sys） | 48.00 s | 51.18 s |
| 各轮失败数 | 8 / 5 / 5 | 6 / 9 / 7 |
| 各轮跳过数 | 0 / 0 / 0 | 0 / 0 / 0 |

本次墙钟中位数减少 **8.75 s（17.6%，约 1.21×）**，但 CPU 时间增加 **6.6%**。nextest 能[跨 binary 调度测试并统一汇总失败](https://nexte.st/docs/design/how-it-works/)，并[逐测试进程隔离](https://nexte.st/docs/design/why-process-per-test/)；进程启动也有成本，不能推导所有测试或冷构建都会更快。失败项与超时会影响本次耗时，以上只是当前失败测试集的观测值，不是全绿测试集的稳定加速保证。

两种运行器均保留两个已有失败：`tui::tests::context_usage_is_right_aligned_on_input_top_without_changing_draft` 与 `tui::tests::context_token_metrics_keep_ctx_and_draft_at_every_width`。PTY 集成测试另有输出断言／等待超时，失败集合随轮次和运行器变化；**全套没有通过，也未证实稳定性改善**。未用重试、过滤或放宽断言掩盖失败。另已实际执行并通过 nextest 的流式双协议输出用例（`--success-output immediate`）及 MCP 多服务用例（`--test-threads=1`），验证迁移后的定向参数有效。`cargo test --locked --doc` 实际返回 `no library targets found`，与当前 binary-only 结构一致。未在 Linux 上测量。

Default replace example after reading `hello`:

```json
{"path":"demo.txt","edits":[{"oldText":"hello","newText":"edited"}]}
```

With `"edit":"hashline"`, after reading `1:a430d84680aabd0b|hello`:

```json
{"path":"demo.txt","edits":[{"op":"replace","start":"1:a430d84680aabd0b","end":"1:a430d84680aabd0b","lines":["edited"]}]}
```

```sh
cargo nextest run --locked context_ --test-threads=1
cargo nextest run --locked --test cli_agent context_ --test-threads=1
cargo nextest run --locked --test cli_agent session:: --success-output immediate
cargo nextest run --locked --test cli_agent tui_session_ --success-output immediate
cargo nextest run --locked slash_
cargo nextest run --locked file_command_ --success-output immediate
cargo nextest run --locked --test cli_agent tui::tui_slash_file_command_menu_and_submission --success-output immediate
cargo nextest run --locked --test cli_agent session::session_file_command_expansion_survives_resume --success-output immediate
cargo nextest run --locked model_switch
cargo nextest run --locked model_thinking --success-output immediate
cargo nextest run --locked llm::
cargo nextest run --locked --test cli_agent default_reasoning_rejection_allows_manual_effort_recovery_without_committing_failure
cargo nextest run --locked streaming_ --success-output immediate
cargo nextest run --locked replace_ --success-output immediate
cargo nextest run --locked --test cli_agent edit_mode_survives_model_switch --success-output immediate
cargo nextest run --locked hashline_ --success-output immediate
cargo nextest run --locked drains_both_large_pipes_without_deadlock
cargo nextest run --locked responses_invalid_batches_never_write_and_repl_recovers
cargo nextest run --locked --test cli_agent tui:: --success-output immediate
cargo nextest run --locked --test cli_agent plan_ --test-threads=1 --success-output immediate
cargo nextest run --locked plan_ --test-threads=1
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Plan 实现后的最新完整回归（macOS arm64）：`cargo build --locked` 通过；CLI `plan_` 筛选 12/12、全部 `plan_` 筛选 68/68、完整 `cargo nextest run --locked` 816/816 通过（无跳过）；`cargo clippy --locked --all-targets -- -D warnings` 与 `cargo fmt --check` 通过。独立真实 TUI／文件 smoke 的观察与本地 fixture 限制见上方 Plan 节，一次性 provider、配置和项目已清理。以下保留此前各阶段的历史验证记录。

测试维护采用均衡精简：删除估算器自证、转发回声、全文布局/配色快照和重复 CLI 场景，缩减同路径配置矩阵及 PTY 外观断言；不合并无关测试、不增加替代快照。保留双协议、安全边界、历史/文件事务、流异常、取消、终端恢复及真实交互。实际注册测试由 691 项减至 679 项（433 单元 + 246 集成），删除 12 个函数，另 2 个配置测试仅重命名；15 个目标文件净减 616 行。

本次在 macOS arm64 上，15 组定向命令（含 106 项真实 PTY）、单独串行运行的 246 项集成测试、构建和隔离 HOME 的 CLI 写文件 smoke 通过。完整串行命令在单元测试阶段为 431 通过、2 失败：`context_usage_is_right_aligned_on_input_top_without_changing_draft` 期望 `ctx 12k`，实际为 `ctx 12.3k`；`context_token_metrics_keep_ctx_and_draft_at_every_width` 期望 `ctx 12k/40k`，13 列实际为 `12.0k/40.0k`。这两项位于本次删减范围外的 `src/tui.rs`，保留失败及原断言，未为通过而修改生产实现或放宽断言。不据此宣称全套通过或速度改善。

工具加固回归包括：读取字节/行数边界、完整行分页和真实 hashline 行号，原子保存部分写入/同步/提交失败注入、权限保持、越界/符号链接拒绝和硬链接对端保留，以及静默/持续输出/忽略 TERM/后代持有管道的超时。两种协议验证工具失败后的历史记录、恢复和继续执行，跨模型切换及 `/new` 验证启动限制快照。

本次工具加固已在 macOS arm64 验证：104 项单元测试、55 项集成测试（含双协议与 PTY）、构建、Clippy 和格式检查通过。Linux 尚未实机运行；本地夹具测试不代表真实服务商兼容性。

新增搜索工具另经真实构建产物与临时 loopback provider 验证：全后端、仅系统 fallback、rg/find、grep/fd、两组均缺失五种 PATH 组合分别运行 Completion/Responses，确认动态 description、原生命令结果、无匹配成功、非法正则 stderr、带空格路径与 CLI 标签一致。另两次 bash smoke 保留原标签、500 行截断及超时行为。实际 100×30 PTY 观察到 grep 青色／find 普通卡片、紧凑 JSON argv 标题、结束前实时输出及非法正则失败状态；Ctrl-C 退出为 0 并恢复 termios。全部使用隔离 HOME／配置／工作目录，未访问个人凭证；临时脚本已删除。

后台任务另经双协议真实 CLI 与 PTY 验证：受管理 bash 启动实际 `pp stats --port 0`，保留超过前台 timeout + grace 后以前台 curl 获得可解析 JSON／HTTP 200；显式 stop 后 PID、监听端口与 scratch 均消失，仍能继续执行前台工具。最终回答、模型 HTTP 错误、请求预算耗尽、Esc 取消与 Ctrl-C 受控退出均覆盖自动回收；下一用户消息不能查询旧 ID，新 ID 不复用。独立 Python 标准库 SSE provider smoke 对最终构建产物重复 HTTP 闭环，并要求 stop 后 curl 精确返回连接失败码 7；实际输出为 `started: bg-1`、`probe: HTTP 200`、`stopped: bg-1`、`post-stop: connection refused`。日志边界、UTF-8 尾字节、并发清理与关闭后禁止启动亦有进程级回归。一次性 smoke 脚本已删除。

回归测试使用回环 HTTP、真实子进程、独立临时 HOME 下的全局 YAML 配置及临时工作目录； they do not modify the test process environment or access external model services. Both APIs cover ordered tool dependencies, cross-turn history, tool-error correction, invalid batches without partial writes, request limits, timeouts, error redaction, and no automatic retries. Responses-specific cases cover omitted/explicit protocol selection, text aggregation/refusal, encrypted reasoning and complete output replay, optional function-call status, reset preserving instructions, rejected response/item statuses, and recovery without invalid output entering history. Configuration cases cover composite provider/model routing, default ordering, model aliases containing `/`, scoped uniqueness, strict protocol/URL/field types, full validation of unselected providers, removed protocol rejection, and no environment fallback. HTTP regressions include non-JSON 302/401/429/500 errors without redirects or exposed bodies.

Unix regressions additionally drive the TUI through a real PTY (100×30, `TERM=xterm-256color`), restoring the visible screen via vt100: multi-line input, drafts while waiting, paste without sending, real write/read, session history, protocol-error recovery, confirmed reset, unavailable configuration, and Ctrl-C while busy. After exit, termios flags, alternate screen, bracketed paste, mouse protocol mode/encoding, and cursor restoration are checked; the original pipe regressions remain. An SGR mouse-wheel scenario verifies scrolling through a 60-line streamed answer, stable history and CJK draft during incoming output, bottom clamping, and restored following on the next response. Physical mouse wheels and touchpads in specific terminal emulators have not been manually verified. Run alone: `cargo nextest run --locked --test cli_agent tui:: --success-output immediate`.

Streaming regressions gate SSE completion on observed child output, covering both APIs' first deltas, uncompleted call fragments, atomic invalid batches, completed-only text/refusal precedence, missing-tail reconciliation, failed streams and redaction. Bash gates prove both pipes are visible before newline/exit; unit cases cover fragmented/invalid UTF-8 and exact byte caps. Ratatui TestBackend covers all five display categories, while real PTY cells verify User/Bash/Data colors. Paging while streaming preserves the old viewport and draft; resizing and Ctrl-C preserve the terminal restoration contract.

Hashline regressions cover source-byte truncation, strict anchors, snapshot multi-edits, line endings, rejection without partial writes, and stale-anchor recovery through both protocols. A standalone disposable Rust loopback driver also launched the built `pp` for each protocol: read → edit → read produced `edited\n` and `ok:true, edits_applied:1, bytes_written:7`; a stale multi-edit was rejected without changing external bytes, then a fresh read enabled recovery. Both processes returned exit 0 with stdout exactly `hashline-ok\n`. The temporary driver was removed; no personal config or real provider was accessed, and fixture success does not establish vendor compatibility.

Replace regressions cover snapshot batches, Unicode/whitespace boundaries, conservative uniqueness, overlap rejection, BOM/newlines/deletion, unchanged-line preservation, strict mode arguments, and source-byte read limits. Real subprocess tests exercise both APIs with omitted/explicit replace settings and both modes across Completion → Responses → Completion switches. An independent disposable Rust standard-library loopback driver served five requests to the built CLI: plain read → two-item exact/loose edit (27 bytes) → rejected valid+missing batch with unchanged bytes → recovery edit (32 bytes) → final answer. Observed results were `ok:true, edits_applied:2, bytes_written:27`, `ok:false`, then `ok:true, edits_applied:1, bytes_written:32`; exit 0 and stdout exactly `replace-smoke-ok\n`. Only temporary dummy-key configuration and loopback HTTP were used; no real provider compatibility is implied.

A separate Python standard-library loopback fixture exercised the built binary in all four combinations: Completion/Responses × pipe/TUI. Each showed `smoke-first-中` before releasing the remaining model text and both bash channels before creating the command's release file; returned tool JSON was exactly stdout `out-firstout-last`, stderr `err-first`, exit code 0. Pipe stdout contained only assistant text, without duplicates or ANSI. TUI observation confirmed distinct User/Bash/Data foregrounds, draft editing, paging across 60 lines, 100×30 → 20×4 → 100×30 resize, and immediate Ctrl-C during a pending request with original termios/alternate-screen/paste/cursor restored. Only disposable fixture configuration was used; no real provider compatibility is implied.

An actual PTY smoke test with the local fixture has been run: CJK/emoji editing, multi-line paste, soft-wrapping of indented code and long paths, paging through a 60-line answer, scrolling while running, resizing 100×30 → 40×10 → 20×4 → 100×30 with draft preservation, and a real tool writing and reading back `tui-ok\n` in `proof.txt` with no tool log breaking through the screen. Ctrl-C while busy returned to the shell with working echo and cursor, the file still present. Also verified: `-p` inside a terminal does not switch screens, and one-shot and pipe-mode stdout contain only the fixture's final answer and a newline. No external network or personal model configuration was touched.

The dual-API CLI was smoke-tested against a standalone Python standard-library loopback fixture in temporary working directories. Omitting `protocol` and explicitly setting `"response"` both used `/responses`; `"completion"` used `/chat/completions`. All three modes preserved an encoded custom URL prefix, ignored environment-provided SDK configuration, and completed a real write/read/bash batch in exactly two requests. `proof.txt` contained `dual-api-ok\n`, bash counted one line, tool results carried matching call IDs, and stdout was `dual-api-ok: 1 line` plus a newline, exit code 0. Responses additionally replayed encrypted reasoning and aggregated multiple text parts. No personal configuration or external service was used; temporary files were removed.

A separate default-protocol PTY smoke launched the actual TUI at 100×30: the task used `/v1/responses`, the screen rendered `responses-tui-ok`, and Ctrl-C exited with code 0 while restoring the alternate screen and original termios settings.

Slash/model regressions exercise local commands without HTTP, alias routing and credentials, complete tool histories across bidirectional protocol switches, native reasoning replay versus cross-provider portability, atomic initialization failure, read-only startup snapshots, and one-shot slash prompts. PTY cases cover command completion, picker confirmation/cancellation, retained answers and drafts, busy rejection, switch failure, 100×30 → 20×4 → 100×30 resizing, and terminal restoration when exiting in a picker or during a request.

A standalone Python standard-library fixture also exercised the actual binary through Completion A → Responses B → Completion A. All three requests carried the expected model/authorization and complete prior dialogue; slash commands made no requests. Stdout was exactly `alpha`, `alpha via b`, and `alpha via a`, one per line, exit code 0; both configuration files were byte-identical afterward. No external service or personal configuration was used.

Session 新格式已在 macOS arm64 验证：13 项 session 行为测试、3 项专用真实 PTY 测试、9 项 persistence 单测通过；当时的 `cargo test --locked -- --test-threads=1` 全套 236 项通过，`cargo build --locked` 通过。默认并发全套曾有 1 项 session 惰性创建用例在 5 秒内未收到 CLI 请求；串行全套通过，未放宽 deadline 或断言。覆盖原工作目录、保存的 edit/read/bash 限制优先级、完整原生上下文、链/版本/旧格式拒绝、残尾/锁/符号链接边界，以及真实文件大小限制注入的持久化失败。PTY 还覆盖未结束 SSE 文本 delta 后强杀并 `-r`：保留部分文本、显示中断提示、无恢复 HTTP、无部分原生 assistant 提交。Linux 尚未实机验证。

新格式另经一次性 Python 标准库 SSE 服务与构建后的 CLI 独立冒烟：Completion/Responses 分别使用隔离 HOME、项目 A 与启动目录 B，验证 HTTP 打开前不创建 session、stdout 只有回答、header/version 3 与线性链正确、普通消息内容正确；`-r` 无 HTTP 且日志字节不变，`-c` 精确发送保存的原生请求、不加用户消息、保留首次 assistant，并以原 cwd 连接旧链追加同一个文件。每个协议共 2 次 HTTP。此验证不代表真实供应商兼容性。

日期时间格式 ID 已通过独立 CLI 冒烟：临时 HOME 与本地 SSE 服务下分别设置 `TZ=UTC`、`TZ=Asia/Shanghai`，确认 ID 时间与 checkpoint 创建时间一致、文件名与记录 ID 一致，并可用完整 ID 执行 `-r`（无 HTTP）和 `-c`（继续原会话）。

Session recovery was independently smoke-tested against a disposable Python standard-library SSE fixture and the built `target/debug/pp`, using isolated HOME, project A, and launch directory B. For both protocols: empty startup left no sessions directory; `-p` returned `ack smoke-marker` and an ID; `-r` in a real 100×30 PTY showed saved dialogue without HTTP, then retained it on new input; `-c` sent the exact fully reconstructed native request without a keypress or extra user item. A new write tool created its proof file only in A, not B. JSONL first/final records parsed successfully, all successful process exits were 0, and termios/alternate-screen/paste/cursor restoration was observed. Responses preserved the opaque reasoning field and item ID. A separate restored start-only event rendered `Previous request was interrupted.` with zero HTTP. Temporary fixture files were removed; no real HOME or provider was accessed.

当前配置的 `ark/glm-5.3-flash` 已通过真实 Responses CLI 冒烟：`high` 档位、临时 HOME 和工作目录，普通回答返回 `OK`，调用 `read` 读取临时文件后正确续轮并返回文件内容，两项退出码均为 0。这不代表其他模型、档位或工具已验证。此前 `low` 普通回答也曾通过，同一服务端曾明确拒绝 `none`。After filling in a real vendor configuration supporting function tools and selecting its API via `protocol`, 将全局配置复制到临时 HOME 中运行（配置文件仅从 HOME 下的 `.pp/agent/` 读取）：

```sh
BIN="$PWD/target/debug/pp"
WORK="$(mktemp -d)"
mkdir -p "$WORK/home/.pp/agent"
cp ~/.pp/agent/settings.yml ~/.pp/agent/models.yml "$WORK/home/.pp/agent/"
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
