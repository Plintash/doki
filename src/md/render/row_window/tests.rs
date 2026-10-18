use super::*;
use gpui::TestAppContext;

fn window(top: f32, height: f32) -> MessageBodyWindow {
    MessageBodyWindow {
        visible_top: top,
        visible_height: height,
    }
}

#[test]
fn row_spacers_preserve_height_and_visible_ranges() {
    for count in [1, 2, 10, 101] {
        for gap in [0.0, 3.5, 5.0] {
            let mut rows = Rows::new(count, Vec::new());
            let heights = (0..count)
                .map(|i| px((i % 7) as f32 * 11.5))
                .collect::<Vec<_>>();
            for (index, height) in heights.iter().enumerate() {
                rows.measure(index, *height);
            }
            let gap = px(gap);
            let total = heights.iter().copied().sum::<Pixels>() + gap * (count - 1) as f32;
            for top in [
                -2000.0,
                0.0,
                600.0,
                1300.0,
                f32::from(total),
                f32::from(total) + 2000.0,
            ] {
                let plan = rows.plan(window(top, 170.0), gap).unwrap();
                let parts = plan.range.len()
                    + usize::from(plan.before.is_some())
                    + usize::from(plan.after.is_some());
                let actual = heights[plan.range.clone()].iter().copied().sum::<Pixels>()
                    + plan.before.unwrap_or_default()
                    + plan.after.unwrap_or_default()
                    + gap * parts.saturating_sub(1) as f32;
                assert_eq!(actual, total, "count={count}, gap={gap:?}, top={top}");
                let mut start = Pixels::ZERO;
                for (index, height) in heights.iter().enumerate() {
                    if start + *height > px(top - MARKDOWN_WINDOW_MARGIN)
                        && start < px(top + 170.0 + MARKDOWN_WINDOW_MARGIN)
                    {
                        assert!(
                            plan.range.contains(&index),
                            "visible row {index} was omitted"
                        );
                    }
                    start += *height + gap;
                }
            }
            assert_eq!(
                rows.offset_rebuilds, 1,
                "warm plans reuse the prefix ledger"
            );
        }
    }
}

#[test]
fn unknown_or_changed_heights_rebuild_the_row_plan() {
    let mut rows = Rows::new(2, Vec::new());
    rows.measure(0, px(20.0));
    assert!(rows.plan(window(0.0, 100.0), px(5.0)).is_none());
    rows.measure(1, px(30.0));
    assert!(rows.plan(window(0.0, 100.0), px(5.0)).is_some());
    rows.measure(1, px(30.0));
    assert!(rows.offsets.is_some(), "same geometry does not invalidate");
    rows.measure(1, px(40.0));
    assert!(rows.offsets.is_none());
    rows.plan(window(0.0, 100.0), px(5.0)).unwrap();
    assert_eq!(rows.offset_rebuilds, 2);
    assert_eq!(rows.offsets.unwrap()[2], px(70.0));
}

struct Harness {
    view: MarkdownView,
    selection: TranscriptSelection,
    palette: Palette,
    metrics: Metrics,
    width: f32,
    window: Option<MessageBodyWindow>,
    reference: bool,
    search: bool,
    annotation: bool,
    height: Rc<Cell<Option<Pixels>>>,
}

impl gpui::Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        self.view.set_render_width(self.width);
        let mut ctx = Ctx::new("rows", &self.palette, self.metrics, self.selection.clone());
        let marked_ordinal = (self.search || self.annotation).then(|| {
            markdown_element_texts(self.view.parser.text())
                .last()
                .unwrap()
                .0
        });
        if self.search {
            let found = TextSearchMatch {
                ordinal: marked_ordinal.unwrap(),
                range: 0..1,
            };
            ctx = ctx.with_search_highlights(SearchHighlights {
                matches: Rc::new(vec![found.clone()]),
                active: Some(found),
            });
        }
        if self.annotation {
            ctx = ctx.with_annotations(AnnotationMarks {
                marks: Rc::new(vec![AnnotationMark {
                    id: uuid::Uuid::nil(),
                    ordinal: marked_ordinal.unwrap(),
                    range: 0..1,
                    number: Some(1),
                }]),
                style: AnnotationStyle::from_palette(&self.palette),
                flash: None,
            });
        }
        let body = if self.reference {
            markdown_capped(&self.view, &ctx, usize::MAX, false)
        } else if let Some(window) = self.window {
            markdown_windowed(&self.view, &ctx, window)
        } else {
            markdown(&self.view, &ctx)
        };
        let input = self.selection.clone();
        div()
            .w(px(self.width))
            .flex()
            .items_start()
            .child(frame_reset(self.selection.clone()))
            .child(Measured {
                inner: body.unwrap_or_else(|| div().into_any_element()),
                sink: MeasureSink::Body(self.height.clone()),
            })
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

