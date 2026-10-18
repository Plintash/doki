//! Task completion waits for running and detached work to settle.

use super::board::{TaskCompletionConfirm, focus_overlay_control};
use super::*;

/// One completion attempt owns its absence evidence; it is never a general
/// cache of daemon liveness and is consumed when that attempt begins stopping.
#[derive(Default)]
struct CompletionRuntimeEvidence {
    attempt: Uuid,
    sessions: HashSet<Uuid>,
    pending: HashSet<Uuid>,
    absent: HashSet<Uuid>,
}

impl CompletionRuntimeEvidence {
    fn new(sessions: HashSet<Uuid>, attached: &HashSet<Uuid>) -> Self {
        let pending = sessions.difference(attached).copied().collect();
        Self {
            attempt: Uuid::new_v4(),
            sessions,
            pending,
            absent: HashSet::new(),
        }
    }

    fn record(&mut self, attempt: Uuid, session_id: Uuid, absent: bool) -> bool {
        if self.attempt != attempt || !self.pending.remove(&session_id) {
            return false;
        }
        if absent {
            self.absent.insert(session_id);
        }
        self.pending.is_empty()
    }

    fn ready_for(&self, sessions: &HashSet<Uuid>) -> bool {
        self.pending.is_empty() && self.sessions == *sessions
    }

    fn controls_known(&self, attached: impl Fn(&Uuid) -> bool) -> bool {
        self.sessions
            .iter()
            .all(|id| attached(id) || self.absent.contains(id))
    }
}

pub(super) struct TaskCompletionPreflight {
    evidence: CompletionRuntimeEvidence,
    window: gpui::AnyWindowHandle,
}

#[derive(Default)]
pub(super) struct TaskCompletionStop {
    pub(super) dirty: bool,
    pub(super) failed: bool,
    attempt: Uuid,
    runtime_drivers: HashMap<Uuid, (Uuid, DriverHandle)>,
    closing: bool,
    pending_closes: HashSet<Uuid>,
}

impl TaskCompletionStop {
    fn acknowledge_close(&mut self, attempt: Uuid, session_id: Uuid, success: bool) {
        if self.attempt != attempt || !self.pending_closes.remove(&session_id) {
            return;
        }
        self.failed |= !success;
        self.dirty = true;
    }

    fn runtime_state(&self) -> BackgroundCompletionState {
        if self.failed {
            BackgroundCompletionState::Failed
        } else if !self.pending_closes.is_empty()
            || (!self.closing && !self.runtime_drivers.is_empty())
        {
            BackgroundCompletionState::Stopping
        } else {
            BackgroundCompletionState::Settled
        }
    }
}

/// Placement snapshots deliberately exclude editable issue content.
#[derive(Clone, Copy)]
struct TaskPlacement {
    project: Option<Uuid>,
    state: TaskState,
    position: i64,
    stage_id: Option<Uuid>,
    completed_at: Option<u64>,
}
impl TaskPlacement {
    fn of(task: &Task) -> Self {
        Self {
            project: task.project_id(),
            state: task.state,
            position: task.position,
            stage_id: task.stage_id,
            completed_at: task.completed_at,
        }
    }
    fn restore(self, task: &mut Task) {
        task.state = self.state;
        task.position = self.position;
        task.stage_id = self.stage_id;
        task.completed_at = self.completed_at;
        task.touch();
    }
}

/// One save attempt retains both its acknowledged baseline and sent placement.
#[derive(Clone, Copy)]
pub(super) struct TaskCompletionSaveRollback {
    attempt: Uuid,
    before: TaskPlacement,
    after: TaskPlacement,
}
impl TaskCompletionSaveRollback {
    fn begin(task: &mut Task) -> Self {
        let before = TaskPlacement::of(task);
        task.set_state(TaskState::Done);
        Self {
            attempt: Uuid::new_v4(),
            before,
            after: TaskPlacement::of(task),
        }
    }
    pub(super) fn move_to(
        task: &mut Task,
        state: TaskState,
        stage: Option<&crate::model::BoardStage>,
        position: i64,
    ) -> Self {
        let before = TaskPlacement::of(task);
        if let Some(stage) = stage {
            task.set_stage(stage, position);
        } else {
            task.set_state(state);
            task.stage_id = task
                .project_id()
                .map(|id| crate::model::canonical_stage_id(id, state));
            task.position = position;
            task.touch();
        }
        Self {
            attempt: Uuid::new_v4(),
            before,
            after: TaskPlacement::of(task),
        }
    }
    fn apply(&self, attempt: Uuid, task: &mut Task) -> bool {
        if self.attempt != attempt
            || task.project_id() != self.after.project
            || task.state != self.after.state
            || task.stage_id != self.after.stage_id
        {
            return false;
        }
        self.before.restore(task);
        true
    }
    fn rebase(&mut self, previous: Self, saved: bool) {
        // A workspace reassignment owns its own destination; never restore a
        // previous project's stage into that new owner.
        if self.before.project == previous.after.project {
            self.before = if saved {
                previous.after
            } else {
                previous.before
            };
        }
    }
}

