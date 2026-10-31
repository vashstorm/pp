# TUI 性能优化验证

## 行为与配置

`~/.pp/agent/settings.yml` 支持以下配置，修改后重启生效：

```yaml
tui:
  max_fps: 60
```

`max_fps` 默认为 60，接受 1 到 120 的整数。输入处理和取消信号不等待绘制；所有正常帧（含输入、通知和历史提交）使用同一截止时间。退出收尾允许立即输出。空闲且没有变化时不绘制。

TUI 按帧合并同一目标、类型和轮次的文本分片；达到 64 KiB 或遇到控制事件时提前提交。原生事件、会话日志和 CLI 输出不经过此显示缓冲。Markdown 内容变化时仍完整解析，随后按语义块和宽度复用排版结果。

助手正文与思考输出在同一正文追加链、相同宽度下保留最近完成的 Markdown 快照，等待后台更新期间继续展示它，不反复切回原始 Markdown 文本。后台落后于最新分片时，已完成的中间版本仍可用于展示，最终收敛到最新版本。新分片立即撤销精确排版资格，只有最新版本能够提交原生历史；历史提交仍在终端输出成功后确认。正文整体替换或宽度变化时不复用不兼容快照；首次没有兼容快照时，最多预览末尾 8192 字节，保留 UTF-8 边界、换行、空行与制表符。

向上滚动后，浏览位置按“条目索引＋条目内物理行”保存，上一帧几何信息独立于正在更新的布局。后台重排和前面条目的高度变化不会把浏览位置推走；临时预览过短时只裁切当前画面，不永久改写目标行。正文确实缩短且精确排版完成后才裁剪条目内锚点；用户主动滚动更新锚点，回到底部恢复跟随，会话重置清空锚点。首次预览切换为完整 Markdown、窗口改宽或 Markdown 语义重排允许合理的布局变化。

## 本机测量

使用 release 构建、160×48 PTY、隔离 HOME 和回环模拟模型服务。负载为 400 段含中文和样式的文本、100 行 Rust 代码，随后发送 1500 个分片，分片间隔约 2ms。优化前使用既有 33ms 刷新间隔，优化后设为 30 FPS；交替运行三组，取中位数。CPU 时间包含进程启动及取消收尾，输出量覆盖流式阶段及退出。

| 指标 | 优化前 | 优化后 | 变化 |
| --- | ---: | ---: | ---: |
| CPU 时间 | 1.3372s | 1.1299s | 降低约 15.5% |
| 终端输出字节 | 1,729,703 | 1,645,342 | 降低约 4.9% |

单独的确定性缓存用例以 100 个稳定段落加一个增长中的尾段为输入，更新 20 次；排除首次预热，完整排版需要构建 2020 个块，缓存路径构建 20 个块，每次输出均与完整排版逐行比较。

默认 60 FPS 的 release 双协议 PTY 验收覆盖普通文本、代码思考输出、ASCII、中文、emoji、粘贴和光标位置：输入 P95 为 24–25ms，Esc 取消为 26–38ms。通过 `PP_TUI_PERF_ASSERT=1` 显式启用 50ms／100ms 阈值，避免日常并行回归受主机负载影响。

这些结果来自本机模拟负载，不代表所有终端或供应商。

## 验证命令与边界

```sh
cargo nextest run --locked -E 'test(frame_clock) | test(stream_buffer) | test(markdown_block_cache) | test(layout_worker_) | test(tui_max_fps) | test(streaming_frame_batches)'
cargo nextest run --locked -E 'test(tui::render_worker::) | test(tui::transcript::) | test(tui::surface::) | test(streaming_frame_batches) | test(stream_buffer) | test(tui_runtime_responsiveness) | test(tui_scrolled_stream_) | test(native_tui_)'
PP_TUI_PERF_ASSERT=1 cargo nextest run --release --locked --test cli_agent -E 'test(tui_runtime_responsiveness)' --success-output immediate
cargo nextest run --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

全量并行回归运行 1437 项，1436 项通过，`tui_menu_overlay_keeps_input_cursor_and_history_anchored` 失败；该用例随后单独串行通过，与持续输出用例组成的小集合并行运行也通过。尚未确定首次失败原因，不能将本轮验证表述为全量通过。全量之后新增的分块工作量对照用例已单独通过；最后的缓冲阈值调整也通过流式与 PTY 定向验证，未重复全量回归。

滚动稳定性回归使用通道暂停后台排版，分别断言等待期间和完成后的正文、样式与锚点，并覆盖长消息、宽度变化、替换、取消与原生历史门禁。双协议 PTY 用例以默认 60 FPS 发送真实 SGR 滚轮、列表／代码块和连续正文／思考分片，比较固定历史区域的终端内容与样式，验证输入、回到底部及备用屏幕切换次数。这些自动检查不等同于真实终端渲染器的视觉验收，终端同步更新兼容性需要另行观察。

本次滚动修复的定向验证共 85 项通过（78 项单元测试、7 项 PTY 集成测试）；PTY 用例在获得本地回环监听权限后单独并行运行。Clippy 和格式检查通过，未重复全量回归。Ghostty 原生应用访问被工具安全策略拒绝，真实终端视觉验收尚未完成。
