//! The unsaved-edits guard in front of window close.
//!
//! A window is disposable but its file editors are not: the buffers live in
//! this process and nowhere else. Closing a window that holds unsaved changes
//! therefore asks first — the confirmation is the window's last chance to say
//! no — and only a discard lets the window go. Cancelling leaves the window and
//! the buffers exactly as they were, and the buffers are never persisted, so a
//! discard loses them by design.
//!
//! Every close route asks [`Waku::request_window_close`]: AppKit's own close
//! control reaches it through the main window's `on_window_should_close` hook,
//! and the routes Waku closes itself — Cmd-W and a client-decorated window's
//! close control — call it before removing the window. The close-time frame
//! save lives in that same method, so the guard cannot take the save's place.

use gpui::{KeyBinding, actions};

use super::*;

actions!(waku_window_close, [ConfirmWindowClose, DismissWindowClose]);

/// The key context the confirmation's choices answer in. Escape reaches it
/// only from inside the confirmation, which is why raising one moves focus
/// into it.
const CONFIRMATION_CONTEXT: &str = "WindowCloseConfirmation";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", DismissWindowClose, Some(CONFIRMATION_CONTEXT)),
        KeyBinding::new(
            "secondary-enter",
            ConfirmWindowClose,
            Some(CONFIRMATION_CONTEXT),
        ),
    ]);
}

/// What a close request leaves the window doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WindowClose {
    /// The window may go. Whoever asked removes it: AppKit does that itself on
    /// the native close path, Waku's own routes call `platform::close_window`.
    Close,
    /// The window stays. The confirmation is up, or the user just cancelled
    /// out of it.
    Stay,
}

/// The confirmation standing between unsaved file editors and a window close.
#[derive(Default)]
pub(super) struct UnsavedEditsGuard {
    /// Set while the confirmation is on screen: the close is held until the
    /// user discards or cancels.
    open: bool,
    /// What held focus when the confirmation opened, so cancelling puts the
    /// user back where they were rather than at the top of the window.
    previous_focus: Option<FocusHandle>,
}

impl UnsavedEditsGuard {
    /// Answer a close request with the workspace's answer about its editors:
    /// unsaved ones raise the confirmation instead of the close.
    pub(super) fn request(&mut self, unsaved_edits: bool) -> WindowClose {
        if !unsaved_edits {
            return WindowClose::Close;
        }
        self.open = true;
        WindowClose::Stay
    }

    /// The user chose to keep the window: the confirmation comes down, and the
    /// buffers are exactly as they were.
    pub(super) fn cancel(&mut self) -> WindowClose {
        self.open = false;
        WindowClose::Stay
    }

    /// The user chose to discard the unsaved edits: the window may go. Nothing
    /// is written on the way out.
    pub(super) fn discard(&mut self) -> WindowClose {
        self.open = false;
        WindowClose::Close
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    fn remember_focus(&mut self, focus: Option<FocusHandle>) {
        self.previous_focus = focus;
    }

    fn previous_focus(&self) -> Option<FocusHandle> {
        self.previous_focus.clone()
    }
}

/// Whether any file editor holds edits that were never saved.
///
/// Editors belong to their task: the ones the user switched away from wait in
/// their task's panel state, and closing the window discards those buffers
/// just as it discards the visible one's.
fn unsaved_edits(
    selected: &HashMap<String, RightPanelFileEditor>,
    other_tasks: &HashMap<Uuid, RightPanelSessionState>,
) -> bool {
    selected.values().any(|editor| editor.dirty)
        || other_tasks
            .values()
            .flat_map(|state| state.file_editors.values())
            .any(|editor| editor.dirty)
}

impl Waku {
    /// Whether a file editor holds edits that were never saved.
    pub(super) fn has_unsaved_edits(&self) -> bool {
        unsaved_edits(
            &self.right_panel_file_editors,
            &self.right_panel_session_states,
        )
    }

    /// Ask to close the window.
    ///
    /// Returns whether the window may go now. A file editor with unsaved
    /// changes holds the close and raises the confirmation instead; otherwise
    /// the desktop snapshot is landed first, because the application outlives
    /// the window and the quit-time save would never see the frame the user
    /// left it at. The guard and the save are one decision here, so no route
    /// can take the save's place.
    pub(crate) fn request_window_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // A second close request — the close control again, Cmd-W while the
        // confirmation is up — finds the same confirmation in the same state,
        // so nothing about it is raised twice.
        let already_asking = self.unsaved_edits_guard.is_open();
        let decision = self.unsaved_edits_guard.request(self.has_unsaved_edits());
        if self.finish_window_close(decision, window, cx) {
            return true;
        }
        if !already_asking {
            self.raise_unsaved_edits_confirmation(window, cx);
        }
        false
    }

