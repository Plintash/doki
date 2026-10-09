//! The desktop application's one main window.
//!
//! A window is disposable: first launch, Dock activation, and notification
//! activation all request the main window, and the application keeps at most
//! one. Every request goes through [`show_main_window`], so the single-window
//! rule lives in exactly one place, and the services the window borrows — the
//! daemon supervisor, the updater — are owned at application scope so a
//! rebuilt window reattaches to them instead of starting new ones.
//!
//! The window also opens before its daemon is ready. [`MainWindow`] paints
//! skeleton content on the first frame and swaps in the daemon-backed
//! workspace when the connection lands, so a daemon that is slow to start, or
//! never starts at all, leaves a window that reports itself rather than a
//! delayed or blank frame. The connection itself is the application's (see
//! `crate::daemon`), so closing the window while it is in flight neither drops
//! the supervisor nor starts a second one.

use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, Application, Bounds, Context, Entity, FontWeight,
    IntoElement, ParentElement, Render, Styled, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions, div, point, px, size,
};
use uuid::Uuid;

use crate::app::Waku;
use crate::app::window_chrome::render_window_frame;
use crate::daemon::{DaemonConnector, DaemonState};
use crate::identity::APP_ID;
use crate::latency::{Milestone, RunKind};
use crate::persistence::AppSettings;
use crate::startup_trace;
use crate::theme::{Theme, sp};
use crate::ui::icon;

const DEFAULT_WINDOW_WIDTH: f32 = 1380.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 880.0;
const MIN_WINDOW_WIDTH: f32 = 980.0;
const MIN_WINDOW_HEIGHT: f32 = 680.0;
/// How much titlebar must stay on the display for the window to be dragged
/// back by hand.
const TITLEBAR_GRAB_WIDTH: f32 = 160.0;
const TITLEBAR_GRAB_HEIGHT: f32 = 22.0;

/// How long a traced cold launch waits after closing its window before it
/// rebuilds it. Long enough for the removal to land, and not counted in the
/// rebuild's own latency, which starts when the opener runs.
const REBUILD_DELAY_MS: u64 = 300;

/// Show the main window: focus the one the application already has, or open it
/// with `options`.
///
/// The root view type identifies which window is the main one, so the guard
/// needs no extra bookkeeping and `build` runs only on the opening path.
pub fn show_main_window<V: 'static + Render>(
    cx: &mut App,
    options: WindowOptions,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> WindowHandle<V> {
    let window = match cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<V>())
    {
        Some(window) => {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            window
        }
        None => {
            // Opening, rather than focusing, is what starts a traced run: the
            // first window continues the cold launch, a later one is a
            // rebuild measured from this moment.
            startup_trace::note_window_open(cx);
            cx.open_window(options, build)
                .expect("failed to open Waku window")
        }
    };
    cx.activate(true);
    window
}

/// The main window's root view.
///
/// It opens with no workspace at all and paints skeleton content, then swaps in
/// the daemon-backed workspace when the application's connection lands. A
/// daemon that never answers is reported in the same window instead of leaving
/// a blank frame.
pub struct MainWindow {
    /// What the window shows. One state rather than a workspace beside a
    /// status flag, so "connecting", "failed", and "hydrated" cannot
    /// disagree with each other.
    content: WindowContent,
    /// The persisted preferences the window painted its first frame with.
    /// Hydration applies the same snapshot, so the workspace cannot switch the
    /// theme, language, or font size under a skeleton that already showed
    /// them.
    preferences: AppSettings,
    /// The task a notification asked for before the workspace existed. A
    /// notification click can beat a slow daemon, and the tag has to survive
    /// until there is a workspace to select the task in.
    requested_task: Option<Uuid>,
}

/// What the main window shows.
enum WindowContent {
    /// The daemon connection is in flight; the window paints skeleton content.
    Connecting,
    /// The daemon could not be reached: the window reports why.
    Failed(String),
    /// The daemon answered: its workspace, hydrated for this window.
    Workspace(Entity<Waku>),
}

impl MainWindow {
    /// A window with no workspace yet: it paints skeleton content while the
    /// daemon connection runs, using the persisted preferences it was opened
    /// with.
    fn connecting(preferences: AppSettings) -> Self {
        Self {
            content: WindowContent::Connecting,
            preferences,
            requested_task: None,
        }
    }

