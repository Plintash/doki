use chrono::{DateTime, Datelike, Days, Local, NaiveDate, Utc};
use gpui::{AnyView, ElementId, KeyBinding, actions};

use crate::ui::menu::{anchor_popover, open_popover};

use super::*;

actions!(waku_sidebar, [CancelSessionRename]);

const SESSION_RENAME_PARENT_CONTEXT: &str = "SessionRename";
const SESSION_RENAME_FIELD_CONTEXT: &str = "SessionRename > TextInput";

/// Keep Escape inside the focused inline editor so it cancels the rename,
/// rather than falling through to the window-wide Stop action.
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        CancelSessionRename,
        Some(SESSION_RENAME_FIELD_CONTEXT),
    )]);
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SessionDateGroup {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    ThisYear,
    More,
}

impl SessionDateGroup {
    const ALL: [Self; 6] = [
        Self::Today,
        Self::Yesterday,
        Self::ThisWeek,
        Self::ThisMonth,
        Self::ThisYear,
        Self::More,
    ];

    fn index(self) -> usize {
        match self {
            Self::Today => 0,
            Self::Yesterday => 1,
            Self::ThisWeek => 2,
            Self::ThisMonth => 3,
            Self::ThisYear => 4,
            Self::More => 5,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Today => tr!("sidebar.today"),
            Self::Yesterday => tr!("sidebar.yesterday"),
            Self::ThisWeek => tr!("sidebar.this_week"),
            Self::ThisMonth => tr!("sidebar.this_month"),
            Self::ThisYear => tr!("sidebar.this_year"),
            Self::More => tr!("sidebar.more"),
        }
    }
}

/// One section of the status view, in the order [`Self::ALL`] lists them:
/// what needs the user first, what is running next, and the settled tasks
/// last.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SidebarStatusSection {
    NeedsYou,
    Running,
    Recent,
}

impl SidebarStatusSection {
    pub(super) const ALL: [Self; 3] = [Self::NeedsYou, Self::Running, Self::Recent];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|section| *section == self)
            .expect("every section is listed in ALL")
    }

    /// The stable identity the section's collapsed state is kept under.
    fn group(self) -> SidebarGroup {
        match self {
            Self::NeedsYou => SidebarGroup::NeedsYou,
            Self::Running => SidebarGroup::Running,
            Self::Recent => SidebarGroup::Recent,
        }
    }
}

/// Stable identity for a collapsible sidebar section. Keeping every view's
/// sections in one set preserves disclosure state when the user switches
/// between Project, Updated and Status grouping; a status section shares its
/// identity with no other section, so folding one takes nothing else with it.
///
/// `Archived` is the one section more than one view draws — a put-away task
/// belongs to no project and no status, so it is listed once, trailing
/// whatever the view put above it, and folding it away is the same choice
/// wherever the user made it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum SidebarGroup {
    Updated(SessionDateGroup),
    Project(Uuid),
    Projectless,
    NeedsYou,
    Running,
    Recent,
    Archived,
}

impl SidebarGroup {
    fn element_key(self) -> SharedString {
        match self {
            Self::Updated(group) => format!("updated-{}", group.index()).into(),
            Self::Project(project_id) => format!("project-{project_id}").into(),
            Self::Projectless => "projectless".into(),
            Self::NeedsYou => "status-needs-you".into(),
            Self::Running => "status-running".into(),
            Self::Recent => "status-recent".into(),
            Self::Archived => "archived".into(),
        }
    }

    fn mix_fingerprint(self, fingerprint: u64) -> u64 {
        match self {
            Self::Updated(group) => mix(fingerprint, group.index() as u64 + 1),
            Self::Project(project_id) => mix_uuid(mix(fingerprint, 0x100), project_id),
            Self::Projectless => mix(fingerprint, 0x200),
            Self::NeedsYou => mix(fingerprint, 0x300),
            Self::Running => mix(fingerprint, 0x301),
            Self::Recent => mix(fingerprint, 0x302),
            Self::Archived => mix(fingerprint, 0x303),
        }
    }
}

fn sidebar_grouping_label(grouping: SidebarGrouping) -> String {
    match grouping {
        SidebarGrouping::Project => tr!("sidebar.grouping_project"),
        SidebarGrouping::Updated => tr!("sidebar.grouping_updated"),
        SidebarGrouping::Status => tr!("sidebar.grouping_status"),
    }
}

/// The glyph the view switch shows for each grouping. It is paired with the
/// view's own label, so the three views stay tellable apart without opening the
/// menu: a folder is the projects, a clock is the recency headings, a list is
/// the sections a task's state puts it in.
fn sidebar_grouping_glyph(grouping: SidebarGrouping) -> &'static str {
    match grouping {
        SidebarGrouping::Project => "icons/folder.svg",
        SidebarGrouping::Updated => "icons/clock.svg",
        SidebarGrouping::Status => "icons/list.svg",
    }
}

fn sidebar_ordering_label(ordering: SidebarOrdering) -> String {
    match ordering {
        SidebarOrdering::Newest => tr!("sidebar.ordering_newest"),
        SidebarOrdering::Oldest => tr!("sidebar.ordering_oldest"),
    }
}

fn session_date_group(timestamp: u64, today: NaiveDate) -> SessionDateGroup {
    let session_date = i64::try_from(timestamp)
        .ok()
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map(|timestamp| timestamp.with_timezone(&Local).date_naive())
        .unwrap_or(today);
    session_date_group_for_dates(session_date, today)
}

fn session_date_group_for_dates(session_date: NaiveDate, today: NaiveDate) -> SessionDateGroup {
    if session_date >= today {
        return SessionDateGroup::Today;
    }

    if today.pred_opt() == Some(session_date) {
        return SessionDateGroup::Yesterday;
    }

    let week_start = today
        .checked_sub_days(Days::new(today.weekday().num_days_from_monday().into()))
        .unwrap_or(today);
    if session_date >= week_start {
        return SessionDateGroup::ThisWeek;
    }

    if session_date.year() == today.year() && session_date.month() == today.month() {
        return SessionDateGroup::ThisMonth;
    }

    if session_date.year() == today.year() {
        return SessionDateGroup::ThisYear;
    }

    SessionDateGroup::More
}

fn session_group_header(theme: &Theme) -> Div {
    div()
        .h(px(SIDEBAR_GROUP_HEADER_HEIGHT))
        .px(px(8.0))
        .flex()
        .items_center()
        .text_size(sp(13.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_secondary)
}

fn append_sidebar_group_rows(
    rows: &mut Vec<SidebarRow>,
    group: SidebarGroup,
    sessions: &[Uuid],
    collapsed: bool,
    show_more: bool,
) {
    if sessions.is_empty() && !show_more {
        return;
    }

    rows.push(SidebarRow::Header(group));
    if !collapsed {
        rows.extend(sessions.iter().copied().map(SidebarRow::Session));
        if show_more {
            rows.push(SidebarRow::ShowMore(group));
        }
    }
    rows.push(SidebarRow::GroupSpacer);
}

/// Append the trailing archived section.
///
/// Unlike every other section, it stands even with nothing in it: it is
/// rendered because the user asked for it — or because the open task is in it
/// — and an empty heading is the honest answer to that request. `show_more`
/// never applies here; archived tasks are listed in full or not at all.
fn append_sidebar_archived_section(rows: &mut Vec<SidebarRow>, sessions: &[Uuid], collapsed: bool) {
    rows.push(SidebarRow::Header(SidebarGroup::Archived));
    if !collapsed {
        rows.extend(sessions.iter().copied().map(SidebarRow::Session));
    }
    rows.push(SidebarRow::GroupSpacer);
}

fn updater_button_available_content(
    foreground: Hsla,
    label: SharedString,
    label_reveal: f32,
) -> Div {
    div()
        .relative()
        .size_full()
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .opacity(1.0 - label_reveal)
                .child(icon("icons/download.svg", 12.0, foreground)),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .whitespace_nowrap()
                .opacity(label_reveal)
                .child(label),
        )
}

/// Height of a session card plus the separation reserved beneath it in the
/// virtualized sidebar list. Keep the gap inside the list row so measured and
/// estimated heights stay identical for off-screen sessions.
const SIDEBAR_SESSION_CARD_HEIGHT: f32 = 51.0;
const SIDEBAR_SESSION_ROW_GAP: f32 = 1.0;
const SIDEBAR_SESSION_ROW_HEIGHT: f32 = SIDEBAR_SESSION_CARD_HEIGHT + SIDEBAR_SESSION_ROW_GAP;
const SIDEBAR_ACTION_ROW_HEIGHT: f32 = 32.0;
const SIDEBAR_SEARCH_BOTTOM_GAP: f32 = 10.0;
const SIDEBAR_GROUP_HEADER_HEIGHT: f32 = 28.0;
const SIDEBAR_GROUP_HEADER_BOTTOM_GAP: f32 = 2.0;
const SIDEBAR_SHOW_MORE_ROW_HEIGHT: f32 = 30.0;
const SIDEBAR_GROUP_SPACER_HEIGHT: f32 = 10.0;
const SIDEBAR_GROUP_CHILD_PADDING: f32 = 28.0;
const SIDEBAR_PROJECT_RECENT_WINDOW_SECONDS: u64 = 3 * 24 * 60 * 60;
const SIDEBAR_PROJECT_REVEAL_BATCH: usize = 30;

/// The session row's trailing time: how long the live turn has been working,
/// or how long ago the agent last replied. A session that has never replied
/// shows nothing.
pub(super) fn session_time_label(session: &AgentSession, now: u64) -> Option<String> {
    if session.status == SessionStatus::Background {
        return Some(tr!("sidebar.status_background"));
    }
    if session.is_busy()
        && let Some(turn) = session
            .turns
            .last()
            .filter(|turn| turn.status == TurnStatus::Running)
    {
        return Some(tr!(
            "sidebar.working",
            elapsed = format_working_elapsed(now.saturating_sub(turn.started_at))
        ));
    }
    session
        .last_reply_at
        .map(|last_reply_at| format_time_ago(now.saturating_sub(last_reply_at)))
}

/// Recency for sidebar ordering and date groups. A submitted turn promotes the
/// task immediately, while metadata edits such as a rename do not; a task with
/// no turns stays anchored to when it was created.
fn sidebar_session_timestamp(session: &AgentSession) -> u64 {
    session.last_reply_at.unwrap_or(session.created_at)
}

fn sort_sidebar_sessions(sessions: &mut Vec<&AgentSession>, ordering: SidebarOrdering) {
    match ordering {
        SidebarOrdering::Newest => {
            sessions.sort_by_key(|session| std::cmp::Reverse(sidebar_session_timestamp(session)))
        }
        SidebarOrdering::Oldest => {
            sessions.sort_by_key(|session| sidebar_session_timestamp(session))
        }
    }
}

/// Folds one started task's row facts into the snapshot fingerprint: exactly
/// the values [`Waku::sidebar_rows`] decides a row's place by.
///
/// Status and archive state belong here even though only the status view
/// sections by them, because a flip has to re-section the rows instead of
/// leaving the old snapshot in place. The title and `updated_at` are
/// deliberately absent: a metadata edit must not move a row.
fn mix_sidebar_session_facts(fingerprint: u64, session: &AgentSession) -> u64 {
    let status = match session.status {
        SessionStatus::Idle => 1,
        SessionStatus::Connecting => 2,
        SessionStatus::Working => 3,
        SessionStatus::Waiting => 4,
        SessionStatus::Background => 5,
        SessionStatus::Failed => 6,
    };
    let fingerprint = mix_uuid(fingerprint, session.id);
    let fingerprint = mix_uuid(fingerprint, session.project_id);
    let fingerprint = mix(fingerprint, sidebar_session_timestamp(session));
    let fingerprint = mix(fingerprint, status);
    mix(fingerprint, u64::from(session.archived_at.is_some()))
}

/// The active status section a task belongs to, or none while the task is
/// archived: an archived task belongs to the trailing archived section and
/// must not be mixed into the three active ones, whatever its status.
fn status_section_of(session: &AgentSession) -> Option<SidebarStatusSection> {
    if session.archived_at.is_some() {
        return None;
    }
    Some(match session.status {
        // A provider question and a failure both leave the task on the user.
        SessionStatus::Waiting | SessionStatus::Failed => SidebarStatusSection::NeedsYou,
        // The turn is live: the provider is connecting, thinking, or parked on
        // detached work it will wake the turn for.
        SessionStatus::Connecting | SessionStatus::Working | SessionStatus::Background => {
            SidebarStatusSection::Running
        }
        SessionStatus::Idle => SidebarStatusSection::Recent,
    })
}

/// The stamp "needs you" orders by: when the task entered `Waiting` or
/// `Failed`, or, for a task that was blocked before that field existed, its
/// newest turn activity — the closest thing it has to a blockage clock.
fn blocked_since_or_turn_activity(session: &AgentSession) -> u64 {
    session
        .blocked_since
        .unwrap_or_else(|| sidebar_session_timestamp(session))
}

/// The status view's sections, each already in render order and holding the
/// ids to list under it. A task appears in exactly one section.
///
/// "Needs you" leads with the longest blockage, and the other two read the
/// same conversation recency the project view sorts by, newest first. The task
/// id breaks every tie, so two tasks that share a stamp keep one order across
/// launches rather than following whatever order the list was collected in.
fn status_sidebar_sections(sessions: &[&AgentSession]) -> [Vec<Uuid>; 3] {
    let mut sections: [Vec<&AgentSession>; 3] = std::array::from_fn(|_| Vec::new());
    for session in sessions.iter().copied() {
        if let Some(section) = status_section_of(session) {
            sections[section.index()].push(session);
        }
    }
    for section in SidebarStatusSection::ALL {
        let sessions = &mut sections[section.index()];
        match section {
            SidebarStatusSection::NeedsYou => sessions
                .sort_by_key(|session| (blocked_since_or_turn_activity(session), session.id)),
            SidebarStatusSection::Running | SidebarStatusSection::Recent => {
                sessions.sort_by_key(|session| {
                    (
                        std::cmp::Reverse(sidebar_session_timestamp(session)),
                        session.id,
                    )
                })
            }
        }
    }
    sections.map(|section| section.into_iter().map(|session| session.id).collect())
}

/// Folds the archive-visibility inputs the row snapshot's shape depends on:
/// the persisted reveal toggle, and whether the open task is itself archived.
///
/// The second one is not redundant. With the toggle off an archived task is
/// hidden from every section, but the task the window is showing may not be a
/// gap in the list that is showing it, so that one row stands in the archived
/// section whatever the toggle says — and moving the selection on to a task
/// that is not archived has to take the section down again.
fn mix_sidebar_archived_visibility(
    fingerprint: u64,
    show_archived: bool,
    active_task_archived: bool,
) -> u64 {
    mix(
        mix(fingerprint, u64::from(show_archived)),
        u64::from(active_task_archived),
    )
}

/// Lift the started tasks that were put away out of the sections a view would
/// otherwise file them in, keeping the order that view sorted them into, and
/// say whether their trailing section stands.
///
/// An archived task is removed here whatever its status — waiting, running or
/// idle — so a view's sections can never disagree with the trailing one about
/// where a task belongs. The trailing section stands when the user asked to
/// see what was put away, and also when the task the window is showing is one
/// of them, which is what keeps the open task from becoming a gap in the list
/// that is showing it.
fn split_archived_sessions<'a>(
    sessions: Vec<&'a AgentSession>,
    show_archived: bool,
    active_task_archived: bool,
) -> (Vec<&'a AgentSession>, Vec<Uuid>, bool) {
    let mut listed = Vec::with_capacity(sessions.len());
    let mut archived = Vec::new();
    for session in sessions {
        if session.archived_at.is_some() {
            archived.push(session.id);
        } else {
            listed.push(session);
        }
    }
    (listed, archived, show_archived || active_task_archived)
}

/// Apply one archive action to a client-held task the way the daemon applies
/// it: an archive stamps the time it happened, a restore clears it, and
/// nothing else about the task is touched.
///
/// The action is the only thing that changes this state — no save can set or
/// clear it — so the client mirrors the daemon's own move onto the row's copy,
/// which is what lets the row leave the list on the frame the user asked
/// instead of on the next catalog refresh. Returns whether the task moved
/// between the two, which is what decides if the row snapshot has to be
/// rebuilt.
pub(super) fn apply_archive_action(session: &mut AgentSession, archived: bool, now: u64) -> bool {
    let moved = session.archived_at.is_some() != archived;
    session.archived_at = archived.then_some(now);
    moved
}

fn project_sidebar_groups(
    sessions: &[&AgentSession],
    projectless_project_ids: &HashSet<Uuid>,
) -> Vec<(SidebarGroup, Vec<Uuid>)> {
    let mut groups: Vec<(SidebarGroup, Vec<Uuid>)> = Vec::new();
    let mut indexes = HashMap::new();
    let mut projectless_sessions = Vec::new();
    for session in sessions {
        if projectless_project_ids.contains(&session.project_id) {
            projectless_sessions.push(session.id);
            continue;
        }
        let index = *indexes.entry(session.project_id).or_insert_with(|| {
            let index = groups.len();
            groups.push((SidebarGroup::Project(session.project_id), Vec::new()));
            index
        });
        groups[index].1.push(session.id);
    }
    if !projectless_sessions.is_empty() {
        groups.push((SidebarGroup::Projectless, projectless_sessions));
    }
    groups
}

