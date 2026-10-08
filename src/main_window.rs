//! The desktop application's one main window.
//!
//! A window is disposable: first launch, Dock activation, and notification
//! activation all request the main window, and the application keeps at most
//! one. Every request goes through [`show_main_window`], so the single-window
//! rule lives in exactly one place, and the services the window borrows — the
//! daemon supervisor, the updater — are owned at application scope so a
//! rebuilt window reattaches to them instead of starting new ones.

use gpui::{
    App, Application, Bounds, Entity, Render, TitlebarOptions, Window, WindowBounds, WindowHandle,
    WindowOptions, point, px, size,
};

use crate::app::Waku;
use crate::identity::{APP_ID, APP_NAME};

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

/// Open the Waku main window, restoring its persisted placement.
///
/// The window body only borrows application-scope services, so a rebuilt
/// window reattaches to the running daemon instead of starting a second one.
pub fn open_main_window(cx: &mut App) -> WindowHandle<Waku> {
    let (window_bounds, display_id) = restored_window_placement(cx);
    show_main_window(
        cx,
        main_window_options(window_bounds, display_id),
        |window, cx| {
            let daemon = crate::daemon::supervisor(cx);
            let waku = Waku::new(window, cx, daemon);
            let composer_focus = waku.read(cx).composer_focus(cx);
            window.focus(&composer_focus, cx);
            crate::platform::configure_sidebar_material(
                window,
                crate::theme::Theme::current(cx).is_dark,
            );
            waku
        },
    )
}

/// Deliver a system-notification click to the main window, opening one when
/// the application has none.
///
/// Registered once at application scope, so the handler outlives every window
/// it delivers to. The asserted task is applied as soon as the window exists;
/// its transcript hydrates asynchronously like any other selection.
pub fn init_notification_activation(cx: &mut App) {
    cx.on_system_notification_response(|response, cx| {
        let Some(task_id) = crate::app::task_id_from_notification_tag(&response.tag) else {
            return;
        };
        open_main_window(cx)
            .update(cx, |waku, window, cx| {
                waku.open_task_from_notification(task_id, cx);
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

    use gpui::{
        AppContext as _, Context, IntoElement, Render, TestAppContext, Window, WindowOptions, div,
    };

    use super::show_main_window;

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
}
