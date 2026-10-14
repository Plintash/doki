//! Spike: validate the automation taps added to our GPUI fork.
//!
//! Builds a window whose accessibility tree is forced on without any
//! assistive technology attached, reads the tree back with stable node ids,
//! and drives a control through an accessibility action — the exact path a
//! screen reader would take, with no coordinates chosen by the caller.

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    ClickEvent, Context, FocusHandle, IntoElement, ParentElement, Render, TestAppContext, Window,
    div, px,
};

struct SpikeView {
    clicks: Rc<Cell<u32>>,
    focus: FocusHandle,
}

impl Render for SpikeView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let clicks = self.clicks.clone();
        div().size_full().p(px(20.0)).child(
            div()
                .id("spike.button")
                .role(gpui::Role::Button)
                .aria_label("Press me")
                .track_focus(&self.focus)
                .tab_index(0)
                .tab_stop(true)
                .w(px(120.0))
                .h(px(40.0))
                .on_click(move |_: &ClickEvent, _window, _cx| {
                    clicks.set(clicks.get() + 1);
                })
                .child("Press me"),
        )
    }
}

fn build_view(
    clicks: Rc<Cell<u32>>,
    automation: bool,
) -> impl FnOnce(&mut Window, &mut Context<SpikeView>) -> SpikeView {
    move |window, cx| {
        if automation {
            window.set_a11y_automation(true);
        }
        SpikeView {
            clicks,
            focus: cx.focus_handle(),
        }
    }
}

#[gpui::test]
fn automation_builds_the_tree_and_dispatches_actions(cx: &mut TestAppContext) {
    let clicks = Rc::new(Cell::new(0));
    let (_, cx) = cx.add_window_view(build_view(clicks.clone(), false));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();

    // Without automation nothing asked for a tree, and the test platform never
    // activates accessibility, so there is no snapshot to read.
    let idle = cx.update(|window, _| window.a11y_automation_snapshot());
    assert!(
        idle.is_none(),
        "no tree should exist before automation is on"
    );

    // Turn automation on and let the next frame build the tree.
    cx.update(|window, _| window.set_a11y_automation(true));
    cx.run_until_parked();

    let snapshot = cx
        .update(|window, _| window.a11y_automation_snapshot())
        .expect("automation keeps a tree for every frame");
    let json: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    let nodes = json["nodes"].as_array().expect("nodes array");

    let button = nodes
        .iter()
        .find(|node| node["label"] == "Press me")
        .unwrap_or_else(|| panic!("button node missing from snapshot: {snapshot}"));
    assert_eq!(button["role"], "Button");
    assert!(
        button["element_id"]
            .as_str()
            .unwrap_or_default()
            .contains("spike.button"),
        "the node should carry its element provenance: {button}"
    );
    let actions = button["actions"].as_array().expect("button actions");
    assert!(
        actions.iter().any(|action| action == "Click"),
        "clickable node should support Click: {button}"
    );
    assert!(
        actions.iter().any(|action| action == "Focus"),
        "focusable node should support Focus: {button}"
    );
    let bounds = &button["bounds"];
    assert!(
        bounds["width"].as_f64().unwrap() > 0.0 && bounds["height"].as_f64().unwrap() > 0.0,
        "the frame should have given the node layout bounds: {button}"
    );

    let node_id: u64 = button["node"].as_str().unwrap().parse().unwrap();
    let node_id = gpui::accesskit::NodeId(node_id);

    // Dispatch the click a screen reader would: listener first, then the
    // built-in click-at-bounds fallback.
    cx.update(|window, cx| {
        window.dispatch_a11y_action(node_id, gpui::AccessibleAction::Click, None, cx);
    });
    cx.run_until_parked();
    assert_eq!(clicks.get(), 1, "the click listener should have run");

    // And the same route for focus.
    cx.update(|window, cx| {
        window.dispatch_a11y_action(node_id, gpui::AccessibleAction::Focus, None, cx);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let snapshot = cx
        .update(|window, _| window.a11y_automation_snapshot())
        .expect("snapshot after focus");
    let json: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(
        json["focus"].as_str(),
        Some(node_id.0.to_string().as_str()),
        "the focused node should be reported in the snapshot"
    );
}
