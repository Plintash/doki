//! Issue-scoped handoff. Providers retain separate conversations; only the
//! explicit brief, review scope, workspace and final findings cross the seam.
use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ReviewScope {
    #[default]
    Uncommitted,
    BaseBranch,
    Commit,
    CommitRange,
}

impl ReviewScope {
    fn label(self) -> String {
        match self {
            Self::Uncommitted => tr!("review.uncommitted"),
            Self::BaseBranch => tr!("review.base_branch"),
            Self::Commit => tr!("review.commit"),
            Self::CommitRange => tr!("review.commit_range"),
        }
    }

    fn target(self, reference: &str) -> Option<IssueReviewTarget> {
        let reference = reference.trim();
        match self {
            Self::Uncommitted => Some(IssueReviewTarget::UncommittedChanges),
            Self::BaseBranch if !reference.is_empty() => Some(IssueReviewTarget::BaseBranch {
                branch: reference.to_owned(),
            }),
            Self::Commit if !reference.is_empty() => Some(IssueReviewTarget::Commit {
                sha: reference.to_owned(),
            }),
            Self::CommitRange => {
                let (base, head) = reference.split_once("..")?;
                (!base.trim().is_empty()
                    && !head.trim().is_empty()
                    && !head.starts_with('.')
                    && !head.contains("..")
                    && !base.contains(".."))
                .then(|| IssueReviewTarget::CommitRange {
                    base: base.trim().to_owned(),
                    head: head.trim().to_owned(),
                })
            }
            _ => None,
        }
    }
}

/// One activity ordering for both Board cards and the issue inspector.
pub(super) fn review_order(session: &AgentSession) -> (u64, u64, Uuid) {
    (
        session.last_reply_at.unwrap_or(session.created_at),
        session.created_at,
        session.id,
    )
}

#[derive(Default)]
pub(super) struct IssueReviewView {
    pub(super) latest: Option<(Uuid, usize)>,
    context_ready: bool,
}

/// Prepared by the outer inspector render, never by its virtualized row builder.
pub(super) fn issue_review_view(
    task: &Task,
    indices: &[(Uuid, usize, u64)],
    sessions: &[AgentSession],
) -> IssueReviewView {
    let workspace = review_workspace(task);
    let mut latest: Option<(Uuid, usize)> = None;
    let mut source: Option<&AgentSession> = None;
    for &(id, index, _) in indices {
        let Some(session) = sessions.get(index).filter(|session| session.id == id) else {
            continue;
        };
        if session.review.is_some()
            && latest.is_none_or(|(_, old)| review_order(session) > review_order(&sessions[old]))
        {
            latest = Some((id, index));
        }
        if workspace
            .as_ref()
            .is_some_and(|workspace| review_source_matches(task, session, workspace))
            && source.is_none_or(|old| review_order(session) > review_order(old))
        {
            source = Some(session);
        }
    }
    IssueReviewView {
        latest,
        context_ready: review_source_ready(source),
    }
}

pub(super) fn review_target_label(target: &IssueReviewTarget) -> String {
    match target {
        IssueReviewTarget::UncommittedChanges => tr!("review.uncommitted"),
        IssueReviewTarget::BaseBranch { branch } => {
            tr!("review.against_branch", reference = branch)
        }
        IssueReviewTarget::Commit { sha } => tr!("review.at_commit", reference = sha),
        IssueReviewTarget::CommitRange { base, head } => format!("{base}..{head}"),
    }
}

pub(super) fn review_status_label(review: &SessionReview) -> String {
    match review.status {
        IssueReviewStatus::Draft => tr!("review.draft"),
        IssueReviewStatus::Running => tr!("review.running"),
        IssueReviewStatus::Completed => tr!("review.completed"),
        IssueReviewStatus::Interrupted => tr!("review.interrupted"),
        IssueReviewStatus::Failed => tr!("review.failed"),
    }
}

