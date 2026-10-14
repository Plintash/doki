use super::*;
use serde_json::{Value, json};
use std::net::TcpListener;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const INSTANCE: &str = "00000000-0000-4000-8000-000000000002";
const EPOCH: &str = "00000000-0000-4000-8000-000000000003";

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}
fn cursor(sequence: u64) -> Value {
    json!({"epoch": EPOCH, "sequence": sequence})
}
fn instance() -> Value {
    json!({"instanceId":INSTANCE,"checkout":"/test/checkout","database":"/test/checkout/temp/app.db",
        "pid":42,"build":"test-build","version":"test","executorConnected":true,"scratch":false})
}
fn event(sequence: u64) -> Value {
    json!({"sessionId":SESSION,"cursor":cursor(sequence),"stream":"ui","payload":{"sequence":sequence}})
}
fn attached(sequence: u64, events: Vec<Value>) -> Value {
    json!({"type":"attached","session":{"sessionId":SESSION,"instance":instance(),"cursor":cursor(sequence),"streams":["ui"]},
        "replay":{"events":events,"cursor":cursor(sequence),"expiry":null}})
}
fn send(socket: &mut WebSocket<TcpStream>, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .unwrap();
}
fn read(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = socket.read().unwrap() {
            return serde_json::from_str(&text).unwrap();
        }
    }
}
fn accept(listener: &TcpListener) -> WebSocket<TcpStream> {
    let (stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut socket = tungstenite::accept(stream).unwrap();
    let hello = read(&mut socket);
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["token"], "test-token");
    send(
        &mut socket,
        json!({"type":"hello","protocolVersion":PROTOCOL_VERSION,"daemonVersion":"test"}),
    );
    socket
}
fn respond(socket: &mut WebSocket<TcpStream>, request: &Value, result: Value) {
    send(
        socket,
        json!({"type":"response","requestId":request["requestId"],
        "outcome":{"status":"ok","payload":{"type":"automation","result":result}}}),
    );
}
fn received_sequence(receiver: &Receiver<AutomationMessage>) -> u64 {
    match receiver.recv_timeout(Duration::from_secs(3)).unwrap() {
        AutomationMessage::Event(event) => event.cursor.sequence,
        other => panic!("unexpected message {other:?}"),
    }
}

#[test]
fn automation_call_subscription_and_resume_share_the_daemon_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let attach = read(&mut socket);
        assert_eq!(attach["command"]["command"]["type"], "attach");
        respond(&mut socket, &attach, attached(0, vec![]));
        let subscribe = read(&mut socket);
        assert_eq!(subscribe["command"]["command"]["streams"], json!(["ui"]));
        respond(
            &mut socket,
            &subscribe,
            json!({"type":"subscribed","session":attached(0,vec![])["session"]}),
        );
        let call = read(&mut socket);
        assert_eq!(call["command"]["command"]["call"]["type"], "snapshot");
        respond(
            &mut socket,
            &call,
            json!({"type":"called","result":{"cursor":cursor(1),"value":{"ready":true},
            "observed":{"state":{}},"treeDiff":{},"stateDiff":[]}}),
        );
        for sequence in [1, 2, 2] {
            send(
                &mut socket,
                json!({"type":"automationEvent","event":event(sequence)}),
            );
        }
        socket.close(None).unwrap();
        let mut socket = accept(&listener);
        let resume = read(&mut socket);
        assert_eq!(resume["command"]["command"]["after"], cursor(2));
        // A live event races the response. It must not swallow the older replay.
        send(
            &mut socket,
            json!({"type":"automationEvent","event":event(4)}),
        );
        respond(&mut socket, &resume, attached(4, vec![event(3), event(4)]));
        send(
            &mut socket,
            json!({"type":"automationEvent","event":event(5)}),
        );
    });
    let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
    let session = client.attach_automation(uuid(INSTANCE)).unwrap();
    let events = client.subscribe_automation(session.session_id);
    client
        .automation(AutomationCommand::Subscribe {
            session_id: session.session_id,
            streams: vec![AutomationStream::Ui],
        })
        .unwrap();
    let result = client
        .automation_call(
            session.session_id,
            AutomationCall::Snapshot { sections: vec![] },
        )
        .unwrap();
    assert_eq!(result.execution.value, json!({"ready":true}));
    assert_eq!(received_sequence(&events), 1);
    assert_eq!(received_sequence(&events), 2);
    let after = client.last_automation_cursor(session.session_id);
    assert_eq!(after.unwrap().sequence, 2);
    let reconnected = DaemonClient::connect(&address, "test-token".into()).unwrap();
    let events = reconnected.subscribe_automation(session.session_id);
    reconnected
        .resume_automation(session.session_id, after)
        .unwrap();
    assert_eq!(received_sequence(&events), 3);
    assert_eq!(received_sequence(&events), 4);
    assert_eq!(received_sequence(&events), 5);
    assert!(events.try_recv().is_err());
    server.join().unwrap();
}

