//! The generated objective: one sentence for what a settled task is for.
//!
//! A task's objective is Waku-owned (see [`crate::model`] and `docs/titles.md`).
//! This module is the whole mechanism around the one Waku generates: the shape
//! of the trigger the daemon sends into the task's own provider process, the
//! rules that decide whether a generated sentence may be stored, and the pacing
//! that keeps a long session from generating on every turn.
//!
//! The rules live here rather than in the Pi extension that produces the
//! sentence, so there is exactly one copy of them: the extension returns raw
//! text, the daemon parses it, and a provider that returns something hostile
//! leaves the task with the objective it already had.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::persistence::PersistedState;

/// The namespace Waku's own Pi commands share. The composer never offers one:
/// they are the daemon's internal surface, and the driver filters the
/// provider's reported list by this prefix before clients ever see it.
pub const COMMAND_NAMESPACE: &str = "waku:";

/// The namespaced surface Waku's bundled Pi extension exposes: the command Pi
/// dispatches, and the custom type the result rides back in. One surface, one
/// spelling — the prompt the daemon writes is this name.
pub const DIGEST_SURFACE: &str = "waku:digest";

/// The format version of a published result. Waku stores a result that carries
/// this version and ignores every other one, so an extension from a newer Waku
/// cannot be read into an older daemon's fields.
pub const ENTRY_VERSION: u64 = 1;

/// How long a settled task stays quiet before a generation starts. A newer
/// settle replaces the pending one, so a rapid back-and-forth generates once,
/// after the last turn, rather than once per turn.
pub const QUIET_PERIOD: Duration = Duration::from_secs(15);

/// How long one generation may take before the daemon stops expecting it.
/// A result that arrives later is stale and is not stored.
pub const GENERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// How many generations one task may get inside [`GENERATION_WINDOW`].
pub const MAX_GENERATIONS_PER_WINDOW: usize = 6;

/// The rolling window the generation cap is measured over.
pub const GENERATION_WINDOW: Duration = Duration::from_secs(3600);

/// The longest sentence Waku stores. A row shows an outcome, not a paragraph,
/// so a completion that returns more than this is a failed generation rather
/// than text to display.
const MAX_OBJECTIVE_CHARS: usize = 240;

/// The words a sentence can drop without losing its meaning. They are dropped
/// before two objectives are compared, so a rewording that only rearranges
/// them still reads as the same objective.
const MEANINGLESS_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "these", "those", "from", "into", "onto", "its",
    "their", "there", "then", "than", "them", "they", "itself", "not", "but", "are", "was", "were",
    "has", "have", "had", "will", "would", "can", "could", "should", "shall", "may", "might",
    "must", "when", "while", "after", "before", "your", "you", "our", "out", "all", "any", "now",
    "one", "two", "how", "why", "what", "which", "who", "whom", "does", "did", "doing", "been",
    "being", "also", "still", "just", "only", "more", "most", "less", "least", "very", "such",
];

/// The prompt that dispatches one generation.
///
/// It carries the dispatch the daemon expects the result back with. That is the
/// reason the trigger is a marked prompt rather than the bare command: a prompt
/// naming a dispatch is one only the daemon writes, which is what lets the
/// transport treat the answer as an internal exchange instead of a submission.
pub fn trigger_prompt(dispatch: Uuid) -> String {
    format!("/{DIGEST_SURFACE} {dispatch}")
}

/// The dispatch a trigger prompt carries, or `None` for any other prompt —
/// including the same command without a dispatch, which is what a person
/// typing it produces.
pub fn trigger_dispatch(prompt: &str) -> Option<Uuid> {
    let rest = prompt
        .strip_prefix('/')?
        .strip_prefix(DIGEST_SURFACE)?
        .strip_prefix(' ')?
        .trim();
    if rest.is_empty() || rest.split_whitespace().count() != 1 {
        return None;
    }
    Uuid::parse_str(rest).ok()
}