fn visible_project_sessions(
    sessions: &[Uuid],
    session_timestamps: &HashMap<Uuid, u64>,
    recent_cutoff: u64,
    revealed_older_sessions: usize,
) -> (Vec<Uuid>, bool) {
    let mut visible = Vec::with_capacity(sessions.len());
    let mut older_seen = 0usize;
    for session_id in sessions {
        let recent = session_timestamps
            .get(session_id)
            .is_some_and(|timestamp| *timestamp >= recent_cutoff);
        if recent || older_seen < revealed_older_sessions {
            visible.push(*session_id);
        }
        if !recent {
            older_seen = older_seen.saturating_add(1);
        }
    }
    (visible, older_seen > revealed_older_sessions)
}

fn sidebar_project_is_projectless(project: &Project, projectless_root: Option<&Path>) -> bool {
    projectless_root.is_some_and(|root| project.path.starts_with(root))
}

fn persisted_sidebar_branch_label(workspace: &SessionWorkspace) -> Option<&str> {
    match workspace {
        SessionWorkspace::Local => None,
        SessionWorkspace::NewWorktree { base_branch } => base_branch.as_deref(),
        SessionWorkspace::Worktree { branch, .. } => Some(branch.as_str()),
    }
    .filter(|branch| !branch.is_empty())
}

/// The identifier a row shows when its second line is not reporting the
/// task's present state: the task's own worktree branch when the client knows
/// it, and the project it belongs to otherwise.
///
/// The project's *current* branch is never substituted for the task's own.
/// The list projection does not carry a task's workspace, so a client that
/// just started cannot tell a task in the checkout from one in a worktree;
/// naming the project is the honest answer instead of a branch the task may
/// not be on.
fn sidebar_row_identifier(
    grouped_by_project: bool,
    session: &AgentSession,
    project: Option<&Project>,
) -> (&'static str, SharedString) {
    let branch = if grouped_by_project {
        persisted_sidebar_branch_label(&session.workspace)
    } else {
        None
    };
    match branch {
        Some(branch) => (
            "icons/git-branch.svg",
            SharedString::from(branch.to_owned()),
        ),
        None => ("icons/folder.svg", sidebar_project_label(project)),
    }
}

/// A task's project as a single-line label: a projectless task shows the
/// no-project name, and a task whose project is missing from the catalog shows
/// the unknown-project name.
fn sidebar_project_label(project: Option<&Project>) -> SharedString {
    SharedString::from(
        project
            .map(Project::display_name)
            .unwrap_or_else(|| tr!("sidebar.unknown_project")),
    )
}

/// What a task row's second line says about where the task stands now, decided
/// in the order the sidebar spec fixes, or `None` for a task to describe with
/// the content its row already has.
///
/// A blocked task leads with the reason it recorded. A busy task then shows the
/// provider's plan step for the live turn, falling back to the objective it is
/// working toward. An idle task borrows neither: an objective there would pass
/// off what the task was doing as what it is doing.
pub(super) fn sidebar_row_detail(
    session: &AgentSession,
    facts: &SidebarSessionFacts,
) -> Option<SidebarRowDetail> {
    let blocked = matches!(
        session.status,
        SessionStatus::Waiting | SessionStatus::Failed
    );
    if blocked && let Some(reason) = facts.blocked_reason.clone() {
        return Some(SidebarRowDetail::BlockedReason(reason));
    }
    if !session.is_busy() {
        return None;
    }
    facts
        .step
        .clone()
        .map(SidebarRowDetail::PlanStep)
        .or_else(|| facts.objective.clone().map(SidebarRowDetail::Objective))
}

/// What a row reports about a task's present state. A task with none of these
/// keeps the content it already has, so a row never falls back to a placeholder
/// such as "working" or "unknown".
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SidebarRowDetail {
    /// Why a blocked or failed task is blocked.
    BlockedReason(SharedString),
    /// The provider's plan step for the turn that is running now.
    PlanStep(SharedString),
    /// What a busy task is working toward.
    Objective(SharedString),
}

impl SidebarRowDetail {
    fn text(&self) -> &SharedString {
        match self {
            Self::BlockedReason(text) | Self::PlanStep(text) | Self::Objective(text) => text,
        }
    }

    /// Paired with the text, so a reader never has to tell the three apart by
    /// color alone.
    fn icon(&self) -> &'static str {
        match self {
            Self::BlockedReason(_) => "icons/alert.svg",
            Self::PlanStep(_) => activity_icon(ActivityKind::Plan),
            Self::Objective(_) => "icons/target.svg",
        }
    }
}

/// The values a task row reads about one session.
///
/// The list entry already carries the objective, the blockage reason and the
/// counters. The plan step it cannot: that lives in the transcript, so it is
/// resolved here — once, where the session changed — and cached, because a row
/// builder runs for every visible row on every frame and must never walk a
/// transcript (`AGENTS.md`). `turn_count` and `changed_files` are kept beside
/// it for the row card.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct SidebarSessionFacts {
    pub(super) objective: Option<SharedString>,
    pub(super) blocked_reason: Option<SharedString>,
    pub(super) step: Option<SharedString>,
    pub(super) turn_count: Option<u32>,
    pub(super) changed_files: Option<u32>,
}

/// The facts one session gives a row, as of now.
pub(super) fn sidebar_session_facts(session: &AgentSession) -> SidebarSessionFacts {
    SidebarSessionFacts {
        objective: session.objective.as_deref().map(SharedString::from),
        blocked_reason: session.blocked_reason.as_deref().map(SharedString::from),
        step: live_plan_step(session),
        turn_count: session.turn_count,
        changed_files: session.changed_files,
    }
}

/// Rebuild the facts cache from the whole catalog in one pass, so a refresh of
/// the session list (startup, a restart, another client's change) also drops
/// the tasks that are gone.
pub(super) fn rebuild_sidebar_session_facts(
    facts: &mut HashMap<Uuid, SidebarSessionFacts>,
    sessions: &[AgentSession],
) {
    facts.clear();
    facts.extend(
        sessions
            .iter()
            .map(|session| (session.id, sidebar_session_facts(session))),
    );
}

/// The provider's plan step for the turn that is running now: the newest plan
/// activity the live turn's transcript holds, named the way the transcript
/// names it.
///
/// A settled turn's plan activity describes work that already happened, so it
/// is deliberately not a current step: an idle row falls back to the content it
/// has today rather than resurrecting the last plan of a finished turn.
fn live_plan_step(session: &AgentSession) -> Option<SharedString> {
    let turn_id = session.active_turn_id()?;
    session
        .transcript_blocks
        .iter()
        .rev()
        .filter(|block| block.turn_id == Some(turn_id))
        .flat_map(|block| block.activities.iter().rev())
        .find(|activity| activity.kind == ActivityKind::Plan)
        .map(|activity| SharedString::from(activity_display_title(activity)))
        .filter(|step| !step.trim().is_empty())
}

/// The card a task row reveals on hover and on keyboard focus: what the task
/// is and where it stands, for a reader who does not want to switch to it.
///
/// Every field is a value the client already holds — the task's list entry, its
/// row facts, its project, and the branch the client knows it is on — and the
/// field types are the guarantee that revealing the card cannot fetch anything:
/// there is no daemon client, store, or path handle in here to fetch one with,
/// so a hover can never issue a request, retry, or spawn work.
#[derive(Clone)]
struct SidebarTaskCard {
    /// The task this card is about. It names the card's animation ids: gpui
    /// tracks an animation by element id, so a fixed id would hand the next
    /// card the previous one's finished animation instead of starting its own.
    session_id: Uuid,
    title: SharedString,
    /// What the task is working toward, when a summary has been generated.
    objective: Option<SharedString>,
    /// Why a blocked task is blocked, or the provider's step for the live turn.
    state: Option<SidebarRowDetail>,
    project: SharedString,
    /// The task's own worktree branch, when the client knows it.
    branch: Option<SharedString>,
    turns: Option<u32>,
    changed_files: Option<u32>,
    /// How long ago the task last moved, in the same words as the row's
    /// trailing label.
    recency: SharedString,
}

/// The card for one task, resolved from values the client already holds.
///
/// A row reports a busy task's objective only when the provider has no step for
/// it; the card is what the task *is*, so it gives the objective its own line
/// and lets the reason or the step say where the task stands.
fn sidebar_task_card(
    session: &AgentSession,
    facts: &SidebarSessionFacts,
    project: Option<&Project>,
    now: u64,
) -> SidebarTaskCard {
    SidebarTaskCard {
        session_id: session.id,
        title: SharedString::from(localized_session_title(session)),
        objective: facts.objective.clone(),
        state: sidebar_row_detail(session, facts)
            .filter(|detail| !matches!(detail, SidebarRowDetail::Objective(_))),
        project: sidebar_project_label(project),
        branch: persisted_sidebar_branch_label(&session.workspace).map(SharedString::from),
        turns: facts.turn_count,
        changed_files: facts.changed_files,
        recency: SharedString::from(format_time_ago(
            now.saturating_sub(sidebar_session_timestamp(session)),
        )),
    }
}

/// The card's facts line: what is known about the task's size and recency, with
/// the unknown left out rather than guessed. Recency is always known, so the
/// line itself is always there.
fn sidebar_card_facts_line(
    turns: Option<u32>,
    changed_files: Option<u32>,
    recency: &str,
) -> SharedString {
    let mut facts = Vec::with_capacity(3);
    if let Some(turns) = turns {
        facts.push(tr!(
            if turns == 1 {
                "sidebar.card_turn_one"
            } else {
                "sidebar.card_turn_many"
            },
            count = turns
        ));
    }
    if let Some(changed_files) = changed_files {
        facts.push(tr!(
            if changed_files == 1 {
                "sidebar.card_file_one"
            } else {
                "sidebar.card_file_many"
            },
            count = changed_files
        ));
    }
    facts.push(recency.to_owned());
    SharedString::from(facts.join(" · "))
}

/// The card's width, matching the app's other summary cards. The card is one
/// stack: this is the whole card, never a column inside it.
const SIDEBAR_CARD_WIDTH: f32 = 300.0;

impl SidebarTaskCard {
    /// The view both routes to the card render: GPUI's hover tooltip and the
    /// row's own anchored card.
    fn into_view(self, cx: &mut App) -> AnyView {
        cx.new(|_| self).into()
    }
}

impl Render for SidebarTaskCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        sidebar_task_card_view(self, cx)
    }
}

/// The card's one compact stack, drawn: no dividers and no columns, every line
/// one line tall. The facts are the shortest line, so they take the right edge
/// of the block that carries them rather than a row of their own.
fn sidebar_task_card_view(
    card: &SidebarTaskCard,
    cx: &mut Context<SidebarTaskCard>,
) -> impl IntoElement {
    let theme = Theme::current(cx);
    let reduce_motion = cx.reduce_motion();
    let facts = sidebar_card_facts_line(card.turns, card.changed_files, &card.recency);
    let facts_element = |color: Hsla, facts: SharedString| {
        div()
            .flex_none()
            .pl(px(8.0))
            .text_size(sp(11.5))
            .text_color(color)
            .child(facts)
            .into_any_element()
    };
    // The objective is where a reader looks first for "what is this", so the
    // facts ride its line; a card with no objective keeps them on the title
    // instead of adding a row.
    let objective = card.objective.clone();
    let facts_ride_the_title = objective.is_none();

    div()
        .id("sidebar-task-card")
        .w(px(SIDEBAR_CARD_WIDTH))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(10.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.raised)
        .shadow_md()
        .text_size(sp(12.5))
        .line_height(sp(16.0))
        .child(
            sidebar_card_line(
                ElementId::Name(SharedString::from(format!(
                    "card-{}-title",
                    card.session_id
                ))),
                None,
                card.title.clone(),
                theme.text,
                theme.raised,
                facts_ride_the_title.then(|| facts_element(theme.text_ghost, facts.clone())),
                reduce_motion,
            )
            .text_size(sp(13.5))
            .font_weight(FontWeight::MEDIUM),
        )
        .when_some(objective, |element, objective| {
            element.child(sidebar_card_line(
                ElementId::Name(SharedString::from(format!(
                    "card-{}-objective",
                    card.session_id
                ))),
                Some("icons/target.svg"),
                objective,
                theme.text_secondary,
                theme.raised,
                (!facts_ride_the_title).then(|| facts_element(theme.text_ghost, facts.clone())),
                reduce_motion,
            ))
        })
        .when_some(card.state.as_ref(), |element, state| {
            element.child(sidebar_card_line(
                ElementId::Name(SharedString::from(format!(
                    "card-{}-state",
                    card.session_id
                ))),
                Some(state.icon()),
                state.text().clone(),
                theme.text_secondary,
                theme.raised,
                None,
                reduce_motion,
            ))
        })
        .child(sidebar_card_line(
            ElementId::Name(SharedString::from(format!(
                "card-{}-project",
                card.session_id
            ))),
            Some("icons/folder.svg"),
            card.project.clone(),
            theme.text_tertiary,
            theme.raised,
            None,
            reduce_motion,
        ))
        .when_some(card.branch.clone(), |element, branch| {
            element.child(sidebar_card_line(
                ElementId::Name(SharedString::from(format!(
                    "card-{}-branch",
                    card.session_id
                ))),
                Some("icons/git-branch.svg"),
                branch,
                theme.text_tertiary,
                theme.raised,
                None,
                reduce_motion,
            ))
        })
}

/// How wide the fade at a clipped line's end is. The composer masks text that
/// runs under its controls with the same band, so a clipped line here fades into
/// the card exactly the way text there fades into the composer.
const SIDEBAR_CARD_FADE_WIDTH: f32 = 22.0;

/// How long a line takes to glide to its end once the card is revealed.
const SIDEBAR_CARD_MARQUEE: Duration = Duration::from_millis(1_400);

/// One line of the card: an optional icon, then the text, then an optional
/// trailing element that takes the space the text leaves.
///
/// A line is one line tall, so a long title cannot grow the card. Instead the
/// text glides to its end once, and the last characters fade into the card's
/// surface rather than stopping dead at the edge. The glide is one animation on
/// `ease_out_quint`, which is what makes it arrive at the end instead of
/// hitting it - and it is skipped entirely when the system asks for reduced
/// motion, which leaves the fade as the whole treatment.
#[allow(clippy::too_many_arguments)]
fn sidebar_card_line(
    id: ElementId,
    icon_path: Option<&'static str>,
    text: SharedString,
    color: Hsla,
    surface: Hsla,
    trailing: Option<AnyElement>,
    reduce_motion: bool,
) -> Div {
    let scroll = ScrollHandle::new();
    let animated_scroll = scroll.clone();
    let text = div().whitespace_nowrap().child(text);
    let text = if reduce_motion {
        text.into_any_element()
    } else {
        text.with_animation(
            id.clone(),
            Animation::new(SIDEBAR_CARD_MARQUEE).with_easing(ease_out_quint()),
            move |element, delta| {
                let overflow = animated_scroll.max_offset().x;
                if overflow > px(0.0) {
                    animated_scroll.set_offset(point(-overflow * delta, px(0.0)));
                }
                element
            },
        )
        .into_any_element()
    };

    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_color(color)
        .when_some(icon_path, |element, path| {
            element.child(icon(path, 12.5, color))
        })
        .child(
            div()
                .relative()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .id(id)
                        .overflow_x_scroll()
                        .track_scroll(&scroll)
                        .child(text),
                )
                .child(sidebar_card_line_fade(surface)),
        )
        .when_some(trailing, |element, trailing| element.child(trailing))
}

/// The band that covers a line's end.
///
/// It is always drawn: the cover is the point, because a line that stops at the
/// edge reads as cut off, while one that fades into the card reads as
/// continuing. The composer masks text that runs under its controls with the
/// same 22-point band, so this is the app's own way of saying the same thing.
fn sidebar_card_line_fade(surface: Hsla) -> impl IntoElement {
    div()
        .absolute()
        .right_0()
        .top_0()
        .bottom_0()
        .w(px(SIDEBAR_CARD_FADE_WIDTH))
        .bg(linear_gradient(
            90.0,
            linear_color_stop(surface.opacity(0.0), 0.0),
            linear_color_stop(surface, 1.0),
        ))
}

/// Attach a row's card: GPUI's own tooltip for the pointer, and the same card
/// anchored to the row for keyboard focus through the row's second handle. The
/// row keeps the first handle for its context menu, so opening one surface
/// never opens the other.
///
/// Both routes draw the card the caller already resolved; revealing it is a
/// paint, so neither can ask the daemon for anything.
fn with_sidebar_task_card(
    row: Stateful<Div>,
    card: SidebarTaskCard,
    card_handle: &ContextMenuHandle,
) -> Div {
    let hover = card.clone();
    // While the card stands for this row's keyboard focus the tooltip stays
    // off, so the pointer never draws a second copy of it.
    //
    // The card is a *hoverable* tooltip: a line glides to its end over about a
    // second, so the pointer has to be able to come to rest on the card and read
    // it. A plain tooltip vanishes the moment the pointer leaves the row.
    let row = row.when(!card_handle.is_open(), |row| {
        row.hoverable_tooltip(move |_window, cx| hover.clone().into_view(cx))
    });
    anchor_popover(row, card_handle, MenuAlign::BelowLeft, move |_, _, cx| {
        card.clone().into_view(cx).into_any_element()
    })
}