#[test]
fn automation_same_client_resume_recovers_local_overflow() {
    // Cover both kinds of local loss: no subscriber yet, and a subscriber
    // whose bounded receiver closes after its explicit overflow notice.
    for stalled_subscriber in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let newest = MAX_AUTOMATION_EVENTS as u64 + 10;
        let after = if stalled_subscriber {
            MAX_BUFFERED_AUTOMATION_MESSAGES as u64
        } else {
            0
        };
        let other_session = Uuid::new_v4();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let attach = read(&mut socket);
            for sequence in 1..=newest {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(sequence)}),
                );
            }
            let mut other_event = event(1000);
            other_event["sessionId"] = json!(other_session);
            send(
                &mut socket,
                json!({"type":"automationEvent","event":other_event}),
            );
            // The answer is a wire-order barrier after the overflowing burst.
            respond(&mut socket, &attach, attached(0, vec![]));

            let resume = read(&mut socket);
            assert_eq!(resume["command"]["command"]["type"], "resume");
            assert_eq!(resume["command"]["command"]["after"], cursor(after));
            for _ in 0..2 {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(newest)}),
                );
            }
            for sequence in [1000, 1001, 1001] {
                let mut other_event = event(sequence);
                other_event["sessionId"] = json!(other_session);
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":other_event}),
                );
            }
            let oldest = 11.max(after + 1);
            let mut replay = attached(newest, (oldest..=newest).map(event).collect());
            if after == 0 {
                replay["replay"]["expiry"] = json!({
                    "requested":cursor(after),"oldestSequence":oldest,"cursor":cursor(newest)
                });
            }
            respond(&mut socket, &resume, replay);

            // Let the consumer drain the full retained tail before more live
            // events arrive, so this does not deliberately overflow it again.
            let instances = read(&mut socket);
            assert_eq!(instances["command"]["command"]["type"], "instances");
            for sequence in [newest, newest + 1, newest + 1] {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(sequence)}),
                );
            }
            respond(
                &mut socket,
                &instances,
                json!({"type":"instances","instances":[]}),
            );
        });

        let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
        let session_id = uuid(SESSION);
        let stalled = stalled_subscriber.then(|| client.subscribe_automation(session_id));
        client.attach_automation(uuid(INSTANCE)).unwrap();
        if let Some(stalled) = stalled {
            for sequence in 1..=MAX_BUFFERED_AUTOMATION_MESSAGES as u64 {
                assert_eq!(received_sequence(&stalled), sequence);
            }
            let AutomationMessage::Expired(expiry) = stalled.recv().unwrap() else {
                panic!("missing stalled-subscriber overflow notice")
            };
            assert_eq!(expiry.requested.sequence, after);
            assert!(stalled.recv_timeout(Duration::from_secs(3)).is_err());
        }
        let events = client.subscribe_automation(session_id);
        if !stalled_subscriber {
            let AutomationMessage::Expired(expiry) = events.recv().unwrap() else {
                panic!("missing pre-subscription overflow notice")
            };
            assert_eq!(expiry.requested.sequence, 0);
        }
        for sequence in MAX_AUTOMATION_EVENTS as u64 + 2..=newest {
            assert_eq!(received_sequence(&events), sequence);
        }
        assert!(events.try_recv().is_err());
        assert_eq!(
            client.last_automation_cursor(session_id).unwrap().sequence,
            newest
        );
        let other_events = client.subscribe_automation(other_session);
        assert_eq!(received_sequence(&other_events), 1000);

        client
            .resume_automation(
                session_id,
                Some(serde_json::from_value(cursor(after)).unwrap()),
            )
            .unwrap();
        if after == 0 {
            let AutomationMessage::Expired(expiry) = events.recv().unwrap() else {
                panic!("missing server retention expiry")
            };
            assert_eq!(expiry.requested.sequence, 0);
            assert_eq!(expiry.oldest_sequence, 11);
            assert_eq!(expiry.cursor.sequence, newest);
        }
        for sequence in 11.max(after + 1)..=newest {
            assert_eq!(received_sequence(&events), sequence);
        }
        assert!(events.try_recv().is_err());
        assert_eq!(
            client.last_automation_cursor(session_id).unwrap().sequence,
            newest
        );
        assert_eq!(received_sequence(&other_events), 1001);
        assert!(other_events.try_recv().is_err());
        assert_eq!(
            client
                .last_automation_cursor(other_session)
                .unwrap()
                .sequence,
            1001
        );

        client.automation_instances().unwrap();
        assert_eq!(received_sequence(&events), newest + 1);
        assert!(events.try_recv().is_err());
        assert_eq!(
            client.last_automation_cursor(session_id).unwrap().sequence,
            newest + 1
        );
        server.join().unwrap();
    }
}

