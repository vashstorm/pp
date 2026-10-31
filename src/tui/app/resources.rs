use crate::{
    tui::{App, Level, PendingKind, State, resources},
    user_input,
};

impl App {
    pub(in crate::tui) fn resource_input_available(&self) -> bool {
        !self.shell_mode
            && self.auth_panel.is_none()
            && self.ask_panel.is_none()
            && self.model_panel.is_none()
            && self.roles_panel.is_none()
            && self.skill_panel.is_none()
            && self.session_panel.is_none()
            && self.tree_panel.is_none()
            && self.changes_panel.is_none()
            && self.subagent_panel.is_none()
            && self.plan_list_panel.is_none()
            && self.plan_leave_confirmation.is_none()
            && self
                .plan_panel
                .as_ref()
                .is_none_or(|panel| panel.input_focused())
            && self.mcp_panel.is_none()
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
        let images = self.resources.active_images(text);
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
                    let before = self.input_text();
                    let previous = self.resources.live.clone();
                    self.input.undo();
                    self.resources
                        .edited(before, self.input_text(), previous, true, false);
                }
                self.insert_images(images);
            }
            Ok(resources::Output::Text(_)) if job.replace_paste => {}
            Ok(resources::Output::Text(text)) => {
                let before = self.input_text();
                let previous = self.resources.live.clone();
                self.insert_input(text.replace("\r\n", "\n").replace('\r', "\n"));
                self.resources
                    .edited(before, self.input_text(), previous, false, false);
                self.refresh_candidates(true);
            }
            Err(error) => self.notify(Level::Error, format!("{error:#}")),
        }
        true
    }

    pub(in crate::tui) fn insert_images(&mut self, images: Vec<user_input::Image>) {
        for mut image in images {
            let before = self.input_text();
            let previous = self.resources.live.clone();
            loop {
                self.resources.next_image += 1;
                image.marker = format!("[图片 {}]", self.resources.next_image);
                if !self.input_text().contains(&image.marker) {
                    break;
                }
            }
            self.input.insert_str(&image.marker);
            self.notify(Level::Normal, format!("{} {}", image.marker, image.summary));
            self.resources.live.insert(image.marker.clone());
            self.resources.images.push(image);
            self.resources
                .edited(before, self.input_text(), previous, false, false);
        }
        self.refresh_candidates(true);
    }
}
