//! 终端转义与 CR 归一化：只保留可展示正文。
use crate::tools::stream::StreamText;

#[derive(Clone, Copy, Default)]
enum EscapeState {
    #[default]
    Text,
    Escape,
    Csi,
    Intermediate,
    ControlString {
        bell: bool,
        escaped: bool,
    },
}

#[derive(Default)]
pub struct Normalizer {
    decoder: StreamText,
    escape: EscapeState,
    cr: bool,
}

impl Normalizer {
    pub fn feed(&mut self, bytes: &[u8], finish: bool) -> String {
        let decoded = if finish {
            self.decoder.finish()
        } else {
            self.decoder.append(bytes)
        };
        self.decoder.text.clear();
        let mut text = String::new();
        for ch in decoded.chars() {
            if let EscapeState::ControlString { bell, escaped } = self.escape {
                self.escape = if ch == '\u{9c}' || bell && ch == '\u{7}' || escaped && ch == '\\' {
                    EscapeState::Text
                } else {
                    EscapeState::ControlString {
                        bell,
                        escaped: ch == '\u{1b}',
                    }
                };
                continue;
            }
            if ch == '\u{1b}' {
                self.escape = EscapeState::Escape;
                continue;
            }
            match self.escape {
                EscapeState::Escape => {
                    self.escape = match ch {
                        '[' => EscapeState::Csi,
                        ']' => EscapeState::ControlString {
                            bell: true,
                            escaped: false,
                        },
                        'P' | 'X' | '^' | '_' => EscapeState::ControlString {
                            bell: false,
                            escaped: false,
                        },
                        ' '..='/' => EscapeState::Intermediate,
                        _ => EscapeState::Text,
                    };
                    continue;
                }
                EscapeState::Csi => {
                    if ('@'..='~').contains(&ch) {
                        self.escape = EscapeState::Text;
                    }
                    continue;
                }
                EscapeState::Intermediate => {
                    if ('0'..='~').contains(&ch) {
                        self.escape = EscapeState::Text;
                    }
                    continue;
                }
                EscapeState::ControlString { .. } => unreachable!("控制字符串已在前面处理"),
                EscapeState::Text => {}
            }
            let control = match ch {
                '\u{9b}' => Some(EscapeState::Csi),
                '\u{9d}' => Some(EscapeState::ControlString {
                    bell: true,
                    escaped: false,
                }),
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => Some(EscapeState::ControlString {
                    bell: false,
                    escaped: false,
                }),
                _ => None,
            };
            if let Some(control) = control {
                self.escape = control;
                continue;
            }
            if ch == '\n' && self.cr {
                self.cr = false;
                continue;
            }
            self.cr = ch == '\r';
            if self.cr {
                text.push('\n');
            } else if !ch.is_control() || matches!(ch, '\n' | '\t') {
                text.push(ch);
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{fixture, read};
    use pp_config::config::OutputSettings;
    use pp_core::cancellation::CancellationToken;
    use pp_core::events::OutputStream;

    #[test]
    fn artifact_normalization_preserves_split_utf8_and_uncapped_lines() {
        let (_dir, store, mut sink) = fixture(OutputSettings {
            output_max_columns: 4,
            ..Default::default()
        });
        let text = "\u{1b}(B\u{1b}[31m中😀\u{1b}[0m\r\nnext\rprogress\tend\u{1b}]title\u{7}\u{9b}32m\u{9b}0m\u{1b}Pignored\u{7}still-ignored\u{1b}\\\n";
        for chunk in text.as_bytes().chunks(1) {
            sink.append(OutputStream::Stdout, chunk);
        }
        sink.append(OutputStream::Stderr, b"error\n");
        sink.finish();
        let result = sink.value();
        assert_eq!(result["truncated"], true);
        let saved = read(
            store
                .snapshot(
                    result["artifact"]["uri"].as_str().unwrap(),
                    &CancellationToken::new(),
                )
                .unwrap(),
        );
        assert_eq!(saved, "中😀\nnext\nprogress\tend\nerror\n");
        assert_eq!(result["total_lines"], 4);
        assert!(!result["output"].as_str().unwrap().contains('�'));
    }
}