/// The focus identity of a task row, as a tab stop.
///
/// gpui reads tab membership off the *handle*, and a handle from
/// `focus_handle()` starts outside the tab order: the element's own
/// `tab_index`/`tab_stop` only seed a handle gpui creates for it. Without this
/// a row is reachable only by clicking it, which is the one route its card
/// deliberately leaves to the tooltip.
fn sidebar_row_focus(menu: &ContextMenuHandle) -> FocusHandle {
    let focus = menu.trigger_focus_handle();
    if focus.tab_stop {
        return focus.clone();
    }
    focus.clone().tab_stop(true).tab_index(0)
}

/// The handle behind a row's card, with the focus wiring that reveals it,
/// created the first frame the row draws one and kept as that row's own element
/// state for as long as the row keeps drawing.
///
/// A row already owns one handle for its context menu, and one handle drives
/// one surface, so the card gets a second one; opening either surface takes
/// focus, which blurs the row, which dismisses the card, so the two never stand
/// at the same time.
///
/// The state is the row's, not the menu registry's: the registry is cleared for
/// every task switch, and a focus listener registered per handle would
/// otherwise accumulate a stale copy per switch, each kept alive by the
/// listener that captured it. Element state is dropped with the row, which is
/// exactly when its card should stop existing.
fn sidebar_task_card_handle(
    session_id: Uuid,
    row_focus: &FocusHandle,
    window: &mut Window,
    cx: &mut Context<Waku>,
) -> ContextMenuHandle {
    let key = SharedString::from(format!("session-card-{session_id}"));
    let row_focus = row_focus.clone();
    window
        .use_keyed_state(key, cx, move |window, cx| {
            SidebarTaskCardState::new(row_focus, window, cx)
        })
        .read(cx)
        .handle
        .clone()
}

/// One row's card: the second handle, and the row focus it follows.
///
/// Held as the row's element state, so it is created with the row and dropped
/// when the row stops being drawn. Its focus listeners belong to this entity,
/// which means a dropped row's listeners go with it rather than staying
/// subscribed to a focus handle nothing draws any more.
struct SidebarTaskCardState {
    handle: ContextMenuHandle,
}

impl SidebarTaskCardState {
    fn new(row_focus: FocusHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // No toggle observer: a card that follows focus updates nothing else,
        // so opening and closing it may run inside the focus listener itself.
        let handle = ContextMenuHandle::new(cx);
        let opened = handle.clone();
        cx.on_focus(&row_focus, window, move |_this, window, cx| {
            // A click focuses a row too — GPUI transfers focus on mouse down —
            // but the pointer has its own route to this card, the row's
            // tooltip, and a card pinned over the list by every click would
            // stand in the way of the next task. Only keyboard focus opens
            // this one.
            if window.last_input_was_keyboard() && !opened.is_open() {
                open_popover(&opened, MenuAlign::BelowLeft, window, cx);
            }
        })
        .detach();
        let dismissed = handle.clone();
        cx.on_blur(&row_focus, window, move |_this, window, cx| {
            dismissed.close(window, cx);
            window.refresh();
        })
        .detach();
        Self { handle }
    }
}

/// Compact "how long ago" for the sidebar: "just now", then one coarse unit —
/// "5m", "3h", "420d". Days are the largest unit so a glance still reads as a
/// count rather than a date.
pub(super) fn format_time_ago(seconds: u64) -> String {
    match seconds {
        0..=59 => tr!("sidebar.just_now"),
        60..=3_599 => tr!("sidebar.minutes_ago", count = seconds / 60),
        3_600..=86_399 => tr!("sidebar.hours_ago", count = seconds / 3_600),
        _ => tr!("sidebar.days_ago", count = seconds / 86_400),
    }
}

/// One row of the virtualized sidebar session history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarRow {
    /// Opens the window-wide command palette and scrolls with history.
    Search,
    /// Group header; the first row also carries the sidebar actions.
    Header(SidebarGroup),
    /// A started session.
    Session(Uuid),
    /// Reveals the next batch of older sessions in a project section.
    ShowMore(SidebarGroup),
    /// Spacing between date groups.
    GroupSpacer,
}

fn sidebar_session_row_index(rows: &[SidebarRow], session_id: Uuid) -> Option<usize> {
    rows.iter()
        .position(|row| *row == SidebarRow::Session(session_id))
}

fn sidebar_row_height(row: SidebarRow) -> Pixels {
    px(match row {
        SidebarRow::Search => SIDEBAR_ACTION_ROW_HEIGHT + SIDEBAR_SEARCH_BOTTOM_GAP,
        SidebarRow::Header(_) => SIDEBAR_GROUP_HEADER_HEIGHT + SIDEBAR_GROUP_HEADER_BOTTOM_GAP,
        SidebarRow::Session(_) => SIDEBAR_SESSION_ROW_HEIGHT,
        SidebarRow::ShowMore(_) => SIDEBAR_SHOW_MORE_ROW_HEIGHT,
        SidebarRow::GroupSpacer => SIDEBAR_GROUP_SPACER_HEIGHT,
    })
}

fn sidebar_bottom_aligned_offset(
    rows: &[SidebarRow],
    target: usize,
    viewport_height: Pixels,
) -> ListOffset {
    let mut item_ix = target;
    let mut height = sidebar_row_height(rows[target]);
    while item_ix > 0 && height < viewport_height {
        item_ix -= 1;
        height += sidebar_row_height(rows[item_ix]);
    }
    ListOffset {
        item_ix,
        offset_in_item: (height - viewport_height).max(Pixels::ZERO),
    }
}

fn reveal_sidebar_list_row(list: &ListState, rows: &[SidebarRow], index: usize) {
    let viewport = list.viewport_bounds();
    if viewport.size.height <= Pixels::ZERO {
        return;
    }
    if let Some(item) = list.bounds_for_item(index) {
        if item.top() >= viewport.top() && item.bottom() <= viewport.bottom() {
            return;
        }
        list.scroll_to_reveal_item(index);
    } else if index <= list.logical_scroll_top().item_ix {
        list.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: Pixels::ZERO,
        });
    } else {
        // Off-screen rows have not necessarily been measured yet. Their
        // sidebar heights are fixed, so align a lower target to the viewport
        // bottom just like scrollIntoView({ block: "nearest" }).
        list.scroll_to(sidebar_bottom_aligned_offset(
            rows,
            index,
            viewport.size.height,
        ));
    }
}

