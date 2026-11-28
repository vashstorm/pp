//! Terminal escape and CR normalization: keep only displayable text and collapse in-place overwritten progress frames.
use crate::stream_text::StreamText;

/// Safety cap for a single `\r` overwrite buffer, preventing unbounded memory growth from huge lines without newlines.
const CR_LINE_CAP: usize = 32 * 1024;

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
    /// A `\r` was just received and no following character has been seen yet (waiting to distinguish `\r\n` from in-place overwrite).
    cr_pending: bool,
    /// Whether in-place `\r` overwrite buffering is active (after a standalone `\r`, keep overwriting until `\n` or end of stream).
    cr_active: bool,
    /// Line body after the most recent `\r` overwrite frame.
    cr_buf: String,
    /// Whether the stream already emitted downstream is currently mid-line (non-empty and not ending with `\n`).
    mid_line: bool,
}

impl Normalizer {
    pub fn feed(&mut self, bytes: &[u8], finish: bool) -> String {
        let decoded = if finish {
            self.decoder.finish_delta()
        } else {
            self.decoder.append_delta(bytes)
        };
        if matches!(self.escape, EscapeState::Text)
            && !self.cr_pending
            && !self.cr_active
            && !finish
            && !decoded
                .bytes()
                .any(|b| (b < 0x20 && b != b'\n' && b != b'\t') || b >= 0x7f)
        {
            if let Some(last) = decoded.as_bytes().last() {
                self.mid_line = *last != b'\n';
            }
            return decoded;
        }
        let mut text = String::with_capacity(decoded.len());
        let started_mid_line = self.mid_line;
        // If this feed sees a `\r`, record the start offset of the current line first and only
        // trim the earlier frame on the same line once the next character is known not to be `\n`,
        // so `\r\n` is not damaged.
        let mut cr_overwrite_offset: Option<usize> = None;
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
                EscapeState::ControlString { .. } => {
                    unreachable!("control strings are handled above")
                }
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
            if ch == '\r' {
                if self.cr_active {
                    self.cr_buf.clear();
                    self.cr_pending = true;
                } else {
                    cr_overwrite_offset = match text.rfind('\n') {
                        Some(last_nl) if last_nl + 1 < text.len() => Some(last_nl + 1),
                        None if !text.is_empty() && !started_mid_line => Some(0),
                        _ => None,
                    };
                    self.cr_pending = true;
                }
                continue;
            }
            if ch == '\n' {
                cr_overwrite_offset = None;
                if self.cr_active {
                    self.flush_cr(&mut text, true);
                } else {
                    self.cr_pending = false;
                    text.push('\n');
                    self.mid_line = false;
                }
                continue;
            }
            if ch.is_control() && ch != '\t' {
                continue;
            }
            if self.cr_pending {
                self.cr_pending = false;
                if let Some(offset) = cr_overwrite_offset.take() {
                    text.truncate(offset);
                    self.mid_line = false;
                }
                self.cr_active = true;
                self.cr_buf.clear();
            }
            if self.cr_active {
                if self.cr_buf.len() + ch.len_utf8() <= CR_LINE_CAP {
                    self.cr_buf.push(ch);
                }
            } else {
                text.push(ch);
                self.mid_line = true;
            }
        }
        if finish {
            if self.cr_active {
                self.flush_cr(&mut text, false);
            }
            self.cr_pending = false;
        }
        text
    }

    fn flush_cr(&mut self, out: &mut String, newline: bool) {
        self.cr_active = false;
        self.cr_pending = false;
        if !self.cr_buf.is_empty() {
            if self.mid_line && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&self.cr_buf);
            self.cr_buf.clear();
            self.mid_line = true;
        }
        if newline {
            out.push('\n');
            self.mid_line = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{fixture, read};
    use super::Normalizer;
    use pp_config::config::OutputSettings;
    use pp_core::cancellation::CancellationToken;
    use pp_core::events::OutputStream;

    #[test]
    fn carriage_return_progress_frames_collapse_to_final_line() {
        let mut normalizer = Normalizer::default();
        let mut out = normalizer.feed(
            b"Downloading 10%\rDownloading 50%\rDownloading 100%\n",
            false,
        );
        out.push_str(&normalizer.feed(&[], true));
        assert_eq!(out, "Downloading 100%\n");

        let mut streamed = Normalizer::default();
        let mut acc = String::new();
        for frame in [
            &b"\rCompiling 1/100"[..],
            &b"\rCompiling 50/100"[..],
            &b"\rCompiling 100/100\n"[..],
            &b"Finished\r\n"[..],
        ] {
            acc.push_str(&streamed.feed(frame, false));
        }
        acc.push_str(&streamed.feed(&[], true));
        assert_eq!(acc, "Compiling 100/100\nFinished\n");

        let mut tail = Normalizer::default();
        let mut tail_out = tail.feed(b"\rStep 1\rStep 2", false);
        tail_out.push_str(&tail.feed(&[], true));
        assert_eq!(tail_out, "Step 2");
    }

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
