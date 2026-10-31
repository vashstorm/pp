/// 本地反馈的级别；只有错误会追加到消息栏。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Normal,
    Error,
}
