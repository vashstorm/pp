# pp 性能基线

本文件记录本机可重复的性能基线，`docs/tui-performance.md` 只保留 TUI 帧指标。
所有数字都来自 release 构建、隔离 HOME、回环 mock provider，同一台机器（macOS arm64），
每个用例运行三次取中位数。优化后只需在同一张表追加“优化后”列。

## 测量设施

- `PP_PERF_TRACE=1` 启用阶段计时：`pp_core::logging::perf::mark(phase)` 在既有启动边界向
  stderr 写 `phase=<name> micros=<n>`（相对 `main` 首行 `mark("start")`）。未设置该变量时
  零输出、零分配，不进入 `~/.pp/logs` 契约。
- 回环 mock provider（Chat Completions SSE）由 `docs/perf.md` 记录的最小 Python 脚本提供，
  通过 `probe.json` 指定本轮要触发的一次工具调用与轮数，因此既能测启动，也能测真实工具路径。
- `crates/pp-tools/tests/perf_probe.rs` 是 release 探针用例（`#[ignore]`，默认不跑），
  在进程内直接调用工具执行器，排除进程启动与模型往返。

release profile 本轮保持不变（`opt-level = "z"`、`codegen-units = 16`、`strip = true`）。

```sh
# 夹具（$PERF_FIXTURES）：1k / 10k / 50k 个文件的多层目录，5k 行文本，3k 行文本
mkdir -p /private/tmp/perf_fixtures && cd /private/tmp/perf_fixtures
python3 - <<'PY'
import os
body = "filler line a\nfiller line b. needle here\nfiller line c\n"
for n in (1000, 10000, 50000):
    d = f"d{n}"
    for i in range(n):
        sub = os.path.join(d, f"{i % 100:02d}")
        os.makedirs(sub, exist_ok=True)
        with open(os.path.join(sub, f"f{i}.txt"), "w") as f:
            f.write(f"file {i}\n" + body)
with open("big5k.txt", "w") as f:
    for i in range(5000):
        f.write(f"line {i} value {'x' * 40}\n")
with open("edit3k.txt", "w") as f:
    for i in range(3000):
        f.write(f"line {i} value\n")
PY
```

```sh
# 隔离 HOME：settings.yml / models.yml 指向回环 mock provider
mkdir -p "$HOME/.pp/agent"
printf 'rate_limit_retry_budget_seconds: 0\ndefault_model: fixture/primary\ntools:\n  sandbox:\n    mode: off\n' \
  > "$HOME/.pp/agent/settings.yml"
printf 'providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:45678/v1\n    models:\n      primary:\n        model: fixture-model\n' \
  > "$HOME/.pp/agent/models.yml"
```

```sh
# 单次启动阶段测量（mock provider 监听 127.0.0.1:45678）
TMPDIR=/private/tmp HOME=/private/tmp/perf-home PP_PERF_TRACE=1 \
  ./target/release/pp -p probe
```

## 1. 启动阶段（`-p` 冷启动）

`phase` 名字来自 `PP_PERF_TRACE`；`request_ready` 是首个模型请求发出前的最后边界。

| 阶段 | MCP 0 个 | MCP 1 个 | MCP 3 个 | 优化后 MCP 0／1／3 个 |
| --- | ---: | ---: | ---: | ---: |
| `agent_db_ready`（agent.db 迁移检查后） | 10.7 ms | 10.7 ms | 10.7 ms | 11.0／11.0／11.0 ms |
| `settings_ready`（首份 settings 解析后） | 10.8 ms | 10.8 ms | 10.8 ms | 11.1／11.1／11.1 ms |
| `skills_ready`（技能发现后） | 11.5 ms | 11.5 ms | 11.5 ms | 14.4／14.4／14.4 ms |
| `tools_ready`（`Tools::with_mcp` 后） | 43.3 ms | 44.8 ms | 43.8 ms | 14.7／14.7／14.7 ms |
| `sandbox_ready`（沙盒探测后） | 50.8 ms | — | — | 15.0／15.0／15.0 ms |
| `lsp_installed`（LSP 注册后） | 52.6 ms | — | — | 不单独计时（惰性发现） |
| `bootstrap_ready`（`Bootstrap::build` 返回） | 43.3 ms | 44.8 ms | 43.8 ms | 36.7／36.6／36.3 ms |
| `request_ready`（首个请求前） | 46.1 ms | 76.6 ms | 104.2 ms | 37.6／70.1／131.9 ms |

优化后一列为同一套命令在本轮改动后的中位数（三次）。

