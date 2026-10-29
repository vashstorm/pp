use crate::{
    agent::{Agent, Bootstrap},
    auth::{
        Provider,
        service::{self, Progress, Update},
    },
    cancellation::CancellationToken,
};
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};
use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

pub(super) enum Request {
    List,
    Login {
        provider: Provider,
        key: Option<String>,
    },
    Discover(Provider),
    Logout(Provider),
    Open(String),
    Bootstrap(Bootstrap),
}

pub(super) enum Reply {
    Listed(Result<Vec<Provider>>),
    Progress(Progress),
    Updated(Result<Update>),
    Opened(Result<()>),
    Bootstrapped(Result<Box<Agent>>),
}

pub(super) struct Worker {
    pub(super) requests: Sender<(Request, CancellationToken)>,
    pub(super) replies: Receiver<Reply>,
    browser_results: Sender<Reply>,
    lifetime: CancellationToken,
}

impl Worker {
    #[cfg(test)]
    pub(super) fn fixture() -> (Self, Receiver<(Request, CancellationToken)>, Sender<Reply>) {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        (
            Self {
                requests,
                replies,
                browser_results: sender.clone(),
                lifetime: CancellationToken::new(),
            },
            receiver,
            sender,
        )
    }

    pub(super) fn new() -> Result<Self> {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        let browser_results = sender.clone();
        thread::Builder::new()
            .name("pp-auth".into())
            .spawn(move || {
                while let Ok((request, cancellation)) = receiver.recv() {
                    let mut progress = |progress| {
                        let _ = sender.send(Reply::Progress(progress));
                    };
                    let dir = crate::config::global_dir;
                    let reply = match request {
                        Request::List => Reply::Listed(dir().and_then(|dir| service::list(&dir))),
                        Request::Login { provider, key } => Reply::Updated(dir().and_then(|dir| {
                            service::login(&dir, provider, key, &cancellation, &mut progress)
                        })),
                        Request::Discover(provider) => Reply::Updated(dir().and_then(|dir| {
                            service::discover(&dir, provider, &cancellation, &mut progress)
                        })),
                        Request::Logout(provider) => Reply::Updated(
                            dir().and_then(|dir| service::logout(&dir, provider, &cancellation)),
                        ),
                        Request::Open(url) => Reply::Opened(service::open_browser(&url)),
                        Request::Bootstrap(factory) => {
                            Reply::Bootstrapped(factory.build().map(Box::new))
                        }
                    };
                    if sender.send(reply).is_err() {
                        break;
                    }
                }
            })
            .context("starting authentication worker")?;
        Ok(Self {
            requests,
            replies,
            browser_results,
            lifetime: CancellationToken::new(),
        })
    }

    pub(super) fn token(&self) -> CancellationToken {
        self.lifetime.child_token()
    }

    pub(super) fn open(&self, url: String) -> Result<()> {
        let results = self.browser_results.clone();
        thread::Builder::new()
            .name("pp-auth-browser".into())
            .spawn(move || {
                let _ = results.send(Reply::Opened(service::open_browser(&url)));
            })
            .context("starting browser opener")?;
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}

const GOLD: Color = Color::Rgb(255, 190, 50);
const CYAN: Color = Color::Rgb(0, 170, 210);
const GREEN: Color = Color::Rgb(132, 211, 125);
const MUTED: Color = Color::Rgb(117, 124, 139);
const BG: Color = Color::Rgb(39, 41, 54);

pub(super) enum Action {
    None,
    Close,
    Cancel,
    Login(Provider, Option<String>),
    Discover(Provider),
    Logout(Provider),
    Open(String),
}

#[derive(Clone, Copy)]
enum Page {
    List,
    Options(Provider),
    Key(Provider),
    Waiting(Provider),
}

pub(super) struct Panel {
    logout: bool,
    pub(super) logged: Vec<Provider>,
    page: Page,
    selected: usize,
    offset: usize,
    rows: Vec<(Rect, usize)>,
    url_area: Rect,
    url: String,
    code: String,
    key: String,
    pub(super) message: String,
    pub(super) busy: bool,
    pub(super) closing: bool,
}

impl Panel {
    pub(super) fn new(logout: bool) -> Self {
        Self {
            logout,
            logged: Vec::new(),
            page: Page::List,
            selected: 0,
            offset: 0,
            rows: Vec::new(),
            url_area: Rect::default(),
            url: String::new(),
            code: String::new(),
            key: String::new(),
            message: "Loading authorization status…".into(),
            busy: true,
            closing: false,
        }
    }

