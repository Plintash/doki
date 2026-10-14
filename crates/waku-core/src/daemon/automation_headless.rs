//! Provider-free semantic automation over the daemon's real session store.
//!
//! This executor has no window, UI tree, composer widget, or provider driver.
//! It shares the normal persistence lifecycle: standalone unstarted sessions
//! are transient, their drafts are durable project-keyed records, and selection
//! is process-local. Creating a draft never fabricates a transcript to make its
//! conversation identity durable. Board operations are deliberately excluded.

use serde_json::{Map, Value, json};
use uuid::Uuid;
use waku_protocol::{
    AutomationCall, AutomationError, AutomationExecution, AutomationInstance,
    AutomationObservation, MAX_AUTOMATION_OBSERVATION_BYTES,
};

use super::WakuBackend;
use crate::model::{AgentSession, Project};
#[cfg(test)]
use crate::persistence::ComposerDraftChange;
use crate::persistence::{ComposerDraftKey, PersistedState};

const MAX_LIST_ITEMS: usize = 128;
// Leave room for JSON escaping, duplicate snapshot value/observation payloads,
// and mutation diffs beneath the transport's 4 MiB execution limit.
const MAX_SUMMARY_BYTES: usize = 256;
const MAX_TITLE_BYTES: usize = 1024;
const MAX_DRAFT_BYTES: usize = 64 * 1024;
const SECTIONS: &[&str] = &["session", "sessions", "projects", "selection", "draft"];
const VERBS: &[&str] = &["newSession", "openSession", "renameSession", "setDraft"];

type Result<T> = std::result::Result<T, AutomationError>;

impl WakuBackend {
    /// Install the host's executable build identity before registration so
    /// observations and executor routing report exactly the same generation.
    pub(crate) fn configure_automation_headless(&self, build: String) {
        self.headless_identity.get_or_init(|| {
            let mut identity = self.new_automation_headless_identity();
            identity.build = build;
            identity
        });
    }

    pub(crate) fn automation_headless_identity(&self) -> AutomationInstance {
        self.headless_identity
            .get_or_init(|| self.new_automation_headless_identity())
            .clone()
    }

    fn new_automation_headless_identity(&self) -> AutomationInstance {
        AutomationInstance {
            instance_id: std::env::var("WAKU_RPA_INSTANCE_ID")
                .ok()
                .and_then(|value| Uuid::parse_str(&value).ok())
                .unwrap_or_else(Uuid::new_v4),
            checkout: self.default_cwd.display().to_string(),
            database: self.task_store.path().display().to_string(),
            pid: std::process::id(),
            build: option_env!("WAKU_BUILD_ID")
                .unwrap_or(concat!(env!("CARGO_PKG_VERSION"), "-daemon-headless"))
                .to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            executor_connected: true,
            scratch: std::env::var_os("WAKU_RPA_SCRATCH_NAME").is_some(),
            headless: true,
        }
    }

    pub(crate) fn automation_headless_describe(&self) -> Value {
        json!({
            "instance": self.automation_headless_identity(),
            "headless": true,
            "verbs": VERBS,
            "snapshotSections": ["state", "session", "sessions", "projects", "selection", "draft"],
            "capabilities": {
                "readOnly": ["describe", "snapshot", "transcript"],
                "window": false,
                "providers": false,
                "board": false,
                "selection": "process-local; openSession does not attach a runtime or hydrate history",
                "session": "bounded metadata only; use transcript for persisted history",
                "draft": "persisted draft text, attachment/annotation counts; no composer widget",
                "persistence": "unstarted standalone sessions are transient; drafts are durable by project; started session titles are durable",
                "verbs": [
                    {"name":"newSession", "args":{"projectId?":"existing project UUID; defaults to selected or only project"}, "effect":"creates or reuses a standalone unstarted draft; never starts a provider"},
                    {"name":"openSession", "args":{"id":"conversation UUID"}, "effect":"process-local selection only"},
                    {"name":"renameSession", "args":{"id":"conversation UUID", "title":"nonempty string; at most 1024 UTF-8 bytes"}, "effect":"updates the ordinary session title"},
                    {"name":"setDraft", "args":{"id":"conversation UUID", "text":"string; at most 65536 UTF-8 bytes"}, "effect":"persists draft text; preserves attachments, annotations and other drafts"}
                ],
                "limits": {"listItems":MAX_LIST_ITEMS, "summaryStringBytes":MAX_SUMMARY_BYTES, "titleBytes":MAX_TITLE_BYTES, "draftBytes":MAX_DRAFT_BYTES, "observationBytes":MAX_AUTOMATION_OBSERVATION_BYTES},
                "unsupported": ["act", "type", "key", "tree", "composer", "route", "board", "model", "providers", "runtime", "workspace", "submitPrompt"]
            }
        })
    }

