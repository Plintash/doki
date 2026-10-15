//! A search-first conversation picker. Catalog work happens once on opening;
//! filtering happens only when the query changes, and rendering is O(visible).

use super::*;
use crate::ui::scrollbar::Scrollable;

gpui::actions!(
    task_attach_picker,
    [FirstEntry, LastEntry, PreviousPage, NextPage]
);

pub(super) fn init(cx: &mut App) {
    let context = Some("TaskAttachPicker > TextInput");
    cx.bind_keys([
        gpui::KeyBinding::new("home", FirstEntry, context),
        gpui::KeyBinding::new("end", LastEntry, context),
        gpui::KeyBinding::new("pageup", PreviousPage, context),
        gpui::KeyBinding::new("pagedown", NextPage, context),
    ]);
}

const ROW_HEIGHT: f32 = 54.0;
const MAX_LIST_HEIGHT: f32 = 324.0;

#[derive(Clone)]
struct AttachEntry {
    id: Uuid,
    title: SharedString,
    detail: SharedString,
    search: String,
    provider: ProviderKind,
}

/// This predicate is shared by the opening snapshot and the final mutation.
/// A stale picker must never steal a conversation filed under another issue.
fn eligible_session(session: &AgentSession, project_id: Option<Uuid>) -> bool {
    session.task_id.is_none() && project_id.is_none_or(|id| id == session.project_id)
}

fn matching_entries(entries: &[AttachEntry], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    let terms = query.split_whitespace().collect::<Vec<_>>();
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            terms
                .iter()
                .all(|term| entry.search.contains(term))
                .then_some(index)
        })
        .collect()
}

fn next_highlight(current: Option<usize>, count: usize, forward: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match current {
        Some(index) if forward => (index + 1).min(count - 1),
        Some(index) => index.saturating_sub(1).min(count - 1),
        None if forward => 0,
        None => count - 1,
    })
}

// Pinned GPUI invalidates height hints when width is first measured. Only
// nearby, currently rendered rows can use pixel-based nearest-edge reveal;
// distant jumps must anchor by index instead of walking unmeasured heights.
fn reveal_picker_row(list: &ListState, rendered: Option<(usize, usize)>, index: usize) {
    if rendered.is_some_and(|(first, last)| (first..=last).contains(&index)) {
        list.scroll_to_reveal_item(index);
    } else {
        list.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: px(0.0),
        });
    }
}

/// Fixed-height rows give the scrollbar an exact full-catalog extent even
/// after GPUI invalidates measurement hints. Dragging stays O(log n) by index.
#[derive(Clone)]
struct AttachScrollSurface {
    list: ListState,
    count: usize,
}

impl Scrollable for AttachScrollSurface {
    fn viewport_height(&self) -> Pixels {
        self.list.viewport_bounds().size.height
    }

    fn max_offset(&self) -> Pixels {
        (px(self.count as f32 * ROW_HEIGHT) - self.viewport_height()).max(px(0.0))
    }

    fn scrolled(&self) -> Pixels {
        let offset = self.list.logical_scroll_top();
        (px(offset.item_ix as f32 * ROW_HEIGHT) + offset.offset_in_item)
            .clamp(px(0.0), self.max_offset())
    }

    fn scroll_to(&self, offset: Pixels) {
        let offset = offset.clamp(px(0.0), self.max_offset());
        let item_ix = (offset / px(ROW_HEIGHT)).floor() as usize;
        self.list.scroll_to(ListOffset {
            item_ix,
            offset_in_item: offset - px(item_ix as f32 * ROW_HEIGHT),
        });
    }
}

pub(super) struct TaskAttachPicker {
    task_id: Uuid,
    owner: WeakEntity<Waku>,
    search: Entity<TextInput>,
    handle: ContextMenuHandle,
    entries: Rc<Vec<AttachEntry>>,
    matches: Rc<Vec<usize>>,
    scope: SharedString,
    highlighted: Option<usize>,
    list: ListState,
    scrollbar: Rc<ScrollbarState>,
    rendered_rows: Rc<Cell<Option<(usize, usize)>>>,
}

