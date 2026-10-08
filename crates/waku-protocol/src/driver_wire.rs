use anyhow::{Context as _, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::WireDriverEvent;
use crate::computer_use::{ComputerTarget, ComputerUsePhase, ComputerUseState};
use crate::model::{
    ActivityKind, DriverEvent, ExtensionWidgetPlacement, NotificationSeverity, PermissionOption,
    UserInputQuestion,
};

pub fn decode_enum<T: DeserializeOwned>(value: &str) -> anyhow::Result<T> {
    serde_json::from_value(Value::String(value.to_owned()))
        .with_context(|| format!("invalid protocol enum value {value:?}"))
}

pub fn encode_enum<T: Serialize>(value: T) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("protocol enum did not serialize as a string"))
}

pub fn event_to_wire(event: DriverEvent) -> anyhow::Result<WireDriverEvent> {
    let (kind, payload) = match event {
        DriverEvent::RuntimeEventCursorAdvanced(_) => {
            bail!("client-only runtime cursors cannot be sent by the daemon")
        }
        DriverEvent::Connected { provider_cursor } => {
            ("connected", serde_json::to_value(provider_cursor)?)
        }
        DriverEvent::AgentPresetSelected(preset) => {
            ("agentPresetSelected", serde_json::to_value(preset)?)
        }
        DriverEvent::AutoTitleUpdated(title) => ("autoTitleUpdated", serde_json::to_value(title)?),
        DriverEvent::AvailableCommands(commands) => {
            ("availableCommands", serde_json::to_value(commands)?)
        }
        DriverEvent::TurnStarted => ("turnStarted", Value::Null),
        DriverEvent::TurnParked => ("turnParked", Value::Null),
        DriverEvent::TextDelta(text) => ("textDelta", Value::String(text)),
        DriverEvent::ReasoningDelta(text) => ("reasoningDelta", Value::String(text)),
        DriverEvent::Activity {
            id,
            kind,
            title,
            detail,
            complete,
        } => (
            "activity",
            json!({
                "id": id,
                "kind": kind,
                "title": title,
                "detail": detail,
                "complete": complete,
            }),
        ),
        DriverEvent::RichActivity(activity) => ("richActivity", serde_json::to_value(activity)?),
        DriverEvent::BackgroundWork(work) => ("backgroundWork", serde_json::to_value(work)?),
        DriverEvent::ExtensionMessage {
            custom_type,
            text,
            display,
        } => (
            "extensionMessage",
            json!({ "customType": custom_type, "text": text, "display": display }),
        ),
        DriverEvent::ExtensionNotification { message, severity } => (
            "extensionNotification",
            json!({
                "message": message,
                "severity": encode_enum(severity)?,
            }),
        ),
        DriverEvent::ExtensionStatus { key, text } => {
            ("extensionStatus", json!({ "key": key, "text": text }))
        }
        DriverEvent::ExtensionWidget {
            key,
            lines,
            placement,
        } => (
            "extensionWidget",
            json!({
                "key": key,
                "lines": lines,
                "placement": encode_enum(placement)?,
            }),
        ),
        DriverEvent::ExtensionTitle { title } => ("extensionTitle", json!({ "title": title })),
        DriverEvent::ExtensionEditorText { text } => {
            ("extensionEditorText", json!({ "text": text }))
        }
        DriverEvent::Permission {
            request_id,
            title,
            detail,
            options,
        } => (
            "permission",
            json!({
                "requestId": request_id,
                "title": title,
                "detail": detail,
                "options": options,
            }),
        ),
        DriverEvent::UserInputRequested {
            request_id,
            questions,
        } => (
            "userInputRequested",
            json!({
                "requestId": request_id,
                "questions": questions,
            }),
        ),
        DriverEvent::ComputerUseUpdated(state) => (
            "computerUseUpdated",
            serde_json::to_value(ComputerUseWire {
                target: state.target,
                phase: state.phase,
                visible: state.visible,
                image_url: state.image_url,
            })?,
        ),
        DriverEvent::PromptSubmitted {
            message,
            turn_id,
            message_id,
        } => (
            "promptSubmitted",
            json!({ "message": message, "turnId": turn_id, "messageId": message_id }),
        ),
        DriverEvent::SteerAccepted { message } => ("steerAccepted", json!({ "message": message })),
        DriverEvent::SteerRejected { message, reason } => (
            "steerRejected",
            json!({ "message": message, "reason": reason }),
        ),
        DriverEvent::ProviderQueue {
            steering,
            follow_up,
        } => (
            "providerQueue",
            json!({ "steering": steering, "followUp": follow_up }),
        ),
        DriverEvent::QueuedMessagesRetracted { messages } => {
            ("queuedMessagesRetracted", json!({ "messages": messages }))
        }
        DriverEvent::UsageUpdated {
            context_tokens,
            context_window,
        } => (
            "usageUpdated",
            json!({
                "contextTokens": context_tokens,
                "contextWindow": context_window,
            }),
        ),
        DriverEvent::PlanUsageUpdated(usage) => ("planUsageUpdated", serde_json::to_value(usage)?),
        DriverEvent::GoalUpdated(goal) => ("goalUpdated", serde_json::to_value(goal)?),
        DriverEvent::TurnFinished {
            success,
            summary,
            interrupted,
        } => (
            "turnFinished",
            json!({
                "success": success,
                "summary": summary,
                "interrupted": interrupted,
            }),
        ),
        DriverEvent::Error(error) => ("error", Value::String(error)),
        DriverEvent::ProcessExited => ("processExited", Value::Null),
    };
    Ok(WireDriverEvent::new(kind, payload))
}

