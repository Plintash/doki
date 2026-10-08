#![recursion_limit = "256"]

rust_i18n::i18n!("locales", fallback = "en");

// rust-i18n expands locale data in a proc macro, which Cargo does not always
// discover as an input when only a YAML file changes. Keep explicit source
// dependencies so the watcher rebuilds the translation registry itself.
const _LOCALE_SOURCES: [&str; 3] = [
    include_str!("../locales/app.yml"),
    include_str!("../locales/zh-CN.yml"),
    include_str!("../locales/ja.yml"),
];

macro_rules! tr {
    ($key:expr) => {
        crate::i18n::translate($key)
    };
    ($key:expr, $($args:tt)*) => {
        rust_i18n::t!($key, $($args)*).into_owned()
    };
}

/// Borrow static translations on hot render paths; interpolation uses `tr!`
/// because formatted messages necessarily allocate.
macro_rules! tr_cow {
    ($key:literal) => {
        rust_i18n::t!($key)
    };
}

mod app;
mod assets;
mod browser;
mod computer_use;
pub mod daemon;
mod driver;
mod input;
pub mod latency;
mod main_window;
mod md;
mod platform;
mod query;
mod review_diff;
mod startup_trace;
mod terminal;
mod theme;
mod ui;
mod updater;

pub use waku_client::{
    checkpoint, command_env, composer_complete, git_branch, git_commit, i18n, identity, model,
    model_catalog, persistence, projectless, skills, usage, usage_history, worktree,
};

// The environment variables the latency harness sets on the app it launches.
pub use crate::startup_trace::{CLOSE_AFTER_LAUNCH_ENV, TRACE_ENV};

use gpui::{App, KeyBinding, Menu, MenuItem, actions};

use crate::identity::{APP_ID, APP_NAME};
use crate::main_window::WakuApplicationExt as _;
actions!(
    waku,
    [
        Quit,
        About,
        CloseWindow,
        NewSession,
        NewProject,
        OpenSettings,
        CheckForUpdates,
        ToggleSidebar,
        ToggleRightPanel,
        ToggleCommandPalette,
        OpenResumePicker,
        ToggleFpsCounter,
        NavigateBack,
        NavigateForward,
        SwitchTaskForward,
        SwitchTaskBackward,
        SelectFirstTask,
        SelectLastTask,
        ConfirmTaskSwitch,
        CancelTaskSwitch,
        FocusComposer,
        ToggleModelPicker,
        ToggleUsagePanel,
        SaveFile,
        CancelTurn,
        CopySelection,
        AnnotateSelection,
        DismissAnnotationEditor,
        OpenFind,
        OpenFindReplace,
        CloseFind,
        FindNext,
        FindPrevious,
        ToggleFindCaseSensitive,
        ToggleFindWholeWord,
        ToggleFindRegex,
        ReplaceAllMatches,
        BrowserBack,
        BrowserForward,
        BrowserReload,
        BrowserHardReload,
        BrowserStop,
        BrowserDevtools,
        FocusBrowserAddress,
        BrowserAddressCancel,
        WebviewCopy,
        WebviewCut,
        WebviewPaste,
        WebviewSelectAll,
        FocusNext,
        FocusPrev
    ]
);

