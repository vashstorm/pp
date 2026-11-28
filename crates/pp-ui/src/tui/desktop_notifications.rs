use std::{
    process::{Command, Stdio},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use crossterm::event::Event;
use pp_config::config::DesktopNotifications;
use pp_core::cancellation::CancellationToken;

use super::App;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Completed,
    Failed,
    PlanReady,
    Ask,
    Permission,
}

impl Kind {
    fn message(self) -> &'static str {
        match self {
            Self::Completed => "Task completed.",
            Self::Failed => "Task failed. Return to pp for details.",
            Self::PlanReady => "A plan is ready for review.",
            Self::Ask => "pp is waiting for your answer.",
            Self::Permission => "pp is waiting for your approval.",
        }
    }
}

#[derive(Default)]
enum Focus {
    #[default]
    Unknown,
    Foreground,
    Background,
}

pub(super) struct Notice {
    pub(super) kind: Kind,
    dialog: Option<Weak<CancellationToken>>,
}

impl Notice {
    pub(super) fn is_pending(&self) -> bool {
        self.dialog
            .as_ref()
            .is_none_or(|dialog| dialog.upgrade().is_some_and(|token| !token.is_cancelled()))
    }
}

struct Dialog {
    kind: Option<Kind>,
    cancellation: Arc<CancellationToken>,
}

#[derive(Default)]
pub(super) struct Pending {
    focus: Focus,
    tasks: Vec<Kind>,
    dialog: Option<Dialog>,
}

impl Pending {
    pub(super) fn update_focus(&mut self, event: &Event) -> bool {
        self.focus = match event {
            Event::FocusGained => Focus::Foreground,
            Event::FocusLost => Focus::Background,
            _ => return false,
        };
        true
    }

    pub(super) fn task_finished(&mut self, kind: Kind) {
        self.tasks.push(kind);
    }

    pub(super) fn dialog_opened(&mut self, kind: Option<Kind>, cancellation: &CancellationToken) {
        self.dialog = kind.map(|kind| Dialog {
            kind: Some(kind),
            cancellation: Arc::new(cancellation.clone()),
        });
    }

    pub(super) fn dialog_closed(&mut self) {
        self.dialog = None;
    }
}

impl App {
    pub(super) fn flush_desktop_notifications(
        &mut self,
        mode: DesktopNotifications,
        mut send: impl FnMut(Notice),
    ) {
        let pending = &mut self.desktop_notifications;
        let enabled = match mode {
            DesktopNotifications::Background => matches!(pending.focus, Focus::Background),
            DesktopNotifications::Always => true,
            DesktopNotifications::Off => false,
        };
        if !enabled {
            pending.tasks.clear();
        }
        for kind in pending.tasks.drain(..) {
            send(Notice { kind, dialog: None });
        }
        if let Some(dialog) = &mut pending.dialog
            && let Some(kind) = dialog.kind.take()
            && enabled
            && !dialog.cancellation.is_cancelled()
        {
            send(Notice {
                kind,
                // The sender thread re-checks when it picks up a queued notification so a finished conversation is not announced.
                dialog: Some(Arc::downgrade(&dialog.cancellation)),
            });
        }
    }
}

pub(super) struct Worker {
    sender: Option<SyncSender<Notice>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(super) fn start(mode: DesktopNotifications) -> Option<Self> {
        if mode == DesktopNotifications::Off || !cfg!(any(target_os = "macos", target_os = "linux"))
        {
            return None;
        }
        let (sender, receiver) = mpsc::sync_channel::<Notice>(16);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("pp-desktop-notifications".into())
            .spawn(move || {
                while let Ok(notice) = receiver.recv() {
                    if stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    if notice.is_pending()
                        && let Err(error) = deliver(
                            notification_command(notice.kind),
                            COMMAND_TIMEOUT,
                            &stopping,
                        )
                    {
                        tracing::warn!(event = "desktop_notification_failed", %error);
                    }
                }
            });
        match thread {
            Ok(thread) => Some(Self {
                sender: Some(sender),
                stop,
                thread: Some(thread),
            }),
            Err(error) => {
                tracing::warn!(event = "desktop_notification_worker_failed", %error);
                None
            }
        }
    }

    pub(super) fn send(&self, notice: Notice) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(notice);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn notification_command(kind: Kind) -> Command {
    if cfg!(target_os = "macos") {
        let mut command = Command::new("/usr/bin/osascript");
        command.args([
            "-e",
            "on run argv\n display notification (item 1 of argv) with title \"pp\"\nend run",
            kind.message(),
        ]);
        command
    } else {
        let mut command = Command::new("notify-send");
        command.args(["--app-name=pp", "--", "pp", kind.message()]);
        command
    }
}

fn deliver(mut command: Command, timeout: Duration, stop: &AtomicBool) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting desktop notification command")?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                ensure!(
                    status.success(),
                    "desktop notification command exited with {status}"
                );
                return Ok(());
            }
            Ok(None) => {}
            Err(error) => break Err(error).context("waiting for desktop notification command"),
        }
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break Err(anyhow!("desktop notification command timed out"));
        }
        thread::sleep(remaining.min(WAIT_INTERVAL));
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod integration {
    use super::*;

    #[test]
    fn notification_delivery_handles_missing_failed_and_stalled_commands() {
        let stop = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let missing = deliver(
            Command::new(dir.path().join("missing")),
            COMMAND_TIMEOUT,
            &stop,
        )
        .unwrap_err();
        assert_eq!(
            missing.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );

        let mut fail = Command::new("/bin/sh");
        fail.args(["-c", "exit 7"]);
        assert!(
            deliver(fail, COMMAND_TIMEOUT, &stop)
                .unwrap_err()
                .to_string()
                .contains('7')
        );

        let mut stall = Command::new("/bin/sleep");
        stall.arg("60");
        assert!(
            deliver(stall, Duration::from_millis(50), &stop)
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );

        stop.store(true, Ordering::Relaxed);
        let mut shutdown = Command::new("/bin/sleep");
        shutdown.arg("60");
        deliver(shutdown, Duration::ZERO, &stop).unwrap();
    }
}
