//! Agent-facing context from the state the app already owns. These reads do
//! not probe providers, touch Git, hydrate history, or change the selection.
use serde_json::{Value, json};

use super::{Waku, json_of};
use crate::model::{AgentSession, ProviderKind};
use crate::query::QueryState;

pub(super) const CONTEXT_SECTIONS: &[&str] = &[
    "route",
    "selection",
    "task",
    "composer",
    "providers",
    "runtime",
    "workspace",
    "board",
    "panels",
    "ui",
];

pub(super) fn capabilities() -> Value {
    json!({
        "readOnly": ["describe", "snapshot", "transcript"],
        "daemonOnly": ["transcript"],
        "transcript": {"conversationId":"explicit conversation UUID; does not open it", "index":{"type":"index", "collection":"messages|activities|turns", "offset?":"zero-based item offset", "limit?":"1..100; default50"}, "text":{"type":"text", "collection":"messages|activities|turns", "itemId":"item UUID from index", "field":"field name from index", "offsetBytes?":"UTF-8 byte boundary; use returned continuation", "maxBytes?":"4..65536; default32768"}, "consistency":"per-call observation; persisted fallback can lag live unsaved state; re-index legacyDerived items if source changes or history is hydrated/migrated"},
        "contextSections": CONTEXT_SECTIONS,
        "sections": {
            "tree": "Rendered controls only; excludes virtualized off-screen rows",
            "selection": "Selected conversation metadata, history counts and detailLoaded; no transcript bodies",
            "session": "Complete selected conversation; request explicitly for full history",
            "sessions": "Conversation list projections, not Board issues",
            "task": "Selected Board issue, independent of the selected conversation",
            "tasks": "All Board issues, including issues with no conversations",
            "providers": "Installed/enabled providers, cached model catalogs and discovery progress; not authentication proof",
            "runtime": "Selected conversation process state and complete pending approval/input choices",
            "workspace": "Selected conversation workspace and cached Git/review state; never a fresh filesystem scan",
            "model": "Complete serialized model; may exceed the observation limit",
            "projects": "Known project identities and paths",
            "route": "Visible route plus separate conversation and issue selection",
            "board": "Board filters, dialogs and asynchronous save status",
            "composer": "Current draft and submission preparation/provider availability",
            "toast": "Current visible feedback",
            "panels": "Sidebar and right-panel visibility",
            "ui": "Cached layout measurements and live annotation selection/comment/editor/preview state"
        },
        "verbs": [
            {"name":"openTask", "args":{"id":"Board issue UUID"}, "effect":"navigation"},
            {"name":"newTask", "args":{"title?":"nonempty string; omit to open dialog"}, "effect":"creates issue when title is supplied"},
            {"name":"openSession", "args":{"id":"conversation UUID"}, "effect":"navigation; may hydrate history"},
            {"name":"newSession", "args":{"taskId?":"Board issue UUID"}, "effect":"creates or reuses draft; may reopen issue"},
            {"name":"selectProject", "args":{"id":"project UUID"}, "effect":"selects project and creates/reuses its standalone draft"},
            {"name":"submitPrompt", "args":{"text?":"string; omit to use composer"}, "effect":"submits/steers/queues a model prompt; may incur charges"},
            {"name":"cancelPrompt", "args":{}, "effect":"cancels selected conversation turn"},
            {"name":"answerPermission", "args":{"requestId":"runtime.permission.requestId", "optionId":"runtime.permission.options[].id", "message?":"string"}, "effect":"answers pending approval; may authorize tool execution"},
            {"name":"openSettings", "args":{"page?":"general|providers|skills|usage|daemon|computerUse|appearance"}, "effect":"navigation"},
            {"name":"togglePanel", "args":{"panel":"sidebar|rightPanel"}, "effect":"changes panel visibility"},
            {"name":"chooseProvider", "args":{"provider":"providers.items[].provider", "model?":"catalog model id"}, "effect":"changes selected conversation provider/model"},
            {"name":"chooseModel", "args":{"provider?":"provider id; defaults to selected conversation", "model":"catalog model id"}, "effect":"changes selected conversation model"}
        ],
        "limits": {"observationBytes": waku_client::MAX_AUTOMATION_OBSERVATION_BYTES, "transcriptItems":waku_client::transcript::MAX_TRANSCRIPT_ITEMS, "transcriptTextBytes":waku_client::transcript::MAX_TRANSCRIPT_TEXT_BYTES},
        "identities": {"sessionId":"automation attachment in command envelopes; conversation in diagnostic queries", "conversationId":"AgentSession.id", "taskId":"Board issue id"},
        "examples": ["inspect", "state /runtime/permission", "state /providers", "state /workspace", "tree composer", "snap --sections session"]
    })
}