pub fn run() {
    // The earliest point the app can measure from; every trace milestone is
    // relative to it.
    crate::startup_trace::mark_process_start();
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .with_main_window_reopen()
        .run(|cx: &mut App| {
            // Linux uses this for Wayland app_id/X11 WM_CLASS and notification
            // attribution. Other platforms also benefit from one stable
            // process identity.
            cx.set_app_identity(APP_ID, APP_NAME);
            crate::assets::register_fonts(cx).expect("failed to register bundled fonts");
            crate::input::init(cx);
            crate::ui::menu::init(cx);
            crate::app::init_composer_autocomplete(cx);
            crate::app::init_settings_keys(cx);
            crate::app::init_command_palette(cx);
            crate::app::init_commit_dialog_keys(cx);
            crate::app::init_goal_dialog_keys(cx);
            crate::app::init_image_preview_keys(cx);
            crate::app::init_sidebar_keys(cx);
            crate::app::init_skills_keys(cx);
            crate::app::init_window_close_keys(cx);
            crate::theme::init(cx);
            crate::platform::init_reduce_motion(cx);

            // Platform updaters only run from a supported release layout (or
            // when explicitly forced for development); everywhere else the
            // menu item is omitted along with the updater itself.
            let updater = crate::updater::Updater::init();
            let updater_available = updater.is_some();
            cx.set_global(crate::updater::UpdaterState(updater));
            // The daemon is app-scoped because it outlives any window, but it
            // is not connected yet: the window opens and paints first, then
            // asks for it.
            cx.set_global(crate::daemon::DaemonState::Idle);
            // Off unless `WAKU_STARTUP_TRACE` asks for it. Application scope
            // so a rebuilt window joins the run the process already started.
            cx.set_global(crate::startup_trace::StartupTrace::from_env());
            cx.on_action(|_: &CheckForUpdates, cx| {
                if let Some(updater) = &cx.global::<crate::updater::UpdaterState>().0 {
                    updater.check_for_updates();
                }
            });
            cx.on_action(|_: &About, _| crate::platform::show_about_panel());

            cx.bind_keys([
                // `secondary` is Command on macOS and Control elsewhere.
                KeyBinding::new("secondary-q", Quit, None),
                KeyBinding::new("secondary-w", CloseWindow, None),
                KeyBinding::new("secondary-n", NewSession, None),
                KeyBinding::new("secondary-o", NewProject, None),
                KeyBinding::new("secondary-,", OpenSettings, None),
                KeyBinding::new("secondary-b", ToggleSidebar, None),
                KeyBinding::new("secondary-shift-b", ToggleRightPanel, None),
                KeyBinding::new("secondary-k", ToggleCommandPalette, None),
                KeyBinding::new("secondary-alt-shift-f", ToggleFpsCounter, None),
                KeyBinding::new("secondary-[", NavigateBack, Some("Waku")),
                KeyBinding::new("secondary-]", NavigateForward, Some("Waku")),
                KeyBinding::new("ctrl-tab", SwitchTaskForward, Some("Waku")),
                KeyBinding::new("ctrl-shift-tab", SwitchTaskBackward, Some("Waku")),
                KeyBinding::new("ctrl-escape", CancelTaskSwitch, Some("Waku")),
                KeyBinding::new("ctrl-shift-escape", CancelTaskSwitch, Some("Waku")),
                KeyBinding::new("down", SwitchTaskForward, Some("TaskSwitcher")),
                KeyBinding::new("right", SwitchTaskForward, Some("TaskSwitcher")),
                KeyBinding::new("up", SwitchTaskBackward, Some("TaskSwitcher")),
                KeyBinding::new("left", SwitchTaskBackward, Some("TaskSwitcher")),
                KeyBinding::new("home", SelectFirstTask, Some("TaskSwitcher")),
                KeyBinding::new("end", SelectLastTask, Some("TaskSwitcher")),
                KeyBinding::new("enter", ConfirmTaskSwitch, Some("TaskSwitcher")),
                KeyBinding::new("escape", CancelTaskSwitch, Some("TaskSwitcher")),
                KeyBinding::new("secondary-l", FocusComposer, None),
                KeyBinding::new("secondary-/", ToggleModelPicker, None),
                KeyBinding::new("secondary-u", ToggleUsagePanel, None),
                KeyBinding::new("secondary-s", SaveFile, None),
                KeyBinding::new("escape", CancelTurn, Some("Waku")),
                // GPUI's focus_next/focus_prev are not bound by default, so Tab
                // does nothing without this. Zed binds the same pair.
                KeyBinding::new("tab", FocusNext, Some("Waku")),
                KeyBinding::new("shift-tab", FocusPrev, Some("Waku")),
                // Escape inside the annotation comment field or editor closes
                // the overlay before it can cancel the turn.
                KeyBinding::new("escape", DismissAnnotationEditor, Some("AnnotationEditor")),
                KeyBinding::new("secondary-c", CopySelection, Some("Waku")),
                // The transcript note action. It shares the composer's focus
                // fallback with copy, so an active selection anywhere in the
                // transcript is enough to annotate it.
                KeyBinding::new("secondary-alt-a", AnnotateSelection, Some("Waku")),
                // Find and replace in the right panel's file editor, on the
                // conventional VS Code bindings. The primary shortcut + G cycles matches from
                // the editor without moving focus to the bar.
                KeyBinding::new("secondary-f", OpenFind, Some("Waku")),
                // The text input's macOS-style Ctrl-F caret binding is more
                // specific than Waku's root context. Reassert the platform
                // primary shortcut for inputs inside this window so Ctrl-F
                // remains find-in-page on Linux/Windows while Cmd-F keeps the
                // native behavior on macOS.
                KeyBinding::new("secondary-f", OpenFind, Some("Waku > TextInput")),
                KeyBinding::new("secondary-alt-f", OpenFindReplace, Some("Waku")),
                KeyBinding::new("secondary-g", FindNext, Some("Waku")),
                KeyBinding::new("secondary-shift-g", FindPrevious, Some("Waku")),
                // Scoped to the editor pane: escape closes the bar there and
                // falls through to CancelTurn anywhere else.
                KeyBinding::new("escape", CloseFind, Some("FileEditorPane")),
                KeyBinding::new("escape", CloseFind, Some("FindBar")),
                KeyBinding::new(
                    "secondary-alt-c",
                    ToggleFindCaseSensitive,
                    Some("FileEditorPane"),
                ),
                KeyBinding::new(
                    "secondary-alt-w",
                    ToggleFindWholeWord,
                    Some("FileEditorPane"),
                ),
                KeyBinding::new("secondary-alt-r", ToggleFindRegex, Some("FileEditorPane")),
                KeyBinding::new("shift-enter", FindPrevious, Some("FindBar")),
                KeyBinding::new("secondary-alt-enter", ReplaceAllMatches, Some("FindBar")),
                // Browser surface. Deeper than "Waku", so while focus is on the
                // page or its address bar the browser reads the platform's
                // conventional navigation shortcuts; the same keys elsewhere
                // keep their app meanings. The clipboard trio is rebound
                // because GPUI's window view claims key equivalents before
                // AppKit can walk the responder chain into the webview.
                KeyBinding::new("secondary-l", FocusBrowserAddress, Some("Browser")),
                KeyBinding::new("secondary-r", BrowserReload, Some("Browser")),
                KeyBinding::new("secondary-shift-r", BrowserHardReload, Some("Browser")),
                KeyBinding::new("secondary-[", BrowserBack, Some("Browser")),
                KeyBinding::new("secondary-]", BrowserForward, Some("Browser")),
                KeyBinding::new("escape", BrowserStop, Some("Browser")),
                KeyBinding::new("secondary-alt-i", BrowserDevtools, Some("Browser")),
                KeyBinding::new("secondary-c", WebviewCopy, Some("Browser")),
                KeyBinding::new("secondary-x", WebviewCut, Some("Browser")),
                KeyBinding::new("secondary-v", WebviewPaste, Some("Browser")),
                KeyBinding::new("secondary-a", WebviewSelectAll, Some("Browser")),
                KeyBinding::new("escape", BrowserAddressCancel, Some("BrowserAddress")),
            ]);

            cx.on_action(|_: &Quit, cx| cx.quit());

            // Unlike AppKit, Linux has no Dock activation path that can
            // restore a hidden last window. Follow Zed's GPUI precedent and
            // terminate when the final window closes.
            #[cfg(not(target_os = "macos"))]
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            crate::main_window::open_main_window(cx);
            // Registered once, at application scope, so a notification click
            // still lands after the window that first handled one is gone.
            crate::main_window::init_notification_activation(cx);

            set_app_menus(cx, updater_available);
            // A Linux handoff retains the previous prefix until this freshly
            // relaunched build has successfully opened its main window.
            crate::updater::signal_relaunch_ready();
        });
}

