use super::*;
use gpui::{Modifiers, TestAppContext};

struct SelectionHarness {
    prompt: Option<MarkdownView>,
    prompt_scroll: gpui::ScrollHandle,
    markdown: MarkdownView,
    selection: TranscriptSelection,
}

impl gpui::Render for SelectionHarness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&Theme::light());
        let ctx = Ctx::new("reply", &palette, Metrics::BODY, self.selection.clone());
        let input = self.selection.clone();
        let prompt_ctx = Ctx::new("prompt", &palette, Metrics::BODY, self.selection.clone());
        let prompt = self.prompt.as_ref().map(|prompt| {
            div()
                .id("clipped-prompt")
                .h(px(100.0))
                .overflow_y_scroll()
                .track_scroll(&self.prompt_scroll)
                .children(markdown(prompt, &prompt_ctx))
        });
        div()
            .w(px(640.0))
            .flex()
            .flex_col()
            .child(frame_reset(self.selection.clone()))
            .children(prompt)
            .children(markdown(&self.markdown, &ctx))
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        install_selection_input(window, &input);
                    },
                )
                .absolute()
                .size_0(),
            )
    }
}

#[gpui::test]
fn selection_drag_across_markdown_blocks_keeps_the_anchor(cx: &mut TestAppContext) {
    let selection = TranscriptSelection::default();
    let mut markdown = MarkdownView::new();
    markdown.set_text("Done. Here are the results to confirm.\n\n## Status\n\n- **worktree**: `feature/selection` based on `main`.\n- The previously uncommitted OpenSpec changes passed `openspec validate --strict`.\n- The previous changes have already been merged.\n", false);
    let (_, cx) = cx.add_window_view(|_, _| SelectionHarness {
        prompt: None,
        prompt_scroll: gpui::ScrollHandle::new(),
        markdown,
        selection: selection.clone(),
    });
    cx.run_until_parked();
    let bounds = selection
        .registry
        .borrow()
        .entries()
        .iter()
        .map(|entry| entry.geometry.bounds())
        .collect::<Vec<_>>();
    assert_eq!(bounds.len(), 5);
    let origin = point(bounds[0].left(), bounds[0].top() + px(4.0));
    cx.simulate_mouse_down(origin, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    for x in 0..120 {
        let point = point(bounds[3].left() + px(x as f32), bounds[3].top() + px(4.0));
        cx.simulate_mouse_move(point, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert!(selection.selection.borrow().is_dragging());
        assert!(
            selection
                .selection
                .borrow()
                .selected_text()
                .unwrap()
                .starts_with("Done. Here are the results to confirm.")
        );
        let entries = selection.registry.borrow();
        assert_eq!(entries.entries().len(), 5);
    }
}

fn assert_clipped_prompt_preserves_reply_selection(cx: &mut TestAppContext, source: &str) {
    let selection = TranscriptSelection::default();
    let mut prompt = MarkdownView::new();
    prompt.set_text(source, false);
    let mut markdown = MarkdownView::new();
    markdown.set_text(
        "Reply text that must own this selection.\n\nThe second reply paragraph.",
        false,
    );
    let (view, cx) = cx.add_window_view(|_, _| SelectionHarness {
        prompt: Some(prompt),
        prompt_scroll: gpui::ScrollHandle::new(),
        markdown,
        selection: selection.clone(),
    });
    cx.run_until_parked();
    for scroll_y in [0.0, -40.0, -300.0] {
        view.update(cx, |view, cx| {
            view.prompt_scroll.set_offset(point(px(0.0), px(scroll_y)));
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = selection
            .registry
            .borrow()
            .entries()
            .iter()
            .filter(|entry| entry.key.row.as_ref() == "reply")
            .map(|entry| entry.geometry.bounds())
            .collect::<Vec<_>>();
        assert_eq!(bounds.len(), 2);
        let start = point(bounds[0].left() + px(1.0), bounds[0].top() + px(4.0));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            selection.selection.borrow().anchor().unwrap().row.as_ref(),
            "reply"
        );
        // Cross the block boundary, including the horizontal region occupied
        // by the clipped prompt's invisible laid-out text.
        for x in (10..200).step_by(10) {
            let head = point(bounds[1].left() + px(x as f32), bounds[1].top() + px(4.0));
            cx.simulate_mouse_move(head, MouseButton::Left, Modifiers::none());
            cx.run_until_parked();
            let state = selection.selection.borrow();
            assert!(state.selected_text().unwrap().starts_with("Reply text"));
            assert!(
                state
                    .spans()
                    .iter()
                    .all(|span| span.key.row.as_ref() == "reply")
            );
        }
        let selected = selection.selection.borrow().selected_text();
        cx.simulate_mouse_up(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(start, None, Modifiers::none());
        cx.run_until_parked();
        assert!(!selection.selection.borrow().is_dragging());
        assert_eq!(selection.selection.borrow().selected_text(), selected);
        let after = selection
            .registry
            .borrow()
            .entries()
            .iter()
            .filter(|entry| entry.key.row.as_ref() == "reply")
            .map(|entry| entry.geometry.bounds())
            .collect::<Vec<_>>();
        assert_eq!(bounds, after, "selecting text must not move the reply");
    }
}

#[gpui::test]
fn hidden_prompt_text_cannot_steal_a_selection_in_the_reply(cx: &mut TestAppContext) {
    assert_clipped_prompt_preserves_reply_selection(
        cx,
        &"A long prompt line hidden behind the reply.\n\n".repeat(40),
    );
}

#[gpui::test]
fn hidden_prompt_math_cannot_steal_a_selection_in_the_reply(cx: &mut TestAppContext) {
    assert_clipped_prompt_preserves_reply_selection(
        cx,
        &"A long prompt with $x^2$ hidden behind the reply.\n\n".repeat(40),
    );
}

#[gpui::test]
fn hidden_prompt_code_cannot_steal_a_selection_in_the_reply(cx: &mut TestAppContext) {
    assert_clipped_prompt_preserves_reply_selection(
        cx,
        &format!(
            "```text\n{}```",
            "A long prompt line hidden behind the reply.\n".repeat(40)
        ),
    );
}

#[gpui::test]
fn selection_gutters_and_clipped_wrap_edges_use_visible_document_ends(cx: &mut TestAppContext) {
    let selection = TranscriptSelection::default();
    let source = "one two three four five six seven eight nine ten eleven twelve ".repeat(8);
    let mut markdown = MarkdownView::new();
    markdown.set_text(&source, false);
    let (_, cx) = cx.add_window_view(|_, _| SelectionHarness {
        prompt: None,
        prompt_scroll: gpui::ScrollHandle::new(),
        markdown,
        selection: selection.clone(),
    });
    cx.run_until_parked();
    let registry = selection.registry.borrow();
    let entry = &registry.entries()[0];
    let bounds = entry.geometry.bounds();
    for x in [bounds.left(), bounds.center().x, bounds.right()] {
        assert_eq!(
            registry_point(&registry, point(x, bounds.top() - px(20.0))),
            Some((0, 0))
        );
        assert_eq!(
            registry_point(&registry, point(x, bounds.bottom() + px(20.0))),
            Some((0, entry.text.len()))
        );
        assert_eq!(
            registry_point(&registry, point(x, bounds.bottom())),
            Some((0, entry.text.len()))
        );
    }
    let TextGeometry::Text(layout, _) = &entry.geometry else {
        panic!("plain text fixture")
    };
    let lines = layout.line_layouts();
    let first_wrap = lines[0].wrap_boundaries()[0];
    let boundary =
        lines[0].unwrapped_layout.runs[first_wrap.run_ix].glyphs[first_wrap.glyph_ix].index;
    assert!(boundary > 0 && boundary < entry.text.len());
    let mask = Bounds::new(bounds.origin, size(bounds.size.width, layout.line_height()));
    let mut clipped = SelectionRegistry::default();
    clipped.push(RegisteredText {
        geometry: TextGeometry::Text(layout.clone(), mask),
        ..entry.clone()
    });
    for y in [mask.bottom(), mask.bottom() + px(100.0)] {
        for x in [bounds.left(), bounds.center().x, bounds.right()] {
            assert_eq!(
                registry_point(&clipped, point(x, y)),
                Some((0, boundary)),
                "a clipped bottom must stop before the first hidden row"
            );
        }
    }
    let lower = Bounds::from_corners(point(bounds.left(), mask.bottom()), bounds.bottom_right());
    let mut clipped_top = SelectionRegistry::default();
    clipped_top.push(RegisteredText {
        geometry: TextGeometry::Text(layout.clone(), lower),
        ..entry.clone()
    });
    assert_eq!(
        registry_point(&clipped_top, point(bounds.right(), bounds.top())),
        Some((0, boundary))
    );
    let mut hidden = SelectionRegistry::default();
    hidden.push(RegisteredText {
        geometry: TextGeometry::Text(
            layout.clone(),
            Bounds::new(
                bounds.bottom_right() + point(px(1.0), px(1.0)),
                size(px(10.0), px(10.0)),
            ),
        ),
        ..entry.clone()
    });
    assert_eq!(registry_point(&hidden, bounds.center()), None);
}

#[gpui::test]
fn a_scrolled_clip_boundary_does_not_resolve_into_a_hidden_wrapped_row(cx: &mut TestAppContext) {
    let selection = TranscriptSelection::default();
    let (_, cx) = cx.add_window_view(|_, _| SelectionHarness {
        prompt: None,
        prompt_scroll: gpui::ScrollHandle::new(),
        markdown: MarkdownView::new(),
        selection,
    });
    let text: SharedString = "one two three four five six seven eight nine ten "
        .repeat(40)
        .into();
    let styled = StyledText::new(text.clone());
    let layout = styled.layout().clone();
    cx.draw(
        point(px(0.0), px(-320.0)),
        size(px(96.0), px(2400.0)),
        move |_, _| {
            div()
                .w(px(96.0))
                .text_size(px(14.0))
                .line_height(px(21.0))
                .child(styled)
        },
    );
    assert_eq!(layout.bounds().top(), px(-320.0));
    let lines = layout.line_layouts();
    let boundary = lines[0].wrap_boundaries()[19];
    let expected = lines[0].unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index;
    let mut registry = SelectionRegistry::default();
    registry.push(RegisteredText {
        key: TextKey::new("scrolled", 0),
        text: Rc::from(text.as_ref()),
        block_break: false,
        geometry: TextGeometry::Text(
            layout,
            Bounds::new(point(px(0.0), px(0.0)), size(px(96.0), px(100.0))),
        ),
    });
    assert_eq!(
        registry_point(&registry, point(px(50.0), px(150.0))),
        Some((0, expected))
    );
}

#[gpui::test]
fn a_clipped_hard_line_top_uses_the_first_visible_line(cx: &mut TestAppContext) {
    let selection = TranscriptSelection::default();
    let (_, cx) = cx.add_window_view(|_, _| SelectionHarness {
        prompt: None,
        prompt_scroll: gpui::ScrollHandle::new(),
        markdown: MarkdownView::new(),
        selection,
    });
    let text: SharedString = "hidden line\nvisible line\nlast line".into();
    let styled = StyledText::new(text.clone());
    let layout = styled.layout().clone();
    cx.draw(
        point(px(0.0), px(-21.0)),
        size(px(200.0), px(100.0)),
        move |_, _| {
            div()
                .w(px(200.0))
                .text_size(px(14.0))
                .line_height(px(21.0))
                .child(styled)
        },
    );
    let mut registry = SelectionRegistry::default();
    registry.push(RegisteredText {
        key: TextKey::new("scrolled", 0),
        text: Rc::from(text.as_ref()),
        block_break: false,
        geometry: TextGeometry::Text(
            layout,
            Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(100.0))),
        ),
    });
    assert_eq!(
        registry_point(&registry, point(px(50.0), px(-10.0))),
        Some((0, "hidden line\n".len()))
    );
    let entry = &registry.entries()[0];
    let expected = match entry.geometry.index_for_position(point(px(50.0), px(0.5))) {
        Ok(offset) | Err(offset) => offset,
    };
    assert!(expected > "hidden line\n".len());
    assert_eq!(registry_offset(entry, point(px(50.0), px(0.0))), expected);
}