    /// Act on the guard's answer: land the desktop snapshot when the window may
    /// go, and report whether it may. The application outlives the window, so
    /// this is the last moment the frame it was left at is known; the quit-time
    /// save never sees it. Whoever asked removes the window through its own
    /// route — AppKit on the native close path, `platform::close_window` for
    /// Waku's own routes.
    fn finish_window_close(
        &mut self,
        decision: WindowClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if decision != WindowClose::Close {
            return false;
        }
        self.persist_window_state(window, cx);
        true
    }

    /// Close the window from a route Waku owns: Cmd-W, or the close control of
    /// a window GPUI decorates itself. AppKit removes the window on its own
    /// close path, so only these routes remove it here.
    pub(super) fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.request_window_close(window, cx) {
            crate::platform::close_window(window);
        }
    }

    /// Put the confirmation on screen and hand it the keyboard: while it is up
    /// the window cannot close, so both choices have to be reachable without a
    /// pointer.
    fn raise_unsaved_edits_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.unsaved_edits_guard.remember_focus(window.focused(cx));
        // A deferred surface joins the dispatch tree only after it has drawn,
        // so the confirmation takes focus once it is on screen.
        let cancel_focus = self.window_close_cancel_focus.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&cancel_focus, cx));
        });
        cx.notify();
    }

    /// The user chose to keep the window: nothing is discarded, and focus
    /// returns to where it was.
    pub(super) fn cancel_window_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unsaved_edits_guard.cancel() == WindowClose::Stay {
            let focus = self
                .unsaved_edits_guard
                .previous_focus()
                .unwrap_or_else(|| self.composer_focus(cx));
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// The user chose to discard the unsaved edits and close: the buffers go
    /// with the window and are never written.
    pub(super) fn discard_unsaved_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let decision = self.unsaved_edits_guard.discard();
        if self.finish_window_close(decision, window, cx) {
            crate::platform::close_window(window);
        }
    }

    /// The confirmation a close request raised.
    ///
    /// Both of the window's surfaces draw it: the workspace and the settings
    /// page can each be on screen with a dirty editor behind them, and a
    /// confirmation that only one of them drew would hold a window that could
    /// not be closed.
    pub(super) fn render_window_close_confirmation(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.unsaved_edits_guard.is_open() {
            return None;
        }
        let theme = Theme::current(cx);
        let weak = cx.entity().downgrade();
        let discard = window_close_choice(
            WindowCloseChoice {
                id: "window-close-discard",
                focus: self.window_close_discard_focus.clone(),
                icon: "icons/trash.svg",
                label: tr!("window_close.discard"),
                tint: theme.danger,
            },
            weak.clone(),
            &theme,
            |waku, window, cx| waku.discard_unsaved_edits(window, cx),
        );
        let cancel = window_close_choice(
            WindowCloseChoice {
                id: "window-close-cancel",
                focus: self.window_close_cancel_focus.clone(),
                icon: "icons/x.svg",
                label: tr!("common.cancel"),
                tint: theme.text,
            },
            weak,
            &theme,
            |waku, window, cx| waku.cancel_window_close(window, cx),
        );
        let card = div()
            .id("window-close-card")
            .key_context(CONFIRMATION_CONTEXT)
            .on_action(cx.listener(|waku, _: &ConfirmWindowClose, window, cx| {
                waku.discard_unsaved_edits(window, cx);
            }))
            .on_action(cx.listener(|waku, _: &DismissWindowClose, window, cx| {
                waku.cancel_window_close(window, cx);
            }))
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(420.0))
            .overflow_hidden()
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/alert.svg", 15.0, theme.warning))
                    .child(tr!("window_close.title")),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .text_size(sp(14.0))
                    .line_height(sp(21.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("window_close.body")),
            )
            .child(div().mx(px(8.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(discard)
                    .child(cancel),
            );

        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("window-close-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            // A click outside the card is the cancel path: taking the window
            // away is never what an accidental click means.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|waku, _, window, cx| waku.cancel_window_close(window, cx)),
            )
            .child(card);
        // Above every other surface, including the task switcher: the window
        // cannot close until this one is answered.
        Some(gpui::deferred(layer).with_priority(7).into_any_element())
    }
}

/// One choice in the confirmation, mouse and keyboard alike.
struct WindowCloseChoice {
    id: &'static str,
    focus: FocusHandle,
    icon: &'static str,
    label: String,
    tint: Hsla,
}

