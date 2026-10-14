//! End-to-end recording tests through the authenticated daemon websocket.
//! The executor is an explicit provider-free fixture, not a model invocation.
#![cfg(debug_assertions)]
use super::*;
use std::collections::BTreeMap;

fn start_recording(client: &DaemonClient, session_id: Uuid) {
    match client
        .automation(AutomationCommand::RecordingStart {
            session_id,
            name: "portable fixture".into(),
        })
        .unwrap()
    {
        AutomationResult::Recording { recording } => {
            assert_eq!(recording.status, AutomationRecordingStatus::Recording)
        }
        result => panic!("{result:?}"),
    }
}

fn recording_status(client: &DaemonClient, session_id: Uuid) -> AutomationRecording {
    match client
        .automation(AutomationCommand::RecordingStatus { session_id })
        .unwrap()
    {
        AutomationResult::Recording { recording } => recording,
        result => panic!("{result:?}"),
    }
}

fn stop_recording(client: &DaemonClient, session_id: Uuid) -> AutomationReplay {
    match client
        .automation(AutomationCommand::RecordingStop { session_id })
        .unwrap()
    {
        AutomationResult::Replay { replay } => replay,
        result => panic!("{result:?}"),
    }
}

fn assert_error(client: &DaemonClient, command: AutomationCommand, code: &str) {
    let failure = client.automation(command).unwrap_err();
    let failure = failure
        .downcast_ref::<waku_client::DaemonAutomationError>()
        .unwrap();
    assert_eq!(failure.error.code, code, "{failure}");
}

fn call_and_reply(
    client: &DaemonClient,
    app: &DaemonClient,
    incoming: &Receiver<AutomationExecutorMessage>,
    session_id: Uuid,
    call: AutomationCall,
    value: Value,
) {
    let expected = serde_json::to_value(&call).unwrap();
    let caller = client.clone();
    let worker = std::thread::spawn(move || caller.automation_call(session_id, call));
    let request = executor_request(incoming);
    assert_eq!(serde_json::to_value(&request.call).unwrap(), expected);
    reply(
        app,
        request,
        AutomationExecutorOutcome::Ok {
            execution: AutomationExecution {
                value,
                observation: observation(1, "fixture"),
            },
        },
    );
    worker.join().unwrap().unwrap();
}

fn no_executor_requests(incoming: &Receiver<AutomationExecutorMessage>) {
    assert!(
        incoming
            .try_iter()
            .all(|message| matches!(message, AutomationExecutorMessage::Interest { .. }))
    );
}

#[test]
fn websocket_recording_exports_redacted_calls_and_replays_with_fresh_ids() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session_id = client
        .attach_automation(instance.instance_id)
        .unwrap()
        .session_id;
    let notifications = client.subscribe_automation(session_id);
    let old_id = Uuid::new_v4();
    start_recording(&client, session_id);
    call_and_reply(
        &client,
        &app,
        &incoming,
        session_id,
        AutomationCall::Invoke {
            verb: "newSession".into(),
            args: json!({}),
        },
        json!({"opened":old_id}),
    );
    call_and_reply(
        &client,
        &app,
        &incoming,
        session_id,
        AutomationCall::Type {
            text: "private original draft".into(),
            target: Some(AutomationTarget::Id("composer-field".into())),
        },
        json!({"typed":22}),
    );
    call_and_reply(
        &client,
        &app,
        &incoming,
        session_id,
        AutomationCall::Invoke {
            verb: "openSession".into(),
            args: json!({"id":old_id}),
        },
        json!({"opened":old_id}),
    );
    let replay = stop_recording(&client, session_id);
    assert_eq!(replay.steps.len(), 3);
    let serialized = serde_json::to_string(&replay).unwrap();
    assert!(!serialized.contains("private original draft"));
    assert!(!serialized.contains(&old_id.to_string()));
    let status = recording_status(&client, session_id);
    assert_eq!(status.status, AutomationRecordingStatus::Stopped);
    assert_eq!(status.required_inputs.len(), 1);
    assert_error(
        &client,
        AutomationCommand::ReplayStart {
            session_id,
            replay: replay.clone(),
            parameters: BTreeMap::new(),
        },
        "missing_parameter",
    );
    no_executor_requests(&incoming);
    let parameters = BTreeMap::from([("text1".into(), json!("fresh replay draft"))]);
    let run_id = match client
        .automation(AutomationCommand::ReplayStart {
            session_id,
            replay,
            parameters,
        })
        .unwrap()
    {
        AutomationResult::Scenario { run } => run.run_id,
        result => panic!("{result:?}"),
    };
    let fresh_id = Uuid::new_v4();
    let first = executor_request(&incoming);
    assert!(
        matches!(&first.call, AutomationCall::Invoke { verb, args } if verb == "newSession" && args == &json!({}))
    );
    reply(
        &app,
        first,
        AutomationExecutorOutcome::Ok {
            execution: AutomationExecution {
                value: json!({"opened":fresh_id}),
                observation: observation(2, "fixture"),
            },
        },
    );
    let second = executor_request(&incoming);
    assert!(
        matches!(&second.call, AutomationCall::Type { text, .. } if text == "fresh replay draft")
    );
    reply(&app, second, ok(observation(3, "fixture")));
    let third = executor_request(&incoming);
    assert!(
        matches!(&third.call, AutomationCall::Invoke { verb, args } if verb == "openSession" && args["id"] == fresh_id.to_string())
    );
    reply(&app, third, ok(observation(4, "fixture")));
    let run = final_run(&notifications);
    assert_eq!(run.run_id, run_id);
    assert!(matches!(run.status, AutomationStatus::Succeeded));
    assert_eq!(run.steps.len(), 3);
    assert_eq!(
        run.steps[0].result.as_ref().unwrap()["value"]["opened"],
        fresh_id.to_string()
    );
    no_executor_requests(&incoming);
}

