//! `/pipes` 面板按键：只改本地状态并返回后续命令。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use super::{Action, Panel};

impl Panel {
    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || !key.modifiers.is_empty()
        {
            return Action::None;
        }
        if key.code == KeyCode::Esc {
            return Action::Close;
        }
        let page = self.body_height.max(1);
        match key.code {
            KeyCode::Char('f') if key.kind == KeyEventKind::Press => {
                if self.following {
                    self.following = false;
                    return Action::None;
                }
                self.following = true;
                self.awaiting = true;
                self.new_bytes = 0;
                self.evicted = false;
                return self.follow_action();
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
                self.error = None;
                return Action::Refresh {
                    full: self.following,
                };
            }
            KeyCode::Left => {
                if self.unavailable {
                    if self.rebind_current_run() {
                        return self.select_run_action();
                    }
                } else if let Some(index) = self.selected_run.checked_sub(1)
                    && self.select_run(index)
                {
                    return self.select_run_action();
                }
            }
            KeyCode::Right => {
                if self.unavailable {
                    if self.rebind_current_run() {
                        return self.select_run_action();
                    }
                } else if self.select_run(self.selected_run.saturating_add(1)) {
                    return self.select_run_action();
                }
            }
            KeyCode::Up if !self.unavailable => {
                if let Some(index) = self.selected_stream.checked_sub(1)
                    && self.select_stream(index)
                {
                    return self.select_stream_action();
                }
            }
            KeyCode::Down if !self.unavailable => {
                if self.select_stream(self.selected_stream.saturating_add(1)) {
                    return self.select_stream_action();
                }
            }
            KeyCode::Home => {
                self.following = false;
                self.scroll = 0;
                if self.body.trimmed {
                    // 面板副本已裁掉前缀：重新取保留区间起点，但仍保持冻结。
                    self.awaiting = true;
                    self.reload_frozen = true;
                    return Action::Refresh { full: true };
                }
            }
            KeyCode::End => {
                self.following = true;
                self.awaiting = true;
                self.new_bytes = 0;
                self.evicted = false;
                return self.follow_action();
            }
            KeyCode::PageUp => {
                self.freeze();
                self.scroll = self.scroll.saturating_sub(page);
            }
            KeyCode::PageDown if !self.following => {
                self.scroll = self.scroll.saturating_add(page);
            }
            _ => {}
        }
        Action::None
    }

    pub(in crate::tui) fn wheel(&mut self, up: bool, lines: usize) {
        if self.body.text.is_empty() {
            return;
        }
        if up {
            self.freeze();
            self.scroll = self.scroll.saturating_sub(lines);
        } else if !self.following {
            self.scroll = self.scroll.saturating_add(lines);
        }
    }

    fn freeze(&mut self) {
        self.following = false;
    }

    fn select_run(&mut self, index: usize) -> bool {
        if index == self.selected_run || index >= self.runs.len() {
            return false;
        }
        let id = self.runs[index].run_id.clone();
        self.selected_run = index;
        self.selected_stream = 0;
        self.explicit_run = Some(id);
        self.unavailable = false;
        self.error = None;
        self.reset_body();
        true
    }

    /// 从“指定 run 不可用”回到当前最近 run，仅在本地面板内切换。
    fn rebind_current_run(&mut self) -> bool {
        if self.runs.is_empty() {
            return false;
        }
        let id = self.runs[self.selected_run.min(self.runs.len() - 1)]
            .run_id
            .clone();
        self.selected_stream = 0;
        self.explicit_run = Some(id);
        self.unavailable = false;
        self.error = None;
        self.reset_body();
        true
    }

    fn select_stream(&mut self, index: usize) -> bool {
        if index == self.selected_stream || index >= self.streams().len() {
            return false;
        }
        self.selected_stream = index;
        self.error = None;
        self.reset_body();
        true
    }

    fn select_run_action(&self) -> Action {
        match self.selected_run_id() {
            Some(run) => Action::SelectRun {
                run: run.to_owned(),
            },
            None => Action::None,
        }
    }

    fn select_stream_action(&self) -> Action {
        match (self.selected_run_id(), self.current_stream()) {
            (Some(run), Some(stream)) => Action::SelectStream {
                run: run.to_owned(),
                stream: stream.id,
            },
            _ => Action::None,
        }
    }

    fn follow_action(&self) -> Action {
        match (self.selected_run_id(), self.current_stream()) {
            (Some(run), Some(stream)) => Action::Follow {
                run: run.to_owned(),
                stream: stream.id,
            },
            _ => Action::Refresh { full: true },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::BodyUpdate;
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn arrows_switch_run_and_stream_with_commands() {
        let mut panel = panel();
        assert!(matches!(panel.key(key(KeyCode::Left)), Action::None));
        let Action::SelectRun { run } = panel.key(key(KeyCode::Right)) else {
            panic!("right arrow selects the next run");
        };
        let expected = "b".repeat(32);
        assert_eq!(run, expected);
        assert_eq!(panel.selected_run_id_for_test(), Some(expected.as_str()));
        assert!(panel.body_text().is_empty());
        let Action::SelectStream { stream, .. } = panel.key(key(KeyCode::Down)) else {
            panic!("down arrow selects the next stream");
        };
        assert_eq!(stream, 3);
        assert!(matches!(panel.key(key(KeyCode::Right)), Action::None));
        assert!(matches!(panel.key(key(KeyCode::Down)), Action::None));
    }

    #[test]
    fn scrolling_freezes_and_end_or_follow_resumes_tail() {
        let mut panel = panel();
        draw(&mut panel, 80, 24);
        assert!(panel.following());
        panel.key(key(KeyCode::PageUp));
        assert!(!panel.following());
        let frozen = panel.scroll();
        assert!(frozen < 19);
        assert!(matches!(
            panel.key(key(KeyCode::End)),
            Action::Follow { .. }
        ));
        assert!(panel.following());
        draw(&mut panel, 80, 24);
        assert!(panel.scroll() > frozen);
        panel.key(key(KeyCode::Char('f')));
        assert!(!panel.following());
        assert!(matches!(
            panel.key(key(KeyCode::Char('f'))),
            Action::Follow { .. }
        ));
        assert!(panel.following());
        panel.wheel(true, 3);
        assert!(!panel.following());
        panel.key(key(KeyCode::Home));
        assert_eq!(panel.scroll(), 0);
    }

    #[test]
    fn home_on_trimmed_snapshot_reloads_retained_prefix_while_staying_frozen() {
        let mut panel = panel();
        draw(&mut panel, 80, 24);
        panel.body.trimmed = true;
        assert!(matches!(
            panel.key(key(KeyCode::Home)),
            Action::Refresh { full: true }
        ));
        assert!(!panel.following());
        panel.apply_body(BodyUpdate {
            run: "a".repeat(32),
            stream: 1,
            data: "retained-start\n".into(),
            start: 0,
            end: 15,
            reset: true,
            retained_start: 0,
            retained_end: 15,
            total_bytes: 15,
            open: false,
            revision: 6,
            prefix_evicted: false,
        });
        assert_eq!(panel.body_text(), "retained-start\n");
        assert!(!panel.body.trimmed);
        assert!(!panel.following());
        assert_eq!(panel.scroll, 0);
    }

    #[test]
    fn escape_only_closes() {
        let mut panel = panel();
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Close));
        assert!(matches!(
            panel.key(key(KeyCode::Char('r'))),
            Action::Refresh { full: true }
        ));
        panel.key(key(KeyCode::PageUp));
        assert!(matches!(
            panel.key(key(KeyCode::Char('r'))),
            Action::Refresh { full: false }
        ));
    }
}