fn draw(entity: &gpui::Entity<Harness>, cx: &mut gpui::VisualTestContext) {
    cx.update(|window, cx| {
        entity.update(cx, |_, cx| cx.notify());
        window.draw(cx).clear(cx);
    });
}

type Entry = (TextKey, Rc<str>, bool, Bounds<Pixels>);

fn entries(selection: &TranscriptSelection) -> Vec<Entry> {
    selection
        .registry
        .borrow()
        .entries()
        .iter()
        .map(|entry| {
            (
                entry.key.clone(),
                entry.text.clone(),
                entry.block_break,
                entry.geometry.bounds(),
            )
        })
        .collect()
}

fn counters(view: &MarkdownView) -> (usize, usize) {
    view.row_windows
        .borrow()
        .values()
        .fold((0, 0), |(built, measured), rows| {
            let rows = rows.borrow();
            (built + rows.built, measured + rows.measured)
        })
}

fn list_source(count: usize, nested: bool) -> String {
    (0..count).map(|i| if nested {
        format!("{}. [x] Row {i}: **bold** and [a link](https://example.com).\n\n   A second paragraph with wrapped words 中文 👩🏽‍💻 e\u{301}.\n\n   - Nested child {i}\n   - Another child\n\n", i + 7)
    } else {
        format!("- Row {i}: words that wrap around the narrow transcript column.\n")
    }).collect()
}

fn table_source(count: usize) -> String {
    let mut source = "| Name | Description | Count |\n| :--- | :---: | ---: |\n".to_owned();
    for i in 0..count {
        source.push_str(&format!(
            "| **Row {i}** | {} 中文 👩🏽‍💻 | {i} |\n",
            "Wrapped cell words. ".repeat(i % 4 + 1)
        ));
    }
    source
}

#[gpui::test]
fn list_and_table_windows_preserve_geometry_and_selection_identities(cx: &mut TestAppContext) {
    for (source, metrics) in [
        (list_source(120, false), Metrics::BODY),
        (list_source(80, true), Metrics::COMPACT),
        (table_source(120), Metrics::BODY),
        (
            format!(
                "Intro.\n\n{}\n\n{}",
                "Earlier paragraph.\n\n".repeat(5),
                table_source(120)
            ),
            Metrics::COMPACT,
        ),
    ] {
        let mut view = MarkdownView::new();
        view.set_text(&source, true);
        let selection = TranscriptSelection::default();
        let height = Rc::new(Cell::new(None));
        let (entity, visual) = cx.add_window_view(|_, _| Harness {
            view,
            selection: selection.clone(),
            palette: Palette::from_theme(&Theme::dark()),
            metrics,
            width: 410.0,
            window: None,
            reference: true,
            search: false,
            annotation: false,
            height: height.clone(),
        });
        draw(&entity, visual);
        let full_height = height.get().unwrap();
        let full_entries = entries(&selection);
        assert!(!full_entries.is_empty());
        entity.update(visual, |h, _| h.reference = false);
        draw(&entity, visual);
        assert_eq!(
            height.get(),
            Some(full_height),
            "measurement wrappers preserve height"
        );
        assert_eq!(entries(&selection), full_entries);
        for top in [
            0.0,
            f32::from(full_height) * 0.5,
            f32::from(full_height) - 150.0,
            f32::from(full_height) + 2000.0,
            -2000.0,
        ] {
            let before = entity.read_with(visual, |h, _| counters(&h.view));
            entity.update(visual, |h, _| h.window = Some(window(top, 150.0)));
            draw(&entity, visual);
            assert!(
                (f32::from(height.get().unwrap() - full_height)).abs() < 0.5,
                "window at {top} changed total height"
            );
            for entry in entries(&selection) {
                let expected = full_entries.iter().find(|full| full.0 == entry.0).unwrap();
                assert_eq!(
                    &entry, expected,
                    "row identity/text/block break/geometry changed"
                );
            }
            let after = entity.read_with(visual, |h, _| counters(&h.view));
            assert!(
                after.0 - before.0 < 80,
                "warm viewport rebuilt every row: {}",
                after.0 - before.0
            );
            assert_eq!(
                after.0 - before.0,
                after.1 - before.1,
                "every built row is measured once"
            );
        }
        // A live selection, search, or annotation must restore the full registry.
        for gate in 0..3 {
            entity.update(visual, |h, _| {
                h.selection.selection.borrow_mut().clear();
                h.search = gate == 1;
                h.annotation = gate == 2;
                if gate == 0 {
                    h.selection
                        .selection
                        .borrow_mut()
                        .begin(TextKey::new("rows", 0), 0);
                }
            });
            draw(&entity, visual);
            assert_eq!(
                entries(&selection),
                full_entries,
                "fallback {gate} lost registry entries"
            );
            assert_eq!(height.get(), Some(full_height));
        }
    }
}