    /// A bounded projection; it never serializes or hydrates a transcript.
    pub(crate) fn automation_headless_snapshot(&self, sections: &[String]) -> Result<Value> {
        validate_sections(sections)?;
        let state = self.task_state.lock();
        let selected = state
            .selected_session
            .and_then(|id| state.sessions.iter().find(|session| session.id == id));
        let all = sections.is_empty() || sections.iter().any(|section| section == "state");
        let wants = |section: &str| all || sections.iter().any(|value| value == section);
        let mut values = Map::new();
        let mut bounds = Map::new();
        if wants("session") {
            values.insert(
                "session".into(),
                selected.map(session_summary).unwrap_or(Value::Null),
            );
        }
        if wants("selection") {
            values.insert(
                "selection".into(),
                json!({
                    "selectedSession": state.selected_session,
                    "selectedProject": state.selected_project,
                    "persistent": false,
                }),
            );
        }
        if wants("sessions") {
            values.insert(
                "sessions".into(),
                Value::Array(
                    state
                        .sessions
                        .iter()
                        .take(MAX_LIST_ITEMS)
                        .map(session_summary)
                        .collect(),
                ),
            );
            bounds.insert("sessions".into(), list_bounds(state.sessions.len()));
        }
        if wants("projects") {
            values.insert(
                "projects".into(),
                Value::Array(
                    state
                        .projects
                        .iter()
                        .take(MAX_LIST_ITEMS)
                        .map(project_summary)
                        .collect(),
                ),
            );
            bounds.insert("projects".into(), list_bounds(state.projects.len()));
        }
        if wants("draft") {
            let draft = match selected {
                Some(session) => {
                    let drafts = self.composer_drafts.load().map_err(persistence_error)?;
                    let draft = drafts.get_for(session);
                    let text = draft.map(|draft| draft.text.as_str()).unwrap_or("");
                    json!({
                        "target": waku_protocol::persistence::ComposerDraftTarget::from(ComposerDraftKey::for_session(session)),
                        "text": prefix(text, MAX_DRAFT_BYTES),
                        "totalBytes": text.len(),
                        "truncated": text.len() > MAX_DRAFT_BYTES,
                        "attachmentCount": draft.map_or(0, |draft| draft.attachments.len()),
                        "annotationCount": draft.map_or(0, |draft| draft.annotations.len()),
                    })
                }
                None => Value::Null,
            };
            values.insert("draft".into(), draft);
        }
        let observed = json!({
            "instance": self.automation_headless_identity(),
            "headless": true,
            "window": null,
            "tree": null,
            "state": values,
            "bounds": bounds,
        });
        check_size(&observed)?;
        Ok(observed)
    }

    /// Invoked by the explicitly enabled headless executor over the same
    /// authenticated daemon transport as a native UI executor.
    pub(crate) fn automation_headless_call(
        &self,
        call: AutomationCall,
    ) -> Result<AutomationExecution> {
        if !cfg!(debug_assertions) {
            return Err(AutomationError::new(
                "automation_disabled",
                "headless automation is only available in debug builds",
            ));
        }
        if let AutomationCall::Invoke { verb, .. } = &call
            && !VERBS.contains(&verb.as_str())
        {
            return Err(unsupported(format!(
                "verb `{verb}` is unavailable headlessly"
            )));
        }
        let mutation = matches!(call, AutomationCall::Invoke { .. });
        let before = if mutation {
            Some(self.automation_headless_snapshot(&[])?)
        } else {
            None
        };
        let value = match &call {
            AutomationCall::Describe => self.automation_headless_describe(),
            AutomationCall::Snapshot { sections } => self.automation_headless_snapshot(sections)?,
            AutomationCall::Transcript {
                conversation_id,
                query,
            } => {
                let result = crate::transcript::read_transcript(
                    self.task_store.path(),
                    *conversation_id,
                    query,
                )
                .map_err(|error| {
                    match error.downcast::<waku_protocol::TranscriptError>() {
                        Ok(error) => AutomationError::new(error.code, error.message),
                        Err(error) => {
                            AutomationError::new("transcript_read_failed", error.to_string())
                        }
                    }
                })?;
                serde_json::to_value(result).map_err(|error| {
                    AutomationError::new("serialization_failed", error.to_string())
                })?
            }
            AutomationCall::Invoke { verb, args } => self.automation_headless_invoke(verb, args)?,
            AutomationCall::Act { .. } => return Err(unsupported("act requires a window")),
            AutomationCall::Type { .. } => {
                return Err(unsupported(
                    "type requires a composer widget; use setDraft for persisted text",
                ));
            }
            AutomationCall::Key { .. } => return Err(unsupported("key requires a window")),
        };
        let observed = match call {
            AutomationCall::Snapshot { .. } => value.clone(),
            AutomationCall::Describe | AutomationCall::Transcript { .. } => {
                self.automation_headless_snapshot(&["session".into(), "selection".into()])?
            }
            _ => self.automation_headless_snapshot(&[])?,
        };
        let state_diff = before
            .as_ref()
            .map_or_else(|| json!([]), |before| state_diff(before, &observed));
        let execution = AutomationExecution {
            value,
            observation: AutomationObservation {
                observed,
                tree_diff: Value::Null,
                state_diff,
            },
        };
        check_size(&execution)?;
        Ok(execution)
    }