/// One generation's result, as the bundled extension publishes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestResult {
    /// The dispatch this result answers.
    pub dispatch: Uuid,
    /// The sentence the extension produced, raw: the parser owns its shape.
    pub objective: String,
}

/// Decodes a published result.
///
/// `None` is what the daemon stores nothing for — a format version this build
/// does not know, a payload that is not the shape that version defines, or one
/// that names no dispatch. A malformed result is not an error to report: it
/// leaves the task with the objective it already had.
pub fn parse_result(raw: &str) -> Option<DigestResult> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("v").and_then(Value::as_u64) != Some(ENTRY_VERSION) {
        return None;
    }
    let dispatch = Uuid::parse_str(value.get("dispatch")?.as_str()?).ok()?;
    let objective = value.get("objective")?.as_str()?.to_owned();
    Some(DigestResult {
        dispatch,
        objective,
    })
}

/// Why a generated objective was not stored.
///
/// Every one of these keeps the task's previous objective: an objective that
/// says the wrong thing is worse than one that still says the last true thing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectiveRejection {
    /// Nothing but whitespace, or packaging a one-line answer should not carry.
    Empty,
    /// Longer than [`MAX_OBJECTIVE_CHARS`]: not the sentence that was asked for.
    TooLong,
    /// Names a path or a file: implementation detail, not an outcome.
    Path,
    /// Names a file extension: the same thing, one step less obvious.
    Extension,
    /// Names a symbol (camelCase, PascalCase, or `snake_case`).
    Symbol,
    /// Restates the task's title, which is already on the row.
    Title,
    /// Says what the stored objective already says. Storage keeps the stored
    /// text, so a regeneration that decides nothing does not churn.
    Unchanged,
}

/// Accepts or rejects one generated objective for a task.
///
/// `stored` is the generated objective the task already carries and `title` the
/// name the task is recognized by. `Ok` carries the text to store; every `Err`
/// means the caller keeps what the task already has.
pub fn accept(
    candidate: &str,
    stored: Option<&str>,
    title: &str,
) -> Result<String, ObjectiveRejection> {
    let text = normalize(candidate).ok_or(ObjectiveRejection::Empty)?;
    if text.chars().count() > MAX_OBJECTIVE_CHARS {
        return Err(ObjectiveRejection::TooLong);
    }
    // A path separator anywhere is enough: an objective that names one file is
    // describing the work, not the outcome, and splitting a sentence on what a
    // separator separates cannot tell the two apart.
    if text.contains('/') || text.contains('\\') {
        return Err(ObjectiveRejection::Path);
    }
    for token in text.split_whitespace() {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.');
        if names_file_extension(token) {
            return Err(ObjectiveRejection::Extension);
        }
        if names_symbol(token) {
            return Err(ObjectiveRejection::Symbol);
        }
    }
    if same_meaning(&text, title) {
        return Err(ObjectiveRejection::Title);
    }
    if let Some(stored) = stored.filter(|stored| !stored.trim().is_empty())
        && same_meaning(&text, stored)
    {
        return Err(ObjectiveRejection::Unchanged);
    }
    Ok(text)
}

/// The text Waku stores out of one completion: the first line that carries
/// anything, with the packaging a model wraps a one-line answer in taken off.
/// Fence lines are skipped rather than stored, so a completion that answers
/// inside a code block still reads as the sentence it is.
fn normalize(candidate: &str) -> Option<String> {
    for line in candidate.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let mut text = line;
        for _ in 0..3 {
            text = text
                .trim_start_matches(['-', '*', '>', '#', '`'])
                .trim_start();
            for label in ["Objective:", "Outcome:", "Goal:", "Summary:"] {
                if let Some(rest) = strip_label(text, label) {
                    text = rest.trim_start();
                }
            }
            text = text.trim_matches(['"', '\'', '`', '*', ' ']);
        }
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    None
}