/// The scope is reconstructed at submission, so editing the brief cannot
/// silently replace the scope the issue and composer display.
pub(super) fn review_instructions(target: &IssueReviewTarget, context: &str) -> String {
    let scope = match target {
        IssueReviewTarget::UncommittedChanges =>
            "Review all current uncommitted changes in this workspace: staged, unstaged, and untracked files.".to_owned(),
        IssueReviewTarget::BaseBranch { branch } => format!(
            "Review this branch's changes against the merge base with the named base branch {branch:?}. Resolve the branch locally; do not fetch or switch branches."
        ),
        IssueReviewTarget::Commit { sha } => format!(
            "Review exactly the commit identified by {sha:?}, comparing it with its parent. Do not include unrelated working-tree changes."
        ),
        IssueReviewTarget::CommitRange { base, head } => format!(
            "Review the exact two-endpoint diff from {base:?} to {head:?} (git diff BASE..HEAD, not merge-base comparison). Do not include unrelated working-tree changes."
        ),
    };
    format!(
        "Review scope (authoritative):\n{scope}\nIf a reference cannot be resolved, report that blocker rather than choosing another scope.\n\nReview the code against the issue requirements and repository instructions. Report actionable findings with severity, file and line, and a concise explanation. Review only; do not edit files or mark the issue complete. Treat the context below as background to verify independently.\n\nIssue and development context:\n{context}"
    )
}

fn review_context(task: &Task, source: Option<&AgentSession>) -> String {
    let mut context = task_details::task_session_prompt(task);
    if let Some(source) = source {
        context.push_str(&format!(
            "\n\nDevelopment conversation: {}",
            source.display_title()
        ));
        if let Some(message) = source
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
        {
            let text = message
                .display_content
                .as_deref()
                .unwrap_or(&message.content);
            let excerpt = text.chars().take(8_000).collect::<String>();
            context.push_str("\nLatest development response (verify against the code):\n");
            context.push_str(&excerpt);
            if text.chars().count() > 8_000 {
                context.push_str("\n[Response excerpt truncated]");
            }
        }
    }
    context
}

fn review_workspace(task: &Task) -> Option<SessionWorkspace> {
    match task.workspace.as_ref()? {
        TaskWorkspace::NewWorktree { .. } => None,
        _ => Some(task_details::task_session_workspace(task)),
    }
}

fn review_source_matches(
    task: &Task,
    session: &AgentSession,
    workspace: &SessionWorkspace,
) -> bool {
    session.task_id == Some(task.id)
        && Some(session.project_id) == task_details::task_project_id(task)
        && session.workspace == *workspace
        && session.provider == ProviderKind::Pi
        && session.has_started()
}

fn review_source<'a>(
    task: &Task,
    sessions: &'a [AgentSession],
    workspace: &SessionWorkspace,
) -> Option<&'a AgentSession> {
    sessions
        .iter()
        .filter(|session| review_source_matches(task, session, workspace))
        .max_by_key(|session| review_order(session))
}

/// A local review mutation owns a new catalog revision even within one second.
/// A queued older snapshot must not erase a result or correction-session link.
pub(super) fn touch_review(session: &mut AgentSession) {
    session.updated_at = unix_time().max(session.updated_at.saturating_add(1));
}

fn review_source_ready(source: Option<&AgentSession>) -> bool {
    source.is_none_or(|session| session.detail_loaded)
}

/// All settlement routes use this, including startup/transport failures and
/// local Stop. Provider success without a review result is deliberately failed.
pub(super) fn settle_review(session: &mut AgentSession, status: TurnStatus) {
    let Some(review) = session.review.as_mut() else {
        return;
    };
    review.status = match status {
        TurnStatus::Completed if review.result_message_id.is_some() => IssueReviewStatus::Completed,
        TurnStatus::Interrupted => IssueReviewStatus::Interrupted,
        _ => IssueReviewStatus::Failed,
    };
    touch_review(session);
}

pub(super) fn begin_review(session: &mut AgentSession) {
    if let Some(review) = session.review.as_mut() {
        review.status = IssueReviewStatus::Running;
        review.summary = None;
        review.result_message_id = None;
        review.fix_session_id = None;
        touch_review(session);
    }
}

pub(super) fn record_review_result(session: &mut AgentSession, text: String) -> bool {
    if text.trim().is_empty()
        || session.active_turn_id().is_none()
        || session.review.as_ref().is_none_or(|review| {
            review.status != IssueReviewStatus::Running || review.result_message_id.is_some()
        })
    {
        return false;
    }
    let summary = text
        .chars()
        .take(400)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let id = session.push_message(MessageRole::Assistant, text);
    let review = session.review.as_mut().expect("checked review");
    review.summary = Some(summary);
    review.result_message_id = Some(id);
    // The adapter only emits this after successful provider settlement.
    review.status = IssueReviewStatus::Completed;
    touch_review(session);
    true
}