    /// Ask the application for its daemon and show whatever it has.
    ///
    /// The window is already on screen when this runs, so the request only
    /// schedules work: a daemon that is slow to start never delays the first
    /// frame. The request itself is the application's, so it outlives this
    /// window — closing while the daemon answers cannot drop the supervisor —
    /// and a window opened while one is in flight joins it.
    fn connect(&mut self, window: &mut Window, connector: DaemonConnector, cx: &mut Context<Self>) {
        crate::daemon::request(cx, connector);
        self.sync_daemon_state(window, cx);
    }

    /// Show the application's daemon connection in this window: build the
    /// workspace once it is ready, and report a failed attempt. The connection
    /// is the application's; this window only renders it, and reattaches when
    /// it lands after the window opened.
    pub(crate) fn sync_daemon_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match crate::daemon::state(cx) {
            DaemonState::Ready(daemon) => {
                // A rebuild joins a daemon that is already connected; the
                // milestone is still recorded for this run, at the moment the
                // window saw it.
                startup_trace::record(cx, Milestone::DaemonReady);
                if !matches!(self.content, WindowContent::Workspace(_)) {
                    self.attach_workspace(daemon, window, cx);
                }
            }
            DaemonState::Failed(error) => {
                if !matches!(&self.content, WindowContent::Failed(current) if *current == error) {
                    self.content = WindowContent::Failed(error);
                    cx.notify();
                }
            }
            DaemonState::Idle | DaemonState::Connecting => {}
        }
    }

    /// Build the workspace for `daemon` and show it in place of the starting
    /// surface.
    fn attach_workspace(
        &mut self,
        daemon: waku_client::DaemonSupervisor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The connection is the application's, so the daemon was published at
        // application scope before this window could see it; the workspace
        // only borrows it.
        let workspace = Waku::new(window, cx, daemon, self.preferences.clone());
        startup_trace::record(cx, Milestone::TasksHydrated);
        let composer_focus = workspace.read(cx).composer_focus(cx);
        window.focus(&composer_focus, cx);
        crate::platform::configure_sidebar_material(window, Theme::current(cx).is_dark);
        self.content = WindowContent::Workspace(workspace.clone());
        if let Some(task_id) = self.requested_task.take() {
            workspace.update(cx, |workspace, cx| {
                workspace.open_task_from_notification(task_id, cx)
            });
        }
        cx.notify();
    }

    /// Deliver a notification click's task, waiting for the workspace when the
    /// click beats the daemon.
    pub fn open_task_from_notification(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        match &self.content {
            WindowContent::Workspace(workspace) => workspace.update(cx, |workspace, cx| {
                workspace.open_task_from_notification(task_id, cx)
            }),
            WindowContent::Connecting | WindowContent::Failed(_) => {
                self.requested_task = Some(task_id);
            }
        }
    }

    /// AppKit's answer to the window's own close control. The workspace is the
    /// one that answers: it holds the close back — with a confirmation — while
    /// a file editor is unsaved, and lands the desktop snapshot when the
    /// window may go. A window with no workspace yet holds nothing unsaved and
    /// has nothing to save.
    fn window_should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match &self.content {
            WindowContent::Workspace(workspace) => workspace.update(cx, |workspace, cx| {
                workspace.request_window_close(window, cx)
            }),
            WindowContent::Connecting | WindowContent::Failed(_) => true,
        }
    }
}