impl Waku {
    pub(super) fn window_drag_region(
        &self,
        region: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        // Windows drags from the hit test, not from a mouse-move handler:
        // `DefWindowProc` moves the window once the region reports itself as
        // caption, and performs the user's configured double-click action.
        #[cfg(target_os = "windows")]
        let region = region.window_control_area(gpui::WindowControlArea::Drag);

        region
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    crate::platform::titlebar_double_click(window);
                }
            })
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.header_drag_armed = false;
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = false;
                }),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.header_drag_armed {
                    this.header_drag_armed = false;
                    crate::platform::start_window_move(window);
                }
            }))
    }
    // ── Sidebar ────────────────────────────────────────────────────────────

    fn render_fps_counter(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let fps = self.fps_value;
        let dot = if fps == 0 {
            theme.text_ghost
        } else if fps >= 55 {
            theme.success
        } else if fps >= 30 {
            theme.warning
        } else {
            theme.danger
        };
        div()
            .flex_none()
            .h(px(26.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(sp(12.5))
            .line_height(sp(0.0))
            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(dot))
            .child(
                div()
                    .text_color(theme.text_tertiary)
                    .font_family(crate::md::render::MONO_FAMILY)
                    .child(SharedString::from(format!("{fps} FPS"))),
            )
    }

    fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id("toggle-sidebar")
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon("icons/panel-left.svg", 14.0, theme.text_tertiary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_sidebar_visible(!this.sidebar_visible, cx);
            }))
    }

    pub(super) fn render_history_button(
        &self,
        id: &'static str,
        icon_path: &'static str,
        enabled: bool,
        navigate_back: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id(id)
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .when(!enabled, |element| element.opacity(0.35))
            .when(enabled, |element| {
                element
                    .hover(|element| element.bg(theme.overlay))
                    .active(|element| element.bg(theme.overlay_strong))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if navigate_back {
                            this.navigate_back_action(&NavigateBack, window, cx);
                        } else {
                            this.navigate_forward_action(&NavigateForward, window, cx);
                        }
                    }))
            })
            .child(icon(icon_path, 14.0, theme.text_tertiary))
    }

    fn render_sidebar_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("sidebar-titlebar")
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .children(self.render_client_window_controls(
                super::window_chrome::WindowControlSide::Left,
                window,
                cx,
            ))
            .child(
                self.window_drag_region(
                    div()
                        .id("sidebar-traffic-light-drag-region")
                        .w(px(TRAFFIC_LIGHT_CLEARANCE))
                        .h_full()
                        .flex_none(),
                    cx,
                ),
            )
            .child(self.render_sidebar_toggle(cx))
            .child(
                div()
                    .ml(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(self.render_history_button(
                        "navigate-back",
                        "icons/arrow-left.svg",
                        !self.session_navigation.back.is_empty(),
                        true,
                        cx,
                    ))
                    .child(self.render_history_button(
                        "navigate-forward",
                        "icons/arrow-right.svg",
                        !self.session_navigation.forward.is_empty(),
                        false,
                        cx,
                    )),
            )
            .child(self.window_drag_region(
                div().id("sidebar-titlebar-drag-region").h_full().flex_1(),
                cx,
            ))
            .child(self.render_sidebar_view_switch(cx))
    }

    /// The visible way to change what the sidebar groups by.
    ///
    /// The options menu still carries the same choice, but that is two clicks
    /// deep and tells a reader nothing about the view they are in. This button
    /// shows the current view's glyph and opens the three of them by name, so
    /// the project and updated views stay one click away now that the status
    /// view exists beside them.
    ///
    /// It is deliberately glyph-only: the titlebar already spends 86 points on
    /// the traffic lights, 26 on the panel toggle and 52 on the history arrows,
    /// which at the default 252-point sidebar leaves about 70 for everything
    /// else. A labeled chip needed 75 of them and was clipped off the edge - the
    /// bug this replaces. The view's name lives in the tooltip and in the menu,
    /// where there is room for it. `dropdown_menu` owns the trigger's focus and
    /// key handling, so Tab reaches this and Enter or Space opens it.
    fn render_sidebar_view_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let grouping = self.state.sidebar_grouping;
        let menu = self.menu_handle("sidebar-view-switch", cx);
        let open = menu.is_open();
        let weak = cx.entity().downgrade();
        let trigger = div()
            .id("sidebar-view-switch")
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .when(open, |element| element.bg(theme.overlay_strong))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!(
                "sidebar.view_switch",
                view = sidebar_grouping_label(grouping)
            )))
            .child(icon(
                sidebar_grouping_glyph(grouping),
                14.0,
                if open {
                    theme.text
                } else {
                    theme.text_secondary
                },
            ));

        div()
            .pr(px(10.0))
            .flex_none()
            .child(dropdown_menu(
                trigger,
                "sidebar-view-switch-menu",
                &menu,
                MenuAlign::BelowRight,
                move |_| {
                    [
                        SidebarGrouping::Project,
                        SidebarGrouping::Updated,
                        SidebarGrouping::Status,
                    ]
                    .into_iter()
                    .map(|candidate| {
                        let item_weak = weak.clone();
                        MenuItem::new(sidebar_grouping_label(candidate), move |_, cx| {
                            let _ = item_weak.update(cx, |this, cx| {
                                this.set_sidebar_grouping(candidate, cx);
                            });
                        })
                        .selected(grouping == candidate)
                    })
                    .collect()
                },
            ))
            .into_any_element()
    }

    fn render_sidebar_header_actions(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let menu = self.menu_handle("sidebar-options", cx);
        let menu_open = menu.is_open();
        let weak = cx.entity().downgrade();
        let grouping = self.state.sidebar_grouping;
        let ordering = self.state.sidebar_ordering;
        let show_archived = self.state.sidebar_show_archived;
        let options = dropdown_menu(
            div()
                .id("sidebar-options")
                .w(px(20.0))
                .h(px(20.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .when(menu_open, |element| element.bg(theme.overlay_strong))
                .hover(|element| element.bg(theme.overlay))
                .active(|element| element.bg(theme.overlay_strong))
                .tooltip(Tooltip::text(tr!("sidebar.options")))
                .child(icon("icons/list-filter.svg", 14.0, theme.text_secondary)),
            "sidebar-options-menu",
            &menu,
            MenuAlign::BelowLeft,
            move |_| {
                let grouping_weak = weak.clone();
                let ordering_weak = weak.clone();
                let mut items = vec![MenuItem::submenu_with_value(
                    tr!("sidebar.grouping"),
                    sidebar_grouping_label(grouping),
                    move |_| {
                        let project_weak = grouping_weak.clone();
                        let updated_weak = grouping_weak.clone();
                        let status_weak = grouping_weak.clone();
                        vec![
                            MenuItem::new(tr!("sidebar.grouping_project"), move |_, cx| {
                                let _ = project_weak.update(cx, |this, cx| {
                                    this.set_sidebar_grouping(SidebarGrouping::Project, cx);
                                });
                            })
                            .selected(grouping == SidebarGrouping::Project),
                            MenuItem::new(tr!("sidebar.grouping_updated"), move |_, cx| {
                                let _ = updated_weak.update(cx, |this, cx| {
                                    this.set_sidebar_grouping(SidebarGrouping::Updated, cx);
                                });
                            })
                            .selected(grouping == SidebarGrouping::Updated),
                            MenuItem::new(tr!("sidebar.grouping_status"), move |_, cx| {
                                let _ = status_weak.update(cx, |this, cx| {
                                    this.set_sidebar_grouping(SidebarGrouping::Status, cx);
                                });
                            })
                            .selected(grouping == SidebarGrouping::Status),
                        ]
                    },
                )];
                // The status sections have a fixed order — what needs the user
                // first — so that view offers no ordering control.
                if grouping != SidebarGrouping::Status {
                    items.push(MenuItem::submenu_with_value(
                        tr!("sidebar.ordering"),
                        sidebar_ordering_label(ordering),
                        move |_| {
                            let newest_weak = ordering_weak.clone();
                            let oldest_weak = ordering_weak.clone();
                            vec![
                                MenuItem::new(tr!("sidebar.ordering_newest"), move |_, cx| {
                                    let _ = newest_weak.update(cx, |this, cx| {
                                        this.set_sidebar_ordering(SidebarOrdering::Newest, cx);
                                    });
                                })
                                .selected(ordering == SidebarOrdering::Newest),
                                MenuItem::new(tr!("sidebar.ordering_oldest"), move |_, cx| {
                                    let _ = oldest_weak.update(cx, |this, cx| {
                                        this.set_sidebar_ordering(SidebarOrdering::Oldest, cx);
                                    });
                                })
                                .selected(ordering == SidebarOrdering::Oldest),
                            ]
                        },
                    ));
                }
                // The archive toggle is a view of the same lists, not another
                // list: it reveals what was put away without changing any
                // task's archive state.
                let show_archived_weak = weak.clone();
                items.push(MenuItem::Separator);
                items.push(
                    MenuItem::new(tr!("sidebar.show_archived"), move |_, cx| {
                        let _ = show_archived_weak.update(cx, |this, cx| {
                            this.set_sidebar_show_archived(!show_archived, cx);
                        });
                    })
                    .selected(show_archived),
                );
                items
            },
        );
        let add_project = div()
            .id("add-project")
            .tab_index(0)
            .w(px(20.0))
            .h(px(22.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!("project.new_project")))
            .child(icon("icons/folder-new.svg", 14.0, theme.text_secondary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.add_project(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.add_project(cx);
                    cx.stop_propagation();
                }
            }));

        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(options)
            .child(add_project)
    }

    fn render_sidebar_action_row(
        &self,
        id: &'static str,
        icon_path: &'static str,
        label: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id(id)
            .tab_index(0)
            .w_full()
            .h(px(SIDEBAR_ACTION_ROW_HEIGHT))
            .flex_none()
            .px(px(4.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.overlay_strong))
            .child(
                div()
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(icon_path, 14.0, theme.text_secondary)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(sp(13.0))
                    .text_color(theme.text_secondary)
                    .child(label),
            )
    }

    fn render_sidebar_new_session(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        self.render_sidebar_action_row(
            "sidebar-new-session",
            "icons/compose.svg",
            tr!("menu.new_task"),
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.new_session_action(&NewSession, window, cx);
        }))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                this.new_session_action(&NewSession, window, cx);
                cx.stop_propagation();
            }
        }))
    }

    fn render_sidebar_search(&self, cx: &mut Context<Self>) -> Div {
        let search = self
            .render_sidebar_action_row(
                "sidebar-search",
                "icons/search.svg",
                tr!("sidebar.search"),
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_command_palette_action(&ToggleCommandPalette, window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_command_palette_action(&ToggleCommandPalette, window, cx);
                    cx.stop_propagation();
                }
            }));
        div()
            .w_full()
            .h(px(SIDEBAR_ACTION_ROW_HEIGHT + SIDEBAR_SEARCH_BOTTOM_GAP))
            .flex_none()
            .child(search)
    }

    fn start_available_update(&mut self, cx: &mut Context<Self>) {
        if self.updater_status != crate::updater::UpdateStatus::Available {
            return;
        }
        let started = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|state| state.0.as_ref())
            .is_some_and(|updater| updater.install_available_update());
        if started {
            self.updater_status = crate::updater::UpdateStatus::Updating;
            self.reset_updater_button_animation();
            cx.notify();
        }
    }

    fn render_updater_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.updater_status;
        if status == crate::updater::UpdateStatus::Idle {
            return None;
        }

        let theme = Theme::current(cx);
        let foreground = rgb(0xFFFFFF).into();
        let available = status == crate::updater::UpdateStatus::Available;
        let button = div()
            .id("sidebar-update")
            .track_focus(&self.updater_button_focus)
            .when(available, |button| button.tab_index(0))
            .w(px(UPDATER_BUTTON_COLLAPSED_WIDTH))
            .h(px(20.0))
            .flex_none()
            .overflow_hidden()
            .rounded_full()
            .relative()
            .cursor_default()
            .bg(theme.gauge)
            .text_color(foreground)
            .text_size(sp(12.5))
            .font_weight(FontWeight::MEDIUM)
            .when(available, |button| {
                button
                    .hover(|style| style.opacity(0.92))
                    .focus_visible(|style| style.border_1().border_color(rgb(0xFFFFFF)))
                    .active(|style| style.opacity(0.8))
                    .on_hover(cx.listener(|this, hovering: &bool, _, cx| {
                        this.set_updater_button_hovered(*hovering, cx);
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_available_update(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.start_available_update(cx);
                            cx.stop_propagation();
                        }
                    }))
            });

        if !available {
            let indicator = motion::spin_slow(icon("icons/loader-circle.svg", 14.0, foreground));
            return Some(
                button
                    .tooltip(Tooltip::text(tr!("updater.updating")))
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(indicator),
                    )
                    .into_any_element(),
            );
        }

        let label: SharedString = tr_cow!("updater.update").into();
        let animation_generation = self.updater_button_animation_generation;
        if animation_generation == 0 {
            return Some(
                button
                    .child(updater_button_available_content(foreground, label, 0.0))
                    .into_any_element(),
            );
        }

        let from_width = self.updater_button_animation_from_width;
        let from_reveal = self.updater_button_animation_from_reveal;
        let target_width = if self.updater_button_expanded() {
            UPDATER_BUTTON_EXPANDED_WIDTH
        } else {
            UPDATER_BUTTON_COLLAPSED_WIDTH
        };
        let target_reveal = if self.updater_button_expanded() {
            1.0
        } else {
            0.0
        };
        let current_width = self.updater_button_width.clone();
        let current_reveal = self.updater_button_label_reveal.clone();

        Some(
            button
                .with_animation(
                    SharedString::from(format!("sidebar-updater-expand-{animation_generation}")),
                    Animation::new(Duration::from_millis(150)).with_easing(ease_out_quint()),
                    move |button, delta| {
                        let width = from_width + (target_width - from_width) * delta;
                        let reveal = from_reveal + (target_reveal - from_reveal) * delta;
                        current_width.set(width);
                        current_reveal.set(reveal);
                        button.w(px(width)).child(updater_button_available_content(
                            foreground,
                            label.clone(),
                            reveal,
                        ))
                    },
                )
                .into_any_element(),
        )
    }

    fn render_sidebar_footer(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        div()
            .flex_none()
            .h(px(40.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .child(
                div()
                    .id("open-settings")
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .w(px(26.0))
                    .h(px(26.0))
                    .flex_none()
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .hover(|element| element.bg(theme.overlay))
                    .active(|element| element.bg(theme.overlay_strong))
                    .tooltip(Tooltip::text(tr_cow!("common.settings")))
                    .child(icon("icons/settings.svg", 14.0, theme.text_tertiary))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings_action(&OpenSettings, window, cx);
                    })),
            )
            .child(div().flex_1())
            .when_some(self.render_updater_button(cx), |footer, button| {
                footer.child(button)
            })
    }

    pub(super) fn render_sidebar(
        &self,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let is_resizing = self
            .panel_resize_drag
            .is_some_and(|drag| drag.target == PanelResizeTarget::Sidebar);

        let rows = self.sidebar_rows_cached(Local::now().date_naive(), unix_time());
        self.sync_sidebar_rows(&rows);
        // Restored selection exists before ListState knows the viewport size.
        // Retry after the first layout so nearest-edge alignment has a height.
        if self.sidebar_list_state.viewport_bounds().size.height <= Pixels::ZERO
            && let Some(session_id) = self
                .pending_session_activation
                .map(|pending| pending.session_id)
                .or(self.state.selected_session)
        {
            let entity = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = entity.update(cx, |this, cx| {
                    let selected_session = this
                        .pending_session_activation
                        .map(|pending| pending.session_id)
                        .or(this.state.selected_session);
                    if selected_session == Some(session_id) {
                        this.reveal_sidebar_session(session_id);
                        cx.notify();
                    }
                });
            });
        }
        let history_scrolled =
            self.sidebar_list_state.scroll_px_offset_for_scrollbar().y < px(-0.5);
        let entity = cx.entity().downgrade();

        div()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(if is_resizing {
                theme.sidebar_drag_background
            } else {
                theme.sidebar
            })
            .child(self.render_sidebar_titlebar(window, cx))
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .child(self.render_sidebar_new_session(cx)),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div().px(px(10.0)).size_full().child(
                            list(self.sidebar_list_state.clone(), move |index, window, cx| {
                                entity
                                    .upgrade()
                                    .map(|entity| {
                                        entity.update(cx, |this, cx| {
                                            this.sidebar_row(index, &rows, window, cx)
                                        })
                                    })
                                    .unwrap_or_else(|| div().into_any_element())
                            })
                            .size_full(),
                        ),
                    )
                    .child(scrollbar::vertical(
                        &self.sidebar_list_state,
                        &self.sidebar_scrollbar,
                    ))
                    .when(history_scrolled, |scroll| {
                        scroll.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .w_full()
                                .h(px(1.0))
                                .bg(theme.border),
                        )
                    }),
            )
            .child(self.render_sidebar_footer(cx))
    }

    /// Keep a newly selected task visible without disturbing the sidebar when
    /// its row is already fully inside the viewport.
    pub(super) fn reveal_sidebar_session(&self, session_id: Uuid) {
        let rows = self.sidebar_rows_cached(Local::now().date_naive(), unix_time());
        self.sync_sidebar_rows(&rows);
        if let Some(index) = sidebar_session_row_index(&rows, session_id) {
            reveal_sidebar_list_row(&self.sidebar_list_state, &rows, index);
        }
    }

    /// Refresh the cached row facts for one session, wherever its data
    /// changed: an applied activity, a recorded blockage, an attached runtime
    /// whose transcript has just landed.
    pub(super) fn refresh_sidebar_row_facts(&self, session_id: Uuid) {
        let mut facts = self.sidebar_session_facts.borrow_mut();
        match self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        {
            Some(session) => {
                facts.insert(session_id, sidebar_session_facts(session));
            }
            None => {
                facts.remove(&session_id);
            }
        }
    }

    /// Rebuild the cached row facts for every session in one pass. The catalog
    /// owns which sessions exist, so this is also what forgets the tasks that
    /// are gone.
    pub(super) fn rebuild_sidebar_row_facts(&self) {
        rebuild_sidebar_session_facts(
            &mut self.sidebar_session_facts.borrow_mut(),
            &self.state.sessions,
        );
    }

    /// The sidebar row snapshot, rebuilt only when its inputs move.
    ///
    /// The sidebar re-renders at pulse cadence whenever one of its session
    /// rows shows a working spinner, and rebuilding the snapshot sorts every
    /// started session and runs calendar math per session — far too much per
    /// tick for values that move at most once per stream commit. The
    /// fingerprint is an allocation-free scan of exactly what
    /// [`Self::sidebar_rows`] reads: started sessions with their project,
    /// recency, status and archive state, the presentation preferences, the
    /// collapsed-group set, and today's date and the moving project-recency
    /// boundary.
    ///
    /// A fact the rows are filtered or partitioned by has to be in here, or the
    /// snapshot would outlive the rule that produced it — which is why the
    /// archived-task visibility toggle, the archive state of the open task,
    /// and each row's own status and archive state are all folded in.
    fn sidebar_rows_cached(&self, today: NaiveDate, now: u64) -> Rc<Vec<SidebarRow>> {
        let mut fingerprint = mix(0x51de_ba5e_5eed_c0de, today.num_days_from_ce() as u64);
        fingerprint = mix(
            fingerprint,
            match self.state.sidebar_grouping {
                SidebarGrouping::Project => 1,
                SidebarGrouping::Updated => 2,
                SidebarGrouping::Status => 3,
            },
        );
        fingerprint = mix(
            fingerprint,
            match self.state.sidebar_ordering {
                SidebarOrdering::Newest => 1,
                SidebarOrdering::Oldest => 2,
            },
        );
        let mut active_task_archived = false;
        for session in &self.state.sessions {
            if !session.has_started() {
                continue;
            }
            if Some(session.id) == self.state.selected_session {
                active_task_archived = session.archived_at.is_some();
            }
            fingerprint = mix_sidebar_session_facts(fingerprint, session);
            if self.state.sidebar_grouping == SidebarGrouping::Project {
                fingerprint = mix(
                    fingerprint,
                    u64::from(
                        sidebar_session_timestamp(session)
                            >= now.saturating_sub(SIDEBAR_PROJECT_RECENT_WINDOW_SECONDS),
                    ),
                );
            }
        }
        fingerprint = mix_sidebar_archived_visibility(
            fingerprint,
            self.state.sidebar_show_archived,
            active_task_archived,
        );
        if self.state.sidebar_grouping == SidebarGrouping::Project {
            for project in &self.state.projects {
                fingerprint = mix_uuid(fingerprint, project.id);
            }
            // A map has no stable iteration order; combine order-independently.
            let revealed =
                self.sidebar_project_reveal_counts
                    .iter()
                    .fold(0u64, |combined, (group, count)| {
                        combined.wrapping_add(group.mix_fingerprint(*count as u64))
                    });
            fingerprint = mix(
                mix(fingerprint, self.sidebar_project_reveal_counts.len() as u64),
                revealed,
            );
        }
        // A set has no stable iteration order; combine order-independently.
        let collapsed = self
            .sidebar_collapsed_groups
            .iter()
            .fold(0u64, |combined, group| {
                combined.wrapping_add(group.mix_fingerprint(0))
            });
        fingerprint = mix(
            mix(fingerprint, self.sidebar_collapsed_groups.len() as u64),
            collapsed,
        );
        if self.sidebar_rows_fingerprint.get() != Some(fingerprint) {
            let (rows, status_section_counts, archived_count) = self.sidebar_rows(today, now);
            // All three come out of the same pass, so a header can never label
            // itself with a count the rows do not back.
            *self.sidebar_rows_snapshot.borrow_mut() = Rc::new(rows);
            *self.sidebar_status_section_counts.borrow_mut() = status_section_counts;
            self.sidebar_archived_count.set(archived_count);
            self.sidebar_rows_fingerprint.set(Some(fingerprint));
        }
        self.sidebar_rows_snapshot.borrow().clone()
    }

    /// Whether the open task is one that was put away. The task the window is
    /// showing is never a gap in the list showing it, so this is the second
    /// half of the trailing archived section's gate.
    fn active_task_is_archived(&self) -> bool {
        self.state.selected_session.is_some_and(|selected| {
            self.state
                .sessions
                .iter()
                .any(|session| session.id == selected && session.archived_at.is_some())
        })
    }

    /// Snapshot the session history as a flat list of lightweight rows under
    /// the current grouping and ordering preferences, together with the counts
    /// the section headers label themselves with: one per active status
    /// section, and the trailing archived section's own.
    fn sidebar_rows(
        &self,
        today: NaiveDate,
        now: u64,
    ) -> (
        Vec<SidebarRow>,
        [usize; SidebarStatusSection::ALL.len()],
        usize,
    ) {
        let mut sorted_sessions = self
            .state
            .sessions
            .iter()
            .filter(|session| session.has_started())
            .collect::<Vec<_>>();
        sort_sidebar_sessions(&mut sorted_sessions, self.state.sidebar_ordering);

        // An archived task has exactly one home, whatever its status and
        // whatever the view above it would otherwise have done with it, so it
        // is taken out of this view's sections here and put back in the
        // trailing one at the bottom.
        let (sorted_sessions, archived_ids, archived_section_stands) = split_archived_sessions(
            sorted_sessions,
            self.state.sidebar_show_archived,
            self.active_task_is_archived(),
        );

        let mut status_section_counts = [0usize; SidebarStatusSection::ALL.len()];
        let mut rows = vec![SidebarRow::Search];
        match self.state.sidebar_grouping {
            SidebarGrouping::Status => {
                // Every section's tasks are already in render order, so the
                // ordering preference does not reach this view.
                let sections = status_sidebar_sections(&sorted_sessions);
                for section in SidebarStatusSection::ALL {
                    let group = section.group();
                    let session_ids = &sections[section.index()];
                    status_section_counts[section.index()] = session_ids.len();
                    append_sidebar_group_rows(
                        &mut rows,
                        group,
                        session_ids,
                        self.sidebar_collapsed_groups.contains(&group),
                        false,
                    );
                }
            }
            SidebarGrouping::Updated => {
                let mut grouped_sessions: [Vec<Uuid>; 6] = std::array::from_fn(|_| Vec::new());
                for session in sorted_sessions {
                    grouped_sessions
                        [session_date_group(sidebar_session_timestamp(session), today).index()]
                    .push(session.id);
                }
                let mut groups = SessionDateGroup::ALL;
                if self.state.sidebar_ordering == SidebarOrdering::Oldest {
                    groups.reverse();
                }
                for date_group in groups {
                    let group = SidebarGroup::Updated(date_group);
                    append_sidebar_group_rows(
                        &mut rows,
                        group,
                        &grouped_sessions[date_group.index()],
                        self.sidebar_collapsed_groups.contains(&group),
                        false,
                    );
                }
            }
            SidebarGrouping::Project => {
                let recent_cutoff = now.saturating_sub(SIDEBAR_PROJECT_RECENT_WINDOW_SECONDS);
                let session_timestamps = sorted_sessions
                    .iter()
                    .map(|session| (session.id, sidebar_session_timestamp(session)))
                    .collect::<HashMap<_, _>>();
                let projectless_root = crate::projectless::workspace_root();
                let projectless_project_ids = self
                    .state
                    .projects
                    .iter()
                    .filter(|project| {
                        sidebar_project_is_projectless(project, projectless_root.as_deref())
                    })
                    .map(|project| project.id)
                    .collect::<HashSet<_>>();
                for (group, sessions) in
                    project_sidebar_groups(&sorted_sessions, &projectless_project_ids)
                {
                    let revealed_older_sessions = self
                        .sidebar_project_reveal_counts
                        .get(&group)
                        .copied()
                        .unwrap_or_default();
                    let (visible_sessions, show_more) = visible_project_sessions(
                        &sessions,
                        &session_timestamps,
                        recent_cutoff,
                        revealed_older_sessions,
                    );
                    append_sidebar_group_rows(
                        &mut rows,
                        group,
                        &visible_sessions,
                        self.sidebar_collapsed_groups.contains(&group),
                        show_more,
                    );
                }
            }
        }
        let archived_count = archived_ids.len();
        if archived_section_stands {
            append_sidebar_archived_section(
                &mut rows,
                &archived_ids,
                self.sidebar_collapsed_groups
                    .contains(&SidebarGroup::Archived),
            );
        }
        if rows.len() == 1 {
            // Keep the header actions visible while there is no history.
            let group = match self.state.sidebar_grouping {
                SidebarGrouping::Status => SidebarGroup::NeedsYou,
                SidebarGrouping::Updated => SidebarGroup::Updated(SessionDateGroup::Today),
                SidebarGrouping::Project => {
                    let projectless_root = crate::projectless::workspace_root();
                    self.state
                        .selected_project
                        .and_then(|project_id| {
                            self.state
                                .projects
                                .iter()
                                .find(|project| project.id == project_id)
                        })
                        .or_else(|| self.state.projects.first())
                        .map(|project| {
                            if sidebar_project_is_projectless(project, projectless_root.as_deref())
                            {
                                SidebarGroup::Projectless
                            } else {
                                SidebarGroup::Project(project.id)
                            }
                        })
                        .unwrap_or(SidebarGroup::Projectless)
                }
            };
            rows.push(SidebarRow::Header(group));
        }
        (rows, status_section_counts, archived_count)
    }

    /// Keep the virtualized list in sync with the current row snapshot.
    /// Rows are cheap values, so only the minimal changed suffix is spliced,
    /// preserving scroll position and measured heights across unrelated churn
    /// (e.g. the active session's `updated_at` bumping on every stream tick).
    fn sync_sidebar_rows(&self, rows: &[SidebarRow]) {
        let mut cached = self.sidebar_row_cache.borrow_mut();
        if cached.as_slice() == rows {
            return;
        }
        let prefix = cached
            .iter()
            .zip(rows.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let old_count = cached.len();
        *cached = rows.to_vec();
        if old_count == 0 {
            self.sidebar_list_state
                .reset_with_uniform_height(rows.len(), px(SIDEBAR_SESSION_ROW_HEIGHT));
        } else {
            self.sidebar_list_state
                .splice(prefix..old_count, rows.len() - prefix);
            // Newly inserted rows have no measured height yet; give them the
            // uniform hint so the scrollbar keeps a correct total height.
            self.sidebar_list_state
                .clone()
                .with_uniform_item_height(px(SIDEBAR_SESSION_ROW_HEIGHT));
        }
    }

    fn sidebar_row(
        &self,
        index: usize,
        rows: &[SidebarRow],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = rows.get(index) else {
            return div().into_any_element();
        };
        match *row {
            SidebarRow::Search => self.render_sidebar_search(cx).into_any_element(),
            SidebarRow::Header(group) => self
                .render_sidebar_group_header(group, index == 1, cx)
                .into_any_element(),
            SidebarRow::Session(session_id) => self
                .render_sidebar_session_item(session_id, window, cx)
                .into_any_element(),
            SidebarRow::ShowMore(group) => {
                self.render_sidebar_show_more(group, cx).into_any_element()
            }
            SidebarRow::GroupSpacer => div()
                .w_full()
                .h(px(SIDEBAR_GROUP_SPACER_HEIGHT))
                .into_any_element(),
        }
    }

    fn render_sidebar_group_header(
        &self,
        group: SidebarGroup,
        first: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let collapsed = self.sidebar_collapsed_groups.contains(&group);
        let group_key = group.element_key();
        let group_name = SharedString::from(format!("sidebar-group-header-{group_key}"));
        let header_focus = self
            .sidebar_group_header_focuses
            .borrow_mut()
            .entry(group)
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let show_folder_icon =
            matches!(group, SidebarGroup::Project(_) | SidebarGroup::Projectless);
        let folder_icon = if collapsed {
            "icons/folder.svg"
        } else {
            "icons/folder-open.svg"
        };
        let label = match group {
            SidebarGroup::Updated(group) => group.label(),
            SidebarGroup::NeedsYou => tr!("sidebar.section_needs_you"),
            SidebarGroup::Running => tr!("sidebar.section_running"),
            SidebarGroup::Recent => tr!("sidebar.section_recent"),
            SidebarGroup::Archived => tr!("sidebar.section_archived"),
            SidebarGroup::Project(project_id) => self
                .state
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .map(Project::display_name)
                .unwrap_or_else(|| tr!("project.no_project_name")),
            SidebarGroup::Projectless => tr!("project.no_project_name"),
        };
        // A status section is labeled with how many tasks it holds, and hints
        // at its collapse control on hover. The other views' groups carry a
        // date or a folder instead, and the lone placeholder header that keeps
        // the sidebar actions reachable while there is no history carries
        // neither. The count comes from the pass that built the row snapshot;
        // a header must not count while it renders.
        let status_section = SidebarStatusSection::ALL
            .into_iter()
            .find(|section| section.group() == group);
        let count = status_section
            .map(|section| self.sidebar_status_section_counts.borrow()[section.index()])
            .or_else(|| {
                (group == SidebarGroup::Archived).then(|| self.sidebar_archived_count.get())
            })
            .filter(|count| *count > 0);
        let collapse_hint = (matches!(group, SidebarGroup::Updated(_) | SidebarGroup::Archived)
            || status_section.is_some())
        .then(|| {
            icon("icons/chevron-down.svg", 14.0, theme.text_secondary)
                .when(collapsed, |icon| {
                    icon.with_transformation(gpui::Transformation::rotate(gpui::percentage(0.75)))
                })
                .invisible()
                .group_hover(group_name.clone(), |icon| icon.visible())
        });
        let compose = show_folder_icon.then(|| {
            let compose_focus = self
                .sidebar_group_compose_focuses
                .borrow_mut()
                .entry(group)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            div()
                .w(px(20.0))
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "sidebar-group-compose-{group_key}"
                        )))
                        .track_focus(&compose_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .w_0()
                        .h(px(22.0))
                        .overflow_hidden()
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_default()
                        .opacity(0.0)
                        .group_hover(group_name.clone(), |style| style.w(px(20.0)).opacity(1.0))
                        .focus_visible(|style| {
                            style
                                .w(px(20.0))
                                .opacity(1.0)
                                .border_1()
                                .border_color(theme.accent)
                        })
                        .hover(|style| style.bg(theme.overlay))
                        .active(|style| style.bg(theme.overlay_strong))
                        .tooltip(Tooltip::text(tr!("menu.new_task")))
                        .child(icon("icons/compose.svg", 14.0, theme.text_secondary))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_new_task_for_sidebar_group(group, window, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.open_new_task_for_sidebar_group(group, window, cx);
                                cx.stop_propagation();
                            }
                        })),
                )
        });

        let header = session_group_header(&theme)
            .id(SharedString::from(format!(
                "sidebar-group-toggle-{group_key}"
            )))
            .track_focus(&header_focus)
            .tab_index(0)
            .tab_group()
            .tab_stop(true)
            .group(group_name)
            .w_full()
            .rounded(px(6.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.sidebar_item_background))
            .active(|style| style.bg(theme.overlay_strong))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .when(show_folder_icon, |element| {
                        element.child(icon(folder_icon, 14.0, theme.text_secondary))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .child(div().min_w_0().truncate().child(label))
                            .when_some(collapse_hint, |element, chevron| element.child(chevron)),
                    )
                    .child(div().flex_1()),
            )
            .when_some(count, |element, count| {
                element.child(
                    div()
                        .flex_none()
                        .pl(px(6.0))
                        .text_size(sp(12.5))
                        .line_height(sp(0.0))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(count.to_string())),
                )
            })
            .when_some(compose, |element, compose| element.child(compose))
            .when(first, |element| {
                element.child(self.render_sidebar_header_actions(cx))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_sidebar_group(group, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "enter" | "space" => {
                        this.toggle_sidebar_group(group, cx);
                        cx.stop_propagation();
                    }
                    "left" if !collapsed => {
                        this.set_sidebar_group_collapsed(group, true, cx);
                        cx.stop_propagation();
                    }
                    "right" if collapsed => {
                        this.set_sidebar_group_collapsed(group, false, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }));

        div()
            .w_full()
            .pb(px(SIDEBAR_GROUP_HEADER_BOTTOM_GAP))
            .child(header)
    }

    fn open_new_task_for_sidebar_group(
        &mut self,
        group: SidebarGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_page = None;
        match group {
            SidebarGroup::Project(project_id) => self.select_project(project_id, cx),
            SidebarGroup::Projectless => self.create_projectless_session(cx),
            // Only a project section can start a task, so neither the status
            // sections, the archived section, nor a date heading offer the
            // action.
            SidebarGroup::Updated(_)
            | SidebarGroup::NeedsYou
            | SidebarGroup::Running
            | SidebarGroup::Recent
            | SidebarGroup::Archived => return,
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
    }

    fn render_sidebar_show_more(&self, group: SidebarGroup, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let group_key = group.element_key();
        let focus = self
            .sidebar_show_more_focuses
            .borrow_mut()
            .entry(group)
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let button = div()
            .id(SharedString::from(format!("sidebar-show-more-{group_key}")))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .flex_none()
            .cursor_default()
            .text_size(sp(12.5))
            .text_color(theme.text_tertiary)
            .focus_visible(|style| style.text_color(theme.text))
            .hover(|style| style.text_color(theme.text))
            .child(tr!("sidebar.show_more"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.show_more_project_sessions(group, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.show_more_project_sessions(group, cx);
                    cx.stop_propagation();
                }
            }));

        div()
            .w_full()
            .h(px(SIDEBAR_SHOW_MORE_ROW_HEIGHT))
            .pl(px(SIDEBAR_GROUP_CHILD_PADDING))
            .flex()
            .items_center()
            .child(button)
    }

    fn show_more_project_sessions(&mut self, group: SidebarGroup, cx: &mut Context<Self>) {
        let revealed = self.sidebar_project_reveal_counts.entry(group).or_default();
        *revealed = revealed.saturating_add(SIDEBAR_PROJECT_REVEAL_BATCH);
        self.sidebar_rows_fingerprint.set(None);
        cx.notify();
    }

    fn toggle_sidebar_group(&mut self, group: SidebarGroup, cx: &mut Context<Self>) {
        let collapsed = !self.sidebar_collapsed_groups.contains(&group);
        self.set_sidebar_group_collapsed(group, collapsed, cx);
    }

    pub(super) fn collapse_all_sidebar_groups(&mut self, cx: &mut Context<Self>) {
        let groups = self
            .sidebar_rows_cached(Local::now().date_naive(), unix_time())
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Header(group) => Some(*group),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut changed = false;
        for group in groups {
            changed |= self.sidebar_collapsed_groups.insert(group);
            changed |= self.sidebar_project_reveal_counts.remove(&group).is_some();
        }
        if changed {
            self.sidebar_rows_fingerprint.set(None);
            cx.notify();
        }
    }

    fn set_sidebar_group_collapsed(
        &mut self,
        group: SidebarGroup,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        let collapse_changed = if collapsed {
            self.sidebar_collapsed_groups.insert(group)
        } else {
            self.sidebar_collapsed_groups.remove(&group)
        };
        let reveal_reset = collapsed && self.sidebar_project_reveal_counts.remove(&group).is_some();
        if collapse_changed || reveal_reset {
            self.sidebar_rows_fingerprint.set(None);
            cx.notify();
        }
    }

    fn set_sidebar_grouping(&mut self, grouping: SidebarGrouping, cx: &mut Context<Self>) {
        if self.state.sidebar_grouping == grouping {
            return;
        }
        self.state.sidebar_grouping = grouping;
        // A grouping that actually changed is the user's pick, not a default:
        // the marker is what keeps a future change of default from rewriting
        // it on the next launch.
        self.state.sidebar_grouping_chosen = true;
        self.sidebar_rows_fingerprint.set(None);
        self.sidebar_list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: Pixels::ZERO,
        });
        self.save(cx);
        cx.notify();
    }

    fn set_sidebar_ordering(&mut self, ordering: SidebarOrdering, cx: &mut Context<Self>) {
        if self.state.sidebar_ordering == ordering {
            return;
        }
        self.state.sidebar_ordering = ordering;
        self.sidebar_rows_fingerprint.set(None);
        self.sidebar_list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: Pixels::ZERO,
        });
        self.save(cx);
        cx.notify();
    }

    /// Reveal or hide the tasks that have been put away.
    ///
    /// This is a list preference, not a change to any task: the catalog keeps
    /// every task either way, which is what keeps an archived one findable by
    /// search. Whether a task is archived at all is the daemon's to say, and
    /// changes only through [`Self::set_session_archived`].
    pub(super) fn set_sidebar_show_archived(
        &mut self,
        show_archived: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.sidebar_show_archived == show_archived {
            return;
        }
        self.state.sidebar_show_archived = show_archived;
        self.sidebar_rows_fingerprint.set(None);
        self.save(cx);
        cx.notify();
    }

    /// Put one task away, or bring it back.
    ///
    /// The daemon owns the archive time and applies the action itself; the
    /// client sends the action and mirrors the daemon's own move onto the
    /// row's copy so the list changes on the frame the user asked rather than
    /// on the next catalog refresh. Nothing else about the task is touched —
    /// putting a task away frees nothing.
    pub(super) fn set_session_archived(
        &mut self,
        session_id: Uuid,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let moved = apply_archive_action(session, archived, unix_time());
        if let Err(error) = self.store.set_task_archived(session_id, archived) {
            self.show_toast(tr!("errors.save_local_state", error = error));
        }
        if moved {
            self.sidebar_rows_fingerprint.set(None);
        }
        cx.notify();
    }

    fn begin_session_rename(
        &mut self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(localized_session_title)
        else {
            return;
        };

        self.session_rename = Some(session_id);
        self.session_rename_input.update(cx, |input, cx| {
            input.set_content(title, cx);
            input.select_all_text(cx);
        });
        let focus = self.session_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn commit_session_rename(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_rename.take() else {
            return;
        };
        let title = self
            .session_rename_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let should_update = !title.is_empty()
            && self
                .state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .is_some_and(|session| session.title != title);
        if should_update
            && self
                .state
                .session_mut(session_id)
                .is_some_and(|session| session.set_title(&title))
        {
            self.save(cx);
        }
        cx.notify();
    }

    fn cancel_session_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_rename.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn render_sidebar_session_item(
        &self,
        session_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return div().into_any_element();
        };
        let selected = sidebar_session_selected(
            self.state.selected_session,
            self.pending_session_activation
                .map(|pending| pending.session_id),
            session_id,
        );
        let working = matches!(
            session.status,
            SessionStatus::Connecting | SessionStatus::Working
        );
        let project = self
            .state
            .projects
            .iter()
            .find(|project| project.id == session.project_id);
        let grouped_by_project = self.state.sidebar_grouping == SidebarGrouping::Project;
        let left_padding = if grouped_by_project {
            SIDEBAR_GROUP_CHILD_PADDING
        } else {
            8.0
        };
        // The row reports the task's present state whenever it has any, and
        // otherwise names what the task is working on: its own branch when the
        // client knows it, its project when it does not. Both come from the
        // cached row facts, never from a transcript walk here.
        let facts = self
            .sidebar_session_facts
            .borrow()
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let detail = sidebar_row_detail(session, &facts);
        // What the card answers with, resolved once here from the values the
        // client already holds: the hover tooltip and the focus-revealed card
        // render the same content.
        let card = sidebar_task_card(session, &facts, project, unix_time());
        let menu = self.menu_handle(format!("session-{session_id}"), cx);
        let row_focus = sidebar_row_focus(&menu);
        let card_handle = sidebar_task_card_handle(session_id, &row_focus, window, cx);
        // A view whose sections are not projects has to keep naming the
        // project in the row; a project section heading already names it, so
        // there the state stands alone. The label is resolved only when it is
        // drawn, because this runs for every visible row on every frame.
        let identifier = (detail.is_none() || !grouped_by_project)
            .then(|| sidebar_row_identifier(grouped_by_project, session, project));
        let has_detail = detail.is_some();
        let rename_input =
            (self.session_rename == Some(session_id)).then(|| self.session_rename_input.clone());
        let renaming = rename_input.is_some();
        let title = if let Some(rename_input) = rename_input {
            div()
                .id(SharedString::from(format!(
                    "session-rename-field-{session_id}"
                )))
                .key_context(SESSION_RENAME_PARENT_CONTEXT)
                .on_action(cx.listener(|this, _: &CancelSessionRename, window, cx| {
                    this.cancel_session_rename(window, cx);
                }))
                .h(px(18.0))
                .flex_1()
                .min_w_0()
                .px(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.accent)
                .bg(theme.inset)
                .flex()
                .items_center()
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(rename_input)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .line_clamp(1)
                .text_overflow(gpui::TextOverflow::Truncate("...".into()))
                .text_size(sp(13.5))
                .text_color(theme.text)
                .child(SharedString::from(localized_session_title(session)))
                .into_any_element()
        };
        let waku = cx.entity().downgrade();
        let keyboard_menu = menu.clone();
        // A task that was put away says so on its own row, not only by where
        // the row is filed: the one archived task the window can be showing
        // while archived tasks are hidden is the task it is showing.
        let archived = session.archived_at.is_some();
        let row = div()
            .id(SharedString::from(format!("session-{}", session.id)))
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .pl(px(left_padding))
            .pr(px(8.0))
            .py(px(7.0))
            .rounded(px(7.0))
            .cursor_default()
            .when(selected, |element| {
                element.bg(theme.sidebar_item_background)
            })
            .hover(|element| element.bg(theme.sidebar_item_background))
            .active(|element| element.bg(theme.sidebar_item_background))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .overflow_hidden()
                    .line_height(sp(18.0))
                    .child(title)
                    .when(working, |element| {
                        element.child(motion::spin_slow(icon(
                            "icons/loader-circle.svg",
                            12.0,
                            status_color(&theme, session.status),
                        )))
                    })
                    .when(session.status == SessionStatus::Background, |element| {
                        element.child(icon(
                            "icons/hourglass.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(session.status == SessionStatus::Waiting, |element| {
                        element.child(icon(
                            "icons/alert.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(session.status == SessionStatus::Failed, |element| {
                        element.child(icon(
                            "icons/x.svg",
                            12.0,
                            status_color(&theme, session.status),
                        ))
                    })
                    .when(archived, |element| {
                        element.child(icon("icons/package.svg", 12.0, theme.text_tertiary))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(sp(if grouped_by_project { 12.5 } else { 13.0 }))
                    .line_height(sp(15.0))
                    .when_some(
                        identifier,
                        |element, (identifier_icon, identifier_label)| {
                            element
                                .child(icon(identifier_icon, 12.5, theme.text_tertiary))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.text_tertiary)
                                        .child(identifier_label),
                                )
                        },
                    )
                    .when_some(detail, |element, detail| {
                        element
                            .child(icon(detail.icon(), 12.5, theme.text_tertiary))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_tertiary)
                                    .child(detail.text().clone()),
                            )
                    })
                    .when(!has_detail, |element| element.child(div().flex_1()))
                    .when_some(
                        session_time_label(session, unix_time()),
                        |element, label| {
                            element.child(
                                div()
                                    .flex_none()
                                    .text_size(sp(12.5))
                                    .text_color(if session.is_busy() {
                                        theme.text_tertiary
                                    } else {
                                        theme.text_ghost
                                    })
                                    .child(SharedString::from(label)),
                            )
                        },
                    ),
            )
            .when(!renaming, |element| {
                element
                    .track_focus(&row_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if matches!(key, "enter" | "space") {
                            this.select_session(session_id, cx);
                            cx.stop_propagation();
                        } else if key == "f10" && event.keystroke.modifiers.shift {
                            keyboard_menu.open_context_menu(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(session_id, cx);
                    }))
            });
        let row = if renaming {
            div()
                .w_full()
                .child(row)
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    if this.session_rename == Some(session_id) {
                        this.commit_session_rename(cx);
                    }
                }))
                .into_any_element()
        } else {
            context_menu(
                with_sidebar_task_card(row, card, &card_handle),
                SharedString::from(format!("session-menu-{session_id}")),
                &menu,
                move |_| {
                    let rename_waku = waku.clone();
                    let archive_waku = waku.clone();
                    let remove_waku = waku.clone();
                    vec![
                        MenuItem::new(tr!("common.rename"), move |window, cx| {
                            let _ = rename_waku.update(cx, |waku, cx| {
                                waku.begin_session_rename(session_id, window, cx);
                            });
                        }),
                        MenuItem::Separator,
                        // An archived task is offered the way back instead: one
                        // action, saying what this task's next move is.
                        MenuItem::new(
                            if archived {
                                tr!("sidebar.bring_back")
                            } else {
                                tr!("sidebar.put_away")
                            },
                            move |_, cx| {
                                let _ = archive_waku.update(cx, |waku, cx| {
                                    waku.set_session_archived(session_id, !archived, cx);
                                });
                            },
                        ),
                        MenuItem::Separator,
                        MenuItem::new(tr!("common.remove"), move |_, cx| {
                            let _ = remove_waku
                                .update(cx, |waku, cx| waku.remove_session(session_id, cx));
                        }),
                    ]
                },
            )
        };

        div()
            .w_full()
            .pb(px(SIDEBAR_SESSION_ROW_GAP))
            .child(row)
            .into_any_element()
    }

    // ── Header ─────────────────────────────────────────────────────────────

    pub(super) fn render_header(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::current(cx);
        let session = self.selected_session();
        let title = session
            .map(localized_session_title)
            .unwrap_or_else(|| tr!("session.new_task"));
        let agent_preset_label = session
            .filter(|session| session.provider == ProviderKind::DeepSeek && session.has_started())
            .and_then(|session| self.agent_preset_label_for_session(session));
        let left_window_controls = (!self.sidebar_visible)
            .then(|| {
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Left,
                    window,
                    cx,
                )
            })
            .flatten();
        let right_window_controls = (!self.right_panel_visible)
            .then(|| {
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Right,
                    window,
                    cx,
                )
            })
            .flatten();
        div()
            .id("window-header")
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .children(left_window_controls)
            // The header starts where the sidebar ends, so until the sidebar
            // is wide enough to host the traffic lights itself the header has
            // to clear them. Steady state with the sidebar open adds nothing;
            // a sidebar sliding in shrinks the inset as it takes the lights
            // over, which is what keeps the title from passing under them.
            .pl(if self.sidebar_visible {
                px(14.0 + (TRAFFIC_LIGHT_CLEARANCE - self.sidebar_rendered_width).max(0.0))
            } else {
                px(0.0)
            })
            .pr(px(14.0))
            .when(!self.sidebar_visible, |element| {
                element
                    .child(
                        self.window_drag_region(
                            div()
                                .id("header-traffic-light-drag-region")
                                .w(px(TRAFFIC_LIGHT_CLEARANCE - 8.0))
                                .h_full()
                                .flex_none(),
                            cx,
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(self.render_sidebar_toggle(cx))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(self.render_history_button(
                                        "navigate-back",
                                        "icons/arrow-left.svg",
                                        !self.session_navigation.back.is_empty(),
                                        true,
                                        cx,
                                    ))
                                    .child(self.render_history_button(
                                        "navigate-forward",
                                        "icons/arrow-right.svg",
                                        !self.session_navigation.forward.is_empty(),
                                        false,
                                        cx,
                                    )),
                            ),
                    )
            })
            .child(
                self.window_drag_region(
                    div()
                        .id("header-title-drag-region")
                        .h_full()
                        .min_w_0()
                        .flex_shrink(1.0)
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(sp(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        )
                        .children(agent_preset_label.map(|label| {
                            div()
                                .h(px(22.0))
                                .max_w(px(180.0))
                                .px(px(6.0))
                                .rounded(px(6.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .bg(theme.overlay)
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_secondary)
                                .child(icon("icons/bot.svg", 10.5, theme.text_tertiary))
                                .child(div().min_w_0().truncate().child(SharedString::from(label)))
                        }))
                        // Several debug builds can run side by side, each on its
                        // own checkout's database; the badge says which is
                        // which. Release builds have one app and no badge.
                        .children(crate::instance::debug_label().map(|label| {
                            div()
                                .id("debug-instance")
                                .h(px(22.0))
                                .max_w(px(240.0))
                                .px(px(6.0))
                                .rounded(px(6.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .bg(theme.overlay)
                                .text_size(sp(11.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_ghost)
                                .tooltip(Tooltip::text(format!(
                                    "debug build · database {}",
                                    crate::instance::database_path()
                                )))
                                .child(div().min_w_0().truncate().child(SharedString::from(label)))
                        })),
                    cx,
                ),
            )
            .child(
                self.window_drag_region(
                    div().id("header-center-drag-region").h_full().flex_1(),
                    cx,
                ),
            )
            .child(self.render_background_work_summary(cx))
            .when(!self.right_panel_visible, |element| {
                element
                    .when(self.fps_counter_visible, |element| {
                        element.child(self.render_fps_counter(cx))
                    })
                    .child(self.render_right_panel_toggle(cx))
            })
            .children(right_window_controls)
    }

    // ── Empty states ───────────────────────────────────────────────────────

    pub(super) fn render_empty_state(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        if self.selected_project().is_none() {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px_8()
                .pb(px(46.0))
                .child(icon("icons/sparkle.svg", 24.0, theme.accent))
                .child(
                    div()
                        .mt(px(16.0))
                        .text_size(sp(20.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr_cow!("onboarding.open_project_to_begin")),
                )
                .child(
                    div()
                        .mt(px(8.0))
                        .max_w(px(380.0))
                        .text_center()
                        .text_size(sp(12.5))
                        .line_height(sp(19.0))
                        .text_color(theme.text_tertiary)
                        .child(tr_cow!("onboarding.description")),
                )
                .child(
                    div()
                        .mt(px(20.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.0))
                        .tab_index(0)
                        .tab_group()
                        .tab_stop(false)
                        .child(
                            div()
                                .id("onboarding-add-project")
                                .track_focus(&self.onboarding_add_project_focus)
                                .tab_index(0)
                                .focus_visible(|style| style.border_1().border_color(theme.accent))
                                .h(px(32.0))
                                .px(px(14.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .cursor_default()
                                .bg(theme.inverse)
                                .text_color(theme.on_inverse)
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .hover(|element| element.opacity(0.9))
                                .active(|element| element.opacity(0.8))
                                .child(tr_cow!("onboarding.open_project_folder"))
                                .on_click(cx.listener(|this, _, _, cx| this.add_project(cx)))
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.add_project(cx);
                                        cx.stop_propagation();
                                    }
                                })),
                        )
                        .child(
                            div()
                                .id("onboarding-projectless")
                                .track_focus(&self.onboarding_projectless_focus)
                                .tab_index(1)
                                .focus_visible(|style| style.border_1().border_color(theme.accent))
                                .h(px(30.0))
                                .px(px(12.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .cursor_default()
                                .text_color(theme.text_secondary)
                                .text_size(sp(12.5))
                                .hover(|element| element.bg(theme.overlay))
                                .active(|element| element.bg(theme.overlay_strong))
                                .child(icon("icons/x.svg", 11.0, theme.text_tertiary))
                                .child(tr_cow!("project.no_project"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.create_projectless_session(cx);
                                }))
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.create_projectless_session(cx);
                                        cx.stop_propagation();
                                    }
                                })),
                        ),
                );
        }
        let selected_project_id = self.state.selected_project;
        let projectless_selected = self.selected_project().is_some_and(Project::is_projectless);
        let project_name = self
            .selected_project()
            .map(|project| {
                if project.is_projectless() {
                    tr!("project.without_a_project")
                } else {
                    project.display_name()
                }
            })
            .unwrap_or_else(|| tr!("project.your_project"));
        let project_options = self
            .state
            .projects
            .iter()
            .filter(|project| !project.is_projectless())
            .filter(|project| Some(project.id) == selected_project_id)
            .chain(
                self.state
                    .projects
                    .iter()
                    .filter(|project| !project.is_projectless())
                    .filter(|project| Some(project.id) != selected_project_id),
            )
            .map(|project| (project.id, project.display_name()))
            .collect::<Vec<_>>();
        let weak = cx.entity().downgrade();
        let handle = self.menu_handle("empty-state-project", cx);
        let project_selector = dropdown_menu(
            ProjectNameSelector::new("empty-state-project", project_name)
                .selected(handle.is_open()),
            "empty-state-project-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |_| {
                let mut items = project_options
                    .clone()
                    .into_iter()
                    .map(|(project_id, project_name)| {
                        let weak = weak.clone();
                        MenuItem::new(project_name, move |_, cx| {
                            if Some(project_id) == selected_project_id {
                                return;
                            }
                            let _ = weak.update(cx, |this, cx| this.select_project(project_id, cx));
                        })
                        .selected(Some(project_id) == selected_project_id)
                    })
                    .collect::<Vec<_>>();
                if !items.is_empty() {
                    items.push(MenuItem::Separator);
                }
                let add_project_weak = weak.clone();
                items.push(
                    MenuItem::new(tr!("project.new_project"), move |_, cx| {
                        let _ = add_project_weak.update(cx, |this, cx| this.add_project(cx));
                    })
                    .icon("icons/folder-new.svg"),
                );
                let projectless_weak = weak.clone();
                items.push(
                    MenuItem::new(tr!("project.no_project"), move |_, cx| {
                        let _ = projectless_weak.update(cx, |this, cx| {
                            if !this.selected_project().is_some_and(Project::is_projectless) {
                                this.create_projectless_session(cx);
                            }
                        });
                    })
                    .icon("icons/x.svg")
                    .selected(projectless_selected),
                );
                items
            },
        );
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px_8()
            .pb(px(52.0))
            .child(icon("icons/sparkle.svg", 20.0, theme.accent))
            .child(
                div()
                    .mt(px(14.0))
                    .flex()
                    .items_baseline()
                    .text_size(sp(20.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .when(projectless_selected, |element| {
                        element.child(tr_cow!("onboarding.what_should_we_build"))
                    })
                    .when(!projectless_selected, |element| {
                        element
                            .child(tr_cow!("onboarding.what_should_we_build_in"))
                            .child(project_selector)
                            .child(tr_cow!("onboarding.question_mark"))
                    }),
            )
    }
}

