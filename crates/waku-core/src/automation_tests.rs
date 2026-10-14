use super::*;
use crate::server::{EventSink, ServerOptions, Subscriber, serve};
use crossbeam_channel::Receiver;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use waku_client::{AutomationExecutorMessage, AutomationMessage, DaemonClient};

struct NoBackend;
impl Backend for NoBackend {
    fn handle(&self, _: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
        anyhow::bail!("automation must be handled by the runtime")
    }
}
fn instance() -> AutomationInstance {
    AutomationInstance {
        instance_id: Uuid::new_v4(),
        checkout: "/test/checkout".into(),
        database: "/test/checkout/temp/app.db".into(),
        pid: 42,
        build: "build-one".into(),
        version: "test".into(),
        executor_connected: false,
        scratch: false,
        headless: false,
    }
}
fn observation(frame: u64, label: &str) -> AutomationObservation {
    AutomationObservation {
        observed: json!({"tree":{"frame":frame,"nodes":[{"element_id":"composer.send","label":label}]},"state":{"composer":{"text":label}},"health":{"ready":true}}),
        tree_diff: json!({"changed":[{"after":{"element_id":"composer.send","label":label}}]}),
        state_diff: json!([{"pointer":"/composer/text","value":label}]),
    }
}
fn ok(observation: AutomationObservation) -> AutomationExecutorOutcome {
    AutomationExecutorOutcome::Ok {
        execution: AutomationExecution {
            value: json!({"ok":true}),
            observation,
        },
    }
}
fn recv(receiver: &Receiver<ServerMessage>) -> ServerMessage {
    receiver.recv_timeout(Duration::from_secs(3)).unwrap()
}
struct Fixture {
    hub: Arc<Hub>,
    backend: Arc<dyn Backend>,
    executor: u64,
    incoming: Receiver<ServerMessage>,
    instance: AutomationInstance,
}
impl Fixture {
    fn new() -> Self {
        let hub = Arc::new(Hub::default());
        let (executor, incoming) = Self::subscriber(&hub);
        let fixture = Self {
            hub,
            backend: Arc::new(NoBackend),
            executor,
            incoming,
            instance: instance(),
        };
        assert!(matches!(
            fixture.command(
                executor,
                AutomationCommand::RegisterExecutor {
                    instance: fixture.instance.clone()
                }
            ),
            AutomationResult::Registered { .. }
        ));
        assert!(matches!(
            recv(&fixture.incoming),
            ServerMessage::AutomationInterest {
                attached: false,
                ..
            }
        ));
        fixture
    }
    fn subscriber(hub: &Hub) -> (u64, Receiver<ServerMessage>) {
        let (sender, receiver) = bounded(1024);
        let (subscriber, _) = Subscriber::new(sender);
        (hub.subscribe(&[], None, subscriber), receiver)
    }
    fn command(&self, subscriber: u64, command: AutomationCommand) -> AutomationResult {
        self.hub
            .automation
            .command(&self.hub, &self.backend, command, subscriber)
    }
    fn attach(&self, subscriber: u64) -> AutomationSession {
        match self.command(
            subscriber,
            AutomationCommand::Attach {
                instance_id: self.instance.instance_id,
            },
        ) {
            AutomationResult::Attached { session, .. } => session,
            other => panic!("{other:?}"),
        }
    }
    fn event(&self, frame: u64, label: &str) {
        self.hub.automation.event(
            &self.hub,
            self.executor,
            AutomationExecutorEvent {
                instance_id: self.instance.instance_id,
                stream: AutomationStream::Ui,
                observation: observation(frame, label),
            },
        );
    }
}

#[test]
fn automation_surface_respects_build_mode() {
    let hub = Arc::new(Hub::default());
    let backend: Arc<dyn Backend> = Arc::new(NoBackend);
    let result = hub
        .automation
        .command(&hub, &backend, AutomationCommand::Instances, 0);
    if cfg!(debug_assertions) {
        assert!(
            matches!(result, AutomationResult::Instances { instances } if instances.is_empty())
        );
    } else {
        assert!(matches!(result, AutomationResult::Error { error } if error.code == "unavailable"));
    }
}

#[test]
#[cfg(debug_assertions)]
fn attached_sessions_enable_tree_but_only_subscriptions_request_pushes() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    assert!(
        matches!(recv(&f.incoming), ServerMessage::AutomationInterest { attached:true, streams, .. } if streams.is_empty())
    );
    f.command(
        client,
        AutomationCommand::Subscribe {
            session_id: session.session_id,
            streams: vec![AutomationStream::Ui],
        },
    );
    assert!(
        matches!(recv(&f.incoming), ServerMessage::AutomationInterest { attached:true, streams, .. } if streams == vec![AutomationStream::Ui])
    );
    f.command(
        client,
        AutomationCommand::Subscribe {
            session_id: session.session_id,
            streams: vec![],
        },
    );
    assert!(
        matches!(recv(&f.incoming), ServerMessage::AutomationInterest { attached:true, streams, .. } if streams.is_empty())
    );
    f.hub.unsubscribe(client);
    assert!(
        matches!(recv(&f.incoming), ServerMessage::AutomationInterest { attached:false, streams, .. } if streams.is_empty())
    );
    assert!(
        f.hub
            .automation
            .state
            .lock()
            .sessions
            .contains_key(&session.session_id)
    );
    let (resumed, _) = Fixture::subscriber(&f.hub);
    f.command(
        resumed,
        AutomationCommand::Resume {
            session_id: session.session_id,
            after: Some(session.cursor),
        },
    );
    assert!(matches!(
        recv(&f.incoming),
        ServerMessage::AutomationInterest { attached: true, .. }
    ));
    f.command(
        resumed,
        AutomationCommand::Detach {
            session_id: session.session_id,
        },
    );
    assert!(matches!(
        recv(&f.incoming),
        ServerMessage::AutomationInterest {
            attached: false,
            ..
        }
    ));
}

#[test]
#[cfg(debug_assertions)]
fn sessions_resume_independently_and_name_retention_and_lifetime_expiry() {
    let f = Fixture::new();
    let (a, _) = Fixture::subscriber(&f.hub);
    let (b, _) = Fixture::subscriber(&f.hub);
    let first = f.attach(a);
    let second = f.attach(b);
    for frame in 1..=3 {
        f.event(frame, "send");
    }
    let diff = |subscriber, session_id, after| match f.command(
        subscriber,
        AutomationCommand::Diff {
            session_id,
            after,
            streams: vec![AutomationStream::Ui],
        },
    ) {
        AutomationResult::Diff { diff } => diff,
        other => panic!("{other:?}"),
    };
    assert_eq!(diff(a, first.session_id, first.cursor).events.len(), 3);
    assert_eq!(diff(b, second.session_id, second.cursor).events.len(), 3);
    let after = diff(a, first.session_id, first.cursor).cursor;
    f.hub.unsubscribe(a);
    f.event(4, "done");
    let (a, _) = Fixture::subscriber(&f.hub);
    match f.command(
        a,
        AutomationCommand::Resume {
            session_id: first.session_id,
            after: Some(after),
        },
    ) {
        AutomationResult::Attached { replay, .. } => {
            assert_eq!(replay.events.len(), 1);
            assert_eq!(replay.events[0].cursor.sequence, 4);
            assert!(replay.expiry.is_none());
        }
        other => panic!("{other:?}"),
    }
    for frame in 5..=(MAX_AUTOMATION_EVENTS as u64 + 6) {
        f.event(frame, "send");
    }
    let expired = diff(a, first.session_id, first.cursor);
    assert!(expired.expiry.is_some());
    assert_eq!(expired.events.len(), MAX_AUTOMATION_EVENTS);
    assert_eq!(expired.events[0].cursor.sequence, 7);
    f.hub.unsubscribe(a);
    f.hub
        .automation
        .state
        .lock()
        .sessions
        .get_mut(&first.session_id)
        .unwrap()
        .touched = Instant::now() - SESSION_IDLE;
    f.hub.automation.tick(&f.hub);
    assert!(
        matches!(f.command(a, AutomationCommand::Resume { session_id:first.session_id, after:Some(after) }), AutomationResult::Error { error } if error.code == "session_expired")
    );
    assert!(
        f.hub
            .automation
            .state
            .lock()
            .sessions
            .contains_key(&second.session_id)
    );
}