/// `text` without a leading `label`, matched however the model capitalized it.
fn strip_label<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let head = text.get(..label.len())?;
    head.eq_ignore_ascii_case(label)
        .then(|| &text[label.len()..])
}

/// Whether the word ends in a dot followed by letters: `sidebar.rs`, `app.ts`,
/// `docs.json`. A sentence's own final period carries nothing after it, so it
/// is not an extension.
fn names_file_extension(token: &str) -> bool {
    match token.rsplit_once('.') {
        Some((name, extension)) => {
            !name.is_empty()
                && (1..=8).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

/// Whether the word is written the way a symbol is: an internal underscore, or
/// a lower-case letter followed by an upper-case one.
fn names_symbol(token: &str) -> bool {
    if token.len() < 4 || !token.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    if token.trim_matches('_').contains('_') {
        return true;
    }
    let mut letters = token.chars().filter(|c| c.is_alphabetic());
    let mut previous = letters.next();
    for letter in letters {
        if previous.is_some_and(char::is_lowercase) && letter.is_uppercase() {
            return true;
        }
        previous = Some(letter);
    }
    false
}

/// Whether `candidate` says what `other` already says.
///
/// The comparison is the words a sentence carries, not its characters: three of
/// every five of `candidate`'s significant words appearing in `other` is the
/// bar. The same check answers both questions Waku asks about a generated
/// objective — whether it is the title again, and whether a regeneration
/// decided nothing.
fn same_meaning(candidate: &str, other: &str) -> bool {
    let candidate = significant_words(candidate);
    let other = significant_words(other);
    if candidate.is_empty() || other.is_empty() {
        return false;
    }
    let shared = candidate
        .iter()
        .filter(|word| other.contains(*word))
        .count();
    shared * 5 >= candidate.len() * 3
}

/// The words of a sentence that carry its meaning: lowercased, punctuation
/// dropped, function words left out.
fn significant_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|word| word.len() > 2 && !MEANINGLESS_WORDS.contains(&word.as_str()))
        .collect()
}

/// Stores one generated objective for a task, applying the parser's rules
/// against what the task already carries.
///
/// The returned value is the objective clients should now render — the sentence
/// that was stored — or `None` when nothing changed, which is exactly when no
/// catalog revision may be published. A rewrite that says what the stored text
/// already says, one that restates the title or names the work, and a task
/// whose goal already owns the field all leave the store, and every client
/// holding it, untouched.
///
/// A task the daemon does not know is left alone too: the objective belongs to a
/// task that exists, and there is nothing to publish for one that does not.
pub fn store_generated_objective(
    state: &mut PersistedState,
    session_id: Uuid,
    candidate: &str,
) -> Option<String> {
    let session = state
        .sessions
        .iter_mut()
        .find(|session| session.id == session_id)?;
    // A goal the user or the provider set owns the field while it is set, so
    // the stored text and the catalog revision stay where they are. The
    // generated value is what a cleared goal falls back to, not a competitor
    // to the goal itself.
    if session
        .thread_goal
        .as_ref()
        .is_some_and(|goal| !goal.objective.trim().is_empty())
    {
        return None;
    }
    let text = accept(
        candidate,
        session.objective.as_deref(),
        session.display_title(),
    )
    .ok()?;
    session.objective = Some(text.clone());
    state.mark_session_dirty(session_id);
    Some(text)
}

/// What the daemon does at one deadline for one task's objective.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DigestStep {
    /// Nothing is due.
    Idle,
    /// Send the trigger for this dispatch.
    Dispatch(Uuid),
    /// The generation this dispatch asked for never published a result within
    /// [`GENERATION_TIMEOUT`]. A result that arrives later is not stored.
    Timeout(Uuid),
}

