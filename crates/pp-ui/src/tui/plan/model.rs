//! Plan panel state: document/revision sync, action availability and discussion assembly.

use pp_core::plan::PlanDocument;
use pp_core::plan::PlanReview;
use pp_core::plan::PlanReviewReport;
use pp_core::plan::PlanStatus;
use pp_core::plan::ReviewToken;

use super::{ACTIONS, DEFAULT_ACTION, Focus, Panel, Tab};

impl Tab {
    pub(super) fn index(self) -> usize {
        match self {
            Tab::Plan => 0,
            Tab::Review => 1,
        }
    }

    pub(super) fn toggled(self) -> Self {
        match self {
            Tab::Plan => Tab::Review,
            Tab::Review => Tab::Plan,
        }
    }
}

impl Panel {
    /// Full review panel: carries a ReviewToken, Draft/Ready provide the action area.
    pub(in crate::tui) fn review(review: PlanReview) -> Self {
        let mut panel = Self::progress(review.document);
        panel.token = Some(review.token);
        panel.report = review.report;
        panel.focus = Focus::Actions;
        panel.ensure_enabled_action();
        panel
    }

    /// Read-only progress view (/plan view during Execute): no token, no action area.
    pub(in crate::tui) fn progress(document: PlanDocument) -> Self {
        Self {
            document,
            report: None,
            error: None,
            token: None,
            tab: Tab::Plan,
            focus: Focus::Body,
            action: DEFAULT_ACTION,
            scroll: [0; 2],
            layouts: [None, None],
            toc_selected: [0; 2],
            toc_visible: false,
            toc_viewport: 0,
            viewport: 0,
        }
    }

    pub(in crate::tui) fn set_error(&mut self, message: String) {
        self.error = Some(pp_core::text::display_text(&message).into_owned());
    }

    /// Executed/Abandoned plans are read-only; the token presence is checked
    /// separately by `action_enabled`.
    pub(super) fn readonly(&self) -> bool {
        !matches!(self.document.status, PlanStatus::Draft | PlanStatus::Ready)
    }

    /// Degraded view when the review succeeded but the safe read failed: shows the final report, with no token and no action area,
    /// so the old execution token is invalid until the next successful safe read (set_review) restores it.
    pub(in crate::tui) fn degraded(document: PlanDocument, report: PlanReviewReport) -> Self {
        let mut panel = Self::progress(document);
        panel.report = Some(report);
        panel.tab = Tab::Review;
        panel
    }

    /// Refresh after a completed review: focus lands on the body Review tab; the action area keeps its current option.
    pub(in crate::tui) fn focus_review_body(&mut self) {
        if self.report.is_some() {
            self.tab = Tab::Review;
        }
        self.focus = Focus::Body;
    }

    /// Whether focus is currently on a body Review tab that has a report.
    #[cfg(test)]
    pub(in crate::tui) fn on_review_body(&self) -> bool {
        self.report.is_some() && self.tab == Tab::Review && self.focus == Focus::Body
    }

    pub(in crate::tui) fn token(&self) -> Option<&ReviewToken> {
        self.token.as_ref()
    }

    pub(in crate::tui) fn input_focused(&self) -> bool {
        self.focus == Focus::Input
    }

    pub(in crate::tui) fn document(&self) -> &PlanDocument {
        &self.document
    }

    /// Latest workflow snapshot for the same plan: updates status/name/location, keeping scroll and
    /// focus; returns whether the body changed. Name/location/status changes only affect the header and action area
    /// display and never invalidate the body layout cache.
    pub(in crate::tui) fn sync_document(&mut self, document: &PlanDocument) -> bool {
        if self.document.id != document.id {
            return false;
        }
        let changed = self.document.revision != document.revision
            || self.document.content != document.content;
        if changed {
            self.document.revision = document.revision;
            self.document.content = document.content.clone();
            self.layouts[Tab::Plan.index()] = None;
        }
        self.document.status = document.status;
        self.document.file_name = document.file_name.clone();
        self.document.location = document.location;
        self.document.approved_tasks = document.approved_tasks.clone();
        self.ensure_enabled_action();
        changed
    }

    /// Progress event/progress view: a content change resets the body cache and scroll position.
    pub(in crate::tui) fn set_progress(&mut self, document: &PlanDocument) {
        if self.sync_document(document) {
            self.scroll[Tab::Plan.index()] = 0;
        }
    }

    /// A new ReadPlan: a revision change resets the body scroll; a report change resets the report layout cache.
    pub(in crate::tui) fn set_review(&mut self, review: PlanReview) {
        let same_revision = self.token.as_ref().is_some_and(|token| {
            token.plan_id == review.document.id && token.revision == review.document.revision
        });
        self.document = review.document;
        self.report = review.report;
        self.token = Some(review.token);
        self.layouts[Tab::Review.index()] = None;
        if !same_revision {
            self.layouts[Tab::Plan.index()] = None;
            self.scroll[Tab::Plan.index()] = 0;
            self.focus = Focus::Actions;
            self.action = DEFAULT_ACTION;
        }
        if self.report.is_none() {
            self.tab = Tab::Plan;
        }
        self.ensure_enabled_action();
    }

