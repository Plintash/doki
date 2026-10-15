//! Waku's tooltip surface.
//!
//! GPUI already owns tooltip *behaviour* — hover timing, placement, dismissal —
//! through `InteractiveElement::tooltip`, which asks only for a view to render.
//! This is that view, and nothing more.

use gpui::{
    AnyView, App, AppContext, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    div, px,
};

use crate::theme::{Theme, sp};

/// A single-line hint.
pub struct Tooltip {
    label: SharedString,
}

impl Tooltip {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }

    /// Build the view GPUI's `.tooltip(..)` expects.
    pub fn build(self, _window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| self).into()
    }

    /// Shorthand for the overwhelmingly common case:
    /// `.tooltip(Tooltip::text("Copy message"))`.
    pub fn text(
        label: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let label = label.into();
        move |window, cx| Tooltip::new(label.clone()).build(window, cx)
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        // The outer wrapper is transparent and only offsets the card from the
        // cursor; the shadow needs a parent that does not clip it.
        div().pt(px(4.0)).pl(px(2.0)).child(
            div()
                .px(px(7.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.raised)
                .shadow_md()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(sp(12.5))
                .line_height(sp(15.0))
                .text_color(theme.text_secondary)
                .child(self.label.clone()),
        )
    }
}

/// The same short name is available when an icon is reached with Tab. Kept
/// outside layout so learning a control never shifts its neighbors.
#[derive(gpui::IntoElement)]
pub struct FocusHint {
    focus: gpui::FocusHandle,
    label: SharedString,
}

impl FocusHint {
    pub fn new(focus: gpui::FocusHandle, label: impl Into<SharedString>) -> Self {
        Self {
            focus,
            label: label.into(),
        }
    }
}

impl gpui::RenderOnce for FocusHint {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let visible = self.focus.is_focused(window) && window.last_input_was_keyboard();
        div().absolute().top_0().left_0().children(visible.then(|| {
            gpui::deferred(
                gpui::anchored()
                    .offset(gpui::point(px(0.0), px(32.0)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(
                        div()
                            .px(px(7.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border_strong)
                            .bg(theme.raised)
                            .shadow_md()
                            .text_size(sp(12.5))
                            .line_height(sp(15.0))
                            .text_color(theme.text_secondary)
                            .whitespace_nowrap()
                            .child(self.label),
                    ),
            )
            .with_priority(2)
        }))
    }
}