fn localized_session_title(session: &AgentSession) -> String {
    let title = session.display_title();
    if title == AgentSession::DEFAULT_TITLE {
        tr!("session.new_task")
    } else {
        title.to_owned()
    }
}

fn sidebar_session_selected(
    selected_session: Option<Uuid>,
    pending_session: Option<Uuid>,
    session_id: Uuid,
) -> bool {
    pending_session.map_or(selected_session == Some(session_id), |pending| {
        pending == session_id
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every view keeps a name and a glyph of its own: the switch is the only
    /// place a reader learns which view they are in, and the three have to stay
    /// tellable apart at a glance.
    #[test]
    fn every_view_has_its_own_label_and_glyph() {
        let views = [
            SidebarGrouping::Project,
            SidebarGrouping::Updated,
            SidebarGrouping::Status,
        ];
        let labels = views.map(|view| sidebar_grouping_label(view));
        let glyphs = views.map(|view| sidebar_grouping_glyph(view));
        assert!(
            labels.iter().all(|label| !label.trim().is_empty()),
            "a view without a name cannot be offered: {labels:?}"
        );
        assert!(
            glyphs.iter().all(|glyph| glyph.starts_with("icons/")),
            "each view needs an icon: {glyphs:?}"
        );
        for (index, glyph) in glyphs.iter().enumerate() {
            assert!(
                !glyphs[..index].contains(glyph),
                "two views cannot share one glyph: {glyphs:?}"
            );
        }
    }

    #[test]
    fn groups_sessions_by_calendar_period() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let cases = [
            ((2026, 8, 12), SessionDateGroup::Today),
            ((2026, 8, 11), SessionDateGroup::Yesterday),
            ((2026, 8, 10), SessionDateGroup::ThisWeek),
            ((2026, 8, 1), SessionDateGroup::ThisMonth),
            ((2026, 1, 1), SessionDateGroup::ThisYear),
            ((2025, 12, 31), SessionDateGroup::More),
        ];

        for ((year, month, day), expected) in cases {
            let session_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
            assert_eq!(session_date_group_for_dates(session_date, today), expected);
        }
    }

    #[test]
    fn future_sessions_stay_in_today() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let tomorrow = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        assert_eq!(
            session_date_group_for_dates(tomorrow, today),
            SessionDateGroup::Today
        );
    }

    #[test]
    fn collapsed_sidebar_group_keeps_only_its_header_and_spacer() {
        let sessions = [Uuid::from_u128(1), Uuid::from_u128(2)];
        let group = SidebarGroup::Updated(SessionDateGroup::Today);
        let mut expanded = Vec::new();
        append_sidebar_group_rows(&mut expanded, group, &sessions, false, false);
        assert_eq!(
            expanded,
            vec![
                SidebarRow::Header(group),
                SidebarRow::Session(sessions[0]),
                SidebarRow::Session(sessions[1]),
                SidebarRow::GroupSpacer,
            ]
        );

        let mut collapsed = Vec::new();
        append_sidebar_group_rows(&mut collapsed, group, &sessions, true, false);
        assert_eq!(
            collapsed,
            vec![SidebarRow::Header(group), SidebarRow::GroupSpacer,]
        );
    }

    #[test]
    fn hidden_project_sessions_keep_a_keyboard_reveal_row() {
        let group = SidebarGroup::Project(Uuid::from_u128(1));
        let mut expanded = Vec::new();
        append_sidebar_group_rows(&mut expanded, group, &[], false, true);
        assert_eq!(
            expanded,
            vec![
                SidebarRow::Header(group),
                SidebarRow::ShowMore(group),
                SidebarRow::GroupSpacer,
            ]
        );

        let mut collapsed = Vec::new();
        append_sidebar_group_rows(&mut collapsed, group, &[], true, true);
        assert_eq!(
            collapsed,
            vec![SidebarRow::Header(group), SidebarRow::GroupSpacer]
        );
    }

    #[test]
    fn project_sessions_reveal_older_history_in_thirty_item_batches() {
        let sessions = (1..=36).map(Uuid::from_u128).collect::<Vec<_>>();
        let recent_cutoff = 100;
        let timestamps = sessions
            .iter()
            .enumerate()
            .map(|(index, session_id)| {
                (
                    *session_id,
                    if index == 0 {
                        recent_cutoff
                    } else {
                        recent_cutoff - 1
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        let (initial, show_more) =
            visible_project_sessions(&sessions, &timestamps, recent_cutoff, 0);
        assert_eq!(initial, vec![sessions[0]]);
        assert!(show_more);

        let (first_batch, show_more) = visible_project_sessions(
            &sessions,
            &timestamps,
            recent_cutoff,
            SIDEBAR_PROJECT_REVEAL_BATCH,
        );
        assert_eq!(first_batch, sessions[..31]);
        assert!(show_more);

        let (all_sessions, show_more) = visible_project_sessions(
            &sessions,
            &timestamps,
            recent_cutoff,
            SIDEBAR_PROJECT_REVEAL_BATCH * 2,
        );
        assert_eq!(all_sessions, sessions);
        assert!(!show_more);
    }

    #[test]
    fn sidebar_recency_uses_last_reply_with_creation_fallback() {
        let project_id = Uuid::new_v4();
        let mut renamed_old_session = AgentSession::new(project_id, ProviderKind::Codex);
        renamed_old_session.created_at = 10;
        renamed_old_session.last_reply_at = Some(20);
        renamed_old_session.updated_at = 1_000;

        let mut newer_unanswered_session = AgentSession::new(project_id, ProviderKind::Codex);
        newer_unanswered_session.created_at = 30;
        newer_unanswered_session.last_reply_at = None;
        newer_unanswered_session.updated_at = 30;

        assert_eq!(sidebar_session_timestamp(&renamed_old_session), 20);
        assert_eq!(sidebar_session_timestamp(&newer_unanswered_session), 30);

        let mut sessions = vec![&renamed_old_session, &newer_unanswered_session];
        sort_sidebar_sessions(&mut sessions, SidebarOrdering::Newest);
        assert_eq!(sessions[0].id, newer_unanswered_session.id);

        sort_sidebar_sessions(&mut sessions, SidebarOrdering::Oldest);
        assert_eq!(sessions[0].id, renamed_old_session.id);
    }

    #[test]
    fn project_grouping_preserves_global_group_and_session_order() {
        let first_project = Uuid::from_u128(1);
        let second_project = Uuid::from_u128(2);
        let first = AgentSession::new(first_project, ProviderKind::Codex);
        let second = AgentSession::new(second_project, ProviderKind::Codex);
        let third = AgentSession::new(first_project, ProviderKind::Codex);

        let groups = project_sidebar_groups(&[&second, &first, &third], &HashSet::new());

        assert_eq!(
            groups,
            vec![
                (SidebarGroup::Project(second_project), vec![second.id]),
                (
                    SidebarGroup::Project(first_project),
                    vec![first.id, third.id]
                ),
            ]
        );
    }

    #[test]
    fn projectless_sessions_share_one_trailing_group() {
        let ordinary_project = Uuid::from_u128(1);
        let first_projectless_project = Uuid::from_u128(2);
        let second_projectless_project = Uuid::from_u128(3);
        let first_projectless = AgentSession::new(first_projectless_project, ProviderKind::Codex);
        let ordinary = AgentSession::new(ordinary_project, ProviderKind::Codex);
        let second_projectless = AgentSession::new(second_projectless_project, ProviderKind::Codex);

        let groups = project_sidebar_groups(
            &[&first_projectless, &ordinary, &second_projectless],
            &HashSet::from([first_projectless_project, second_projectless_project]),
        );

        assert_eq!(
            groups,
            vec![
                (SidebarGroup::Project(ordinary_project), vec![ordinary.id]),
                (
                    SidebarGroup::Projectless,
                    vec![first_projectless.id, second_projectless.id]
                ),
            ]
        );
    }

    #[test]
    fn projectless_sidebar_projects_are_paths_under_the_workspace_root() {
        let root = Path::new("/tmp/.waku/projects");
        let projectless = Project {
            id: Uuid::from_u128(1),
            name: "Task".to_owned(),
            path: root.join("2026-08-23/task"),
            created_at: 0,
        };
        let ordinary = Project {
            id: Uuid::from_u128(2),
            name: "Ordinary".to_owned(),
            path: PathBuf::from("/tmp/dev/ordinary"),
            created_at: 0,
        };

        assert!(sidebar_project_is_projectless(&projectless, Some(root)));
        assert!(!sidebar_project_is_projectless(&ordinary, Some(root)));
        assert!(!sidebar_project_is_projectless(&projectless, None));
    }

    #[test]
    fn persisted_worktree_branches_supply_sidebar_labels() {
        let local = SessionWorkspace::Local;
        let planned = SessionWorkspace::NewWorktree {
            base_branch: Some("develop".to_owned()),
        };
        let worktree = SessionWorkspace::Worktree {
            path: PathBuf::from("/tmp/worktree"),
            branch: "feature/sidebar".to_owned(),
        };

        assert_eq!(persisted_sidebar_branch_label(&local), None);
        assert_eq!(persisted_sidebar_branch_label(&planned), Some("develop"));
        assert_eq!(
            persisted_sidebar_branch_label(&worktree),
            Some("feature/sidebar")
        );
    }

    #[test]
    fn pending_session_replaces_sidebar_selection_immediately() {
        let current = Uuid::from_u128(1);
        let pending = Uuid::from_u128(2);

        assert!(!sidebar_session_selected(
            Some(current),
            Some(pending),
            current
        ));
        assert!(sidebar_session_selected(
            Some(current),
            Some(pending),
            pending
        ));
        assert!(sidebar_session_selected(Some(current), None, current));
    }

    #[test]
    fn selected_session_uses_nearest_bottom_edge_for_an_unmeasured_lower_row() {
        let target = Uuid::from_u128(31);
        let group = SidebarGroup::Updated(SessionDateGroup::Today);
        let mut rows = vec![SidebarRow::Search, SidebarRow::Header(group)];
        rows.extend((1..=40).map(|id| SidebarRow::Session(Uuid::from_u128(id))));
        rows.push(SidebarRow::GroupSpacer);

        let index = sidebar_session_row_index(&rows, target).unwrap();
        let offset = sidebar_bottom_aligned_offset(&rows, index, px(400.0));

        assert_eq!(index, 32);
        assert_eq!(offset.item_ix, 25);
        assert_eq!(offset.offset_in_item, px(16.0));
        let visible_height = rows[offset.item_ix..=index]
            .iter()
            .copied()
            .map(sidebar_row_height)
            .fold(Pixels::ZERO, |height, row| height + row)
            - offset.offset_in_item;
        assert_eq!(visible_height, px(400.0));
        assert_eq!(sidebar_session_row_index(&rows, Uuid::from_u128(41)), None);
    }

    fn status_test_session(status: SessionStatus) -> AgentSession {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.set_status(status);
        session
    }

    /// The fingerprint `sidebar_rows_cached` folds over the session list, so a
    /// test can ask whether a change would rebuild the row snapshot at all.
    fn test_snapshot_fingerprint(sessions: &[&AgentSession]) -> u64 {
        sessions.iter().fold(0u64, |fingerprint, session| {
            mix_sidebar_session_facts(fingerprint, session)
        })
    }

    fn status_section_ids(sessions: &[&AgentSession], section: SidebarStatusSection) -> Vec<Uuid> {
        status_sidebar_sections(sessions)[section.index()].clone()
    }

    #[test]
    fn status_view_lists_each_task_in_the_section_its_status_implies() {
        let waiting = status_test_session(SessionStatus::Waiting);
        let failed = status_test_session(SessionStatus::Failed);
        let connecting = status_test_session(SessionStatus::Connecting);
        let working = status_test_session(SessionStatus::Working);
        let background = status_test_session(SessionStatus::Background);
        let idle = status_test_session(SessionStatus::Idle);

        let sessions = [
            &idle as &AgentSession,
            &background,
            &failed,
            &working,
            &connecting,
            &waiting,
        ];
        let sections = status_sidebar_sections(&sessions);

        let needs_you = sections[SidebarStatusSection::NeedsYou.index()].clone();
        let running = sections[SidebarStatusSection::Running.index()].clone();
        let recent = sections[SidebarStatusSection::Recent.index()].clone();
        assert_eq!(needs_you.len(), 2);
        assert!(needs_you.contains(&waiting.id));
        assert!(needs_you.contains(&failed.id));
        assert_eq!(running.len(), 3);
        assert!(running.contains(&connecting.id));
        assert!(running.contains(&working.id));
        assert!(running.contains(&background.id));
        assert_eq!(recent, vec![idle.id]);
        assert_eq!(sections.concat().len(), sessions.len());
    }

    #[test]
    fn an_archived_task_belongs_to_no_active_status_section() {
        let mut archived_waiting = status_test_session(SessionStatus::Waiting);
        archived_waiting.archived_at = Some(1_700_000_000);

        assert!(
            status_section_ids(&[&archived_waiting], SidebarStatusSection::NeedsYou).is_empty()
        );
        assert!(
            status_sidebar_sections(&[&archived_waiting])
                .iter()
                .all(Vec::is_empty)
        );

        archived_waiting.archived_at = None;
        assert_eq!(
            status_section_ids(&[&archived_waiting], SidebarStatusSection::NeedsYou),
            vec![archived_waiting.id]
        );
    }

    #[test]
    fn the_longest_blockage_leads_the_needs_you_section() {
        let now = 1_700_000_000u64;
        let mut waiting_four_minutes = status_test_session(SessionStatus::Waiting);
        waiting_four_minutes.blocked_since = Some(now - 4 * 60);
        let mut waiting_forty_minutes = status_test_session(SessionStatus::Waiting);
        waiting_forty_minutes.blocked_since = Some(now - 40 * 60);

        assert_eq!(
            status_section_ids(
                &[&waiting_four_minutes, &waiting_forty_minutes],
                SidebarStatusSection::NeedsYou
            ),
            vec![waiting_forty_minutes.id, waiting_four_minutes.id]
        );
    }

    #[test]
    fn a_blockage_recorded_before_the_stamp_existed_orders_by_its_newest_activity() {
        let mut silent = status_test_session(SessionStatus::Failed);
        silent.blocked_since = None;
        silent.created_at = 100;
        let mut stamped = status_test_session(SessionStatus::Failed);
        stamped.blocked_since = Some(500);
        let mut unstamped = status_test_session(SessionStatus::Failed);
        unstamped.blocked_since = None;
        unstamped.last_reply_at = Some(900);

        assert_eq!(
            status_section_ids(
                &[&unstamped, &stamped, &silent],
                SidebarStatusSection::NeedsYou
            ),
            vec![silent.id, stamped.id, unstamped.id]
        );
    }

    #[test]
    fn running_and_recent_sections_lead_with_the_newest_reply() {
        let mut working_older = status_test_session(SessionStatus::Working);
        working_older.last_reply_at = Some(10);
        let mut working_newer = status_test_session(SessionStatus::Working);
        working_newer.last_reply_at = Some(20);
        let mut idle_older = status_test_session(SessionStatus::Idle);
        idle_older.last_reply_at = Some(30);
        let mut idle_newer = status_test_session(SessionStatus::Idle);
        idle_newer.last_reply_at = Some(40);

        assert_eq!(
            status_section_ids(
                &[&working_older, &working_newer],
                SidebarStatusSection::Running
            ),
            vec![working_newer.id, working_older.id]
        );
        assert_eq!(
            status_section_ids(&[&idle_older, &idle_newer], SidebarStatusSection::Recent),
            vec![idle_newer.id, idle_older.id]
        );
    }

    #[test]
    fn status_section_order_comes_from_the_record_not_the_collection_order() {
        let mut first = status_test_session(SessionStatus::Waiting);
        first.blocked_since = Some(1_000);
        first.last_reply_at = Some(1_000);
        let mut second = status_test_session(SessionStatus::Failed);
        second.blocked_since = Some(2_000);
        second.last_reply_at = Some(2_000);
        let mut third = status_test_session(SessionStatus::Idle);
        third.last_reply_at = Some(3_000);

        let collected = status_sidebar_sections(&[&first, &second, &third]);
        let reversed = status_sidebar_sections(&[&third, &second, &first]);

        assert_eq!(collected, reversed, "a restart keeps the order it stored");
    }

    #[test]
    fn flipping_a_task_to_running_re_sections_the_row_snapshot() {
        let mut task = status_test_session(SessionStatus::Idle);
        task.last_reply_at = Some(50);

        assert_eq!(
            status_section_ids(&[&task], SidebarStatusSection::Recent),
            vec![task.id]
        );
        let before = test_snapshot_fingerprint(&[&task]);

        task.set_status(SessionStatus::Working);

        assert_eq!(
            status_section_ids(&[&task], SidebarStatusSection::Running),
            vec![task.id]
        );
        assert_ne!(
            test_snapshot_fingerprint(&[&task]),
            before,
            "the snapshot has to rebuild or the row keeps its old section"
        );
    }

    #[test]
    fn archiving_a_task_moves_the_row_snapshot() {
        let mut task = status_test_session(SessionStatus::Idle);
        let before = test_snapshot_fingerprint(&[&task]);

        task.archived_at = Some(1_700_000_000);

        assert_ne!(test_snapshot_fingerprint(&[&task]), before);
    }

    /// The archived-visibility inputs `sidebar_rows_cached` folds over and
    /// above the session list: turning the reveal preference on, or the open
    /// task becoming one of the archived ones, has to re-section the rows.
    #[test]
    fn revealing_archived_tasks_re_sections_the_row_snapshot() {
        let folded = |show_archived, active_task_archived| {
            mix_sidebar_archived_visibility(0x5eed, show_archived, active_task_archived)
        };

        assert_ne!(
            folded(false, false),
            folded(true, false),
            "the reveal toggle has to rebuild the snapshot"
        );
        assert_ne!(
            folded(false, false),
            folded(false, true),
            "the open task being archived has to rebuild it too"
        );
    }

    #[test]
    fn an_archived_task_is_lifted_out_of_the_sections_its_status_implies() {
        let mut archived_waiting = status_test_session(SessionStatus::Waiting);
        archived_waiting.archived_at = Some(1_700_000_000);
        let running = status_test_session(SessionStatus::Working);
        let idle = status_test_session(SessionStatus::Idle);

        let (listed, archived, stands) = split_archived_sessions(
            vec![&archived_waiting as &AgentSession, &running, &idle],
            false,
            false,
        );

        assert_eq!(
            listed.iter().map(|session| session.id).collect::<Vec<_>>(),
            vec![running.id, idle.id],
            "an archived task joins no active section, whatever its status"
        );
        assert_eq!(archived, vec![archived_waiting.id]);
        assert!(
            !stands,
            "what was put away stays hidden until the user asks for it"
        );
    }

    #[test]
    fn the_archived_section_stands_when_the_user_asks_or_the_open_task_is_in_it() {
        let mut archived = status_test_session(SessionStatus::Idle);
        archived.archived_at = Some(1_700_000_000);

        let (_, _, hidden) = split_archived_sessions(vec![&archived], false, false);
        assert!(!hidden, "the default is not to reveal what was put away");

        let (_, _, revealed) = split_archived_sessions(vec![&archived], true, false);
        assert!(revealed, "the toggle reveals the trailing archived section");

        let (_, _, open_task) = split_archived_sessions(vec![&archived], false, true);
        assert!(
            open_task,
            "the open task is never a gap, so its section stands whatever the toggle says"
        );
    }

    /// The action says what it does and no more. "Put away" is the whole
    /// promise: nothing offered here may read as having freed disk space,
    /// because archiving frees nothing.
    #[test]
    fn the_archive_action_promises_no_reclaimed_space() {
        let wordings = [
            tr!("sidebar.put_away"),
            tr!("sidebar.bring_back"),
            tr!("sidebar.show_archived"),
            tr!("sidebar.section_archived"),
        ];
        for wording in &wordings {
            let wording = wording.to_lowercase();
            for forbidden in ["free", "space", "disk", "storage", "reclaim", "clean"] {
                assert!(
                    !wording.contains(forbidden),
                    "`{wording}` claims something archiving does not do"
                );
            }
        }
        assert!(
            tr!("sidebar.put_away").to_lowercase().contains("away"),
            "the action is described as putting the task away"
        );
    }

    /// The same task as JSON with the archive stamp taken out, so everything
    /// else about it can be compared across an archive action.
    fn session_without_archive_stamp(session: &AgentSession) -> serde_json::Value {
        let mut value = serde_json::to_value(session).expect("a session serializes");
        value
            .as_object_mut()
            .expect("a session is an object")
            .remove("archived_at");
        value
    }

    /// Archiving records that a task was put away and changes nothing else.
    /// The messages, the stored transcript detail, the checkpoints' refs and
    /// the worktree all survive it — putting a task away frees nothing.
    #[test]
    fn putting_a_task_away_leaves_everything_else_about_it_alone() {
        let mut task = task_with_a_live_plan_step("Move the row's second line");
        task.set_title("Fix the sidebar row");
        task.objective = Some("The sidebar row says where the task stands".to_owned());
        task.turn_count = Some(4);
        task.changed_files = Some(3);
        task.workspace = SessionWorkspace::Worktree {
            path: PathBuf::from("/tmp/worktree"),
            branch: "waku/fix-the-sidebar-row".to_owned(),
        };
        task.finish_active_turn(TurnStatus::Completed);
        task.turns
            .last_mut()
            .expect("the test has a turn")
            .checkpoint = Some(Checkpoint {
            turn_count: 1,
            git_ref: "refs/waku/test-turn-1".to_owned(),
            status: CheckpointStatus::Ready,
            files: vec![crate::model::CheckpointFile {
                path: "src/app/sidebar.rs".to_owned(),
                additions: 12,
                deletions: 3,
            }],
            additions: 12,
            deletions: 3,
            created_at: 1,
        });
        let before = session_without_archive_stamp(&task);
        assert!(!task.messages.is_empty(), "the task has messages to keep");
        assert!(
            !task.transcript_blocks.is_empty(),
            "the task has stored transcript detail to keep"
        );
        assert!(task.turns.iter().any(|turn| turn.checkpoint.is_some()));

        assert!(apply_archive_action(&mut task, true, 1_700_000_000));

        assert_eq!(task.archived_at, Some(1_700_000_000));
        assert_eq!(
            session_without_archive_stamp(&task),
            before,
            "putting a task away deletes nothing"
        );

        // Bringing it back is the same field going away again, and it, too,
        // leaves the rest of the task exactly where it was.
        assert!(apply_archive_action(&mut task, false, 1_800_000_000));
        assert_eq!(task.archived_at, None);
        assert_eq!(session_without_archive_stamp(&task), before);

        // Asking to put an already-parked task away again moves nothing.
        assert!(apply_archive_action(&mut task, true, 1_900_000_000));
        assert!(!apply_archive_action(&mut task, true, 2_000_000_000));
    }

    #[test]
    fn a_rename_does_not_move_the_row_but_a_submitted_turn_does() {
        let mut task = status_test_session(SessionStatus::Idle);
        task.last_reply_at = Some(50);
        let before = test_snapshot_fingerprint(&[&task]);

        task.set_title("a name the user typed");
        task.updated_at = 9_999;

        assert_eq!(
            test_snapshot_fingerprint(&[&task]),
            before,
            "a metadata edit does not move a row"
        );

        task.last_reply_at = Some(60);

        assert_ne!(
            test_snapshot_fingerprint(&[&task]),
            before,
            "a submitted turn does"
        );
    }

    /// A busy task whose provider reported a plan step for the live turn.
    fn task_with_a_live_plan_step(step: &str) -> AgentSession {
        let mut task = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        task.set_status(SessionStatus::Working);
        task.begin_turn("do the work");
        push_transcript_activity(
            &mut task,
            ActivityItem::new(None, ActivityKind::Plan, step, None, false),
            false,
        );
        task
    }

    /// A busy task with no provider plan step of its own.
    fn task_without_a_live_plan_step() -> AgentSession {
        let mut task = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        task.set_status(SessionStatus::Working);
        task.begin_turn("do the work");
        task
    }

    #[test]
    fn a_busy_task_row_shows_its_live_plan_step_instead_of_its_objective() {
        let mut task = task_with_a_live_plan_step("Move the row's second line");
        task.objective = Some("The sidebar row says where the task stands".to_owned());

        let facts = sidebar_session_facts(&task);

        assert_eq!(
            sidebar_row_detail(&task, &facts),
            Some(SidebarRowDetail::PlanStep(
                "Move the row's second line".into()
            ))
        );
    }

    #[test]
    fn a_busy_task_row_without_a_plan_step_shows_its_objective() {
        let mut task = task_without_a_live_plan_step();
        task.objective = Some("The sidebar row says where the task stands".to_owned());

        let facts = sidebar_session_facts(&task);

        assert_eq!(
            sidebar_row_detail(&task, &facts),
            Some(SidebarRowDetail::Objective(
                "The sidebar row says where the task stands".into()
            ))
        );
    }

    #[test]
    fn a_blocked_task_row_shows_the_reason_it_recorded() {
        let mut task = task_with_a_live_plan_step("Move the row's second line");
        task.objective = Some("The sidebar row says where the task stands".to_owned());
        task.set_status(SessionStatus::Waiting);
        task.set_blocked_reason("Approve the network request");

        let facts = sidebar_session_facts(&task);

        assert_eq!(
            sidebar_row_detail(&task, &facts),
            Some(SidebarRowDetail::BlockedReason(
                "Approve the network request".into()
            ))
        );
    }

    #[test]
    fn a_blocked_task_row_without_a_reason_invents_nothing() {
        let mut task = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        task.set_status(SessionStatus::Failed);

        let facts = sidebar_session_facts(&task);

        assert_eq!(sidebar_row_detail(&task, &facts), None);

        // A task that is waiting with no recorded reason is still busy, so the
        // row reports the turn it is in the middle of instead of a reason
        // nobody recorded.
        let mut waiting = task_with_a_live_plan_step("Move the row's second line");
        waiting.set_status(SessionStatus::Waiting);

        let facts = sidebar_session_facts(&waiting);

        assert_eq!(
            sidebar_row_detail(&waiting, &facts),
            Some(SidebarRowDetail::PlanStep(
                "Move the row's second line".into()
            ))
        );
    }

    #[test]
    fn an_idle_task_row_keeps_its_content_and_borrows_no_objective() {
        let mut task = task_with_a_live_plan_step("Move the row's second line");
        task.objective = Some("The sidebar row says where the task stands".to_owned());
        task.finish_active_turn(TurnStatus::Completed);
        task.set_status(SessionStatus::Idle);

        let facts = sidebar_session_facts(&task);

        assert_eq!(
            facts.step, None,
            "a settled turn's plan step is not the current step"
        );
        assert_eq!(sidebar_row_detail(&task, &facts), None);
    }

    #[test]
    fn the_row_facts_cache_follows_the_session_it_is_refreshed_from() {
        let mut task = task_with_a_live_plan_step("Move the row's second line");
        task.objective = Some("The sidebar row says where the task stands".to_owned());
        task.turn_count = Some(4);
        task.changed_files = Some(3);
        let mut facts = HashMap::new();

        rebuild_sidebar_session_facts(&mut facts, std::slice::from_ref(&task));

        assert_eq!(
            facts.get(&task.id),
            Some(&SidebarSessionFacts {
                objective: Some("The sidebar row says where the task stands".into()),
                blocked_reason: None,
                step: Some("Move the row's second line".into()),
                turn_count: Some(4),
                changed_files: Some(3),
            })
        );

        // The live turn reports a new step: the row has to follow it without
        // ever walking the transcript itself.
        push_transcript_activity(
            &mut task,
            ActivityItem::new(None, ActivityKind::Plan, "Refresh the cache", None, true),
            true,
        );
        rebuild_sidebar_session_facts(&mut facts, std::slice::from_ref(&task));
        assert_eq!(
            facts.get(&task.id).and_then(|facts| facts.step.clone()),
            Some("Refresh the cache".into())
        );

        // A task that is gone leaves no entry behind.
        rebuild_sidebar_session_facts(&mut facts, &[]);
        assert!(facts.is_empty());
    }

    fn card_test_project(id: Uuid, name: &str) -> Project {
        Project {
            id,
            name: name.to_owned(),
            path: PathBuf::from(format!("/tmp/dev/{name}")),
            created_at: 0,
        }
    }

    #[test]
    fn a_task_card_carries_everything_the_client_holds() {
        let mut task = task_with_a_live_plan_step("Move the row's second line");
        task.set_title("Fix the sidebar row");
        task.objective = Some("The sidebar row says where the task stands".to_owned());
        task.workspace = SessionWorkspace::Worktree {
            path: PathBuf::from("/tmp/worktree"),
            branch: "waku/fix-the-sidebar-row".to_owned(),
        };
        task.turn_count = Some(4);
        task.changed_files = Some(3);
        task.last_reply_at = Some(1_000);
        let project = card_test_project(task.project_id, "doki");

        let facts = sidebar_session_facts(&task);
        let card = sidebar_task_card(&task, &facts, Some(&project), 1_300);

        assert_eq!(card.title, SharedString::from("Fix the sidebar row"));
        assert_eq!(
            card.objective,
            Some(SharedString::from(
                "The sidebar row says where the task stands"
            ))
        );
        assert_eq!(
            card.state,
            Some(SidebarRowDetail::PlanStep(
                "Move the row's second line".into()
            ))
        );
        assert_eq!(card.project, SharedString::from("doki"));
        assert_eq!(
            card.branch,
            Some(SharedString::from("waku/fix-the-sidebar-row"))
        );
        assert_eq!(card.turns, Some(4));
        assert_eq!(card.changed_files, Some(3));
        assert_eq!(card.recency, SharedString::from("5m"));
        assert_eq!(
            sidebar_card_facts_line(card.turns, card.changed_files, &card.recency),
            SharedString::from("4 turns · 3 files · 5m")
        );
    }

    /// The card is not the row: a busy task whose row shows its step still has
    /// an objective to answer "what is this" with, and one whose row falls back
    /// to the objective does not say the same sentence twice.
    #[test]
    fn a_busy_task_card_keeps_the_objective_and_the_step_apart() {
        let mut stepped = task_with_a_live_plan_step("Refresh the cache");
        stepped.objective = Some("The sidebar row says where the task stands".to_owned());
        let facts = sidebar_session_facts(&stepped);
        let card = sidebar_task_card(&stepped, &facts, None, 0);

        assert_eq!(
            card.state,
            Some(SidebarRowDetail::PlanStep("Refresh the cache".into()))
        );
        assert_eq!(
            card.objective,
            Some(SharedString::from(
                "The sidebar row says where the task stands"
            ))
        );

        let mut unstepped = task_without_a_live_plan_step();
        unstepped.objective = Some("The sidebar row says where the task stands".to_owned());
        let facts = sidebar_session_facts(&unstepped);
        let card = sidebar_task_card(&unstepped, &facts, None, 0);

        assert_eq!(card.state, None);
        assert_eq!(
            card.objective,
            Some(SharedString::from(
                "The sidebar row says where the task stands"
            ))
        );
    }

    #[test]
    fn a_blocked_task_card_says_why_it_is_blocked() {
        let mut task = task_with_a_live_plan_step("Refresh the cache");
        task.set_status(SessionStatus::Waiting);
        task.set_blocked_reason("Approve the network request");

        let facts = sidebar_session_facts(&task);
        let card = sidebar_task_card(&task, &facts, None, 0);

        assert_eq!(
            card.state,
            Some(SidebarRowDetail::BlockedReason(
                "Approve the network request".into()
            ))
        );
    }

    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    /// Stands in for a sidebar row: a control before it to Tab from, the row's
    /// card wired the way the real row wires it, and the row's context menu.
    struct CardFocusHarness {
        before_focus: FocusHandle,
        row_focus: FocusHandle,
        card: Entity<SidebarTaskCardState>,
        menu: ContextMenuHandle,
    }

    impl Render for CardFocusHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let handle = self.card.read(cx).handle.clone();
            div()
                .size_full()
                .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
                .child(
                    div()
                        .id("before")
                        .track_focus(&self.before_focus)
                        .w(px(120.0))
                        .h(px(16.0)),
                )
                .child(context_menu(
                    with_sidebar_task_card(
                        div()
                            .id("row")
                            .track_focus(&self.row_focus)
                            .tab_index(0)
                            .w(px(120.0))
                            .h(px(32.0)),
                        sidebar_task_card(
                            &AgentSession::new(Uuid::new_v4(), ProviderKind::Claude),
                            &SidebarSessionFacts::default(),
                            None,
                            0,
                        ),
                        &handle,
                    ),
                    "card-focus-row-menu",
                    &self.menu,
                    |_| vec![MenuItem::new("Entry", |_, _| {})],
                ))
        }
    }

    struct CardFocus {
        before: FocusHandle,
        row: FocusHandle,
        card: ContextMenuHandle,
        menu: ContextMenuHandle,
    }

    /// Builds the harness, hands its pieces to `body`, and gives the row
    /// keyboard focus by tabbing from the control before it — the route the
    /// card exists for, and the only one GPUI counts as keyboard input.
    fn with_a_focused_row_card(
        cx: &mut TestAppContext,
        body: impl FnOnce(&mut VisualTestContext, CardFocus),
    ) {
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new("tab", FocusNext, None)]);
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            let menu = ContextMenuHandle::new(cx);
            let row_focus = sidebar_row_focus(&menu);
            let card = cx.new(|cx| SidebarTaskCardState::new(row_focus.clone(), window, cx));
            CardFocusHarness {
                before_focus: cx.focus_handle().tab_stop(true).tab_index(0),
                row_focus,
                card,
                menu,
            }
        });
        let focus = view.read_with(cx, |harness, cx| CardFocus {
            before: harness.before_focus.clone(),
            row: harness.row_focus.clone(),
            card: harness.card.read(cx).handle.clone(),
            menu: harness.menu.clone(),
        });
        // GPUI hands focus listeners their before/after paths only while the
        // window is active, and a test window starts inactive.
        cx.update(|window, _| window.activate_window());
        cx.update(|window, cx| window.focus(&focus.before, cx));
        cx.run_until_parked();

        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        assert!(
            cx.update(|window, _| focus.row.is_focused(window)),
            "the row is a tab stop"
        );
        assert!(
            focus.card.is_open(),
            "keyboard focus stands the row's card up"
        );

        body(cx, focus);
    }

    #[gpui::test]
    fn keyboard_focus_reveals_a_rows_card(cx: &mut TestAppContext) {
        with_a_focused_row_card(cx, |cx, focus| {
            // Moving focus away is what hides it again.
            cx.update(|window, cx| window.focus(&focus.before, cx));
            cx.run_until_parked();
            assert!(!focus.card.is_open(), "leaving the row takes its card down");

            // The pointer focuses a row by clicking it, and has the tooltip
            // for it: a click leaves no card pinned over the list.
            cx.update(|window, cx| window.focus(&focus.row, cx));
            cx.simulate_keystrokes("down");
            cx.run_until_parked();
            assert!(focus.card.is_open(), "keyboard focus still stands it up");
            cx.simulate_mouse_down(
                point(px(10.0), px(10.0)),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();
            assert!(
                !focus.card.is_open(),
                "a click closes the card rather than pinning one of its own"
            );
        });
    }

    /// The card and the row's context menu never stand together: opening the
    /// menu takes focus, which is what puts the card down.
    #[gpui::test]
    fn opening_a_rows_context_menu_puts_its_card_down(cx: &mut TestAppContext) {
        with_a_focused_row_card(cx, |cx, focus| {
            cx.simulate_mouse_down(
                point(px(10.0), px(20.0)),
                MouseButton::Right,
                Modifiers::none(),
            );
            cx.run_until_parked();
            assert!(focus.menu.is_open(), "the right-click opened the menu");

            // An open menu takes focus two frames later; a test window never
            // runs the frame loop that waits, so the test hands it the focus
            // the menu would take.
            let menu_focus = focus.menu.focus_handle().clone();
            cx.update(|window, cx| window.focus(&menu_focus, cx));
            cx.run_until_parked();
            assert!(
                !focus.card.is_open(),
                "the menu's focus took the card down, leaving neither half-open"
            );
        });
    }

    #[test]
    fn a_task_with_no_known_branch_names_its_project() {
        let mut task = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        task.workspace = SessionWorkspace::Worktree {
            path: PathBuf::from("/tmp/worktree"),
            branch: "waku/fix-the-sidebar-row".to_owned(),
        };
        let project = Project {
            id: task.project_id,
            name: "doki".to_owned(),
            path: PathBuf::from("/tmp/dev/doki"),
            created_at: 0,
        };

        assert_eq!(
            sidebar_row_identifier(true, &task, Some(&project)),
            (
                "icons/git-branch.svg",
                SharedString::from("waku/fix-the-sidebar-row")
            )
        );

        // The list projection zeroes the workspace, so after a restart the
        // client no longer knows the task's branch: the project's current
        // branch is not the task's own and must not stand in for it.
        task.workspace = SessionWorkspace::Local;
        assert_eq!(
            sidebar_row_identifier(true, &task, Some(&project)),
            ("icons/folder.svg", SharedString::from("doki"))
        );
        assert_eq!(
            sidebar_row_identifier(true, &task, None),
            ("icons/folder.svg", SharedString::from("Unknown project"))
        );

        // The status view has no project heading, so it always names it.
        assert_eq!(
            sidebar_row_identifier(false, &task, Some(&project)),
            ("icons/folder.svg", SharedString::from("doki"))
        );
    }

    /// The body of the `anchor` item in `source`, up to the next item at the
    /// same or a shallower indent, or the test module. Source guards share it
    /// so each one names what it holds and what it forbids, and nothing else.
    fn source_body<'a>(source: &'a str, anchor: &str) -> &'a str {
        let start = source
            .find(anchor)
            .unwrap_or_else(|| panic!("{anchor} must exist"));
        let body = &source[start + 1..];
        let end = [
            "\nfn ",
            "\npub(super) fn ",
            "\npub(crate) fn ",
            "\npub fn ",
            "\n    fn ",
            "\n    pub(super) fn ",
            "\n    pub(crate) fn ",
            "\n    pub fn ",
            "\n#[cfg(test)]",
        ]
        .into_iter()
        .filter_map(|terminator| body.find(terminator))
        .min()
        .unwrap_or(body.len());
        &body[..end]
    }

    /// Row builders run for every visible row on every frame, so they may not
    /// reach a transcript: the plan step is resolved into the facts cache where
    /// the session changes instead. This reads the source rather than the
    /// behavior because the cost of a regression is invisible until the
    /// sidebar is under a long, busy transcript.
    #[test]
    fn the_row_builders_never_reach_the_transcript() {
        let source = include_str!("sidebar.rs");
        for anchor in [
            "\n    fn sidebar_rows_cached(",
            "\n    fn sidebar_rows(",
            "\n    fn sidebar_row(",
            "\n    fn render_sidebar_session_item(",
            "\npub(super) fn sidebar_row_detail(",
            "\nfn sidebar_row_identifier(",
        ] {
            for forbidden in [".transcript_blocks", "live_plan_step("] {
                assert!(
                    !source_body(source, anchor).contains(forbidden),
                    "{anchor} must not call `{forbidden}`; resolve it into the row facts cache \
                     where the session changes"
                );
            }
        }
    }

    /// The card answers from values the client already holds, so neither route
    /// to it may reach for one: a card that could ask the daemon, a store or
    /// the filesystem would turn the pointer crossing a list into work. This
    /// reads the source because the cost of a regression only shows against a
    /// real daemon on a real machine, and hovering draws no such thing.
    #[test]
    fn the_row_card_asks_for_nothing() {
        let source = include_str!("sidebar.rs");
        for anchor in [
            "\nfn sidebar_task_card(",
            "\nfn sidebar_card_facts_line(",
            "\nfn sidebar_task_card_view(",
            "\nfn sidebar_card_line(",
            "\nfn with_sidebar_task_card(",
            "\nfn sidebar_task_card_handle(",
            "\n    fn new(row_focus: FocusHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {",
        ] {
            let body = source_body(source, anchor);
            // The card's own view may mention no request machinery at all —
            // including the view's body and the focus wiring that opens it,
            // which is where a "fetch it if it is missing" regression lands.
            let forbidden = [
                ".daemon",
                "background_executor(",
                "cx.spawn(",
                "store",
                "std::fs",
                "read_dir(",
                "Command::new",
                "PathBuf",
                "SessionOptions",
            ];
            for forbidden in forbidden {
                assert!(
                    !body.contains(forbidden),
                    "{anchor} must not mention `{forbidden}`; the card is drawn from values the \
                     client already holds"
                );
            }
        }
        // And its signature cannot name a handle it could fetch with, whatever
        // the body does.
        let signature = source_body(source, "\nfn sidebar_task_card(");
        let signature = &signature[..signature.find('{').expect("a function body")];
        for forbidden in ["Path", "Client", "Store", "Handle"] {
            assert!(
                !signature.contains(forbidden),
                "a `{forbidden}` in `{signature}` would let the card fetch what it draws"
            );
        }
    }

    #[test]
    fn status_sections_keep_their_own_collapse_identity() {
        let keys = SidebarStatusSection::ALL
            .iter()
            .map(|section| section.group().element_key().to_string())
            .chain([
                SidebarGroup::Updated(SessionDateGroup::Today)
                    .element_key()
                    .to_string(),
                SidebarGroup::Projectless.element_key().to_string(),
            ])
            .collect::<HashSet<_>>();

        assert_eq!(
            keys.len(),
            SidebarStatusSection::ALL.len() + 2,
            "a section shares no identity with another section or view"
        );
    }
}