#[test]
fn websocket_recorded_pending_wait_blocks_next_step_and_stop_until_resolution() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session_id = client
        .attach_automation(instance.instance_id)
        .unwrap()
        .session_id;
    let notifications = client.subscribe_automation(session_id);
    start_recording(&client, session_id);
    let wait_id = match client
        .automation(AutomationCommand::Wait {
            session_id,
            condition: AutomationCondition::Text {
                id: "composer.send".into(),
                contains: "done".into(),
            },
            timeout_ms: 10_000,
        })
        .unwrap()
    {
        AutomationResult::Wait { wait } => {
            assert!(is_pending(&wait.status));
            wait.wait_id
        }
        result => panic!("{result:?}"),
    };
    let initial = executor_request(&incoming);
    assert!(matches!(initial.call, AutomationCall::Snapshot { .. }));
    assert!(recording_status(&client, session_id).pending);
    assert_error(
        &client,
        AutomationCommand::Call {
            session_id,
            call: AutomationCall::Describe,
        },
        "recording_busy",
    );
    assert_error(
        &client,
        AutomationCommand::RecordingStop { session_id },
        "recording_busy",
    );
    no_executor_requests(&incoming);
    reply(&app, initial, ok(observation(1, "waiting")));
    app.publish_automation_event(AutomationExecutorEvent {
        instance_id: instance.instance_id,
        stream: AutomationStream::Ui,
        observation: observation(2, "done"),
    })
    .unwrap();
    loop {
        if let AutomationMessage::WaitResolved(wait) =
            notifications.recv_timeout(Duration::from_secs(3)).unwrap()
        {
            assert_eq!(wait.wait_id, wait_id);
            assert!(matches!(wait.status, AutomationStatus::Succeeded));
            break;
        }
    }
    let status = recording_status(&client, session_id);
    assert!(!status.pending);
    assert_eq!(status.completed_steps, 1);
    let replay = stop_recording(&client, session_id);
    assert_eq!(
        replay.steps.len(),
        1,
        "bootstrap snapshots must not be recorded"
    );
    assert!(matches!(
        replay.steps[0].step.operation,
        AutomationStepOperation::Wait { .. }
    ));
    assert_eq!(replay.steps[0].bindings[0].pointer, "/condition/contains");
    assert!(!serde_json::to_string(&replay).unwrap().contains("done"));
}

#[test]
fn websocket_unrecordable_calls_are_rejected_before_effect_and_cancel_releases_session() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session_id = client
        .attach_automation(instance.instance_id)
        .unwrap()
        .session_id;
    start_recording(&client, session_id);
    assert_error(
        &client,
        AutomationCommand::Call {
            session_id,
            call: AutomationCall::Invoke {
                verb: "submitPrompt".into(),
                args: json!({"text":"never send this private draft"}),
            },
        },
        "unrecordable",
    );
    no_executor_requests(&incoming);
    let status = recording_status(&client, session_id);
    assert_eq!(status.status, AutomationRecordingStatus::Failed);
    assert_eq!(status.completed_steps, 0);
    assert!(
        !serde_json::to_string(&status)
            .unwrap()
            .contains("private draft")
    );
    assert_error(
        &client,
        AutomationCommand::Call {
            session_id,
            call: AutomationCall::Describe,
        },
        "recording_closed",
    );
    no_executor_requests(&incoming);
    match client
        .automation(AutomationCommand::RecordingCancel { session_id })
        .unwrap()
    {
        AutomationResult::Recording { recording } => {
            assert_eq!(recording.status, AutomationRecordingStatus::Cancelled)
        }
        result => panic!("{result:?}"),
    }
    call_and_reply(
        &client,
        &app,
        &incoming,
        session_id,
        AutomationCall::Describe,
        json!({"ready":true}),
    );
    assert_eq!(recording_status(&client, session_id).completed_steps, 0);
}

