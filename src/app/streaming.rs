use super::*;

impl Waku {
    /// How a finished turn presents. An interrupted turn is a user stop — the
    /// Stop button, or a provider-side stop such as a denied permission — and
    /// settles exactly like the local Stop: idle and interrupted, never a red
    /// failure and never a queued follow-up drain.
    pub(super) fn turn_settlement(
        success: bool,
        interrupted: bool,
    ) -> (SessionStatus, TurnStatus, BackgroundWorkStatus) {
        if interrupted {
            (
                SessionStatus::Idle,
                TurnStatus::Interrupted,
                BackgroundWorkStatus::Stopped,
            )
        } else if success {
            (
                SessionStatus::Idle,
                TurnStatus::Completed,
                BackgroundWorkStatus::Completed,
            )
        } else {
            (
                SessionStatus::Failed,
                TurnStatus::Failed,
                BackgroundWorkStatus::Failed,
            )
        }
    }

    /// Records a settlement the provider reported as a refused prompt, and
    /// whether this settlement was one.
    ///
    /// The prompt never reached the conversation, so the user's message is
    /// marked not delivered with the provider's own reason instead of being
    /// answered, and the turn that existed only for it is dropped. A turn the
    /// provider had already started is not a refusal: the run failed, and the
    /// settlement is that turn's outcome.
    fn record_refused_prompt(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        summary: Option<&str>,
    ) -> bool {
        let Some(session) = self.state.session_mut(session_id) else {
            return false;
        };
        let reason = match summary {
            Some(summary) => compact_driver_error(summary),
            // A refusal with no reason of its own still did not deliver the
            // message, and naming the provider says who refused it.
            None => tr!(
                "errors.provider_rejected_prompt",
                provider = session.provider.display_name()
            ),
        };
        if !session.mark_active_prompt_undelivered(&reason) {
            return false;
        }
        runtime.last_driver_error = None;
        self.state.mark_session_dirty(session_id);
        true
    }

    /// Hand back the queued messages a settlement took out of the provider's
    /// queue. They never reached the conversation, so they leave the transcript
    /// and their text returns to the user — the composer when this session is
    /// the one on screen, and its stored draft when it is not, so a background
    /// session's message cannot land in someone else's input. A turn that
    /// existed only for them goes with them, rather than settling as an
    /// answerless turn.
    fn return_retracted_messages(
        &mut self,
        session_id: Uuid,
        texts: &[String],
        cx: &mut Context<Self>,
    ) {
        let selected = self.state.selected_session == Some(session_id);
        let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
        if selected {
            // The visible composer reaches its draft slot on a debounce, and the
            // returned text has to land beside the newest keystrokes rather than
            // behind them.
            self.capture_current_composer_draft(cx);
        }
        let Some(returned) = self
            .state
            .session_mut(session_id)
            .map(|session| session.take_retracted_queue_messages(texts))
        else {
            return;
        };
        if returned.is_empty() {
            return;
        }
        let key = crate::persistence::ComposerDraftKey::Session(session_id);
        let draft = returned_messages_draft(self.composer_drafts.get(key), &returned);
        if self.composer_drafts.set(key, draft) {
            self.schedule_composer_draft_save(cx);
        }
        if selected {
            self.restore_selected_composer_draft(cx);
        }
        self.state.mark_session_dirty(session_id);
        if let Some(previous_kinds) = previous_kinds.as_deref() {
            self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
        }
        cx.notify();
    }