#[test]
#[cfg(debug_assertions)]
fn disconnected_executor_fails_inflight_call_and_never_returns_stale_state() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    recv(&f.incoming);
    let hub = f.hub.clone();
    let call = std::thread::spawn(move || {
        hub.automation
            .call(&hub, session.session_id, AutomationCall::Describe)
    });
    let request_id = match recv(&f.incoming) {
        ServerMessage::AutomationExecutorRequest { request } => request.request_id,
        other => panic!("{other:?}"),
    };
    f.hub.unsubscribe(f.executor);
    f.hub.automation.reply(
        &f.hub,
        f.executor,
        AutomationExecutorReply {
            request_id,
            outcome: ok(observation(1, "stale")),
        },
    );
    let error = call.join().unwrap().unwrap_err();
    assert_eq!(error.code, "executor_unavailable");
    assert!(error.message.contains(&f.instance.instance_id.to_string()));
    assert!(error.bundle.is_some());
    assert!(
        matches!(f.command(client, AutomationCommand::Call { session_id:session.session_id, call:AutomationCall::Describe }), AutomationResult::Error { error } if error.code == "executor_unavailable")
    );
}

#[test]
#[cfg(debug_assertions)]
fn deadlines_fail_before_a_late_matching_event_and_keep_tree_evidence() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    let wait = f
        .hub
        .automation
        .start_wait(
            &f.hub,
            session.session_id,
            AutomationCondition::Diagnostic {
                query: DiagnosticQuery {
                    source: Some("turn".into()),
                    ..Default::default()
                },
            },
            60_000,
        )
        .unwrap();
    let mut state = f.hub.automation.state.lock();
    let pending = state.waits.get_mut(&wait.wait_id).unwrap();
    pending.baseline = observation(1, "before").observed;
    pending.observation = observation(2, "after");
    pending.deadline = Instant::now() - Duration::from_millis(1);
    drop(state);
    f.hub.record_diagnostic(DiagnosticRecord::new(
        DiagnosticLevel::Info,
        DiagnosticOrigin::Daemon { pid: 1 },
        "turn",
        "settled",
    ));
    let state = f.hub.automation.state.lock();
    let result = &state.waits[&wait.wait_id].result;
    assert!(matches!(result.status, AutomationStatus::Failed));
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "wait_timeout");
    let bundle = error.bundle.as_ref().unwrap();
    assert_eq!(bundle.observed["tree"]["frame"], 2);
    assert_eq!(bundle.tree_diff["changed"].as_array().unwrap().len(), 1);
    assert_eq!(
        bundle.recent_events.as_ref().unwrap().events[0]
            .record
            .message,
        "settled"
    );
    assert!(state.waits[&wait.wait_id].observation.observed.is_null());
}

#[test]
fn state_wait_sections_and_partial_observations_preserve_requested_model() {
    let sections = wait_sections(&AutomationCondition::State {
        pointer: "/model/sessions/0/status".into(),
        equals: json!("working"),
    });
    assert!(sections.iter().any(|s| s == "model"));
    let mut before = observation(1, "send");
    before.observed["state"]["model"] = json!({"newField":true});
    let current = AutomationObservation {
        observed: json!({"state":{"composer":{"text":"updated"}},"tree":null}),
        ..Default::default()
    };
    merge_observation(&mut before, &current);
    assert_eq!(before.observed["state"]["model"]["newField"], true);
    assert_eq!(before.observed["state"]["composer"]["text"], "updated");
    assert_eq!(before.observed["tree"]["frame"], 1);
}

#[test]
fn wait_evidence_prefers_current_health_and_respects_an_explicit_empty_selection() {
    let old_session = Uuid::new_v4();
    let current_session = Uuid::new_v4();
    let mut observed = json!({"state":{"session":{"id":old_session}}});
    assert_eq!(evidence_session_id(&observed), Some(old_session));
    observed["health"] = json!({"sessions":{"selected":{"sessionId":current_session}}});
    assert_eq!(evidence_session_id(&observed), Some(current_session));
    observed["health"]["sessions"]["selected"] = Value::Null;
    assert_eq!(evidence_session_id(&observed), None);
}

#[test]
fn implicit_wait_evidence_omits_unrequested_history_but_explicit_state_fields_survive() {
    let condition = AutomationCondition::Node {
        id: "target".into(),
        present: true,
    };
    let mut current = observation(1, "before");
    current.observed["state"]["session"] =
        json!({"messages":["x".repeat(MAX_AUTOMATION_OBSERVATION_BYTES)]});
    current.observed["state"]["model"] = json!({"newField":{"ready":true}});
    let lightweight = selected_wait_observation(&condition, &current);
    assert!(lightweight.observed["state"].get("session").is_none());
    assert!(lightweight.observed["state"].get("model").is_none());
    assert!(serde_json::to_vec(&lightweight).unwrap().len() < 1024);
    let state = selected_wait_observation(
        &AutomationCondition::State {
            pointer: "/model/newField/ready".into(),
            equals: json!(true),
        },
        &current,
    );
    assert_eq!(state.observed["state"]["model"]["newField"]["ready"], true);
    assert!(state.observed["state"].get("session").is_none());
}