#[test]
fn websocket_recorded_failure_retains_sanitized_evidence_and_stop_releases_session() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session_id = client
        .attach_automation(instance.instance_id)
        .unwrap()
        .session_id;
    start_recording(&client, session_id);
    let caller = client.clone();
    let worker = std::thread::spawn(move || {
        caller.automation_call(
            session_id,
            AutomationCall::Type {
                text: "private token".into(),
                target: Some(AutomationTarget::Id("composer-field".into())),
            },
        )
    });
    let request = executor_request(&incoming);
    reply(
        &app,
        request,
        AutomationExecutorOutcome::Error {
            error: AutomationError::new("invalid_type", "private token cannot be typed"),
        },
    );
    let original = worker.join().unwrap().unwrap_err();
    assert!(original.to_string().contains("private token"));
    let status = recording_status(&client, session_id);
    assert_eq!(status.status, AutomationRecordingStatus::Failed);
    assert_eq!(status.error.as_ref().unwrap().code, "invalid_type");
    assert!(
        !serde_json::to_string(&status)
            .unwrap()
            .contains("private token")
    );
    assert!(stop_recording(&client, session_id).steps.is_empty());
    call_and_reply(
        &client,
        &app,
        &incoming,
        session_id,
        AutomationCall::Describe,
        json!({"ready":true}),
    );
    assert_eq!(
        recording_status(&client, session_id).status,
        AutomationRecordingStatus::Failed
    );
}

#[test]
fn websocket_recording_start_rejects_an_already_dispatched_call() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session_id = client
        .attach_automation(instance.instance_id)
        .unwrap()
        .session_id;
    let caller = client.clone();
    let worker =
        std::thread::spawn(move || caller.automation_call(session_id, AutomationCall::Describe));
    let request = executor_request(&incoming);
    assert_error(
        &client,
        AutomationCommand::RecordingStart {
            session_id,
            name: "would miss call".into(),
        },
        "recording_busy",
    );
    reply(&app, request, ok(observation(1, "fixture")));
    worker.join().unwrap().unwrap();
    start_recording(&client, session_id);
    assert!(stop_recording(&client, session_id).steps.is_empty());
}

#[test]
fn websocket_cancelled_recording_ignores_an_inflight_completion() {
    let server = SocketServer::new();
    let app = server.client();
    let incoming = app.subscribe_automation_executor();
    let instance = app.register_automation_executor(instance()).unwrap();
    let client = server.client();
    let session_id = client
        .attach_automation(instance.instance_id)
        .unwrap()
        .session_id;
    start_recording(&client, session_id);
    let caller = client.clone();
    let worker = std::thread::spawn(move || {
        caller.automation_call(
            session_id,
            AutomationCall::Type {
                text: "private cancelled text".into(),
                target: None,
            },
        )
    });
    let request = executor_request(&incoming);
    client
        .automation(AutomationCommand::RecordingCancel { session_id })
        .unwrap();
    assert_eq!(
        recording_status(&client, session_id).status,
        AutomationRecordingStatus::Cancelled
    );
    reply(&app, request, ok(observation(1, "fixture")));
    worker.join().unwrap().unwrap();
    let status = recording_status(&client, session_id);
    assert_eq!(status.status, AutomationRecordingStatus::Cancelled);
    assert_eq!(status.completed_steps, 0);
    assert!(status.required_inputs.is_empty());
}

#[test]
fn recording_start_cannot_enter_the_gap_before_call_dispatch() {
    let fixture = Fixture::new();
    let (subscriber, _) = Fixture::subscriber(&fixture.hub);
    let session = fixture.attach(subscriber);
    let ticket = fixture
        .hub
        .automation
        .record_begin(
            session.session_id,
            &AutomationStepOperation::Call {
                call: AutomationCall::Describe,
            },
        )
        .unwrap();
    assert!(ticket.is_none());
    assert!(
        matches!(fixture.command(subscriber, AutomationCommand::RecordingStart {
        session_id: session.session_id, name: "must wait".into(),
    }), AutomationResult::Error { error } if error.code == "recording_busy")
    );
    fixture
        .hub
        .automation
        .record_complete(session.session_id, ticket, Ok(Value::Null));
    assert!(matches!(
        fixture.command(
            subscriber,
            AutomationCommand::RecordingStart {
                session_id: session.session_id,
                name: "now safe".into(),
            }
        ),
        AutomationResult::Recording { .. }
    ));
}

#[test]
fn headless_waits_reject_ui_conditions_before_executor_dispatch() {
    let fixture = Fixture::new();
    fixture
        .hub
        .automation
        .state
        .lock()
        .executors
        .get_mut(&fixture.instance.instance_id)
        .unwrap()
        .instance
        .headless = true;
    let (subscriber, _) = Fixture::subscriber(&fixture.hub);
    let session = fixture.attach(subscriber);
    for condition in [
        AutomationCondition::Node {
            id: "composer-field".into(),
            present: true,
        },
        AutomationCondition::Frame { after: 0 },
    ] {
        assert!(
            matches!(fixture.command(subscriber, AutomationCommand::Wait {
            session_id: session.session_id, condition, timeout_ms: 100,
        }), AutomationResult::Error { error } if error.code == "unsupported_headless")
        );
    }
    assert!(
        fixture
            .incoming
            .try_iter()
            .all(|message| matches!(message, ServerMessage::AutomationInterest { .. }))
    );
}
