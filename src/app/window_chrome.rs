#[cfg(target_os = "linux")]
use gpui::WindowButtonLayout;
#[cfg(target_os = "windows")]
use gpui::WindowControlArea;
use gpui::{
    AnyElement, App, BoxShadow, Decorations, Div, Hsla, IntoElement, MouseButton, ResizeEdge,
    Tiling, Window, div, prelude::*, px, transparent_black,
};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use gpui::{KeyDownEvent, WeakEntity, WindowButton};

use super::Waku;
use crate::theme::Theme;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use crate::ui::{icon, tooltip::Tooltip};

const CLIENT_FRAME_INSET: f32 = 10.0;
const CLIENT_FRAME_ROUNDING: f32 = 10.0;
pub(super) const TITLEBAR_BUTTON_SIZE: f32 = 26.0;
pub(super) const HISTORY_MARGIN: f32 = 6.0;
pub(super) const HISTORY_GAP: f32 = 2.0;
pub(super) const MACOS_TRAFFIC_LIGHT_CLEARANCE: f32 = 86.0;
const NAVIGATION_WIDTH: f32 = TITLEBAR_BUTTON_SIZE * 3.0 + HISTORY_MARGIN + HISTORY_GAP;
#[cfg(any(target_os = "linux", target_os = "windows"))]
const CLIENT_CONTROLS_PADDING: f32 = 10.0;
#[cfg(any(target_os = "linux", target_os = "windows"))]
const CLIENT_CONTROLS_GAP: f32 = 8.0;

#[derive(Clone, Copy)]
pub(super) enum WindowControlSide {
    Left,
    Right,
}

/// Native traffic lights are AppKit-owned and disappear in fullscreen. Keep
/// only the ordinary edge inset there; never change their native position.
pub(super) fn native_titlebar_clearance(macos: bool, fullscreen: bool) -> f32 {
    if macos && !fullscreen {
        MACOS_TRAFFIC_LIGHT_CLEARANCE
    } else {
        8.0
    }
}

/// One short, interruptible transition shared by every titlebar surface.
#[derive(Default)]
pub(super) struct TitlebarClearance {
    target: Option<f32>,
    width: f32,
    slide: Option<crate::ui::motion::WidthTween>,
}

impl TitlebarClearance {
    pub(super) fn is_sliding(&self) -> bool {
        self.slide.is_some()
    }

    pub(super) fn width(&self) -> f32 {
        self.width
    }

    pub(super) fn settle(&mut self, target: f32, reduce_motion: bool) -> bool {
        if self.target != Some(target) {
            self.slide = self
                .target
                .filter(|_| !reduce_motion)
                .map(|_| crate::ui::motion::WidthTween::new(self.width));
            self.target = Some(target);
            if self.slide.is_some() {
                // Keep the first frame exactly at the last painted position.
                return true;
            }
        }
        if reduce_motion {
            self.slide = None;
        }
        match self.slide.and_then(|slide| slide.width_toward(target)) {
            Some(width) => self.width = width,
            None => {
                self.slide = None;
                self.width = target;
            }
        }
        self.slide.is_some()
    }
}

/// The navigation group stays anchored to the window while the panel edge
/// moves underneath it. The content yields only until it has cleared that
/// group, with no ownership switch at either end of a sidebar transition.
fn titlebar_content_inset(sidebar_width: f32, navigation_end: f32) -> f32 {
    (navigation_end + 14.0 - sidebar_width).max(14.0)
}