struct SocketServer {
    address: String,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl SocketServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let signal = shutdown.clone();
        let thread = std::thread::spawn(move || {
            serve(
                listener,
                "test-secret".into(),
                Arc::new(NoBackend),
                signal,
                ServerOptions::default(),
            )
            .unwrap()
        });
        Self {
            address,
            shutdown,
            thread: Some(thread),
        }
    }
    fn client(&self) -> DaemonClient {
        DaemonClient::connect(&self.address, "test-secret".into()).unwrap()
    }
}
impl Drop for SocketServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn executor_request(receiver: &Receiver<AutomationExecutorMessage>) -> AutomationExecutorRequest {
    loop {
        match receiver.recv_timeout(Duration::from_secs(3)).unwrap() {
            AutomationExecutorMessage::Request(request) => return request,
            AutomationExecutorMessage::Interest { .. } => {}
        }
    }
}
fn reply(
    app: &DaemonClient,
    request: AutomationExecutorRequest,
    outcome: AutomationExecutorOutcome,
) {
    app.reply_automation_executor(AutomationExecutorReply {
        request_id: request.request_id,
        outcome,
    })
    .unwrap();
}
fn final_run(receiver: &Receiver<AutomationMessage>) -> AutomationRun {
    loop {
        if let AutomationMessage::RunChanged(run) =
            receiver.recv_timeout(Duration::from_secs(3)).unwrap()
            && !is_pending(&run.status)
        {
            return run;
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_routes_action_diff_and_resolves_wait_without_blocking_another_call() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let absent = client.attach_automation(Uuid::new_v4()).unwrap_err();
    assert!(absent.to_string().contains("no executor is connected"));
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    client
        .automation(AutomationCommand::Subscribe {
            session_id: session.session_id,
            streams: vec![AutomationStream::Ui],
        })
        .unwrap();
    let wait = match client
        .automation(AutomationCommand::Wait {
            session_id: session.session_id,
            condition: AutomationCondition::Text {
                id: "composer.send".into(),
                contains: "done".into(),
            },
            timeout_ms: 5_000,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    };
    assert!(is_pending(&wait.status));
    // Keep the initial observation request unanswered while another call
    // reaches the same real socket, proving registration did not block RPC.
    let initial = executor_request(&incoming);
    let caller = client.clone();
    let action = std::thread::spawn(move || {
        caller
            .automation_call(
                session.session_id,
                AutomationCall::Key {
                    chord: "enter".into(),
                },
            )
            .unwrap()
    });
    let next = executor_request(&incoming);
    assert!(matches!(next.call, AutomationCall::Key { .. }));
    reply(&app, initial, ok(observation(1, "send")));
    reply(&app, next, ok(observation(2, "send")));
    let result = action.join().unwrap();
    assert!(result.cursor.sequence >= 1);
    assert_eq!(
        result.execution.observation.state_diff[0]["pointer"],
        "/composer/text"
    );
    assert!(
        matches!(client.automation(AutomationCommand::WaitResult { session_id:session.session_id, wait_id:wait.wait_id }).unwrap(), AutomationResult::Wait { wait } if is_pending(&wait.status))
    );
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: observation(3, "done"),
    })
    .unwrap();
    loop {
        if let AutomationMessage::WaitResolved(resolved) =
            messages.recv_timeout(Duration::from_secs(3)).unwrap()
        {
            assert_eq!(resolved.wait_id, wait.wait_id);
            assert!(matches!(resolved.status, AutomationStatus::Succeeded));
            assert_eq!(resolved.result.unwrap()["tree"]["frame"], 3);
            break;
        }
    }
    assert!(
        incoming
            .try_iter()
            .all(|m| matches!(m, AutomationExecutorMessage::Interest { .. }))
    );
}

#[test]
#[cfg(debug_assertions)]
fn websocket_scenarios_are_ordered_stop_at_failure_and_abort_pending_waits() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let step = |name: &str| AutomationStep {
        name: name.into(),
        operation: AutomationStepOperation::Call {
            call: AutomationCall::Key { chord: name.into() },
        },
    };
    client
        .automation(AutomationCommand::ScenarioStart {
            session_id: session.session_id,
            scenario: AutomationScenario {
                name: "ordered".into(),
                steps: vec![step("first"), step("fail"), step("never")],
            },
        })
        .unwrap();
    let first = executor_request(&incoming);
    assert!(matches!(&first.call,AutomationCall::Key { chord } if chord == "first"));
    reply(&app, first, ok(observation(1, "send")));
    let failing = executor_request(&incoming);
    assert!(matches!(&failing.call,AutomationCall::Key { chord } if chord == "fail"));
    reply(
        &app,
        failing,
        AutomationExecutorOutcome::Error {
            error: AutomationError::new("unknown_target", "deliberate failure"),
        },
    );
    let run = final_run(&messages);
    assert!(matches!(run.status, AutomationStatus::Failed));
    assert_eq!(
        run.steps
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "fail"]
    );
    assert!(run.error.unwrap().bundle.is_some());
    assert!(
        incoming
            .try_iter()
            .all(|m| matches!(m, AutomationExecutorMessage::Interest { .. }))
    );
    let result = client
        .automation(AutomationCommand::ScenarioStart {
            session_id: session.session_id,
            scenario: AutomationScenario {
                name: "abort".into(),
                steps: vec![
                    AutomationStep {
                        name: "wait".into(),
                        operation: AutomationStepOperation::Wait {
                            condition: AutomationCondition::Node {
                                id: "never".into(),
                                present: true,
                            },
                            timeout_ms: 60_000,
                        },
                    },
                    step("never"),
                ],
            },
        })
        .unwrap();
    let run_id = match result {
        AutomationResult::Scenario { run } => run.run_id,
        other => panic!("{other:?}"),
    };
    let initial = executor_request(&incoming);
    client
        .automation(AutomationCommand::ScenarioAbort {
            session_id: session.session_id,
            run_id,
        })
        .unwrap();
    reply(&app, initial, ok(observation(2, "send")));
    let run = final_run(&messages);
    assert_eq!(run.run_id, run_id);
    assert!(matches!(run.status, AutomationStatus::Aborted));
    assert!(run.steps.is_empty());
    assert!(
        incoming
            .try_iter()
            .all(|m| matches!(m, AutomationExecutorMessage::Interest { .. }))
    );
}

#[test]
#[cfg(debug_assertions)]
fn websocket_committed_core_smoke_fails_at_a_removed_semantic_identity() {
    let scenario: AutomationScenario =
        serde_json::from_str(include_str!("../../../rpa/scenarios/core-smoke.json")).unwrap();
    let names: Vec<_> = scenario
        .steps
        .iter()
        .map(|step| step.name.clone())
        .collect();
    let missing_step = scenario.steps.iter().position(|step| matches!(
        &step.operation,
        AutomationStepOperation::Wait { condition: AutomationCondition::Node { id, present: true }, .. }
            if id == "toggle-sidebar"
    )).expect("the committed smoke must pin the sidebar identity");
    assert!(
        missing_step + 1 < scenario.steps.len(),
        "the removed ID must leave a later smoke step to guard"
    );

    for remove_identity in [false, true] {
        let server = SocketServer::new();
        let app = server.client();
        let incoming = app.subscribe_automation_executor();
        let instance = app.register_automation_executor(instance()).unwrap();
        let client = server.client();
        let session = client.attach_automation(instance.instance_id).unwrap();
        let messages = client.subscribe_automation(session.session_id);
        let mut nodes = json!([
            {"element_id":"composer-field","role":"text_field","label":"Composer"},
            {"element_id":"send-or-stop","role":"button","label":"Send"},
            {"element_id":"toggle-sidebar","role":"button","label":"Sidebar"},
            {"element_id":"open-settings","role":"button","label":"Settings"},
        ]);
        if remove_identity {
            // Keep the rendered node and its role/label: only the semantic ID
            // is missing, rather than simulating an absent UI control.
            nodes[2].as_object_mut().unwrap().remove("element_id");
        }
        client
            .automation(AutomationCommand::ScenarioStart {
                session_id: session.session_id,
                scenario: scenario.clone(),
            })
            .unwrap();
        let expected_steps = if remove_identity {
            missing_step + 1
        } else {
            scenario.steps.len()
        };
        for frame in 1..=expected_steps {
            let request = executor_request(&incoming);
            assert!(matches!(request.call, AutomationCall::Snapshot { .. }));
            reply(
                &app,
                request,
                ok(AutomationObservation {
                    observed: json!({
                        "tree":{"frame":frame,"nodes":nodes},
                        "state":{"route":"task","composer":{"text":""}},
                        "health":{"ready":true},
                    }),
                    tree_diff: json!({"changed":[]}),
                    state_diff: json!([]),
                }),
            );
        }
        let run = final_run(&messages);
        assert_eq!(run.name, scenario.name);
        assert_eq!(
            run.steps
                .iter()
                .map(|step| step.name.clone())
                .collect::<Vec<_>>(),
            names[..expected_steps]
        );
        if remove_identity {
            assert!(matches!(run.status, AutomationStatus::Failed));
            assert!(
                run.steps[..missing_step]
                    .iter()
                    .all(|step| matches!(step.status, AutomationStatus::Succeeded))
            );
            let failed = &run.steps[missing_step];
            assert!(matches!(failed.status, AutomationStatus::Failed));
            let error = failed.error.as_ref().unwrap();
            assert_eq!(error.code, "wait_timeout");
            assert_eq!(run.error.as_ref().unwrap().code, error.code);
            let bundle = error
                .bundle
                .as_ref()
                .expect("the failed committed smoke must carry evidence");
            assert_eq!(bundle.observed["tree"]["nodes"], nodes);
            assert_eq!(bundle.observed["tree"]["frame"], expected_steps);
            assert_eq!(bundle.observed["tree"]["nodes"][2]["role"], "button");
            assert!(
                bundle.observed["tree"]["nodes"][2]
                    .get("element_id")
                    .is_none()
            );
            assert!(
                !run.steps
                    .iter()
                    .any(|step| step.name == names[missing_step + 1])
            );
        } else {
            assert!(matches!(run.status, AutomationStatus::Succeeded));
            assert!(run.error.is_none());
            assert!(
                run.steps
                    .iter()
                    .all(|step| matches!(step.status, AutomationStatus::Succeeded))
            );
        }
        app.automation_instances().unwrap(); // barrier after the terminal run
        assert!(
            incoming
                .try_iter()
                .all(|message| matches!(message, AutomationExecutorMessage::Interest { .. }))
        );
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_two_clients_replay_independently_after_transport_reconnect() {
    let server = SocketServer::new();
    let app = server.client();
    let _incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let first = server.client();
    let second = server.client();
    let a = first.attach_automation(instance.instance_id).unwrap();
    let b = second.attach_automation(instance.instance_id).unwrap();
    let events_a = first.subscribe_automation(a.session_id);
    let events_b = second.subscribe_automation(b.session_id);
    for (client, session) in [(&first, &a), (&second, &b)] {
        client
            .automation(AutomationCommand::Subscribe {
                session_id: session.session_id,
                streams: vec![AutomationStream::Ui],
            })
            .unwrap();
    }
    let publish = |frame| {
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id: instance.instance_id,
            stream: AutomationStream::Ui,
            observation: observation(frame, "send"),
        })
        .unwrap()
    };
    let sequence = |events: &Receiver<AutomationMessage>| match events
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
    {
        AutomationMessage::Event(event) => event.cursor.sequence,
        other => panic!("{other:?}"),
    };
    publish(1);
    assert_eq!(sequence(&events_a), 1);
    assert_eq!(sequence(&events_b), 1);
    let after = first.last_automation_cursor(a.session_id).unwrap();
    first.disconnect();
    publish(2);
    assert_eq!(sequence(&events_b), 2);
    let resumed = server.client();
    let events = resumed.subscribe_automation(a.session_id);
    let session = resumed
        .resume_automation(a.session_id, Some(after))
        .unwrap();
    assert_eq!(session.session_id, a.session_id);
    assert_eq!(sequence(&events), 2);
    publish(3);
    assert_eq!(sequence(&events), 3);
    assert_eq!(sequence(&events_b), 3);
    assert!(events.try_recv().is_err());
    assert!(events_b.try_recv().is_err());
}