/// Render one confirmation choice as an activatable row.
fn window_close_choice(
    choice: WindowCloseChoice,
    weak: WeakEntity<Waku>,
    theme: &Theme,
    activate: impl Fn(&mut Waku, &mut Window, &mut Context<Waku>) + Clone + 'static,
) -> Stateful<Div> {
    let WindowCloseChoice {
        id,
        focus,
        icon: icon_path,
        label,
        tint,
    } = choice;
    let click_activate = activate.clone();
    let click_weak = weak.clone();
    let key_weak = weak;
    div()
        .id(id)
        .track_focus(&focus)
        .tab_index(0)
        .h(px(38.0))
        .w_full()
        .px(px(10.0))
        .rounded(px(9.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .cursor_default()
        .text_size(sp(14.0))
        .text_color(tint)
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .hover(|style| style.bg(theme.overlay_strong))
        .child(icon(icon_path, 15.0, tint))
        .child(div().min_w_0().flex_1().truncate().child(label))
        .on_click(move |_, window, cx| {
            let _ = click_weak.update(cx, |waku, cx| click_activate(waku, window, cx));
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                let _ = key_weak.update(cx, |waku, cx| activate(waku, window, cx));
                cx.stop_propagation();
            }
        })
}

#[cfg(test)]
mod tests {
    use gpui::{Render, TestAppContext};

    use super::*;

    /// A window the test can hang an editor entity on. The scan reads editor
    /// flags, so nothing here has to render.
    struct EditorProbe;

    impl Render for EditorProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn file_editor(input: Entity<TextInput>, dirty: bool) -> RightPanelFileEditor {
        RightPanelFileEditor {
            state: input,
            disk_content: String::new(),
            writable: true,
            dirty,
            reading: false,
            read_epoch: 0,
        }
    }

    fn editor_fixture(cx: &mut TestAppContext) -> Entity<TextInput> {
        let (_, cx) = cx.add_window_view(|_, _| EditorProbe);
        cx.update(|window, cx| cx.new(|cx| TextInput::new(window, cx)))
    }

    #[test]
    fn a_close_request_with_clean_editors_does_not_ask() {
        let mut guard = UnsavedEditsGuard::default();

        assert_eq!(
            guard.request(false),
            WindowClose::Close,
            "nothing unsaved closes without a confirmation"
        );
        assert!(!guard.is_open());
    }

    #[test]
    fn a_close_request_with_unsaved_edits_asks_before_the_window_goes() {
        let mut guard = UnsavedEditsGuard::default();

        assert_eq!(
            guard.request(true),
            WindowClose::Stay,
            "unsaved edits hold the window until the user chooses"
        );
        assert!(guard.is_open(), "the confirmation is what holds it");
    }

    #[test]
    fn cancelling_the_confirmation_keeps_the_window() {
        let mut guard = UnsavedEditsGuard::default();
        guard.request(true);

        assert_eq!(guard.cancel(), WindowClose::Stay);
        assert!(
            !guard.is_open(),
            "the confirmation is gone and the window is the user's again"
        );
    }

    #[test]
    fn discarding_the_confirmation_lets_the_window_go() {
        let mut guard = UnsavedEditsGuard::default();
        guard.request(true);

        assert_eq!(guard.discard(), WindowClose::Close);
        assert!(!guard.is_open());
    }

    /// Editors belong to their task, so a task the user switched away from
    /// holds its unsaved buffers in that task's panel state. Closing the
    /// window discards those too, so they block the close like the visible
    /// one's.
    #[gpui::test]
    fn unsaved_editors_block_the_close_from_every_task(cx: &mut TestAppContext) {
        let input = editor_fixture(cx);
        let dirty = |path: &str| (path.to_owned(), file_editor(input.clone(), true));
        let clean = |path: &str| (path.to_owned(), file_editor(input.clone(), false));

        let mut selected = HashMap::new();
        let mut other_tasks = HashMap::new();
        assert!(
            !unsaved_edits(&selected, &other_tasks),
            "a window with no editor open has nothing to lose"
        );

        selected.extend([dirty("src/main.rs")]);
        assert!(
            unsaved_edits(&selected, &other_tasks),
            "the selected task's own unsaved editor blocks the close"
        );

        selected.clear();
        let mut stashed = RightPanelSessionState::empty(false);
        stashed.file_editors.extend([dirty("src/lib.rs")]);
        other_tasks.insert(Uuid::new_v4(), stashed);
        assert!(
            unsaved_edits(&selected, &other_tasks),
            "an editor left unsaved in a task the user switched away from blocks the close too"
        );

        let mut switched_to = RightPanelSessionState::empty(true);
        switched_to.file_editors.extend([clean("src/lib.rs")]);
        other_tasks.clear();
        other_tasks.insert(Uuid::new_v4(), switched_to);
        assert!(
            !unsaved_edits(&selected, &other_tasks),
            "clean editors anywhere have nothing to lose"
        );
    }
}
