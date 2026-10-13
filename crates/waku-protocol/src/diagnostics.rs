//! Structured diagnostic records for the instance's own explanation.
//!
//! The app, the daemon, and provider processes each emit
//! [`DiagnosticRecord`]s: a timestamp, a level, the process they came from,
//! the subsystem that produced them, a message, structured fields, and the
//! task/turn/message/permission identity they belong to. The aggregating
//! daemon places each record into the instance's one ordered stream, stamping
//! it as a [`DiagnosticEvent`] with the stream's epoch and a monotonic
//! sequence. Clients read that stream after a [`DiagnosticCursor`], so a
//! dropped or resumed connection loses no observation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

use crate::model::{DriverEvent, ProviderKind};

/// Severity of one diagnostic record, ordered from most to least detailed so
/// a level filter is a comparison.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// The longest one record's message may be, in bytes. Provider CLIs can print
/// arbitrarily long lines and a request error can embed one; one record must
/// not dwarf a ring slot or the log file's size bound. A longer message is
/// truncated to this many bytes plus the truncation marker, so a shortened
/// line is never read as the whole one.
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 8 * 1024;

/// How many recent records the instance's stream retains for replay, and how
/// many a client buffers before anything subscribes. One bound for the daemon
/// and its clients, so a drift cannot silently truncate a handshake replay.
pub const MAX_DIAGNOSTIC_EVENTS: usize = 2048;

/// The process a record came from.
///
/// The app and the daemon are one each per instance; `runtime_id` is a
/// provider's process identity — one driver process per runtime — so it
/// distinguishes a provider restarted for the same task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(
    tag = "process",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DiagnosticOrigin {
    App {
        pid: u32,
        version: String,
    },
    Daemon {
        pid: u32,
    },
    Provider {
        provider: ProviderKind,
        runtime_id: Uuid,
    },
}

/// The identities a record belongs to, when it has them.
///
/// Flattened into its record, so a record for one turn can be found by
/// grepping a single field out of the NDJSON files or querying the daemon.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCorrelation {
    /// The task (session) the record belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<Uuid>,
    /// A provider permission or user-input request the record belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// One structured record, before the aggregating daemon has sequenced it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticRecord {
    /// Milliseconds since the Unix epoch, stamped by the emitting process.
    pub timestamp_ms: u64,
    pub level: DiagnosticLevel,
    pub origin: DiagnosticOrigin,
    /// The subsystem within the process, for example `turn` or `driver`.
    pub source: String,
    /// The human-readable line.
    pub message: String,
    /// Structured detail; null when the message is the whole record.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub fields: Value,
    #[serde(flatten)]
    pub correlation: DiagnosticCorrelation,
}

impl DiagnosticRecord {
    pub fn new(
        level: DiagnosticLevel,
        origin: DiagnosticOrigin,
        source: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            timestamp_ms: crate::model::unix_time_millis(),
            level,
            origin,
            source: source.into(),
            message: bounded_message(message.into()),
            fields: Value::Null,
            correlation: DiagnosticCorrelation::default(),
        }
    }

    /// Attach structured detail. Every string in it is bounded like the
    /// message, so one record cannot outgrow the retention bound.
    pub fn with_fields(mut self, fields: Value) -> Self {
        self.fields = bounded_value(fields);
        self
    }

    pub fn with_correlation(mut self, correlation: DiagnosticCorrelation) -> Self {
        self.correlation = correlation;
        self
    }
}

fn bounded_message(message: String) -> String {
    if message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = message[..end].to_owned();
    bounded.push('…');
    bounded
}

fn bounded_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(bounded_message(text)),
        Value::Array(items) => Value::Array(items.into_iter().map(bounded_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, bounded_value(value)))
                .collect(),
        ),
        other => other,
    }
}

/// One record placed in the instance's stream by the aggregating daemon.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEvent {
    /// Identifies the daemon process that assigned `sequence`.
    pub epoch: Uuid,
    /// Monotonic within the instance's stream; 1 for the daemon's first
    /// record.
    pub sequence: u64,
    #[serde(flatten)]
    pub record: DiagnosticRecord,
}

