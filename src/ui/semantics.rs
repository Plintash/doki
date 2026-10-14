//! Small, local accessibility contract for the shared app primitives.
//!
//! The pinned GPUI fork has no public `aria_disabled` setter, but its subtree
//! builder can annotate the real parent node. This is not a parallel tree.
//! Hand-written id-less, role-less Divs never enter that tree and cannot be
//! audited here; source guards and the core smoke scenario cover our callsites.
use gpui::{ElementId, Role, StatefulInteractiveElement};

pub trait SemanticControlExt: StatefulInteractiveElement {
    #[track_caller]
    fn semantic_control(mut self, role: Role, disabled: bool) -> Self {
        #[cfg(debug_assertions)]
        if missing_identity(self.interactivity().element_id.as_ref()) {
            crate::app::rpa::daemon::identity_warning(format!(
                "interactive primitive lacks an id at {} (role {role:?})",
                std::panic::Location::caller()
            ));
        }
        self = self.role(role);
        self.a11y_synthetic_children(move |builder| {
            if disabled {
                builder.parent_node().set_disabled();
            }
        })
    }
}
#[cfg(debug_assertions)]
fn missing_identity(id: Option<&ElementId>) -> bool {
    match id {
        None => true,
        Some(
            ElementId::Name(name)
            | ElementId::NamedInteger(name, _)
            | ElementId::NamedChild(_, name),
        ) => name.trim().is_empty(),
        Some(_) => false,
    }
}

impl<T: StatefulInteractiveElement> SemanticControlExt for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, Window, div, prelude::*, px};

    #[cfg(debug_assertions)]
    #[test]
    fn missing_primitive_identity_is_reportable_without_string_allocations() {
        assert!(missing_identity(None));
        assert!(missing_identity(Some(&ElementId::Name("".into()))));
        assert!(!missing_identity(Some(&ElementId::Name(
            "settings.save".into()
        ))));
    }

    struct SemanticFixture;
    impl Render for SemanticFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .id("semantic.disabled")
                    .semantic_control(Role::Button, true)
                    .aria_label("Unavailable control")
                    .w(px(40.0))
                    .h(px(20.0)),
            )
        }
    }

    #[gpui::test]
    fn semantic_disabled_is_in_the_real_gpui_tree(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(|window, _| {
            window.set_a11y_automation(true);
            SemanticFixture
        });
        cx.run_until_parked();
        let raw = cx
            .update(|window, _| window.a11y_automation_snapshot())
            .unwrap();
        let tree: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let node = tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["element_id"] == "semantic.disabled")
            .unwrap();
        assert_eq!(node["role"], "Button");
        assert_eq!(node["disabled"], true);
        assert_eq!(node["label"], "Unavailable control");
    }
}