/// Attach the application's daemon to every main window that is waiting for
/// one. A window opened after the connection landed attaches as it opens; this
/// reaches the windows that were already on screen when it landed.
pub(crate) fn attach_main_windows(cx: &mut App) {
    for window in cx.windows() {
        let Some(window) = window.downcast::<MainWindow>() else {
            continue;
        };
        window
            .update(cx, |root, window, cx| root.sync_daemon_state(window, cx))
            .ok();
    }
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The first render of a run is the first frame; the first render that
        // shows the hydrated workspace is the moment the window is usable.
        startup_trace::record(cx, Milestone::FirstFrame);
        if matches!(self.content, WindowContent::Workspace(_))
            && startup_trace::record(cx, Milestone::Interactive) == Some(RunKind::Cold)
            && startup_trace::close_after_launch(cx)
        {
            // Harness-only: let the cold run's line land, close the window, and
            // rebuild it through the same opener Dock activation uses, so the
            // harness can measure the rebuild. Driving AppKit's own reopen from
            // outside needs accessibility control, which a harness cannot rely
            // on.
            window.defer(cx, |window, cx| {
                window.remove_window();
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(REBUILD_DELAY_MS))
                        .await;
                    cx.update(open_main_window);
                })
                .detach();
            });
        }
        let content = match &self.content {
            WindowContent::Workspace(workspace) => workspace.clone().into_any_element(),
            WindowContent::Connecting => startup_surface(tr!("daemon.phase_connecting"), None, cx),
            WindowContent::Failed(error) => {
                startup_surface(tr!("daemon.phase_error"), Some(error.clone()), cx)
            }
        };
        render_window_frame(content, window, cx)
    }
}

/// The surface a window paints before its daemon-backed workspace exists: the
/// app's empty-state shape carrying the startup status, so a slow or
/// unreachable daemon shows a window that says what it is doing rather than a
/// blank frame.
fn startup_surface(title: String, detail: Option<String>, cx: &App) -> AnyElement {
    let theme = Theme::current(cx);
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .bg(theme.surface)
        .child(icon("icons/sparkle.svg", 24.0, theme.accent))
        .child(
            div()
                .mt(px(16.0))
                .text_size(sp(15.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title),
        )
        .children(detail.map(|detail| {
            div()
                .mt(px(8.0))
                .max_w(px(420.0))
                .text_center()
                .text_size(sp(12.5))
                .text_color(theme.text_tertiary)
                .child(detail)
        }))
        .into_any_element()
}

/// Open the Waku main window, restoring its persisted placement.
///
/// The window body only borrows application-scope services, so a rebuilt
/// window reattaches to the running daemon instead of starting a second one.
/// The daemon is reached after the window is on screen, so the first frame
/// paints skeleton content.
pub fn open_main_window(cx: &mut App) -> WindowHandle<MainWindow> {
    open_main_window_with(
        cx,
        Arc::new(crate::daemon::connect),
        load_persisted_preferences(),
    )
}

/// Read the persisted preferences once, ahead of the window's first frame.
///
/// This is a local one-shot file read, and it runs before the window opens, so
/// the first frame already carries the saved theme, language, and font size
/// instead of the system fallbacks `theme::init` published. A read failure
/// leaves the defaults in place rather than holding up the window; the settings
/// UI can correct them afterwards.
fn load_persisted_preferences() -> AppSettings {
    match crate::persistence::load_or_create_app_settings() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("could not load persisted preferences: {error:#}");
            AppSettings::default()
        }
    }
}

/// The main window over an explicit daemon connector and preference snapshot.
/// Tests drive the startup path with a connector that answers without a daemon
/// and a snapshot that need not come from disk.
fn open_main_window_with(
    cx: &mut App,
    connector: DaemonConnector,
    preferences: AppSettings,
) -> WindowHandle<MainWindow> {
    let (window_bounds, display_id) = restored_window_placement(cx);
    show_main_window(
        cx,
        main_window_options(window_bounds, display_id),
        move |window, cx| {
            // Put the persisted preferences in effect before the root view
            // exists, so the skeleton's first frame already shows them and
            // hydration only re-applies the same values.
            apply_persisted_preferences(&preferences, window, cx);
            let root = cx.new(|_| MainWindow::connecting(preferences));
            root.update(cx, |root, cx| root.connect(window, connector, cx));
            let closing = root.clone();
            window.on_window_should_close(cx, move |window, cx| {
                closing.update(cx, |root, cx| root.window_should_close(window, cx))
            });
            root
        },
    )
}

/// Put a persisted preference snapshot in effect on the window that is about to
/// paint. `theme::init` publishes the system theme before any window exists, so
/// a window opened without this would show system colors, the default locale,
/// and GPUI's 16px rem until hydration corrected them. Applying the same
/// snapshot here and again in `Waku::new` is idempotent by construction.
fn apply_persisted_preferences(settings: &AppSettings, window: &mut Window, cx: &mut App) {
    crate::i18n::set_language(settings.language);
    crate::theme::apply_theme_preference(settings.theme, window, cx);
    // Chrome text is authored in `sp` rems against the default UI font size,
    // so the window's rem size *is* the UI font size setting.
    window.set_rem_size(px(waku_client::persistence::sanitized_ui_font_size(
        settings.ui_font_size,
    )));
}

