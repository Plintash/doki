//! Daemon-owned debug automation. Uses the server's authenticated connections,
//! bounded delivery queues and epoch. No instance-wide UI snapshot is retained.
use crate::automation_recording::{self, Recorder, RecordingTicket};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};
use uuid::Uuid;
use waku_protocol::*;

use super::{Backend, Hub};

const MAX_PENDING_CALLS: usize = 64;
const MAX_WAITS: usize = 64;
const MAX_RUNS: usize = 16;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_SESSION_BYTES: usize = 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
// A deadline never extends condition matching, but an in-flight bootstrap
// capture gets a bounded chance to explain a short/zero-timeout failure.
const WAIT_EVIDENCE_GRACE: Duration = Duration::from_secs(1);
const MAX_WAIT_MS: u64 = 5 * 60 * 1000;
const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);

type Result<T> = std::result::Result<T, AutomationError>;

struct Executor {
    instance: AutomationInstance,
    subscriber: Option<u64>,
    interest: Option<(bool, Vec<AutomationStream>, Vec<String>)>,
}

struct Session {
    instance_id: Uuid,
    subscriber: Option<u64>,
    sequence: u64,
    streams: Vec<AutomationStream>,
    events: VecDeque<(AutomationEvent, usize)>,
    bytes: usize,
    touched: Instant,
}

struct PendingCall {
    executor: u64,
    instance_id: Uuid,
    session_id: Uuid,
    reply: Sender<Result<AutomationCallResult>>,
}

struct Wait {
    session_id: Uuid,
    condition: AutomationCondition,
    deadline: Instant,
    started_ms: u64,
    result: AutomationWait,
    baseline: Value,
    observation: AutomationObservation,
    initial_observation_pending: bool,
    recording_ticket: Option<RecordingTicket>,
}

struct Run {
    session_id: Uuid,
    result: AutomationRun,
    wait_id: Option<Uuid>,
}

#[derive(Default)]
struct State {
    executors: HashMap<Uuid, Executor>,
    sessions: HashMap<Uuid, Session>,
    calls: HashMap<Uuid, PendingCall>,
    waits: HashMap<Uuid, Wait>,
    runs: HashMap<Uuid, Run>,
    recordings: HashMap<Uuid, Recorder>,
    // Covers admission before a call/wait enters its own pending registry.
    recording_admissions: HashMap<Uuid, usize>,
}

#[derive(Default)]
pub(super) struct Runtime {
    state: Mutex<State>,
    changed: Condvar,
}

fn unavailable(instance_id: Uuid) -> AutomationError {
    AutomationError::new(
        "executor_unavailable",
        format!("instance {instance_id}: no executor is connected"),
    )
}
fn expired(session_id: Uuid) -> AutomationError {
    AutomationError::new(
        "session_expired",
        format!("automation session {session_id} expired or was detached; attach again"),
    )
}
fn is_pending(status: &AutomationStatus) -> bool {
    matches!(status, AutomationStatus::Pending)
}