    /// Hand back the messages the provider still holds, which a stop is about
    /// to take out of its queue: the transport clears the queue before it
    /// aborts, so without this the text a stopped turn removed would be gone.
    /// The stopped messages reach the user through the same retraction a
    /// settlement performs.
    pub(super) fn return_stopped_queue_messages(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let texts = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.provider_queued_texts())
            .unwrap_or_default();
        if texts.is_empty() {
            return;
        }
        self.return_retracted_messages(session_id, &texts, cx);
    }

    /// The stored prompt each provider queue report entry names, in the
    /// report's order.
    ///
    /// A report speaks the transport's language — templates expanded, skills
    /// in provider syntax — while the transcript keeps what the user typed, so
    /// each entry is resolved back through the same seam its submission used
    /// before the pending list is matched against it.
    fn provider_queue_texts(&self, session_id: Uuid, report: &[String]) -> Vec<String> {
        let Some((provider, stored)) = self.stored_prompt_texts(session_id) else {
            return report.to_vec();
        };
        stored_queue_texts(provider, &self.slash_command_index, &stored, report)
    }

    /// The prompts the transcript holds for a session, newest first: the list
    /// every provider queue report is resolved against.
    ///
    /// A `queue_update` names both of its queues, and both name the same
    /// stored prompts, so the transcript is read once for the pair.
    fn stored_prompt_texts(&self, session_id: Uuid) -> Option<(ProviderKind, Vec<String>)> {
        let session = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)?;
        let stored = session
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.content.clone())
            .collect();
        Some((session.provider, stored))
    }

    pub(super) fn finish_streaming_assistant(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            for message in &mut session.messages {
                if message.role == MessageRole::Assistant && message.streaming {
                    message.streaming = false;
                }
            }
        }
    }

    pub(super) fn append_text_delta(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        delta: String,
    ) {
        let previous_phase = runtime.stream_phase;
        if previous_phase == Some(StreamPhase::Reasoning) {
            self.complete_reasoning_activity(session_id);
        }
        let continuing = previous_phase == Some(StreamPhase::Text);
        append_text_delta_to_session(&mut self.state.sessions, session_id, continuing, delta);
        self.state.mark_session_dirty(session_id);
        runtime.stream_phase = Some(StreamPhase::Text);
    }

    fn complete_reasoning_activity(&mut self, session_id: Uuid) {
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        let reasoning = session
            .transcript_blocks
            .iter_mut()
            .rev()
            .flat_map(|block| block.activities.iter_mut().rev())
            .find(|activity| activity.reasoning.is_some() && !activity.complete);
        if let Some(reasoning) = reasoning {
            reasoning.complete = true;
            session.updated_at = unix_time();
        }
    }

    pub(super) fn append_reasoning_delta(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        delta: String,
    ) {
        let previous_phase = runtime.stream_phase;
        let continuing = previous_phase == Some(StreamPhase::Reasoning);
        if !continuing && delta.trim().is_empty() {
            return;
        }
        let now = unix_time_millis();
        if !continuing {
            self.finish_streaming_assistant(session_id);
        }
        if let Some(session) = self.state.session_mut(session_id) {
            if continuing
                && let Some(reasoning) = session
                    .transcript_blocks
                    .last_mut()
                    .and_then(|block| block.activities.last_mut())
                    .and_then(|activity| activity.reasoning.as_mut())
            {
                reasoning.content.push_str(&delta);
                reasoning.finished_at_ms = now;
            } else {
                push_transcript_activity(
                    session,
                    ActivityItem::from_reasoning(
                        ReasoningBlock {
                            content: delta,
                            started_at_ms: now,
                            finished_at_ms: now,
                        },
                        false,
                    ),
                    matches!(
                        previous_phase,
                        Some(StreamPhase::Reasoning | StreamPhase::Activity)
                    ),
                );
            }
            session.updated_at = unix_time();
        }
        runtime.stream_phase = Some(StreamPhase::Reasoning);
    }

    pub(super) fn update_activity(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        item: ActivityItem,
    ) {
        let previous_phase = runtime.stream_phase;
        if previous_phase == Some(StreamPhase::Text) {
            self.finish_streaming_assistant(session_id);
        }
        if previous_phase == Some(StreamPhase::Reasoning) {
            self.complete_reasoning_activity(session_id);
        }

        let continuing_work = matches!(
            previous_phase,
            Some(StreamPhase::Reasoning | StreamPhase::Activity)
        );
        if let Some(session) = self.state.session_mut(session_id) {
            for block in session.transcript_blocks.iter_mut().rev() {
                let matching = block.activities.iter_mut().rev().find(|activity| {
                    item.source_id
                        .as_ref()
                        .is_some_and(|id| activity.source_id.as_ref() == Some(id))
                        || (item.source_id.is_none()
                            && activity.title == item.title
                            && !activity.complete)
                });
                if let Some(activity) = matching {
                    let has_arguments = item.arguments.is_some();
                    let replaces_changes = !item.file_changes.is_empty();
                    let activity_id = activity.id;
                    activity.kind = item.kind;
                    activity.title = item.title;
                    if item.tool_name.is_some() {
                        activity.tool_name = item.tool_name;
                    }
                    if item.mcp_server.is_some() {
                        activity.mcp_server = item.mcp_server;
                    }
                    activity.complete = item.complete;
                    activity.failed = item.failed;
                    if item.detail.is_some() {
                        activity.detail = item.detail;
                    }
                    if item.arguments.is_some() {
                        activity.arguments = item.arguments;
                    }
                    if item.output.is_some() {
                        activity.output = item.output;
                    }
                    if !item.image_urls.is_empty() {
                        activity.image_urls = item.image_urls;
                    }
                    if !item.file_changes.is_empty() {
                        activity.file_changes = item.file_changes;
                    }
                    if item.display_target.is_some()
                        && (activity.display_target.is_none() || has_arguments)
                    {
                        activity.display_target = item.display_target;
                    }
                    if item.display_description.is_some()
                        && (activity.display_description.is_none() || has_arguments)
                    {
                        activity.display_description = item.display_description;
                    }
                    if item.reasoning.is_some() {
                        activity.reasoning = item.reasoning;
                    }
                    session.updated_at = unix_time();
                    runtime.stream_phase = Some(StreamPhase::Activity);
                    if replaces_changes {
                        // The rows this activity's diff was built from are gone;
                        // an expanded card rebuilds from the new ones.
                        self.activity_diffs.borrow_mut().remove(&activity_id);
                    }
                    return;
                }
            }

            push_transcript_activity(session, item, continuing_work);
            session.updated_at = unix_time();
        }
        runtime.stream_phase = Some(StreamPhase::Activity);
    }

    pub(super) fn complete_turn_blocks(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            for block in &mut session.transcript_blocks {
                for activity in &mut block.activities {
                    activity.complete = true;
                }
            }
        }
    }

    pub(super) fn turn_has_assistant_message(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| {
                let Some(turn_id) = session.active_turn_id() else {
                    return false;
                };
                session.messages.iter().any(|message| {
                    message.role == MessageRole::Assistant && message.turn_id == Some(turn_id)
                })
            })
    }

    /// Whether the running turn was prompted — a provider-started wake has no
    /// user message of its own.
    pub(super) fn active_turn_has_user_message(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| {
                let turn_id = session.active_turn_id()?;
                Some(session.messages.iter().any(|message| {
                    message.turn_id == Some(turn_id) && message.role == MessageRole::User
                }))
            })
            .unwrap_or(false)
    }

    pub(super) fn accepts_turn_output(&mut self, session_id: Uuid) -> bool {
        // The turn begins at submission accept, before its prompt has reached
        // any provider. While preparation is still running, a reused runtime
        // could only be draining leftovers of a settled turn — output landing
        // in the new turn then would attribute stale text to it.
        if self.submission_preparations.contains(&session_id) {
            return false;
        }
        self.state
            .session_mut(session_id)
            .is_some_and(session_accepts_turn_output)
    }

    /// Returns whether the runtime should remain attached after this event.
    ///
    /// `allow_queue_drain` is false when the caller is flushing buffered
    /// events for a turn the user just stopped: a settling event must not
    /// start queued follow-ups then, because the user asked to stop, not to
    /// continue.
    pub(super) fn handle_driver_event(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        event: DriverEvent,
        allow_queue_drain: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        runtime.last_active_at = Instant::now();
        match event {
            DriverEvent::RuntimeEventCursorAdvanced(cursor) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    session.runtime_event_cursor = Some(cursor);
                }
            }
            DriverEvent::Connected { provider_cursor } => {
                runtime.last_driver_error = None;
                runtime.last_background_refresh_at = Instant::now();
                runtime.driver.refresh_background_work();
                if let Some(session) = self.state.session_mut(session_id) {
                    if let Some(ProviderResumeCursor::Claude {
                        resume_at: Some(message_id),
                        ..
                    }) = &provider_cursor
                    {
                        session.mark_active_turn_provider_resume_at(message_id.clone());
                    }
                    session.provider_cursor = provider_cursor;
                    if session.status == SessionStatus::Connecting {
                        session.status = SessionStatus::Working;
                    }
                }
            }
            DriverEvent::AgentPresetSelected(agent_preset) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    session.agent_preset = agent_preset;
                }
            }
            DriverEvent::AutoTitleUpdated(title) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    session.set_auto_title(title);
                }
            }
            DriverEvent::AvailableCommands(names) => {
                if let Some(session) = self
                    .state
                    .session_mut(session_id)
                    .filter(|session| session.available_commands != names)
                {
                    session.available_commands = names;
                    // The drain has no `Context`; the frame loop rebuilds the
                    // drawn index when it sees this.
                    self.composer_sources_stale = true;
                }
            }
            DriverEvent::PromptSubmitted {
                message,
                turn_id,
                message_id,
            } => {
                // A prompt reached this runtime: another client's submission,
                // or the echo of this one. The session decides whether that
                // is news; a mirrored turn is marked for the next save so the
                // projection this client persists carries the prompt whose
                // reply it is about to stream.
                if let Some(session) = self.state.session_mut(session_id)
                    && session.adopt_submitted_prompt(&message, turn_id, message_id)
                {
                    self.state.mark_session_dirty(session_id);
                }
            }
            DriverEvent::TurnStarted => {
                runtime.last_driver_error = None;
                if let Some(session) = self.state.session_mut(session_id) {
                    if session.active_turn_id().is_some() {
                        // Covers submissions and the optimistic pursuit turn
                        // a `/goal` began: the provider's start confirms it.
                        session.mark_active_turn_provider_started();
                        session.status = SessionStatus::Working;
                    } else if begin_provider_initiated_turn(session) {
                        self.state.mark_session_dirty(session_id);
                    }
                }
            }
            DriverEvent::TurnParked => {
                // The reply ended while detached work the provider will wake
                // the session for is still running. The turn stays open for
                // that wake; only its streaming state settles, and the session
                // shows the wait instead of a finish. A prompted turn announces
                // the wait once; a wake that parks again stays quiet.
                if self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_none()
                {
                    return true;
                }
                self.settle_foreground_work(session_id, BackgroundWorkStatus::Completed);
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                let announce = cx.active_window().is_none()
                    && !runtime.park_announced
                    && self.active_turn_has_user_message(session_id);
                let task_notification = announce
                    .then(|| {
                        self.state
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .map(|session| {
                                if session.display_title() == AgentSession::DEFAULT_TITLE {
                                    tr!("session.new_task")
                                } else {
                                    session.display_title().to_owned()
                                }
                            })
                    })
                    .flatten();
                self.finish_streaming_assistant(session_id);
                self.complete_turn_blocks(session_id);
                runtime.stream_phase = None;
                runtime.park_announced = true;
                if let Some(session) = self.state.session_mut(session_id) {
                    session.status = SessionStatus::Background;
                    session.updated_at = unix_time();
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                if let Some(title) = task_notification {
                    crate::platform::show_task_notification(
                        &task_notification_tag(session_id),
                        &title,
                        &tr!("session.turn_waiting_background"),
                        cx,
                    );
                }
            }
            DriverEvent::TextDelta(delta) => {
                if self.accepts_turn_output(session_id) {
                    self.append_text_delta(session_id, runtime, delta);
                }
            }
            DriverEvent::ReasoningDelta(delta) => {
                if self.accepts_turn_output(session_id) {
                    self.append_reasoning_delta(session_id, runtime, delta);
                }
            }
            DriverEvent::Activity {
                id,
                kind,
                title,
                detail,
                complete,
            } => {
                if self.accepts_turn_output(session_id) {
                    let refresh_branch = should_refresh_branch_after_activity(kind, complete)
                        && self.state.selected_session == Some(session_id);
                    let item = ActivityItem::new(id, kind, title, detail, complete);
                    self.observe_foreground_command_activity(session_id, &item);
                    self.update_activity(session_id, runtime, item);
                    if refresh_branch {
                        self.refresh_selected_branch_snapshot(cx);
                    }
                }
            }
            DriverEvent::RichActivity(item) => {
                if self.accepts_turn_output(session_id) {
                    let refresh_branch =
                        should_refresh_branch_after_activity(item.kind, item.complete)
                            && self.state.selected_session == Some(session_id);
                    self.observe_foreground_command_activity(session_id, &item);
                    self.update_activity(session_id, runtime, item);
                    if refresh_branch {
                        self.refresh_selected_branch_snapshot(cx);
                    }
                }
            }
            DriverEvent::BackgroundWork(event) => {
                // Background work is session state, not turn output. It must
                // survive a settled or rewound turn and therefore bypasses
                // `accepts_turn_output` deliberately.
                self.handle_background_work_event(session_id, event);
            }
            DriverEvent::ExtensionMessage { text, display, .. } => {
                if let Some(session) = self.state.session_mut(session_id) {
                    record_extension_message(session, text, display);
                }
            }
            DriverEvent::ExtensionNotification { message, severity } => {
                // The provider records nothing for a notification — it is not a
                // message in its session tree — so the notice surface is the
                // only place it can land. It is shown whichever session is on
                // screen: pi-subagents reports a failed child this way, and
                // silence about work that was started here is the failure the
                // notification exists to prevent.
                self.show_toast_with_tone(message, extension_notification_tone(severity));
            }
            DriverEvent::ExtensionStatus { key, text } => {
                if let Some(session) = self.state.session_mut(session_id) {
                    set_extension_status(session, key, text);
                }
            }
            DriverEvent::ExtensionWidget {
                key,
                lines,
                placement,
            } => {
                if let Some(session) = self.state.session_mut(session_id) {
                    set_extension_widget(session, key, lines, placement);
                }
            }
            DriverEvent::ExtensionTitle { title } => {
                if let Some(session) = self.state.session_mut(session_id) {
                    set_extension_window_title(session, title);
                }
            }
            DriverEvent::ExtensionEditorText { text } => {
                self.apply_extension_editor_text(session_id, text, cx);
            }
            DriverEvent::Permission {
                request_id,
                title,
                detail,
                options,
            } => {
                if self.accepts_turn_output(session_id) {
                    runtime.permission_note_open = false;
                    runtime.pending_permission = Some(PendingPermission {
                        request_id,
                        title,
                        detail,
                        options,
                    });
                    if let Some(session) = self.state.session_mut(session_id) {
                        session.status = SessionStatus::Waiting;
                    }
                }
            }
            DriverEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                // A provider whose dismissal is a cancellation (Pi's extension
                // dialogs) can ask outside a turn: an extension command opens
                // the dialog before any run starts, and dropping it there
                // would leave the extension blocked. A structured question
                // still belongs to the turn that asked it.
                let dismissible = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .is_some_and(|session| session.provider.supports_user_input_cancellation());
                if !questions.is_empty() && (dismissible || self.accepts_turn_output(session_id)) {
                    runtime.pending_user_input =
                        Some(PendingUserInput::new(request_id, questions, dismissible));
                    if self.state.selected_session == Some(session_id) {
                        self.user_input_answer
                            .update(cx, |input, cx| input.clear(cx));
                    }
                    if let Some(session) = self.state.session_mut(session_id)
                        && session.active_turn_id().is_some()
                    {
                        session.status = SessionStatus::Waiting;
                    }
                }
            }
            DriverEvent::ComputerUseUpdated(state) => {
                if self.accepts_turn_output(session_id) {
                    Self::upsert_computer_use_preview(session_id, runtime, state, cx);
                }
            }
            DriverEvent::SteerAccepted { message } => {
                let submission = runtime
                    .pending_steers
                    .iter()
                    .position(|submission| submission.prompt == message)
                    .and_then(|index| runtime.pending_steers.remove(index))
                    // Providers normally echo the exact transport text, but a
                    // normalized echo still acknowledges the oldest pending
                    // steer. Preserve its attachment presentation metadata.
                    .or_else(|| runtime.pending_steers.pop_front())
                    .unwrap_or_else(|| ComposerSubmission::plain(message.clone()));
                // The provider folded the message into the live turn. Append
                // it to the same turn so the transcript mirrors the provider
                // conversation (no new turn boundary).
                if let Some(session) = self.state.session_mut(session_id) {
                    session.push_user_message_with_presentation(
                        message,
                        submission.display_content,
                        submission.attachments,
                        submission.annotations,
                    );
                    session.updated_at = unix_time();
                }
            }
            DriverEvent::SteerRejected { message, reason } => {
                let submission = runtime
                    .pending_steers
                    .iter()
                    .position(|submission| submission.prompt == message)
                    .and_then(|index| runtime.pending_steers.remove(index))
                    .or_else(|| runtime.pending_steers.pop_front())
                    .unwrap_or_else(|| ComposerSubmission::plain(message));
                let (busy, settled_cleanly) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| {
                        let settled_cleanly = session
                            .turns
                            .last()
                            .is_some_and(|turn| turn.status == TurnStatus::Completed);
                        (session.is_busy(), settled_cleanly)
                    })
                    .unwrap_or((false, false));
                if busy {
                    self.enqueue_follow_up_submission(session_id, submission, cx);
                    if self.state.selected_session == Some(session_id) {
                        self.show_toast(tr!(
                            "session.steer_rejected",
                            error = compact_driver_error(&reason)
                        ));
                    }
                } else if settled_cleanly {
                    // The turn settled before the steer arrived; run the
                    // message as a fresh turn instead of losing it. Submission
                    // is deferred through the queue-drain pass because this
                    // session's runtime is detached from the map while its
                    // events are handled — an inline submit would spawn a
                    // second driver process only to have it clobbered when the
                    // drain re-inserts the detached runtime.
                    if let Some(session) = self.state.session_mut(session_id) {
                        session
                            .queued_messages
                            .insert(0, submission.into_queued_message());
                    }
                    if allow_queue_drain {
                        self.pending_queue_drains.push(session_id);
                    }
                } else {
                    // The user stopped the turn (or the provider died) before
                    // the steer landed. Keep the message visible and
                    // user-controlled instead of auto-running it.
                    self.enqueue_follow_up_submission(session_id, submission, cx);
                }
            }
            DriverEvent::ProviderQueue {
                steering,
                follow_up,
            } => {
                // The provider's own queue is the truth about what is still on
                // its way: a message in it is queued, and one it leaves out has
                // been delivered.
                if let Some((provider, stored)) = self.stored_prompt_texts(session_id) {
                    let commands = &self.slash_command_index;
                    let steering = stored_queue_texts(provider, commands, &stored, &steering);
                    let follow_up = stored_queue_texts(provider, commands, &stored, &follow_up);
                    if let Some(session) = self.state.session_mut(session_id)
                        && session.mark_provider_queue(&steering, &follow_up)
                    {
                        self.state.mark_session_dirty(session_id);
                    }
                }
            }
            DriverEvent::QueuedMessagesRetracted { messages } => {
                // No run will carry these, so they leave the transcript and
                // their text goes back to the user. The settle that caused it
                // arrives next.
                let messages = self.provider_queue_texts(session_id, &messages);
                self.return_retracted_messages(session_id, &messages, cx);
            }
            DriverEvent::PlanUsageUpdated(usage) => {
                if let Some(provider) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.provider)
                {
                    self.plan_usage.insert(provider, usage);
                }
            }
            DriverEvent::GoalUpdated(goal) => {
                // Conversation meta like usage: it applies regardless of turn
                // state, and `None` means the provider cleared the goal.
                if goal.is_some() {
                    self.goal_observed_at.insert(session_id, Instant::now());
                } else {
                    self.goal_observed_at.remove(&session_id);
                }
                if let Some(session) = self.state.session_mut(session_id) {
                    if let Some(goal) = &goal
                        && session.messages.is_empty()
                    {
                        // A goal-first task is named after its objective
                        // until the provider reports a better title.
                        session.set_title_from_prompt(&goal.objective);
                    }
                    if session.thread_goal != goal {
                        session.thread_goal = goal;
                        self.state.mark_session_dirty(session_id);
                    }
                }
            }
            DriverEvent::UsageUpdated {
                context_tokens,
                context_window,
            } => {
                // Meta about the conversation, not turn output: it applies
                // even while a rewound or cancelled turn's tail drains.
                if let Some(session) = self.state.session_mut(session_id) {
                    let usage = session.context_usage.get_or_insert(ContextUsage::default());
                    if let Some(tokens) = context_tokens {
                        usage.tokens = tokens;
                    }
                    if let Some(window) = context_window {
                        usage.window = Some(window);
                    }
                    self.state.mark_session_dirty(session_id);
                }
            }
            DriverEvent::TurnFinished {
                success,
                summary,
                interrupted,
            } => {
                // A manual compaction's settlement carries no turn id. When the
                // recorded compaction turn is no longer the active one, the app
                // already ended it (the user stopped) and this event belongs to
                // that ended turn: settling the current one with it would end a
                // turn that never ended.
                if let Some(recorded) = runtime.compaction_turn
                    && self
                        .state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .and_then(AgentSession::active_turn_id)
                        != Some(recorded)
                {
                    runtime.compaction_turn = None;
                    return true;
                }
                // A prompt the provider refused before accepting it settles as
                // the delivery failure of the message that asked for the run:
                // that message stays, marked undelivered with the reason, and
                // the turn goes with it. Nothing ran, so the usual settlement
                // — an answer row, a checkpoint, background work — would be
                // reporting a turn that never happened.
                if !success && self.record_refused_prompt(session_id, runtime, summary.as_deref()) {
                    runtime.compaction_turn = None;
                    return true;
                }
                let (session_status, turn_status, background_status) =
                    Self::turn_settlement(success, interrupted);
                self.settle_foreground_work(session_id, background_status);
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                runtime.last_driver_error = None;
                // A settled turn moved the account's rate-limit needles; ask
                // that provider's plan meter to refresh once its backoff
                // allows.
                if let Some(provider) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.provider)
                    .filter(|provider| usage_meter::PLAN_USAGE_PROVIDERS.contains(provider))
                {
                    self.plan_usage_stale.insert(provider);
                }
                if self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_none()
                {
                    return true;
                }
                let task_notification = cx.active_window().is_none().then(|| {
                    self.state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| {
                            let title = if session.display_title() == AgentSession::DEFAULT_TITLE {
                                tr!("session.new_task")
                            } else {
                                session.display_title().to_owned()
                            };
                            let body = if success {
                                tr!("session.turn_completed")
                            } else {
                                tr!("session.stopped")
                            };
                            (title, body)
                        })
                });
                self.finish_streaming_assistant(session_id);
                self.complete_turn_blocks(session_id);
                runtime.stream_phase = None;
                runtime.park_announced = false;
                // A provider command whose transport answers without a model
                // turn (Pi's `/compact`) settles with its activity row as the
                // whole record; the answerless-turn fallback would add a
                // synthetic reply under it. Only that recorded turn skips it.
                let compaction_turn = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_some_and(|turn_id| runtime.compaction_turn.take() == Some(turn_id));
                let needs_fallback =
                    !self.turn_has_assistant_message(session_id) && !compaction_turn;
                if let Some(session) = self.state.session_mut(session_id) {
                    // A provider-side user stop — the Stop button, or a denied
                    // permission the provider aborted on — settles like the
                    // app's own Stop: idle, never a red failure.
                    session.status = session_status;
                    if needs_fallback {
                        let fallback = if interrupted {
                            tr!("session.stopped")
                        } else {
                            summary.unwrap_or_else(|| {
                                if success {
                                    tr!("session.turn_completed")
                                } else {
                                    tr!("session.stopped_before_response")
                                }
                            })
                        };
                        session.push_message(MessageRole::Assistant, fallback);
                    }
                }
                self.finish_active_turn(session_id, turn_status);
                runtime.pending_permission = None;
                runtime.permission_note_open = false;
                runtime.pending_user_input = None;
                runtime.pending_computer_approval = None;
                runtime.driver.cancel_computer_use();
                // The agent may have edited files or switched branches, so the
                // cached view of the workspace is no longer trustworthy. This
                // handler has no `Context`, so the drain loop acts on the flag.
                if self.state.selected_session == Some(session_id) {
                    self.workspace_queries_stale = true;
                }
                runtime.computer_use_previews.clear();
                runtime.driver.refresh_background_work();
                self.capture_latest_turn_checkpoint_for(session_id);
                if allow_queue_drain && success {
                    // Start the next queued follow-up once the runtime has
                    // been re-inserted so the same process is reused.
                    self.pending_queue_drains.push(session_id);
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                if let Some(Some((title, body))) = task_notification {
                    crate::platform::show_task_notification(
                        &task_notification_tag(session_id),
                        &title,
                        &body,
                        cx,
                    );
                }
            }
            DriverEvent::Error(error) => {
                let error = compact_driver_error(&error);
                runtime.last_driver_error = Some(error.clone());
                if self.state.selected_session == Some(session_id) {
                    self.show_toast(error.clone());
                }
                // An optimistic pursuit turn has no submission to fail with.
                // Unwind it so the error cannot strand a spinner; if the
                // pursuit does start later, its own start report recreates
                // the turn.
                self.unwind_unconfirmed_pursuit_turn(session_id);
                let has_active_turn = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_some();
                let should_append = has_active_turn
                    && !self.turn_has_assistant_message(session_id)
                    && self
                        .state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .is_some_and(|session| session.status != SessionStatus::Working);
                if let Some(session) = self.state.session_mut(session_id)
                    && has_active_turn
                {
                    if session.status != SessionStatus::Working {
                        session.status = SessionStatus::Failed;
                    }
                    if should_append {
                        session.push_message(MessageRole::Assistant, error);
                    }
                }
            }
            DriverEvent::ProcessExited => {
                self.mark_background_work_lost(session_id);
                runtime.compaction_turn = None;
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                self.finish_streaming_assistant(session_id);
                self.complete_turn_blocks(session_id);
                runtime.stream_phase = None;
                runtime.pending_permission = None;
                runtime.permission_note_open = false;
                runtime.pending_user_input = None;
                runtime.pending_computer_approval = None;
                runtime.driver.cancel_computer_use();
                runtime.computer_use_previews.clear();
                let needs_fallback = !self.turn_has_assistant_message(session_id);
                let failure_message = runtime
                    .last_driver_error
                    .take()
                    .unwrap_or_else(|| tr!("session.codex_exited_before_response"));
                let should_finish_turn = if let Some(session) = self.state.session_mut(session_id)
                    && session.status.is_busy()
                {
                    session.status = SessionStatus::Failed;
                    session.updated_at = unix_time();
                    if needs_fallback {
                        session.push_message(MessageRole::Assistant, failure_message);
                    }
                    true
                } else {
                    false
                };
                let finished_turn = should_finish_turn
                    && self
                        .finish_active_turn(session_id, TurnStatus::Failed)
                        .is_some();
                if finished_turn {
                    self.capture_latest_turn_checkpoint_for(session_id);
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                return false;
            }
        }
        true
    }

    fn upsert_computer_use_preview(
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        state: ComputerUseState,
        cx: &mut Context<Self>,
    ) {
        if !state.visible {
            return;
        }
        let Some(window_id) = state.target.as_ref().map(|target| target.window_id) else {
            return;
        };
        let mut preview = if let Some(index) =
            runtime.computer_use_previews.iter().position(|preview| {
                preview
                    .target
                    .as_ref()
                    .is_some_and(|target| target.window_id == window_id)
            }) {
            if !runtime.computer_use_previews[index].visible {
                return;
            }
            runtime.computer_use_previews.remove(index)
        } else {
            ComputerUsePreview {
                target: None,
                phase: state.phase,
                visible: state.visible,
                frames: Default::default(),
                decode_task: None,
            }
        };
        preview.target = state.target;
        preview.phase = state.phase;
        preview.visible = state.visible;
        if let Some(image_url) = state.image_url {
            let generation = preview.frames.begin();
            // Dropping the prior task also prevents a dismissed/recreated
            // window or replaced runtime from receiving its stale completion.
            preview.decode_task = None;
            let renderer = cx.svg_renderer();
            let current_source = preview.frames.current.as_ref().map(|frame| frame.source_id);
            let decode = cx.background_executor().spawn(async move {
                crate::computer_use::decode_preview_image_url(&image_url, renderer, current_source)
                    .ok()
                    .flatten()
            });
            preview.decode_task = Some(cx.spawn(async move |this, cx| {
                let image = decode.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(preview) = this.runtimes.get_mut(&session_id).and_then(|runtime| {
                        runtime.computer_use_previews.iter_mut().find(|preview| {
                            preview
                                .target
                                .as_ref()
                                .is_some_and(|target| target.window_id == window_id)
                        })
                    }) else {
                        return;
                    };
                    let image = image.map(|(source_id, image)| {
                        crate::computer_use::PreviewImage::new(source_id, image, cx)
                    });
                    if preview.frames.complete(generation, image) {
                        cx.notify();
                    }
                });
            }));
        }
        runtime.computer_use_previews.push(preview);
    }

    /// Put text an extension handed the composer (`set_editor_text`) where the
    /// user will find it.
    ///
    /// It becomes the session's own draft, so a session in the background
    /// keeps it until the user switches to it, and the visible composer only
    /// takes it when that session is the one on screen. The draft is captured
    /// first because the visible composer reaches its slot on a debounce, and
    /// the extension's text belongs after the newest keystrokes, not behind
    /// them.
    fn apply_extension_editor_text(
        &mut self,
        session_id: Uuid,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let selected = self.state.selected_session == Some(session_id);
        if selected {
            self.capture_current_composer_draft(cx);
        }
        let key = crate::persistence::ComposerDraftKey::Session(session_id);
        let draft = extension_editor_text_draft(self.composer_drafts.get(key), text);
        if self.composer_drafts.set(key, draft) {
            self.schedule_composer_draft_save(cx);
        }
        if selected {
            self.restore_selected_composer_draft(cx);
        }
        cx.notify();
    }
}