impl Waku {
    fn set_issue_review_scope(&mut self, scope: ReviewScope, cx: &mut Context<Self>) {
        if let Some(detail) = self.task_detail.as_mut() {
            detail.review_scope = scope;
            // Different target kinds have different reference semantics.
            detail
                .review_reference
                .update(cx, |input, cx| input.clear(cx));
            detail.invalidate_review_layout();
        }
        cx.notify();
    }

    fn start_issue_review(&mut self, task_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.task_detail.as_ref().and_then(|detail| {
            detail
                .review_scope
                .target(detail.review_reference.read(cx).content())
        }) else {
            return;
        };
        let Some(task) = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = review_workspace(&task) else {
            self.show_toast(tr!("review.workspace_required"));
            cx.notify();
            return;
        };
        let Some(project_id) = task_details::task_project_id(&task)
            .filter(|id| self.state.projects.iter().any(|project| project.id == *id))
        else {
            return;
        };
        let draft_id = self
            .state
            .sessions
            .iter()
            .find(|session| {
                session.task_id == Some(task_id)
                    && session.project_id == project_id
                    && session.workspace == workspace
                    && !session.has_started()
                    && session
                        .review
                        .as_ref()
                        .is_some_and(|review| review.target == target)
            })
            .map(|session| session.id);
        let id = if let Some(id) = draft_id {
            id
        } else {
            let source = review_source(&task, &self.state.sessions, &workspace);
            if !review_source_ready(source) {
                let id = source.expect("unloaded source exists").id;
                self.ensure_session_loaded(id, cx);
                self.show_toast(tr!("review.loading_context"));
                cx.notify();
                return;
            }
            let context = review_context(&task, source);
            let source_session_id = source.map(|source| source.id);
            let mut session = self.state.new_session(project_id, ProviderKind::Codex);
            session.auto_title = Some(tr!("review.session_title", title = task.title));
            session.task_id = Some(task_id);
            session.workspace = workspace;
            session.runtime_mode = RuntimeMode::Ask;
            session.review = Some(SessionReview {
                target,
                source_session_id,
                status: IssueReviewStatus::Draft,
                summary: None,
                result_message_id: None,
                fix_session_id: None,
            });
            let id = session.id;
            self.composer_drafts.set(
                crate::persistence::ComposerDraftKey::for_session(&session),
                crate::persistence::ComposerDraft {
                    text: context,
                    ..Default::default()
                },
            );
            self.state.push_session(session);
            id
        };
        self.reopen_task_for_continued_work(task_id, cx);
        self.select_session(id, cx);
        self.schedule_composer_draft_save(cx);
        self.save_task_changes(cx);
        window.focus(&self.composer_focus(cx), cx);
    }