    fn automation_headless_invoke(&self, verb: &str, args: &Value) -> Result<Value> {
        match verb {
            "newSession" => {
                if args.get("taskId").is_some() {
                    return Err(unsupported(
                        "issue-owned session creation requires the native executor",
                    ));
                }
                validate_args(verb, args, &["projectId"])?;
                let mut state = self.task_state.lock();
                let project_id = if args.get("projectId").is_some() {
                    uuid_arg(verb, args, "projectId")?
                } else if let Some(id) = state.selected_project {
                    id
                } else if state.projects.len() == 1 {
                    state.projects[0].id
                } else {
                    return Err(AutomationError::new(
                        "invalid_args",
                        "newSession needs projectId when no project is selected and there is not exactly one existing project",
                    ));
                };
                if !state
                    .projects
                    .iter()
                    .any(|project| project.id == project_id)
                {
                    return Err(not_found("project", project_id));
                }
                let reused = state
                    .sessions
                    .iter()
                    .find(|session| {
                        session.project_id == project_id
                            && session.task_id.is_none()
                            && !session.has_started()
                    })
                    .map(|session| session.id);
                let id = match reused {
                    Some(id) => id,
                    None => {
                        let mut session = state.new_session(project_id, state.last_provider);
                        if let Some(source) = state
                            .selected_session
                            .and_then(|id| state.sessions.iter().find(|session| session.id == id))
                        {
                            session.runtime_mode = source.runtime_mode;
                        }
                        let id = session.id;
                        state.push_session(session);
                        if let Err(error) = self.task_store.save(&mut state) {
                            state.sessions.retain(|session| session.id != id);
                            return Err(persistence_error(error));
                        }
                        id
                    }
                };
                select_session(&mut state, id)?;
                Ok(json!({"opened":id, "reused":reused.is_some(), "persistent":false}))
            }
            "openSession" => {
                validate_args(verb, args, &["id"])?;
                let id = uuid_arg(verb, args, "id")?;
                select_session(&mut self.task_state.lock(), id)?;
                Ok(json!({"opened":id}))
            }
            "renameSession" => {
                validate_args(verb, args, &["id", "title"])?;
                let id = uuid_arg(verb, args, "id")?;
                let title = string_arg(verb, args, "title", MAX_TITLE_BYTES)?.trim();
                if title.is_empty() {
                    return Err(AutomationError::new(
                        "invalid_args",
                        "renameSession title must not be empty",
                    ));
                }
                let mut state = self.task_state.lock();
                let session = state
                    .session_mut(id)
                    .ok_or_else(|| not_found("session", id))?;
                let previous = (session.title.clone(), session.updated_at);
                let changed = session.set_title(title);
                let persistent = session.has_started() || session.task_id.is_some();
                if changed && let Err(error) = self.task_store.save(&mut state) {
                    let session = state.session_mut(id).expect("session was found above");
                    session.title = previous.0;
                    session.updated_at = previous.1;
                    return Err(persistence_error(error));
                }
                Ok(json!({"renamed":id, "title":title, "changed":changed, "persistent":persistent}))
            }
            "setDraft" => {
                validate_args(verb, args, &["id", "text"])?;
                let id = uuid_arg(verb, args, "id")?;
                let text = string_arg(verb, args, "text", MAX_DRAFT_BYTES)?;
                let state = self.task_state.lock();
                let session = state
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
                    .ok_or_else(|| not_found("session", id))?;
                let key = ComposerDraftKey::for_session(session);
                let target: waku_protocol::persistence::ComposerDraftTarget = key.into();
                self.composer_drafts
                    .set_text(key, text.to_owned())
                    .map_err(persistence_error)?;
                Ok(json!({"updated":id, "target":target, "textBytes":text.len()}))
            }
            _ => Err(unsupported(format!(
                "verb `{verb}` is unavailable without a window/provider; supported verbs: {}",
                VERBS.join(", ")
            ))),
        }
    }
}

