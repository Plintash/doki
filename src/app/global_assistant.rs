//! A global conversation, rendered independently from the selected work session.
//! All provider activity uses the ordinary daemon session/runtime transport.
use super::global_assistant_context::{self as context, AssistantCommand};
use super::*;

pub(super) struct AssistantUi {
    pub open: bool,
    pub input: Entity<TextInput>,
    creating: bool,
    pub(super) notice: Option<String>,
    rows: ListState,
    row_count: usize,
    tail_signature: Option<(Uuid, usize, bool)>,
    layout: (f32, f32, f32),
    markdown: RefCell<HashMap<Uuid, MarkdownView>>,
    pub(super) selection: TranscriptSelection,
    return_focus: Option<FocusHandle>,
    pub(super) transcript_focus: FocusHandle,
    context_selection: (Option<Uuid>, Option<Uuid>, Option<Uuid>),
    provider_choice: Option<(ProviderKind, Option<String>)>,
}

impl AssistantUi {
    pub fn new(open: bool, drafts: &ComposerDrafts, window: &mut Window, cx: &mut App) -> Self {
        let draft = drafts
            .get(crate::persistence::ComposerDraftKey::Session(
                AgentSession::GLOBAL_ASSISTANT_ID,
            ))
            .map(|draft| draft.text.clone())
            .unwrap_or_default();
        let rows = ListState::new(0, ListAlignment::Bottom, px(256.0));
        rows.set_follow_mode(gpui::FollowMode::Tail);
        Self {
            open,
            input: cx.new(|cx| {
                let mut input = TextInput::new(window, cx)
                    .semantic_id("global-assistant-input")
                    .multi_line()
                    .submit_on_enter()
                    .auto_height()
                    .tab_stop(true)
                    .placeholder(tr!("assistant.placeholder"));
                input.set_content(draft, cx);
                input
            }),
            creating: false,
            notice: None,
            rows,
            row_count: 0,
            tail_signature: None,
            layout: (0.0, 0.0, 0.0),
            markdown: RefCell::new(HashMap::new()),
            selection: TranscriptSelection::default(),
            return_focus: None,
            transcript_focus: cx.focus_handle().tab_stop(true),
            context_selection: (None, None, None),
            provider_choice: None,
        }
    }
}

impl Waku {
    pub(super) fn assistant_session(&self) -> Option<&AgentSession> {
        self.session(AgentSession::GLOBAL_ASSISTANT_ID)
    }

    pub(super) fn capture_assistant_draft(&mut self, cx: &mut Context<Self>) {
        let changed = self.composer_drafts.set(
            crate::persistence::ComposerDraftKey::Session(AgentSession::GLOBAL_ASSISTANT_ID),
            crate::persistence::ComposerDraft {
                text: self.assistant.input.read(cx).content().to_owned(),
                ..Default::default()
            },
        );
        if changed {
            self.schedule_composer_draft_save(cx);
        }
    }

    pub(super) fn toggle_assistant_action(
        &mut self,
        _: &crate::ToggleAssistant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_assistant_open(!self.assistant.open, window, cx);
    }