pub(super) fn session_summary(session: &AgentSession) -> Value {
    let mut summary = json_of(&session.list_projection());
    let fields = summary
        .as_object_mut()
        .expect("session serializes as an object");
    for key in ["messages", "transcript_blocks", "turns", "provider_cursor"] {
        fields.remove(key);
    }
    fields.insert("detailLoaded".into(), json!(session.detail_loaded));
    // A lazy skeleton has unknown counts, not an empty conversation.
    fields.insert("historyCounts".into(), if session.detail_loaded {
        json!({"messages":session.messages.len(), "transcriptBlocks":session.transcript_blocks.len(), "turns":session.turns.len()})
    } else { Value::Null });
    fields.insert("activeTurnId".into(), json!(session.active_turn_id()));
    fields.insert(
        "providerNativeId".into(),
        json!(
            session
                .provider_cursor
                .as_ref()
                .map(|cursor| cursor.native_id())
        ),
    );
    fields.insert("contextUsage".into(), json!(session.context_usage));
    fields.insert(
        "queuedMessages".into(),
        json!(session.queued_messages.len()),
    );
    fields.insert("subagentRuns".into(), json!(session.subagent_runs.len()));
    summary
}

fn permission_info(permission: &crate::model::PendingPermission) -> Value {
    json!({"requestId":permission.request_id, "title":permission.title, "detail":permission.detail, "options":permission.options})
}

fn user_input_info(input: &super::super::PendingUserInput) -> Value {
    json!({"requestId":input.request_id, "questions":input.questions, "questionIndex":input.question_index, "selections":input.selections, "customAnswers":input.custom_answers, "dismissible":input.dismissible})
}

fn computer_approval_info(approval: &crate::computer_use::PendingComputerApproval) -> Value {
    let mut decisions = vec![("task", "session"), ("deny", "none")];
    if approval.target.persistable() {
        decisions.insert(1, ("always", "persistent"));
    }
    json!({
        "requestId":approval.request.call_id, "tool":approval.request.tool, "summary":approval.request.summary(), "sensitive":approval.sensitive,
        "target":approval.target, "persistable":approval.target.persistable(),
        "options":decisions.into_iter().map(|(decision,scope)| json!({"decision":decision,"scope":scope,"controlId":format!("computer-permission-{}-{decision}",approval.request.call_id)})).collect::<Vec<_>>()
    })
}

fn branch_info(
    branch: QueryState<'_, Result<Option<crate::git_branch::BranchSnapshot>, String>>,
    operation_pending: bool,
) -> Value {
    let (state, value, error) = match branch {
        QueryState::Missing => ("notRequested", Value::Null, None),
        QueryState::Pending => ("loading", Value::Null, None),
        QueryState::Ready(Ok(snapshot)) => (
            if snapshot.is_some() {
                "ready"
            } else {
                "notRepository"
            },
            json_of(snapshot),
            None,
        ),
        QueryState::Ready(Err(error)) => ("error", Value::Null, Some(error)),
    };
    json!({"state":state,"value":value,"error":error,"operationPending":operation_pending})
}

fn review_info(
    source: crate::review_diff::Source,
    snapshot: Option<&crate::review_diff::Snapshot>,
    loading: bool,
    error: Option<&str>,
) -> Value {
    json!({
        "state":if loading {"loading"} else if error.is_some() {"error"} else if snapshot.is_some() {"ready"} else {"notRequested"},
        "source":format!("{source:?}"), "error":error,
        "snapshot":snapshot.map(|snapshot| json!({
            "source":format!("{:?}",snapshot.source),"additions":snapshot.additions,"deletions":snapshot.deletions,"truncated":snapshot.truncated,
            "files":snapshot.files.iter().map(|file| json!({"path":file.path,"status":format!("{:?}",file.status),"additions":file.additions,"deletions":file.deletions})).collect::<Vec<_>>()
        }))
    })
}