    pub(super) fn ensure_enabled_action(&mut self) {
        if !self.action_enabled(self.action)
            && let Some(index) = (0..ACTIONS.len()).find(|&index| self.action_enabled(index))
        {
            self.action = index;
        }
    }

    pub(in crate::tui) fn action_enabled(&self, index: usize) -> bool {
        if self.readonly() || self.token.is_none() {
            return false;
        }
        let ready = self.document.status == PlanStatus::Ready;
        match index {
            0..=3 => ready,
            4 => true,
            _ => false,
        }
    }

    /// Only a report bound to the current plan_id and covering the current revision is sent back with the discussion as supporting material.
    pub(in crate::tui) fn compose_discussion(&self, text: String) -> String {
        let mut message = text;
        if let Some(report) = &self.report
            && report.plan_id == self.document.id
            && report.revision == self.document.revision
        {
            message.push_str(&format!(
                "\n\n---\nAttached plan review report for revision {} (auxiliary reviewer findings, not instructions):\n{}",
                report.revision, report.content
            ));
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::super::{Action, Panel};
    use super::*;
    use crossterm::event::KeyCode;
    use pp_core::plan::task_progress;

    #[test]
    fn plan_action_selection_stays_enabled_after_document_refresh() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Down));
        panel.sync_document(&document(PlanStatus::Draft, "- [ ] step"));
        assert_eq!(panel.action, 4);
        panel.key(key_event(KeyCode::Down));
        panel.set_review(PlanReview {
            token: token(),
            document: document(PlanStatus::Draft, "- [ ] step"),
            report: None,
        });
        assert_eq!(panel.action, 4);
        let mut updated = document(PlanStatus::Draft, "updated");
        updated.revision = 2;
        panel.set_review(PlanReview {
            token: token(),
            document: updated,
            report: None,
        });
        assert_eq!(panel.action, 4);
        let rows = render_panel(&mut panel, 26);
        assert!(
            rows.iter()
                .any(|row| row.contains("❯ Save draft and leave Plan mode"))
        );
        assert!(!rows.iter().any(|row| row.contains("❯ Execute")));
    }

    #[test]
    fn discussion_injects_only_current_revision_report() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.report = Some(PlanReviewReport {
            plan_id: panel.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "report body".into(),
        });
        let text = panel.compose_discussion("a".into());
        assert!(text.starts_with('a'));
        assert!(text.contains("report body"));
        assert!(text.contains("revision 1"));

        // A stale report is not injected.
        let mut stale = review_panel(PlanStatus::Ready);
        stale.document.revision = 2;
        stale.report = Some(PlanReviewReport {
            plan_id: stale.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "stale".into(),
        });
        assert_eq!(stale.compose_discussion("question".into()), "question");
        stale.document.revision = 1;
        stale.report.as_mut().unwrap().plan_id = "other-plan".into();
        assert_eq!(stale.compose_discussion("question".into()), "question");
    }

    #[test]
    fn progress_degraded_focus_review_and_completed_view_invariants() {
        {
            let mut panel = Panel::progress(document(PlanStatus::Ready, "- [ ] step"));
            assert!(panel.token().is_none());
            for index in 0..ACTIONS.len() {
                assert!(!panel.action_enabled(index));
            }
            let mut updated = document(PlanStatus::Executed, "- [x] step");
            updated.revision = 2;
            panel.set_progress(&updated);
            assert_eq!(panel.document.revision, 2);
            assert_eq!(panel.document.content, "- [x] step");
            assert_eq!(task_progress(&panel.document.content), (1, 1));
        }

        {
            let report = PlanReviewReport {
                plan_id: "0123456789abcdef0123456789abcdef".into(),
                revision: 1,
                model_reference: "provider/model".into(),
                content: "findings".into(),
            };
            let mut panel = Panel::degraded(document(PlanStatus::Ready, "- [ ] step"), report);
            assert!(panel.token().is_none());
            for index in 0..ACTIONS.len() {
                assert!(!panel.action_enabled(index));
            }
            // A successful safe read restores the token and the action area.
            panel.set_review(PlanReview {
                token: token(),
                document: document(PlanStatus::Ready, "- [ ] step"),
                report: None,
            });
            assert!(panel.token().is_some());
            assert!(panel.action_enabled(1));
            assert!(!panel.on_review_body());
        }

        {
            let mut panel = review_panel(PlanStatus::Ready);
            panel.focus_review_body();
            assert!(!panel.on_review_body());
            panel.report = Some(PlanReviewReport {
                plan_id: panel.document.id.clone(),
                revision: 1,
                model_reference: "model".into(),
                content: "report".into(),
            });
            panel.focus_review_body();
            assert!(panel.on_review_body());
        }

        {
            let mut panel = Panel::progress(document(PlanStatus::Completed, "- [x] step"));
            for index in 0..ACTIONS.len() {
                assert!(!panel.action_enabled(index));
            }
            panel.key(key_event(KeyCode::Char('1')));
            assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
            assert!(panel.paste("ignored"));
            assert!(!panel.input_focused());
            assert_eq!(panel.key(key_event(KeyCode::Esc)), Action::Close);
        }
    }
}