/// The composer state a returned submission restores: the text the settlement
/// handed back, in front of whatever the draft already held, with the
/// returned message's own attachments and annotations kept alongside.
pub(super) fn returned_messages_draft(
    existing: Option<&crate::persistence::ComposerDraft>,
    returned: &[Message],
) -> crate::persistence::ComposerDraft {
    let mut draft = existing.cloned().unwrap_or_default();
    let returned_text = returned
        .iter()
        .map(|message| message.visible_content().trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let existing_text = std::mem::take(&mut draft.text);
    draft.text = match (returned_text.is_empty(), existing_text.is_empty()) {
        (true, _) => existing_text,
        (false, true) => returned_text,
        (false, false) => format!("{returned_text}\n\n{existing_text}"),
    };
    let mut attachments = returned
        .iter()
        .flat_map(|message| message.attachments.iter())
        .map(super::drafts::draft_attachment)
        .collect::<Vec<_>>();
    attachments.extend(draft.attachments);
    draft.attachments = attachments;
    let mut annotations = returned
        .iter()
        .flat_map(|message| message.annotations.iter().cloned())
        .collect::<Vec<_>>();
    annotations.extend(draft.annotations);
    draft.annotations = annotations;
    draft
}

/// The stored prompt each provider queue report entry names.
///
/// The transport text and the transcript text differ wherever submission
/// resolves one: a template expands, a skill takes the provider's syntax. A
/// queue report names the message by the transport's text, so each entry is
/// matched back to the newest stored prompt that resolves to it; an entry
/// already in transport form (a steer's message) matches its own text as it
/// is.
///
/// `stored` is newest first, so the match agrees with the newest-first rule
/// the pending mark itself applies.
pub(super) fn stored_queue_texts(
    provider: ProviderKind,
    commands: &[SlashCommand],
    stored: &[String],
    report: &[String],
) -> Vec<String> {
    report
        .iter()
        .map(|entry| {
            stored
                .iter()
                .find(|prompt| {
                    prompt.as_str() == entry
                        || crate::composer_complete::resolved_submission(provider, prompt, commands)
                            .as_deref()
                            == Some(entry.as_str())
                })
                .cloned()
                .unwrap_or_else(|| entry.clone())
        })
        .collect()
}

/// Records an extension message from the provider's own session tree.
///
/// A message the provider marked for display becomes a transcript notice, the
/// same shape the app's own system lines take. A message the provider withheld
/// from the conversation adds no row and is not kept beside one: nothing in the
/// client renders it, and the provider's own session file is the record of a
/// tree it owns.
///
/// Recording a message never opens a turn: Pi announces a run of its own with
/// `agent_start`/`turn_start`, and a notice an extension appends without one
/// must not fabricate it.
pub(super) fn record_extension_message(session: &mut AgentSession, text: String, display: bool) {
    if !display || text.trim().is_empty() {
        return;
    }
    session.push_message(MessageRole::System, text);
}

/// Records a status entry an extension published for its session.
///
/// Pi keys these entries by the extension's own key, so setting a key again
/// replaces its entry and `None` clears it: the session holds exactly the
/// entries the extension still keeps, which is what the composer strip shows.
pub(super) fn set_extension_status(session: &mut AgentSession, key: String, text: Option<String>) {
    match text {
        Some(text) => match session
            .extension_status
            .iter_mut()
            .find(|entry| entry.key == key)
        {
            Some(entry) => entry.text = text,
            None => session
                .extension_status
                .push(ExtensionStatusEntry { key, text }),
        },
        None => session.extension_status.retain(|entry| entry.key != key),
    }
}

/// Records a widget an extension displays against the composer, keyed like a
/// status entry: `lines` replaces that key's widget and `None` clears it.
pub(super) fn set_extension_widget(
    session: &mut AgentSession,
    key: String,
    lines: Option<Vec<String>>,
    placement: ExtensionWidgetPlacement,
) {
    match lines {
        Some(lines) => match session
            .extension_widgets
            .iter_mut()
            .find(|widget| widget.key == key)
        {
            Some(widget) => {
                widget.lines = lines;
                widget.placement = placement;
            }
            None => session.extension_widgets.push(ExtensionWidget {
                key,
                lines,
                placement,
            }),
        },
        None => session.extension_widgets.retain(|widget| widget.key != key),
    }
}

/// Records the window title an extension asked for on this session's behalf.
///
/// A blank title is how an extension takes that title down again, and the
/// window then carries the platform's own title as it did before.
pub(super) fn set_extension_window_title(session: &mut AgentSession, title: String) {
    session.extension_window_title = (!title.trim().is_empty()).then_some(title);
}

/// The draft an extension's editor text leaves behind.
///
/// The text replaces what the draft held — an empty text is the extension
/// clearing the editor — while the user's own attachments and annotations stay
/// beside it, because the provider's editor is text-only and never carried
/// them.
pub(super) fn extension_editor_text_draft(
    existing: Option<&crate::persistence::ComposerDraft>,
    text: String,
) -> crate::persistence::ComposerDraft {
    let mut draft = existing.cloned().unwrap_or_default();
    draft.text = text;
    draft
}

/// Whether a provider's own stream may open a turn that no Waku prompt did.
///
/// Codex goal continuation pursues an active goal whenever its thread is idle,
/// Claude Code re-enters the model once a backgrounded command, subagent or
/// monitor settles, and Pi wakes its session when an extension posts a
/// turn-triggering message. A provider absent here never gets a turn without a
/// prompt.
pub(super) fn provider_starts_turns_on_its_own(provider: ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Codex
            | ProviderKind::Claude
            | ProviderKind::OpenCode
            | ProviderKind::Pi
            | ProviderKind::OhMyPi
    )
}