impl TaskAttachPicker {
    pub(super) fn new(
        task_id: Uuid,
        owner: WeakEntity<Waku>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(window, cx)
                .semantic_id("task-attach-search")
                .tab_stop(true)
                .placeholder(tr!("board.attach_search"))
        });
        cx.subscribe(&search, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                this.filter(input.read(cx).content());
                cx.notify();
            }
        })
        .detach();
        let picker = cx.entity().downgrade();
        let base_handle = ContextMenuHandle::new(cx);
        let trigger_focus = base_handle.trigger_focus_handle().clone();
        let search_focus = search.read(cx).focus();
        let handle = base_handle.on_toggle(move |open, window, cx| {
            if !open {
                // Preserve an outside click's destination, but restore the
                // trigger after keyboard dismissal or a chosen result.
                if search_focus.is_focused(window) {
                    window.focus(&trigger_focus, cx);
                }
                return;
            }
            let _ = picker.update(cx, |this, cx| this.refresh(cx));
            let picker = picker.clone();
            let focus = search_focus.clone();
            // Deferred popovers join the key-dispatch tree after drawing.
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |window, cx| {
                    if picker
                        .upgrade()
                        .is_some_and(|picker| picker.read(cx).handle.is_open())
                    {
                        window.focus(&focus, cx);
                    }
                });
            });
        });
        Self {
            task_id,
            owner,
            search,
            handle,
            entries: Rc::new(Vec::new()),
            matches: Rc::new(Vec::new()),
            scope: SharedString::default(),
            highlighted: None,
            list: ListState::new(0, ListAlignment::Top, px(ROW_HEIGHT))
                .with_uniform_item_height(px(ROW_HEIGHT)),
            scrollbar: ScrollbarState::new(),
            rendered_rows: Rc::new(Cell::new(None)),
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let owner = owner.read(cx);
        let Some(task) = owner
            .state
            .tasks
            .iter()
            .find(|task| task.id == self.task_id)
        else {
            return;
        };
        let project_id = task_project_id(task);
        let projects = owner
            .state
            .projects
            .iter()
            .map(|project| (project.id, project.display_name()))
            .collect::<HashMap<_, _>>();
        self.scope = project_id
            .and_then(|id| projects.get(&id))
            .map(|project| tr!("board.attach_project_scope", project = project))
            .unwrap_or_else(|| tr!("board.attach_all_scope"))
            .into();
        let mut sessions = owner
            .state
            .sessions
            .iter()
            .filter(|session| eligible_session(session, project_id))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| (std::cmp::Reverse(session.updated_at), session.id));
        self.entries = Rc::new(
            sessions
                .into_iter()
                .map(|session| {
                    let title = single_line_label(session.display_title());
                    let project = projects
                        .get(&session.project_id)
                        .cloned()
                        .unwrap_or_default();
                    let workspace = match &session.workspace {
                        SessionWorkspace::Local => tr!("workspace.local"),
                        SessionWorkspace::NewWorktree { .. } => tr!("workspace.new_worktree"),
                        SessionWorkspace::Worktree { branch, path } => {
                            format!("{} · {}", branch, compact_path(path))
                        }
                    };
                    let detail = single_line_label(&format!("{project} · {workspace}"));
                    AttachEntry {
                        id: session.id,
                        search: format!("{title} {detail}").to_lowercase(),
                        title: title.into(),
                        detail: detail.into(),
                        provider: session.provider,
                    }
                })
                .collect(),
        );
        self.search.update(cx, |input, cx| input.clear(cx));
        self.filter("");
        cx.notify();
    }

    fn filter(&mut self, query: &str) {
        self.matches = Rc::new(matching_entries(&self.entries, query));
        self.highlighted = (!self.matches.is_empty()).then_some(0);
        self.list
            .reset_with_uniform_height(self.matches.len(), px(ROW_HEIGHT));
    }

    fn move_highlight(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.highlighted = next_highlight(self.highlighted, self.matches.len(), forward);
        if let Some(index) = self.highlighted {
            reveal_picker_row(&self.list, self.rendered_rows.get(), index);
        }
        cx.notify();
    }

    fn jump_to(&mut self, index: usize, cx: &mut Context<Self>) {
        self.highlighted =
            (!self.matches.is_empty()).then_some(index.min(self.matches.len().saturating_sub(1)));
        if let Some(index) = self.highlighted {
            reveal_picker_row(&self.list, self.rendered_rows.get(), index);
        }
        cx.notify();
    }

    fn attach(&mut self, id: Uuid, cx: &mut Context<Self>) -> bool {
        let task_id = self.task_id;
        let attached = self
            .owner
            .update(cx, |owner, cx| {
                if !owner.board_open || owner.selected_task != Some(task_id) {
                    return false;
                }
                let Some(task) = owner.state.tasks.iter().find(|task| task.id == task_id) else {
                    return false;
                };
                let project_id = task_project_id(task);
                if !owner
                    .state
                    .sessions
                    .iter()
                    .any(|session| session.id == id && eligible_session(session, project_id))
                {
                    return false;
                }
                owner.bind_session_to_task(id, task_id, cx);
                true
            })
            .unwrap_or(false);
        if !attached {
            // The catalog may have changed while open. Refresh instead of
            // applying a cached target or leaving an obsolete result visible.
            self.refresh(cx);
        }
        attached
    }
}

