//! Pure projections of the daemon's bounded diagnostic ring.
use std::collections::{HashMap, VecDeque};

use waku_protocol::*;

pub(super) fn logs(
    epoch: uuid::Uuid,
    sequence: u64,
    events: &VecDeque<DiagnosticEvent>,
    query: &DiagnosticQuery,
) -> DiagnosticLogs {
    let oldest_sequence = events
        .front()
        .map_or(sequence.saturating_add(1), |e| e.sequence);
    let expiry = query
        .after
        .filter(|cursor| {
            cursor.epoch != epoch
                || cursor.sequence.saturating_add(1) < oldest_sequence
                || cursor.sequence > sequence
        })
        .map(|cursor| DiagnosticExpiry {
            epoch,
            requested_sequence: cursor.sequence,
            oldest_sequence,
        });
    let after = query
        .after
        .filter(|cursor| cursor.epoch == epoch && expiry.is_none())
        .map_or(0, |cursor| cursor.sequence);
    let mut matching: Vec<_> = events.iter().filter(|event| {
        let record = &event.record;
        let correlation = &record.correlation;
        event.sequence > after
            && query.since_ms.is_none_or(|time| record.timestamp_ms >= time)
            && query.until_ms.is_none_or(|time| record.timestamp_ms <= time)
            && query.min_level.is_none_or(|level| record.level >= level)
            && query.process.is_none_or(|process| record.origin.process() == process)
            && query.source.as_deref().is_none_or(|source| record.source == source)
            && query.session_id.is_none_or(|id| correlation.session_id == Some(id))
            && query.turn_id.is_none_or(|id| correlation.turn_id == Some(id))
            && query.message_id.is_none_or(|id| correlation.message_id == Some(id))
            && query.request_id.as_ref().is_none_or(|id| correlation.request_id.as_ref() == Some(id))
            && query.runtime_id.is_none_or(|id| matches!(record.origin, DiagnosticOrigin::Provider {runtime_id, ..} if runtime_id == id))
    }).cloned().collect();
    let limit = query
        .limit
        .unwrap_or(MAX_DIAGNOSTIC_EVENTS)
        .min(MAX_DIAGNOSTIC_EVENTS);
    let excess = matching.len().saturating_sub(limit);
    matching.drain(..excess);
    DiagnosticLogs {
        events: matching,
        cursor: DiagnosticCursor { epoch, sequence },
        oldest_sequence,
        expiry,
        truncated: excess > 0 || expiry.is_some(),
    }
}

fn phase(record: &DiagnosticRecord) -> Option<DiagnosticPhase> {
    use DiagnosticPhase::*;
    match (record.source.as_str(), record.message.as_str()) {
        ("turn", "submitted") | ("driver", "prompt submitted") => Some(Submitted),
        ("driver", "turn started") => Some(ProviderStarted),
        ("turn", "streaming") => Some(Streaming),
        ("tool", _) => Some(Tool),
        ("turn", "settling") => Some(Settling),
        ("turn", "settled") => Some(Settled),
        ("turn", "failed" | "unwound") | ("driver", "error") => Some(Failed),
        ("turn", "interrupted") => Some(Interrupted),
        ("driver", "turn finished") => Some(if record.fields["interrupted"] == true {
            Interrupted
        } else if record.fields["success"] == true {
            Settled
        } else {
            Failed
        }),
        _ => None,
    }
}

fn terminal(phase: DiagnosticPhase) -> bool {
    matches!(
        phase,
        DiagnosticPhase::Settled | DiagnosticPhase::Failed | DiagnosticPhase::Interrupted
    )
}