    fn fix_issue_review(&mut self, review_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == review_id)
        else {
            return;
        };
        let Some(review) = session
            .review
            .as_ref()
            .filter(|review| review.status == IssueReviewStatus::Completed)
        else {
            return;
        };
        if let Some(id) = review.fix_session_id.filter(|id| {
            self.state
                .sessions
                .iter()
                .any(|candidate| candidate.id == *id && candidate.task_id == session.task_id)
        }) {
            self.select_session(id, cx);
            window.focus(&self.composer_focus(cx), cx);
            return;
        }
        let Some(task) = self
            .state
            .tasks
            .iter()
            .find(|task| Some(task.id) == session.task_id)
            .cloned()
        else {
            return;
        };
        let Some(result) = review
            .result_message_id
            .and_then(|id| session.messages.iter().find(|message| message.id == id))
        else {
            self.ensure_session_loaded(review_id, cx);
            self.show_toast(tr!("review.loading_result"));
            cx.notify();
            return;
        };
        let prompt = format!(
            "{}\n\nAddress the actionable Codex review findings below in the same workspace. Verify each finding against the code and explain any finding that does not apply. Run relevant checks.\n\nReviewed scope: {}\n\nCodex review findings:\n{}",
            task_details::task_session_prompt(&task),
            review_target_label(&review.target),
            result.content
        );
        let mut fix = self.state.new_session(session.project_id, ProviderKind::Pi);
        fix.auto_title = Some(tr!("review.fix_title", title = task.title));
        fix.task_id = Some(task.id);
        fix.workspace = session.workspace.clone();
        fix.review_correction_source_id = Some(review_id);
        fix.runtime_mode = self.state.last_runtime_mode;
        let id = fix.id;
        self.composer_drafts.set(
            crate::persistence::ComposerDraftKey::for_session(&fix),
            crate::persistence::ComposerDraft {
                text: prompt,
                ..Default::default()
            },
        );
        self.state.push_session(fix);
        if let Some(session) = self.state.session_mut(review_id) {
            session
                .review
                .as_mut()
                .expect("review exists")
                .fix_session_id = Some(id);
            touch_review(session);
        }
        self.reopen_task_for_continued_work(task.id, cx);
        self.select_session(id, cx);
        self.schedule_composer_draft_save(cx);
        self.save_task_changes(cx);
        window.focus(&self.composer_focus(cx), cx);
    }

    pub(super) fn render_issue_review(
        &mut self,
        task: &Task,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(detail) = self.task_detail.as_ref() else {
            return div().into_any_element();
        };
        let scope = detail.review_scope;
        let reference = detail.review_reference.clone();
        let valid_target = scope.target(reference.read(cx).content()).is_some();
        let workspace = review_workspace(task);
        let workspace_available = workspace.is_some();
        let context_ready = detail.review_view.context_ready;
        let can_start = workspace_available
            && task_details::task_project_id(task)
                .is_some_and(|id| self.state.projects.iter().any(|project| project.id == id));
        let latest = detail
            .review_view
            .latest
            .and_then(|(id, index)| {
                self.state
                    .sessions
                    .get(index)
                    .filter(|session| session.id == id)
            })
            .and_then(|session| {
                session
                    .review
                    .clone()
                    .map(|review| (session.id, review, session.detail_loaded))
            });
        let theme = Theme::current(cx);
        let handle = self.menu_handle("issue-review-scope", cx);
        let weak = cx.entity().downgrade();
        let picker = dropdown_menu(
            MenuChip::new("issue-review-scope")
                .label(scope.label())
                .caret(true)
                .selected(handle.is_open()),
            "issue-review-scope-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |_: &mut App| {
                [
                    ReviewScope::Uncommitted,
                    ReviewScope::BaseBranch,
                    ReviewScope::Commit,
                    ReviewScope::CommitRange,
                ]
                .into_iter()
                .map(|value| {
                    let weak = weak.clone();
                    MenuItem::new(value.label(), move |_, cx| {
                        let _ = weak.update(cx, |this, cx| this.set_issue_review_scope(value, cx));
                    })
                    .semantic_id(format!("issue-review-scope-{value:?}"))
                })
                .collect()
            },
        );
        let task_id = task.id;
        let start = self.task_icon_action(
            "issue-start-review",
            tr!("review.start"),
            "icons/file-diff.svg",
            can_start && context_ready && valid_target && !task.title.trim().is_empty(),
            move |this, window, cx| this.start_issue_review(task_id, window, cx),
            cx,
        );
        let mut section = div()
            // Match title/spec text and conversation content, not field borders.
            .px(px(11.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("review.section")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    // Preserve the chip hitbox while aligning its visible label.
                    .child(div().ml(px(-7.0)).child(picker))
                    .when(scope != ReviewScope::Uncommitted, |row| {
                        row.child(
                            div()
                                .w(px(220.0))
                                .h(px(30.0))
                                .px(px(8.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(theme.border)
                                .text_size(sp(12.0))
                                .child(reference),
                        )
                    })
                    .child(div().flex_1())
                    .child(start),
            )
            .when(workspace_available && !context_ready, |section| {
                section.child(
                    div()
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("review.loading_context")),
                )
            })
            .when(!workspace_available, |section| {
                section.child(
                    div()
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("review.workspace_required")),
                )
            });
        if let Some((id, review, loaded)) = latest {
            let open = self.task_icon_action(
                "issue-open-review",
                tr!("review.open"),
                "icons/arrow-up-right.svg",
                true,
                move |this, window, cx| {
                    this.select_session(id, cx);
                    window.focus(&this.composer_focus(cx), cx);
                },
                cx,
            );
            let fix = self.task_icon_action(
                "issue-fix-review",
                tr!("review.fix_pi"),
                "icons/wrench.svg",
                loaded
                    && review.status == IssueReviewStatus::Completed
                    && review.result_message_id.is_some(),
                move |this, window, cx| this.fix_issue_review(id, window, cx),
                cx,
            );
            section = section.child(
                div()
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(sp(12.0))
                                    .text_color(theme.text)
                                    .child(review_status_label(&review)),
                            )
                            .child(
                                div()
                                    .text_size(sp(11.5))
                                    .text_color(theme.text_tertiary)
                                    .child(review_target_label(&review.target)),
                            )
                            .child(div().flex_1())
                            .child(open)
                            .child(fix),
                    )
                    .when_some(review.summary, |section, summary| {
                        section.child(
                            div()
                                .text_size(sp(12.0))
                                .text_color(theme.text_secondary)
                                .line_clamp(3)
                                .child(summary),
                        )
                    }),
            );
        }
        section.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_scopes_are_explicit_and_require_references() {
        assert_eq!(
            ReviewScope::Uncommitted.target(""),
            Some(IssueReviewTarget::UncommittedChanges)
        );
        assert!(ReviewScope::BaseBranch.target("  ").is_none());
        assert!(ReviewScope::Commit.target("").is_none());
        assert!(ReviewScope::CommitRange.target("main...HEAD").is_none());
        assert!(ReviewScope::CommitRange.target("..HEAD").is_none());
        assert_eq!(
            ReviewScope::CommitRange.target("abc..def"),
            Some(IssueReviewTarget::CommitRange {
                base: "abc".into(),
                head: "def".into()
            })
        );
        let prompt = review_instructions(
            &IssueReviewTarget::BaseBranch {
                branch: "main".into(),
            },
            "Keep checkout idempotent",
        );
        assert!(prompt.contains("merge base"));
        assert!(prompt.contains("Keep checkout idempotent"));
        assert!(prompt.contains("do not edit files"));
    }

    #[test]
    fn review_reuses_concrete_workspace_and_only_matching_pi_context() {
        let project = Uuid::new_v4();
        let mut task = Task::new("Checkout", TaskState::Doing, 0);
        task.workspace = Some(TaskWorkspace::NewWorktree {
            project_id: project,
        });
        assert!(review_workspace(&task).is_none());
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id: project,
            path: "/work/checkout".into(),
            branch: "checkout".into(),
        });
        let workspace = review_workspace(&task).unwrap();
        let mut source = AgentSession::new(project, ProviderKind::Pi);
        source.task_id = Some(task.id);
        source.workspace = workspace.clone();
        source.begin_turn("Develop");
        source.push_message(MessageRole::Assistant, "Implemented checkout");
        let sessions = vec![source];
        let source = review_source(&task, &sessions, &workspace).unwrap();
        assert!(review_context(&task, Some(source)).contains("Implemented checkout"));
        assert!(review_source(&task, &sessions, &SessionWorkspace::Local).is_none());
    }

    #[test]
    fn current_review_order_agrees_for_newer_and_rerun_conversations() {
        let mut task = Task::new("Checkout", TaskState::Doing, 0);
        let project = Uuid::new_v4();
        task.workspace = Some(TaskWorkspace::Local {
            project_id: project,
        });
        let mut first = AgentSession::new(project, ProviderKind::Codex);
        first.task_id = Some(task.id);
        first.created_at = 10;
        first.last_reply_at = Some(30);
        first.review = Some(SessionReview {
            target: IssueReviewTarget::UncommittedChanges,
            source_session_id: None,
            status: IssueReviewStatus::Completed,
            summary: None,
            result_message_id: None,
            fix_session_id: None,
        });
        let mut second = first.clone();
        second.id = Uuid::new_v4();
        second.created_at = 20;
        second.last_reply_at = Some(25);
        second.review.as_mut().unwrap().status = IssueReviewStatus::Running;
        let mut sessions = vec![first, second];
        let indices = vec![(sessions[0].id, 0, 0), (sessions[1].id, 1, 0)];
        assert_eq!(
            issue_review_view(&task, &indices, &sessions)
                .latest
                .unwrap()
                .0,
            sessions
                .iter()
                .max_by_key(|session| review_order(session))
                .unwrap()
                .id
        );
        assert_eq!(
            issue_review_view(&task, &indices, &sessions)
                .latest
                .unwrap()
                .0,
            sessions[0].id
        );
        sessions[1].last_reply_at = Some(40);
        assert_eq!(
            issue_review_view(&task, &indices, &sessions)
                .latest
                .unwrap()
                .0,
            sessions[1].id
        );
    }

    #[test]
    fn provisional_handoff_titles_allow_provider_names_and_user_overrides() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.auto_title = Some("Fix review: Checkout".into());
        assert_eq!(session.display_title(), "Fix review: Checkout");
        session.set_auto_title(Some("Preserve carts during retries".into()));
        assert_eq!(session.display_title(), "Preserve carts during retries");
        session.set_title("My checkout fix");
        session.set_auto_title(Some("Another provider title".into()));
        assert_eq!(session.display_title(), "My checkout fix");
    }

    #[test]
    fn cold_development_context_blocks_review_until_hydrated() {
        let mut source = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        source.begin_turn("Implement checkout");
        source.push_message(MessageRole::Assistant, "Preserve existing carts");
        assert!(!review_source_ready(Some(&source.list_projection())));
        assert!(review_source_ready(Some(&source)));
        assert!(review_source_ready(None));
    }

    #[test]
    fn same_second_catalog_cannot_erase_the_fix_link_or_review_result() {
        let mut local = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        local.review = Some(SessionReview {
            target: IssueReviewTarget::UncommittedChanges,
            source_session_id: None,
            status: IssueReviewStatus::Completed,
            summary: Some("Review result".into()),
            result_message_id: Some(Uuid::new_v4()),
            fix_session_id: None,
        });
        local.updated_at = unix_time().saturating_add(60);
        let stale = local.list_projection();
        let fix = Uuid::new_v4();
        local.review.as_mut().unwrap().fix_session_id = Some(fix);
        touch_review(&mut local);
        let revision = local.updated_at;
        let mut sessions = vec![local];
        runtime::merge_remote_session_catalog(&mut sessions, vec![stale.clone()], |_| false);
        assert_eq!(
            sessions[0].review.as_ref().unwrap().fix_session_id,
            Some(fix)
        );
        assert_eq!(sessions[0].updated_at, revision);
        runtime::replace_hydrated_session(&mut sessions[0], stale);
        assert_eq!(
            sessions[0].review.as_ref().unwrap().fix_session_id,
            Some(fix)
        );
    }

    #[test]
    fn stale_catalog_cannot_restore_a_review_result_removed_by_rollback() {
        let mut local = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        local.begin_turn("Review the issue changes");
        let result = local.push_message(MessageRole::Assistant, "Review findings");
        local.finish_active_turn(TurnStatus::Completed);
        local.review = Some(SessionReview {
            target: IssueReviewTarget::UncommittedChanges,
            source_session_id: None,
            status: IssueReviewStatus::Completed,
            summary: Some("Review findings".into()),
            result_message_id: Some(result),
            fix_session_id: Some(Uuid::new_v4()),
        });
        local.updated_at = unix_time().saturating_add(60);
        let stale = local.list_projection();
        local.truncate_after_turn(0);
        let mut sessions = vec![local];
        runtime::merge_remote_session_catalog(&mut sessions, vec![stale], |_| false);
        let review = sessions[0].review.as_ref().unwrap();
        assert_eq!(review.status, IssueReviewStatus::Interrupted);
        assert!(review.result_message_id.is_none());
        assert!(review.summary.is_none());
        assert!(review.fix_session_id.is_none());
        assert!(sessions[0].messages.is_empty());
    }

    #[test]
    fn review_completion_requires_actual_result_and_deduplicates_replay() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.review = Some(SessionReview {
            target: IssueReviewTarget::UncommittedChanges,
            source_session_id: None,
            status: IssueReviewStatus::Draft,
            summary: None,
            result_message_id: None,
            fix_session_id: None,
        });
        session.begin_turn("Review");
        begin_review(&mut session);
        settle_review(&mut session, TurnStatus::Completed);
        assert_eq!(
            session.review.as_ref().unwrap().status,
            IssueReviewStatus::Failed
        );
        begin_review(&mut session);
        assert!(record_review_result(
            &mut session,
            "[P1] Preserve existing carts".into()
        ));
        assert!(!record_review_result(&mut session, "duplicate".into()));
        settle_review(&mut session, TurnStatus::Completed);
        assert_eq!(
            session.review.as_ref().unwrap().status,
            IssueReviewStatus::Completed
        );
        assert_eq!(session.messages.len(), 2);
        begin_review(&mut session);
        settle_review(&mut session, TurnStatus::Interrupted);
        assert_eq!(
            session.review.as_ref().unwrap().status,
            IssueReviewStatus::Interrupted
        );
        assert!(session.review.as_ref().unwrap().result_message_id.is_none());
    }
}