/// Gives a run the provider started on its own a transcript home.
///
/// There is no user message for such a turn, so its work would otherwise be
/// dropped. Only the provider's own run-start signal calls this, and an
/// already-open turn is left to its own start report.
pub(super) fn begin_provider_initiated_turn(session: &mut AgentSession) -> bool {
    if session.active_turn_id().is_some() || !provider_starts_turns_on_its_own(session.provider) {
        return false;
    }
    session.begin_provider_turn();
    session.mark_active_turn_provider_started();
    session.status = SessionStatus::Working;
    true
}

/// Foreground output is stronger evidence of a started provider turn than a
/// replayed lifecycle cursor. Repair both pieces of transient state here so a
/// runtime attachment that missed `TurnStarted` cannot leave Cmd-Enter
/// permanently falling back to the follow-up queue while output is visible.
pub(super) fn session_accepts_turn_output(session: &mut AgentSession) -> bool {
    if session.active_turn_id().is_none() || !session.status.is_busy() {
        return false;
    }
    session.mark_active_turn_provider_started();
    if session.status == SessionStatus::Connecting {
        session.status = SessionStatus::Working;
    }
    true
}

/// A completed edit or shell command is the earliest provider-neutral point at
/// which its filesystem effects are stable enough to re-read. The actual Git
/// work remains behind the branch cache's background fetch.
pub(super) fn should_refresh_branch_after_activity(
    kind: crate::model::ActivityKind,
    complete: bool,
) -> bool {
    complete
        && matches!(
            kind,
            crate::model::ActivityKind::Command | crate::model::ActivityKind::FileChange
        )
}