#[test]
fn content_style_and_width_changes_discard_row_ledgers() {
    let mut view = MarkdownView::new();
    view.set_text("- first\n- **next", true);
    view.set_render_width(700.0);
    let dark = Palette::from_theme(&Theme::dark());
    view.sync_style(&dark, &Metrics::BODY);
    let seed = |view: &MarkdownView| {
        view.row_windows
            .borrow_mut()
            .insert(0, Rc::new(RefCell::new(Rows::new(2, Vec::new()))));
    };
    seed(&view);
    view.set_text("- first\n- **next", true);
    view.set_render_width(700.0);
    view.sync_style(&dark, &Metrics::BODY);
    assert_eq!(
        view.row_windows.borrow().len(),
        1,
        "unchanged pulses reuse rows"
    );
    for (source, streaming) in [
        ("- first\n- **next word", true),
        ("- replacement", true),
        ("- replacement", false),
    ] {
        seed(&view);
        view.set_text(source, streaming);
        assert!(
            view.row_windows.borrow().is_empty(),
            "commit/rewrite/mending change reused rows"
        );
    }
    seed(&view);
    view.set_render_width(400.0);
    assert!(view.row_windows.borrow().is_empty());
    seed(&view);
    view.sync_style(&dark, &Metrics::COMPACT);
    assert!(view.row_windows.borrow().is_empty());
    seed(&view);
    view.sync_style(&Palette::from_theme(&Theme::light()), &Metrics::COMPACT);
    assert!(view.row_windows.borrow().is_empty());
}

#[gpui::test]
fn commits_and_reflows_remeasure_before_windowing_again(cx: &mut TestAppContext) {
    let mut source = table_source(100);
    let mut view = MarkdownView::new();
    view.set_text(&source, true);
    let selection = TranscriptSelection::default();
    let height = Rc::new(Cell::new(None));
    let (entity, visual) = cx.add_window_view(|_, _| Harness {
        view,
        selection: selection.clone(),
        palette: Palette::from_theme(&Theme::dark()),
        metrics: Metrics::BODY,
        width: 450.0,
        window: Some(window(1500.0, 150.0)),
        reference: false,
        search: false,
        annotation: false,
        height: height.clone(),
    });
    for revision in 0..7 {
        entity.update(visual, |h, _| {
            h.reference = false;
            match revision {
                1 => {
                    source.push_str(&format!("| {} | short | 101 |\n", "wide name ".repeat(20)));
                    h.view.set_text(&source, true);
                    // A real commit may arrive without viewport bounds.
                    h.window = None;
                }
                2 => {
                    source.push('|');
                    h.view.set_text(&source, true);
                    h.window = Some(window(1500.0, 150.0));
                }
                3 => {
                    source.push_str(" partial row | repaired cell | 102 |\n");
                    h.view.set_text(&source, true);
                }
                4 => {
                    h.width = 320.0;
                    h.window = Some(window(1500.0, 150.0));
                }
                5 => {
                    h.metrics = Metrics::COMPACT;
                    h.palette = Palette::from_theme(&Theme::light());
                }
                6 => {
                    source = list_source(100, false);
                    source.push_str("- **mended last item");
                    h.view.set_text(&source, true);
                }
                // add_window_view may have painted already; explicitly start
                // this assertion from unknown geometry rather than relying on
                // the test platform's initial draw scheduling.
                _ => h.view.forget_measured_geometry(),
            }
        });
        draw(&entity, visual);
        let (built, measured) = entity.read_with(visual, |h, _| counters(&h.view));
        assert!(
            built >= 100,
            "cold/revision {revision} failed to measure the full container"
        );
        assert_eq!(built, measured);
        let measured_height = height.get().unwrap();
        let measured_entries = entries(&selection);
        entity.update(visual, |h, _| h.reference = true);
        draw(&entity, visual);
        assert_eq!(
            height.get(),
            Some(measured_height),
            "revision {revision} changed full geometry"
        );
        assert_eq!(entries(&selection), measured_entries);
        entity.update(visual, |h, _| {
            h.reference = false;
            h.window = Some(window(1500.0, 150.0));
        });
        draw(&entity, visual);
        let after = entity.read_with(visual, |h, _| counters(&h.view));
        assert!(
            after.0 - built < 90,
            "revision {revision} never returned to bounded row work"
        );
        assert_eq!(height.get(), Some(measured_height));
        for entry in entries(&selection) {
            assert!(
                measured_entries.contains(&entry),
                "revision {revision} changed a row's canonical text/geometry"
            );
        }
    }
}