#[test]
fn automation_resume_replaces_unread_backlog_before_full_replay() {
    for mode in 0..3 {
        let fresh_receiver = mode != 0;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let newest = MAX_AUTOMATION_EVENTS as u64 + 10;
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let attach = read(&mut socket);
            // The third case replaces a still-healthy receiver, so racing
            // notifications must be held rather than stranded in that receiver.
            for sequence in 1..=if mode == 2 { 2 } else { newest } {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(sequence)}),
                );
            }
            respond(&mut socket, &attach, attached(0, vec![]));
            let resume = read(&mut socket);
            assert_eq!(resume["command"]["command"]["after"], cursor(0));
            for sequence in [newest, newest + 1, newest + 1] {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(sequence)}),
                );
            }
            send(
                &mut socket,
                json!({"type":"automationWaitResolved","sessionId":SESSION,
                "wait":{"waitId":INSTANCE,"status":"succeeded","cursor":cursor(newest),"result":true,"error":null}}),
            );
            let mut replay = attached(newest, (11..=newest).map(event).collect());
            replay["replay"]["expiry"] =
                json!({"requested":cursor(0),"oldestSequence":11,"cursor":cursor(newest)});
            respond(&mut socket, &resume, replay);
            let instances = read(&mut socket);
            for sequence in [newest + 1, newest + 2, newest + 2] {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(sequence)}),
                );
            }
            respond(
                &mut socket,
                &instances,
                json!({"type":"instances","instances":[]}),
            );
        });
        let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
        let session_id = uuid(SESSION);
        // Model a stale REPL receiver as well as resume-before-first-subscribe.
        // Neither path drains old events to make room for the retained tail.
        let _stale = fresh_receiver.then(|| client.subscribe_automation(session_id));
        client.attach_automation(uuid(INSTANCE)).unwrap();
        let after = Some(serde_json::from_value(cursor(0)).unwrap());
        let events = if fresh_receiver {
            let (result, events) = client
                .resume_automation_with_subscription(session_id, after, Duration::from_secs(3))
                .unwrap();
            assert!(matches!(result, AutomationResult::Attached { .. }));
            events
        } else {
            client.resume_automation(session_id, after).unwrap();
            client.subscribe_automation(session_id)
        };
        let AutomationMessage::Expired(expiry) =
            events.recv_timeout(Duration::from_secs(3)).unwrap()
        else {
            panic!("missing retained-tail expiry")
        };
        assert_eq!(expiry.oldest_sequence, 11);
        assert_eq!(expiry.requested.sequence, 0);
        for sequence in 11..=newest {
            assert_eq!(received_sequence(&events), sequence);
        }
        assert_eq!(received_sequence(&events), newest + 1);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)).unwrap(),
            AutomationMessage::WaitResolved(_)
        ));
        assert!(events.try_recv().is_err());
        client.automation_instances().unwrap();
        assert_eq!(received_sequence(&events), newest + 2);
        assert!(events.try_recv().is_err());
        server.join().unwrap();
    }
}

