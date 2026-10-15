# pp

Rust 版 Pi 风格最小编码 agent：OpenAI 兼容 Chat Completions、真实 read/write/bash 工具、终端多轮交互。单 crate，Rust Edition 2024，支持 macOS/Linux；bash 必须在 PATH 中。

## 构建与运行

```sh
cargo build --locked
./target/debug/pp --help
./target/debug/pp --version
```

在启动目录创建 `pp.jsonc`，填入供应商配置（支持 `//`、`/* ... */` 注释和尾逗号）：

```jsonc
{
  // 必填：支持 Chat Completions function tools 的服务和模型
  "api_key": "你的密钥",
  "model": "支持工具调用的模型名",
  // 可选，省略时默认 https://api.openai.com/v1
  "base_url": "https://api.openai.com/v1",
}
```

默认读取启动目录下的 `pp.jsonc`。使用 `-c, --config <PATH>` 可选择其他文件，相对路径仍相对启动 cwd，而不是二进制位置；工具工作目录不受配置文件位置影响：

```sh
chmod 600 pp.jsonc
./target/debug/pp -p '读取当前项目并解释结构，不修改文件。'
./target/debug/pp
./target/debug/pp --config /path/to/provider.jsonc -p '读取当前项目并解释结构。'
```

**密钥以明文保存在配置文件中。** 仓库根的 `pp.jsonc` 已加入 `.gitignore`；自定义路径需自行排除版本控制并限制文件权限。工具无沙箱，仍可读取配置文件；不要让模型读取或输出包含密钥的文件。

`api_key`、`model` 必填且不能为空白；字段必须为字符串，未知字段报错。不设默认模型；部分模型仅支持 Responses API，不能用于本程序。供应商配置只从选定 JSONC 文件读取，不读取供应商环境变量，不做环境变量替换或回退，也不加载 `.env` 或 OAuth。配置缺失、语法或字段类型错误会明确失败；错误不转储配置内容或密钥。

`base_url` 是 API 根地址，保留已有路径前缀，去除尾部 `/` 后追加 `/chat/completions`，不会自动补 `/v1`。只接受 http/https，拒绝 userinfo、query、fragment；显式空值报错。显式 HTTP 地址会以明文传输凭据和会话，仅用于可信兼容服务。

交互模式一次一行：空行忽略，`/new` 清空会话（保留 system 消息），`/quit` 或 EOF 退出。其他文本是用户任务。无 `--prompt` 时进入交互；空白单次 prompt 报错。帮助、版本无需配置文件。最终回答输出 stdout；提示符、警告、工具状态和错误输出 stderr。单次任务或启动失败退出码为 1；交互任务失败后可继续输入。

## 执行与权限

**无沙箱、无逐次授权确认。模型工具继承启动进程权限，可覆盖文件和执行任意 bash 命令。请在可信或临时目录运行。**

- 启动 cwd 是相对路径基准，不是安全边界。绝对路径、`..` 和符号链接均允许，不扫描项目规则或 skills 文件。
- `read(path)`：读取开头最多 65,536 字节，UTF-8 lossy 解码，返回截断标记。不解析图片/二进制。
- `write(path, content)`：创建父目录并覆盖 UTF-8 文件，空内容合法。无备份或局部 edit。
- `bash(command)`：`bash -c`，工作目录为启动 cwd，stdin 关闭；stdout/stderr 同时排空，各保留最多 65,536 字节，超出仍排空但丢弃。返回退出码、输出和截断标记；信号退出码为 null。
- 参数严格校验，缺字段、多字段、非法 JSON、空白 path/command、未知工具、文件和进程错误均返回 `ok: false` 给模型，允许模型修正。非零 bash 退出同样返回 `ok: false`，保留输出。
- 无命令超时、后台进程管理或取消后恢复。只运行有限时的前台命令；卡住时 Ctrl-C 退出整个程序。
- 工具串行执行，同批 write → read 等依赖按顺序可见。先保存完整 assistant 调用消息，再逐条追加匹配 id 的 tool 结果，工具失败不会跳过同批剩余调用。
- 每个用户任务最多 20 次模型请求。第 20 次的整批工具仍执行并回传到内存历史，然后报上限错误；不发送第 21 次请求、不宣称完成。
- 保留完整内存会话，无持久化或上下文裁剪。网络/协议错误不回滚真实文件改动，也不自动重试；此前已完成历史保留。

## HTTP 协议

同步、非流式 `POST /chat/completions`，单次请求超时 120 秒，禁止自动重定向和自动重试。发送 `model`、`messages`、`tools`、`tool_choice: auto`、`stream: false`，不发送模型专属参数。

只取 `choices[0]`，要求 assistant role，支持文本/null content 和缺省/null/数组 tool_calls。只接受 finish_reason 为 `stop` 或 `tool_calls`，截断/过滤/其他状态报错。无工具调用时可用 refusal 文本作最终回答；无文本且无调用则报协议错误。

整批调用在执行前校验：必须为 function，id/工具名非空、批内 id 不重复；坏响应不会执行其中的合法 write。工具参数在执行阶段按工具类型解码。不支持 SSE、Responses API、旧 function_call 或需要 reasoning 字段回传的供应商扩展。HTTP 错误不转储请求、响应正文或密钥。

## 验证

```sh
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

回归使用 loopback HTTP fixture 和真实子进程，独立创建 JSONC 配置和临时 cwd，不修改测试进程环境、不访问外网。覆盖同批依赖、跨轮记忆、工具失败修正、坏响应无副作用、20 次上限、交互错误恢复与 `/new`、双管道超限输出，以及 JSONC 注释/尾逗号、自定义配置路径、环境变量不覆盖配置、错误不泄露凭据等边界。

已执行 JSONC 配置的本地协议 smoke：使用带注释和尾逗号的独立配置文件启动真实 CLI，通过 HTTP 接收同批 write/read/bash 调用，在临时目录写入并读取 `demo.txt`，执行 `wc -l` 得到 1；下一轮保留历史并回答文件名。共 3 次 HTTP 请求，退出码 0。另确认缺少默认配置时不会回退到环境变量。远端响应由一次性 Python 标准库驱动模拟，交付程序没有 mock provider。

**未验证真实供应商调用**。填好支持 Chat Completions function tools 的真实供应商配置后，可在仓库根运行：

```sh
BIN="$PWD/target/debug/pp"
CONFIG="$PWD/pp.jsonc"
WORK="$(mktemp -d)"
(
  cd "$WORK" || exit 1
  "$BIN" --config "$CONFIG" -p '创建 hello.txt，内容为 hello from rust agent 加换行；读取它，再用 bash 的 wc -l 验证行数，最后报告文件内容和行数。'
  cat hello.txt
  wc -l < hello.txt
  "$BIN" --config "$CONFIG"
)
```

预期文件为 `hello from rust agent` 加换行，行数 1。交互中先问文件内容，再追问文件名，检查上下文连续；临时目录保留供检查。

参考：[Pi](https://github.com/badlogic/pi-mono)、[工具循环](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md)、[Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)。不承诺上游 SDK、扩展协议或会话文件兼容。