fn select_session(state: &mut PersistedState, id: Uuid) -> Result<()> {
    let session = state
        .sessions
        .iter()
        .find(|session| session.id == id)
        .ok_or_else(|| not_found("session", id))?;
    state.selected_project = Some(session.project_id);
    state.last_provider = session.provider;
    state.last_model = session.model.clone();
    state.last_reasoning_effort = session.reasoning_effort.clone();
    state.last_service_tier = session.service_tier.clone();
    state.last_context_window = session.context_window.clone();
    state.selected_session = Some(id);
    Ok(())
}

fn session_summary(session: &AgentSession) -> Value {
    // Enumerated scalar fields keep large extension/provider fields and
    // transcripts out of observations, including hydrated sessions.
    let mut value = json!({
        "id":session.id,
        "title":prefix(&session.title, MAX_SUMMARY_BYTES),
        "auto_title":session.auto_title.as_deref().map(|value| prefix(value, MAX_SUMMARY_BYTES)),
        "project_id":session.project_id,
        "task_id":session.task_id,
        "provider":session.provider,
        "model":session.model.as_deref().map(|value| prefix(value, MAX_SUMMARY_BYTES)),
        "runtime_mode":session.runtime_mode,
        "status":session.status,
        "created_at":session.created_at,
        "updated_at":session.updated_at,
        "last_reply_at":session.last_reply_at,
        "is_draft":!session.has_started(),
        "detailLoaded":session.detail_loaded,
        "summaryOnly":true,
        "historyCounts":if session.detail_loaded { json!({"messages":session.messages.len(), "transcriptBlocks":session.transcript_blocks.len(), "turns":session.turns.len()}) } else { Value::Null },
    });
    value["truncatedFields"] = json!(
        [
            ("title", Some(session.title.as_str())),
            ("auto_title", session.auto_title.as_deref()),
            ("model", session.model.as_deref()),
        ]
        .into_iter()
        .filter_map(|(key, value)| value
            .is_some_and(|value| value.len() > MAX_SUMMARY_BYTES)
            .then_some(key))
        .collect::<Vec<_>>()
    );
    value
}

fn project_summary(project: &Project) -> Value {
    let path = project.path.to_string_lossy();
    json!({
        "id":project.id, "name":prefix(&project.name, MAX_SUMMARY_BYTES),
        "path":prefix(&path, MAX_SUMMARY_BYTES), "created_at":project.created_at,
        "truncatedFields":([("name",project.name.as_str()),("path",path.as_ref())].into_iter().filter_map(|(key,value)| (value.len() > MAX_SUMMARY_BYTES).then_some(key)).collect::<Vec<_>>()),
    })
}

fn prefix(value: &str, max: usize) -> &str {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn list_bounds(total: usize) -> Value {
    json!({"total":total, "returned":total.min(MAX_LIST_ITEMS), "truncated":total > MAX_LIST_ITEMS})
}

fn validate_sections(sections: &[String]) -> Result<()> {
    for section in sections {
        if section != "state" && !SECTIONS.contains(&section.as_str()) {
            return Err(unsupported(format!(
                "snapshot section `{section}` is unavailable headlessly; supported sections: state, {}",
                SECTIONS.join(", ")
            )));
        }
    }
    Ok(())
}

fn validate_args(verb: &str, args: &Value, allowed: &[&str]) -> Result<()> {
    if args.is_null() && verb == "newSession" {
        return Ok(());
    }
    let fields = args.as_object().ok_or_else(|| {
        AutomationError::new(
            "invalid_args",
            format!("{verb} requires an argument object"),
        )
    })?;
    if let Some(key) = fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(AutomationError::new(
            "invalid_args",
            format!("{verb} does not accept `{key}`"),
        ));
    }
    Ok(())
}

fn uuid_arg(verb: &str, args: &Value, key: &str) -> Result<Uuid> {
    args.get(key)
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| {
            AutomationError::new("invalid_args", format!("{verb} wants a UUID in `{key}`"))
        })
}