/// Draw the frame a Wayland compositor delegates back to the client.
/// Server-decorated windows pass through untouched, so X11 and Wayland
/// compositors that provide native chrome keep doing so.
pub(crate) fn render_window_frame(
    content: AnyElement,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    let Decorations::Client { tiling } = window.window_decorations() else {
        window.set_client_inset(px(0.0));
        return content;
    };

    let inset = px(CLIENT_FRAME_INSET);
    let rounding = px(CLIENT_FRAME_ROUNDING);
    let border = px(1.0);
    let theme = Theme::current(cx);
    window.set_client_inset(inset);

    let frame = div()
        .relative()
        .size_full()
        .overflow_hidden()
        .when(!(tiling.top || tiling.left), |frame| {
            frame.rounded_tl(rounding)
        })
        .when(!(tiling.top || tiling.right), |frame| {
            frame.rounded_tr(rounding)
        })
        .when(!(tiling.bottom || tiling.left), |frame| {
            frame.rounded_bl(rounding)
        })
        .when(!(tiling.bottom || tiling.right), |frame| {
            frame.rounded_br(rounding)
        })
        .when(!tiling.top, |frame| frame.border_t(border))
        .when(!tiling.bottom, |frame| frame.border_b(border))
        .when(!tiling.left, |frame| frame.border_l(border))
        .when(!tiling.right, |frame| frame.border_r(border))
        .border_color(theme.border_strong)
        .when(!tiling.is_tiled(), |frame| {
            frame.shadow(vec![
                BoxShadow::new(
                    px(0.0),
                    px(0.0),
                    Hsla {
                        h: 0.0,
                        s: 0.0,
                        l: 0.0,
                        a: if theme.is_dark { 0.5 } else { 0.25 },
                    },
                )
                .blur_radius(inset / 2.0),
            ])
        })
        .child(content);

    div()
        .id("client-window-backdrop")
        .relative()
        .size_full()
        .bg(transparent_black())
        .when(!tiling.top, |backdrop| backdrop.pt(inset))
        .when(!tiling.bottom, |backdrop| backdrop.pb(inset))
        .when(!tiling.left, |backdrop| backdrop.pl(inset))
        .when(!tiling.right, |backdrop| backdrop.pr(inset))
        .child(frame)
        .children(
            window
                .is_resizable()
                .then(|| client_resize_handles(tiling, inset))
                .into_iter()
                .flatten(),
        )
        .into_any_element()
}

impl Waku {
    pub(super) fn titlebar_navigation_width(&self, window: &Window, cx: &App) -> f32 {
        self.titlebar_clearance.width()
            + self.left_client_controls_width(window, cx)
            + NAVIGATION_WIDTH
    }

    pub(super) fn titlebar_content_inset(&self, window: &Window, cx: &App) -> f32 {
        titlebar_content_inset(
            self.sidebar_rendered_width,
            self.titlebar_navigation_width(window, cx),
        )
    }

    fn left_client_controls_width(&self, window: &Window, cx: &App) -> f32 {
        #[cfg(target_os = "linux")]
        if matches!(window.window_decorations(), Decorations::Client { .. }) {
            let count = cx
                .button_layout()
                .unwrap_or_else(WindowButtonLayout::linux_default)
                .left
                .into_iter()
                .flatten()
                .count();
            if count > 0 {
                return CLIENT_CONTROLS_PADDING * 2.0
                    + count as f32 * TITLEBAR_BUTTON_SIZE
                    + (count - 1) as f32 * CLIENT_CONTROLS_GAP;
            }
        }
        let _ = (window, cx);
        0.0
    }