    fn set_assistant_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if open {
            self.assistant.return_focus = window.focused(cx);
            self.ensure_session_loaded(AgentSession::GLOBAL_ASSISTANT_ID, cx);
            if self
                .assistant_session()
                .is_some_and(AgentSession::has_started)
            {
                self.start_runtime_attachment(AgentSession::GLOBAL_ASSISTANT_ID, cx);
            }
            window.focus(&self.assistant.input.read(cx).focus(), cx);
        } else if let Some(focus) = self.assistant.return_focus.take() {
            window.focus(&focus, cx);
        } else {
            window.focus(&self.composer.read(cx).focus(), cx);
        }
        self.assistant.open = open;
        self.state.assistant_panel_open = open;
        if open {
            self.start_assistant_workflow_poll(cx);
        }
        self.right_panel_slide = None;
        self.save(cx);
        cx.notify();
    }

    pub(super) fn render_assistant_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        self.task_icon_action(
            "toggle-global-assistant",
            tr!("assistant.toggle"),
            "icons/sparkle.svg",
            true,
            |this, window, cx| this.set_assistant_open(!this.assistant.open, window, cx),
            cx,
        )
    }

    /// Called only by the user's input or an explicit control. Model replies
    /// are display data and never route through the local command parser.
    pub(super) fn submit_assistant(&mut self, raw: String, cx: &mut Context<Self>) {
        let text = raw.trim().to_owned();
        if text.is_empty() || self.assistant.creating {
            return;
        }
        if self
            .assistant_session()
            .is_some_and(|session| !session.detail_loaded || session.is_busy())
        {
            self.assistant.notice = Some(tr!("assistant.wait"));
            self.ensure_session_loaded(AgentSession::GLOBAL_ASSISTANT_ID, cx);
            cx.notify();
            return;
        }
        let command = match context::parse_command(&text) {
            Ok(command) => command,
            Err(error) => {
                self.assistant.notice = Some(error);
                cx.notify();
                return;
            }
        };
        if self.assistant_session().is_none() {
            self.create_assistant_session(text, cx);
            return;
        }
        self.assistant.notice = None;
        if self.state.selected_session != Some(AgentSession::GLOBAL_ASSISTANT_ID) {
            self.assistant.context_selection = self.assistant_context_selection();
        }
        if let Some(command) = command {
            self.run_assistant_command(command, &text, cx);
        } else {
            let snapshot = self.assistant_context();
            let mut submission = ComposerSubmission::plain(context::model_prompt(&snapshot, &text));
            submission.display_content = Some(text);
            self.submit_submission_for_session(AgentSession::GLOBAL_ASSISTANT_ID, submission, cx);
        }
        self.assistant
            .input
            .update(cx, |input, cx| input.set_content("", cx));
        self.capture_assistant_draft(cx);
        cx.notify();
    }

    fn create_assistant_session(&mut self, text: String, cx: &mut Context<Self>) {
        self.assistant.creating = true;
        let daemon = self.daemon.clone();
        let (provider, model) = self
            .assistant
            .provider_choice
            .clone()
            .unwrap_or((self.state.last_provider, self.state.last_model.clone()));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match daemon.client().request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::EnsureGlobalAssistant { provider, model },
                    )? {
                        waku_client::ResponsePayload::GlobalAssistant { project, session } => {
                            Ok((project, session))
                        }
                        _ => anyhow::bail!("The daemon returned an invalid assistant response"),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.creating = false;
                match result {
                    Ok((project, session)) => {
                        if !this
                            .state
                            .projects
                            .iter()
                            .any(|entry| entry.id == project.id)
                        {
                            this.state.projects.push(project);
                        }
                        if let Some(existing) = this
                            .state
                            .sessions
                            .iter_mut()
                            .find(|entry| entry.is_global_assistant())
                        {
                            super::runtime::replace_hydrated_session(existing, session);
                        } else {
                            this.state.sessions.push(session);
                        }
                        this.submit_assistant(text, cx);
                    }
                    Err(error) => {
                        this.assistant.notice = Some(error.to_string());
                        cx.notify();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn assistant_context_selection(&self) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
        if self.board_open {
            let project = self
                .selected_task
                .and_then(|id| self.state.tasks.iter().find(|task| task.id == id))
                .map(Task::project_id)
                .unwrap_or(self.board_project_filter);
            (project, None, self.selected_task)
        } else if self.state.selected_session == Some(AgentSession::GLOBAL_ASSISTANT_ID) {
            self.assistant.context_selection
        } else {
            (
                self.state.selected_project,
                self.state.selected_session,
                self.selected_task,
            )
        }
    }

    fn assistant_context(&self) -> String {
        let (project, session, task) = self.assistant_context_selection();
        let mut snapshot = context::build_context(
            &self.state.projects,
            &self.state.tasks,
            &self.state.sessions,
            project,
            session,
            task,
        );
        snapshot.push_str("\n");
        snapshot.push_str(&self.assistant_workflow_context());
        snapshot
    }

    fn assistant_local_reply(&mut self, prompt: &str, reply: String, cx: &mut Context<Self>) {
        if let Some(session) = self.state.session_mut(AgentSession::GLOBAL_ASSISTANT_ID) {
            session.push_message(MessageRole::User, prompt);
            session.push_message(MessageRole::System, reply);
        }
        self.save(cx);
        cx.notify();
    }

    fn run_assistant_command(
        &mut self,
        command: AssistantCommand,
        prompt: &str,
        cx: &mut Context<Self>,
    ) {
        let reply = match command {
            AssistantCommand::Status => self.assistant_context(),
            AssistantCommand::Open(reference) => {
                match context::resolve_issue(&reference, &self.state.tasks) {
                    Ok(id) => {
                        self.open_task_from_card(id, cx);
                        tr!("assistant.issue_opened")
                    }
                    Err(error) => error,
                }
            }
            command => {
                self.run_assistant_workflow_command(command, cx);
                return;
            }
        };
        self.assistant_local_reply(prompt, reply, cx);
    }

    fn choose_assistant_model(
        &mut self,
        provider: ProviderKind,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.assistant.creating || !self.provider_enabled(provider) {
            return;
        }
        if let Some(session) = self.assistant_session() {
            if session.is_busy() || (!session.turns.is_empty() && session.provider != provider) {
                return;
            }
            let provider_changed = session.provider != provider;
            let session = self
                .state
                .session_mut(AgentSession::GLOBAL_ASSISTANT_ID)
                .expect("checked above");
            session.provider = provider;
            session.model = model;
            session.reasoning_effort = None;
            session.service_tier = None;
            session.context_window = None;
            session.touch();
            if provider_changed {
                self.reset_session_runtime(AgentSession::GLOBAL_ASSISTANT_ID);
            } else {
                self.apply_session_options(AgentSession::GLOBAL_ASSISTANT_ID, cx);
            }
            self.save(cx);
        } else {
            self.assistant.provider_choice = Some((provider, model));
            self.create_assistant_session(String::new(), cx);
        }
        cx.notify();
    }

    fn render_assistant_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.assistant_session();
        let provider = session
            .map(|session| session.provider)
            .unwrap_or(self.state.last_provider);
        let model = session.and_then(|session| self.model_for_session(session));
        let busy = self.assistant.creating || session.is_some_and(AgentSession::is_busy);
        let handle = self.menu_handle("assistant-model", cx);
        let locked = session
            .filter(|session| !session.turns.is_empty())
            .map(|session| session.provider);
        let choices = if handle.is_open() {
            self.probes
                .iter()
                .filter(|probe| {
                    self.provider_enabled(probe.provider)
                        && locked.is_none_or(|provider| provider == probe.provider)
                })
                .flat_map(|probe| {
                    std::iter::once((
                        probe.provider,
                        None,
                        format!(
                            "{} · {}",
                            probe.provider.display_name(),
                            tr!("assistant.provider_default")
                        ),
                    ))
                    .chain(probe.models.iter().map(|model| {
                        (
                            probe.provider,
                            Some(model.id.clone()),
                            format!("{} · {}", probe.provider.short_name(), model.name),
                        )
                    }))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let weak = cx.entity().downgrade();
        dropdown_menu(
            MenuChip::new("assistant-model")
                .label(self.model_display_name(provider, model))
                .caret(true)
                .selected(handle.is_open())
                .disabled(busy),
            "assistant-model-menu",
            &handle,
            MenuAlign::AboveLeft,
            move |_| {
                choices
                    .iter()
                    .map(|(provider, model, label)| {
                        let weak = weak.clone();
                        let provider = *provider;
                        let model = model.clone();
                        MenuItem::new(label.clone(), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.choose_assistant_model(provider, model.clone(), cx)
                            });
                        })
                    })
                    .collect()
            },
        )
        .into_any_element()
    }

    fn render_assistant_message(
        &self,
        index: usize,
        body_window: Option<MessageBodyWindow>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(message) = self
            .assistant_session()
            .and_then(|session| session.messages.get(index))
        else {
            return div().into_any_element();
        };
        let theme = Theme::current(cx);
        let palette = MarkdownPalette::from_theme(&theme);
        let mut cache = self.assistant.markdown.borrow_mut();
        let view = cache.entry(message.id).or_insert_with(MarkdownView::new);
        view.set_text(message.visible_content(), message.streaming);
        view.set_render_width(self.assistant.layout.0);
        let markdown = MarkdownCtx::new(
            format!("global-assistant-{}", message.id),
            &palette,
            self.scaled_markdown_metrics(MarkdownMetrics::COMPACT),
            self.assistant.selection.clone(),
        )
        .with_math_enabled(self.state.render_math)
        // The panel follows the existing stream-commit cadence without a
        // second decorative wake loop or partially dissolved text at rest.
        .with_streaming_animation(false)
        .with_link_handler(self.markdown_link_handler.clone());
        let body = if let Some(body_window) = body_window.filter(|_| message.streaming) {
            md::render::markdown_windowed(view, &markdown, body_window)
        } else {
            md::render::markdown(view, &markdown)
        };
        div()
            .id(SharedString::from(format!(
                "assistant-message-{}",
                message.id
            )))
            .px(px(14.0))
            .py(px(10.0))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_tertiary)
                    .child(match message.role {
                        MessageRole::User => tr!("assistant.you"),
                        MessageRole::Assistant => tr!("assistant.title"),
                        MessageRole::System => tr!("assistant.local_result"),
                    }),
            )
            .children(body)
            .when_some(message.undelivered_reason.as_ref(), |row, reason| {
                row.child(
                    div()
                        .text_size(sp(11.0))
                        .text_color(theme.warning)
                        .child(reason.clone()),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_assistant_panel(
        &mut self,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let workflow_controls = self.render_assistant_workflow_controls(window, cx);
        let selection = self.assistant.selection.clone();
        let selection_input = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                md::render::install_selection_input(window, &selection);
            },
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0));
        let theme = Theme::current(cx);
        let (count, busy, loaded) = self
            .assistant_session()
            .map(|session| {
                (
                    session.messages.len(),
                    session.is_busy(),
                    session.detail_loaded,
                )
            })
            .unwrap_or((0, false, true));
        let layout = (
            (width - 29.0).max(0.0),
            self.state.ui_font_size,
            self.state.code_font_size,
        );
        let layout_changed = self.assistant.layout != layout;
        if layout_changed {
            self.assistant.layout = layout;
            self.assistant.rows.remeasure();
        }
        // ListState is mutably borrowed during row building. Capture bounds
        // before entering list(), and before a stream commit invalidates its
        // row measurement. The Markdown ledger handles appends and reflow.
        let streaming_window = (!layout_changed)
            .then(|| {
                let session = self.assistant_session()?;
                let index = session.messages.len().checked_sub(1)?;
                if !session.messages[index].streaming {
                    return None;
                }
                let viewport = self.assistant.rows.viewport_bounds();
                let bounds = self.assistant.rows.bounds_for_item(index)?;
                Some((
                    index,
                    MessageBodyWindow {
                        visible_top: f32::from(viewport.top() - bounds.top()),
                        visible_height: f32::from(viewport.size.height),
                    },
                ))
            })
            .flatten();
        if count > self.assistant.row_count {
            self.assistant.rows.splice(
                self.assistant.row_count..self.assistant.row_count,
                count - self.assistant.row_count,
            );
        } else if count < self.assistant.row_count {
            self.assistant.rows.reset(count);
        }
        let tail_signature = self.assistant_session().and_then(|session| {
            session.messages.last().map(|message| {
                (
                    message.id,
                    message.visible_content().len(),
                    message.streaming,
                )
            })
        });
        if tail_signature != self.assistant.tail_signature && count > 0 {
            self.assistant.rows.remeasure_items(count - 1..count);
        }
        self.assistant.tail_signature = tail_signature;
        self.assistant.row_count = count;
        let waiting = self
            .runtimes
            .get(&AgentSession::GLOBAL_ASSISTANT_ID)
            .is_some_and(|runtime| {
                runtime.pending_permission.is_some()
                    || runtime.pending_user_input.is_some()
                    || runtime.pending_computer_approval.is_some()
            });
        let (project_id, session_id, task_id) = self.assistant_context_selection();
        let selected = task_id
            .or_else(|| {
                session_id
                    .and_then(|id| self.session(id))
                    .and_then(|session| session.task_id)
            })
            .and_then(|id| self.state.tasks.iter().find(|task| task.id == id))
            .map(|task| task.title.clone())
            .or_else(|| {
                project_id
                    .and_then(|id| self.state.projects.iter().find(|project| project.id == id))
                    .map(|project| project.name.clone())
            })
            .unwrap_or_else(|| tr!("assistant.all_projects"));
        let input = self.assistant.input.clone();
        let header = div()
            .h(px(48.0))
            .flex_none()
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .child(icon("icons/sparkle.svg", 14.0, theme.text_secondary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(sp(13.0))
                    .child(tr!("assistant.title")),
            )
            .child(self.task_icon_action(
                "assistant-status",
                tr!("assistant.status"),
                "icons/info.svg",
                !busy && !self.assistant.creating,
                |this, _, cx| this.submit_assistant("/status".to_owned(), cx),
                cx,
            ))
            .child(self.task_icon_action(
                "assistant-tools",
                tr!("right_panel.toggle"),
                "icons/panel-right.svg",
                true,
                |this, window, cx| {
                    this.set_assistant_open(false, window, cx);
                    this.set_right_panel_visible(true, cx);
                },
                cx,
            ))
            .child(self.task_icon_action(
                "assistant-close",
                tr!("assistant.close"),
                "icons/x.svg",
                true,
                |this, window, cx| this.set_assistant_open(false, window, cx),
                cx,
            ))
            .children(self.render_client_window_controls(
                super::window_chrome::WindowControlSide::Right,
                window,
                cx,
            ));
        let rows = list(
            self.assistant.rows.clone(),
            cx.processor(move |this, index, _, cx| {
                let body_window = streaming_window
                    .filter(|(row, _)| *row == index)
                    .map(|(_, window)| window);
                this.render_assistant_message(index, body_window, cx)
            }),
        )
        .flex_1()
        .min_h_0()
        .w_full();
        div()
            .id("global-assistant-panel")
            .relative()
            .on_action(cx.listener(|this, _: &crate::CancelTurn, window, cx| {
                this.set_assistant_open(false, window, cx);
                cx.stop_propagation();
            }))
            .w(px(width))
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(selection_input)
            .child(header)
            .child(
                div()
                    .flex_none()
                    .px(px(14.0))
                    .py(px(7.0))
                    .text_size(sp(11.0))
                    .text_color(theme.text_tertiary)
                    .min_w_0()
                    .truncate()
                    .child(selected),
            )
            .children(workflow_controls)
            .when(count == 0, |panel| {
                panel.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .px(px(18.0))
                        .py(px(20.0))
                        .text_size(sp(13.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("assistant.empty")),
                )
            })
            .when(count > 0, |panel| {
                panel.child(
                    div()
                        .id("assistant-transcript")
                        .track_focus(&self.assistant.transcript_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                window.focus(&this.assistant.transcript_focus, cx);
                            }),
                        )
                        .child(md::render::frame_reset(self.assistant.selection.clone()))
                        .child(rows),
                )
            })
            .when(!loaded || self.assistant.creating || busy, |panel| {
                panel.child(
                    div()
                        .flex_none()
                        .px(px(14.0))
                        .py(px(6.0))
                        .text_size(sp(11.0))
                        .text_color(theme.text_tertiary)
                        .child(if waiting {
                            tr!("assistant.needs_attention")
                        } else {
                            tr!("assistant.working")
                        }),
                )
            })
            .when(waiting, |panel| {
                panel.child(self.task_action_button(
                    "assistant-review-request",
                    tr!("assistant.review_request"),
                    false,
                    true,
                    |this, _, cx| {
                        this.select_session(AgentSession::GLOBAL_ASSISTANT_ID, cx);
                    },
                    cx,
                ))
            })
            .when_some(self.assistant.notice.clone(), |panel, notice| {
                panel.child(
                    div()
                        .id("assistant-notice")
                        .flex_none()
                        .max_h(px(70.0))
                        .overflow_y_scroll()
                        .px(px(14.0))
                        .py(px(6.0))
                        .text_size(sp(12.0))
                        .text_color(theme.warning)
                        .child(notice),
                )
            })
            .child(
                div()
                    .flex_none()
                    .m(px(10.0))
                    .p(px(9.0))
                    .min_w_0()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("assistant-composer-scroll")
                            .min_w_0()
                            .max_h(px(150.0))
                            .overflow_y_scroll()
                            .child(input),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(self.render_assistant_model_picker(cx)),
                            )
                            .child(if busy {
                                self.task_icon_action(
                                    "assistant-stop",
                                    tr!("assistant.stop"),
                                    "icons/stop.svg",
                                    !self
                                        .submission_preparations
                                        .contains(&AgentSession::GLOBAL_ASSISTANT_ID),
                                    |this, _, cx| {
                                        this.stop_session_turn(
                                            AgentSession::GLOBAL_ASSISTANT_ID,
                                            cx,
                                        )
                                    },
                                    cx,
                                )
                            } else {
                                self.task_icon_action(
                                    "assistant-send",
                                    tr!("assistant.send"),
                                    "icons/arrow-up.svg",
                                    !self.assistant.creating && loaded,
                                    |this, _, cx| {
                                        let text =
                                            this.assistant.input.read(cx).content().to_owned();
                                        this.submit_assistant(text, cx);
                                    },
                                    cx,
                                )
                            }),
                    ),
            )
            .child(self.render_panel_resize_handle(
                "assistant-resize-handle",
                PanelResizeTarget::RightPanel,
                cx,
            ))
    }
}