pub(super) fn push_transcript_activity(
    session: &mut AgentSession,
    item: ActivityItem,
    continuing_work: bool,
) {
    let after_message = session.messages.len();
    let turn_id = session.active_turn_id();
    if continuing_work
        && let Some(block) = session.transcript_blocks.last_mut()
        && block.after_message == after_message
        && block.turn_id == turn_id
    {
        block.activities.push(item);
    } else {
        session.transcript_blocks.push(TranscriptBlock {
            after_message,
            turn_id,
            activities: vec![item],
        });
    }
}

pub(super) fn stream_delta_kind(event: &DriverEvent) -> Option<StreamDeltaKind> {
    match event {
        DriverEvent::TextDelta(_) => Some(StreamDeltaKind::Text),
        DriverEvent::ReasoningDelta(_) => Some(StreamDeltaKind::Reasoning),
        _ => None,
    }
}

pub(super) fn stream_delta_text(event: &DriverEvent, kind: StreamDeltaKind) -> Option<&str> {
    match (kind, event) {
        (StreamDeltaKind::Text, DriverEvent::TextDelta(text))
        | (StreamDeltaKind::Reasoning, DriverEvent::ReasoningDelta(text)) => Some(text),
        _ => None,
    }
}

pub(super) fn compact_driver_error(error: &str) -> String {
    const MAX_LINES: usize = 6;
    const MAX_CHARS: usize = 800;

    let lines = error.lines().collect::<Vec<_>>();
    let mut compact = lines
        .iter()
        .take(MAX_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_LINES {
        compact.push_str("\n…");
    }
    if compact.chars().count() > MAX_CHARS {
        compact = compact.chars().take(MAX_CHARS - 1).collect();
        compact.push('…');
    }
    compact
}

/// Coalesce every adjacent delta of one kind while retaining provider order.
/// Runtime cursors are acknowledgements rather than visible boundaries, so the
/// newest cursor follows the combined delta. The full text enters layout in
/// this pass; Markdown's paint-only veil provides the progressive dissolve.
pub(super) fn pop_stream_batch(
    events: &mut VecDeque<DriverEvent>,
    kind: StreamDeltaKind,
) -> Option<DriverEvent> {
    let mut chunk = String::new();
    let mut latest_cursor = None;
    loop {
        match events.front() {
            Some(DriverEvent::RuntimeEventCursorAdvanced(_)) => {
                latest_cursor = events.pop_front();
            }
            Some(event) if stream_delta_text(event, kind).is_some() => {
                let event = events.pop_front()?;
                match (kind, event) {
                    (StreamDeltaKind::Text, DriverEvent::TextDelta(text))
                    | (StreamDeltaKind::Reasoning, DriverEvent::ReasoningDelta(text)) => {
                        chunk.push_str(&text);
                    }
                    _ => unreachable!("the stream kind was checked before removing the event"),
                }
            }
            _ => break,
        }
    }
    if let Some(cursor) = latest_cursor {
        events.push_front(cursor);
    }
    match kind {
        StreamDeltaKind::Text => Some(DriverEvent::TextDelta(chunk)),
        StreamDeltaKind::Reasoning => Some(DriverEvent::ReasoningDelta(chunk)),
    }
}

pub(super) fn append_text_delta_to_session(
    sessions: &mut [AgentSession],
    session_id: Uuid,
    continuing: bool,
    delta: String,
) {
    let Some(session) = sessions.iter_mut().find(|session| session.id == session_id) else {
        return;
    };
    if !continuing {
        for message in &mut session.messages {
            if message.role == MessageRole::Assistant && message.streaming {
                message.streaming = false;
            }
        }
    }
    let existing = continuing.then(|| {
        session
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role == MessageRole::Assistant && message.streaming)
    });
    if let Some(Some(message)) = existing {
        message.content.push_str(&delta);
    } else {
        let mut message = session
            .active_turn_id()
            .map(|turn_id| Message::new_for_turn(MessageRole::Assistant, delta.clone(), turn_id))
            .unwrap_or_else(|| Message::new(MessageRole::Assistant, delta));
        message.streaming = true;
        session.messages.push(message);
    }
    session.updated_at = unix_time();
}