#[test]
fn automation_failed_resume_preserves_the_existing_receiver_and_pending_backlog() {
    for subscribed in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let attach = read(&mut socket);
            for sequence in [1, 2] {
                send(
                    &mut socket,
                    json!({"type":"automationEvent","event":event(sequence)}),
                );
            }
            respond(&mut socket, &attach, attached(0, vec![]));
            let resume = read(&mut socket);
            send(
                &mut socket,
                json!({"type":"automationEvent","event":event(3)}),
            );
            respond(
                &mut socket,
                &resume,
                json!({"type":"error","error":{"code":"unavailable","message":"try again"}}),
            );
            let instances = read(&mut socket);
            respond(
                &mut socket,
                &instances,
                json!({"type":"instances","instances":[]}),
            );
        });
        let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
        let session_id = uuid(SESSION);
        let existing = subscribed.then(|| client.subscribe_automation(session_id));
        client.attach_automation(uuid(INSTANCE)).unwrap();
        assert!(
            client
                .resume_automation_with_subscription(
                    session_id,
                    Some(serde_json::from_value(cursor(0)).unwrap()),
                    Duration::from_secs(3)
                )
                .is_err()
        );
        let events = existing.unwrap_or_else(|| client.subscribe_automation(session_id));
        for sequence in 1..=3 {
            assert_eq!(received_sequence(&events), sequence);
        }
        assert!(events.try_recv().is_err());
        assert!(client.inner.automation.lock().resume_subscribers.is_empty());
        client.automation_instances().unwrap();
        server.join().unwrap();
    }
}

#[test]
fn automation_late_resume_response_cannot_replace_a_newer_staged_receiver() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let attach = read(&mut socket);
        for sequence in [1, 2] {
            send(
                &mut socket,
                json!({"type":"automationEvent","event":event(sequence)}),
            );
        }
        respond(&mut socket, &attach, attached(0, vec![]));
        let timed_out = read(&mut socket);
        send(
            &mut socket,
            json!({"type":"automationEvent","event":event(3)}),
        );
        // Reading the next request is a deterministic barrier: the caller has
        // timed out, kept its old receiver, and staged a replacement again.
        let resumed = read(&mut socket);
        assert_eq!(resumed["command"]["command"]["after"], cursor(3));
        respond(
            &mut socket,
            &timed_out,
            attached(3, vec![event(1), event(2), event(3)]),
        );
        send(
            &mut socket,
            json!({"type":"automationEvent","event":event(5)}),
        );
        respond(&mut socket, &resumed, attached(5, vec![event(4), event(5)]));
        let instances = read(&mut socket);
        send(
            &mut socket,
            json!({"type":"automationEvent","event":event(6)}),
        );
        respond(
            &mut socket,
            &instances,
            json!({"type":"instances","instances":[]}),
        );
    });
    let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
    let session_id = uuid(SESSION);
    let old = client.subscribe_automation(session_id);
    client.attach_automation(uuid(INSTANCE)).unwrap();
    let error = client
        .resume_automation_with_subscription(
            session_id,
            Some(serde_json::from_value(cursor(0)).unwrap()),
            Duration::from_millis(150),
        )
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    for sequence in 1..=3 {
        assert_eq!(received_sequence(&old), sequence);
    }
    assert!(client.inner.automation.lock().resume_subscribers.is_empty());
    let (_, fresh) = client
        .resume_automation_with_subscription(
            session_id,
            Some(serde_json::from_value(cursor(3)).unwrap()),
            Duration::from_secs(3),
        )
        .unwrap();
    for receiver in [&old, &fresh] {
        assert_eq!(received_sequence(receiver), 4);
        assert_eq!(received_sequence(receiver), 5);
        assert!(receiver.try_recv().is_err());
    }
    assert!(client.inner.automation.lock().resume_subscribers.is_empty());
    assert_eq!(
        client.last_automation_cursor(session_id).unwrap().sequence,
        5
    );
    client.automation_instances().unwrap();
    assert_eq!(received_sequence(&old), 6);
    assert_eq!(received_sequence(&fresh), 6);
    assert!(fresh.try_recv().is_err());
    server.join().unwrap();
}

