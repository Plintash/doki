//! Isolation: do virtualized `list()` rows contribute accessibility nodes?
//!
//! The live app's sidebar and transcript are built on `list()`, and the RPA
//! snapshot saw no rows from either. This pins down whether the framework
//! emits them at all before the bridge tries to work around it.

use gpui::prelude::*;
use gpui::{
    Context, IntoElement, ListAlignment, ListState, ParentElement, Render, TestAppContext, Window,
    div, list, px,
};

struct ListView {
    state: ListState,
}

impl Render for ListView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            list(self.state.clone(), move |index, _window, _cx| {
                div()
                    .id(gpui::ElementId::Name(format!("row.{index}").into()))
                    .role(gpui::Role::Button)
                    .aria_label(format!("Row {index}"))
                    .h(px(44.0))
                    .w_full()
                    .child(format!("Row {index}"))
                    .into_any_element()
            })
            .w_full()
            .h_full(),
        )
    }
}

#[gpui::test]
fn list_rows_are_reported(cx: &mut TestAppContext) {
    let state = ListState::new(0, ListAlignment::Top, px(400.0));
    state.reset_with_uniform_height(5, px(44.0));
    let view = move |_window: &mut Window, _cx: &mut Context<ListView>| ListView {
        state: state.clone(),
    };
    let (_, cx) = cx.add_window_view(view);
    cx.update(|window, _| window.set_a11y_automation(true));
    cx.run_until_parked();
    let rows_on = |cx: &mut gpui::VisualTestContext| {
        let snapshot = cx
            .update(|window, _| window.a11y_automation_snapshot())
            .expect("snapshot");
        let json: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
        json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| {
                node["element_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("row."))
            })
            .count()
    };
    let first = rows_on(cx);
    // The bridge forces a full refresh + draw per call; make sure a second
    // forced frame still reports the rows (that is the sequence the app sees).
    cx.update(|window, cx| {
        window.refresh();
        let _ = window.draw(cx);
    });
    cx.run_until_parked();
    let second = rows_on(cx);
    println!("rows: first={first} second={second}");
    assert_eq!(first, 5, "first frame should report every row");
    assert_eq!(second, 5, "a refreshed frame should still report every row");
}
