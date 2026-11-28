/// Level of local feedback; only errors are appended to the message bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Normal,
    Error,
}