fn string_arg<'a>(verb: &str, args: &'a Value, key: &str, max: usize) -> Result<&'a str> {
    let value = args.get(key).and_then(Value::as_str).ok_or_else(|| {
        AutomationError::new("invalid_args", format!("{verb} wants a string in `{key}`"))
    })?;
    if value.len() > max {
        return Err(AutomationError::new(
            "invalid_args",
            format!("{verb} `{key}` exceeds {max} UTF-8 bytes"),
        ));
    }
    Ok(value)
}

fn check_size(value: &impl serde::Serialize) -> Result<()> {
    if serde_json::to_vec(value)
        .map_err(|error| AutomationError::new("serialization_failed", error.to_string()))?
        .len()
        > MAX_AUTOMATION_OBSERVATION_BYTES
    {
        return Err(AutomationError::new(
            "observation_too_large",
            "headless observation exceeds 4 MiB; select fewer sections",
        ));
    }
    Ok(())
}

fn state_diff(before: &Value, after: &Value) -> Value {
    fn visit(before: &Value, after: &Value, pointer: &str, changes: &mut Vec<Value>) {
        if before == after {
            return;
        }
        if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
            for (key, value) in after {
                let pointer = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                visit(
                    before.get(key).unwrap_or(&Value::Null),
                    value,
                    &pointer,
                    changes,
                );
            }
            for key in before.keys().filter(|key| !after.contains_key(*key)) {
                changes.push(json!({"pointer":format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")), "removed":true}));
            }
        } else {
            changes.push(json!({"pointer":pointer, "value":after}));
        }
    }
    let mut changes = Vec::new();
    visit(&before["state"], &after["state"], "", &mut changes);
    Value::Array(changes)
}

fn unsupported(message: impl Into<String>) -> AutomationError {
    AutomationError::new("unsupported_headless", message)
}

fn not_found(kind: &str, id: Uuid) -> AutomationError {
    let mut error = AutomationError::new("not_found", format!("no {kind} `{id}`"));
    error.target = Some(id.to_string());
    error
}

fn persistence_error(error: std::io::Error) -> AutomationError {
    AutomationError::new("persistence_failed", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, MessageRole, ProviderKind};
    use crate::persistence::{ComposerDraft, ComposerDraftAttachment, StateStore};
    use crate::settings::DaemonSettingsStore;
    use std::path::PathBuf;
    use waku_protocol::{
        AutomationAction, AutomationTarget, TranscriptCollection, TranscriptQuery,
    };

    struct Fixture {
        root: PathBuf,
        project: Uuid,
        first: Uuid,
        second: Uuid,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("waku-headless-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let project = Project::from_path(root.clone());
            let mut first = AgentSession::new(project.id, ProviderKind::Codex);
            first.messages.push(Message::new(
                MessageRole::User,
                "provider-free fixture history",
            ));
            let mut second = AgentSession::new(project.id, ProviderKind::Codex);
            second.messages.push(Message::new(
                MessageRole::User,
                "unselected fixture history",
            ));
            let fixture = Self {
                root,
                project: project.id,
                first: first.id,
                second: second.id,
            };
            let mut state = PersistedState::empty();
            state.projects.push(project);
            state.push_session(first);
            state.push_session(second);
            StateStore::daemon(fixture.root.join("app.db"))
                .save(&mut state)
                .unwrap();
            fixture
        }

        fn backend(&self) -> WakuBackend {
            WakuBackend::new(
                DaemonSettingsStore::open(self.root.join("settings.json")).unwrap(),
                StateStore::daemon(self.root.join("app.db")),
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn invoke(backend: &WakuBackend, verb: &str, args: Value) -> Result<AutomationExecution> {
        backend.automation_headless_call(AutomationCall::Invoke {
            verb: verb.into(),
            args,
        })
    }

    fn opened(execution: &AutomationExecution) -> Uuid {
        Uuid::parse_str(execution.value["opened"].as_str().unwrap()).unwrap()
    }

    fn index(backend: &WakuBackend, id: Uuid, limit: usize) -> Result<AutomationExecution> {
        backend.automation_headless_call(AutomationCall::Transcript {
            conversation_id: id,
            query: TranscriptQuery::Index {
                collection: TranscriptCollection::Messages,
                offset: 0,
                limit,
            },
        })
    }

    #[test]
    fn headless_describe_has_truthful_capabilities_and_no_window() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        backend.configure_automation_headless("test-build-identity".into());
        let result = backend
            .automation_headless_call(AutomationCall::Describe)
            .unwrap();
        assert_eq!(result.value["instance"]["build"], "test-build-identity");
        assert_eq!(
            result.observation.observed["instance"]["build"],
            "test-build-identity"
        );
        assert_eq!(result.value["instance"]["headless"], true);
        assert_eq!(result.value["capabilities"]["providers"], false);
        assert_eq!(result.value["capabilities"]["window"], false);
        assert_eq!(result.value["verbs"], json!(VERBS));
        assert!(result.observation.observed["tree"].is_null());
        assert!(result.observation.observed["window"].is_null());
        assert!(result.observation.state_diff.as_array().unwrap().is_empty());
        assert_eq!(
            backend.automation_headless_identity().instance_id,
            backend.automation_headless_identity().instance_id
        );
    }

    #[test]
    fn headless_new_session_reuses_native_draft_and_persists_project_keyed_text() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let first = invoke(&backend, "newSession", json!({})).unwrap();
        let id = opened(&first);
        assert_eq!(first.value["reused"], false);
        assert_eq!(first.value["persistent"], false);
        assert_eq!(
            first.observation.observed["state"]["selection"]["selectedProject"],
            fixture.project.to_string()
        );
        assert_eq!(
            opened(&invoke(&backend, "newSession", json!({"projectId":fixture.project})).unwrap()),
            id
        );
        invoke(
            &backend,
            "setDraft",
            json!({"id":id,"text":"saved without a model"}),
        )
        .unwrap();
        assert_eq!(
            index(&backend, id, 1).unwrap_err().code,
            "conversation_not_found"
        );
        assert!(backend.sessions.lock().is_empty());
        assert!(backend.terminals.lock().is_empty());
        drop(backend);

        let reopened = fixture.backend();
        assert!(
            !reopened
                .task_state
                .lock()
                .sessions
                .iter()
                .any(|session| session.id == id)
        );
        let next = invoke(
            &reopened,
            "newSession",
            json!({"projectId":fixture.project}),
        )
        .unwrap();
        assert_ne!(opened(&next), id);
        assert_eq!(
            next.observation.observed["state"]["draft"]["text"],
            "saved without a model"
        );
        assert!(reopened.sessions.lock().is_empty());
        assert!(!fixture.root.join("settings.json").exists());
        assert!(!fixture.root.join("app.json").exists());
        assert!(!fixture.root.join("state.json").exists());
    }

    #[test]
    fn headless_open_and_rename_preserve_persisted_history_without_hydration() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let before = index(&backend, fixture.first, 10).unwrap().value["page"].clone();
        invoke(&backend, "openSession", json!({"id":fixture.first})).unwrap();
        let result = invoke(
            &backend,
            "renameSession",
            json!({"id":fixture.first,"title":"  persisted title  "}),
        )
        .unwrap();
        assert_eq!(result.value["title"], "persisted title");
        assert_eq!(result.value["persistent"], true);
        assert_eq!(
            result.observation.observed["state"]["session"]["detailLoaded"],
            false
        );
        assert_eq!(
            result.observation.observed["state"]["session"]["title"],
            "persisted title"
        );
        assert!(
            result
                .observation
                .state_diff
                .as_array()
                .unwrap()
                .iter()
                .any(|diff| diff["pointer"] == "/session/title")
        );
        assert_eq!(
            index(&backend, fixture.first, 10).unwrap().value["page"],
            before
        );
        assert!(
            !invoke(
                &backend,
                "renameSession",
                json!({"id":fixture.first,"title":"persisted title"})
            )
            .unwrap()
            .value["changed"]
                .as_bool()
                .unwrap()
        );
        drop(backend);

        let reopened = fixture.backend();
        assert!(reopened.task_state.lock().selected_session.is_none());
        let selected = invoke(&reopened, "openSession", json!({"id":fixture.first})).unwrap();
        assert_eq!(
            selected.observation.observed["state"]["session"]["title"],
            "persisted title"
        );
        assert_eq!(
            index(&reopened, fixture.first, 10).unwrap().value["page"],
            before
        );
        let other = reopened
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == fixture.second)
            .unwrap()
            .title
            .clone();
        assert_eq!(other, AgentSession::DEFAULT_TITLE);
        assert!(reopened.sessions.lock().is_empty());
    }

    #[test]
    fn headless_set_draft_preserves_attachments_and_other_sessions() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let attachment = ComposerDraftAttachment {
            path: fixture.root.join("document.txt"),
            mention: "document".into(),
            name: "document.txt".into(),
            is_dir: false,
            is_image: false,
            blob_reference: None,
        };
        backend
            .composer_drafts
            .apply_changes(vec![
                ComposerDraftChange {
                    target: ComposerDraftKey::Session(fixture.first).into(),
                    draft: Some(ComposerDraft {
                        text: "old".into(),
                        attachments: vec![attachment.clone()],
                        annotations: vec![],
                    }),
                },
                ComposerDraftChange {
                    target: ComposerDraftKey::Session(fixture.second).into(),
                    draft: Some(ComposerDraft {
                        text: "untouched".into(),
                        ..Default::default()
                    }),
                },
            ])
            .unwrap();
        invoke(
            &backend,
            "setDraft",
            json!({"id":fixture.first,"text":"updated"}),
        )
        .unwrap();
        invoke(&backend, "setDraft", json!({"id":fixture.first,"text":""})).unwrap();
        drop(backend);
        let reopened = fixture.backend();
        let drafts = reopened.composer_drafts.load().unwrap();
        let changed = drafts
            .get(ComposerDraftKey::Session(fixture.first))
            .unwrap();
        assert!(changed.text.is_empty());
        assert_eq!(changed.attachments, vec![attachment]);
        assert_eq!(
            drafts
                .get(ComposerDraftKey::Session(fixture.second))
                .unwrap()
                .text,
            "untouched"
        );
        assert!(reopened.task_state.lock().selected_session.is_none());
        assert!(reopened.sessions.lock().is_empty());
    }

    #[test]
    fn headless_ui_and_provider_operations_fail_explicitly() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        for call in [
            AutomationCall::Act {
                action: AutomationAction::Click,
                target: AutomationTarget::Id("send-or-stop".into()),
            },
            AutomationCall::Type {
                text: "do not submit".into(),
                target: None,
            },
            AutomationCall::Key {
                chord: "enter".into(),
            },
            AutomationCall::Invoke {
                verb: "submitPrompt".into(),
                args: json!({"text":"do not submit"}),
            },
            AutomationCall::Invoke {
                verb: "newSession".into(),
                args: json!({"taskId":Uuid::new_v4()}),
            },
        ] {
            assert_eq!(
                backend.automation_headless_call(call).unwrap_err().code,
                "unsupported_headless"
            );
        }
        for section in [
            "tree",
            "composer",
            "ui",
            "board",
            "model",
            "runtime",
            "providers",
            "route",
            "panels",
            "toast",
            "workspace",
            "task",
            "tasks",
            "unknown",
        ] {
            let call = AutomationCall::Snapshot {
                sections: vec!["state".into(), section.into()],
            };
            assert_eq!(
                backend.automation_headless_call(call).unwrap_err().code,
                "unsupported_headless"
            );
        }
        assert!(backend.sessions.lock().is_empty());
        assert_eq!(backend.task_state.lock().sessions.len(), 2);
    }

    #[test]
    fn headless_rejects_invalid_arguments_and_preserves_selection() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        invoke(&backend, "openSession", json!({"id":fixture.first})).unwrap();
        for (verb, args) in [
            ("newSession", json!({"provider":"codex"})),
            ("newSession", json!({"projectId":null})),
            ("openSession", json!({"id":"not-a-uuid"})),
            ("openSession", json!({"id":fixture.first,"typo":true})),
            ("renameSession", json!({"id":fixture.first,"title":" "})),
            (
                "renameSession",
                json!({"id":fixture.first,"title":"a".repeat(MAX_TITLE_BYTES+1)}),
            ),
            ("setDraft", json!({"id":fixture.first,"text":123})),
            (
                "setDraft",
                json!({"id":fixture.first,"text":"a".repeat(MAX_DRAFT_BYTES+1)}),
            ),
            ("setDraft", json!({"id":fixture.first})),
        ] {
            assert_eq!(
                invoke(&backend, verb, args).unwrap_err().code,
                "invalid_args"
            );
        }
        assert_eq!(
            invoke(&backend, "openSession", json!({"id":Uuid::new_v4()}))
                .unwrap_err()
                .code,
            "not_found"
        );
        assert_eq!(
            invoke(&backend, "newSession", json!({"projectId":Uuid::new_v4()}))
                .unwrap_err()
                .code,
            "not_found"
        );
        assert_eq!(
            backend.task_state.lock().selected_session,
            Some(fixture.first)
        );
        assert_eq!(backend.task_state.lock().sessions.len(), 2);
        assert!(!fixture.root.join("composer-drafts.json").exists());
    }

    #[test]
    fn headless_snapshots_bound_lists_text_and_hydrated_history() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        {
            let mut state = backend.task_state.lock();
            for _ in 0..MAX_LIST_ITEMS {
                let mut session = AgentSession::new(fixture.project, ProviderKind::Codex);
                session.title = "🐱".repeat(MAX_SUMMARY_BYTES);
                session.messages.push(Message::new(
                    MessageRole::User,
                    "large transcript".repeat(1000),
                ));
                state.push_session(session);
            }
            state.selected_session = state.sessions.last().map(|session| session.id);
        }
        let all = backend.automation_headless_snapshot(&[]).unwrap();
        assert_eq!(
            all["state"]["sessions"].as_array().unwrap().len(),
            MAX_LIST_ITEMS
        );
        assert_eq!(all["bounds"]["sessions"]["truncated"], true);
        assert_eq!(all["bounds"]["sessions"]["total"], MAX_LIST_ITEMS + 2);
        assert!(all["state"]["session"].get("messages").is_none());
        assert_eq!(
            all["state"]["session"]["title"].as_str().unwrap().len(),
            MAX_SUMMARY_BYTES
        );
        assert_eq!(all["state"]["session"]["truncatedFields"], json!(["title"]));
        assert!(serde_json::to_vec(&all).unwrap().len() < MAX_AUTOMATION_OBSERVATION_BYTES);
        let selected = backend
            .automation_headless_snapshot(&["selection".into()])
            .unwrap();
        assert_eq!(selected["state"].as_object().unwrap().len(), 1);
        assert!(selected["state"].get("sessions").is_none());
        assert_eq!(prefix("x🐱", 4), "x");
    }

    #[test]
    fn headless_complete_execution_fits_bound_with_worst_case_json_escaping() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        {
            let mut state = backend.task_state.lock();
            state.projects.clear();
            state.sessions.clear();
            for _ in 0..MAX_LIST_ITEMS {
                let mut project = Project::from_path(PathBuf::from("\0".repeat(MAX_SUMMARY_BYTES)));
                project.name = "\0".repeat(MAX_SUMMARY_BYTES);
                let mut session = AgentSession::new(project.id, ProviderKind::Codex);
                session.title = "\0".repeat(MAX_SUMMARY_BYTES);
                session.auto_title = Some("\0".repeat(MAX_SUMMARY_BYTES));
                session.model = Some("\0".repeat(MAX_SUMMARY_BYTES));
                state.projects.push(project);
                state.push_session(session);
            }
            state.selected_session = state.sessions.last().map(|session| session.id);
            state.selected_project = state.projects.last().map(|project| project.id);
        }
        let id = backend.task_state.lock().selected_session.unwrap();
        let changed = invoke(
            &backend,
            "setDraft",
            json!({"id":id,"text":"\0".repeat(MAX_DRAFT_BYTES)}),
        )
        .unwrap();
        assert!(serde_json::to_vec(&changed).unwrap().len() <= MAX_AUTOMATION_OBSERVATION_BYTES);
        let result = backend
            .automation_headless_call(AutomationCall::Snapshot { sections: vec![] })
            .unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_AUTOMATION_OBSERVATION_BYTES);
    }

    #[test]
    fn headless_persisted_transcript_keeps_reader_bounds_and_does_not_select() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let result = index(&backend, fixture.first, 1).unwrap();
        assert_eq!(result.value["page"]["total"], 1);
        assert!(backend.task_state.lock().selected_session.is_none());
        assert_eq!(
            index(&backend, fixture.first, 0).unwrap_err().code,
            "invalid_arguments"
        );
        assert_eq!(
            index(&backend, fixture.first, 101).unwrap_err().code,
            "invalid_arguments"
        );
        assert_eq!(
            index(&backend, Uuid::new_v4(), 1).unwrap_err().code,
            "conversation_not_found"
        );
    }

    #[test]
    fn headless_failed_title_save_rolls_back_memory_and_database() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let connection = rusqlite::Connection::open(fixture.root.join("app.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_headless_title BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'injected title failure'); END;").unwrap();
        let error = invoke(
            &backend,
            "renameSession",
            json!({"id":fixture.first,"title":"must not stick"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "persistence_failed");
        assert_eq!(
            backend
                .task_state
                .lock()
                .sessions
                .iter()
                .find(|session| session.id == fixture.first)
                .unwrap()
                .title,
            AgentSession::DEFAULT_TITLE
        );
        drop(backend);
        let reopened = fixture.backend();
        assert_eq!(
            reopened
                .task_state
                .lock()
                .sessions
                .iter()
                .find(|session| session.id == fixture.first)
                .unwrap()
                .title,
            AgentSession::DEFAULT_TITLE
        );
        assert_eq!(
            index(&reopened, fixture.first, 1).unwrap().value["page"]["total"],
            1
        );
    }
}