#[gpui::test]
fn clicking_a_windowed_row_restores_full_selection_geometry(cx: &mut TestAppContext) {
    let source = list_source(120, false);
    let mut view = MarkdownView::new();
    view.set_text(&source, true);
    let selection = TranscriptSelection::default();
    let (entity, visual) = cx.add_window_view(|_, _| Harness {
        view,
        selection: selection.clone(),
        palette: Palette::from_theme(&Theme::dark()),
        metrics: Metrics::BODY,
        width: 410.0,
        window: None,
        reference: false,
        search: false,
        annotation: false,
        height: Rc::new(Cell::new(None)),
    });
    // Give the synthetic drag destination a real paint-time hit region. The
    // planner's deliberately smaller window can still omit those lower rows.
    visual.simulate_resize(size(px(700.0), px(6000.0)));
    draw(&entity, visual);
    let full = entries(&selection);
    entity.update(visual, |h, _| h.window = Some(window(0.0, 150.0)));
    draw(&entity, visual);
    let visible = entries(&selection);
    assert!(visible.len() < full.len());
    let first = &visible[0];
    let origin = first.3.origin + point(px(2.0), px(5.0));
    visual.simulate_mouse_down(origin, MouseButton::Left, gpui::Modifiers::none());
    draw(&entity, visual);
    assert_eq!(selection.selection.borrow().anchor(), Some(&first.0));
    assert_eq!(
        entries(&selection),
        full,
        "first click must restore the full registry before extension"
    );
    let destination = full[40].3.origin + point(px(20.0), px(5.0));
    visual.simulate_mouse_move(destination, MouseButton::Left, gpui::Modifiers::none());
    draw(&entity, visual);
    let selected = selection.selection.borrow().selected_text().unwrap();
    assert!(selected.starts_with("Row 0"));
    assert!(
        selected.contains("Row 39"),
        "drag must cross the formerly hidden rows"
    );
    visual.simulate_mouse_up(destination, MouseButton::Left, gpui::Modifiers::none());
}

#[gpui::test]
fn hidden_row_veils_expire_and_reentry_does_not_restart_them(cx: &mut TestAppContext) {
    let mut source = list_source(120, false);
    let mut view = MarkdownView::new();
    view.set_text(&source, true);
    let (entity, visual) = cx.add_window_view(|_, _| Harness {
        view,
        selection: TranscriptSelection::default(),
        palette: Palette::from_theme(&Theme::dark()),
        metrics: Metrics::BODY,
        width: 410.0,
        window: None,
        reference: false,
        search: false,
        annotation: false,
        height: Rc::new(Cell::new(None)),
    });
    draw(&entity, visual);
    entity.update(visual, |h, _| h.window = Some(window(100_000.0, 100.0)));
    draw(&entity, visual);
    entity.update(visual, |h, _| {
        h.view
            .veil
            .borrow_mut()
            .finish_frame(Instant::now() + Duration::from_secs(1));
        assert!(
            !h.view.veil.borrow().is_fading(),
            "hidden fades must release their animation lease"
        );
        h.window = Some(window(0.0, 150.0));
    });
    draw(&entity, visual);
    entity.read_with(visual, |h, _| {
        assert!(
            !h.view.veil.borrow().is_fading(),
            "returning rows must not flash again"
        )
    });
    source.push_str("- brand new tail item\n");
    entity.update(visual, |h, _| h.view.set_text(&source, true));
    draw(&entity, visual);
    entity.read_with(visual, |h, _| {
        assert!(
            h.view.veil.borrow().is_fading(),
            "new commit text must still dissolve"
        )
    });
    entity.update(visual, |h, _| {
        h.view.set_text(&source, false);
        h.window = None;
    });
    draw(&entity, visual);
    entity.read_with(visual, |h, _| {
        assert!(
            h.view.row_windows.borrow().is_empty(),
            "settled bodies skip the row ledger"
        );
        assert!(!h.view.veil.borrow().is_fading());
    });
    entity.update(visual, |h, _| {
        h.view.set_text(&source, true);
        h.window = Some(window(0.0, 150.0));
    });
    draw(&entity, visual);
    entity.read_with(visual, |h, _| {
        assert!(!h.view.row_windows.borrow().is_empty());
        assert!(
            !h.view.veil.borrow().is_fading(),
            "resuming must adopt existing content without flashing it"
        );
    });
}

