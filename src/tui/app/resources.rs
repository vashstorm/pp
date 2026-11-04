use crate::{
    tui::{App, Level, PendingKind, State, resources},
    user_input,
};

impl App {
    pub(in crate::tui) fn resource_input_available(&self) -> bool {
        !self.shell_mode
            && self.completion_input_focused()
            && matches!(self.state, State::Ready | State::Running)
    }

    pub(in crate::tui) fn start_resource_job(
        &mut self,
        operation: resources::Operation,
        kind: PendingKind,
    ) {
        if self.resources.job.is_some() {
            self.notify(Level::Normal, "正在准备输入资源，Esc 可取消。".into());
            return;
        }
        match resources::start(
            operation,
            self.history_cwd.clone(),
            self.input_text(),
            kind,
            self.input.cursor(),
        ) {
            Ok(job) => self.resources.job = Some(job),
            Err(error) => self.notify(Level::Error, format!("{error:#}")),
        }
    }

    pub(in crate::tui) fn paste_resource_input(&mut self, text: String) {
        // 先显示原始粘贴文本；随后编辑或取消时也不会丢掉用户输入。
        self.resources.cancel();
        self.insert_input(text.replace("\r\n", "\n").replace('\r', "\n"));
        self.refresh_candidates(true);
        self.start_resource_job(resources::Operation::Paste(text), PendingKind::Steering);
        if let Some(job) = &mut self.resources.job {
            job.replace_paste = true;
        }
    }

    pub(in crate::tui) fn prepare_resource_input(
        &mut self,
        source: &str,
        text: &str,
        kind: PendingKind,
    ) -> bool {
        if self.resources.ready.is_some() {
            return false;
        }
        if self.load_image_references(true, kind) {
            if let Some(job) = &mut self.resources.job {
                job.prepare_after = Some(text.to_owned());
            }
            return true;
        }
        let images = self.resources.active_images(source);
        if user_input::references(text).is_empty() && images.is_empty() {
            return false;
        }
        self.start_resource_job(
            resources::Operation::Prepare {
                source: source.into(),
                text: text.into(),
                images,
                limits: self.input_limits,
            },
            kind,
        );
        true
    }