    pub(super) fn listed(&mut self, logged: Vec<Provider>) {
        self.logged = logged;
        self.busy = false;
        self.message.clear();
    }

    pub(super) fn updated(&mut self, update: Update) {
        self.logged = update.logged;
        self.message = update.message;
        self.page = Page::List;
        self.selected = 0;
        self.offset = 0;
        self.clear_secrets();
        self.busy = false;
    }

    pub(super) fn progress(&mut self, progress: Progress) {
        match progress {
            Progress::Verification { url, code } => {
                self.url = url;
                self.code = code;
                self.message = "Waiting for authorization… (Esc to cancel)".into();
            }
            Progress::Discovering => {
                self.code.clear();
                self.message = "Discovering subscription models… (Esc to cancel)".into();
            }
            Progress::BrowserFailed => {
                self.message =
                    "Could not open browser. Click the URL or press F2 to open it manually.".into();
            }
        }
    }

    fn providers(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|provider| !self.logout || self.logged.contains(provider))
            .collect()
    }

    fn count(&self) -> usize {
        match self.page {
            Page::List => self.providers().len(),
            Page::Options(_) => 2,
            _ => 0,
        }
    }

    fn clear_secrets(&mut self) {
        self.key.clear();
        self.code.clear();
        self.url.clear();
    }

    fn select(&mut self) -> Action {
        match self.page {
            Page::List => {
                let Some(provider) = self.providers().get(self.selected).copied() else {
                    return Action::None;
                };
                if self.logout {
                    self.busy = true;
                    return Action::Logout(provider);
                }
                if self.logged.contains(&provider) {
                    self.page = Page::Options(provider);
                    self.selected = 0;
                    self.offset = 0;
                    self.message.clear();
                    Action::None
                } else {
                    self.start(provider)
                }
            }
            Page::Options(provider) => {
                if self.selected == 0 {
                    self.start(provider)
                } else {
                    self.page = Page::Waiting(provider);
                    self.busy = true;
                    Action::Discover(provider)
                }
            }
            Page::Key(provider) => {
                if self.key.trim().is_empty() {
                    self.message = "API key must not be blank.".into();
                    return Action::None;
                }
                self.page = Page::Waiting(provider);
                self.busy = true;
                Action::Login(provider, Some(std::mem::take(&mut self.key)))
            }
            Page::Waiting(_) => Action::None,
        }
    }

    fn start(&mut self, provider: Provider) -> Action {
        self.clear_secrets();
        self.message.clear();
        self.selected = 0;
        self.offset = 0;
        if let Some(url) = provider.key_url() {
            self.page = Page::Key(provider);
            self.url = url.into();
            Action::Open(url.into())
        } else {
            self.page = Page::Waiting(provider);
            self.busy = true;
            self.message = "Starting device authorization… (Esc to cancel)".into();
            Action::Login(provider, None)
        }
    }