- `-p` 冷启动到首个请求：MCP 0 个 46 → 38 ms；1 个 77 → 70 ms；3 个 104 → 132 ms（三次样本 126／135／132 ms）。
- `tools_ready` 43 → 15 ms：`Tools::new` 不再探测 Python（默认配置下这是首个任务前最大的一笔固定开销）。
- `sandbox_ready` 51 → 15 ms：`tools.sandbox.mode: off` 不再启动一次 `sandbox-exec` 子进程；启用模式的探测时机不变。
- MCP 每增加一个 stdio 服务，首请求前仍多等约 30–50 ms；`await_connect` 的“首个请求必须带完整工具表”语义未改，这一列的高方差来自 fixture 服务进程启动，不代表本轮改动。
- **本轮补充优化（同机热 HOME，回环 Chat Completions SSE，三次样本 / 暖机第 2–3 次）**：
  - 首次冷 `HOME` 建库：`agent_db_ready=9.75 ms`、`skills_ready=12.38 ms`、`sandbox_ready=12.49 ms`、`request_ready=28.87 ms`（端到端 `58.5 ms`）。
  - 同目录暖 `HOME`（复用已迁移 `agent.db`，对照本轮前基线 `skills_ready=4.15–4.30 ms`、`request_ready=24.28–24.34 ms`）：`agent_db_ready=0.72–0.80 ms`、`skills_ready=3.39–3.45 ms`、`tools_ready=3.47–3.53 ms`、`sandbox_ready=3.49–3.55 ms`、`request_ready=20.09–25.38 ms`。
  - 主要改动：复用 `AgentStore` 单连接完成迁移检查与 `ModelCatalog` 加载；`logging` 复用 `open_regular_with_len` 的 `fstat` 长度；`skills` 用 `DirEntry::file_type()` 避开普通条目 `stat`；`pp-lsp` 惰性启动维护线程；`pp-eval` 探测增加 `-S`、指数退避轮询与惰性候选 env 构造。

## 2. 每轮请求固定开销

同一个 mock provider 按 `probe.json` 的 `rounds` 重复同一次 `read` 工具调用，随后返回文本：

| 用例 | 墙钟中位数 | 优化后 | 本轮复测 |
| --- | ---: | ---: | ---: |
| 0 轮工具（`noop`） | 0.10 s | 0.06 s | 0.047 s |
| 1 轮 `read`（5k 行） | 0.09 s | 0.07 s | 0.055 s |
| 5 轮 `read`（5k 行） | 0.15 s | 0.09 s | 0.077 s |
| 每轮固定开销（(5 轮 − 1 轮) / 4） | 12–13 ms | 5 ms | ~5.6 ms |

每轮固定开销覆盖：统计库写入（请求 + 工具各一次）、会话 JSONL 追加、项目指令刷新、
提示词模板渲染、token 估算。本轮分别改为：统计库句柄复用单连接 + `Rates` 按
`(mtime, len)` 缓存；JSONL 单次序列化（记录值直接 intern，不再“序列化→回读→再序列化”）；
项目指令按 `(device, inode, len, mtime 秒+纳秒)` 指纹复用，`sent` 与 `current` 共享
`Arc<[Document]>`；提示词模板在 `Snapshot` 内注册一次、`compose` 只渲染；token 估算改为
“请求骨架 + 逐条消息”增量累加（`SessionEstimate`，整段替换历史时显式失效）。

`/context` 诊断的发布路径同时改为增量分类 + 指纹缓存：同一会话的全量强算实测约 4.4 ms，
复用缓存发布约 83 µs（该数字来自临时探针，仓库内没有对应可复制入口，仅作量级参考）。

> 计划要求的 `Active.file` `BufWriter` 未落地：用户态缓冲会推迟写入可见性，与
> `tree_view()` 只读、失败收尾字节不变、`publish_project_load` 写失败必须当场返回
> 三条既有回归断言直接冲突。保留逐条即时 `write_all`，只消除重复序列化。

```sh
printf '{"tool":"read","arguments":{"path":"/private/tmp/perf_fixtures/big5k.txt"},"rounds":5}' \
  > probe.json
```

## 3. 大输入工具路径

进程内探针（`cargo nextest run --release --locked -p pp-tools --run-ignored only -E 'test(perf_probe)'`，
`PERF_FIXTURES=/private/tmp/perf_fixtures`），排除进程启动与模型往返：