/// Same-process full-render reference versus warm viewport work. These are
/// GPUI test-platform timings, not native display FPS or real-provider runs.
#[gpui::test]
#[ignore = "manual performance measurement"]
fn bench_row_window_frames(cx: &mut TestAppContext) {
    let paragraphs = (0..400)
        .map(|i| format!("Paragraph {i} with words for shaping and wrapping.\n\n"))
        .collect::<String>();
    for (label, mut source, append) in [
        (
            "400 list items",
            list_source(400, false),
            "- appended item\n",
        ),
        (
            "400 table rows",
            table_source(400),
            "| appended | cell | 401 |\n",
        ),
        (
            "400 paragraph blocks",
            paragraphs,
            "Appended paragraph.\n\n",
        ),
        (
            "one giant paragraph",
            "Words in one giant wrapped paragraph. ".repeat(1200),
            " appended",
        ),
        (
            "one giant fence",
            format!(
                "```text\n{}",
                "let value = another_value + 1;\n".repeat(1200)
            ),
            "let appended = 1;\n",
        ),
    ] {
        let mut view = MarkdownView::new();
        view.set_text(&source, true);
        view.seed_streaming_baseline();
        let height = Rc::new(Cell::new(None));
        let (entity, visual) = cx.add_window_view(|_, _| Harness {
            view,
            selection: TranscriptSelection::default(),
            palette: Palette::from_theme(&Theme::dark()),
            metrics: Metrics::BODY,
            width: 700.0,
            window: Some(window(0.0, 800.0)),
            reference: false,
            search: false,
            annotation: false,
            height: height.clone(),
        });
        let start = Instant::now();
        entity.update(visual, |h, _| h.view.forget_measured_geometry());
        draw(&entity, visual);
        println!(
            "{label}: cold measurement={:?}, rows={:?}",
            start.elapsed(),
            entity.read_with(visual, |h, _| counters(&h.view))
        );
        let top = (f32::from(height.get().unwrap()) - 900.0).max(0.0);
        entity.update(visual, |h, _| {
            h.reference = true;
            h.window = Some(window(top, 800.0));
        });
        draw(&entity, visual);
        const FRAMES: u32 = 30;
        let start = Instant::now();
        for _ in 0..FRAMES {
            draw(&entity, visual);
        }
        println!(
            "{label}: full reference per-frame={:?}",
            start.elapsed() / FRAMES
        );
        entity.update(visual, |h, _| h.reference = false);
        draw(&entity, visual);
        let before = entity.read_with(visual, |h, _| counters(&h.view));
        let start = Instant::now();
        for _ in 0..FRAMES {
            draw(&entity, visual);
        }
        let elapsed = start.elapsed() / FRAMES;
        let after = entity.read_with(visual, |h, _| counters(&h.view));
        println!(
            "{label}: warm windowed per-frame={elapsed:?}, built/measured rows per-frame={}/{}",
            (after.0 - before.0) / FRAMES as usize,
            (after.1 - before.1) / FRAMES as usize
        );
        source.push_str(append);
        entity.update(visual, |h, _| h.view.set_text(&source, true));
        let start = Instant::now();
        draw(&entity, visual);
        println!(
            "{label}: post-commit measurement={:?}, rows={:?}",
            start.elapsed(),
            entity.read_with(visual, |h, _| counters(&h.view))
        );
    }
}