#[test]
fn queued_executor_work_does_not_block_other_rpc_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let request_id = Uuid::new_v4();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let register = read(&mut socket);
        respond(
            &mut socket,
            &register,
            json!({"type":"registered","instance":instance()}),
        );
        send(
            &mut socket,
            json!({"type":"automationExecutorRequest","request":{
            "requestId":request_id,"sessionId":SESSION,"instanceId":INSTANCE,"call":{"type":"describe"}}}),
        );
        send(
            &mut socket,
            json!({"type":"automationInterest","instanceId":INSTANCE,"attached":true,"streams":["ui","model"]}),
        );
        let ordinary = read(&mut socket);
        assert_eq!(ordinary["command"]["type"], "getSettings");
        send(
            &mut socket,
            json!({"type":"response","requestId":ordinary["requestId"],"outcome":{"status":"ok","payload":{"type":"ack"}}}),
        );
        let reply = read(&mut socket);
        assert_eq!(reply["type"], "automationExecutorReply");
        assert_eq!(reply["reply"]["requestId"], json!(request_id));
        let event = read(&mut socket);
        assert_eq!(event["type"], "automationExecutorEvent");
    });
    let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
    let incoming = client.subscribe_automation_executor();
    client
        .register_automation_executor(serde_json::from_value(instance()).unwrap())
        .unwrap();
    assert!(matches!(
        client
            .request(Uuid::nil(), Uuid::nil(), Command::GetSettings)
            .unwrap(),
        ResponsePayload::Ack
    ));
    let AutomationExecutorMessage::Request(request) =
        incoming.recv_timeout(Duration::from_secs(3)).unwrap()
    else {
        panic!("missing executor request")
    };
    assert_eq!(request.request_id, request_id);
    assert!(matches!(
        incoming.recv_timeout(Duration::from_secs(3)).unwrap(),
        AutomationExecutorMessage::Interest { attached: true, .. }
    ));
    client
        .reply_automation_executor(AutomationExecutorReply {
            request_id,
            outcome: AutomationExecutorOutcome::Error {
                error: AutomationError::new("test", "test failure"),
            },
        })
        .unwrap();
    client
        .publish_automation_event(AutomationExecutorEvent {
            instance_id: uuid(INSTANCE),
            stream: AutomationStream::Ui,
            observation: Default::default(),
        })
        .unwrap();
    server.join().unwrap();
}

#[test]
fn automation_errors_keep_the_original_failure_bundle() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let request = read(&mut socket);
        respond(
            &mut socket,
            &request,
            json!({"type":"error","error":{"code":"not_found","message":"missing node","target":"missing",
            "nearest":["composer.send"],"bundle":{"observed":{"state":{"ready":true}},"treeDiff":{"removed":["missing"]},
                "recentEvents":null,"counters":{"observedAtMs":4,"processes":[]},"diagnosticErrors":["evidence unavailable"]}}}),
        );
    });
    let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
    let error = client
        .automation_call(uuid(SESSION), AutomationCall::Describe)
        .unwrap_err();
    let error = &error.downcast_ref::<DaemonAutomationError>().unwrap().error;
    assert_eq!(error.code, "not_found");
    assert_eq!(error.nearest.as_ref().unwrap(), &["composer.send"]);
    assert_eq!(
        error.bundle.as_ref().unwrap().observed["state"]["ready"],
        true
    );
    assert_eq!(
        error.bundle.as_ref().unwrap().diagnostic_errors,
        ["evidence unavailable"]
    );
    server.join().unwrap();
}

#[test]
fn automation_disconnect_closes_only_the_client_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        match socket.read().unwrap() {
            Message::Close(_) => {}
            other => panic!("disconnect sent unexpected protocol message: {other:?}"),
        }
    });
    let client = DaemonClient::connect(&address, "test-token".into()).unwrap();
    client.disconnect();
    assert!(client.is_disconnected());
    server.join().unwrap();
}

#[test]
fn automation_slow_subscriber_is_bounded_and_gets_an_explicit_gap() {
    let session_id = uuid(SESSION);
    let mut state = AutomationState::default();
    let (subscriber, receiver) = AutomationSubscriber::new(MAX_BUFFERED_AUTOMATION_MESSAGES);
    state.subscribers.insert(session_id, vec![subscriber]);
    for sequence in 1..=MAX_BUFFERED_AUTOMATION_MESSAGES as u64 + 10 {
        state.event(serde_json::from_value(event(sequence)).unwrap());
    }
    assert_eq!(receiver.len(), MAX_BUFFERED_AUTOMATION_MESSAGES + 1);
    for sequence in 1..=MAX_BUFFERED_AUTOMATION_MESSAGES as u64 {
        assert_eq!(received_sequence(&receiver), sequence);
    }
    let AutomationMessage::Expired(expiry) = receiver.recv().unwrap() else {
        panic!("missing overflow notice")
    };
    assert_eq!(
        expiry.requested.sequence,
        MAX_BUFFERED_AUTOMATION_MESSAGES as u64
    );
    assert!(receiver.recv().is_err());
    assert!(state.subscribers[&session_id].is_empty());
    assert_eq!(state.pending[&session_id].messages.len(), 10);
}