    /// Render the window controls Waku owns: the desktop's configured button
    /// order when GPUI had to fall back from server-side to client-side
    /// decorations, and the platform order on Windows.
    pub(super) fn render_client_window_controls(
        &self,
        side: WindowControlSide,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        #[cfg(target_os = "windows")]
        let buttons = {
            // Windows keeps all three on the right, in this order, and has no
            // per-user layout preference to read.
            if !matches!(side, WindowControlSide::Right) {
                return None;
            }
            [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ]
        };

        #[cfg(target_os = "linux")]
        let buttons = {
            if !matches!(window.window_decorations(), Decorations::Client { .. }) {
                return None;
            }
            let layout = cx
                .button_layout()
                .unwrap_or_else(WindowButtonLayout::linux_default);
            match side {
                WindowControlSide::Left => layout.left,
                WindowControlSide::Right => layout.right,
            }
        };

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            if buttons.iter().all(Option::is_none) {
                return None;
            }

            let theme = Theme::current(cx);
            let is_maximized = window.is_maximized();
            let supported = window.window_controls();
            let side_id = match side {
                WindowControlSide::Left => "client-window-controls-left",
                WindowControlSide::Right => "client-window-controls-right",
            };
            // A window control owns the close, so the close control reaches the
            // workspace that can ask about unsaved edits.
            let waku = cx.entity().downgrade();
            let controls = buttons.into_iter().flatten().map(|button| {
                let enabled = match button {
                    WindowButton::Minimize => supported.minimize && window.is_minimizable(),
                    WindowButton::Maximize => supported.maximize && window.is_resizable(),
                    WindowButton::Close => true,
                };
                client_window_button(button, enabled, is_maximized, theme, waku.clone(), cx)
            });

            Some(
                div()
                    .id(side_id)
                    .tab_group()
                    .tab_stop(false)
                    .h_full()
                    .flex_none()
                    .px(px(CLIENT_CONTROLS_PADDING))
                    .flex()
                    .items_center()
                    .gap(px(CLIENT_CONTROLS_GAP))
                    .children(controls)
                    .into_any_element(),
            )
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = (side, window, cx);
            None
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn client_window_button(
    button: WindowButton,
    enabled: bool,
    is_maximized: bool,
    theme: Theme,
    waku: WeakEntity<Waku>,
    cx: &mut Context<Waku>,
) -> AnyElement {
    let (id, icon_path, label) = match button {
        WindowButton::Minimize => (
            "client-window-minimize",
            "icons/window-minimize.svg",
            tr!("window.minimize"),
        ),
        WindowButton::Maximize if is_maximized => (
            "client-window-restore",
            "icons/window-restore.svg",
            tr!("window.restore"),
        ),
        WindowButton::Maximize => (
            "client-window-maximize",
            "icons/window-maximize.svg",
            tr!("window.maximize"),
        ),
        WindowButton::Close => (
            "client-window-close",
            "icons/x.svg",
            tr!("menu.close_window"),
        ),
    };
    let focus = cx.focus_handle();
    let icon_color = if enabled {
        theme.text_secondary
    } else {
        theme.text_ghost
    };
    let click_waku = waku.clone();
    let key_waku = waku;

    let control = div().id(id);
    // Windows hit-tests the caption before it dispatches a mouse event.
    // Claiming the button's area here is also what lets the maximize control
    // show Windows 11's snap layouts on hover.
    #[cfg(target_os = "windows")]
    let control = control.window_control_area(match button {
        WindowButton::Minimize => WindowControlArea::Min,
        WindowButton::Maximize => WindowControlArea::Max,
        WindowButton::Close => WindowControlArea::Close,
    });

    control
        .track_focus(&focus)
        .tab_index(0)
        .tab_stop(true)
        .size(px(TITLEBAR_BUTTON_SIZE))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .opacity(if enabled { 1.0 } else { 0.45 })
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .when(enabled, |control| {
            control
                .hover(move |style| {
                    if button == WindowButton::Close {
                        style.bg(theme.danger_soft)
                    } else {
                        style.bg(theme.overlay)
                    }
                })
                .active(|style| style.bg(theme.overlay_strong))
        })
        .tooltip(Tooltip::text(label))
        .child(icon(icon_path, 14.0, icon_color))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            if enabled {
                activate_window_button(button, window, &click_waku, cx);
            }
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if enabled
                && !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                activate_window_button(button, window, &key_waku, cx);
                cx.stop_propagation();
            }
        })
        .into_any_element()
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn activate_window_button(
    button: WindowButton,
    window: &mut Window,
    waku: &WeakEntity<Waku>,
    cx: &mut App,
) {
    match button {
        WindowButton::Minimize => window.minimize_window(),
        WindowButton::Maximize => window.zoom_window(),
        // The window's own close control is the window's to close: it goes
        // through the unsaved-edits guard, so a dirty editor is never
        // discarded behind the user's back on a client-decorated window.
        WindowButton::Close => {
            let _ = waku.update(cx, |waku, cx| waku.close_window(window, cx));
        }
    }
}

fn client_resize_handles(tiling: Tiling, inset: gpui::Pixels) -> Vec<AnyElement> {
    let mut handles = Vec::with_capacity(8);
    if !tiling.top {
        handles.push(client_resize_handle(ResizeEdge::Top, inset));
    }
    if !tiling.bottom {
        handles.push(client_resize_handle(ResizeEdge::Bottom, inset));
    }
    if !tiling.left {
        handles.push(client_resize_handle(ResizeEdge::Left, inset));
    }
    if !tiling.right {
        handles.push(client_resize_handle(ResizeEdge::Right, inset));
    }
    if !tiling.top && !tiling.left {
        handles.push(client_resize_handle(ResizeEdge::TopLeft, inset));
    }
    if !tiling.top && !tiling.right {
        handles.push(client_resize_handle(ResizeEdge::TopRight, inset));
    }
    if !tiling.bottom && !tiling.left {
        handles.push(client_resize_handle(ResizeEdge::BottomLeft, inset));
    }
    if !tiling.bottom && !tiling.right {
        handles.push(client_resize_handle(ResizeEdge::BottomRight, inset));
    }
    handles
}