impl Waku {
    pub(super) fn rpa_providers(&self) -> Value {
        json!({
            "detectionPending": self.provider_detection_remaining > 0,
            "items": ProviderKind::ALL.iter().map(|provider| {
                let probe = self.provider_probe(*provider);
                let installed = probe.is_some_and(|probe| probe.installed);
                let disabled = self.state.disabled_providers.contains(provider);
                json!({
                    "provider": provider,
                    "installed": installed,
                    "disabled": disabled,
                    "enabled": self.provider_enabled(*provider),
                    "selectableForSession": self.provider_enabled(*provider) && self.selected_session().is_some_and(|session| session.can_choose_model(*provider)),
                    "path": probe.and_then(|probe| probe.path.as_ref()),
                    "version": self.provider_versions.get(provider).and_then(Option::as_ref),
                    "versionPending": self.provider_version_probes_pending.contains(provider),
                    "versionState": if self.provider_version_probes_pending.contains(provider) { "loading" } else { match self.provider_versions.get(provider) { None => "notRequested", Some(None) => "unavailable", Some(Some(_)) => "ready" } },
                    "catalogState": if self.provider_model_discoveries_pending.contains(provider) { "loading" } else if self.provider_model_discoveries.contains(provider) { "attempted" } else { "cachedOrFallback" },
                    "models": probe.map(|probe| &probe.models),
                    "agentPresets": probe.map(|probe| &probe.agent_presets),
                    "authentication": "notChecked"
                })
            }).collect::<Vec<_>>()
        })
    }

    pub(super) fn rpa_runtime(&self) -> Value {
        let Some(session) = self.selected_session() else {
            return Value::Null;
        };
        let runtime = self.runtimes.get(&session.id);
        json!({
            "sessionId": session.id,
            "present": runtime.is_some(),
            "status": session.status,
            "runtimeId": session.runtime_event_cursor.map(|cursor| cursor.runtime_id),
            "preparing": self.submission_preparations.contains(&session.id) || self.response_fork_preparations.contains_key(&session.id),
            "streamPhase": runtime.and_then(|runtime| runtime.stream_phase).map(|phase| match phase {
                super::super::StreamPhase::Text => "text",
                super::super::StreamPhase::Reasoning => "reasoning",
                super::super::StreamPhase::Activity => "activity",
            }),
            "eventBacklog": runtime.map(|runtime| runtime.events.len() + runtime.pending_events.len()).unwrap_or(0),
            "lastError": runtime.and_then(|runtime| runtime.last_driver_error.as_ref()),
            "permission": runtime.and_then(|runtime| runtime.pending_permission.as_ref()).map(permission_info),
            "computerApproval": runtime.and_then(|runtime| runtime.pending_computer_approval.as_ref()).map(computer_approval_info),
            "userInput": runtime.and_then(|runtime| runtime.pending_user_input.as_ref()).map(user_input_info),
            "queuedMessageCount": session.queued_messages.len(),
            "subagentRuns": session.subagent_runs,
        })
    }