#[test]
#[cfg(debug_assertions)]
fn websocket_executor_disconnect_returns_a_named_failure_for_pending_call() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let caller = client.clone();
    let pending = std::thread::spawn(move || {
        caller.automation_call(session.session_id, AutomationCall::Describe)
    });
    executor_request(&incoming);
    app.disconnect();
    let error = pending.join().unwrap().unwrap_err();
    let error = &error
        .downcast_ref::<waku_client::DaemonAutomationError>()
        .unwrap()
        .error;
    assert_eq!(error.code, "executor_unavailable");
    assert!(error.message.contains(&instance.instance_id.to_string()));
    assert!(error.bundle.is_some());
    assert!(!client.automation_instances().unwrap()[0].executor_connected);
}

#[test]
#[cfg(debug_assertions)]
fn oversized_delta_names_a_gap_instead_of_retaining_a_silent_hole() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    f.event(1, "before");
    f.event(2, &"x".repeat(MAX_EVENT_BYTES));
    f.event(3, "after");
    match f.command(
        client,
        AutomationCommand::Diff {
            session_id: session.session_id,
            after: session.cursor,
            streams: vec![],
        },
    ) {
        AutomationResult::Diff { diff } => {
            assert_eq!(diff.expiry.unwrap().oldest_sequence, 3);
            assert_eq!(diff.events.len(), 1);
            assert_eq!(diff.events[0].cursor.sequence, 3);
        }
        other => panic!("{other:?}"),
    }
    assert!(f.hub.state.lock().subscribers.contains_key(&client));
    assert!(f.hub.automation.state.lock().sessions[&session.session_id].bytes <= MAX_SESSION_BYTES);
}

#[test]
#[cfg(debug_assertions)]
fn a_reply_from_another_authenticated_client_cannot_complete_an_executor_call() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    recv(&f.incoming);
    let hub = f.hub.clone();
    let pending = std::thread::spawn(move || {
        hub.automation
            .call(&hub, session.session_id, AutomationCall::Describe)
    });
    let request_id = match recv(&f.incoming) {
        ServerMessage::AutomationExecutorRequest { request } => request.request_id,
        other => panic!("{other:?}"),
    };
    f.hub.automation.reply(
        &f.hub,
        client,
        AutomationExecutorReply {
            request_id,
            outcome: ok(observation(1, "spoofed")),
        },
    );
    assert!(
        f.hub
            .automation
            .state
            .lock()
            .calls
            .contains_key(&request_id)
    );
    f.hub.automation.reply(
        &f.hub,
        f.executor,
        AutomationExecutorReply {
            request_id,
            outcome: ok(observation(2, "real")),
        },
    );
    assert_eq!(
        pending
            .join()
            .unwrap()
            .unwrap()
            .execution
            .observation
            .observed["tree"]["frame"],
        2
    );
}

#[test]
#[cfg(debug_assertions)]
fn dead_unreferenced_registrations_do_not_exhaust_relaunch_capacity() {
    let f = Fixture::new();
    {
        let mut state = f.hub.automation.state.lock();
        while state.executors.len() < MAX_AUTOMATION_SESSIONS {
            let instance = instance();
            state.executors.insert(
                instance.instance_id,
                Executor {
                    instance,
                    subscriber: None,
                    interest: None,
                },
            );
        }
    }
    let fresh = instance();
    assert!(matches!(
        f.command(
            f.executor,
            AutomationCommand::RegisterExecutor { instance: fresh }
        ),
        AutomationResult::Registered { .. }
    ));
    assert_eq!(
        f.hub.automation.state.lock().executors.len(),
        MAX_AUTOMATION_SESSIONS
    );
}

#[test]
#[cfg(debug_assertions)]
fn oversized_model_push_fails_wait_with_the_last_bounded_evidence() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    let wait = f
        .hub
        .automation
        .start_wait(
            &f.hub,
            session.session_id,
            AutomationCondition::Diagnostic {
                query: DiagnosticQuery::default(),
            },
            60_000,
        )
        .unwrap();
    {
        let mut state = f.hub.automation.state.lock();
        let wait = state.waits.get_mut(&wait.wait_id).unwrap();
        wait.condition = AutomationCondition::State {
            pointer: "/model/ready".into(),
            equals: json!(true),
        };
        wait.baseline = observation(1, "send").observed;
        wait.observation = observation(2, "send");
    }
    f.hub.automation.event(&f.hub,f.executor,AutomationExecutorEvent {
        instance_id:f.instance.instance_id,stream:AutomationStream::Model,
        observation:AutomationObservation { observed:json!({"observationError":{"code":"observation_too_large","message":"select fewer snapshot sections"}}), ..Default::default() },
    });
    let state = f.hub.automation.state.lock();
    let result = &state.waits[&wait.wait_id].result;
    assert!(matches!(result.status, AutomationStatus::Failed));
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "observation_too_large");
    assert_eq!(error.bundle.as_ref().unwrap().observed["tree"]["frame"], 2);
    assert_eq!(
        state.sessions[&session.session_id]
            .events
            .back()
            .unwrap()
            .0
            .payload["observationError"]["code"],
        "observation_too_large"
    );
}

#[test]
#[cfg(debug_assertions)]
fn replies_commit_cursors_before_waking_workers_and_before_later_pushes() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    let mut receivers = vec![];
    for frame in 1..=2 {
        let request_id = Uuid::new_v4();
        let (reply, receiver) = bounded(1);
        f.hub.automation.state.lock().calls.insert(
            request_id,
            PendingCall {
                executor: f.executor,
                instance_id: f.instance.instance_id,
                session_id: session.session_id,
                reply,
            },
        );
        f.hub.automation.reply(
            &f.hub,
            f.executor,
            AutomationExecutorReply {
                request_id,
                outcome: ok(observation(frame, &frame.to_string())),
            },
        );
        receivers.push(receiver);
    }
    f.event(3, "3");
    for (index, receiver) in receivers.into_iter().enumerate() {
        assert_eq!(
            receiver.recv().unwrap().unwrap().cursor.sequence,
            index as u64 + 1
        );
    }
    let state = f.hub.automation.state.lock();
    let labels = state.sessions[&session.session_id]
        .events
        .iter()
        .map(|(event, _)| {
            event.payload["treeDiff"]["changed"][0]["after"]["label"]
                .as_str()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["1", "2", "3"]);
}