fn register_placement_save(
    pending: &mut HashMap<Uuid, TaskCompletionSaveRollback>,
    task_id: Uuid,
    mut next: TaskCompletionSaveRollback,
) {
    if let Some(previous) = pending.get(&task_id) {
        next.rebase(*previous, false);
    }
    pending.insert(task_id, next);
}

fn reconcile_placement_saves(
    pending: &mut HashMap<Uuid, TaskCompletionSaveRollback>,
    local: &[Task],
    remote: &[Task],
) {
    for task in remote {
        if local
            .iter()
            .find(|local| local.id == task.id)
            .is_none_or(|local| task.updated_at >= local.updated_at)
        {
            pending.remove(&task.id);
        }
    }
}

fn settle_placement_saves(
    pending: &mut HashMap<Uuid, TaskCompletionSaveRollback>,
    tasks: &mut [Task],
    attempts: &[(Uuid, TaskCompletionSaveRollback)],
    saved: bool,
) -> bool {
    let mut changed = false;
    for (task_id, sent) in attempts {
        let Some(current) = pending.get_mut(task_id) else {
            continue;
        };
        if current.attempt != sent.attempt {
            // Later moves are coalesced but not sent yet. Their rollback must
            // follow what this completed save actually acknowledged.
            current.rebase(*sent, saved);
            continue;
        }
        let rollback = pending.remove(task_id).unwrap();
        if !saved && let Some(task) = tasks.iter_mut().find(|task| task.id == *task_id) {
            changed |= rollback.apply(sent.attempt, task);
        }
    }
    changed
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BackgroundCompletionState {
    Settled,
    Stopping,
    Failed,
}

fn completion_can_finish(
    states: impl IntoIterator<Item = BackgroundCompletionState>,
) -> BackgroundCompletionState {
    states
        .into_iter()
        .fold(BackgroundCompletionState::Settled, |current, next| {
            match (current, next) {
                (BackgroundCompletionState::Failed, _) | (_, BackgroundCompletionState::Failed) => {
                    BackgroundCompletionState::Failed
                }
                (BackgroundCompletionState::Stopping, _)
                | (_, BackgroundCompletionState::Stopping) => BackgroundCompletionState::Stopping,
                _ => BackgroundCompletionState::Settled,
            }
        })
}

/// An idle snapshot and an empty event queue do not establish that a
/// connected runtime is idle: attachment/reconnect replay may still be in flight.
fn completion_requires_stop_confirmation(
    status: SessionStatus,
    has_background_work: bool,
    has_runtime: bool,
) -> bool {
    status.is_busy() || has_background_work || has_runtime
}

impl Waku {
    /// The dialog count and decision share the same live in-memory evidence.
    pub(super) fn task_completion_stop_count(&self, task_id: Uuid) -> usize {
        self.state
            .sessions
            .iter()
            .filter(|session| {
                session.task_id == Some(task_id)
                    && completion_requires_stop_confirmation(
                        session.status,
                        self.session_has_live_background_work(session.id),
                        self.runtimes.contains_key(&session.id),
                    )
            })
            .count()
    }

    /// Every path to Done shares the same stop/acknowledgment boundary.
    pub(super) fn move_task(
        &mut self,
        task_id: Uuid,
        state: TaskState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_task_to_stage(task_id, state, None, window, cx);
    }

    pub(super) fn move_task_to_stage(
        &mut self,
        task_id: Uuid,
        state: TaskState,
        stage: Option<crate::model::BoardStage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.state.tasks.iter().find(|task| task.id == task_id) else {
            return;
        };
        let current = task.state;
        if state.is_done() {
            if current != state {
                self.request_task_completion(task_id, window, cx);
            }
            return;
        }
        let target_id = stage.as_ref().map(|s| s.id).or_else(|| {
            task_details::task_project_id(task)
                .map(|project| crate::model::canonical_stage_id(project, state))
        });
        let unchanged = current == state && task.stage_id == target_id;
        self.task_completion_preflights.remove(&task_id);
        if self
            .pending_task_completion
            .as_ref()
            .is_some_and(|pending| pending.task_id == task_id)
        {
            self.pending_task_completion = None;
        }
        // Cancels the archive intent; already sent stops remain valid.
        let cancelled = self.task_completion_stops.remove(&task_id).is_some();
        if unchanged {
            if cancelled {
                cx.notify();
            }
            return;
        }
        let position = self.next_task_position(state);
        let task = self
            .state
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .unwrap();
        let rollback = TaskCompletionSaveRollback::move_to(task, state, stage.as_ref(), position);
        self.register_task_placement_save(task_id, rollback);
        self.sidebar_rows_fingerprint.set(None);
        self.save_task_changes(cx);
        cx.notify();
    }

    fn task_completion_session_ids(&self, task_id: Uuid) -> HashSet<Uuid> {
        self.state
            .sessions
            .iter()
            .filter(|session| session.task_id == Some(task_id))
            .map(|session| session.id)
            .collect()
    }

    fn begin_task_completion_preflight(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .task_completion_preflights
            .get(&task_id)
            .is_some_and(|preflight| !preflight.evidence.pending.is_empty())
        {
            return true;
        }
        self.task_completion_preflights.remove(&task_id);
        let sessions = self.task_completion_session_ids(task_id);
        let attached = self.runtimes.keys().copied().collect();
        let evidence = CompletionRuntimeEvidence::new(sessions, &attached);
        let pending = evidence.pending.iter().copied().collect::<Vec<_>>();
        self.task_completion_preflights.insert(
            task_id,
            TaskCompletionPreflight {
                evidence,
                window: window.window_handle(),
            },
        );
        if pending.is_empty() {
            return false;
        }
        for session_id in pending {
            self.start_runtime_attachment(session_id, cx);
        }
        self.show_toast(tr!("board.completion_waiting_for_runtime"));
        cx.notify();
        true
    }

    pub(super) fn task_completion_attachment_attempts(
        &self,
        session_id: Uuid,
    ) -> Vec<(Uuid, Uuid)> {
        self.task_completion_preflights
            .iter()
            .filter_map(|(id, preflight)| {
                preflight
                    .evidence
                    .pending
                    .contains(&session_id)
                    .then_some((*id, preflight.evidence.attempt))
            })
            .collect()
    }

    /// A preflight can join a normal attachment already in flight, or replace
    /// a canceled attempt. Old None evidence cannot settle the newer attempt.
    pub(super) fn resume_task_completion_attachment(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let attempts = self.task_completion_attachment_attempts(session_id);
        if attempts.is_empty() {
            return;
        }
        if self.runtimes.contains_key(&session_id) {
            // An installed control is current local evidence, independent of
            // the old request's liveness snapshot.
            self.note_task_completion_attachment(session_id, Some(false), &attempts, cx);
        } else {
            self.start_runtime_attachment(session_id, cx);
        }
    }

    pub(super) fn task_completion_waits_for_attachment(&self, session_id: Uuid) -> bool {
        self.task_completion_preflights
            .values()
            .any(|preflight| preflight.evidence.pending.contains(&session_id))
    }

    pub(super) fn note_task_completion_attachment(
        &mut self,
        session_id: Uuid,
        absent: Option<bool>,
        attempts: &[(Uuid, Uuid)],
        cx: &mut Context<Self>,
    ) {
        for &(task_id, attempt) in attempts {
            if self
                .task_completion_preflights
                .get(&task_id)
                .is_none_or(|preflight| preflight.evidence.attempt != attempt)
            {
                continue;
            }
            if absent.is_none() || self.task_has_unstoppable_work(task_id) {
                self.task_completion_preflights.remove(&task_id);
                self.show_toast(tr!("board.completion_stop_failed"));
                cx.notify();
                continue;
            }
            let Some(preflight) = self.task_completion_preflights.get_mut(&task_id) else {
                continue;
            };
            if !preflight
                .evidence
                .record(attempt, session_id, absent.unwrap())
            {
                continue;
            }
            let attempt = preflight.evidence.attempt;
            let window = preflight.window;
            cx.spawn(async move |waku, cx| {
                let updated = window.update(cx, |_, window, cx| {
                    let _ = waku.update(cx, |this, cx| {
                        let sessions = this.task_completion_session_ids(task_id);
                        let current = this
                            .task_completion_preflights
                            .get(&task_id)
                            .is_some_and(|preflight| preflight.evidence.attempt == attempt);
                        if !current {
                            return;
                        }
                        let valid = this
                            .task_completion_preflights
                            .get(&task_id)
                            .is_some_and(|preflight| preflight.evidence.ready_for(&sessions));
                        if !valid || this.task_has_unstoppable_work(task_id) {
                            this.task_completion_preflights.remove(&task_id);
                            this.show_toast(tr!("board.completion_stop_failed"));
                            cx.notify();
                            return;
                        }
                        // Apply events already available, without treating an
                        // empty queue as proof that attachment replay is ready.
                        this.drain_driver_events(cx);
                        this.request_task_completion_ready(task_id, window, cx);
                    });
                });
                if updated.is_err() {
                    let _ = waku.update(cx, |this, cx| {
                        if this
                            .task_completion_preflights
                            .get(&task_id)
                            .is_some_and(|preflight| preflight.evidence.attempt == attempt)
                        {
                            this.task_completion_preflights.remove(&task_id);
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    fn task_has_unstoppable_work(&self, task_id: Uuid) -> bool {
        self.state.sessions.iter().any(|session| {
            session.task_id == Some(task_id)
                && self.session_has_local_runtime_preparation(session.id)
        })
    }

    fn request_task_completion(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.task_completion_stops.contains_key(&task_id) {
            self.show_toast(tr!("board.completion_stopping"));
            cx.notify();
            return;
        }
        if self.task_has_unstoppable_work(task_id) {
            self.show_toast(tr!("board.completion_waiting_for_setup"));
            cx.notify();
            return;
        }
        if self.begin_task_completion_preflight(task_id, window, cx) {
            return;
        }
        self.drain_driver_events(cx);
        self.request_task_completion_ready(task_id, window, cx);
    }

    fn request_task_completion_ready(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .is_none_or(|task| task.state.is_done())
        {
            self.task_completion_preflights.remove(&task_id);
            return;
        }
        if self.task_has_unstoppable_work(task_id) {
            self.task_completion_preflights.remove(&task_id);
            self.show_toast(tr!("board.completion_waiting_for_setup"));
            cx.notify();
            return;
        }
        if self.task_completion_stop_count(task_id) > 0 {
            let confirm_focus = cx.focus_handle().tab_stop(true);
            let cancel_focus = cx.focus_handle().tab_stop(true);
            self.pending_task_completion = Some(TaskCompletionConfirm {
                task_id,
                confirm_focus,
                cancel_focus: cancel_focus.clone(),
            });
            focus_overlay_control(window, cx, cancel_focus, move |this| {
                this.pending_task_completion
                    .as_ref()
                    .is_some_and(|pending| pending.task_id == task_id)
            });
            cx.notify();
            return;
        }
        self.finish_task_completion(task_id, cx);
    }

    fn finish_task_completion(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        if self
            .task_completion_preflights
            .get(&task_id)
            .is_some_and(|preflight| {
                !preflight
                    .evidence
                    .ready_for(&self.task_completion_session_ids(task_id))
            })
        {
            self.task_completion_preflights.remove(&task_id);
            self.show_toast(tr!("board.completion_stop_failed"));
            cx.notify();
            return;
        }
        if self.task_has_unstoppable_work(task_id) {
            self.task_completion_preflights.remove(&task_id);
            self.show_toast(tr!("board.completion_waiting_for_setup"));
            cx.notify();
            return;
        }
        // A control can disappear while the confirmation is open. Do not
        // silently replace this intent with ordinary orphan-attachment probes.
        let controls_known =
            self.task_completion_preflights
                .get(&task_id)
                .is_some_and(|preflight| {
                    preflight
                        .evidence
                        .controls_known(|session_id| self.runtimes.contains_key(session_id))
                });
        if !controls_known {
            self.task_completion_preflights.remove(&task_id);
            self.show_toast(tr!("board.completion_stop_failed"));
            cx.notify();
            return;
        }
        let sessions = self
            .state
            .sessions
            .iter()
            .filter(|session| session.task_id == Some(task_id))
            .map(|session| session.id)
            .collect::<Vec<_>>();
        // Preflight all detached work before issuing any stop. An absent control
        // channel is not evidence that a detached process has actually ended.
        let requests = sessions
            .iter()
            .map(|id| {
                self.task_background_stop_requests(*id)
                    .map(|keys| (*id, keys))
            })
            .collect::<Option<Vec<_>>>();
        let Some(requests) = requests else {
            self.show_toast(tr!("board.completion_cannot_stop"));
            cx.notify();
            return;
        };
        // Keep the exact controls even if normal foreground cleanup removes
        // a runtime. Only explicit shutdown acknowledgments release this gate.
        let mut runtime_drivers = self
            .unconfirmed_task_closes
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        for id in &sessions {
            if let Some(runtime) = self.runtimes.get(id) {
                if !runtime_drivers.values().any(|(session_id, driver)| {
                    session_id == id && driver.same_runtime(&runtime.driver)
                }) {
                    runtime_drivers.insert(Uuid::new_v4(), (*id, runtime.driver.clone()));
                }
            }
        }
        self.unconfirmed_task_closes
            .insert(task_id, runtime_drivers.clone());
        self.task_completion_preflights.remove(&task_id);
        self.task_completion_stops.insert(
            task_id,
            TaskCompletionStop {
                dirty: true,
                attempt: Uuid::new_v4(),
                runtime_drivers,
                ..TaskCompletionStop::default()
            },
        );
        for (session_id, keys) in requests {
            for key in keys {
                self.stop_background_work(session_id, key, cx);
            }
        }
        for session_id in sessions {
            if self
                .state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_some_and(|session| session.status.is_busy())
            {
                self.stop_session_turn_for_completion(session_id, cx);
            }
        }
        self.reconcile_task_completions(cx);
        if self.task_completion_stops.contains_key(&task_id) {
            self.show_toast(tr!("board.completion_stopping"));
            cx.notify();
        }
    }

    /// Called while applying provider events. The actual transition is deferred
    /// to the event-pump boundary, when all temporarily removed runtimes are back.
    pub(super) fn note_task_completion_background_event(&mut self, session_id: Uuid, failed: bool) {
        if self.task_completion_stops.is_empty() {
            return;
        }
        let task_id = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.task_id);
        if let Some(progress) = task_id.and_then(|id| self.task_completion_stops.get_mut(&id)) {
            progress.dirty = true;
            progress.failed |= failed;
        }
    }

    pub(super) fn reconcile_task_completions(&mut self, cx: &mut Context<Self>) {
        let dirty = self
            .task_completion_stops
            .iter_mut()
            .filter_map(|(id, progress)| {
                std::mem::take(&mut progress.dirty).then_some((*id, progress.failed))
            })
            .collect::<Vec<_>>();
        for (task_id, failed) in dirty {
            let sessions = self
                .state
                .sessions
                .iter()
                .filter(|session| session.task_id == Some(task_id))
                .map(|session| session.id)
                .collect::<Vec<_>>();
            let outcome = if failed || self.task_has_unstoppable_work(task_id) {
                BackgroundCompletionState::Failed
            } else {
                completion_can_finish(
                    sessions
                        .iter()
                        .map(|id| self.session_background_completion_state(*id)),
                )
            };
            match outcome {
                BackgroundCompletionState::Stopping => {}
                BackgroundCompletionState::Failed => {
                    self.task_completion_stops.remove(&task_id);
                    self.show_toast(tr!("board.completion_stop_failed"));
                    cx.notify();
                }
                BackgroundCompletionState::Settled => {
                    let Some(progress) = self.task_completion_stops.get_mut(&task_id) else {
                        continue;
                    };
                    // A replacement belongs to newer work. Never close it or
                    // let an acknowledgment for its predecessor archive it.
                    if sessions.iter().any(|id| {
                        self.runtimes.get(id).is_some_and(|runtime| {
                            !progress
                                .runtime_drivers
                                .values()
                                .any(|(session_id, driver)| {
                                    session_id == id && runtime.driver.same_runtime(driver)
                                })
                        })
                    }) {
                        self.task_completion_stops.remove(&task_id);
                        self.show_toast(tr!("board.completion_stop_failed"));
                        cx.notify();
                        continue;
                    }
                    if !progress.closing {
                        progress.closing = true;
                        progress.pending_closes =
                            progress.runtime_drivers.keys().copied().collect();
                        let attempt = progress.attempt;
                        for (control_id, (_, driver)) in progress.runtime_drivers.clone() {
                            cx.spawn(async move |waku, cx| {
                                let (driver, result) = cx
                                    .background_executor()
                                    .spawn(async move {
                                        let result = driver.close_and_wait();
                                        (driver, result)
                                    })
                                    .await;
                                let _ = waku.update(cx, |this, cx| {
                                    if result.is_ok() {
                                        if let Some(unconfirmed) =
                                            this.unconfirmed_task_closes.get_mut(&task_id)
                                        {
                                            if unconfirmed.get(&control_id).is_some_and(
                                                |(_, current)| current.same_runtime(&driver),
                                            ) {
                                                unconfirmed.remove(&control_id);
                                            }
                                            if unconfirmed.is_empty() {
                                                this.unconfirmed_task_closes.remove(&task_id);
                                            }
                                        }
                                    }
                                    if let Some(progress) =
                                        this.task_completion_stops.get_mut(&task_id)
                                    {
                                        progress.acknowledge_close(
                                            attempt,
                                            control_id,
                                            result.is_ok(),
                                        );
                                    }
                                    this.reconcile_task_completions(cx);
                                });
                            })
                            .detach();
                        }
                    }
                    if progress.runtime_state() != BackgroundCompletionState::Settled {
                        continue;
                    }
                    // Remove the intent before stopping any provider wake that
                    // arrived along with its final background-work event.
                    self.task_completion_stops.remove(&task_id);
                    for session_id in &sessions {
                        if self
                            .state
                            .sessions
                            .iter()
                            .find(|session| session.id == *session_id)
                            .is_some_and(|session| session.status.is_busy())
                        {
                            self.stop_session_turn(*session_id, cx);
                        }
                    }
                    // Stopping a wake drains queued provider events. Recheck in
                    // case those events revealed another detached process.
                    if sessions
                        .iter()
                        .any(|id| self.session_has_live_background_work(*id))
                    {
                        self.show_toast(tr!("board.completion_stop_failed"));
                        cx.notify();
                        continue;
                    }
                    for session_id in sessions {
                        // No live detached work remains; closing now prevents an
                        // automatic provider wake from restarting an archived issue.
                        self.reset_session_runtime(session_id);
                    }
                    let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id)
                    else {
                        continue;
                    };
                    let rollback = TaskCompletionSaveRollback::begin(task);
                    self.register_task_placement_save(task_id, rollback);
                    self.unconfirmed_task_closes.remove(&task_id);
                    self.hide_toast();
                    self.sidebar_rows_fingerprint.set(None);
                    self.save_task_changes(cx);
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn register_task_placement_save(
        &mut self,
        task_id: Uuid,
        rollback: TaskCompletionSaveRollback,
    ) {
        register_placement_save(&mut self.task_completion_save_rollbacks, task_id, rollback);
    }

    pub(super) fn reconcile_task_placement_saves(&mut self, remote: &[Task]) {
        reconcile_placement_saves(
            &mut self.task_completion_save_rollbacks,
            &self.state.tasks,
            remote,
        );
    }

    pub(super) fn task_completion_save_attempts(&self) -> Vec<(Uuid, TaskCompletionSaveRollback)> {
        self.task_completion_save_rollbacks
            .iter()
            .map(|(id, rollback)| (*id, *rollback))
            .collect()
    }

    pub(super) fn settle_task_completion_save(
        &mut self,
        attempts: &[(Uuid, TaskCompletionSaveRollback)],
        saved: bool,
    ) {
        if settle_placement_saves(
            &mut self.task_completion_save_rollbacks,
            &mut self.state.tasks,
            attempts,
            saved,
        ) {
            super::rpa::daemon::model_changed();
            self.sidebar_rows_fingerprint.set(None);
        }
    }

    pub(super) fn confirm_task_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_task_completion.take() else {
            return;
        };
        let focus = self.board_dialog_focus(cx);
        window.focus(&focus, cx);
        self.finish_task_completion(pending.task_id, cx);
    }

    /// Cancel only the archive intent; already-sent process stops remain valid.
    pub(super) fn cancel_task_completion_stop(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        if self.task_completion_stops.remove(&task_id).is_some() {
            self.hide_toast();
            cx.notify();
        }
    }

    pub(super) fn cancel_task_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_task_completion.take() else {
            return;
        };
        self.task_completion_preflights.remove(&pending.task_id);
        let focus = self.board_dialog_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundCompletionState::*, CompletionRuntimeEvidence, TaskCompletionSaveRollback,
        TaskCompletionStop, completion_can_finish, completion_requires_stop_confirmation,
    };
    use crate::model::{AgentSession, ProviderKind, SessionStatus, Task, TaskState};
    use std::collections::HashSet;
    use uuid::Uuid;

    #[test]
    fn moves_coalesced_behind_an_unrelated_save_keep_the_first_baseline() {
        let project = uuid::Uuid::new_v4();
        let board = crate::model::ProjectBoard::canonical(project);
        let mut task = Task::new("Issue", TaskState::Doing, 7);
        task.workspace = Some(crate::model::TaskWorkspace::Local {
            project_id: project,
        });
        task.stage_id = Some(board.stages[1].id);
        let id = task.id;
        let mut pending = std::collections::HashMap::new();
        for position in [3, 2, 1] {
            let mut stage = board.stages[1].clone();
            stage.id = uuid::Uuid::new_v4();
            let next = TaskCompletionSaveRollback::move_to(
                &mut task,
                TaskState::Doing,
                Some(&stage),
                position,
            );
            super::register_placement_save(&mut pending, id, next);
        }
        let sent = pending[&id];
        let mut tasks = vec![task];
        super::settle_placement_saves(&mut pending, &mut tasks, &[(id, sent)], false);
        assert_eq!(tasks[0].stage_id, Some(board.stages[1].id));
        assert_eq!(tasks[0].position, 7);
    }

    #[test]
    fn authoritative_newer_catalog_cannot_be_overwritten_by_failed_local_save() {
        let project = uuid::Uuid::new_v4();
        let board = crate::model::ProjectBoard::canonical(project);
        let mut task = Task::new("Issue", TaskState::Backlog, 7);
        task.workspace = Some(crate::model::TaskWorkspace::Local {
            project_id: project,
        });
        task.stage_id = Some(board.stages[0].id);
        let sent = TaskCompletionSaveRollback::move_to(
            &mut task,
            TaskState::Doing,
            Some(&board.stages[1]),
            3,
        );
        let id = task.id;
        let mut remote = task.clone();
        remote.position = 1;
        remote.title = "Remote edit".into();
        remote.touch();
        let mut pending = std::collections::HashMap::from([(id, sent)]);
        super::reconcile_placement_saves(&mut pending, &[task], &[remote.clone()]);
        let mut tasks = vec![remote];
        super::settle_placement_saves(&mut pending, &mut tasks, &[(id, sent)], false);
        assert_eq!(tasks[0].stage_id, Some(board.stages[1].id));
        assert_eq!(tasks[0].position, 1);
        assert_eq!(tasks[0].title, "Remote edit");
    }

    #[test]
    fn coalesced_moves_roll_back_to_the_last_acknowledged_placement() {
        for (first_saved, second_saved) in [(false, false), (true, false), (false, true)] {
            let project = uuid::Uuid::new_v4();
            let board = crate::model::ProjectBoard::canonical(project);
            let mut b = board.stages[1].clone();
            b.id = uuid::Uuid::new_v4();
            let mut c = b.clone();
            c.id = uuid::Uuid::new_v4();
            let mut task = Task::new("Original", TaskState::Doing, 7);
            task.workspace = Some(crate::model::TaskWorkspace::Local {
                project_id: project,
            });
            task.stage_id = Some(board.stages[1].id);
            let first =
                TaskCompletionSaveRollback::move_to(&mut task, TaskState::Doing, Some(&b), 3);
            let second =
                TaskCompletionSaveRollback::move_to(&mut task, TaskState::Doing, Some(&c), 1);
            task.title = "Edited during save".into();
            task.description = "Keep this edit too".into();
            let id = task.id;
            let mut tasks = vec![task];
            let mut pending = std::collections::HashMap::from([(id, second)]);
            super::settle_placement_saves(&mut pending, &mut tasks, &[(id, first)], first_saved);
            let second_sent = pending[&id];
            super::settle_placement_saves(
                &mut pending,
                &mut tasks,
                &[(id, second_sent)],
                second_saved,
            );
            let expected = if second_saved {
                c.id
            } else if first_saved {
                b.id
            } else {
                board.stages[1].id
            };
            assert_eq!(tasks[0].stage_id, Some(expected));
            assert_eq!(
                tasks[0].position,
                if second_saved {
                    1
                } else if first_saved {
                    3
                } else {
                    7
                }
            );
            assert_eq!(tasks[0].title, "Edited during save");
            assert_eq!(tasks[0].description, "Keep this edit too");
        }
    }

    #[test]
    fn same_lifecycle_move_and_failed_save_restore_exact_placement() {
        let project = uuid::Uuid::new_v4();
        let board = crate::model::ProjectBoard::canonical(project);
        let mut review = board.stages[1].clone();
        review.id = uuid::Uuid::new_v4();
        review.name = "Review".into();
        let mut task = Task::new("Issue", TaskState::Doing, 7);
        task.workspace = Some(crate::model::TaskWorkspace::Local {
            project_id: project,
        });
        task.stage_id = Some(board.stages[1].id);
        let revision = task.updated_at;
        let rollback =
            TaskCompletionSaveRollback::move_to(&mut task, TaskState::Doing, Some(&review), -2);
        assert_eq!(task.stage_id, Some(review.id));
        assert!(task.updated_at > revision);
        assert!(rollback.apply(rollback.attempt, &mut task));
        assert_eq!(task.stage_id, Some(board.stages[1].id));
        assert_eq!(task.position, 7);
        assert_eq!(task.completed_at, None);
        task.set_stage(&review, 3);
        TaskCompletionSaveRollback::move_to(&mut task, TaskState::Doing, None, -3);
        assert_eq!(task.stage_id, Some(board.stages[1].id));
    }

    #[test]
    fn completion_failure_restores_review_and_reopen_failure_restores_terminal_timestamp() {
        let project = uuid::Uuid::new_v4();
        let board = crate::model::ProjectBoard::canonical(project);
        let mut review = board.stages[1].clone();
        review.id = uuid::Uuid::new_v4();
        let mut task = Task::new("Issue", TaskState::Doing, 8);
        task.workspace = Some(crate::model::TaskWorkspace::Local {
            project_id: project,
        });
        task.set_stage(&review, 8);
        let rollback = TaskCompletionSaveRollback::begin(&mut task);
        assert_eq!(task.stage_id, Some(board.stages[2].id));
        assert!(rollback.apply(rollback.attempt, &mut task));
        assert_eq!(task.stage_id, Some(review.id));
        task.set_state(TaskState::Done);
        let timestamp = task.completed_at;
        let rollback =
            TaskCompletionSaveRollback::move_to(&mut task, TaskState::Doing, Some(&review), 2);
        assert_eq!(task.completed_at, None);
        assert!(rollback.apply(rollback.attempt, &mut task));
        assert_eq!(task.state, TaskState::Done);
        assert_eq!(task.stage_id, Some(board.stages[2].id));
        assert_eq!(task.completed_at, timestamp);
    }

    #[test]
    fn resident_runtime_with_idle_snapshot_requires_confirmation_before_replay() {
        assert!(completion_requires_stop_confirmation(
            SessionStatus::Idle,
            false,
            true
        ));
        // The decision remains the same when delayed running state arrives.
        assert!(completion_requires_stop_confirmation(
            SessionStatus::Working,
            false,
            true
        ));
    }

    #[test]
    fn idle_history_without_runtime_or_background_work_needs_no_confirmation() {
        assert!(!completion_requires_stop_confirmation(
            SessionStatus::Idle,
            false,
            false
        ));
    }

    #[test]
    fn busy_or_detached_work_requires_stop_confirmation() {
        assert!(completion_requires_stop_confirmation(
            SessionStatus::Working,
            false,
            false
        ));
        assert!(completion_requires_stop_confirmation(
            SessionStatus::Idle,
            true,
            false
        ));
    }

    #[test]
    fn lost_attached_control_is_not_replaced_with_absence_evidence() {
        let session = Uuid::new_v4();
        let sessions = HashSet::from([session]);
        let evidence = CompletionRuntimeEvidence::new(sessions.clone(), &sessions);
        assert!(evidence.ready_for(&sessions));
        assert!(evidence.controls_known(|id| *id == session));
        assert!(!evidence.controls_known(|_| false));
    }

    #[test]
    fn idle_catalog_session_is_included_in_completion_runtime_preflight() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.status = SessionStatus::Idle;
        let sessions = HashSet::from([session.id]);
        let evidence = CompletionRuntimeEvidence::new(sessions.clone(), &HashSet::new());
        assert!(evidence.pending.contains(&session.id));
        assert!(!evidence.ready_for(&sessions));
    }

    #[test]
    fn no_runtime_preflight_evidence_resumes_once_without_repeating_the_probe() {
        let session = Uuid::new_v4();
        let sessions = HashSet::from([session]);
        let mut evidence = CompletionRuntimeEvidence::new(sessions.clone(), &HashSet::new());
        assert!(evidence.record(evidence.attempt, session, true));
        assert!(evidence.absent.contains(&session));
        assert!(evidence.ready_for(&sessions));
        assert!(!evidence.record(evidence.attempt, session, true));
        assert!(!evidence.ready_for(&HashSet::from([session, Uuid::new_v4()])));
    }

    #[test]
    fn old_attachment_absence_cannot_settle_a_new_completion_attempt() {
        let session = Uuid::new_v4();
        let sessions = HashSet::from([session]);
        let old = CompletionRuntimeEvidence::new(sessions.clone(), &HashSet::new());
        let mut current = CompletionRuntimeEvidence::new(sessions.clone(), &HashSet::new());
        assert!(!current.record(old.attempt, session, true));
        assert!(current.pending.contains(&session));
        assert!(current.absent.is_empty());
        assert!(!current.ready_for(&sessions));
        assert!(current.record(current.attempt, session, false));
        assert!(current.ready_for(&sessions));
    }

    #[test]
    fn rejected_completion_save_restores_active_state_and_preserves_inflight_edits() {
        let mut task = Task::new("Original title", TaskState::Doing, 7);
        let rollback = TaskCompletionSaveRollback::begin(&mut task);
        task.title = "Edited during save".into();
        task.description = "Keep this detail".into();
        task.touch();
        assert!(rollback.apply(rollback.attempt, &mut task));
        assert_eq!(task.state, TaskState::Doing);
        assert_eq!(task.position, 7);
        assert_eq!(task.title, "Edited during save");
        assert_eq!(task.description, "Keep this detail");
        assert!(task.completed_at.is_none());
    }

    #[test]
    fn stale_completion_save_cannot_undo_reopened_and_recompleted_issue() {
        let mut task = Task::new("Task", TaskState::Backlog, 2);
        let old = TaskCompletionSaveRollback::begin(&mut task);
        task.set_state(TaskState::Doing);
        let current = TaskCompletionSaveRollback::begin(&mut task);
        assert!(!current.apply(old.attempt, &mut task));
        assert_eq!(task.state, TaskState::Done);
        assert!(current.apply(current.attempt, &mut task));
        assert_eq!(task.state, TaskState::Doing);
    }

    #[test]
    fn foreground_only_completion_waits_for_delayed_close_acknowledgment() {
        let session = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let mut progress = TaskCompletionStop {
            attempt,
            closing: true,
            pending_closes: [session].into_iter().collect(),
            ..TaskCompletionStop::default()
        };
        // An empty background registry cannot discharge foreground shutdown.
        assert_eq!(
            completion_can_finish([Settled, progress.runtime_state()]),
            Stopping
        );
        progress.acknowledge_close(Uuid::new_v4(), session, true);
        assert_eq!(progress.runtime_state(), Stopping);
        progress.acknowledge_close(attempt, session, true);
        assert_eq!(progress.runtime_state(), Settled);
    }

    #[test]
    fn rejected_foreground_close_keeps_completion_failed_without_background_work() {
        let session = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let mut progress = TaskCompletionStop {
            attempt,
            closing: true,
            pending_closes: [session].into_iter().collect(),
            ..TaskCompletionStop::default()
        };
        progress.acknowledge_close(attempt, session, false);
        assert_eq!(
            completion_can_finish([Settled, progress.runtime_state()]),
            Failed
        );
        progress.acknowledge_close(attempt, session, true);
        assert_eq!(progress.runtime_state(), Failed);
    }

    #[test]
    fn completion_waits_for_every_background_acknowledgment() {
        assert_eq!(completion_can_finish([]), Settled);
        assert_eq!(completion_can_finish([Settled, Settled]), Settled);
        assert_eq!(completion_can_finish([Settled, Stopping]), Stopping);
        assert_eq!(completion_can_finish([Stopping, Settled]), Stopping);
    }

    #[test]
    fn failed_stop_never_archives_an_issue() {
        assert_eq!(completion_can_finish([Failed, Settled]), Failed);
        assert_eq!(completion_can_finish([Stopping, Failed]), Failed);
        assert_eq!(completion_can_finish([Failed, Stopping]), Failed);
    }
}