#[test]
fn automation_pre_subscription_and_resume_buffers_are_bounded() {
    let session_id = uuid(SESSION);
    let mut state = AutomationState::default();
    for sequence in 1..=MAX_AUTOMATION_EVENTS as u64 + 10 {
        state.event(serde_json::from_value(event(sequence)).unwrap());
    }
    let pending = &state.pending[&session_id].messages;
    assert!(pending.len() <= MAX_BUFFERED_AUTOMATION_MESSAGES);
    assert!(matches!(
        pending.front(),
        Some(AutomationMessage::Expired(_))
    ));
    assert_eq!(
        state.cursors[&session_id].sequence,
        MAX_AUTOMATION_EVENTS as u64 + 10
    );

    let mut state = AutomationState::default();
    state.resuming.insert(session_id);
    for sequence in 1..=MAX_AUTOMATION_EVENTS as u64 * 2 {
        state.event(serde_json::from_value(event(sequence)).unwrap());
    }
    assert_eq!(
        state.resume_messages[&session_id].len(),
        MAX_AUTOMATION_EVENTS
    );
    assert_eq!(
        state.resume_dropped[&session_id].cursor.sequence,
        MAX_AUTOMATION_EVENTS as u64
    );
    state.finish_resume(session_id);
    assert!(state.resume_messages.is_empty());
    assert!(state.resume_dropped.is_empty());
    assert!(
        state.pending[&session_id]
            .messages
            .iter()
            .any(|message| matches!(message, AutomationMessage::Expired(_)))
    );
    assert_eq!(
        state.cursors[&session_id].sequence,
        MAX_AUTOMATION_EVENTS as u64 * 2
    );
}

#[test]
fn automation_full_retained_tail_keeps_its_expiry_before_first_subscriber() {
    let session_id = uuid(SESSION);
    let mut state = AutomationState::default();
    state.deliver(
        session_id,
        AutomationMessage::Expired(AutomationExpiry {
            requested: serde_json::from_value(cursor(0)).unwrap(),
            oldest_sequence: 2,
            cursor: serde_json::from_value(cursor(MAX_AUTOMATION_EVENTS as u64 + 1)).unwrap(),
        }),
    );
    for sequence in 2..=MAX_AUTOMATION_EVENTS as u64 + 1 {
        state.event(serde_json::from_value(event(sequence)).unwrap());
    }
    let pending = state.pending.remove(&session_id).unwrap().messages;
    assert_eq!(pending.len(), MAX_AUTOMATION_EVENTS + 1);
    assert!(matches!(
        pending.front(),
        Some(AutomationMessage::Expired(_))
    ));
    let (mut subscriber, receiver) = AutomationSubscriber::new(MAX_BUFFERED_AUTOMATION_MESSAGES);
    for message in pending {
        assert!(subscriber.deliver(message));
    }
    assert!(matches!(
        receiver.recv().unwrap(),
        AutomationMessage::Expired(_)
    ));
    for sequence in 2..=MAX_AUTOMATION_EVENTS as u64 + 1 {
        assert_eq!(received_sequence(&receiver), sequence);
    }
}

#[test]
fn automation_transport_slots_are_bounded_until_the_message_is_written_or_dropped() {
    let slots = AutomationBackpressure::new();
    let mut messages = Vec::new();
    for _ in 0..MAX_AUTOMATION_OUTGOING {
        messages.push(Outgoing::Automation(
            ClientMessage::Shutdown,
            slots.acquire(Duration::ZERO).unwrap(),
        ));
    }
    assert!(slots.acquire(Duration::ZERO).is_err());
    drop(messages.pop());
    let replacement = slots.acquire(Duration::ZERO).unwrap();
    assert!(slots.acquire(Duration::ZERO).is_err());
    drop(replacement);
    drop(messages);
    assert_eq!(slots.available.len(), MAX_AUTOMATION_OUTGOING);
}