/// Rebuild the native menu bar in the active locale. GPUI menus own their
/// labels, so changing language must replace the model as well as redraw the
/// window.
pub(crate) fn set_app_menus(cx: &mut App, updater_available: bool) {
    cx.set_menus(vec![
        Menu {
            name: APP_NAME.into(),
            disabled: false,
            items: {
                let mut items = vec![MenuItem::action(tr!("menu.about", app = APP_NAME), About)];
                if updater_available {
                    items.push(MenuItem::action(
                        tr!("menu.check_for_updates"),
                        CheckForUpdates,
                    ));
                }
                items.push(MenuItem::separator());
                items.extend([
                    MenuItem::action(tr!("menu.settings"), OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action(tr!("menu.quit", app = APP_NAME), Quit),
                ]);
                items
            },
        },
        Menu {
            name: tr!("menu.file").into(),
            disabled: false,
            items: vec![
                MenuItem::action(tr!("menu.new_task"), NewSession),
                MenuItem::action(tr!("menu.new_project"), NewProject),
            ],
        },
        Menu {
            name: tr!("menu.edit").into(),
            disabled: false,
            items: vec![
                MenuItem::action(tr!("menu.undo"), input::Undo),
                MenuItem::action(tr!("menu.redo"), input::Redo),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.cut"), input::Cut),
                MenuItem::action(tr!("menu.copy"), input::Copy),
                MenuItem::action(tr!("menu.paste"), input::Paste),
                MenuItem::action(tr!("menu.select_all"), input::SelectAll),
            ],
        },
        Menu {
            name: tr!("menu.view").into(),
            disabled: false,
            items: vec![
                MenuItem::action(tr!("menu.command_palette"), ToggleCommandPalette),
                MenuItem::separator(),
                MenuItem::action(tr!("menu.toggle_sidebar"), ToggleSidebar),
                MenuItem::action(tr!("menu.toggle_right_panel"), ToggleRightPanel),
                MenuItem::action(tr!("menu.focus_composer"), FocusComposer),
                MenuItem::action(tr!("menu.toggle_model_picker"), ToggleModelPicker),
                MenuItem::action(tr!("menu.toggle_usage_panel"), ToggleUsagePanel),
            ],
        },
        Menu {
            name: tr!("menu.window").into(),
            disabled: false,
            items: vec![
                MenuItem::action(tr!("menu.toggle_fps_counter"), ToggleFpsCounter),
                MenuItem::action(tr!("menu.close_window"), CloseWindow),
            ],
        },
    ]);
}