#[test]
#[cfg(debug_assertions)]
fn concurrent_diagnostics_preserve_source_sequence_in_the_automation_stream() {
    let f = Fixture::new();
    let (client, messages) = Fixture::subscriber(&f.hub);
    let session = f.attach(client);
    // Stop the first emitter between its diagnostic broadcast and automation
    // delivery. A second emitter must not overtake it at that boundary.
    let state = f.hub.automation.state.lock();
    let emit = |hub: Arc<Hub>, message: &'static str| {
        std::thread::spawn(move || {
            hub.record_diagnostic(DiagnosticRecord::new(
                DiagnosticLevel::Info,
                DiagnosticOrigin::Daemon { pid: 1 },
                "test",
                message,
            ));
        })
    };
    let first = emit(f.hub.clone(), "first");
    assert!(matches!(recv(&messages),ServerMessage::Diagnostic(event) if event.sequence == 1));
    let (started, ready) = bounded(1);
    let hub = f.hub.clone();
    let second = std::thread::spawn(move || {
        started.send(()).unwrap();
        hub.record_diagnostic(DiagnosticRecord::new(
            DiagnosticLevel::Info,
            DiagnosticOrigin::Daemon { pid: 2 },
            "test",
            "second",
        ));
    });
    ready.recv().unwrap();
    assert!(matches!(
        messages.recv_timeout(Duration::from_millis(100)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
    drop(state);
    first.join().unwrap();
    second.join().unwrap();
    assert!(matches!(recv(&messages),ServerMessage::Diagnostic(event) if event.sequence == 2));
    let state = f.hub.automation.state.lock();
    let source_sequences = state.sessions[&session.session_id]
        .events
        .iter()
        .map(|(event, _)| event.payload["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(source_sequences, vec![1, 2]);
}

#[test]
#[cfg(debug_assertions)]
fn a_late_attach_worker_cannot_leave_a_disconnected_client_interested() {
    let f = Fixture::new();
    let (client, _) = Fixture::subscriber(&f.hub);
    f.hub.unsubscribe(client);
    let session = f.attach(client);
    assert!(
        f.hub.automation.state.lock().sessions[&session.session_id]
            .subscriber
            .is_none()
    );
    assert!(matches!(
        recv(&f.incoming),
        ServerMessage::AutomationInterest { attached: true, .. }
    ));
    assert!(matches!(
        recv(&f.incoming),
        ServerMessage::AutomationInterest {
            attached: false,
            ..
        }
    ));
    let instance = instance();
    let instance_id = instance.instance_id;
    f.command(client, AutomationCommand::RegisterExecutor { instance });
    assert!(
        f.hub.automation.state.lock().executors[&instance_id]
            .subscriber
            .is_none()
    );
}

#[test]
#[cfg(debug_assertions)]
fn websocket_wait_survives_bootstrap_unavailability_until_a_real_frame_arrives() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let wait = match client
        .automation(AutomationCommand::Wait {
            session_id: session.session_id,
            condition: AutomationCondition::Node {
                id: "missing".into(),
                present: false,
            },
            timeout_ms: 5_000,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    };
    let initial = executor_request(&incoming);
    reply(
        &app,
        initial,
        AutomationExecutorOutcome::Error {
            error: AutomationError::new("app_not_ready", "workspace is still starting"),
        },
    );
    app.publish_automation_event(AutomationExecutorEvent { instance_id:instance.instance_id, stream:AutomationStream::Ui, observation:AutomationObservation { observed:json!({"tree":null,"state":null,"observationError":{"code":"no_frame","message":"first frame is not ready"}}), ..Default::default() } }).unwrap();
    app.automation_instances().unwrap(); // barrier after the unavailable push
    assert!(
        matches!(client.automation(AutomationCommand::WaitResult { session_id:session.session_id,wait_id:wait.wait_id }).unwrap(), AutomationResult::Wait { wait } if is_pending(&wait.status))
    );
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: observation(1, "send"),
    })
    .unwrap();
    loop {
        if let AutomationMessage::WaitResolved(resolved) =
            messages.recv_timeout(Duration::from_secs(3)).unwrap()
        {
            assert_eq!(resolved.wait_id, wait.wait_id);
            assert!(matches!(resolved.status, AutomationStatus::Succeeded));
            assert_eq!(resolved.result.unwrap()["tree"]["frame"], 1);
            break;
        }
    }
    assert!(
        incoming
            .try_iter()
            .all(|message| matches!(message, AutomationExecutorMessage::Interest { .. }))
    );
}

fn start_diagnostic_wait(
    client: &DaemonClient,
    session_id: Uuid,
    timeout_ms: u64,
) -> AutomationWait {
    match client
        .automation(AutomationCommand::Wait {
            session_id,
            condition: AutomationCondition::Diagnostic {
                query: DiagnosticQuery {
                    source: Some("expected.diagnostic".into()),
                    ..Default::default()
                },
            },
            timeout_ms,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    }
}

fn assert_standalone_diagnostic_timeout(changed: bool) {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    // This only receives wait notifications locally. Do not subscribe to any
    // runtime stream or make another call that could supply UI evidence.
    let messages = client.subscribe_automation(session.session_id);
    let wait = start_diagnostic_wait(&client, session.session_id, 1_500);
    assert!(is_pending(&wait.status));
    let mut observing_ui = false;
    let initial = loop {
        match incoming.recv_timeout(Duration::from_secs(3)).unwrap() {
            AutomationExecutorMessage::Interest { streams, .. } => {
                if !streams.is_empty() {
                    assert_eq!(streams, vec![AutomationStream::Ui]);
                    observing_ui = true;
                }
            }
            AutomationExecutorMessage::Request(request) => break request,
        }
    };
    assert!(
        observing_ui,
        "the wait must request its own UI observations"
    );
    assert!(matches!(
        &initial.call,
        AutomationCall::Snapshot { sections }
            if sections.iter().any(|section| section == "tree")
                && sections.iter().any(|section| section == "composer")
    ));
    reply(&app, initial, ok(observation(1, "before")));
    if changed {
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id: instance.instance_id,
            stream: AutomationStream::Ui,
            observation: observation(2, "after"),
        })
        .unwrap();
        app.record_diagnostic(DiagnosticRecord::new(
            DiagnosticLevel::Info,
            DiagnosticOrigin::App {
                pid: instance.pid,
                version: instance.version.clone(),
            },
            "other.diagnostic",
            "UI changed while the expected record was absent",
        ))
        .unwrap();
    }
    let resolved = match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::WaitResolved(wait) => wait,
        other => panic!("unexpected stream subscription: {other:?}"),
    };
    assert_eq!(resolved.wait_id, wait.wait_id);
    assert!(matches!(resolved.status, AutomationStatus::Failed));
    let error = resolved.error.unwrap();
    assert_eq!(error.code, "wait_timeout");
    let bundle = error.bundle.unwrap();
    let (frame, label) = if changed { (2, "after") } else { (1, "before") };
    assert_eq!(bundle.observed["tree"]["frame"], frame);
    assert_eq!(bundle.observed["state"]["composer"]["text"], label);
    assert_eq!(bundle.observed["health"]["ready"], true);
    assert_eq!(bundle.tree_diff["baselineAvailable"], true);
    assert_eq!(bundle.tree_diff["observedAvailable"], true);
    assert_eq!(bundle.tree_diff["beforeFrame"], 1);
    assert_eq!(bundle.tree_diff["afterFrame"], frame);
    assert!(bundle.tree_diff["added"].as_array().unwrap().is_empty());
    assert!(bundle.tree_diff["removed"].as_array().unwrap().is_empty());
    assert_eq!(
        bundle.tree_diff["changed"].as_array().unwrap().len(),
        usize::from(changed)
    );
    if changed {
        assert_eq!(bundle.tree_diff["changed"][0]["before"]["label"], "before");
        assert_eq!(bundle.tree_diff["changed"][0]["after"]["label"], "after");
        assert!(
            bundle
                .recent_events
                .as_ref()
                .unwrap()
                .events
                .iter()
                .any(|event| {
                    event.record.message == "UI changed while the expected record was absent"
                })
        );
    }
    assert!(bundle.recent_events.is_some());
    assert!(!bundle.counters.processes.is_empty());
    assert!(bundle.diagnostic_errors.is_empty());
    // Resolution removes the wait's interest. Receiving another request here
    // would mean observation was polled rather than driven by pushed events.
    assert!(matches!(
        incoming.recv_timeout(Duration::from_secs(3)).unwrap(),
        AutomationExecutorMessage::Interest { attached: true, streams, .. } if streams.is_empty()
    ));
    assert!(incoming.try_recv().is_err());
}

