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
//! delayed or blank frame.

use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, Application, Bounds, Context, Entity, FontWeight,
    IntoElement, ParentElement, Render, Styled, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions, div, point, px, size,
};
use uuid::Uuid;
use waku_client::DaemonSupervisor;

use crate::app::Waku;
use crate::app::window_chrome::render_window_frame;
use crate::identity::{APP_ID, APP_NAME};
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
        None => cx
            .open_window(options, |window, cx| {
                crate::platform::configure_main_window_close_behavior(window, cx);
                build(window, cx)
            })
            .expect("failed to open Waku window"),
    };
    cx.activate(true);
    window
}

/// How a window reaches its daemon. Production starts the supervised daemon,
/// or connects to one managed elsewhere; tests substitute a prepared answer so
/// the startup path can run without a daemon. Runs off the UI thread.
pub(crate) type DaemonConnector = Arc<dyn Fn() -> anyhow::Result<DaemonSupervisor> + Send + Sync>;

/// The main window's root view.
///
/// It opens with no workspace at all and paints skeleton content, then swaps in
/// the daemon-backed workspace when the connection lands. A daemon that never
/// answers is reported in the same window instead of leaving a blank frame.
pub struct MainWindow {
    /// What the window shows. One state rather than a workspace beside a
    /// status flag, so "connecting", "failed", and "hydrated" cannot
    /// disagree with each other.
    content: WindowContent,
    /// The newest connection request. An answer carrying an older generation
    /// describes a request this window has moved past, so it is discarded
    /// instead of replacing newer content.
    connection: u64,
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
    /// daemon connection runs.
    fn connecting() -> Self {
        Self {
            content: WindowContent::Connecting,
            connection: 0,
            requested_task: None,
        }
    }

    /// Ask for the daemon, off the UI thread.
    ///
    /// The window is already on screen when this runs, so the request only
    /// schedules work: a daemon that is slow to start never delays the first
    /// frame, and one that cannot be reached is reported when its answer
    /// arrives.
    fn connect(&mut self, window: &mut Window, connector: DaemonConnector, cx: &mut Context<Self>) {
        self.connection = self.connection.wrapping_add(1).max(1);
        let connection = self.connection;
        // A window built after an earlier one connected reattaches to the
        // daemon published at application scope instead of starting a second.
        if let Some(daemon) = crate::daemon::connected(cx) {
            self.apply_connection(connection, Ok(daemon), window, cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let connected = cx
                .background_executor()
                .spawn(async move { connector() })
                .await;
            // A window that closed while the daemon was answering has nowhere
            // to put the result: the update fails, and the result is dropped
            // with the window it was meant for.
            this.update_in(cx, |this, window, cx| {
                this.apply_connection(connection, connected, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Apply one connection request's answer. A superseded request is dropped
    /// here; a request whose window has since closed never reaches this at all.
    fn apply_connection(
        &mut self,
        connection: u64,
        result: anyhow::Result<DaemonSupervisor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if connection != self.connection {
            return;
        }
        match result {
            Ok(daemon) => self.attach_workspace(daemon, window, cx),
            Err(error) => {
                self.content = WindowContent::Failed(error.to_string());
                cx.notify();
            }
        }
    }

    /// Build the workspace for `daemon` and show it in place of the starting
    /// surface.
    fn attach_workspace(
        &mut self,
        daemon: DaemonSupervisor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Published before the workspace exists, so a window rebuilt from here
        // on reattaches to this daemon instead of starting another one.
        crate::daemon::publish(cx, daemon.clone());
        let workspace = Waku::new(window, cx, daemon);
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
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
    open_main_window_with(cx, Arc::new(crate::daemon::connect))
}

/// Open the main window over an explicit daemon connector. Tests drive the
/// startup path with one that answers without a daemon.
fn open_main_window_with(cx: &mut App, connector: DaemonConnector) -> WindowHandle<MainWindow> {
    let (window_bounds, display_id) = restored_window_placement(cx);
    show_main_window(
        cx,
        main_window_options(window_bounds, display_id),
        move |window, cx| {
            let root = cx.new(|_| MainWindow::connecting());
            root.update(cx, |root, cx| root.connect(window, connector, cx));
            root
        },
    )
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
/// hidden or rebuilt window comes back identically and never as a second
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
            title: Some(APP_NAME.into()),
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
        AppContext as _, Context, IntoElement, Render, TestAppContext, Window, WindowOptions, div,
    };

    use super::{
        DaemonConnector, MainWindow, WindowContent, open_main_window_with, show_main_window,
    };

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

        let window = cx.update(|cx| open_main_window_with(cx, connector));

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

    /// An answer is only applied while it is still the newest request: one from
    /// a request the window has moved past never replaces newer content.
    #[gpui::test]
    fn a_superseded_connection_answer_is_discarded(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            open_main_window_with(cx, Arc::new(|| Err(anyhow!("the first daemon is down"))))
        });
        let superseded = window
            .read_with(cx, |root: &MainWindow, _| root.connection)
            .unwrap();

        // A second request makes the first one stale.
        cx.update(|cx| {
            window
                .update(cx, |root, window, cx| {
                    root.connect(
                        window,
                        Arc::new(|| Err(anyhow!("the second daemon is down"))),
                        cx,
                    );
                })
                .unwrap();
        });

        // The first request's answer arrives last and lands nowhere.
        cx.update(|cx| {
            window
                .update(cx, |root, window, cx| {
                    root.apply_connection(
                        superseded,
                        Err(anyhow!("the first daemon is down")),
                        window,
                        cx,
                    );
                })
                .unwrap();
        });
        assert!(
            window
                .read_with(cx, |root: &MainWindow, _| matches!(
                    root.content,
                    WindowContent::Connecting
                ))
                .unwrap(),
            "a superseded answer leaves the window's content alone"
        );

        cx.run_until_parked();
        let failure = window
            .read_with(cx, |root: &MainWindow, _| match &root.content {
                WindowContent::Failed(error) => Some(error.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            failure.as_deref(),
            Some("the second daemon is down"),
            "the newest request's answer is the one that stands"
        );
    }

    /// An answer addressed to a window that has since closed is dropped: a late
    /// hydration must not write state for a window that no longer exists.
    #[gpui::test]
    fn an_answer_for_a_closed_window_is_dropped(cx: &mut TestAppContext) {
        let connections = Arc::new(AtomicUsize::new(0));
        let connector: DaemonConnector = {
            let connections = connections.clone();
            Arc::new(move || {
                connections.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("the daemon is unreachable"))
            })
        };
        let window = cx.update(|cx| open_main_window_with(cx, connector));

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

        // A window opened afterwards starts from scratch: the answer that
        // arrived for the closed one belongs to that window alone.
        let reopened = cx.update(|cx| {
            open_main_window_with(cx, Arc::new(|| Err(anyhow!("the daemon is unreachable"))))
        });
        assert!(
            reopened
                .read_with(cx, |root: &MainWindow, _| matches!(
                    root.content,
                    WindowContent::Connecting
                ))
                .unwrap(),
            "a reopened window starts from its own connection"
        );
    }
}