| 用例 | 历史基线 | 上轮优化后 | 本轮前复测 | 本轮优化后 |
| --- | ---: | ---: | ---: | ---: |
| `glob` 1k 文件 | 7.2 ms | 4.5 ms | 4.0 ms | 4.6 ms |
| `glob` 10k 文件 | 53.7 ms | 30.2 ms | 24.3 ms | 23.9 ms |
| `glob` 50k 文件 | 267.1 ms | 140.3 ms | 257.0 ms | 111.1 ms |
| `rg` 10k 文件（全部命中） | 67.5 ms | 59.6 ms | 60.4 ms | 29.4 ms |
| `rg` 50k 文件（全部命中） | 250.0 ms | 198.9 ms | 204.7 ms | 26.3 ms |
| `read` 5k 行 | 0.2 ms | 0.2 ms | 0.2 ms | 0.3 ms |
| `edit` 3k 行（一次替换） | 20.0 ms | 18.1 ms | 19.1 ms | 16.1 ms |
| `bash` 输出 100k 行 | 70.2 ms | 29.9 ms | 27.9 ms | 20.1 ms |

本轮 `glob`/`rg`/`edit`/`bash` 主要改动：`SandboxState` 缓存 `Arc<Policy>` 并提供 `can_contain_denied_read` 整子树快路径（symlink 仍走 `is_read_denied_resolved`）；`glob` 集满 `limit` 后按 `worst_mtime` 过滤路径分配；`rg` 单入口目录搜索改用惰性 `WalkBuilder`（满 2000 命中即停遍历）、跳过非本页行构造并复用行缓冲区；`replace` 与 `output` 增设 ASCII 快路径；`diff` 先裁剪公共前后缀再计算 Myers diff；`bash` supervisor 进程与管道结束时立即转 `Finished` 不再空等 5 ms。

端到端交叉验证（真实二进制 + mock provider 触发一次工具调用，含启动与请求往返）：

| 用例 | 基线 | 优化后 |
| --- | ---: | ---: |
| `glob` 50k 文件一次 | 0.37 s | —（本轮未复测） |
| `bash` 100k 行一次 | 0.19 s | —（本轮未复测） |

`glob`/`rg` 的成本来源与改动：沙盒读拒绝判定改为词法快路径（普通 entry 不再逐个
`canonicalize`，只有 symlink entry 走完整规范化，拒绝集合不缩小）；`glob` 的结果集从
“每候选一次 `max_by(rank)`”改为“超过 2×limit 时一次性排序截断”；`rg` hashline 与 `read`
hashline 的标签计算从每文件 2–3 次降到 1 次，`read` 的全文遍历从 2 遍降到 1 遍；
`bash` 管道环形缓存不再在每次 `push` 时 `drain` 搬移保留字节，supervisor 空闲轮询从固定
5 ms 指数退避到 50 ms 上限。

**未做的优化**：`glob` 的“满 limit 后提前终止遍历”没有落地。`rank` 由 mtime 与路径决定，
而 `ignore` 的遍历顺序（readdir）与 mtime 无单调关系，目录 mtime 也不能作为子项上界，
任何按层级提前 break 都可能丢掉“尚未访问但更新”的候选，从而改变既有可见结果（工具描述
承诺 `Ranked by mtime`）。遍历终止条件仍只有 `max_depth`／ignore 剪枝／超时／取消。

## 4. 流式渲染每帧成本

TUI 帧指标的口径与负载（160×48 PTY、400 段中英混排文本 + 100 行 Rust 代码、1500 个约 2 ms
间隔的分片）沿用 `docs/tui-performance.md` 的“本机测量”表：

| 指标 | 优化前 | 优化后 |
| --- | ---: | ---: |
| CPU 时间（含启动与取消收尾） | 1.3372 s | 1.1299 s |
| 终端输出字节 | 1,729,703 | 1,645,342 |

本轮新增的每帧拷贝与预览回退指标（4 次整帧 `Buffer` 克隆降为 2 次、预览回退按前缀
`Arc` 复用、输入框每帧只排版一次、状态栏字段只构造一次、尾部预览只处理末尾行）在同
PTY 负载下需要真实终端复测；本轮执行环境禁止创建伪终端（见下），因此该表未更新，
而是在 `docs/tui-performance.md` 记录机制与确定性断言。

## 未验证边界

- 本机执行沙盒拒绝创建伪终端（`openpty: Operation not permitted`），因此 TUI 首帧时间、
  流式每帧 CPU 与 PTY 断言无法在本环境重跑；上述 TUI 数字来自既有 `docs/tui-performance.md`
  记录，需要真实终端复测才能更新。
- mock provider 只实现 Chat Completions 的非流式语义（单分片文本 / 单次工具调用），
  不代表真实供应商的分片节奏；`-p` 路径不含 TUI 绘制与 `/context` 诊断开销。
- 所有数字都是本机单一硬件与热文件缓存下的结果，不能外推到其他机器。