pub(super) fn timeline(
    events: &VecDeque<DiagnosticEvent>,
    query: &DiagnosticTimelineQuery,
    observed_at_ms: u64,
) -> anyhow::Result<DiagnosticTimeline> {
    anyhow::ensure!(
        query.session_id.is_some() || query.turn_id.is_some(),
        "timeline requires sessionId or turnId"
    );
    let mut milestones: Vec<_> = events
        .iter()
        .filter(|event| {
            query
                .session_id
                .is_none_or(|id| event.record.correlation.session_id == Some(id))
                && query
                    .turn_id
                    .is_none_or(|id| event.record.correlation.turn_id == Some(id))
        })
        .filter_map(|event| phase(&event.record).map(|phase| (event.clone(), phase)))
        .collect();
    milestones.sort_by_key(|(event, _)| (event.record.timestamp_ms, event.sequence));
    let mut starts = HashMap::new();
    let mut last_per_turn = HashMap::new();
    let mut entries: Vec<DiagnosticTimelineEntry> = Vec::with_capacity(milestones.len());
    for (event, phase) in milestones {
        let key = (
            event.record.correlation.session_id,
            event.record.correlation.turn_id,
        );
        let timestamp_ms = event.record.timestamp_ms;
        let start = *starts.entry(key).or_insert(timestamp_ms);
        if let Some(index) = last_per_turn.insert(key, entries.len()) {
            let previous: &mut DiagnosticTimelineEntry = &mut entries[index];
            if !terminal(previous.phase) {
                previous.duration_ms = timestamp_ms.saturating_sub(previous.timestamp_ms);
            }
        }
        entries.push(DiagnosticTimelineEntry {
            phase,
            timestamp_ms,
            elapsed_ms: timestamp_ms.saturating_sub(start),
            duration_ms: if terminal(phase) {
                0
            } else {
                observed_at_ms.saturating_sub(timestamp_ms)
            },
            event,
        });
    }
    let complete =
        !entries.is_empty() && last_per_turn.values().all(|&i| terminal(entries[i].phase));
    Ok(DiagnosticTimeline {
        observed_at_ms,
        last_phase: entries.last().map(|entry| entry.phase),
        last_changed_at_ms: entries.last().map(|entry| entry.timestamp_ms),
        complete,
        truncated: events.front().is_some_and(|event| event.sequence > 1),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn event(sequence: u64, time: u64, source: &str, message: &str, turn: u128) -> DiagnosticEvent {
        let mut record = DiagnosticRecord::new(
            DiagnosticLevel::Info,
            DiagnosticOrigin::App {
                pid: 1,
                version: "test".into(),
            },
            source,
            message,
        )
        .with_correlation(DiagnosticCorrelation {
            session_id: Some(Uuid::from_u128(1)),
            turn_id: Some(Uuid::from_u128(turn)),
            message_id: Some(Uuid::from_u128(3)),
            request_id: Some("permission".into()),
        });
        record.timestamp_ms = time;
        DiagnosticEvent {
            epoch: Uuid::from_u128(9),
            sequence,
            record,
        }
    }

    #[test]
    fn logs_filter_every_identity_time_level_source_and_process_and_take_a_recent_tail() {
        let mut app = event(1, 100, "turn", "submitted", 2);
        app.record.level = DiagnosticLevel::Warn;
        let mut provider = event(2, 120, "stderr", "refused", 2);
        provider.record.origin = DiagnosticOrigin::Provider {
            provider: crate::model::ProviderKind::Pi,
            runtime_id: Uuid::from_u128(4),
        };
        provider.record.level = DiagnosticLevel::Error;
        let mut other = event(3, 90, "turn", "failed", 2);
        other.record.origin = DiagnosticOrigin::Daemon { pid: 2 };
        let records = VecDeque::from([app, provider, other]);
        let query = DiagnosticQuery {
            since_ms: Some(110),
            until_ms: Some(120),
            min_level: Some(DiagnosticLevel::Warn),
            source: Some("stderr".into()),
            process: Some(DiagnosticProcess::Provider),
            session_id: Some(Uuid::from_u128(1)),
            turn_id: Some(Uuid::from_u128(2)),
            message_id: Some(Uuid::from_u128(3)),
            request_id: Some("permission".into()),
            runtime_id: Some(Uuid::from_u128(4)),
            ..Default::default()
        };
        let found = logs(Uuid::from_u128(9), 3, &records, &query);
        assert_eq!(
            found.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [2]
        );
        for query in [
            DiagnosticQuery {
                until_ms: Some(99),
                since_ms: Some(100),
                ..Default::default()
            },
            DiagnosticQuery {
                request_id: Some("other".into()),
                ..Default::default()
            },
            DiagnosticQuery {
                message_id: Some(Uuid::nil()),
                ..Default::default()
            },
            DiagnosticQuery {
                session_id: Some(Uuid::nil()),
                ..Default::default()
            },
            DiagnosticQuery {
                turn_id: Some(Uuid::nil()),
                ..Default::default()
            },
            DiagnosticQuery {
                runtime_id: Some(Uuid::nil()),
                ..Default::default()
            },
        ] {
            assert!(
                logs(Uuid::from_u128(9), 3, &records, &query)
                    .events
                    .is_empty()
            );
        }
        let tail = logs(
            Uuid::from_u128(9),
            3,
            &records,
            &DiagnosticQuery {
                limit: Some(2),
                ..Default::default()
            },
        );
        // Stream order is authoritative even when a process clock goes backward.
        assert_eq!(
            tail.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(tail.truncated);
        let empty = logs(
            Uuid::from_u128(9),
            3,
            &records,
            &DiagnosticQuery {
                limit: Some(0),
                ..Default::default()
            },
        );
        assert!(empty.events.is_empty());
        assert!(empty.truncated);
    }

    #[test]
    fn logs_name_cursor_gaps_and_resume_exclusively() {
        let epoch = Uuid::from_u128(9);
        let records = VecDeque::from([
            event(7, 10, "turn", "submitted", 2),
            event(8, 20, "turn", "settled", 2),
        ]);
        for cursor in [
            DiagnosticCursor { epoch, sequence: 1 },
            DiagnosticCursor {
                epoch: Uuid::nil(),
                sequence: 7,
            },
            DiagnosticCursor {
                epoch,
                sequence: 99,
            },
        ] {
            let result = logs(
                epoch,
                8,
                &records,
                &DiagnosticQuery {
                    after: Some(cursor),
                    ..Default::default()
                },
            );
            assert_eq!(result.expiry.unwrap().oldest_sequence, 7);
            assert_eq!(result.events.len(), 2);
            assert!(result.truncated);
        }
        let result = logs(
            epoch,
            8,
            &records,
            &DiagnosticQuery {
                after: Some(DiagnosticCursor { epoch, sequence: 7 }),
                ..Default::default()
            },
        );
        assert!(result.expiry.is_none());
        assert_eq!(result.events[0].sequence, 8);
    }

    #[test]
    fn session_filters_keep_conversations_separate_even_when_turn_identity_matches() {
        let session_id = Uuid::from_u128(1);
        let other_session_id = Uuid::from_u128(11);
        let mut other = event(2, 120, "turn", "settled", 2);
        other.record.correlation.session_id = Some(other_session_id);
        let records = VecDeque::from([event(1, 100, "turn", "submitted", 2), other]);
        let logs = logs(
            Uuid::from_u128(9),
            2,
            &records,
            &DiagnosticQuery {
                session_id: Some(session_id),
                ..Default::default()
            },
        );
        assert_eq!(logs.events.len(), 1);
        assert_eq!(logs.events[0].sequence, 1);
        let selected = timeline(
            &records,
            &DiagnosticTimelineQuery {
                session_id: Some(session_id),
                turn_id: Some(Uuid::from_u128(2)),
            },
            200,
        )
        .unwrap();
        assert_eq!(selected.entries.len(), 1);
        assert!(!selected.complete);
        assert_eq!(selected.last_phase, Some(DiagnosticPhase::Submitted));
        let both = timeline(
            &records,
            &DiagnosticTimelineQuery {
                session_id: None,
                turn_id: Some(Uuid::from_u128(2)),
            },
            200,
        )
        .unwrap();
        assert_eq!(both.entries.len(), 2);
        assert!(
            !both.complete,
            "another session's settlement cannot close this turn"
        );
        assert_eq!(both.entries[1].elapsed_ms, 0);
    }

    #[test]
    fn timelines_merge_timestamp_order_and_reconstruct_settled_and_stuck_turns() {
        let mut provider = event(2, 110, "driver", "turn started", 2);
        provider.record.origin = DiagnosticOrigin::Provider {
            provider: crate::model::ProviderKind::Pi,
            runtime_id: Uuid::from_u128(4),
        };
        let records = VecDeque::from([
            event(1, 100, "turn", "submitted", 2),
            provider,
            event(3, 160, "turn", "settled", 2),
            event(4, 130, "turn", "streaming", 2),
            event(5, 140, "tool", "completed", 2),
            event(6, 150, "turn", "settling", 2),
            event(7, 200, "turn", "submitted", 7),
            event(8, 240, "turn", "streaming", 7),
        ]);
        let settled = timeline(
            &records,
            &DiagnosticTimelineQuery {
                session_id: None,
                turn_id: Some(Uuid::from_u128(2)),
            },
            300,
        )
        .unwrap();
        assert_eq!(
            settled.entries.iter().map(|e| e.phase).collect::<Vec<_>>(),
            [
                DiagnosticPhase::Submitted,
                DiagnosticPhase::ProviderStarted,
                DiagnosticPhase::Streaming,
                DiagnosticPhase::Tool,
                DiagnosticPhase::Settling,
                DiagnosticPhase::Settled
            ]
        );
        assert_eq!(
            settled
                .entries
                .iter()
                .map(|e| e.elapsed_ms)
                .collect::<Vec<_>>(),
            [0, 10, 30, 40, 50, 60]
        );
        assert_eq!(
            settled
                .entries
                .iter()
                .map(|e| e.duration_ms)
                .collect::<Vec<_>>(),
            [10, 20, 10, 10, 10, 0]
        );
        assert!(settled.complete);
        let stuck = timeline(
            &records,
            &DiagnosticTimelineQuery {
                session_id: None,
                turn_id: Some(Uuid::from_u128(7)),
            },
            300,
        )
        .unwrap();
        assert!(!stuck.complete);
        assert_eq!(stuck.last_phase, Some(DiagnosticPhase::Streaming));
        assert_eq!(stuck.last_changed_at_ms, Some(240));
        assert_eq!(stuck.entries.last().unwrap().duration_ms, 60);
        let session = timeline(
            &records,
            &DiagnosticTimelineQuery {
                session_id: Some(Uuid::from_u128(1)),
                turn_id: None,
            },
            300,
        )
        .unwrap();
        assert!(!session.complete);
        assert_eq!(session.entries[6].elapsed_ms, 0);
        assert!(timeline(&records, &DiagnosticTimelineQuery::default(), 300).is_err());
    }

    #[test]
    fn terminal_provider_outcomes_and_retention_are_explicit() {
        for (fields, expected) in [
            (
                serde_json::json!({"success":true,"interrupted":false}),
                DiagnosticPhase::Settled,
            ),
            (
                serde_json::json!({"success":false,"interrupted":false}),
                DiagnosticPhase::Failed,
            ),
            (
                serde_json::json!({"success":false,"interrupted":true}),
                DiagnosticPhase::Interrupted,
            ),
        ] {
            let mut finished = event(10, 100, "driver", "turn finished", 2);
            finished.record.fields = fields;
            let timeline = timeline(
                &VecDeque::from([finished]),
                &DiagnosticTimelineQuery {
                    session_id: None,
                    turn_id: Some(Uuid::from_u128(2)),
                },
                120,
            )
            .unwrap();
            assert_eq!(timeline.last_phase, Some(expected));
            assert!(timeline.complete);
            assert!(timeline.truncated);
        }
    }
}