/// Deliver a system-notification click to the main window, opening one when
/// the application has none.
///
/// Registered once at application scope, so the handler outlives every window
/// it delivers to. A click that beats the daemon waits for the workspace, and
/// the asserted task's transcript then hydrates asynchronously like any other
/// selection.
pub fn init_notification_activation(cx: &mut App) {
    cx.on_system_notification_response(|response, cx| {
        let Some(task_id) = crate::app::task_id_from_notification_tag(&response.tag) else {
            return;
        };
        open_main_window(cx)
            .update(cx, |root, window, cx| {
                root.open_task_from_notification(task_id, cx);
                window.activate_window();
                cx.activate(true);
            })
            .ok();
        cx.dismiss_system_notification(&response.tag);
    });
}

/// Route Dock activation through the same opener first launch uses, so a
/// window the user closed comes back identically and never as a second
/// window.
pub trait WakuApplicationExt {
    fn with_main_window_reopen(self) -> Self;
}

impl WakuApplicationExt for Application {
    fn with_main_window_reopen(self) -> Self {
        self.on_reopen(|cx| {
            open_main_window(cx);
        });
        self
    }
}

/// The main window's options, shared by first launch and every rebuild.
fn main_window_options(
    window_bounds: WindowBounds,
    display_id: Option<gpui::DisplayId>,
) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(crate::instance::window_title().into()),
            // Windows creates the window without `WS_CAPTION`
            // either way; asking for the transparent titlebar
            // is what extends the client area over the frame
            // so Waku's own header can host the caption
            // buttons and drag region.
            appears_transparent: cfg!(any(target_os = "macos", target_os = "windows")),
            traffic_light_position: cfg!(target_os = "macos").then(|| point(px(16.0), px(17.0))),
        }),
        // Waku moves its custom macOS titlebar explicitly. Keep
        // the NSWindow movable so native controls and Window-menu
        // tiling remain enabled.
        is_movable: true,
        app_owns_titlebar_drag: cfg!(target_os = "macos"),
        window_background: if cfg!(target_os = "macos") {
            gpui::WindowBackgroundAppearance::Blurred
        } else {
            gpui::WindowBackgroundAppearance::Opaque
        },
        app_id: Some(APP_ID.to_owned()),
        // GPUI defaults to compositor/server decorations. If a
        // Wayland compositor declines them, it reports the
        // client fallback and Waku renders that frame itself.
        #[cfg(target_os = "linux")]
        icon: crate::platform::linux_app_icon(),
        window_bounds: Some(window_bounds),
        display_id,
        window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
        ..Default::default()
    }
}

