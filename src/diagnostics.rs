//! The app's structured diagnostic records.
//!
//! The app records what only it can see — the turn lifecycle as the transcript
//! experiences it, the semantic actions the bridge performs, and the driver
//! events it incorporated — as [`DiagnosticRecord`]s and forwards them to the
//! daemon, which sequences them into the instance's one stream. Records are
//! built by plain functions so the correlation they carry is testable without
//! a window, and [`record`] is the thin wiring to the daemon connection.
//!
//! Recording is debug-build behavior: release builds neither send records nor
//! expose the queries over them.

use gpui::App;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::app::StreamPhase;
use crate::model::{AgentSession, DriverEvent, TurnStatus};
use crate::rpa::protocol::{Action, Request, Target};
use waku_client::{DiagnosticCorrelation, DiagnosticLevel, DiagnosticOrigin, DiagnosticRecord};

/// The identity every record from this process carries.
fn origin() -> DiagnosticOrigin {
    DiagnosticOrigin::App {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Forward one record to the daemon's instance stream. The stream is an
/// observation, so a record the link cannot take is reported here instead of
/// failing the call site that produced it — the transcript's own work must
/// not depend on a diagnostic listener. This is the one debug gate on the
/// app side: the builders are closures, so a release build runs none of the
/// classification, correlation, or JSON work.
pub(crate) fn record(cx: &App, build: impl FnOnce() -> Option<DiagnosticRecord>) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(record) = build() else {
        return;
    };
    let Some(client) = crate::daemon::connected(cx).map(|daemon| daemon.client()) else {
        return;
    };
    if let Err(error) = client.record_diagnostic(record) {
        eprintln!("waku: diagnostic record dropped: {error}");
    }
}

/// The correlation a record for `session` carries: its task, its active turn,
/// and the message that opened that turn.
pub(crate) fn correlation(session: &AgentSession) -> DiagnosticCorrelation {
    DiagnosticCorrelation {
        task_id: Some(session.id),
        turn_id: session.active_turn_id(),
        message_id: session.active_turn_user_message_id(),
        ..DiagnosticCorrelation::default()
    }
}

fn turn_record(
    session: &AgentSession,
    level: DiagnosticLevel,
    message: &str,
    fields: Value,
) -> DiagnosticRecord {
    DiagnosticRecord::new(level, origin(), "turn", message)
        .with_fields(fields)
        .with_correlation(correlation(session))
}

/// A submission was accepted and its turn began.
pub(crate) fn turn_submitted(session: &AgentSession) -> DiagnosticRecord {
    DiagnosticRecord::new(DiagnosticLevel::Info, origin(), "turn", "submitted")
        .with_fields(json!({
            "provider": session.provider.id(),
            "model": session.model,
            "turnCount": session.turns.len(),
        }))
        .with_correlation(correlation(session))
}

/// The first streamed output of the turn's reply.
pub(crate) fn turn_streaming(session: &AgentSession, phase: StreamPhase) -> DiagnosticRecord {
    let phase = match phase {
        StreamPhase::Text => "text",
        StreamPhase::Reasoning => "reasoning",
        StreamPhase::Activity => "activity",
    };
    turn_record(
        session,
        DiagnosticLevel::Info,
        "streaming",
        json!({ "phase": phase }),
    )
}

/// Whether the handler will apply this milestone's event. Activity updates
/// are output — the handler drops a stale turn's update — while settlements,
/// errors, and permissions apply to the session regardless, so their gate is
/// never consulted.
pub(crate) fn milestone_applies(
    event: &DriverEvent,
    accepts_output: impl FnOnce() -> bool,
) -> bool {
    !matches!(
        event,
        DriverEvent::Activity { .. } | DriverEvent::RichActivity(_)
    ) || accepts_output()
}

/// The turn completed through every settlement path.
pub(crate) fn turn_settled(
    session: &AgentSession,
    turn_id: Uuid,
    turn_count: usize,
    status: TurnStatus,
) -> DiagnosticRecord {
    let (level, message, outcome) = match status {
        TurnStatus::Completed => (DiagnosticLevel::Info, "settled", "completed"),
        TurnStatus::Interrupted => (DiagnosticLevel::Info, "interrupted", "interrupted"),
        TurnStatus::Failed => (DiagnosticLevel::Error, "failed", "failed"),
        TurnStatus::Running => (DiagnosticLevel::Info, "settling", "running"),
    };
    let mut correlation = correlation(session);
    correlation.turn_id = Some(turn_id);
    correlation.message_id = session.user_message_id_for_turn(turn_id);
    DiagnosticRecord::new(level, origin(), "turn", message)
        .with_fields(json!({
            "turnCount": turn_count,
            "outcome": outcome,
        }))
        .with_correlation(correlation)
}

/// A submission was accepted but never reached a provider: its preparation
/// failed and the turn was unwound, so the error explains why.
pub(crate) fn turn_unwound(session: &AgentSession, error: &anyhow::Error) -> DiagnosticRecord {
    turn_record(
        session,
        DiagnosticLevel::Error,
        "unwound",
        json!({ "error": error.to_string() }),
    )
}

/// The app's record for one classified driver event. The milestone's own
/// turn and message identity override the session's active turn, so an event
/// that names its turn is recorded against that one.
pub(crate) fn milestone_record(
    session: &AgentSession,
    milestone: waku_client::DriverMilestone,
) -> DiagnosticRecord {
    milestone.record(origin(), correlation(session))
}

/// The record for one semantic action the bridge performed. The invocation is
/// recorded before it runs, so a call that never returns still explains what
/// was asked for. The correlation is the task and turn the workspace showed
/// at invocation — resolved lazily, so a read-only request that is never
/// recorded does not pay for it. An action whose target or arguments name a
/// different task keeps that identity in its fields.
pub(crate) fn action(
    request: &Request,
    correlation: impl FnOnce() -> DiagnosticCorrelation,
) -> Option<DiagnosticRecord> {
    let (message, fields) = match request {
        // Reading the app is observation, not an action; recording every
        // snapshot would fill the stream with the caller's polling.
        Request::Describe | Request::Snapshot => return None,
        Request::Act { action, target } => (
            "act",
            json!({
                "action": match action {
                    Action::Click => "click",
                    Action::Focus => "focus",
                },
                "target": target_name(target),
            }),
        ),
        Request::Type { text, target } => (
            "type",
            json!({
                "chars": text.chars().count(),
                "target": target.as_ref().map(target_name),
            }),
        ),
        Request::Key { chord } => ("key", json!({ "chord": chord })),
        Request::Invoke { verb, args } => ("invoke", json!({ "verb": verb, "args": args })),
        Request::Wait {
            condition,
            timeout_ms,
        } => (
            "wait",
            json!({ "condition": condition.describe(), "timeoutMs": timeout_ms }),
        ),
    };
    Some(
        DiagnosticRecord::new(DiagnosticLevel::Info, origin(), "action", message)
            .with_fields(fields)
            .with_correlation(correlation()),
    )
}

fn target_name(target: &Target) -> String {
    match target {
        Target::Node(node) => format!("node:{node}"),
        Target::Id(id) => id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ActivityKind, MessageRole, ProviderKind};

    /// A whole turn's records are complete at the default level: submission,
    /// provider start, first streamed output, tool activity, and settlement
    /// each carry the task and turn they belong to, and nothing a default
    /// reader needs is below the default threshold.
    #[test]
    fn default_level_records_cover_a_whole_turn() {
        fn driver_record(session: &AgentSession, event: &DriverEvent) -> Option<DiagnosticRecord> {
            waku_client::driver_milestone(event)
                .map(|milestone| milestone_record(session, milestone))
        }

        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        let turn_id = session.begin_turn("run the fan-out");
        session.mark_active_turn_provider_started();

        let mut records = vec![turn_submitted(&session)];
        records.push(driver_record(&session, &DriverEvent::TurnStarted).unwrap());
        records.push(turn_streaming(&session, StreamPhase::Text));
        records.push(
            driver_record(
                &session,
                &DriverEvent::Activity {
                    id: Some("tool-1".into()),
                    kind: ActivityKind::Tool,
                    title: "run tests".into(),
                    detail: None,
                    complete: true,
                },
            )
            .unwrap(),
        );
        let finished = session.finish_active_turn(TurnStatus::Completed).unwrap();
        records.push(turn_settled(
            &session,
            finished.0,
            finished.1,
            TurnStatus::Completed,
        ));

        let phases = records
            .iter()
            .map(|record| record.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            phases,
            [
                "submitted",
                "turn started",
                "streaming",
                "completed",
                "settled"
            ]
        );
        for record in &records {
            assert!(
                record.level >= DiagnosticLevel::Info,
                "`{}` is below the default level",
                record.message
            );
            assert_eq!(record.correlation.task_id, Some(session.id));
            assert_eq!(record.correlation.turn_id, Some(turn_id));
        }
        assert_eq!(
            records[0].correlation.message_id,
            session
                .messages
                .iter()
                .find(|message| message.role == MessageRole::User)
                .map(|message| message.id),
            "the submission names the user message it created"
        );
        assert_eq!(records[0].fields["provider"], json!("pi"));
        assert_eq!(records[3].fields["title"], "run tests");
    }

    /// A provider failure is an error record carrying the failure text, so one
    /// query for the turn's errors explains what went wrong.
    #[test]
    fn a_driver_error_is_an_error_carrying_its_text() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.begin_turn("ship it");
        let record = milestone_record(
            &session,
            waku_client::driver_milestone(&DriverEvent::Error("codex: rate limit reached".into()))
                .unwrap(),
        );
        assert_eq!(record.level, DiagnosticLevel::Error);
        assert_eq!(record.source, "driver");
        assert_eq!(record.fields["error"], "codex: rate limit reached");
        assert_eq!(record.correlation.task_id, Some(session.id));
    }

    /// High-volume events stay out of the stream; recording every delta would
    /// evict the milestones from a bounded ring.
    #[test]
    fn deltas_and_fleet_snapshots_are_not_records() {
        assert!(waku_client::driver_milestone(&DriverEvent::TextDelta("chunk".into())).is_none());
        assert!(
            waku_client::driver_milestone(&DriverEvent::ReasoningDelta("think".into())).is_none()
        );
        assert!(
            waku_client::driver_milestone(&DriverEvent::SubagentFleet(
                waku_client::subagent::SubagentFleet::default()
            ))
            .is_none()
        );
    }

    /// Activity milestones obey the handler's output gate; a settlement's gate
    /// is never consulted because the handler always applies it.
    #[test]
    fn activity_milestones_obey_the_output_gate() {
        let activity = DriverEvent::Activity {
            id: None,
            kind: ActivityKind::Tool,
            title: "run tests".into(),
            detail: None,
            complete: true,
        };
        assert!(!milestone_applies(&activity, || false));
        assert!(milestone_applies(&activity, || true));
        let mut consulted = false;
        assert!(milestone_applies(&DriverEvent::TurnStarted, || {
            consulted = true;
            false
        }));
        assert!(!consulted);
    }

    #[test]
    fn an_action_names_its_target_verb_and_arguments() {
        let click = action(
            &Request::Act {
                action: Action::Click,
                target: Target::Id("composer.send".into()),
            },
            || DiagnosticCorrelation {
                task_id: Some(Uuid::from_u128(30)),
                turn_id: Some(Uuid::from_u128(31)),
                ..DiagnosticCorrelation::default()
            },
        )
        .unwrap();
        assert_eq!(click.source, "action");
        assert_eq!(click.message, "act");
        assert_eq!(click.fields["action"], "click");
        assert_eq!(click.fields["target"], "composer.send");
        assert_eq!(click.correlation.task_id, Some(Uuid::from_u128(30)));
        assert_eq!(click.correlation.turn_id, Some(Uuid::from_u128(31)));

        let invoke = action(
            &Request::Invoke {
                verb: "openTask".into(),
                args: json!({ "id": "task-1" }),
            },
            DiagnosticCorrelation::default,
        )
        .unwrap();
        assert_eq!(invoke.message, "invoke");
        assert_eq!(invoke.fields["verb"], "openTask");
        assert_eq!(invoke.fields["args"]["id"], "task-1");
        assert_eq!(invoke.correlation.task_id, None);
    }

    /// Reading the app is not an action: observations the caller polls must
    /// not fill the stream, and their record — including the correlation
    /// lookup — is never built.
    #[test]
    fn read_only_requests_are_not_action_records() {
        let mut looked_up = false;
        let record = action(&Request::Snapshot, || {
            looked_up = true;
            DiagnosticCorrelation::default()
        });
        assert!(record.is_none());
        assert!(!looked_up, "the correlation is resolved only for records");
    }

    #[test]
    fn a_failed_turn_settles_at_error_level() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let turn_id = session.begin_turn("break it");
        let finished = session.finish_active_turn(TurnStatus::Failed).unwrap();
        let record = turn_settled(&session, finished.0, finished.1, TurnStatus::Failed);
        assert_eq!(record.level, DiagnosticLevel::Error);
        assert_eq!(record.message, "failed");
        assert_eq!(record.correlation.turn_id, Some(turn_id));
        assert_eq!(record.fields["outcome"], json!("failed"));
    }

    #[test]
    fn an_unwound_submission_names_why_it_never_ran() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.begin_turn("start something");
        let record = turn_unwound(&session, &anyhow::anyhow!("worktree creation failed"));
        assert_eq!(record.level, DiagnosticLevel::Error);
        assert_eq!(record.message, "unwound");
        assert_eq!(record.fields["error"], "worktree creation failed");
        assert_eq!(record.correlation.task_id, Some(session.id));
    }
}