/// One task's generation pacing.
///
/// A settlement opens a quiet period; a newer settlement replaces it, so a
/// rapid back-and-forth generates once after the last turn instead of once per
/// turn. One generation runs at a time, a generation that is never answered
/// times out, and no more than [`MAX_GENERATIONS_PER_WINDOW`] generations run
/// inside any window, so a task that settles continuously for an hour stays
/// within its budget.
#[derive(Debug, Default)]
pub struct DigestSchedule {
    /// When the pending generation may start, if one is waiting for quiet.
    quiet_until: Option<Instant>,
    /// The generation in flight, and when the daemon stops expecting it.
    in_flight: Option<(Uuid, Instant)>,
    /// When generations started, oldest first, within the window.
    started: VecDeque<Instant>,
}

impl DigestSchedule {
    /// Opens — or, for a newer settlement, replaces — the quiet period.
    pub fn settled(&mut self, now: Instant) {
        self.quiet_until = Some(now + QUIET_PERIOD);
    }

    /// When [`Self::advance`] next has something to do.
    pub fn next_wake(&self) -> Option<Instant> {
        match (self.quiet_until, self.in_flight) {
            (Some(quiet), Some((_, deadline))) => Some(quiet.min(deadline)),
            (Some(quiet), None) => Some(quiet),
            (None, Some((_, deadline))) => Some(deadline),
            (None, None) => None,
        }
    }

    /// The step that is due now, spending what it consumes.
    pub fn advance(&mut self, now: Instant) -> DigestStep {
        if let Some((dispatch, deadline)) = self.in_flight {
            if now >= deadline {
                self.in_flight = None;
                return DigestStep::Timeout(dispatch);
            }
            // One generation at a time: a settlement that arrives while one is
            // in flight waits for it rather than overlapping it.
            return DigestStep::Idle;
        }
        let Some(quiet_until) = self.quiet_until else {
            return DigestStep::Idle;
        };
        if now < quiet_until {
            return DigestStep::Idle;
        }
        self.quiet_until = None;
        match self.take_generation(now) {
            Some(dispatch) => {
                self.in_flight = Some((dispatch, now + GENERATION_TIMEOUT));
                DigestStep::Dispatch(dispatch)
            }
            None => DigestStep::Idle,
        }
    }

    /// A dispatch id to use, if the window still has room for one.
    fn take_generation(&mut self, now: Instant) -> Option<Uuid> {
        self.started
            .retain(|started| now.saturating_duration_since(*started) < GENERATION_WINDOW);
        if self.started.len() >= MAX_GENERATIONS_PER_WINDOW {
            return None;
        }
        self.started.push_back(now);
        Some(Uuid::new_v4())
    }

    /// Whether `dispatch` is the generation still in flight — a result is
    /// stored only for the dispatch the daemon is waiting on.
    pub fn resolve(&mut self, dispatch: Uuid) -> bool {
        match self.in_flight {
            Some((in_flight, _)) if in_flight == dispatch => {
                self.in_flight = None;
                true
            }
            _ => false,
        }
    }
}

/// Where the bundled task-digest extension lives in an installed Waku build.
///
/// The file ships beside the app's other resources, so the directory is the
/// platform's resource directory — the layout `computer_use::pi_extension_path`
/// resolves for Waku's Computer Use extension. That helper is private to the
/// Computer Use module, and a second extension needs the same answer, so this
/// repeats the platform rule rather than reaching into another module's
/// internals; both move together if a platform layout changes.
pub fn pi_extension_path() -> anyhow::Result<PathBuf> {
    let executable = std::env::var_os(crate::APP_EXECUTABLE_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::current_exe().context("Waku executable path is unavailable")
        })?;
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("Waku executable has no parent"))?;
    let resources = match std::env::consts::OS {
        "macos" => directory
            .parent()
            .ok_or_else(|| anyhow!("Waku app bundle is malformed"))?
            .join("Resources"),
        "linux" if directory.file_name().is_some_and(|name| name == "bin") => directory
            .parent()
            .ok_or_else(|| anyhow!("Waku installation is malformed"))?
            .join("share/waku"),
        _ => directory.join("resources"),
    };
    let path = resources.join(EXTENSION_RESOURCE);
    if !path.is_file() {
        bail!(
            "the Waku task-digest extension is missing from this Waku build: {}",
            path.display()
        );
    }
    Ok(path)
}