    pub(super) fn rpa_workspace(&self) -> Value {
        let Some(session) = self.selected_session() else {
            return Value::Null;
        };
        let path = self.workspace_path_for_session(session);
        let branch = path
            .map(|path| self.branch_snapshots.observe(&path.to_path_buf()))
            .unwrap_or(QueryState::Missing);
        json!({
            "sessionId": session.id,
            "projectId": session.project_id,
            "intent": session.workspace,
            "path": path,
            "source": "cachedAppState",
            "invalidationPending": self.workspace_queries_stale,
            "branch": branch_info(branch, self.branch_operation_pending),
            "review": review_info(self.right_panel_diff_source, self.right_panel_diff_snapshot.as_deref(), self.right_panel_diff_loading, self.right_panel_diff_error.as_deref())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_summary_never_copies_history_and_names_unloaded_history() {
        let mut session = AgentSession::new(uuid::Uuid::new_v4(), ProviderKind::Pi);
        session.messages.push(crate::model::Message::new(
            crate::model::MessageRole::Assistant,
            "x".repeat(5 * 1024 * 1024),
        ));
        session.provider_cursor = Some(crate::model::ProviderResumeCursor::Amp {
            thread_id: "native-thread".into(),
            fork_context: Some("large fork context".repeat(100_000)),
        });
        let summary = session_summary(&session);
        assert!(summary.get("messages").is_none());
        assert!(summary.get("provider_cursor").is_none());
        assert_eq!(summary["providerNativeId"], "native-thread");
        assert_eq!(summary["historyCounts"]["messages"], 1);
        assert!(serde_json::to_vec(&summary).unwrap().len() < 4096);
        session.detail_loaded = false;
        assert_eq!(session_summary(&session)["historyCounts"], Value::Null);
    }

    #[test]
    fn pending_attention_retains_details_and_actual_choice_ids() {
        let permission = crate::model::PendingPermission {
            request_id: "approval-7".into(),
            title: "Run command".into(),
            detail: "cargo test".into(),
            options: vec![
                crate::model::PermissionOption {
                    id: "allow-once".into(),
                    label: "Allow once".into(),
                    allow: true,
                },
                crate::model::PermissionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    allow: false,
                },
            ],
        };
        let value = permission_info(&permission);
        assert_eq!(value["detail"], "cargo test");
        assert_eq!(value["options"][1]["id"], "deny");
        assert_eq!(value["options"][1]["allow"], false);
        let input = super::super::super::PendingUserInput::new(
            "input-3".into(),
            vec![crate::model::UserInputQuestion {
                id: "branch".into(),
                header: "Branch".into(),
                question: "Which branch?".into(),
                options: vec![crate::model::UserInputOption {
                    label: "Main".into(),
                    description: Some("Use existing branch".into()),
                }],
                multi_select: false,
            }],
            true,
        );
        let value = user_input_info(&input);
        assert_eq!(value["questions"][0]["question"], "Which branch?");
        assert_eq!(value["dismissible"], true);
    }

    #[test]
    fn computer_approval_only_lists_the_controls_the_target_can_offer() {
        let mut approval = crate::computer_use::PendingComputerApproval {
            request: crate::computer_use::ComputerToolRequest {
                call_id: "approval-a".into(),
                tool: "use".into(),
                arguments: json!({}),
            },
            target: crate::computer_use::ComputerTarget {
                window_id: 7,
                bundle_id: String::new(),
                team_id: None,
                app_name: "Fixture".into(),
                window_title: "Fixture window".into(),
                width: 800,
                height: 600,
            },
            sensitive: false,
        };
        let value = computer_approval_info(&approval);
        assert_eq!(value["target"]["windowId"], 7);
        assert_eq!(value["options"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["options"][0]["controlId"],
            "computer-permission-approval-a-task"
        );
        approval.target.bundle_id = "fixture.app".into();
        let value = computer_approval_info(&approval);
        assert_eq!(value["options"][1]["decision"], "always");
        assert_eq!(value["options"][1]["scope"], "persistent");
    }

    #[test]
    fn cached_workspace_states_distinguish_not_requested_loading_error_and_stale_data() {
        assert_eq!(
            branch_info(QueryState::Missing, false)["state"],
            "notRequested"
        );
        assert_eq!(branch_info(QueryState::Pending, false)["state"], "loading");
        assert_eq!(
            branch_info(QueryState::Ready(&Ok(None)), false)["state"],
            "notRepository"
        );
        assert_eq!(
            branch_info(QueryState::Ready(&Err("git unavailable".into())), false)["error"],
            "git unavailable"
        );
        let source = crate::review_diff::Source::Uncommitted;
        let snapshot = crate::review_diff::parse_collected(source, "1\t0\tfile.txt\n", "", true);
        assert_eq!(
            review_info(source, None, false, None)["state"],
            "notRequested"
        );
        assert_eq!(
            review_info(source, Some(&snapshot), false, None)["state"],
            "ready"
        );
        for (loading, error, state) in [
            (true, None, "loading"),
            (false, Some("refresh failed"), "error"),
        ] {
            let value = review_info(source, Some(&snapshot), loading, error);
            assert_eq!(value["state"], state);
            assert_eq!(value["snapshot"]["files"][0]["path"], "file.txt");
        }
    }

    #[test]
    fn discovery_covers_actual_verbs_and_sections() {
        let manifest = capabilities();
        let names = manifest["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|verb| verb["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, super::super::VERBS);
        for section in super::super::STATE_SECTIONS {
            assert!(
                manifest["sections"].get(section).is_some(),
                "missing {section}"
            );
        }
    }
}