#[test]
#[cfg(debug_assertions)]
fn websocket_standalone_diagnostic_timeout_captures_initial_evidence() {
    assert_standalone_diagnostic_timeout(false);
}

#[test]
#[cfg(debug_assertions)]
fn websocket_standalone_diagnostic_timeout_keeps_changed_ui_evidence() {
    assert_standalone_diagnostic_timeout(true);
}

#[test]
#[cfg(debug_assertions)]
fn websocket_diagnostic_wait_succeeds_only_on_a_matching_record() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let wait = start_diagnostic_wait(&client, session.session_id, 5_000);
    let initial = executor_request(&incoming);
    reply(&app, initial, ok(observation(1, "before")));
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: observation(2, "after"),
    })
    .unwrap();
    let record = |source| {
        DiagnosticRecord::new(
            DiagnosticLevel::Info,
            DiagnosticOrigin::App {
                pid: instance.pid,
                version: instance.version.clone(),
            },
            source,
            "observed",
        )
    };
    app.record_diagnostic(record("other.diagnostic")).unwrap();
    app.automation_instances().unwrap(); // barrier after the unmatched events
    assert!(matches!(
        messages.recv_timeout(Duration::from_millis(100)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
    app.record_diagnostic(record("expected.diagnostic"))
        .unwrap();
    let resolved = match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::WaitResolved(wait) => wait,
        other => panic!("{other:?}"),
    };
    assert_eq!(resolved.wait_id, wait.wait_id);
    assert!(matches!(resolved.status, AutomationStatus::Succeeded));
    assert_eq!(resolved.result.unwrap()["source"], "expected.diagnostic");
    assert!(matches!(
        incoming.recv_timeout(Duration::from_secs(3)).unwrap(),
        AutomationExecutorMessage::Interest { attached: true, streams, .. } if streams.is_empty()
    ));
}

#[test]
#[cfg(debug_assertions)]
fn websocket_short_wait_timeouts_keep_late_bootstrap_evidence_without_succeeding() {
    for timeout_ms in [0, 30] {
        let server = SocketServer::new();
        let app = server.client();
        let incoming = app.subscribe_automation_executor();
        let instance = app.register_automation_executor(instance()).unwrap();
        let client = server.client();
        let session = client.attach_automation(instance.instance_id).unwrap();
        let messages = client.subscribe_automation(session.session_id);
        let wait = match client
            .automation(AutomationCommand::Wait {
                session_id: session.session_id,
                condition: AutomationCondition::Text {
                    id: "composer.send".into(),
                    contains: "matching but late".into(),
                },
                timeout_ms,
            })
            .unwrap()
        {
            AutomationResult::Wait { wait } => wait,
            other => panic!("{other:?}"),
        };
        let initial = executor_request(&incoming);
        // A healthy native draw can take longer than a short wait. Its first
        // snapshot is still useful evidence, never grounds for late success.
        assert!(matches!(
            messages.recv_timeout(Duration::from_millis(150)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
        // The in-flight capture must not hold the runtime lock or prevent a
        // second UI request from reaching the executor during finalization.
        let caller = client.clone();
        let other_call = std::thread::spawn(move || {
            caller.automation_call(session.session_id, AutomationCall::Describe)
        });
        let other = executor_request(&incoming);
        assert!(matches!(other.call, AutomationCall::Describe));
        reply(&app, initial, ok(observation(1, "matching but late")));
        reply(&app, other, ok(observation(2, "later")));
        other_call.join().unwrap().unwrap();
        let resolved = match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
            AutomationMessage::WaitResolved(wait) => wait,
            other => panic!("{other:?}"),
        };
        assert_eq!(resolved.wait_id, wait.wait_id);
        assert!(matches!(resolved.status, AutomationStatus::Failed));
        let error = resolved.error.unwrap();
        assert_eq!(error.code, "wait_timeout");
        let bundle = error.bundle.unwrap();
        assert_eq!(bundle.observed["tree"]["frame"], 1);
        assert_eq!(
            bundle.observed["state"]["composer"]["text"],
            "matching but late"
        );
        assert_eq!(bundle.tree_diff["baselineAvailable"], true);
        assert_eq!(bundle.tree_diff["observedAvailable"], true);
        assert!(bundle.diagnostic_errors.is_empty());
        assert!(matches!(
            incoming.recv_timeout(Duration::from_secs(3)).unwrap(),
            AutomationExecutorMessage::Interest { attached: true, streams, .. } if streams.is_empty()
        ));
        assert!(incoming.try_recv().is_err());
        assert!(messages.try_recv().is_err(), "a wait resolves only once");
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_wait_timeout_bounds_evidence_capture_when_executor_is_silent() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let wait = start_diagnostic_wait(&client, session.session_id, 0);
    let initial = executor_request(&incoming);
    // Never answer the only requested capture. Failure must be delivered
    // well before the ordinary 30-second executor-call timeout.
    let resolved = match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::WaitResolved(wait) => wait,
        other => panic!("{other:?}"),
    };
    assert_eq!(resolved.wait_id, wait.wait_id);
    assert!(matches!(resolved.status, AutomationStatus::Failed));
    let error = resolved.error.unwrap();
    assert_eq!(error.code, "wait_timeout");
    let bundle = error.bundle.unwrap();
    assert!(bundle.observed.is_null());
    assert!(
        bundle
            .diagnostic_errors
            .iter()
            .any(|error| error == "executor observation unavailable")
    );
    assert!(matches!(
        incoming.recv_timeout(Duration::from_secs(3)).unwrap(),
        AutomationExecutorMessage::Interest { attached: true, streams, .. } if streams.is_empty()
    ));
    // Even a very late reply cannot mutate or notify the terminal wait again.
    reply(&app, initial, ok(observation(1, "too late for evidence")));
    app.automation_instances().unwrap();
    assert!(messages.try_recv().is_err());
    let retained = match client
        .automation(AutomationCommand::WaitResult {
            session_id: session.session_id,
            wait_id: wait.wait_id,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    };
    assert!(retained.error.unwrap().bundle.unwrap().observed.is_null());
}

fn assert_wait_keeps_current_model_tree_and_correlated_events(condition: AutomationCondition) {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let old_session = Uuid::new_v4();
    let current_session = Uuid::new_v4();
    let selected_model = wait_model_section(&condition);
    let mut saw_interest = false;
    let wait = match client
        .automation(AutomationCommand::Wait {
            session_id: session.session_id,
            condition,
            timeout_ms: 1_000,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    };
    let initial = loop {
        match incoming.recv_timeout(Duration::from_secs(3)).unwrap() {
            AutomationExecutorMessage::Interest {
                streams,
                model_sections,
                ..
            } => {
                if !streams.is_empty() {
                    let expected_streams = if selected_model.is_some() {
                        vec![AutomationStream::Ui, AutomationStream::Model]
                    } else {
                        vec![AutomationStream::Ui]
                    };
                    assert_eq!(streams, expected_streams);
                    assert_eq!(
                        model_sections,
                        selected_model.iter().cloned().collect::<Vec<_>>()
                    );
                    saw_interest = true;
                }
            }
            AutomationExecutorMessage::Request(request) => break request,
        }
    };
    let mut initial_observation = observation(1, "before");
    initial_observation.observed["state"]["session"] = json!({"id":old_session,"status":"idle"});
    initial_observation.observed["health"]["sessions"] =
        json!({"selected":{"sessionId":old_session,"state":"idle"}});
    reply(&app, initial, ok(initial_observation));
    assert!(saw_interest);
    // Model notifications carry the current selected session and status but no
    // tree. A later paint carries a fresh tree but deliberately no session.
    // Neither stream may pin failure evidence to the bootstrap observation.
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Model,
        observation: AutomationObservation {
            observed: json!({"tree":null,"state":{
                "session":{"id":current_session,"status":"working"},
                "route":{"selectedSession":current_session}
            },"health":{"ready":true}}),
            ..Default::default()
        },
    })
    .unwrap();
    let mut painted = observation(2, "after");
    painted.observed["state"]["route"] = json!({"selectedSession":current_session});
    painted.observed["health"]["sessions"] =
        json!({"selected":{"sessionId":current_session,"state":"working"}});
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: painted,
    })
    .unwrap();
    for (session, message) in [
        (old_session, "old session"),
        (current_session, "current session"),
    ] {
        app.record_diagnostic(
            DiagnosticRecord::new(
                DiagnosticLevel::Info,
                DiagnosticOrigin::App {
                    pid: instance.pid,
                    version: instance.version.clone(),
                },
                "other.diagnostic",
                message,
            )
            .with_correlation(DiagnosticCorrelation {
                session_id: Some(session),
                ..Default::default()
            }),
        )
        .unwrap();
    }
    let resolved = match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::WaitResolved(wait) => wait,
        other => panic!("unexpected stream subscription: {other:?}"),
    };
    assert_eq!(resolved.wait_id, wait.wait_id);
    let error = resolved.error.unwrap();
    assert_eq!(error.code, "wait_timeout");
    let bundle = error.bundle.unwrap();
    if selected_model.as_deref() == Some("session") {
        assert_eq!(
            bundle.observed["state"]["session"]["id"],
            json!(current_session)
        );
        assert_eq!(bundle.observed["state"]["session"]["status"], "working");
    } else {
        assert!(bundle.observed["state"].get("session").is_none());
        assert!(bundle.observed["state"].get("model").is_none());
    }
    assert_eq!(
        bundle.observed["health"]["sessions"]["selected"]["sessionId"],
        json!(current_session)
    );
    assert_eq!(
        bundle.observed["health"]["sessions"]["selected"]["state"],
        "working"
    );
    assert_eq!(
        bundle.observed["state"]["route"]["selectedSession"],
        json!(current_session)
    );
    assert_eq!(bundle.observed["tree"]["frame"], 2);
    assert_eq!(bundle.tree_diff["beforeFrame"], 1);
    assert_eq!(bundle.tree_diff["afterFrame"], 2);
    assert_eq!(bundle.tree_diff["changed"][0]["after"]["label"], "after");
    let events = bundle.recent_events.unwrap().events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].record.message, "current session");
    // Both interests disappear when the only pending wait resolves.
    assert!(matches!(
        incoming.recv_timeout(Duration::from_secs(3)).unwrap(),
        AutomationExecutorMessage::Interest { attached: true, streams, .. } if streams.is_empty()
    ));
    assert!(incoming.try_recv().is_err());
}