pub fn event_from_wire(event: WireDriverEvent) -> anyhow::Result<DriverEvent> {
    let payload = event.payload;
    Ok(match event.kind.as_str() {
        "connected" => DriverEvent::Connected {
            provider_cursor: serde_json::from_value(payload)?,
        },
        "agentPresetSelected" => DriverEvent::AgentPresetSelected(serde_json::from_value(payload)?),
        "autoTitleUpdated" => DriverEvent::AutoTitleUpdated(serde_json::from_value(payload)?),
        "availableCommands" => DriverEvent::AvailableCommands(serde_json::from_value(payload)?),
        "turnStarted" => DriverEvent::TurnStarted,
        "turnParked" => DriverEvent::TurnParked,
        "textDelta" => DriverEvent::TextDelta(serde_json::from_value(payload)?),
        "reasoningDelta" => DriverEvent::ReasoningDelta(serde_json::from_value(payload)?),
        "activity" => {
            let activity: ActivityWire = serde_json::from_value(payload)?;
            DriverEvent::Activity {
                id: activity.id,
                kind: activity.kind,
                title: activity.title,
                detail: activity.detail,
                complete: activity.complete,
            }
        }
        "richActivity" => DriverEvent::RichActivity(serde_json::from_value(payload)?),
        "backgroundWork" => DriverEvent::BackgroundWork(serde_json::from_value(payload)?),
        "extensionMessage" => {
            let message: ExtensionMessageWire = serde_json::from_value(payload)?;
            DriverEvent::ExtensionMessage {
                custom_type: message.custom_type,
                text: message.text,
                display: message.display,
            }
        }
        "extensionNotification" => {
            let notification: ExtensionNotificationWire = serde_json::from_value(payload)?;
            DriverEvent::ExtensionNotification {
                message: notification.message,
                severity: notification.severity,
            }
        }
        "extensionStatus" => {
            let status: ExtensionStatusWire = serde_json::from_value(payload)?;
            DriverEvent::ExtensionStatus {
                key: status.key,
                text: status.text,
            }
        }
        "extensionWidget" => {
            let widget: ExtensionWidgetWire = serde_json::from_value(payload)?;
            DriverEvent::ExtensionWidget {
                key: widget.key,
                lines: widget.lines,
                placement: widget.placement,
            }
        }
        "extensionTitle" => {
            let title: ExtensionTitleWire = serde_json::from_value(payload)?;
            DriverEvent::ExtensionTitle { title: title.title }
        }
        "extensionEditorText" => {
            let editor: ExtensionEditorTextWire = serde_json::from_value(payload)?;
            DriverEvent::ExtensionEditorText { text: editor.text }
        }
        "permission" => {
            let permission: PermissionWire = serde_json::from_value(payload)?;
            DriverEvent::Permission {
                request_id: permission.request_id,
                title: permission.title,
                detail: permission.detail,
                options: permission.options,
            }
        }
        "userInputRequested" => {
            let request: UserInputWire = serde_json::from_value(payload)?;
            DriverEvent::UserInputRequested {
                request_id: request.request_id,
                questions: request.questions,
            }
        }
        "computerUseUpdated" => {
            let state: ComputerUseWire = serde_json::from_value(payload)?;
            DriverEvent::ComputerUseUpdated(ComputerUseState {
                target: state.target,
                phase: state.phase,
                visible: state.visible,
                image_url: state.image_url,
            })
        }
        "promptSubmitted" => {
            let submitted: SubmittedPromptWire = serde_json::from_value(payload)?;
            DriverEvent::PromptSubmitted {
                message: submitted.message,
                turn_id: submitted.turn_id,
                message_id: submitted.message_id,
            }
        }
        "steerAccepted" => {
            let steer: AcceptedSteerWire = serde_json::from_value(payload)?;
            DriverEvent::SteerAccepted {
                message: steer.message,
            }
        }
        "steerRejected" => {
            let steer: RejectedSteerWire = serde_json::from_value(payload)?;
            DriverEvent::SteerRejected {
                message: steer.message,
                reason: steer.reason,
            }
        }
        "providerQueue" => {
            let queue: ProviderQueueWire = serde_json::from_value(payload)?;
            DriverEvent::ProviderQueue {
                steering: queue.steering,
                follow_up: queue.follow_up,
            }
        }
        "queuedMessagesRetracted" => {
            let retracted: RetractedMessagesWire = serde_json::from_value(payload)?;
            DriverEvent::QueuedMessagesRetracted {
                messages: retracted.messages,
            }
        }
        "usageUpdated" => {
            let usage: UsageWire = serde_json::from_value(payload)?;
            DriverEvent::UsageUpdated {
                context_tokens: usage.context_tokens,
                context_window: usage.context_window,
            }
        }
        "planUsageUpdated" => DriverEvent::PlanUsageUpdated(serde_json::from_value(payload)?),
        "goalUpdated" => DriverEvent::GoalUpdated(serde_json::from_value(payload)?),
        "turnFinished" => {
            let finished: TurnFinishedWire = serde_json::from_value(payload)?;
            DriverEvent::TurnFinished {
                success: finished.success,
                summary: finished.summary,
                interrupted: finished.interrupted,
            }
        }
        "error" => DriverEvent::Error(serde_json::from_value(payload)?),
        "processExited" => DriverEvent::ProcessExited,
        kind => bail!("daemon sent an unsupported driver event {kind:?}"),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmittedPromptWire {
    message: String,
    turn_id: Uuid,
    message_id: Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityWire {
    id: Option<String>,
    kind: ActivityKind,
    title: String,
    detail: Option<String>,
    complete: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionMessageWire {
    custom_type: String,
    text: String,
    /// Absent on a payload written before the field existed; the message then
    /// behaves as one the provider marked for display.
    #[serde(default = "default_true")]
    display: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionNotificationWire {
    message: String,
    /// Absent on a payload written before the field existed; Pi's own default
    /// for a notification that does not say how serious it is.
    #[serde(default = "default_notification_severity")]
    severity: NotificationSeverity,
}

fn default_notification_severity() -> NotificationSeverity {
    NotificationSeverity::Info
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionStatusWire {
    key: String,
    /// Absent or null on a payload that clears the entry.
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionWidgetWire {
    key: String,
    /// Absent or null on a payload that clears the widget.
    #[serde(default)]
    lines: Option<Vec<String>>,
    #[serde(default = "default_widget_placement")]
    placement: ExtensionWidgetPlacement,
}

fn default_widget_placement() -> ExtensionWidgetPlacement {
    ExtensionWidgetPlacement::AboveEditor
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionTitleWire {
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionEditorTextWire {
    text: String,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionWire {
    request_id: String,
    title: String,
    detail: String,
    options: Vec<PermissionOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInputWire {
    request_id: String,
    questions: Vec<UserInputQuestion>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComputerUseWire {
    target: Option<ComputerTarget>,
    phase: ComputerUsePhase,
    visible: bool,
    image_url: Option<String>,
}

#[derive(Deserialize)]
struct AcceptedSteerWire {
    message: String,
}

#[derive(Deserialize)]
struct RejectedSteerWire {
    message: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderQueueWire {
    steering: Vec<String>,
    follow_up: Vec<String>,
}

#[derive(Deserialize)]
struct RetractedMessagesWire {
    messages: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageWire {
    context_tokens: Option<u64>,
    context_window: Option<u64>,
}

#[derive(Deserialize)]
struct TurnFinishedWire {
    success: bool,
    summary: Option<String>,
    /// Absent on a payload written before the field existed, and on web or
    /// mobile clients that never send it.
    #[serde(default)]
    interrupted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ThreadGoal, ThreadGoalStatus, UserInputOption, UserInputQuestion};

    #[test]
    fn goal_updates_round_trip_through_the_daemon_wire() {
        let wire = event_to_wire(DriverEvent::GoalUpdated(Some(ThreadGoal {
            objective: "Ship the feature".into(),
            status: ThreadGoalStatus::UsageLimited,
            token_budget: Some(50_000),
            tokens_used: 12_500,
            time_used_seconds: 90,
        })))
        .unwrap();
        assert_eq!(wire.kind, "goalUpdated");
        // The status spelling is Codex's own camelCase vocabulary.
        assert_eq!(wire.payload["status"], "usageLimited");

        let DriverEvent::GoalUpdated(Some(goal)) = event_from_wire(wire).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(goal.objective, "Ship the feature");
        assert_eq!(goal.status, ThreadGoalStatus::UsageLimited);
        assert_eq!(goal.token_budget, Some(50_000));

        let cleared = event_to_wire(DriverEvent::GoalUpdated(None)).unwrap();
        assert!(matches!(
            event_from_wire(cleared).unwrap(),
            DriverEvent::GoalUpdated(None)
        ));
    }

    #[test]
    fn the_provider_queue_and_its_retraction_round_trip_through_the_wire() {
        let queue = event_to_wire(DriverEvent::ProviderQueue {
            steering: vec!["stop".into()],
            follow_up: vec!["and also".into()],
        })
        .unwrap();
        assert_eq!(queue.kind, "providerQueue");
        let DriverEvent::ProviderQueue {
            steering,
            follow_up,
        } = event_from_wire(queue).unwrap()
        else {
            panic!("the queue report changed variants on the wire");
        };
        assert_eq!(steering, ["stop"]);
        assert_eq!(follow_up, ["and also"]);

        let retracted = event_to_wire(DriverEvent::QueuedMessagesRetracted {
            messages: vec!["and also".into()],
        })
        .unwrap();
        assert_eq!(retracted.kind, "queuedMessagesRetracted");
        let DriverEvent::QueuedMessagesRetracted { messages } = event_from_wire(retracted).unwrap()
        else {
            panic!("the retraction changed variants on the wire");
        };
        assert_eq!(messages, ["and also"]);
    }

    #[test]
    fn extension_messages_round_trip_and_default_to_displayed() {
        let wire = event_to_wire(DriverEvent::ExtensionMessage {
            custom_type: "subagent_control_notice".into(),
            text: "Workflow paused.".into(),
            display: false,
        })
        .unwrap();
        assert_eq!(wire.kind, "extensionMessage");

        let DriverEvent::ExtensionMessage {
            custom_type,
            text,
            display,
        } = event_from_wire(wire).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(custom_type, "subagent_control_notice");
        assert_eq!(text, "Workflow paused.");
        assert!(!display);

        // A payload written before the field existed renders as a notice.
        let legacy = crate::WireDriverEvent::new(
            "extensionMessage",
            json!({ "customType": "notice", "text": "hello" }),
        );
        let DriverEvent::ExtensionMessage { display, .. } = event_from_wire(legacy).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert!(display);
    }

    #[test]
    fn extension_surface_events_round_trip_and_default_on_decode() {
        // Every fire-and-forget surface an extension publishes has to survive
        // the daemon wire whole, or the app never sees what the extension
        // said: a notification's severity, a status entry's key and text, a
        // widget's lines and placement, and the title and editor text are all
        // the content, not decoration around it.
        let wire = event_to_wire(DriverEvent::ExtensionNotification {
            message: "Subagent failed: **code-auditor**".into(),
            severity: NotificationSeverity::Warning,
        })
        .unwrap();
        assert_eq!(wire.kind, "extensionNotification");
        assert_eq!(wire.payload["severity"], "warning");
        let DriverEvent::ExtensionNotification { message, severity } =
            event_from_wire(wire).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(message, "Subagent failed: **code-auditor**");
        assert_eq!(severity, NotificationSeverity::Warning);

        let wire = event_to_wire(DriverEvent::ExtensionStatus {
            key: "subagent-slash".into(),
            text: Some("running...".into()),
        })
        .unwrap();
        assert_eq!(wire.kind, "extensionStatus");
        let DriverEvent::ExtensionStatus { key, text } = event_from_wire(wire).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(key, "subagent-slash");
        assert_eq!(text.as_deref(), Some("running..."));

        // The clear an extension sends is the same method with no text, and it
        // has to stay distinguishable from a set on the wire.
        let wire = event_to_wire(DriverEvent::ExtensionStatus {
            key: "subagent-slash".into(),
            text: None,
        })
        .unwrap();
        let DriverEvent::ExtensionStatus { text, .. } = event_from_wire(wire).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert!(text.is_none());

        let wire = event_to_wire(DriverEvent::ExtensionWidget {
            key: "subagent-fleet".into(),
            lines: Some(vec!["2 running".into()]),
            placement: ExtensionWidgetPlacement::BelowEditor,
        })
        .unwrap();
        assert_eq!(wire.kind, "extensionWidget");
        assert_eq!(wire.payload["placement"], "belowEditor");
        let DriverEvent::ExtensionWidget {
            key,
            lines,
            placement,
        } = event_from_wire(wire).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(key, "subagent-fleet");
        assert_eq!(lines.as_deref(), Some(["2 running".to_owned()].as_slice()));
        assert_eq!(placement, ExtensionWidgetPlacement::BelowEditor);

        let wire = event_to_wire(DriverEvent::ExtensionTitle {
            title: "pi - waku".into(),
        })
        .unwrap();
        assert_eq!(wire.kind, "extensionTitle");
        let DriverEvent::ExtensionTitle { title } = event_from_wire(wire).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(title, "pi - waku");

        let wire = event_to_wire(DriverEvent::ExtensionEditorText {
            text: "review the diff".into(),
        })
        .unwrap();
        assert_eq!(wire.kind, "extensionEditorText");
        let DriverEvent::ExtensionEditorText { text } = event_from_wire(wire).unwrap() else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(text, "review the diff");

        // A payload written by an older daemon carries no severity and no
        // widget placement; both behave as Pi's own defaults say they should.
        let legacy = crate::WireDriverEvent::new(
            "extensionNotification",
            json!({ "message": "Model registry refreshed" }),
        );
        let DriverEvent::ExtensionNotification { severity, .. } = event_from_wire(legacy).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(severity, NotificationSeverity::Info);

        let legacy = crate::WireDriverEvent::new(
            "extensionWidget",
            json!({ "key": "subagent-fleet", "lines": ["2 running"] }),
        );
        let DriverEvent::ExtensionWidget { placement, .. } = event_from_wire(legacy).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(placement, ExtensionWidgetPlacement::AboveEditor);
    }

    #[test]
    fn structured_user_input_round_trips_through_the_daemon_wire() {
        let wire = event_to_wire(DriverEvent::UserInputRequested {
            request_id: "request-1".into(),
            questions: vec![UserInputQuestion {
                id: "deployment".into(),
                header: "Environment".into(),
                question: "Where should this deploy?".into(),
                options: vec![UserInputOption {
                    label: "Preview".into(),
                    description: Some("Create a preview deployment".into()),
                }],
                multi_select: false,
            }],
        })
        .unwrap();
        assert_eq!(wire.kind, "userInputRequested");

        let DriverEvent::UserInputRequested {
            request_id,
            questions,
        } = event_from_wire(wire).unwrap()
        else {
            panic!("the event changed variants during its wire round trip");
        };
        assert_eq!(request_id, "request-1");
        assert_eq!(questions[0].id, "deployment");
        assert_eq!(questions[0].options[0].label, "Preview");
    }
}