impl Render for TaskAttachPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let viewport = window.viewport_size();
        let width = px(420.0).min((viewport.width - px(32.0)).max(px(200.0)));
        let list_height = px(MAX_LIST_HEIGHT)
            .min((viewport.height - px(140.0)).max(px(ROW_HEIGHT)))
            .min(px(self.matches.len() as f32 * ROW_HEIGHT));
        let page_size = (list_height / px(ROW_HEIGHT)).floor().max(1.0) as usize;
        let entries = self.entries.clone();
        let matches = self.matches.clone();
        let highlighted = self.highlighted;
        let picker = cx.entity().downgrade();
        let confirm_picker = picker.clone();
        let confirm_handle = self.handle.clone();
        let row_handle = self.handle.clone();
        let rendered_rows = self.rendered_rows.clone();
        rendered_rows.set(None);
        let rows = if matches.is_empty() {
            div()
                .h(px(90.0))
                .px(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(sp(12.5))
                .text_color(theme.text_tertiary)
                .child(if entries.is_empty() {
                    tr!("board.no_unfiled_sessions")
                } else {
                    tr!("board.attach_no_matches")
                })
                .into_any_element()
        } else {
            div()
                .relative()
                .w_full()
                .h(list_height)
                .px(px(5.0))
                .child(
                    list(self.list.clone(), move |index, _, _| {
                        rendered_rows.set(Some(
                            rendered_rows.get().map_or((index, index), |(first, last)| {
                                (first.min(index), last.max(index))
                            }),
                        ));
                        let Some(entry) = matches.get(index).and_then(|index| entries.get(*index))
                        else {
                            return div().into_any_element();
                        };
                        let id = entry.id;
                        let picker = picker.clone();
                        let handle = row_handle.clone();
                        div()
                            .id(SharedString::from(format!("task-attach-{id}")))
                            .semantic_control(gpui::Role::Button, false)
                            .aria_label(format!("{} · {}", entry.title, entry.detail))
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .px(px(9.0))
                            .rounded(px(7.0))
                            .flex()
                            .items_center()
                            .gap(px(9.0))
                            .cursor_pointer()
                            .when(highlighted == Some(index), |row| {
                                row.bg(theme.overlay_strong)
                            })
                            .hover(|row| row.bg(theme.overlay))
                            .active(|row| row.bg(theme.overlay_strong))
                            .child(provider_mark(entry.provider, 15.0, theme.text_tertiary))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .w_full()
                                            .truncate()
                                            .text_size(sp(13.0))
                                            .line_height(sp(18.0))
                                            .text_color(theme.text)
                                            .child(entry.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .truncate()
                                            .text_size(sp(11.5))
                                            .line_height(sp(16.0))
                                            .text_color(theme.text_tertiary)
                                            .child(entry.detail.clone()),
                                    ),
                            )
                            .on_click(move |_, window, cx| {
                                if picker
                                    .update(cx, |this, cx| this.attach(id, cx))
                                    .unwrap_or(false)
                                {
                                    handle.close(window, cx);
                                    window.refresh();
                                }
                            })
                            .into_any_element()
                    })
                    .size_full(),
                )
                .child(scrollbar::vertical(
                    &AttachScrollSurface {
                        list: self.list.clone(),
                        count: self.matches.len(),
                    },
                    &self.scrollbar,
                ))
                .into_any_element()
        };
        div()
            .id("task-detail-attach-menu")
            .key_context("TaskAttachPicker")
            .w(width)
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.raised)
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .on_action(
                cx.listener(|this, _: &SelectNextEntry, _, cx| this.move_highlight(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectPreviousEntry, _, cx| this.move_highlight(false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectNextTab, _, cx| this.move_highlight(true, cx)))
            .on_action(
                cx.listener(|this, _: &SelectPreviousTab, _, cx| this.move_highlight(false, cx)),
            )
            .on_action(cx.listener(|this, _: &FirstEntry, _, cx| this.jump_to(0, cx)))
            .on_action(cx.listener(|this, _: &LastEntry, _, cx| {
                this.jump_to(this.matches.len().saturating_sub(1), cx);
            }))
            .on_action(cx.listener(move |this, _: &PreviousPage, _, cx| {
                this.jump_to(this.highlighted.unwrap_or(0).saturating_sub(page_size), cx);
            }))
            .on_action(cx.listener(move |this, _: &NextPage, _, cx| {
                this.jump_to(this.highlighted.unwrap_or(0).saturating_add(page_size), cx);
            }))
            .on_action(move |_: &ConfirmEntry, window, cx| {
                if confirm_picker
                    .update(cx, |this, cx| {
                        // Key events can arrive before the next paint. Resolve
                        // from live picker state, never a render-time selection.
                        let id = this
                            .highlighted
                            .and_then(|index| this.matches.get(index))
                            .and_then(|index| this.entries.get(*index))
                            .map(|entry| entry.id);
                        id.is_some_and(|id| this.attach(id, cx))
                    })
                    .unwrap_or(false)
                {
                    confirm_handle.close(window, cx);
                    window.refresh();
                }
            })
            .child(
                div().p(px(10.0)).child(
                    TextField::new("task-attach-search-field", self.search.clone())
                        .icon("icons/search.svg", 14.0)
                        .w_full(),
                ),
            )
            .child(
                div()
                    .px(px(14.0))
                    .pb(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(sp(11.5))
                    .text_color(theme.text_tertiary)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(self.scope.clone()),
                    )
                    .child(div().flex_none().child(self.matches.len().to_string())),
            )
            .child(rows)
            .child(
                div()
                    .mt(px(5.0))
                    .px(px(14.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(sp(10.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!("board.attach_keyboard_hint")),
            )
    }
}

impl Waku {
    pub(super) fn render_task_attach_picker(
        &mut self,
        _task: &Task,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(detail) = self.task_detail.as_ref() else {
            return div().into_any_element();
        };
        let picker = detail.attach_picker.clone();
        let handle = picker.read(cx).handle.clone();
        popover(
            icon_button("task-detail-attach", "icons/paperclip.svg", theme)
                .size(px(28.0))
                .relative()
                .child(crate::ui::tooltip::FocusHint::new(
                    handle.trigger_focus_handle().clone(),
                    tr!("board.attach_session"),
                ))
                .aria_label(tr!("board.attach_session"))
                .tooltip(Tooltip::text(tr!("board.attach_session")))
                .focus_visible(|style| {
                    style
                        .bg(theme.overlay_strong)
                        .border_1()
                        .border_color(theme.accent)
                }),
            &handle,
            MenuAlign::BelowLeft,
            move |_, _, _| picker.clone().into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, detail: &str) -> AttachEntry {
        AttachEntry {
            id: Uuid::new_v4(),
            title: title.to_owned().into(),
            detail: detail.to_owned().into(),
            search: format!("{title} {detail}").to_lowercase(),
            provider: ProviderKind::Claude,
        }
    }

    #[test]
    fn attach_search_matches_title_project_and_workspace_with_unicode() {
        let entries = vec![
            entry("帮我 build 最新版", "Doki · fix/dialog"),
            entry("Réparer la recherche", "Other · Local"),
            entry("Build docs", "Doki · main"),
        ];
        assert_eq!(matching_entries(&entries, ""), vec![0, 1, 2]);
        assert_eq!(matching_entries(&entries, " DOKI  build "), vec![0, 2]);
        assert_eq!(matching_entries(&entries, "dialog 帮我"), vec![0]);
        assert_eq!(matching_entries(&entries, "RÉPARER"), vec![1]);
        assert!(matching_entries(&entries, "missing").is_empty());
    }

    #[test]
    fn attach_eligibility_rejects_reassigned_or_cross_project_sessions() {
        let project = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut session = AgentSession::new(project, ProviderKind::Claude);
        assert!(eligible_session(&session, Some(project)));
        assert!(eligible_session(&session, None));
        assert!(!eligible_session(&session, Some(other)));
        session.task_id = Some(Uuid::new_v4());
        assert!(!eligible_session(&session, Some(project)));
        assert!(!eligible_session(&session, None));
        session.task_id = None;
        session.project_id = other;
        assert!(!eligible_session(&session, Some(project)));
    }

    #[test]
    fn attach_keyboard_highlight_stays_inside_filtered_results() {
        assert_eq!(next_highlight(None, 0, true), None);
        assert_eq!(next_highlight(None, 2_000, true), Some(0));
        assert_eq!(next_highlight(None, 2_000, false), Some(1_999));
        assert_eq!(next_highlight(Some(0), 2_000, false), Some(0));
        assert_eq!(next_highlight(Some(0), 2_000, true), Some(1));
        assert_eq!(next_highlight(Some(1_999), 2_000, true), Some(1_999));
        assert_eq!(next_highlight(Some(1_999), 2, false), Some(1));
    }

    struct PickerListHarness {
        rows: ListState,
        rendered: Rc<RefCell<Vec<usize>>>,
    }

    impl Render for PickerListHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let rendered = self.rendered.clone();
            div().w(px(420.0)).h(px(MAX_LIST_HEIGHT)).child(
                list(self.rows.clone(), move |index, _, _| {
                    rendered.borrow_mut().push(index);
                    div().w_full().h(px(ROW_HEIGHT)).into_any_element()
                })
                .size_full(),
            )
        }
    }

    #[gpui::test]
    fn attach_picker_keeps_large_catalog_and_keyboard_reveal_virtualized(
        cx: &mut gpui::TestAppContext,
    ) {
        let rows = ListState::new(10_000, ListAlignment::Top, px(ROW_HEIGHT))
            .with_uniform_item_height(px(ROW_HEIGHT));
        let rendered = Rc::new(RefCell::new(Vec::new()));
        let (_view, cx) = cx.add_window_view(|_, _| PickerListHarness {
            rows: rows.clone(),
            rendered: rendered.clone(),
        });
        cx.run_until_parked();
        assert!(rendered.borrow().contains(&0));
        assert!(rendered.borrow().len() < 32);
        let scrollbar = AttachScrollSurface {
            list: rows.clone(),
            count: 10_000,
        };
        assert_eq!(
            scrollbar.max_offset(),
            px(10_000.0 * ROW_HEIGHT - MAX_LIST_HEIGHT)
        );
        rendered.borrow_mut().clear();
        scrollbar.scroll_to(scrollbar.max_offset());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(rendered.borrow().contains(&9_999));
        assert!(rendered.borrow().len() < 32);
        assert_eq!(scrollbar.scrolled(), scrollbar.max_offset());
        rows.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        rendered.borrow_mut().clear();
        reveal_picker_row(&rows, Some((0, 10)), 9_999);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let rendered = rendered.borrow();
        assert!(rendered.contains(&9_999));
        assert!(
            rendered.len() < 32,
            "only a viewport should render: {}",
            rendered.len()
        );
        assert!(rendered.iter().all(|index| *index > 9_980));
    }
}