/// A client's position in the instance's diagnostic stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCursor {
    /// The epoch the sequence was minted under; a record from another epoch
    /// cannot be compared with it.
    pub epoch: Uuid,
    pub sequence: u64,
}

/// Where a client's cursor fell behind the retained stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticExpiry {
    /// The epoch the retained stream belongs to.
    pub epoch: Uuid,
    /// The sequence the client asked for.
    pub requested_sequence: u64,
    /// The oldest sequence still retained.
    pub oldest_sequence: u64,
}

/// One driver event's place in the instance's diagnostic stream.
///
/// The app and the daemon both observe driver events; classifying them once
/// here keeps the two views of one event — the app's and the provider's —
/// carrying the same milestone name and level.
pub struct DriverMilestone {
    pub level: DiagnosticLevel,
    pub source: &'static str,
    pub message: &'static str,
    pub fields: Value,
    /// The turn (and user message) the event itself names, when it carries
    /// one; the caller supplies the active turn otherwise.
    pub turn_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub request_id: Option<String>,
}

impl DriverMilestone {
    /// Build the record for this milestone, with the milestone's own turn,
    /// message, and request identity overriding the caller's correlation.
    /// Both observers of one driver event — the app and the provider — share
    /// this assembly, so their records cannot drift apart.
    pub fn record(
        self,
        origin: DiagnosticOrigin,
        mut correlation: DiagnosticCorrelation,
    ) -> DiagnosticRecord {
        if self.turn_id.is_some() {
            correlation.turn_id = self.turn_id;
        }
        if self.message_id.is_some() {
            correlation.message_id = self.message_id;
        }
        if self.request_id.is_some() {
            correlation.request_id = self.request_id;
        }
        DiagnosticRecord::new(self.level, origin, self.source, self.message)
            .with_fields(self.fields)
            .with_correlation(correlation)
    }
}