    fn move_by(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.count().saturating_sub(1));
    }

    pub(super) fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.code == KeyCode::Esc {
                    self.key.clear();
                    return if self.busy {
                        self.closing = true;
                        Action::Cancel
                    } else {
                        Action::Close
                    };
                }
                if key.code == KeyCode::F(2)
                    && key.kind == KeyEventKind::Press
                    && !self.url.is_empty()
                {
                    return Action::Open(self.url.clone());
                }
                if self.busy {
                    return Action::None;
                }
                if matches!(self.page, Page::Key(_)) {
                    match key.code {
                        KeyCode::Backspace => {
                            self.key.pop();
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.key.clear()
                        }
                        KeyCode::Char(ch)
                            if !ch.is_control()
                                && !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            if self.key.len() + ch.len_utf8() <= 16384 {
                                self.key.push(ch);
                            }
                        }
                        KeyCode::Enter => return self.select(),
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Up => self.move_by(-1),
                        KeyCode::Down => self.move_by(1),
                        KeyCode::Home => self.selected = 0,
                        KeyCode::End => self.selected = self.count().saturating_sub(1),
                        KeyCode::Enter => return self.select(),
                        _ => {}
                    }
                }
            }
            Event::Paste(text) if !self.busy && matches!(self.page, Page::Key(_)) => {
                let text = text.trim();
                if !text.chars().any(char::is_control) && self.key.len() + text.len() <= 16384 {
                    self.key.push_str(text);
                } else {
                    self.message = "Paste a single API key (up to 16384 bytes).".into();
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if self.url_area.contains((mouse.column, mouse.row).into())
                        && !self.url.is_empty() =>
                {
                    return Action::Open(self.url.clone());
                }
                MouseEventKind::Down(MouseButton::Left) if !self.busy => {
                    if let Some((_, index)) = self
                        .rows
                        .iter()
                        .find(|(area, _)| area.contains((mouse.column, mouse.row).into()))
                    {
                        self.selected = *index;
                        return self.select();
                    }
                }
                MouseEventKind::ScrollDown if !self.busy => self.move_by(1),
                MouseEventKind::ScrollUp if !self.busy => self.move_by(-1),
                _ => {}
            },
            _ => {}
        }
        Action::None
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.rows.clear();
        self.url_area = Rect::default();
        frame.render_widget(Clear, area);
        let title = match self.page {
            Page::List => {
                if self.logout {
                    "Select provider to logout".into()
                } else {
                    "Select provider to login".into()
                }
            }
            Page::Options(provider) => format!("Login to {}", provider.label()),
            Page::Key(provider) | Page::Waiting(provider) => {
                format!("Login to {}", provider.label())
            }
        };
        let padding = u16::from(area.width >= 8);
        let desired = if matches!(self.page, Page::Key(_) | Page::Waiting(_)) {
            16
        } else {
            self.count().max(1) as u16 + 5
        };
        let panel_area = Rect::new(
            area.x + padding,
            area.y + u16::from(area.height > 2),
            area.width.saturating_sub(padding * 2),
            desired.min(area.height.saturating_sub(u16::from(area.height > 2))),
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(CYAN))
            .style(Style::default().bg(BG).fg(Color::White))
            .title(Line::styled(
                format!(" {title} "),
                Style::default().fg(GOLD),
            ));
        let inner = block.inner(panel_area);
        frame.render_widget(block, panel_area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let body = Rect::new(
            inner.x + u16::from(inner.width > 2),
            inner.y,
            inner.width.saturating_sub(u16::from(inner.width > 2) * 2),
            inner.height,
        );
        let footer = body.height.saturating_sub(3);
        match self.page {
            Page::List | Page::Options(_) => {
                let capacity = usize::from(footer.max(1));
                if self.selected < self.offset {
                    self.offset = self.selected;
                }
                if self.selected >= self.offset + capacity {
                    self.offset = self.selected + 1 - capacity;
                }
                let providers = self.providers();
                let count = self.count();
                for (index, provider) in providers
                    .iter()
                    .enumerate()
                    .take(count.min(self.offset + capacity))
                    .skip(self.offset)
                {
                    let row =
                        Rect::new(body.x, body.y + (index - self.offset) as u16, body.width, 1);
                    let chosen = index == self.selected;
                    let label = if matches!(self.page, Page::List) {
                        provider.label()
                    } else if index == 0 {
                        "Log in again"
                    } else {
                        "Retry model discovery"
                    };
                    let color = if chosen { GOLD } else { Color::White };
                    let mut spans = vec![
                        Span::styled(if chosen { "❯ " } else { "  " }, Style::default().fg(GOLD)),
                        Span::styled(label, Style::default().fg(color)),
                    ];
                    if matches!(self.page, Page::List) && self.logged.contains(provider) {
                        spans.push(Span::styled(" ● logged in", Style::default().fg(GREEN)));
                        spans.push(Span::styled(
                            if provider.key_url().is_some() {
                                " (api key)"
                            } else {
                                " (login)"
                            },
                            Style::default().fg(MUTED),
                        ));
                    }
                    frame.render_widget(Line::from(spans), row);
                    self.rows.push((row, index));
                }
                if count == 0 {
                    frame.render_widget(
                        Line::styled("No providers are logged in.", Style::default().fg(MUTED)),
                        Rect { height: 1, ..body },
                    );
                }
            }
            Page::Key(provider) | Page::Waiting(provider) => {
                let url_height = if self.url.is_empty() {
                    1
                } else {
                    self.url
                        .chars()
                        .count()
                        .div_ceil(usize::from(body.width.max(1))) as u16
                };
                self.url_area = Rect::new(body.x, body.y, body.width, url_height.min(footer));
                frame.render_widget(
                    Paragraph::new(self.url.as_str())
                        .style(Style::default().fg(GOLD).add_modifier(Modifier::UNDERLINED))
                        .wrap(Wrap { trim: false }),
                    self.url_area,
                );
                let mut lines = vec![
                    Line::styled("Click URL or F2 to open", Style::default().fg(MUTED)),
                    Line::default(),
                ];
                if matches!(self.page, Page::Key(_)) {
                    lines.extend([
                        Line::styled(
                            "Copy your API key from the dashboard",
                            Style::default().fg(Color::Yellow),
                        ),
                        Line::default(),
                        Line::from(format!(
                            "Paste your {} API key",
                            if provider == Provider::Zai {
                                "Z.AI"
                            } else {
                                "GLM"
                            }
                        )),
                        Line::styled("e.g., sk-...", Style::default().fg(MUTED)),
                        Line::from(format!(
                            "> {}",
                            "•".repeat(
                                self.key
                                    .chars()
                                    .count()
                                    .min(usize::from(body.width.saturating_sub(3)))
                            )
                        )),
                        Line::styled(
                            "(Esc to cancel, Enter to submit)",
                            Style::default().fg(MUTED),
                        ),
                    ]);
                } else if !self.code.is_empty() {
                    lines.push(Line::from(format!("Verification code: {}", self.code)));
                    lines.push(Line::styled(
                        "Complete authorization in your browser.",
                        Style::default().fg(GOLD),
                    ));
                }
                let top = body.y + url_height.min(footer);
                if matches!(self.page, Page::Key(_)) {
                    let preceding = Paragraph::new(lines[..6].to_vec()).wrap(Wrap { trim: false });
                    let row = top.saturating_add(preceding.line_count(body.width) as u16);
                    let column = body.x.saturating_add(2).saturating_add(
                        self.key
                            .chars()
                            .count()
                            .min(usize::from(body.width.saturating_sub(3)))
                            as u16,
                    );
                    if row < body.y + footer && column < body.right() {
                        frame.set_cursor_position((column, row));
                    }
                }
                frame.render_widget(
                    Paragraph::new(lines).wrap(Wrap { trim: false }),
                    Rect::new(body.x, top, body.width, footer.saturating_sub(top - body.y)),
                );
            }
        }
        let message = if self.message.is_empty() {
            match self.page {
                Page::List | Page::Options(_) => "↑↓ navigate · Enter select · Esc cancel",
                _ => "",
            }
        } else {
            &self.message
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            Rect::new(body.x, body.y + footer, body.width, body.height - footer),
        );
    }
}
