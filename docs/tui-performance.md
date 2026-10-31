# TUI 性能优化验证

## 行为与配置

`~/.pp/agent/settings.yml` 支持以下配置，修改后重启生效：

```yaml
tui:
  max_fps: 60
```

`max_fps` 默认为 60，接受 1 到 120 的整数。输入处理和取消信号不等待绘制；所有正常帧（含输入、通知和历史提交）使用同一截止时间。退出收尾允许立即输出。空闲且没有变化时不绘制。

TUI 按帧合并同一目标、类型和轮次的文本分片；达到 64 KiB 或遇到控制事件时提前提交。原生事件、会话日志和 CLI 输出不经过此显示缓冲。Markdown 内容变化时仍完整解析，随后按语义块和宽度复用排版结果；未变化消息复用版本快照和有限尾部的回退布局。

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
PP_TUI_PERF_ASSERT=1 cargo nextest run --release --locked --test cli_agent -E 'test(tui_runtime_responsiveness)' --success-output immediate
cargo nextest run --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

全量并行回归运行 1437 项，1436 项通过，`tui_menu_overlay_keeps_input_cursor_and_history_anchored` 失败；该用例随后单独串行通过，与持续输出用例组成的小集合并行运行也通过。尚未确定首次失败原因，不能将本轮验证表述为全量通过。全量之后新增的分块工作量对照用例已单独通过；最后的缓冲阈值调整也通过流式与 PTY 定向验证，未重复全量回归。