impl Runtime {
    pub(super) fn command(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        backend: &Arc<dyn Backend>,
        command: AutomationCommand,
        subscriber: u64,
    ) -> AutomationResult {
        if !cfg!(debug_assertions) {
            return AutomationResult::Error {
                error: AutomationError::new(
                    "unavailable",
                    "automation is unavailable in release builds",
                ),
            };
        }
        let mut remote_instances = vec![];
        if !matches!(
            command,
            AutomationCommand::RegisterExecutor { .. }
                | AutomationCommand::ScratchCreate { .. }
                | AutomationCommand::ScratchRemove { .. }
        ) {
            let outgoing = hub
                .state
                .lock()
                .subscribers
                .get(&subscriber)
                .map(|s| s.messages.clone());
            if let Some(outgoing) = outgoing
                && let Some(remote) = backend.forward_automation(&command, outgoing, subscriber)
            {
                // A request worker can enter forwarding just after its socket
                // guard ran. Close any transport created in that race as well.
                if !hub.state.lock().subscribers.contains_key(&subscriber) {
                    backend.automation_subscriber_disconnected(subscriber);
                }
                match remote {
                    Ok(AutomationResult::Instances { instances })
                        if matches!(command, AutomationCommand::Instances) =>
                    {
                        remote_instances = instances
                    }
                    Ok(result) => return result,
                    Err(error) => {
                        return AutomationResult::Error {
                            error: AutomationError::new("remote_unavailable", error.to_string()),
                        };
                    }
                }
            }
        }
        let result = match command {
            AutomationCommand::RegisterExecutor { mut instance } => {
                let mut state = self.state.lock();
                if state.executors.len() >= MAX_AUTOMATION_SESSIONS {
                    let removable = state.executors.iter().find_map(|(id, executor)| {
                        (executor.subscriber.is_none()
                            && !state
                                .sessions
                                .values()
                                .any(|session| session.instance_id == *id))
                        .then_some(*id)
                    });
                    if let Some(id) = removable {
                        state.executors.remove(&id);
                    }
                }
                if state.executors.len() >= MAX_AUTOMATION_SESSIONS
                    && !state.executors.contains_key(&instance.instance_id)
                {
                    Err(AutomationError::new(
                        "capacity",
                        "too many registered automation instances",
                    ))
                } else if state
                    .executors
                    .get(&instance.instance_id)
                    .and_then(|e| e.subscriber)
                    .is_some_and(|id| id != subscriber)
                {
                    Err(AutomationError::new(
                        "executor_exists",
                        format!("instance {} already has an executor", instance.instance_id),
                    ))
                } else {
                    instance.executor_connected = true;
                    state.executors.insert(
                        instance.instance_id,
                        Executor {
                            instance: instance.clone(),
                            subscriber: Some(subscriber),
                            interest: None,
                        },
                    );
                    Self::interest(&mut state, hub, instance.instance_id);
                    Ok(AutomationResult::Registered { instance })
                }
            }
            AutomationCommand::Instances => {
                let state = self.state.lock();
                let mut instances: Vec<_> = state
                    .executors
                    .values()
                    .map(|e| e.instance.clone())
                    .collect();
                instances.extend(remote_instances);
                instances.sort_by(|a, b| {
                    a.checkout
                        .cmp(&b.checkout)
                        .then_with(|| a.database.cmp(&b.database))
                });
                Ok(AutomationResult::Instances { instances })
            }
            command @ (AutomationCommand::ScratchCreate { .. }
            | AutomationCommand::ScratchRemove { .. }) => backend
                .handle_automation_scratch(&command)
                .map_err(|e| AutomationError::new("scratch_failed", e.to_string())),
            AutomationCommand::Attach { instance_id } => self.attach(hub, instance_id, subscriber),
            AutomationCommand::Resume { session_id, after } => {
                let mut state = self.state.lock();
                if let Some(session) = state.sessions.get_mut(&session_id) {
                    session.subscriber = Some(subscriber);
                    session.touched = Instant::now();
                    let after = after.unwrap_or(AutomationCursor {
                        epoch: hub.epoch,
                        sequence: session.sequence,
                    });
                    let replay = Self::diff(hub, session, after, &[]);
                    let session = Self::session(hub, &state, session_id);
                    Self::interest(&mut state, hub, session.instance.instance_id);
                    Ok(AutomationResult::Attached { session, replay })
                } else {
                    Err(expired(session_id))
                }
            }
            AutomationCommand::Detach { session_id } => {
                let mut state = self.state.lock();
                match state.sessions.remove(&session_id) {
                    Some(session) => {
                        state.waits.retain(|_, wait| wait.session_id != session_id);
                        state.runs.retain(|_, run| run.session_id != session_id);
                        state.recordings.remove(&session_id);
                        self.changed.notify_all();
                        Self::interest(&mut state, hub, session.instance_id);
                        Ok(AutomationResult::Detached)
                    }
                    None => Err(expired(session_id)),
                }
            }
            AutomationCommand::Call { session_id, call } => (|| {
                let ticket = self.record_begin(
                    session_id,
                    &AutomationStepOperation::Call { call: call.clone() },
                )?;
                let outcome = self.call(hub, session_id, call);
                self.record_complete(
                    session_id,
                    ticket,
                    outcome.as_ref().map(call_step_value).map_err(Clone::clone),
                );
                outcome.map(|result| AutomationResult::Called { result })
            })(),
            AutomationCommand::Diff {
                session_id,
                after,
                streams,
            } => {
                let mut state = self.state.lock();
                state
                    .sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| expired(session_id))
                    .map(|session| {
                        session.touched = Instant::now();
                        AutomationResult::Diff {
                            diff: Self::diff(hub, session, after, &streams),
                        }
                    })
            }
            AutomationCommand::Subscribe {
                session_id,
                streams,
            } => {
                let mut state = self.state.lock();
                if let Some(session) = state.sessions.get_mut(&session_id) {
                    session.streams = unique_streams(streams);
                    session.subscriber = Some(subscriber);
                    session.touched = Instant::now();
                    let instance_id = session.instance_id;
                    Self::interest(&mut state, hub, instance_id);
                    Ok(AutomationResult::Subscribed {
                        session: Self::session(hub, &state, session_id),
                    })
                } else {
                    Err(expired(session_id))
                }
            }
            AutomationCommand::Wait {
                session_id,
                condition,
                timeout_ms,
            } => (|| {
                let ticket = self.record_begin(
                    session_id,
                    &AutomationStepOperation::Wait {
                        condition: condition.clone(),
                        timeout_ms,
                    },
                )?;
                match self.start_wait(hub, session_id, condition, timeout_ms) {
                    Ok(wait) => {
                        if let Some(ticket) = ticket {
                            let mut state = self.state.lock();
                            if let Some(pending) = state.waits.get_mut(&wait.wait_id) {
                                pending.recording_ticket = Some(ticket);
                            }
                            finish_recorded_wait(&mut state, wait.wait_id);
                        }
                        self.record_admission_finished(session_id);
                        Ok(AutomationResult::Wait { wait })
                    }
                    Err(error) => {
                        self.record_complete(session_id, ticket, Err(error.clone()));
                        Err(error)
                    }
                }
            })(),
            AutomationCommand::WaitResult {
                session_id,
                wait_id,
            } => {
                let state = self.state.lock();
                state
                    .waits
                    .get(&wait_id)
                    .filter(|wait| wait.session_id == session_id)
                    .map(|wait| AutomationResult::Wait {
                        wait: wait.result.clone(),
                    })
                    .ok_or_else(|| {
                        AutomationError::new(
                            "wait_expired",
                            format!("wait {wait_id} is not retained"),
                        )
                    })
            }
            command @ (AutomationCommand::JobStart { .. }
            | AutomationCommand::JobResult { .. }
            | AutomationCommand::JobAbort { .. }
            | AutomationCommand::ScheduleCreate { .. }
            | AutomationCommand::ScheduleList
            | AutomationCommand::ScheduleGet { .. }
            | AutomationCommand::ScheduleSetEnabled { .. }
            | AutomationCommand::ScheduleCancel { .. }
            | AutomationCommand::ScheduleRemove { .. }) => {
                Ok(backend.handle_automation_jobs(&command))
            }
            AutomationCommand::RecordingStart { session_id, name } => {
                let mut state = self.state.lock();
                if !state.sessions.contains_key(&session_id) {
                    Err(expired(session_id))
                } else if state
                    .recordings
                    .get(&session_id)
                    .is_some_and(|recorder| recorder.blocks_session())
                    || state
                        .recording_admissions
                        .get(&session_id)
                        .is_some_and(|count| *count > 0)
                    || state
                        .calls
                        .values()
                        .any(|call| call.session_id == session_id)
                    || state
                        .runs
                        .values()
                        .any(|run| run.session_id == session_id && is_pending(&run.result.status))
                    || state.waits.values().any(|wait| {
                        wait.session_id == session_id && is_pending(&wait.result.status)
                    })
                {
                    Err(AutomationError::new(
                        "recording_busy",
                        "finish or cancel existing session work before recording",
                    ))
                } else {
                    Recorder::new(name).map(|recorder| {
                        let recording = recorder.status();
                        state.recordings.insert(session_id, recorder);
                        AutomationResult::Recording { recording }
                    })
                }
            }
            AutomationCommand::RecordingStatus { session_id }
            | AutomationCommand::RecordingStop { session_id }
            | AutomationCommand::RecordingCancel { session_id } => {
                let mut state = self.state.lock();
                match state.recordings.get_mut(&session_id) {
                    None => Err(AutomationError::new(
                        "recording_missing",
                        "this session has no recording",
                    )),
                    Some(recorder) => match command {
                        AutomationCommand::RecordingStop { .. } => recorder
                            .stop()
                            .map(|replay| AutomationResult::Replay { replay }),
                        AutomationCommand::RecordingCancel { .. } => {
                            recorder.cancel();
                            Ok(AutomationResult::Recording {
                                recording: recorder.status(),
                            })
                        }
                        _ => Ok(AutomationResult::Recording {
                            recording: recorder.status(),
                        }),
                    },
                }
            }
            AutomationCommand::ReplayStart {
                session_id,
                replay,
                parameters,
            } => (|| {
                automation_recording::preflight(&replay, &parameters)?;
                let scenario = AutomationScenario {
                    name: replay.name.clone(),
                    steps: replay.steps.iter().map(|step| step.step.clone()).collect(),
                };
                self.start_run_plan(hub, session_id, scenario, Some((replay, parameters)))
                    .map(|run| AutomationResult::Scenario { run })
            })(),
            AutomationCommand::ScenarioStart {
                session_id,
                scenario,
            } => self
                .start_run(hub, session_id, scenario)
                .map(|run| AutomationResult::Scenario { run }),
            AutomationCommand::ScenarioResult { session_id, run_id } => {
                let state = self.state.lock();
                state
                    .runs
                    .get(&run_id)
                    .filter(|run| run.session_id == session_id)
                    .map(|run| AutomationResult::Scenario {
                        run: run.result.clone(),
                    })
                    .ok_or_else(|| {
                        AutomationError::new("run_expired", format!("run {run_id} is not retained"))
                    })
            }
            AutomationCommand::ScenarioAbort { session_id, run_id } => self
                .abort_run(hub, session_id, run_id)
                .map(|run| AutomationResult::Scenario { run }),
        };
        // A queued request may first run after its connection guard already
        // disconnected. Do not leave a newly attached session or registration
        // keeping the tree alive on behalf of that vanished subscriber.
        if !hub.state.lock().subscribers.contains_key(&subscriber) {
            self.disconnected(hub, subscriber);
        }
        result.unwrap_or_else(|error| AutomationResult::Error { error })
    }

    fn record_begin(
        &self,
        session_id: Uuid,
        operation: &AutomationStepOperation,
    ) -> Result<Option<RecordingTicket>> {
        let mut state = self.state.lock();
        let ticket = match state.recordings.get_mut(&session_id) {
            Some(recorder) if recorder.blocks_session() => Some(recorder.begin(operation)?),
            _ => None,
        };
        *state.recording_admissions.entry(session_id).or_default() += 1;
        Ok(ticket)
    }

    fn record_admission_finished(&self, session_id: Uuid) {
        finish_record_admission(&mut self.state.lock(), session_id);
    }

    fn record_complete(
        &self,
        session_id: Uuid,
        ticket: Option<RecordingTicket>,
        outcome: Result<Value>,
    ) {
        let mut state = self.state.lock();
        finish_record_admission(&mut state, session_id);
        if let Some(ticket) = ticket
            && let Some(recorder) = state.recordings.get_mut(&session_id)
        {
            let _ = recorder.complete(ticket, outcome);
        }
    }

    fn attach(&self, hub: &Hub, instance_id: Uuid, subscriber: u64) -> Result<AutomationResult> {
        let mut state = self.state.lock();
        if !state
            .executors
            .get(&instance_id)
            .is_some_and(|e| e.subscriber.is_some())
        {
            return Err(unavailable(instance_id));
        }
        if state.sessions.len() >= MAX_AUTOMATION_SESSIONS {
            return Err(AutomationError::new(
                "capacity",
                "too many automation sessions; detach an unused session",
            ));
        }
        let session_id = Uuid::new_v4();
        state.sessions.insert(
            session_id,
            Session {
                instance_id,
                subscriber: Some(subscriber),
                sequence: 0,
                streams: vec![],
                events: VecDeque::new(),
                bytes: 0,
                touched: Instant::now(),
            },
        );
        let session = Self::session(hub, &state, session_id);
        let replay = AutomationDiff {
            events: vec![],
            cursor: session.cursor,
            expiry: None,
        };
        Self::interest(&mut state, hub, instance_id);
        Ok(AutomationResult::Attached { session, replay })
    }

    fn session(hub: &Hub, state: &State, session_id: Uuid) -> AutomationSession {
        let session = &state.sessions[&session_id];
        AutomationSession {
            session_id,
            instance: state.executors[&session.instance_id].instance.clone(),
            cursor: AutomationCursor {
                epoch: hub.epoch,
                sequence: session.sequence,
            },
            streams: session.streams.clone(),
        }
    }

    fn diff(
        hub: &Hub,
        session: &Session,
        requested: AutomationCursor,
        streams: &[AutomationStream],
    ) -> AutomationDiff {
        let cursor = AutomationCursor {
            epoch: hub.epoch,
            sequence: session.sequence,
        };
        let oldest_sequence = session
            .events
            .front()
            .map_or(session.sequence.saturating_add(1), |(e, _)| {
                e.cursor.sequence
            });
        let invalid = requested.epoch != hub.epoch
            || requested.sequence > session.sequence
            || requested.sequence.saturating_add(1) < oldest_sequence;
        AutomationDiff {
            events: session
                .events
                .iter()
                .filter(|(e, _)| {
                    (invalid || e.cursor.sequence > requested.sequence)
                        && (streams.is_empty() || streams.contains(&e.stream))
                })
                .map(|(e, _)| e.clone())
                .collect(),
            cursor,
            expiry: invalid.then_some(AutomationExpiry {
                requested,
                oldest_sequence,
                cursor,
            }),
        }
    }

    fn interest(state: &mut State, hub: &Hub, instance_id: Uuid) {
        let Some(executor) = state.executors.get(&instance_id).and_then(|e| e.subscriber) else {
            return;
        };
        let mut attached = false;
        let mut streams = vec![];
        let mut model_sections = vec![];
        for (session_id, session) in &state.sessions {
            if session.instance_id != instance_id {
                continue;
            }
            if session.subscriber.is_some() {
                attached = true;
                streams.extend(session.streams.iter().copied());
                if session.streams.contains(&AutomationStream::Model) {
                    model_sections.push("state".into());
                }
            }
            for wait in state
                .waits
                .values()
                .filter(|wait| wait.session_id == *session_id && is_pending(&wait.result.status))
            {
                attached = true;
                // All waits need current tree/UI/health evidence, but only
                // state conditions need serialized model data. Never make a
                // control wait depend on the size of unrelated transcripts.
                streams.push(AutomationStream::Ui);
                if let Some(section) = wait_model_section(&wait.condition) {
                    streams.push(AutomationStream::Model);
                    model_sections.push(section);
                }
            }
            if state
                .runs
                .values()
                .any(|r| r.session_id == *session_id && is_pending(&r.result.status))
            {
                attached = true;
            }
        }
        streams.retain(|stream| *stream != AutomationStream::Diagnostic);
        let streams = unique_streams(streams);
        model_sections.sort();
        model_sections.dedup();
        let registration = state
            .executors
            .get_mut(&instance_id)
            .expect("registered executor");
        if registration.interest.as_ref()
            == Some(&(attached, streams.clone(), model_sections.clone()))
        {
            return;
        }
        registration.interest = Some((attached, streams.clone(), model_sections.clone()));
        hub.send_to(
            executor,
            ServerMessage::AutomationInterest {
                instance_id,
                attached,
                streams,
                model_sections,
            },
        );
    }

    fn append(
        state: &mut State,
        hub: &Hub,
        session_id: Uuid,
        stream: AutomationStream,
        payload: Value,
    ) -> AutomationCursor {
        let session = state
            .sessions
            .get_mut(&session_id)
            .expect("session checked while holding runtime lock");
        session.sequence = session.sequence.saturating_add(1);
        let cursor = AutomationCursor {
            epoch: hub.epoch,
            sequence: session.sequence,
        };
        let event = AutomationEvent {
            session_id,
            cursor,
            stream,
            payload,
        };
        let bytes = serde_json::to_vec(&event).map_or(MAX_EVENT_BYTES + 1, |bytes| bytes.len());
        if bytes <= MAX_EVENT_BYTES {
            session.events.push_back((event.clone(), bytes));
            session.bytes += bytes;
            while session.events.len() > MAX_AUTOMATION_EVENTS || session.bytes > MAX_SESSION_BYTES
            {
                if let Some((_, bytes)) = session.events.pop_front() {
                    session.bytes -= bytes;
                }
            }
            if session.streams.contains(&stream)
                && let Some(subscriber) = session.subscriber
            {
                hub.send_to(subscriber, ServerMessage::AutomationEvent { event });
            }
        } else {
            // A single oversized delta creates an explicit cursor gap. Keeping
            // later entries must not make that missing event look continuous.
            session.events.clear();
            session.bytes = 0;
            // Only a live consumer of this stream needs reconnect/replay to
            // learn the gap. Unrelated sessions keep their transport and waits.
            if session.streams.contains(&stream)
                && let Some(subscriber) = session.subscriber
            {
                hub.kick(subscriber);
            }
        }
        cursor
    }

    fn call(
        &self,
        hub: &Arc<Hub>,
        session_id: Uuid,
        call: AutomationCall,
    ) -> Result<AutomationCallResult> {
        self.call_for_run(hub, session_id, call, None)
            .map_err(|error| {
                if error.bundle.is_some() {
                    error
                } else {
                    self.evidence(hub, error, &AutomationObservation::default(), None)
                }
            })
    }

    fn call_for_run(
        &self,
        hub: &Arc<Hub>,
        session_id: Uuid,
        call: AutomationCall,
        run_id: Option<Uuid>,
    ) -> Result<AutomationCallResult> {
        let (receiver, request_id, instance_id) = {
            let mut state = self.state.lock();
            // Check under the same lock as dispatch: abort cannot race the
            // between-step check and let a later action reach the executor.
            if run_id.is_some_and(|id| {
                !state
                    .runs
                    .get(&id)
                    .is_some_and(|run| is_pending(&run.result.status))
            }) {
                return Err(AutomationError::new("run_aborted", "scenario was aborted"));
            }
            let session = state
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| expired(session_id))?;
            session.touched = Instant::now();
            let instance_id = session.instance_id;
            let executor = state
                .executors
                .get(&instance_id)
                .and_then(|e| e.subscriber)
                .ok_or_else(|| unavailable(instance_id))?;
            if state.calls.len() >= MAX_PENDING_CALLS {
                return Err(AutomationError::new(
                    "capacity",
                    "too many pending executor calls",
                ));
            }
            let request_id = Uuid::new_v4();
            let (reply, receiver) = bounded(1);
            state.calls.insert(
                request_id,
                PendingCall {
                    executor,
                    instance_id,
                    session_id,
                    reply,
                },
            );
            if !hub.send_to(
                executor,
                ServerMessage::AutomationExecutorRequest {
                    request: AutomationExecutorRequest {
                        request_id,
                        session_id,
                        instance_id,
                        call,
                    },
                },
            ) {
                state.calls.remove(&request_id);
                return Err(unavailable(instance_id));
            }
            (receiver, request_id, instance_id)
        };
        let outcome = receiver.recv_timeout(CALL_TIMEOUT);
        self.state.lock().calls.remove(&request_id);
        match outcome {
            Ok(result) => result,
            Err(_) => Err(self.evidence(
                hub,
                AutomationError::new(
                    "executor_timeout",
                    format!(
                        "instance {instance_id}: executor did not answer within {} seconds",
                        CALL_TIMEOUT.as_secs()
                    ),
                ),
                &AutomationObservation::default(),
                None,
            )),
        }
    }

    pub(super) fn reply(&self, hub: &Hub, subscriber: u64, reply: AutomationExecutorReply) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut state = self.state.lock();
        if !state
            .calls
            .get(&reply.request_id)
            .is_some_and(|call| call.executor == subscriber)
        {
            return;
        }
        let call = state
            .calls
            .remove(&reply.request_id)
            .expect("matched pending call");
        // Commit in socket arrival order, before waking request workers. Their
        // scheduling must not put an older frame after a newer pushed event.
        let result = self.complete_call(&mut state, hub, &call, reply.outcome);
        let _ = call.reply.try_send(result);
    }

    fn complete_call(
        &self,
        state: &mut State,
        hub: &Hub,
        call: &PendingCall,
        outcome: AutomationExecutorOutcome,
    ) -> Result<AutomationCallResult> {
        let session_id = call.session_id;
        let instance_id = call.instance_id;
        let executor = call.executor;
        match outcome {
            AutomationExecutorOutcome::Ok { execution } => {
                if serde_json::to_vec(&execution.observation)
                    .map_or(true, |b| b.len() > MAX_AUTOMATION_OBSERVATION_BYTES)
                {
                    return Err(self.evidence(
                        hub,
                        AutomationError::new(
                            "observation_too_large",
                            "executor observation exceeds 4 MiB; select fewer snapshot sections",
                        ),
                        &AutomationObservation::default(),
                        None,
                    ));
                }
                if !state.sessions.contains_key(&session_id) {
                    return Err(expired(session_id));
                }
                // A disconnected executor's late reply cannot become fresh UI.
                if state.executors.get(&instance_id).and_then(|e| e.subscriber) != Some(executor) {
                    return Err(unavailable(instance_id));
                }
                let cursor = Self::append(
                    state,
                    hub,
                    session_id,
                    if state
                        .executors
                        .get(&instance_id)
                        .is_some_and(|executor| executor.instance.headless)
                    {
                        AutomationStream::Model
                    } else {
                        AutomationStream::Ui
                    },
                    delta(&execution.observation),
                );
                self.observe_waits(state, hub, instance_id, &execution.observation, None, None);
                Ok(AutomationCallResult { cursor, execution })
            }
            AutomationExecutorOutcome::Error { error } => {
                let observation = error
                    .bundle
                    .as_ref()
                    .map(|bundle| AutomationObservation {
                        observed: bundle.observed.clone(),
                        tree_diff: bundle.tree_diff.clone(),
                        state_diff: Value::Null,
                    })
                    .unwrap_or_default();
                Err(self.evidence(hub, error, &observation, None))
            }
        }
    }

    pub(super) fn event(&self, hub: &Hub, subscriber: u64, event: AutomationExecutorEvent) {
        if !cfg!(debug_assertions) {
            return;
        }
        if event.stream == AutomationStream::Diagnostic {
            return;
        }
        if serde_json::to_vec(&event.observation)
            .map_or(true, |b| b.len() > MAX_AUTOMATION_OBSERVATION_BYTES)
        {
            hub.kick(subscriber);
            return;
        }
        let mut state = self.state.lock();
        if state
            .executors
            .get(&event.instance_id)
            .and_then(|e| e.subscriber)
            != Some(subscriber)
        {
            return;
        }
        let sessions: Vec<_> = state
            .sessions
            .iter()
            .filter(|(_, s)| s.instance_id == event.instance_id)
            .map(|(id, _)| *id)
            .collect();
        for session_id in sessions {
            Self::append(
                &mut state,
                hub,
                session_id,
                event.stream,
                delta(&event.observation),
            );
        }
        self.observe_waits(
            &mut state,
            hub,
            event.instance_id,
            &event.observation,
            Some(event.stream),
            None,
        );
    }

    pub(super) fn model_event(&self, hub: &Hub, event: &SequencedEvent) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut state = self.state.lock();
        let sessions: Vec<_> = state.sessions.keys().copied().collect();
        for session_id in sessions {
            Self::append(
                &mut state,
                hub,
                session_id,
                AutomationStream::Model,
                serde_json::to_value(event).unwrap_or(Value::Null),
            );
        }
    }

    pub(super) fn diagnostic(&self, hub: &Hub, event: &DiagnosticEvent) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut state = self.state.lock();
        let sessions: Vec<_> = state.sessions.keys().copied().collect();
        for session_id in sessions {
            Self::append(
                &mut state,
                hub,
                session_id,
                AutomationStream::Diagnostic,
                serde_json::to_value(event).unwrap_or(Value::Null),
            );
        }
        let instances: Vec<_> = state.executors.keys().copied().collect();
        for instance_id in instances {
            self.observe_waits(
                &mut state,
                hub,
                instance_id,
                &AutomationObservation::default(),
                Some(AutomationStream::Diagnostic),
                Some(event),
            );
        }
    }

    pub(super) fn disconnected(&self, hub: &Hub, subscriber: u64) {
        let mut state = self.state.lock();
        let mut affected = vec![];
        for executor in state
            .executors
            .values_mut()
            .filter(|e| e.subscriber == Some(subscriber))
        {
            executor.subscriber = None;
            executor.instance.executor_connected = false;
            affected.push(executor.instance.instance_id);
        }
        let calls: Vec<_> = state
            .calls
            .iter()
            .filter(|(_, c)| c.executor == subscriber)
            .map(|(id, _)| *id)
            .collect();
        for id in calls {
            if let Some(call) = state.calls.remove(&id) {
                let _ = call.reply.try_send(Err(unavailable(call.instance_id)));
            }
        }
        for session in state
            .sessions
            .values_mut()
            .filter(|s| s.subscriber == Some(subscriber))
        {
            session.subscriber = None;
            session.touched = Instant::now();
            affected.push(session.instance_id);
        }
        // Waits survive a client disconnect, but cannot claim stale UI when
        // their executor disappears. Fresh-build discovery uses Instances.
        let gone: Vec<_> = state
            .waits
            .iter()
            .filter(|(_, w)| {
                is_pending(&w.result.status)
                    && state.sessions.get(&w.session_id).is_some_and(|s| {
                        state
                            .executors
                            .get(&s.instance_id)
                            .is_some_and(|e| e.subscriber.is_none())
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            let instance_id = state.sessions[&state.waits[&id].session_id].instance_id;
            self.fail_wait(&mut state, hub, id, unavailable(instance_id));
        }
        for instance_id in affected {
            Self::interest(&mut state, hub, instance_id);
        }
        self.changed.notify_all();
    }

    fn evidence(
        &self,
        hub: &Hub,
        mut error: AutomationError,
        observation: &AutomationObservation,
        since_ms: Option<u64>,
    ) -> AutomationError {
        let mut bundle = error
            .bundle
            .take()
            .unwrap_or_else(|| AutomationFailureBundle {
                observed: observation.observed.clone(),
                tree_diff: observation.tree_diff.clone(),
                recent_events: None,
                counters: DiagnosticCounters {
                    observed_at_ms: crate::model::unix_time_millis(),
                    processes: vec![],
                },
                diagnostic_errors: vec![],
            });
        if bundle.observed.is_null() {
            bundle
                .diagnostic_errors
                .push("executor observation unavailable".into());
        }
        if bundle.tree_diff.is_null() {
            bundle.tree_diff = json!({"baselineAvailable":false,"observedAvailable":false,"added":[],"removed":[],"changed":[]});
        }
        let session_id = evidence_session_id(&bundle.observed);
        let query = DiagnosticQuery {
            since_ms: Some(
                since_ms.unwrap_or_else(|| crate::model::unix_time_millis().saturating_sub(60_000)),
            ),
            session_id,
            limit: Some(128),
            ..Default::default()
        };
        if let Some(Ok(ResponsePayload::DiagnosticLogs { result })) =
            hub.diagnostic_command(&Command::DiagnosticLogs { query })
        {
            bundle.recent_events = Some(result);
        } else {
            bundle
                .diagnostic_errors
                .push("daemon diagnostic logs unavailable".into());
        }
        if let Some(Ok(ResponsePayload::DiagnosticCounters { result })) =
            hub.diagnostic_command(&Command::DiagnosticCounters)
        {
            // Preserve app failure-time samples, replacing only matching origins.
            for report in result.processes {
                if !bundle
                    .counters
                    .processes
                    .iter()
                    .any(|existing| existing.origin == report.origin)
                {
                    bundle.counters.processes.push(report);
                }
            }
            bundle.counters.observed_at_ms = result.observed_at_ms;
        } else {
            bundle
                .diagnostic_errors
                .push("daemon diagnostic counters unavailable".into());
        }
        error.bundle = Some(bundle);
        error
    }

    fn start_wait(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        session_id: Uuid,
        condition: AutomationCondition,
        timeout_ms: u64,
    ) -> Result<AutomationWait> {
        if timeout_ms > MAX_WAIT_MS {
            return Err(AutomationError::new(
                "invalid_timeout",
                format!("wait timeout must be at most {MAX_WAIT_MS} ms"),
            ));
        }
        let wait = {
            let mut state = self.state.lock();
            let session = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| expired(session_id))?;
            let instance_id = session.instance_id;
            if state
                .executors
                .get(&instance_id)
                .is_some_and(|executor| executor.instance.headless)
                && !matches!(
                    condition,
                    AutomationCondition::State { .. }
                        | AutomationCondition::StateContains { .. }
                        | AutomationCondition::Diagnostic { .. }
                )
            {
                return Err(AutomationError::new(
                    "unsupported_headless",
                    "headless executors support backend state and diagnostic waits, not UI conditions",
                ));
            }
            let cursor = AutomationCursor {
                epoch: hub.epoch,
                sequence: session.sequence,
            };
            if !state
                .executors
                .get(&instance_id)
                .is_some_and(|e| e.subscriber.is_some())
            {
                return Err(unavailable(instance_id));
            }
            if state.waits.len() >= MAX_WAITS {
                let oldest = state
                    .waits
                    .iter()
                    .filter(|(_, w)| !is_pending(&w.result.status))
                    .min_by_key(|(_, w)| w.started_ms)
                    .map(|(id, _)| *id);
                if let Some(id) = oldest {
                    state.waits.remove(&id);
                } else {
                    return Err(AutomationError::new(
                        "capacity",
                        "too many pending automation waits",
                    ));
                }
            }
            let result = AutomationWait {
                wait_id: Uuid::new_v4(),
                status: AutomationStatus::Pending,
                cursor,
                result: None,
                error: None,
            };
            state.waits.insert(
                result.wait_id,
                Wait {
                    session_id,
                    condition: condition.clone(),
                    deadline: Instant::now() + Duration::from_millis(timeout_ms),
                    started_ms: crate::model::unix_time_millis(),
                    result: result.clone(),
                    baseline: Value::Null,
                    observation: AutomationObservation::default(),
                    initial_observation_pending: true,
                    recording_ticket: None,
                },
            );
            Self::interest(&mut state, hub, instance_id);
            result
        };
        // Every wait needs an initial state/tree for failure evidence, even
        // when only a diagnostic event can satisfy its condition. Register
        // first; subsequent observations are driven by events, never polls.
        let headless = {
            let state = self.state.lock();
            state
                .sessions
                .get(&session_id)
                .and_then(|session| state.executors.get(&session.instance_id))
                .is_some_and(|executor| executor.instance.headless)
        };
        let sections = if headless {
            vec![wait_model_section(&condition).unwrap_or_else(|| "state".into())]
        } else {
            wait_sections(&condition)
        };
        let runtime = self.clone();
        let worker_hub = hub.clone();
        let wait_id = wait.wait_id;
        if let Err(error) = std::thread::Builder::new()
            .name("waku-automation-observe".into())
            .spawn(move || {
                if !runtime
                    .state
                    .lock()
                    .waits
                    .get(&wait_id)
                    .is_some_and(|w| is_pending(&w.result.status))
                {
                    return;
                }
                let outcome = runtime.call(
                    &worker_hub,
                    session_id,
                    AutomationCall::Snapshot { sections },
                );
                let mut state = runtime.state.lock();
                let Some(wait) = state
                    .waits
                    .get_mut(&wait_id)
                    .filter(|wait| is_pending(&wait.result.status))
                else {
                    return;
                };
                wait.initial_observation_pending = false;
                if let Err(error) = outcome {
                    if transient_observation_error(&error) || Instant::now() >= wait.deadline {
                        if let Some(bundle) = error.bundle {
                            merge_observation(
                                &mut wait.observation,
                                &AutomationObservation {
                                    observed: bundle.observed,
                                    tree_diff: bundle.tree_diff,
                                    state_diff: Value::Null,
                                },
                            );
                        }
                    } else {
                        runtime.fail_wait(&mut state, &worker_hub, wait_id, error);
                    }
                }
                if state.waits.get(&wait_id).is_some_and(|wait| {
                    is_pending(&wait.result.status) && timeout_ready(wait, Instant::now())
                }) {
                    runtime.fail_wait(&mut state, &worker_hub, wait_id, wait_timeout(wait_id));
                }
            })
        {
            let mut state = self.state.lock();
            self.fail_wait(
                &mut state,
                hub.as_ref(),
                wait.wait_id,
                AutomationError::new("worker_failed", error.to_string()),
            );
        }
        Ok(wait)
    }

    fn observe_waits(
        &self,
        state: &mut State,
        hub: &Hub,
        instance_id: Uuid,
        observation: &AutomationObservation,
        stream: Option<AutomationStream>,
        diagnostic: Option<&DiagnosticEvent>,
    ) {
        let sessions: HashMap<_, _> = state
            .sessions
            .iter()
            .filter(|(_, s)| s.instance_id == instance_id)
            .map(|(id, s)| (*id, (s.sequence, s.subscriber)))
            .collect();
        let observation_error = observation
            .observed
            .get("observationError")
            .and_then(|error| serde_json::from_value::<AutomationError>(error.clone()).ok());
        // A late observation may explain a timeout, but must never satisfy
        // its condition. Retain it before finalizing the failure; otherwise
        // short waits could lose their only in-flight bootstrap snapshot.
        if observation_error.is_none() && !observation.observed.is_null() {
            for wait in state.waits.values_mut().filter(|wait| {
                is_pending(&wait.result.status) && sessions.contains_key(&wait.session_id)
            }) {
                let observation = selected_wait_observation(&wait.condition, observation);
                if wait.baseline.is_null()
                    || (!wait.baseline["tree"]["nodes"].is_array()
                        && observation.observed["tree"]["nodes"].is_array())
                {
                    wait.baseline = observation.observed.clone();
                }
                merge_observation(&mut wait.observation, &observation);
            }
        }
        let elapsed: Vec<_> = state
            .waits
            .iter()
            .filter(|(_, wait)| {
                is_pending(&wait.result.status)
                    && sessions.contains_key(&wait.session_id)
                    && timeout_ready(wait, Instant::now())
            })
            .map(|(id, _)| *id)
            .collect();
        for id in elapsed {
            self.fail_wait(state, hub, id, wait_timeout(id));
        }
        if let Some(error) = observation_error {
            let affected: Vec<_> = state
                .waits
                .iter()
                .filter(|(_, wait)| {
                    is_pending(&wait.result.status)
                        && sessions.contains_key(&wait.session_id)
                        && !matches!(wait.condition, AutomationCondition::Diagnostic { .. })
                        && (stream != Some(AutomationStream::Model)
                            || wait_model_section(&wait.condition).is_some_and(|section| {
                                observation
                                    .observed
                                    .get("observationSection")
                                    .and_then(Value::as_str)
                                    .is_none_or(|affected| affected == section)
                            }))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in affected {
                if transient_observation_error(&error) {
                    if let Some(wait) = state.waits.get_mut(&id) {
                        merge_observation(&mut wait.observation, observation);
                    }
                } else {
                    self.fail_wait(state, hub, id, error.clone());
                }
            }
            return;
        }
        let mut completed = false;
        for wait in state.waits.values_mut().filter(|wait| {
            is_pending(&wait.result.status) && sessions.contains_key(&wait.session_id)
        }) {
            if Instant::now() < wait.deadline
                && condition_matches(&wait.condition, &wait.observation.observed, diagnostic)
            {
                let (sequence, subscriber) = sessions[&wait.session_id];
                wait.result.status = AutomationStatus::Succeeded;
                wait.result.cursor = AutomationCursor {
                    epoch: hub.epoch,
                    sequence,
                };
                wait.result.result = Some(diagnostic.map_or_else(
                    || wait.observation.observed.clone(),
                    |e| serde_json::to_value(e).unwrap_or(Value::Null),
                ));
                // The result owns the final observation. Retain no duplicate
                // snapshots after resolution.
                wait.baseline = Value::Null;
                wait.observation = AutomationObservation::default();
                if let Some(subscriber) = subscriber {
                    hub.send_to(
                        subscriber,
                        ServerMessage::AutomationWaitResolved {
                            session_id: wait.session_id,
                            wait: wait.result.clone(),
                        },
                    );
                }
                completed = true;
            }
        }
        if completed {
            let ids: Vec<_> = state.waits.keys().copied().collect();
            for id in ids {
                finish_recorded_wait(state, id);
            }
            Self::interest(state, hub, instance_id);
            self.changed.notify_all();
        }
    }

    fn fail_wait(&self, state: &mut State, hub: &Hub, wait_id: Uuid, error: AutomationError) {
        let Some(wait) = state
            .waits
            .get_mut(&wait_id)
            .filter(|w| is_pending(&w.result.status))
        else {
            return;
        };
        wait.observation.tree_diff =
            tree_diff(&wait.baseline["tree"], &wait.observation.observed["tree"]);
        wait.result.error = Some(self.evidence(
            hub,
            error,
            &wait.observation,
            Some(wait.started_ms.saturating_sub(1000)),
        ));
        wait.result.status = AutomationStatus::Failed;
        if let Some(session) = state.sessions.get(&wait.session_id) {
            wait.result.cursor = AutomationCursor {
                epoch: hub.epoch,
                sequence: session.sequence,
            };
            if let Some(subscriber) = session.subscriber {
                hub.send_to(
                    subscriber,
                    ServerMessage::AutomationWaitResolved {
                        session_id: wait.session_id,
                        wait: wait.result.clone(),
                    },
                );
            }
        }
        let instance_id = state.sessions.get(&wait.session_id).map(|s| s.instance_id);
        wait.baseline = Value::Null;
        wait.observation = AutomationObservation::default();
        if let Some(instance_id) = instance_id {
            Self::interest(state, hub, instance_id);
        }
        finish_recorded_wait(state, wait_id);
        self.changed.notify_all();
    }

    pub(super) fn tick(&self, hub: &Hub) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut state = self.state.lock();
        let now = Instant::now();
        let elapsed: Vec<_> = state
            .waits
            .iter()
            .filter(|(_, w)| is_pending(&w.result.status) && timeout_ready(w, now))
            .map(|(id, _)| *id)
            .collect();
        for id in elapsed {
            self.fail_wait(&mut state, hub, id, wait_timeout(id));
        }
        let expired: Vec<_> = state
            .sessions
            .iter()
            .filter(|(id, s)| {
                s.subscriber.is_none()
                    && now.duration_since(s.touched) >= SESSION_IDLE
                    && !state
                        .waits
                        .values()
                        .any(|w| w.session_id == **id && is_pending(&w.result.status))
                    && !state
                        .runs
                        .values()
                        .any(|r| r.session_id == **id && is_pending(&r.result.status))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(session) = state.sessions.remove(&id) {
                state.waits.retain(|_, w| w.session_id != id);
                state.runs.retain(|_, r| r.session_id != id);
                state.recordings.remove(&id);
                Self::interest(&mut state, hub, session.instance_id);
            }
        }
    }

    fn start_run(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        session_id: Uuid,
        scenario: AutomationScenario,
    ) -> Result<AutomationRun> {
        self.start_run_plan(hub, session_id, scenario, None)
    }

    fn start_run_plan(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        session_id: Uuid,
        scenario: AutomationScenario,
        replay: Option<(AutomationReplay, BTreeMap<String, Value>)>,
    ) -> Result<AutomationRun> {
        if scenario.steps.is_empty() || scenario.steps.len() > MAX_AUTOMATION_STEPS {
            return Err(AutomationError::new(
                "invalid_scenario",
                format!("scenario must contain 1..={MAX_AUTOMATION_STEPS} steps"),
            ));
        }
        if serde_json::to_vec(&scenario).map_or(true, |b| b.len() > MAX_SESSION_BYTES) {
            return Err(AutomationError::new(
                "invalid_scenario",
                "scenario exceeds 1 MiB",
            ));
        }
        let run = {
            let mut state = self.state.lock();
            if state
                .recordings
                .get(&session_id)
                .is_some_and(Recorder::blocks_session)
            {
                return Err(AutomationError::new(
                    "recording_busy",
                    "stop or cancel recording before starting a scenario",
                ));
            }
            let session = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| expired(session_id))?;
            let cursor = AutomationCursor {
                epoch: hub.epoch,
                sequence: session.sequence,
            };
            let instance_id = session.instance_id;
            if !state
                .executors
                .get(&instance_id)
                .is_some_and(|e| e.subscriber.is_some())
            {
                return Err(unavailable(instance_id));
            }
            if state.runs.len() >= MAX_RUNS {
                let completed = state
                    .runs
                    .iter()
                    .find(|(_, r)| !is_pending(&r.result.status))
                    .map(|(id, _)| *id);
                if let Some(id) = completed {
                    state.runs.remove(&id);
                } else {
                    return Err(AutomationError::new(
                        "capacity",
                        "too many running automation scenarios",
                    ));
                }
            }
            let result = AutomationRun {
                run_id: Uuid::new_v4(),
                name: scenario.name,
                status: AutomationStatus::Pending,
                steps: vec![],
                cursor,
                error: None,
            };
            state.runs.insert(
                result.run_id,
                Run {
                    session_id,
                    result: result.clone(),
                    wait_id: None,
                },
            );
            Self::interest(&mut state, hub, instance_id);
            result
        };
        let runtime = self.clone();
        let worker_hub = hub.clone();
        let run_id = run.run_id;
        if let Err(error) = std::thread::Builder::new()
            .name("waku-automation-scenario".into())
            .spawn(move || {
                runtime.run_steps(&worker_hub, session_id, run_id, scenario.steps, replay)
            })
        {
            let mut state = self.state.lock();
            if let Some(run) = state.runs.get_mut(&run_id) {
                run.result.status = AutomationStatus::Failed;
                run.result.error = Some(self.evidence(
                    hub,
                    AutomationError::new("worker_failed", error.to_string()),
                    &AutomationObservation::default(),
                    None,
                ));
            }
            Self::publish_run(&mut state, hub, run_id);
        }
        Ok(run)
    }

    fn run_steps(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        session_id: Uuid,
        run_id: Uuid,
        steps: Vec<AutomationStep>,
        replay: Option<(AutomationReplay, BTreeMap<String, Value>)>,
    ) {
        let mut prior_results = vec![];
        for (index, step) in steps.into_iter().enumerate() {
            if !self
                .state
                .lock()
                .runs
                .get(&run_id)
                .is_some_and(|r| is_pending(&r.result.status))
            {
                return;
            }
            let resolved = match &replay {
                Some((replay, parameters)) => {
                    automation_recording::resolve_step(replay, index, parameters, &prior_results)
                }
                None => Ok(step.clone()),
            };
            let outcome: Result<Value> = resolved.and_then(|resolved| match resolved.operation {
                AutomationStepOperation::Call { call } => self.call_for_run(hub, session_id, call, Some(run_id)).map(|result| json!({"cursor":result.cursor,"value":result.execution.value,"treeDiff":result.execution.observation.tree_diff,"stateDiff":result.execution.observation.state_diff})),
                AutomationStepOperation::Wait { condition, timeout_ms } => {
                    match self.start_wait(hub, session_id, condition, timeout_ms) {
                        Err(error) => Err(error),
                        Ok(wait) => {
                            let mut state = self.state.lock();
                            if let Some(run) = state.runs.get_mut(&run_id).filter(|r| is_pending(&r.result.status)) { run.wait_id = Some(wait.wait_id); }
                            else {
                                state.waits.remove(&wait.wait_id);
                                if let Some(instance_id) = state.sessions.get(&session_id).map(|s| s.instance_id) {
                                    Self::interest(&mut state, hub, instance_id);
                                }
                                return Err(AutomationError::new("run_aborted", "scenario was aborted"));
                            }
                            loop {
                                if !state.runs.get(&run_id).is_some_and(|r| is_pending(&r.result.status)) { return Err(AutomationError::new("run_aborted", "scenario was aborted")); }
                                let Some(wait) = state.waits.get(&wait.wait_id) else { break Err(AutomationError::new("wait_expired", "scenario wait was removed")); };
                                if !is_pending(&wait.result.status) {
                                    break match &wait.result.error {
                                        Some(error) => Err(error.clone()),
                                        None => Ok(json!({"waitId":wait.result.wait_id,"cursor":wait.result.cursor})),
                                    };
                                }
                                self.changed.wait(&mut state);
                            }
                        }
                    }
                }
            });
            if let Ok(value) = &outcome {
                prior_results.push(value.clone());
            }
            let mut state = self.state.lock();
            let cursor = state.sessions.get(&session_id).map(|s| AutomationCursor {
                epoch: hub.epoch,
                sequence: s.sequence,
            });
            let Some(run) = state
                .runs
                .get_mut(&run_id)
                .filter(|r| is_pending(&r.result.status))
            else {
                return;
            };
            run.wait_id = None;
            if let Some(cursor) = cursor {
                run.result.cursor = cursor;
            }
            let outcome = match outcome {
                Ok(value)
                    if serde_json::to_vec(&value).map_or(true, |b| b.len() > MAX_EVENT_BYTES) =>
                {
                    Err(self.evidence(
                        hub,
                        AutomationError::new(
                            "result_too_large",
                            "scenario step result exceeds 256 KiB",
                        ),
                        &AutomationObservation::default(),
                        None,
                    ))
                }
                outcome => outcome,
            };
            let failed = outcome.is_err();
            match outcome {
                Ok(result) => run.result.steps.push(AutomationStepResult {
                    name: step.name,
                    status: AutomationStatus::Succeeded,
                    result: Some(result),
                    error: None,
                }),
                Err(error) => {
                    let error = if error.bundle.is_some() {
                        error
                    } else {
                        self.evidence(hub, error, &AutomationObservation::default(), None)
                    };
                    run.result.steps.push(AutomationStepResult {
                        name: step.name,
                        status: AutomationStatus::Failed,
                        result: None,
                        error: Some(error.clone()),
                    });
                    run.result.status = AutomationStatus::Failed;
                    run.result.error = Some(error);
                }
            }
            // Each completed step is visible immediately, and failure stops
            // before the next executor request can be dispatched.
            Self::publish_run(&mut state, hub, run_id);
            if failed {
                return;
            }
        }
        let mut state = self.state.lock();
        if let Some(run) = state
            .runs
            .get_mut(&run_id)
            .filter(|r| is_pending(&r.result.status))
        {
            run.result.status = AutomationStatus::Succeeded;
        }
        Self::publish_run(&mut state, hub, run_id);
        self.changed.notify_all();
    }

    fn publish_run(state: &mut State, hub: &Hub, run_id: Uuid) {
        if let Some(run) = state.runs.get(&run_id)
            && let Some(session) = state.sessions.get(&run.session_id)
        {
            if let Some(subscriber) = session.subscriber {
                hub.send_to(
                    subscriber,
                    ServerMessage::AutomationRunChanged {
                        session_id: run.session_id,
                        run: run.result.clone(),
                    },
                );
            }
            Self::interest(state, hub, session.instance_id);
        }
    }

    fn abort_run(&self, hub: &Hub, session_id: Uuid, run_id: Uuid) -> Result<AutomationRun> {
        let mut state = self.state.lock();
        let run = state
            .runs
            .get_mut(&run_id)
            .filter(|r| r.session_id == session_id)
            .ok_or_else(|| {
                AutomationError::new("run_expired", format!("run {run_id} is not retained"))
            })?;
        if is_pending(&run.result.status) {
            run.result.status = AutomationStatus::Aborted;
        }
        let wait_id = run.wait_id.take();
        let result = run.result.clone();
        if let Some(id) = wait_id
            && let Some(wait) = state.waits.get_mut(&id)
        {
            wait.result.status = AutomationStatus::Aborted;
            wait.baseline = Value::Null;
            wait.observation = AutomationObservation::default();
        }
        Self::publish_run(&mut state, hub, run_id);
        self.changed.notify_all();
        Ok(result)
    }
}

fn finish_record_admission(state: &mut State, session_id: Uuid) {
    if let Some(count) = state.recording_admissions.get_mut(&session_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.recording_admissions.remove(&session_id);
        }
    }
}

fn call_step_value(result: &AutomationCallResult) -> Value {
    json!({"cursor":result.cursor,"value":result.execution.value,"treeDiff":result.execution.observation.tree_diff,"stateDiff":result.execution.observation.state_diff})
}

fn finish_recorded_wait(state: &mut State, wait_id: Uuid) {
    let Some(wait) = state
        .waits
        .get_mut(&wait_id)
        .filter(|wait| !is_pending(&wait.result.status))
    else {
        return;
    };
    let Some(ticket) = wait.recording_ticket.take() else {
        return;
    };
    let outcome = match &wait.result.error {
        Some(error) => Err(error.clone()),
        None if matches!(wait.result.status, AutomationStatus::Succeeded) => Ok(Value::Null),
        None => Err(AutomationError::new(
            "wait_aborted",
            "recorded wait was aborted",
        )),
    };
    if let Some(recorder) = state.recordings.get_mut(&wait.session_id) {
        let _ = recorder.complete(ticket, outcome);
    }
}

fn transient_observation_error(error: &AutomationError) -> bool {
    matches!(error.code.as_str(), "app_not_ready" | "no_frame")
}

fn wait_timeout(wait_id: Uuid) -> AutomationError {
    AutomationError::new(
        "wait_timeout",
        format!("wait {wait_id} timed out before its condition was observed"),
    )
}

fn timeout_ready(wait: &Wait, now: Instant) -> bool {
    now >= wait.deadline
        && (!wait.initial_observation_pending
            || (wait.observation.observed["tree"]["nodes"].is_array()
                && wait.observation.observed["state"].is_object())
            || now >= wait.deadline + WAIT_EVIDENCE_GRACE)
}

fn wait_sections(condition: &AutomationCondition) -> Vec<String> {
    let mut sections = vec![
        "tree".into(),
        "route".into(),
        "composer".into(),
        "panels".into(),
        "toast".into(),
    ];
    if let Some(section) = wait_model_section(condition)
        && !sections.contains(&section)
    {
        sections.push(section);
    }
    sections
}

fn wait_model_section(condition: &AutomationCondition) -> Option<String> {
    if let AutomationCondition::State { pointer, .. }
    | AutomationCondition::StateContains { pointer, .. } = condition
    {
        Some(
            pointer
                .strip_prefix('/')
                .and_then(|p| p.split('/').next())
                .filter(|part| !part.is_empty())
                .map(|part| part.replace("~1", "/").replace("~0", "~"))
                .unwrap_or_else(|| "state".into()),
        )
    } else {
        None
    }
}

fn evidence_session_id(observed: &Value) -> Option<Uuid> {
    // An explicit null selection is authoritative; only older observations
    // lacking the health projection may fall back to a full session snapshot.
    let id = match observed.pointer("/health/sessions/selected") {
        Some(selected) => selected.get("sessionId"),
        None => observed.pointer("/state/session/id"),
    };
    id.and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
}

fn selected_wait_observation(
    condition: &AutomationCondition,
    observation: &AutomationObservation,
) -> AutomationObservation {
    let sections = wait_sections(condition);
    let all = sections.iter().any(|section| section == "state");
    let observed = match observation.observed.as_object() {
        Some(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if key == "state" && !all {
                        value.as_object().map_or_else(
                            || value.clone(),
                            |state| {
                                Value::Object(
                                    state
                                        .iter()
                                        .filter(|(name, _)| sections.contains(name))
                                        .map(|(name, value)| (name.clone(), value.clone()))
                                        .collect(),
                                )
                            },
                        )
                    } else {
                        value.clone()
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        None => observation.observed.clone(),
    };
    AutomationObservation {
        observed,
        tree_diff: Value::Null,
        state_diff: Value::Null,
    }
}

// Observations may select state sections or omit the tree. Only pending waits
// retain this bounded evidence, never the instance registry.
fn merge_observation(previous: &mut AutomationObservation, current: &AutomationObservation) {
    let mut observed = current.observed.clone();
    if !observed.is_object() {
        *previous = current.clone();
        return;
    }
    if let (Some(old), Some(new)) = (
        previous.observed["state"].as_object(),
        observed["state"].as_object_mut(),
    ) {
        for (key, value) in old {
            new.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    if observed["tree"].is_null() && !previous.observed["tree"].is_null() {
        observed["tree"] = previous.observed["tree"].clone();
    }
    let merged = AutomationObservation {
        observed,
        tree_diff: current.tree_diff.clone(),
        state_diff: current.state_diff.clone(),
    };
    // Selected sections must not accumulate beyond one bounded observation.
    *previous = if serde_json::to_vec(&merged)
        .is_ok_and(|bytes| bytes.len() <= MAX_AUTOMATION_OBSERVATION_BYTES)
    {
        merged
    } else {
        current.clone()
    };
}

fn unique_streams(streams: Vec<AutomationStream>) -> Vec<AutomationStream> {
    let mut unique = vec![];
    for stream in streams {
        if !unique.contains(&stream) {
            unique.push(stream);
        }
    }
    unique
}
fn delta(observation: &AutomationObservation) -> Value {
    let mut delta = json!({"treeDiff": observation.tree_diff, "stateDiff": observation.state_diff});
    if let Some(error) = observation.observed.get("observationError") {
        delta["observationError"] = error.clone();
        if let Some(section) = observation.observed.get("observationSection") {
            delta["observationSection"] = section.clone();
        }
    }
    delta
}

fn condition_matches(
    condition: &AutomationCondition,
    snapshot: &Value,
    diagnostic: Option<&DiagnosticEvent>,
) -> bool {
    let nodes = snapshot.pointer("/tree/nodes").and_then(Value::as_array);
    let node = |id: &str| {
        nodes.and_then(|nodes| {
            nodes
                .iter()
                .find(|n| n["element_id"].as_str().or_else(|| n["id"].as_str()) == Some(id))
        })
    };
    match condition {
        AutomationCondition::Node { id, present } => {
            nodes.is_some() && node(id).is_some() == *present
        }
        AutomationCondition::Text { id, contains } => node(id).is_some_and(|n| {
            ["label", "value"]
                .iter()
                .any(|key| n[key].as_str().is_some_and(|s| s.contains(contains)))
        }),
        AutomationCondition::State { pointer, equals } => snapshot["state"]
            .pointer(pointer)
            .is_some_and(|value| value == equals),
        AutomationCondition::StateContains { pointer, contains } => {
            snapshot["state"].pointer(pointer).is_some_and(|value| {
                value.as_str().is_some_and(|s| s.contains(contains))
                    || value.as_array().is_some_and(|a| {
                        a.iter()
                            .any(|v| v.as_str().is_some_and(|s| s.contains(contains)))
                    })
            })
        }
        AutomationCondition::Frame { after } => snapshot
            .pointer("/tree/frame")
            .and_then(Value::as_u64)
            .is_some_and(|frame| frame > *after),
        AutomationCondition::Diagnostic { query } => diagnostic.is_some_and(|event| {
            !crate::diagnostic_queries::logs(
                event.epoch,
                event.sequence,
                &VecDeque::from([event.clone()]),
                query,
            )
            .events
            .is_empty()
        }),
    }
}

// Failure-time comparison only: these two observations belong to one bounded
// wait. There is no background daemon tree or UI baseline per instance.
fn tree_diff(before: &Value, after: &Value) -> Value {
    fn index(tree: &Value) -> std::collections::BTreeMap<(String, usize), &Value> {
        let mut counts = HashMap::new();
        tree["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, node)| {
                let key = node["element_id"]
                    .as_str()
                    .or_else(|| node["node"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| index.to_string());
                let count = counts.entry(key.clone()).or_insert(0);
                let entry = ((key, *count), node);
                *count += 1;
                entry
            })
            .collect()
    }
    let (a, b) = (index(before), index(after));
    let available = before["nodes"].is_array() && after["nodes"].is_array();
    let added: Vec<_> = b
        .iter()
        .filter(|(key, _)| available && !a.contains_key(*key))
        .map(|(_, node)| *node)
        .collect();
    let removed: Vec<_> = a
        .iter()
        .filter(|(key, _)| available && !b.contains_key(*key))
        .map(|(_, node)| *node)
        .collect();
    let changed: Vec<_> = b
        .iter()
        .filter_map(|(key, node)| {
            a.get(key)
                .filter(|previous| available && **previous != *node)
                .map(|previous| json!({"before":previous,"after":node}))
        })
        .collect();
    json!({"baselineAvailable":before["nodes"].is_array(),"observedAvailable":after["nodes"].is_array(),"beforeFrame":before["frame"],"afterFrame":after["frame"],"focusBefore":before["focus"],"focusAfter":after["focus"],"added":added,"removed":removed,"changed":changed})
}

#[cfg(test)]
#[path = "automation_tests.rs"]
mod tests;