/// Classify one driver event for the diagnostic stream.
///
/// `None` means the event is high-volume detail — streamed deltas, provider
/// queue reports, subagent fleet snapshots — that a bounded stream cannot
/// keep without evicting the milestones that explain a turn.
pub fn driver_milestone(event: &DriverEvent) -> Option<DriverMilestone> {
    let milestone = |level, source, message, fields| DriverMilestone {
        level,
        source,
        message,
        fields,
        turn_id: None,
        message_id: None,
        request_id: None,
    };
    let milestone = match event {
        DriverEvent::Connected { .. } => {
            milestone(DiagnosticLevel::Info, "driver", "connected", Value::Null)
        }
        DriverEvent::PromptSubmitted {
            turn_id,
            message_id,
            ..
        } => DriverMilestone {
            level: DiagnosticLevel::Info,
            source: "driver",
            message: "prompt submitted",
            fields: Value::Null,
            turn_id: Some(*turn_id),
            message_id: Some(*message_id),
            request_id: None,
        },
        DriverEvent::TurnStarted => {
            milestone(DiagnosticLevel::Info, "driver", "turn started", Value::Null)
        }
        DriverEvent::TurnParked => {
            milestone(DiagnosticLevel::Info, "driver", "turn parked", Value::Null)
        }
        // A provider reports one activity through many progress updates (ACP's
        // `tool_call_update`, for example) and marks the final one complete.
        // Only that final update is a milestone: recording every tick would
        // put provider-rate work on the UI thread's drain and label one
        // activity "started" over and over.
        DriverEvent::Activity {
            complete: false, ..
        }
        | DriverEvent::RichActivity(crate::model::ActivityItem {
            complete: false, ..
        }) => {
            return None;
        }
        DriverEvent::Activity {
            id, kind, title, ..
        } => milestone(
            DiagnosticLevel::Info,
            "tool",
            "completed",
            serde_json::json!({
                "activityId": id,
                "kind": kind,
                "title": title,
            }),
        ),
        DriverEvent::RichActivity(item) => milestone(
            DiagnosticLevel::Info,
            "tool",
            "completed",
            serde_json::json!({
                "activityId": item.id,
                "kind": item.kind,
                "title": item.title,
                "tool": item.tool_name,
            }),
        ),
        DriverEvent::TurnFinished {
            success,
            summary,
            interrupted,
        } => milestone(
            if *success {
                DiagnosticLevel::Info
            } else {
                DiagnosticLevel::Error
            },
            "driver",
            "turn finished",
            serde_json::json!({
                "success": success,
                "interrupted": interrupted,
                "summary": summary,
            }),
        ),
        DriverEvent::Error(error) => milestone(
            DiagnosticLevel::Error,
            "driver",
            "error",
            serde_json::json!({ "error": error }),
        ),
        DriverEvent::ProcessExited => milestone(
            DiagnosticLevel::Warn,
            "driver",
            "provider exited",
            Value::Null,
        ),
        DriverEvent::Permission {
            request_id, title, ..
        } => DriverMilestone {
            level: DiagnosticLevel::Info,
            source: "permission",
            message: "requested",
            fields: serde_json::json!({ "title": title }),
            turn_id: None,
            message_id: None,
            request_id: Some(request_id.clone()),
        },
        DriverEvent::UserInputRequested { request_id, .. } => DriverMilestone {
            level: DiagnosticLevel::Info,
            source: "permission",
            message: "user input requested",
            fields: Value::Null,
            turn_id: None,
            message_id: None,
            request_id: Some(request_id.clone()),
        },
        DriverEvent::SteerAccepted { .. } => milestone(
            DiagnosticLevel::Info,
            "driver",
            "steer accepted",
            Value::Null,
        ),
        DriverEvent::SteerRejected { reason, .. } => milestone(
            DiagnosticLevel::Warn,
            "driver",
            "steer rejected",
            serde_json::json!({ "reason": reason }),
        ),
        _ => return None,
    };
    Some(milestone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_record_round_trips_with_its_correlation_flattened() {
        let turn_id = Uuid::from_u128(7);
        let task_id = Uuid::from_u128(8);
        let record = DiagnosticRecord::new(
            DiagnosticLevel::Info,
            DiagnosticOrigin::App {
                pid: 42,
                version: "test".into(),
            },
            "turn",
            "started",
        )
        .with_fields(json!({ "model": "gpt-5" }))
        .with_correlation(DiagnosticCorrelation {
            task_id: Some(task_id),
            turn_id: Some(turn_id),
            ..DiagnosticCorrelation::default()
        });

        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["level"], "info");
        assert_eq!(json["origin"]["process"], "app");
        assert_eq!(json["origin"]["pid"], 42);
        assert_eq!(json["source"], "turn");
        assert_eq!(json["taskId"], task_id.to_string());
        assert_eq!(json["turnId"], turn_id.to_string());
        assert!(json.get("messageId").is_none());
        assert_eq!(json["fields"]["model"], "gpt-5");

        let decoded: DiagnosticRecord = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn an_event_round_trips_with_its_record_flattened() {
        let epoch = Uuid::from_u128(1);
        let event = DiagnosticEvent {
            epoch,
            sequence: 9,
            record: DiagnosticRecord::new(
                DiagnosticLevel::Warn,
                DiagnosticOrigin::Daemon { pid: 3 },
                "request",
                "slow",
            ),
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["epoch"], epoch.to_string());
        assert_eq!(json["sequence"], 9);
        assert_eq!(json["level"], "warn");
        assert_eq!(json["origin"]["process"], "daemon");

        let decoded: DiagnosticEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn driver_milestones_keep_the_turn_correlation_they_name() {
        let turn_id = Uuid::from_u128(11);
        let message_id = Uuid::from_u128(12);
        let milestone = driver_milestone(&DriverEvent::PromptSubmitted {
            message: "ship it".into(),
            turn_id,
            message_id,
        })
        .unwrap();
        assert_eq!(milestone.message, "prompt submitted");
        assert_eq!(milestone.turn_id, Some(turn_id));
        assert_eq!(milestone.message_id, Some(message_id));

        let milestone = driver_milestone(&DriverEvent::Error("boom".into())).unwrap();
        assert_eq!(milestone.level, DiagnosticLevel::Error);
        assert_eq!(milestone.fields["error"], "boom");

        let milestone = driver_milestone(&DriverEvent::Activity {
            id: Some("tool-1".into()),
            kind: crate::model::ActivityKind::Tool,
            title: "run tests".into(),
            detail: None,
            complete: true,
        })
        .unwrap();
        assert_eq!(milestone.message, "completed");
        assert_eq!(milestone.fields["title"], "run tests");

        assert!(
            driver_milestone(&DriverEvent::Activity {
                id: Some("tool-1".into()),
                kind: crate::model::ActivityKind::Tool,
                title: "run tests".into(),
                detail: None,
                complete: false,
            })
            .is_none(),
            "a provider's progress tick is not a milestone"
        );

        assert!(driver_milestone(&DriverEvent::TextDelta("chunk".into())).is_none());
        assert!(driver_milestone(&DriverEvent::ReasoningDelta("think".into())).is_none());
    }

    #[test]
    fn an_oversized_message_is_truncated_with_a_marker() {
        let long = "x".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES * 4);
        let record = DiagnosticRecord::new(
            DiagnosticLevel::Debug,
            DiagnosticOrigin::Daemon { pid: 1 },
            "stderr",
            long,
        );
        assert!(record.message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES + 4);
        assert!(record.message.ends_with('…'));

        // Truncation lands on a character boundary, so multi-byte text never
        // produces an invalid string.
        let multibyte = "é".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES);
        let record = DiagnosticRecord::new(
            DiagnosticLevel::Debug,
            DiagnosticOrigin::Daemon { pid: 1 },
            "stderr",
            multibyte,
        );
        assert!(record.message.ends_with('…'));

        // Structured fields are bounded the same way, so no single record can
        // outgrow the retention bound.
        let record = DiagnosticRecord::new(
            DiagnosticLevel::Error,
            DiagnosticOrigin::Daemon { pid: 1 },
            "driver",
            "error",
        )
        .with_fields(serde_json::json!({
            "error": "y".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES * 2),
            "items": ["z".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES * 2)],
        }));
        assert!(record.fields["error"].as_str().unwrap().len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES + 4);
        assert!(
            record.fields["items"][0].as_str().unwrap().len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES + 4
        );
    }

    #[test]
    fn a_milestone_records_its_own_turn_and_message_over_the_caller_correlation() {
        let event_turn = Uuid::from_u128(20);
        let event_message = Uuid::from_u128(21);
        let milestone = driver_milestone(&DriverEvent::PromptSubmitted {
            message: "ship it".into(),
            turn_id: event_turn,
            message_id: event_message,
        })
        .unwrap();
        let record = milestone.record(
            DiagnosticOrigin::Provider {
                provider: ProviderKind::Pi,
                runtime_id: Uuid::from_u128(22),
            },
            DiagnosticCorrelation {
                task_id: Some(Uuid::from_u128(23)),
                turn_id: Some(Uuid::from_u128(24)),
                ..DiagnosticCorrelation::default()
            },
        );
        assert_eq!(record.correlation.task_id, Some(Uuid::from_u128(23)));
        assert_eq!(record.correlation.turn_id, Some(event_turn));
        assert_eq!(record.correlation.message_id, Some(event_message));
        assert_eq!(record.source, "driver");

        // A milestone that names no message keeps the caller's: the active
        // turn's prompt belongs to every record of that turn.
        let caller_message = Uuid::from_u128(25);
        let milestone = driver_milestone(&DriverEvent::TurnStarted).unwrap();
        let record = milestone.record(
            DiagnosticOrigin::Provider {
                provider: ProviderKind::Pi,
                runtime_id: Uuid::from_u128(22),
            },
            DiagnosticCorrelation {
                task_id: Some(Uuid::from_u128(23)),
                message_id: Some(caller_message),
                ..DiagnosticCorrelation::default()
            },
        );
        assert_eq!(record.correlation.message_id, Some(caller_message));
    }

    #[test]
    fn levels_order_from_trace_to_error() {
        let mut levels = [
            DiagnosticLevel::Error,
            DiagnosticLevel::Info,
            DiagnosticLevel::Trace,
            DiagnosticLevel::Warn,
            DiagnosticLevel::Debug,
        ];
        levels.sort();
        assert_eq!(
            levels,
            [
                DiagnosticLevel::Trace,
                DiagnosticLevel::Debug,
                DiagnosticLevel::Info,
                DiagnosticLevel::Warn,
                DiagnosticLevel::Error,
            ]
        );
    }
}
