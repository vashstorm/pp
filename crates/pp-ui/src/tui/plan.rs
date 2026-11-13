//! 计划审阅面板：只渲染 worker 发布的快照，文件/网络访问全部留在 worker。
//! 键盘与绘制路径不做任何 IO；审批必须携带面板看到的 ReviewToken。

mod keys;
mod model;
mod render;

use crossterm::event::KeyEvent;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanReviewReport;
use pp_core::plan::ReviewToken;
use ratatui::text::Line;

use super::markdown;

const ACTIONS: [&str; 5] = [
    "Execute in a new session",
    "Execute with context",
    "Compact context and execute",
    "Review Plan",
    "Save draft and leave Plan mode",
];
/// 打开完整审阅面板时直接聚焦操作区，默认选中第一项可用操作。
const DEFAULT_ACTION: usize = 0;
const HINT: &str = "Tab focus · ↑↓ choose/scroll · Enter run/jump/send · ←→ tabs · Esc close";
const READONLY_HINT: &str = "Read-only view · Tab focus · ↑↓ scroll · Enter jump · Esc close";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Close,
    Approve(ExecutionContext),
    Review,
    SaveAndLeave,
    EditInput(KeyEvent),
    DiscussInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Plan,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Toc,
    Body,
    Actions,
    Input,
}

struct BodyLayout {
    width: u16,
    lines: Vec<Line<'static>>,
    headings: Vec<markdown::HeadingAnchor>,
}

pub(super) struct Panel {
    document: PlanDocument,
    report: Option<PlanReviewReport>,
    error: Option<String>,
    token: Option<ReviewToken>,
    tab: Tab,
    focus: Focus,
    action: usize,
    scroll: [usize; 2],
    // 正文与目录锚点只在宽度或正文变化时重建。
    layouts: [Option<BodyLayout>; 2],
    toc_selected: [usize; 2],
    toc_visible: bool,
    toc_viewport: usize,
    viewport: usize,
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pp_core::plan::PlanReview;
    use pp_core::plan::PlanStatus;

    pub(super) fn document(status: PlanStatus, content: &str) -> PlanDocument {
        PlanDocument {
            id: "0123456789abcdef0123456789abcdef".into(),
            revision: 1,
            title: "Plan".into(),
            content: content.into(),
            status,
            origin_prompt: "original task".into(),
            file_name: Some("create-canary.md".into()),
            location: pp_core::plan::PlanLocation::Drafts,
            approved_tasks: None,
        }
    }

    pub(super) fn token() -> ReviewToken {
        ReviewToken {
            session_id: "session".into(),
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            path: ".pp/drafts/create-canary.md".into(),
            revision: 1,
            content: "- [ ] step".into(),
        }
    }

    pub(super) fn review_panel(status: PlanStatus) -> Panel {
        Panel::review(PlanReview {
            token: token(),
            document: document(status, "- [ ] step"),
            report: None,
        })
    }

    pub(super) fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(super) fn render_panel(panel: &mut Panel, height: u16) -> Vec<String> {
        render_panel_size(panel, 100, height)
    }

    pub(super) fn render_panel_size(panel: &mut Panel, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), (0, 1), None))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }
}