fn client_resize_handle(edge: ResizeEdge, inset: gpui::Pixels) -> AnyElement {
    let (id, handle): (&'static str, Div) = match edge {
        ResizeEdge::Top => (
            "client-resize-top",
            div()
                .absolute()
                .top_0()
                .left(inset)
                .right(inset)
                .h(inset)
                .cursor_n_resize(),
        ),
        ResizeEdge::Bottom => (
            "client-resize-bottom",
            div()
                .absolute()
                .bottom_0()
                .left(inset)
                .right(inset)
                .h(inset)
                .cursor_s_resize(),
        ),
        ResizeEdge::Left => (
            "client-resize-left",
            div()
                .absolute()
                .top(inset)
                .bottom(inset)
                .left_0()
                .w(inset)
                .cursor_w_resize(),
        ),
        ResizeEdge::Right => (
            "client-resize-right",
            div()
                .absolute()
                .top(inset)
                .bottom(inset)
                .right_0()
                .w(inset)
                .cursor_e_resize(),
        ),
        ResizeEdge::TopLeft => (
            "client-resize-top-left",
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(inset)
                .cursor_nwse_resize(),
        ),
        ResizeEdge::TopRight => (
            "client-resize-top-right",
            div()
                .absolute()
                .top_0()
                .right_0()
                .size(inset)
                .cursor_nesw_resize(),
        ),
        ResizeEdge::BottomLeft => (
            "client-resize-bottom-left",
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .size(inset)
                .cursor_nesw_resize(),
        ),
        ResizeEdge::BottomRight => (
            "client-resize-bottom-right",
            div()
                .absolute()
                .bottom_0()
                .right_0()
                .size(inset)
                .cursor_nwse_resize(),
        ),
    };

    handle
        .id(id)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        })
        .into_any_element()
}

#[cfg(test)]
mod titlebar_layout_tests {
    use super::*;

    #[test]
    fn native_clearance_matches_actual_window_mode() {
        assert_eq!(native_titlebar_clearance(true, false), 86.0);
        assert_eq!(native_titlebar_clearance(true, true), 8.0);
        assert_eq!(native_titlebar_clearance(false, false), 8.0);
        assert_eq!(native_titlebar_clearance(false, true), 8.0);
    }

    #[test]
    fn content_clears_navigation_through_the_entire_sidebar_slide() {
        for clearance in [8.0, 30.0, 86.0] {
            let end = clearance + 86.0;
            let mut previous = f32::INFINITY;
            for width in 0..=300 {
                let width = width as f32;
                let inset = titlebar_content_inset(width, end);
                assert!(width + inset >= end + 14.0);
                assert!(inset >= 14.0);
                assert!(inset <= previous);
                if previous.is_finite() {
                    assert!(previous - inset <= 1.0);
                }
                previous = inset;
            }
            assert_eq!(titlebar_content_inset(0.0, end), end + 14.0);
            assert_eq!(titlebar_content_inset(260.0, end), 14.0);
        }
    }

    #[test]
    fn first_frame_and_reduce_motion_have_no_transition() {
        let mut clearance = TitlebarClearance::default();
        assert!(!clearance.settle(86.0, false));
        assert_eq!(clearance.width(), 86.0);
        assert!(clearance.settle(8.0, false));
        assert!(!clearance.settle(8.0, true));
        assert_eq!(clearance.width(), 8.0);
        assert!(!clearance.settle(86.0, true));
        assert_eq!(clearance.width(), 86.0);
    }

    #[test]
    fn interrupted_clearance_restarts_from_last_displayed_position() {
        let mut clearance = TitlebarClearance {
            target: Some(8.0),
            width: 30.0,
            slide: Some(crate::ui::motion::WidthTween::new(86.0)),
        };
        assert!(clearance.settle(86.0, false));
        assert_eq!(clearance.width(), 30.0);
    }
}