/// The extension's path inside the resource directory, as `scripts/bundle.sh`
/// copies it.
pub const EXTENSION_RESOURCE: &str = "pi-extensions/task-digest.ts";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The objective a task already carries, and the title it is known by.
    const STORED: Option<&str> = Some("Ship a task list that says what each task is for");
    /// The title of a task the user has not named, so it repeats nothing.
    const TITLE: &str = crate::model::AgentSession::DEFAULT_TITLE;

    fn seconds(count: u64) -> Duration {
        Duration::from_secs(count)
    }

    fn dispatch(step: DigestStep) -> Uuid {
        match step {
            DigestStep::Dispatch(dispatch) => dispatch,
            other => panic!("expected a dispatch, got {other:?}"),
        }
    }

    #[test]
    fn a_trigger_names_the_dispatch_it_wants_back() {
        let dispatch = Uuid::new_v4();
        let prompt = trigger_prompt(dispatch);
        assert_eq!(prompt, format!("/waku:digest {dispatch}"));
        assert_eq!(trigger_dispatch(&prompt), Some(dispatch));
    }

    #[test]
    fn a_person_typing_the_command_is_not_a_trigger() {
        // The command without a dispatch is what a person's composer produces,
        // and there is no generation behind it to answer.
        for prompt in [
            "/waku:digest",
            "/waku:digest ",
            "/waku:digest later",
            "/waku:digest 12345",
            "/waku:digest not-a-uuid",
            "/waku:digest 8f0b0d1e-0000-0000-0000-000000000000 and more",
            "waku:digest 8f0b0d1e-0000-0000-0000-000000000000",
            "/compact",
            "hello",
        ] {
            assert_eq!(
                trigger_dispatch(prompt),
                None,
                "{prompt:?} is not a dispatch"
            );
        }
    }

    #[test]
    fn a_result_carries_the_version_and_the_dispatch_it_answers() {
        let dispatch = Uuid::new_v4();
        let raw = serde_json::json!({
            "v": 1,
            "dispatch": dispatch,
            "objective": "The task list says why each task exists",
        })
        .to_string();
        assert_eq!(
            parse_result(&raw),
            Some(DigestResult {
                dispatch,
                objective: "The task list says why each task exists".to_owned(),
            })
        );
    }

    #[test]
    fn a_result_waku_cannot_read_stores_nothing() {
        let dispatch = Uuid::new_v4();
        for raw in [
            // A version this build does not know, with no version at all, and
            // the shapes a truncated or rewritten payload takes.
            serde_json::json!({"v": 2, "dispatch": dispatch, "objective": "Newer"}).to_string(),
            serde_json::json!({"dispatch": dispatch, "objective": "No version"}).to_string(),
            "not json".to_owned(),
            serde_json::json!({"v": 1, "objective": "No dispatch"}).to_string(),
            serde_json::json!({"v": 1, "dispatch": "nope", "objective": "Bad dispatch"})
                .to_string(),
            serde_json::json!({"v": 1, "dispatch": dispatch}).to_string(),
            serde_json::json!({"v": 1, "dispatch": dispatch, "objective": 7}).to_string(),
        ] {
            assert_eq!(parse_result(&raw), None, "{raw} must not decode");
        }
    }

    #[test]
    fn an_objective_naming_the_work_is_rejected() {
        // A path, a file, or a symbol names the means, not the outcome. Every
        // one of these leaves the task with the objective it already had.
        for (candidate, rejection) in [
            (
                "Refactor src/app/sidebar.rs so the row builder stops allocating",
                ObjectiveRejection::Path,
            ),
            (
                "Rewrite the objective table in docs\\titles.md",
                ObjectiveRejection::Path,
            ),
            (
                "Move the sidebar row builder into sidebar.rs",
                ObjectiveRejection::Extension,
            ),
            (
                "Update the config at ~/.pi/settings.json",
                ObjectiveRejection::Path,
            ),
            (
                "Extend the schema in schema.ts",
                ObjectiveRejection::Extension,
            ),
            (
                "Make render_sidebar_session_item stop rebuilding the whole row",
                ObjectiveRejection::Symbol,
            ),
            (
                "Give the sidebar a RenderSidebarSessionItem that is cheap",
                ObjectiveRejection::Symbol,
            ),
        ] {
            assert_eq!(
                accept(candidate, STORED, TITLE),
                Err(rejection),
                "{candidate:?} names the work, not the outcome"
            );
        }
    }

    #[test]
    fn an_objective_that_restates_the_title_is_rejected() {
        // The title is already on the row, so an objective built out of it says
        // nothing the user cannot see.
        for candidate in [
            "Sidebar groups tasks by what they need",
            "Groups sidebar tasks by what they need",
            "Sidebar tasks grouped by need",
        ] {
            assert_eq!(
                accept(candidate, None, "Sidebar groups tasks by what they need"),
                Err(ObjectiveRejection::Title),
                "{candidate:?} is the title again"
            );
        }
    }

    #[test]
    fn a_rewording_keeps_the_stored_objective() {
        // Deciding nothing must not churn the text the client is showing.
        assert_eq!(
            accept(
                "Give every task a list that says what it is for",
                STORED,
                TITLE
            ),
            Err(ObjectiveRejection::Unchanged)
        );
        assert_eq!(
            accept(
                "Ship a task list that says what each task is for",
                STORED,
                TITLE
            ),
            Err(ObjectiveRejection::Unchanged)
        );
    }

    #[test]
    fn a_real_redefinition_replaces_the_objective() {
        // The user redirected the task: the objective describes different work.
        let candidate = "Keep the background service alive across workspace switches";
        assert_eq!(accept(candidate, STORED, TITLE), Ok(candidate.to_owned()));
    }

    #[test]
    fn a_sentence_is_stored_without_the_packaging_a_model_adds() {
        for candidate in [
            "The sidebar says what every task is for.",
            "  The sidebar says what every task is for.  ",
            "\n\"The sidebar says what every task is for.\"\n",
            "- The sidebar says what every task is for.",
            "**Objective:** The sidebar says what every task is for.",
            "```\nThe sidebar says what every task is for.\n```",
            "Objective: The sidebar says what every task is for.\n\n(one sentence)",
        ] {
            assert_eq!(
                accept(candidate, None, TITLE),
                Ok("The sidebar says what every task is for.".to_owned()),
                "{candidate:?} carries one sentence and packaging"
            );
        }
    }

    #[test]
    fn a_completion_that_is_not_a_sentence_is_rejected() {
        assert_eq!(accept("", None, TITLE), Err(ObjectiveRejection::Empty));
        assert_eq!(
            accept("   \n\t\n", None, TITLE),
            Err(ObjectiveRejection::Empty)
        );
        assert_eq!(accept("```", None, TITLE), Err(ObjectiveRejection::Empty));
        let paragraph = "The sidebar row shows a task's objective as one line. ".repeat(6);
        assert_eq!(
            accept(&paragraph, None, TITLE),
            Err(ObjectiveRejection::TooLong),
            "a row shows an outcome, not a paragraph"
        );
    }

    #[test]
    fn four_settles_inside_the_quiet_period_generate_once() {
        // The user sent four prompts in quick succession; only the last turn
        // earns a generation, and only after the task has been quiet.
        let base = Instant::now();
        let mut schedule = DigestSchedule::default();
        for offset in 0..4 {
            schedule.settled(base + seconds(offset));
        }
        assert_eq!(schedule.advance(base + seconds(3)), DigestStep::Idle);
        assert_eq!(schedule.next_wake(), Some(base + seconds(3) + QUIET_PERIOD));

        let dispatch = dispatch(schedule.advance(base + seconds(18)));
        assert!(schedule.resolve(dispatch));
        assert_eq!(
            schedule.advance(base + seconds(120)),
            DigestStep::Idle,
            "four settles produced one generation, and it already answered"
        );
    }

    #[test]
    fn a_newer_settle_replaces_the_pending_one() {
        let base = Instant::now();
        let mut schedule = DigestSchedule::default();
        schedule.settled(base);
        schedule.settled(base + seconds(10));
        assert_eq!(schedule.advance(base + seconds(15)), DigestStep::Idle);
        dispatch(schedule.advance(base + seconds(25)));
    }

    #[test]
    fn one_generation_runs_at_a_time() {
        let base = Instant::now();
        let mut schedule = DigestSchedule::default();
        schedule.settled(base);
        let first = dispatch(schedule.advance(base + QUIET_PERIOD));

        // A turn settles while the generation is still working.
        let next = base + QUIET_PERIOD + seconds(1);
        schedule.settled(next);
        assert_eq!(schedule.advance(next + seconds(20)), DigestStep::Idle);
        assert!(schedule.resolve(first));
        let second = dispatch(schedule.advance(next + seconds(20)));
        assert_ne!(second, first);
    }

    #[test]
    fn a_generation_that_never_answers_times_out_and_its_result_is_stale() {
        let base = Instant::now();
        let mut schedule = DigestSchedule::default();
        schedule.settled(base);
        let dispatch = dispatch(schedule.advance(base + QUIET_PERIOD));
        let deadline = base + QUIET_PERIOD + GENERATION_TIMEOUT;

        assert_eq!(schedule.advance(deadline - seconds(1)), DigestStep::Idle);
        assert_eq!(schedule.advance(deadline), DigestStep::Timeout(dispatch));
        assert!(
            !schedule.resolve(dispatch),
            "a result that arrives after the timeout must not be stored"
        );
    }

    #[test]
    fn an_hour_of_settles_stays_within_the_budget() {
        // A task that settles continuously for an hour generates six times, not
        // once per turn.
        let base = Instant::now();
        let mut schedule = DigestSchedule::default();
        let mut generations = Vec::new();
        for second in 0..3600 {
            let now = base + seconds(second);
            if second % 20 == 0 {
                schedule.settled(now);
            }
            match schedule.advance(now) {
                DigestStep::Dispatch(dispatch) => {
                    generations.push(now);
                    assert!(schedule.resolve(dispatch), "the generation answered");
                }
                DigestStep::Timeout(dispatch) => panic!("{dispatch} was answered in time"),
                DigestStep::Idle => {}
            }
        }

        assert_eq!(
            generations.len(),
            MAX_GENERATIONS_PER_WINDOW,
            "an hour of settles gets the window's budget and no more"
        );
        for window in 0..generations.len() {
            let inside = generations[window..]
                .iter()
                .filter(|started| {
                    started.saturating_duration_since(generations[window]) < GENERATION_WINDOW
                })
                .count();
            assert!(
                inside <= MAX_GENERATIONS_PER_WINDOW,
                "a rolling hour holds the cap"
            );
        }
    }

    #[test]
    fn the_bundled_extension_is_copied_from_the_resource_tree() {
        // Nothing else puts the extension where the daemon resolves it from:
        // the file has to exist in the source tree and the packaging script has
        // to copy it into the app's resources.
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("resources")
            .join(EXTENSION_RESOURCE);
        assert!(
            source.is_file(),
            "the bundled extension is missing: {}",
            source.display()
        );
        let bundler = include_str!("../../../scripts/bundle.sh");
        assert!(
            bundler.contains(&format!("resources/{EXTENSION_RESOURCE}")),
            "scripts/bundle.sh must copy {EXTENSION_RESOURCE} into the app's resources"
        );
    }
}