/// Reopen the main window where the user last left it, on the display it was
/// left on. GPUI window bounds are display-relative, so the persisted frame is
/// anchored by resolving the saved display UUID against the connected
/// displays — Zed's scheme — and `display_id` rides along in `WindowOptions`.
/// When that display is gone the same offsets re-anchor on the primary
/// display, and the origin is clamped so the titlebar stays grabbable after
/// any display change.
fn restored_window_placement(cx: &App) -> (WindowBounds, Option<gpui::DisplayId>) {
    let centered = |cx: &App| {
        (
            WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
                cx,
            )),
            None,
        )
    };
    let Some(saved) = crate::persistence::load_window_state().filter(|saved| {
        [saved.x, saved.y, saved.width, saved.height]
            .iter()
            .all(|value| value.is_finite())
    }) else {
        return centered(cx);
    };
    let display = saved.display.and_then(|uuid| {
        cx.displays()
            .into_iter()
            .find(|display| display.uuid().ok() == Some(uuid))
    });
    let display_id = display.as_ref().map(|display| display.id());
    let Some(anchor) = display.or_else(|| cx.primary_display()) else {
        return centered(cx);
    };
    let anchor_size = anchor.bounds().size;
    let width = saved.width.max(MIN_WINDOW_WIDTH);
    let height = saved.height.max(MIN_WINDOW_HEIGHT);
    let x = saved.x.clamp(
        TITLEBAR_GRAB_WIDTH - width,
        (f32::from(anchor_size.width) - TITLEBAR_GRAB_WIDTH).max(0.0),
    );
    let y = saved.y.clamp(
        0.0,
        (f32::from(anchor_size.height) - TITLEBAR_GRAB_HEIGHT).max(0.0),
    );
    let bounds = Bounds::new(point(px(x), px(y)), size(px(width), px(height)));
    let window_bounds = if saved.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    (window_bounds, display_id)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::anyhow;
    use gpui::{
        AppContext as _, Context, IntoElement, Render, TestAppContext, VisualTestContext, Window,
        WindowOptions, div, px,
    };

    use super::{
        DaemonConnector, DaemonState, MainWindow, WindowContent, apply_persisted_preferences,
        open_main_window_with, show_main_window,
    };
    use crate::i18n::AppLanguage;
    use crate::persistence::AppSettings;
    use crate::theme::{Theme, ThemePreference};

    /// A daemon that never answers, so the window keeps painting its starting
    /// surface: the window-level tests below are about the close hook, not the
    /// workspace behind it.
    fn unreachable_daemon() -> DaemonConnector {
        Arc::new(|| Err(anyhow!("the daemon is unreachable")))
    }

    /// Stands in for the window's root view. The guard only cares which window
    /// exists, so the probe keeps the daemon out of the test.
    struct MainWindowProbe;

    impl Render for MainWindowProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn a_request_with_no_main_window_opens_one(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            show_main_window(cx, WindowOptions::default(), |_, cx| {
                cx.new(|_| MainWindowProbe)
            })
        });

        assert_eq!(cx.windows().len(), 1);
        assert_eq!(cx.windows()[0].window_id(), window.window_id());
    }

    /// Opening a window is what starts a traced run, and the first window
    /// continues the process's cold launch rather than beginning a rebuild.
    #[gpui::test]
    fn opening_a_window_starts_the_cold_traced_run(cx: &mut TestAppContext) {
        use crate::latency::{Milestone, RunKind};
        use crate::startup_trace::StartupTrace;

        cx.update(|cx| cx.set_global(StartupTrace::collecting(std::time::Instant::now())));
        cx.update(|cx| {
            show_main_window(cx, WindowOptions::default(), |_, cx| {
                cx.new(|_| MainWindowProbe)
            })
        });

        let run = cx
            .read(|cx| cx.global::<StartupTrace>().current_run())
            .expect("opening the window is traced");
        assert_eq!(run.kind, RunKind::Cold);
        assert!(
            run.reached(Milestone::WindowOpen).is_some(),
            "the window open milestone is recorded"
        );
    }

    #[gpui::test]
    fn a_second_open_request_focuses_the_existing_main_window(cx: &mut TestAppContext) {
        let builds = Rc::new(Cell::new(0));

        let first_builds = builds.clone();
        let first = cx.update(|cx| {
            show_main_window(cx, WindowOptions::default(), move |_, cx| {
                first_builds.set(first_builds.get() + 1);
                cx.new(|_| MainWindowProbe)
            })
        });

        let second_builds = builds.clone();
        let second = cx.update(|cx| {
            show_main_window(cx, WindowOptions::default(), move |_, cx| {
                second_builds.set(second_builds.get() + 1);
                cx.new(|_| MainWindowProbe)
            })
        });

        assert_eq!(
            builds.get(),
            1,
            "a second request focuses the open window instead of rebuilding it"
        );
        assert_eq!(first.window_id(), second.window_id());
        assert_eq!(
            cx.windows().len(),
            1,
            "the application keeps at most one main window"
        );
        assert_eq!(
            cx.read(|cx| cx.active_window().map(|window| window.window_id())),
            Some(first.window_id()),
            "the existing window is brought forward"
        );
    }

    /// The window is on screen, painting its starting surface, before the
    /// daemon is touched: opening it hands the connection to the background
    /// executor, so a slow or unreachable daemon cannot delay the first frame.
    /// The framework draws the window, so the surface checked here is the one
    /// this test paints.
    #[gpui::test]
    fn the_window_paints_before_the_daemon_answers(cx: &mut TestAppContext) {
        let connections = Arc::new(AtomicUsize::new(0));
        let connector: DaemonConnector = {
            let connections = connections.clone();
            Arc::new(move || {
                connections.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("the daemon is unreachable"))
            })
        };

        let window = cx.update(|cx| open_main_window_with(cx, connector, AppSettings::default()));

        assert_eq!(cx.windows().len(), 1, "the window opens with no daemon");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            0,
            "opening the window does not wait on the daemon"
        );
        assert!(
            window
                .read_with(cx, |root: &MainWindow, _| matches!(
                    root.content,
                    WindowContent::Connecting
                ))
                .unwrap(),
            "the window paints skeleton content while it connects"
        );

        cx.run_until_parked();

        assert_eq!(connections.load(Ordering::SeqCst), 1);
        let failure = window
            .read_with(cx, |root: &MainWindow, _| match &root.content {
                WindowContent::Failed(error) => Some(error.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            failure.as_deref(),
            Some("the daemon is unreachable"),
            "a failed daemon is reported in the window"
        );
        assert_eq!(
            cx.windows().len(),
            1,
            "the window stays up after a failed connection"
        );
    }

    /// A rebuilt window paints the persisted theme, language, and font size
    /// from its first frame: the opener reads them once, puts them in effect
    /// before the root view exists, and keeps the snapshot so hydration can
    /// re-apply the same values instead of reading a second, possibly
    /// different copy. Re-applying them here stands in for hydration and must
    /// leave the window unchanged.
    #[gpui::test]
    fn the_window_paints_the_persisted_preferences_before_it_hydrates(cx: &mut TestAppContext) {
        let preferences = AppSettings {
            theme: ThemePreference::Dark,
            language: AppLanguage::Japanese,
            ui_font_size: 18.0,
            ..AppSettings::default()
        };

        let window =
            cx.update(|cx| open_main_window_with(cx, unreachable_daemon(), preferences.clone()));

        assert!(
            window
                .read_with(cx, |root: &MainWindow, _| matches!(
                    root.content,
                    WindowContent::Connecting
                ))
                .unwrap(),
            "the skeleton is up before hydration"
        );
        assert!(
            cx.read(|cx| Theme::current(cx).is_dark),
            "the first frame uses the persisted theme, not the system one"
        );
        assert_eq!(
            crate::i18n::translate("menu.file"),
            "ファイル",
            "the skeleton renders in the persisted language"
        );
        assert_eq!(
            window.update(cx, |_, window, _| window.rem_size()).unwrap(),
            px(18.0),
            "the skeleton uses the persisted font size"
        );
        assert_eq!(
            window
                .read_with(cx, |root: &MainWindow, _| root.preferences.theme)
                .unwrap(),
            ThemePreference::Dark,
            "the window keeps the snapshot hydration will apply"
        );

        // Hydration re-applies the snapshot the window was opened with.
        cx.update(|cx| {
            window
                .update(cx, |_, window, cx| {
                    apply_persisted_preferences(&preferences, window, cx)
                })
                .unwrap()
        });
        assert!(cx.read(|cx| Theme::current(cx).is_dark));
        assert_eq!(crate::i18n::translate("menu.file"), "ファイル");
        assert_eq!(
            window.update(cx, |_, window, _| window.rem_size()).unwrap(),
            px(18.0)
        );

        // The locale is process-global; leave it where the rest of the binary
        // expects it.
        crate::i18n::set_language(AppLanguage::English);
    }

    /// A connection request belongs to the application, not to the window that
    /// made it. A window that closes while the daemon is answering must not
    /// drop the supervisor — which would reap the daemon it just started — and
    /// a window opened while that request is still in flight joins it instead
    /// of starting a second daemon.
    #[gpui::test]
    fn a_window_opened_while_a_connection_is_in_flight_joins_it(cx: &mut TestAppContext) {
        let connections = Arc::new(AtomicUsize::new(0));
        let connector = |connections: Arc<AtomicUsize>| -> DaemonConnector {
            Arc::new(move || {
                connections.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("the daemon is unreachable"))
            })
        };

        let first = cx.update(|cx| {
            open_main_window_with(cx, connector(connections.clone()), AppSettings::default())
        });
        // The window closes before its daemon answers.
        cx.update(|cx| {
            first
                .update(cx, |_, window, _| window.remove_window())
                .unwrap()
        });
        assert!(cx.windows().is_empty());

        // Activating the app opens a window while that request is still in
        // flight.
        let reopened = cx.update(|cx| {
            open_main_window_with(cx, connector(connections.clone()), AppSettings::default())
        });
        cx.run_until_parked();

        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "reopening while a connection is in flight joins it instead of starting a second daemon"
        );
        assert!(
            reopened
                .read_with(cx, |root: &MainWindow, _| matches!(
                    root.content,
                    WindowContent::Failed(_)
                ))
                .unwrap(),
            "the reopened window shows the answer the application was already waiting for"
        );
    }

    /// The connection belongs to the application, so an answer that arrives
    /// after the window that asked for it closed is still recorded at
    /// application scope. For a `Ready` answer that is what keeps the
    /// supervisor — and the daemon it just started — from being dropped with
    /// the window that happened to ask for it.
    #[gpui::test]
    fn a_connection_answer_outlives_the_window_that_asked_for_it(cx: &mut TestAppContext) {
        let connections = Arc::new(AtomicUsize::new(0));
        let connector: DaemonConnector = {
            let connections = connections.clone();
            Arc::new(move || {
                connections.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("the daemon is unreachable"))
            })
        };
        let window = cx.update(|cx| open_main_window_with(cx, connector, AppSettings::default()));

        // The window closes while its connection is still in flight.
        cx.update(|cx| {
            window
                .update(cx, |_, window, _| window.remove_window())
                .unwrap()
        });
        assert!(cx.windows().is_empty());

        cx.run_until_parked();

        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "the connection still resolved after the window closed"
        );
        assert!(
            cx.windows().is_empty(),
            "the late answer does not resurrect the window"
        );
        assert!(
            window.read_with(cx, |_: &MainWindow, _| ()).is_err(),
            "the window and its root view are gone"
        );
        assert!(
            cx.read(|cx| matches!(
                crate::daemon::state(cx),
                DaemonState::Failed(message) if message == "the daemon is unreachable"
            )),
            "the answer is recorded at application scope, not with the window"
        );
    }

    /// Closing the only window destroys it and leaves the application running;
    /// activation goes through the same opener and builds exactly one fresh
    /// window.
    #[gpui::test]
    fn closing_the_window_leaves_the_app_running_and_activation_rebuilds_it(
        cx: &mut TestAppContext,
    ) {
        let first = cx.update(|cx| {
            show_main_window(cx, WindowOptions::default(), |_, cx| {
                cx.new(|_| MainWindowProbe)
            })
        });

        cx.update(|cx| {
            first
                .update(cx, |_, window, _| crate::platform::close_window(window))
                .unwrap()
        });
        assert!(cx.windows().is_empty(), "closing removes the window");

        // Dock activation: same opener, a new window, still only one.
        let rebuilt = cx.update(|cx| {
            show_main_window(cx, WindowOptions::default(), |_, cx| {
                cx.new(|_| MainWindowProbe)
            })
        });
        assert_eq!(cx.windows().len(), 1);
        assert_ne!(rebuilt.window_id(), first.window_id());
    }

    /// The unsaved-edits guard sits inside the close hook rather than in place
    /// of it. A window with nothing unsaved — here one whose daemon never
    /// answered, so it has no workspace and no editor at all — still answers
    /// AppKit's close with `true`, and the window comes down when the platform
    /// closes it. A confirmation that swallowed the answer would leave a
    /// window that could not be closed.
    #[gpui::test]
    fn a_window_with_nothing_unsaved_answers_the_close(cx: &mut TestAppContext) {
        let window =
            cx.update(|cx| open_main_window_with(cx, unreachable_daemon(), AppSettings::default()));
        cx.run_until_parked();

        let mut visual = VisualTestContext::from_window(*window, cx);
        assert!(
            visual.simulate_close(),
            "nothing unsaved, so the close goes through"
        );
        assert_eq!(cx.windows().len(), 1, "asking is not closing");

        cx.update(|cx| {
            window
                .update(cx, |_, window, _| crate::platform::close_window(window))
                .unwrap()
        });
        assert!(cx.windows().is_empty(), "the window still closes");
    }
}