#[test]
#[cfg(debug_assertions)]
fn websocket_all_wait_conditions_keep_current_model_tree_and_correlated_events() {
    let conditions = [
        AutomationCondition::Node {
            id: "missing".into(),
            present: true,
        },
        AutomationCondition::Text {
            id: "composer.send".into(),
            contains: "never".into(),
        },
        AutomationCondition::Frame { after: 99 },
        AutomationCondition::Diagnostic {
            query: DiagnosticQuery {
                source: Some("expected.diagnostic".into()),
                ..Default::default()
            },
        },
        AutomationCondition::State {
            pointer: "/session/status".into(),
            equals: json!("never"),
        },
        AutomationCondition::StateContains {
            pointer: "/session/status".into(),
            contains: "never".into(),
        },
    ];
    for condition in conditions {
        assert_wait_keeps_current_model_tree_and_correlated_events(condition);
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_control_waits_ignore_unrelated_oversized_model_observations() {
    for condition in [
        AutomationCondition::Node {
            id: "composer.send".into(),
            present: true,
        },
        AutomationCondition::Text {
            id: "composer.send".into(),
            contains: "after".into(),
        },
        AutomationCondition::Frame { after: 1 },
        AutomationCondition::Diagnostic {
            query: DiagnosticQuery {
                source: Some("expected".into()),
                ..Default::default()
            },
        },
    ] {
        let server = SocketServer::new();
        let app = server.client();
        let incoming = app.subscribe_automation_executor();
        let instance = app.register_automation_executor(instance()).unwrap();
        let client = server.client();
        let session = client.attach_automation(instance.instance_id).unwrap();
        let messages = client.subscribe_automation(session.session_id);
        let wait = match client
            .automation(AutomationCommand::Wait {
                session_id: session.session_id,
                condition,
                timeout_ms: 5_000,
            })
            .unwrap()
        {
            AutomationResult::Wait { wait } => wait,
            other => panic!("{other:?}"),
        };
        let initial = loop {
            match incoming.recv_timeout(Duration::from_secs(3)).unwrap() {
                AutomationExecutorMessage::Interest {
                    streams,
                    model_sections,
                    ..
                } => {
                    assert!(!streams.contains(&AutomationStream::Model));
                    assert!(model_sections.is_empty());
                }
                AutomationExecutorMessage::Request(request) => break request,
            }
        };
        assert!(
            matches!(&initial.call, AutomationCall::Snapshot { sections }
            if !sections.iter().any(|section| matches!(section.as_str(), "state" | "model" | "session")))
        );
        let mut baseline = observation(1, "before");
        baseline.observed["tree"]["nodes"] = json!([]);
        reply(&app, initial, ok(baseline));
        // Another client can request the entire model and exceed the capture
        // bound. Its failure must not break this lightweight control wait.
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id: instance.instance_id, stream: AutomationStream::Model,
            observation: AutomationObservation {
                observed: json!({"observationError":{"code":"observation_too_large","message":"unrelated stored history exceeds 4 MiB"}}),
                ..Default::default()
            },
        }).unwrap();
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id: instance.instance_id,
            stream: AutomationStream::Ui,
            observation: observation(2, "after"),
        })
        .unwrap();
        app.record_diagnostic(DiagnosticRecord::new(
            DiagnosticLevel::Info,
            DiagnosticOrigin::App {
                pid: instance.pid,
                version: instance.version.clone(),
            },
            "expected",
            "ready",
        ))
        .unwrap();
        let resolved = match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
            AutomationMessage::WaitResolved(wait) => wait,
            other => panic!("{other:?}"),
        };
        assert_eq!(resolved.wait_id, wait.wait_id);
        assert!(matches!(resolved.status, AutomationStatus::Succeeded));
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_selected_state_waits_isolate_other_clients_oversized_sections() {
    for oversized_selected in [false, true] {
        let server = SocketServer::new();
        let app = server.client();
        let incoming = app.subscribe_automation_executor();
        let instance = app.register_automation_executor(instance()).unwrap();
        let model_client = server.client();
        let model_session = model_client
            .attach_automation(instance.instance_id)
            .unwrap();
        let model_messages = model_client.subscribe_automation(model_session.session_id);
        model_client
            .automation(AutomationCommand::Subscribe {
                session_id: model_session.session_id,
                streams: vec![AutomationStream::Model],
            })
            .unwrap();
        let composer_client = server.client();
        let session_client = server.client();
        let composer_session = composer_client
            .attach_automation(instance.instance_id)
            .unwrap();
        let selected_session = session_client
            .attach_automation(instance.instance_id)
            .unwrap();
        let composer_messages = composer_client.subscribe_automation(composer_session.session_id);
        let selected_messages = session_client.subscribe_automation(selected_session.session_id);
        let mut waits = Vec::new();
        let mut selected_sections = Vec::new();
        for (client, session_id, condition) in [
            (
                &composer_client,
                composer_session.session_id,
                if oversized_selected {
                    AutomationCondition::StateContains {
                        pointer: "/composer/text".into(),
                        contains: "after".into(),
                    }
                } else {
                    AutomationCondition::State {
                        pointer: "/composer/text".into(),
                        equals: json!("after"),
                    }
                },
            ),
            (
                &session_client,
                selected_session.session_id,
                if oversized_selected {
                    AutomationCondition::State {
                        pointer: "/session/status".into(),
                        equals: json!("working"),
                    }
                } else {
                    AutomationCondition::StateContains {
                        pointer: "/session/status".into(),
                        contains: "working".into(),
                    }
                },
            ),
        ] {
            let wait = match client
                .automation(AutomationCommand::Wait {
                    session_id,
                    condition,
                    timeout_ms: 5_000,
                })
                .unwrap()
            {
                AutomationResult::Wait { wait } => wait,
                other => panic!("{other:?}"),
            };
            let initial = loop {
                match incoming.recv_timeout(Duration::from_secs(3)).unwrap() {
                    AutomationExecutorMessage::Interest { model_sections, .. } => {
                        selected_sections = model_sections
                    }
                    AutomationExecutorMessage::Request(request) => break request,
                }
            };
            let mut baseline = observation(1, "before");
            baseline.observed["state"]["session"] = json!({"status":"idle","detail":"old"});
            reply(&app, initial, ok(baseline));
            waits.push(wait);
        }
        assert_eq!(selected_sections, vec!["composer", "session", "state"]);
        // The executor reports its full-state bound failure independently of
        // the two selected captures (the real >4 MiB capture is app-unit tested).
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id:instance.instance_id, stream:AutomationStream::Model,
            observation:AutomationObservation {
                observed:json!({"observationSection":"state","observationError":{
                    "code":"observation_too_large","message":"unrelated stored history exceeds 4 MiB"
                }}), ..Default::default()
            },
        }).unwrap();
        app.automation_instances().unwrap(); // Commit the event before querying either wait.
        for (client, session_id, wait) in [
            (&composer_client, composer_session.session_id, &waits[0]),
            (&session_client, selected_session.session_id, &waits[1]),
        ] {
            assert!(matches!(client.automation(AutomationCommand::WaitResult {
                session_id, wait_id:wait.wait_id,
            }).unwrap(), AutomationResult::Wait { wait } if is_pending(&wait.status)));
        }
        match model_messages.recv_timeout(Duration::from_secs(3)).unwrap() {
            AutomationMessage::Event(event) => {
                assert_eq!(
                    event.payload["observationError"]["code"],
                    "observation_too_large"
                );
                assert_eq!(event.payload["observationSection"], "state");
            }
            other => panic!("{other:?}"),
        }
        let selected = if oversized_selected {
            json!({"observationSection":"session","observationError":{
                "code":"observation_too_large","message":"requested session exceeds 4 MiB"
            }})
        } else {
            // Plain partial model observations remain mergeable too.
            json!({"state":{"session":{"status":"working","detail":"current"}}})
        };
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id: instance.instance_id,
            stream: AutomationStream::Model,
            observation: AutomationObservation {
                observed: selected,
                ..Default::default()
            },
        })
        .unwrap();
        app.publish_automation_event(AutomationExecutorEvent {
            instance_id:instance.instance_id, stream:AutomationStream::Model,
            observation:AutomationObservation {
                observed:json!({"observationSection":"composer","state":{"composer":{"text":"after"}}}),
                ..Default::default()
            },
        }).unwrap();
        let selected = match selected_messages
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
        {
            AutomationMessage::WaitResolved(wait) => wait,
            other => panic!("{other:?}"),
        };
        assert_eq!(selected.wait_id, waits[1].wait_id);
        if oversized_selected {
            assert!(matches!(selected.status, AutomationStatus::Failed));
            assert_eq!(selected.error.unwrap().code, "observation_too_large");
        } else {
            assert!(matches!(selected.status, AutomationStatus::Succeeded));
            let result = selected.result.unwrap();
            assert_eq!(result["state"]["session"]["detail"], "current");
            assert_eq!(result["tree"]["frame"], 1);
        }
        match composer_messages
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
        {
            AutomationMessage::WaitResolved(wait) => {
                assert_eq!(wait.wait_id, waits[0].wait_id);
                assert!(matches!(wait.status, AutomationStatus::Succeeded));
                assert_eq!(wait.result.unwrap()["state"]["composer"]["text"], "after");
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_ui_state_wait_resolves_on_repaint_without_model_notification() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let wait = match client
        .automation(AutomationCommand::Wait {
            session_id: session.session_id,
            condition: AutomationCondition::State {
                pointer: "/ui/sidebarViewport".into(),
                equals: json!(720),
            },
            timeout_ms: 5_000,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    };
    let initial = executor_request(&incoming);
    assert!(
        matches!(&initial.call, AutomationCall::Snapshot { sections } if sections.contains(&"ui".into()))
    );
    let mut baseline = observation(1, "before");
    baseline.observed["state"]["ui"] = json!({"sidebarViewport":320});
    reply(&app, initial, ok(baseline));
    let mut resized = observation(2, "before");
    resized.observed["state"]["ui"] = json!({"sidebarViewport":720});
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: resized,
    })
    .unwrap();
    match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::WaitResolved(resolved) => {
            assert_eq!(resolved.wait_id, wait.wait_id);
            assert!(matches!(resolved.status, AutomationStatus::Succeeded));
            let result = resolved.result.unwrap();
            assert_eq!(result["state"]["ui"]["sidebarViewport"], 720);
            assert_eq!(result["tree"]["frame"], 2);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
#[cfg(debug_assertions)]
fn websocket_large_model_delta_preserves_unrelated_wait_and_names_subscriber_replay_gap() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let model_client = server.client();
    let model_session = model_client
        .attach_automation(instance.instance_id)
        .unwrap();
    model_client
        .automation(AutomationCommand::Subscribe {
            session_id: model_session.session_id,
            streams: vec![AutomationStream::Model],
        })
        .unwrap();
    let client = server.client();
    let session = client.attach_automation(instance.instance_id).unwrap();
    let messages = client.subscribe_automation(session.session_id);
    let wait = match client
        .automation(AutomationCommand::Wait {
            session_id: session.session_id,
            condition: AutomationCondition::State {
                pointer: "/composer/text".into(),
                equals: json!("after"),
            },
            timeout_ms: 5_000,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => wait,
        other => panic!("{other:?}"),
    };
    let initial = executor_request(&incoming);
    reply(&app, initial, ok(observation(1, "before")));
    let text = "x".repeat(300 * 1024);
    let large = AutomationObservation {
        observed: json!({"state":{"model":{"text":text}}}),
        state_diff: json!([{"pointer":"/model/text","value":text}]),
        ..Default::default()
    };
    assert!(serde_json::to_vec(&large).unwrap().len() < MAX_AUTOMATION_OBSERVATION_BYTES);
    assert!(serde_json::to_vec(&delta(&large)).unwrap().len() > MAX_EVENT_BYTES);
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Model,
        observation: large,
    })
    .unwrap();
    app.automation_instances().unwrap();
    assert!(!client.is_disconnected());
    assert!(client.automation_instances().unwrap()[0].executor_connected);
    assert!(matches!(client.automation(AutomationCommand::WaitResult {
        session_id:session.session_id, wait_id:wait.wait_id,
    }).unwrap(), AutomationResult::Wait { wait } if is_pending(&wait.status)));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !model_client.is_disconnected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(model_client.is_disconnected());
    let resumed_client = server.client();
    match resumed_client
        .automation(AutomationCommand::Resume {
            session_id: model_session.session_id,
            after: Some(model_session.cursor),
        })
        .unwrap()
    {
        AutomationResult::Attached { replay, .. } => {
            let expiry = replay.expiry.unwrap();
            assert_eq!(expiry.requested, model_session.cursor);
            assert_eq!(expiry.cursor.sequence, model_session.cursor.sequence + 1);
            assert_eq!(expiry.oldest_sequence, expiry.cursor.sequence + 1);
            assert!(replay.events.is_empty());
        }
        other => panic!("{other:?}"),
    }
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: observation(2, "after"),
    })
    .unwrap();
    match messages.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::WaitResolved(resolved) => {
            assert_eq!(resolved.wait_id, wait.wait_id);
            assert!(matches!(resolved.status, AutomationStatus::Succeeded));
        }
        other => panic!("{other:?}"),
    }
    // The skipped event still creates an explicit replay gap even for the
    // unaffected client, which was never subscribed to live Model events.
    match client
        .automation(AutomationCommand::Diff {
            session_id: session.session_id,
            after: session.cursor,
            streams: vec![],
        })
        .unwrap()
    {
        AutomationResult::Diff { diff } => {
            assert!(diff.expiry.is_some());
            assert_eq!(diff.events.len(), 1);
            assert_eq!(diff.events[0].stream, AutomationStream::Ui);
        }
        other => panic!("{other:?}"),
    }
}

#[path = "automation_recording_runtime_tests.rs"]
mod recording_tests;