    pub(in crate::tui) fn receive_resources(&mut self) -> bool {
        let Some(job) = &self.resources.job else {
            return false;
        };
        if job.source != self.input_text()
            || job.cursor != self.input.cursor()
            || !self.resource_input_available()
        {
            self.resources.cancel();
            return true;
        }
        let result = match job.reply.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(_) => Err(anyhow::anyhow!("输入资源线程已关闭")),
        };
        let job = self.resources.job.take().expect("当前输入资源任务");
        match result {
            Ok(resources::Output::Prepared(input)) => {
                let discussion = std::mem::take(&mut self.resources.plan_discussion);
                let display = input.display.clone();
                self.resources.ready = Some(input);
                if discussion {
                    self.queue_paused = false;
                    if self.send_task(Some(display.clone())) {
                        self.plan_panel = None;
                        self.plan_list_panel = None;
                        self.plan_list_pending = None;
                        self.remember_input();
                        self.clear_input();
                        self.append("You", &display);
                    }
                } else {
                    self.submit(job.kind);
                }
                // 状态变化导致提交未被接受时，不能把准备结果留给另一条输入。
                if !matches!(self.state, State::ChangingPlan) {
                    self.resources.ready = None;
                }
            }
            Ok(resources::Output::Images(images)) => {
                if job.replace_paste {
                    self.resources.undo_input(&mut self.input, false);
                }
                self.insert_images(images);
            }
            Ok(resources::Output::Text(_)) if job.replace_paste => {
                self.load_image_references(false, job.kind);
            }
            Ok(resources::Output::Text(text)) => {
                let before = self.input_text();
                let previous = self.resources.bindings.clone();
                let cursor = crate::tui::attachment_edit::offset(&self.input, self.input.cursor());
                let selection = self.input.selection_range().map(|((a, b), (c, d))| {
                    crate::tui::attachment_edit::offset(
                        &self.input,
                        ratatui_textarea::DataCursor(a, b),
                    )
                        ..crate::tui::attachment_edit::offset(
                            &self.input,
                            ratatui_textarea::DataCursor(c, d),
                        )
                });
                self.insert_input(text.replace("\r\n", "\n").replace('\r', "\n"));
                let after = self.input_text();
                let range = crate::tui::attachment_edit::edited_range(
                    &before,
                    &after,
                    cursor,
                    crate::tui::attachment_edit::offset(&self.input, self.input.cursor()),
                    selection,
                );
                self.resources.edited(before, after, previous, Some(range));
                self.refresh_candidates(true);
                self.load_image_references(false, job.kind);
            }
            Ok(resources::Output::References(replacements)) => {
                let before = self.input_text();
                let previous = self.resources.bindings.clone();
                let old_cursor =
                    crate::tui::attachment_edit::offset(&self.input, self.input.cursor());
                let mut cursor = old_cursor;
                let mut prepared_text = job.prepare_after.clone();
                let model_offset = prepared_text
                    .as_ref()
                    .and_then(|text| job.source.find(text));
                // 先按正文顺序分配编号，再从后向前替换，保持原始 UTF-8 范围有效。
                let replacements: Vec<_> = replacements
                    .into_iter()
                    .map(|(range, mut image)| {
                        image.marker = self.next_image_marker();
                        (range, image)
                    })
                    .collect();
                for (range, image) in replacements.into_iter().rev() {
                    if let Some(text) = &mut prepared_text {
                        if let Some(offset) = model_offset {
                            if range.start >= offset && range.end <= offset + text.len() {
                                text.replace_range(
                                    range.start - offset..range.end - offset,
                                    &image.marker,
                                );
                            }
                        } else {
                            *text = text.replacen(&job.source[range.clone()], &image.marker, 1);
                        }
                    }
                    let current = self.input_text();
                    let bindings = self.resources.bindings.clone();
                    crate::tui::attachment_edit::jump(&mut self.input, range.start);
                    self.input.cancel_selection();
                    self.input
                        .delete_str(current[range.clone()].chars().count());
                    self.input.insert_str(&image.marker);
                    let delta = image.marker.len() as isize - range.len() as isize;
                    if cursor >= range.end {
                        cursor = cursor.saturating_add_signed(delta);
                    } else if cursor > range.start {
                        cursor = range.start + image.marker.len();
                    }
                    self.resources.bindings.insert(
                        image.marker.clone(),
                        range.start..range.start + image.marker.len(),
                    );
                    let inserted_end = range.start + image.marker.len();
                    self.resources.images.push(image);
                    self.resources.shift_bindings(
                        &current,
                        &self.input_text(),
                        &bindings,
                        Some((range.start, range.end, inserted_end)),
                    );
                }
                crate::tui::attachment_edit::jump(&mut self.input, cursor);
                self.resources.record(before, self.input_text(), previous);
                self.refresh_candidates(true);
                if let Some(text) = prepared_text {
                    let source = self.input_text();
                    self.prepare_resource_input(&source, &text, job.kind);
                }
            }
            Err(error) => self.notify(Level::Error, format!("{error:#}")),
        }
        true
    }

    pub(in crate::tui) fn load_image_references(&mut self, all: bool, kind: PendingKind) -> bool {
        if !self.resource_input_available() || self.resources.job.is_some() {
            return false;
        }
        let source = self.input_text();
        let references: Vec<_> = user_input::references(&source)
            .into_iter()
            .filter(|reference| {
                reference
                    .path
                    .as_deref()
                    .is_some_and(|path| user_input::image_extension(std::path::Path::new(path)))
                    && (all || source[reference.end..].starts_with(char::is_whitespace))
            })
            .collect();
        if references.is_empty() {
            return false;
        }
        self.start_resource_job(resources::Operation::References(references), kind);
        self.resources.job.is_some()
    }

    fn next_image_marker(&mut self) -> String {
        loop {
            self.resources.next_image = self.resources.next_image.checked_add(1).unwrap_or(1);
            let marker = format!(
                "{} #{}",
                crate::tui::attachment_edit::ICON,
                self.resources.next_image
            );
            if !self.input_text().contains(&marker)
                && !self.resources.images.iter().any(|old| old.marker == marker)
            {
                return marker;
            }
        }
    }

    pub(in crate::tui) fn edit_input(&mut self, key: crossterm::event::KeyEvent) -> bool {
        crate::tui::attachment_edit::key(&mut self.input, key, &self.resources.bindings)
    }

    pub(in crate::tui) fn insert_images(&mut self, images: Vec<user_input::Image>) {
        let before = self.input_text();
        let previous = self.resources.bindings.clone();
        let cursor = self
            .input
            .selection_range()
            .map_or(self.input.cursor(), |((row, col), _)| {
                ratatui_textarea::DataCursor(row, col)
            });
        let start = crate::tui::attachment_edit::offset(&self.input, cursor);
        let end = self
            .input
            .selection_range()
            .map_or(start, |(_, (row, col))| {
                crate::tui::attachment_edit::offset(
                    &self.input,
                    ratatui_textarea::DataCursor(row, col),
                )
            });
        let mut markers = String::new();
        for mut image in images {
            image.marker = self.next_image_marker();
            let offset = start + markers.len();
            self.resources
                .bindings
                .insert(image.marker.clone(), offset..offset + image.marker.len());
            markers.push_str(&image.marker);
            self.resources.images.push(image);
        }
        let inserted_end = start + markers.len();
        self.input.insert_str(markers);
        self.resources.edited(
            before,
            self.input_text(),
            previous,
            Some((start, end, inserted_end)),
        );
        self.refresh_candidates(true);
    }
}
