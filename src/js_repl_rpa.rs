//! Native-only daemon authentication. Never place endpoint contents in QuickJS.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;
use waku_client::{AutomationMessage, DaemonClient};
use waku_protocol::{AutomationCommand, Command, ResponsePayload};

#[derive(Deserialize)]
struct Endpoint {
    address: String,
    token: String,
}

pub(super) struct NativeRpaClient {
    path: Option<PathBuf>,
    client: Option<DaemonClient>,
    secret: Option<String>,
    subscriptions: HashMap<Uuid, crossbeam_channel::Receiver<AutomationMessage>>,
}

impl NativeRpaClient {
    pub(super) fn new() -> Self {
        Self {
            path: std::env::var_os("WAKU_RPA_REPL_ENV_FILE").map(PathBuf::from),
            client: None,
            secret: None,
            subscriptions: HashMap::new(),
        }
    }

    fn connect(&mut self, deadline: Instant) -> anyhow::Result<&DaemonClient> {
        remaining(deadline)?;
        if self
            .client
            .as_ref()
            .is_some_and(DaemonClient::is_disconnected)
        {
            self.client.take();
            self.subscriptions.clear();
        }
        if self.client.is_none() {
            let path = self
                .path
                .as_ref()
                .context("debug RPA is not configured for this REPL")?;
            let metadata = std::fs::symlink_metadata(path)
                .context("debug RPA configuration is unavailable")?;
            if !metadata.is_file() || metadata.len() > 8192 {
                bail!("invalid debug RPA configuration file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                let parent = std::fs::symlink_metadata(
                    path.parent()
                        .context("invalid debug RPA configuration path")?,
                )?;
                if metadata.mode() & 0o077 != 0
                    || parent.mode() & 0o077 != 0
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || parent.uid() != metadata.uid()
                    || !parent.is_dir()
                {
                    bail!("debug RPA configuration must be private to its owner");
                }
            }
            let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(path)?)
                .context("invalid debug RPA configuration")?;
            // Connection failures are deliberately generic: neither an address
            // carrying credentials nor a peer's rejection may echo the token.
            let secret = endpoint.token.clone();
            self.client = Some(
                DaemonClient::connect_with_deadline(&endpoint.address, endpoint.token, deadline)
                    .map_err(|_| anyhow::anyhow!("could not connect to the debug RPA daemon"))?,
            );
            self.secret = Some(secret);
        }
        Ok(self.client.as_ref().expect("connected above"))
    }

    pub(super) fn call(&mut self, method: &str, args: Value, deadline: Option<Instant>) -> Value {
        let result = self.execute(method, args, deadline);
        let mut envelope = match result {
            Ok(value) => json!({"ok": true, "value": value}),
            Err(error) => {
                if let Some(error) = error.downcast_ref::<waku_client::DaemonAutomationError>() {
                    json!({"ok": false, "error": error.error})
                } else {
                    json!({"ok": false, "error": {"code": "transport", "message": error.to_string()}})
                }
            }
        };
        if let Some(secret) = self.secret.as_deref() {
            redact_capability(&mut envelope, secret);
        }
        envelope
    }

    fn execute(
        &mut self,
        method: &str,
        args: Value,
        deadline: Option<Instant>,
    ) -> anyhow::Result<Value> {
        let deadline = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        remaining(deadline)?;
        if method == "events" {
            let session_id: Uuid = serde_json::from_value(args["sessionId"].clone())?;
            let receiver = self
                .subscriptions
                .get(&session_id)
                .context("subscribe to this automation session first")?;
            let mut events = Vec::new();
            let mut disconnected = false;
            for _ in 0..=waku_protocol::MAX_AUTOMATION_EVENTS {
                let event = match receiver.try_recv() {
                    Ok(event) => event,
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                events.push(match event {
                    AutomationMessage::Event(event) => json!({"type": "event", "event": event}),
                    AutomationMessage::Expired(expiry) => {
                        json!({"type": "expired", "expiry": expiry})
                    }
                    AutomationMessage::WaitResolved(wait) => {
                        json!({"type": "waitResolved", "wait": wait})
                    }
                    AutomationMessage::RunChanged(run) => json!({"type": "runChanged", "run": run}),
                });
            }
            if disconnected {
                self.subscriptions.remove(&session_id);
            }
            return Ok(json!(events));
        }
        if method == "automation" {
            let command: AutomationCommand =
                serde_json::from_value(args).context("invalid automation command")?;
            if matches!(command, AutomationCommand::RegisterExecutor { .. }) {
                bail!("the REPL cannot register a UI executor");
            }
            self.connect(deadline)?;
            if let AutomationCommand::Resume { session_id, after } = command {
                // The transport activates a fresh receiver atomically with the
                // successful replay. Pre-subscribing would preload stale local
                // pending events and duplicate/overflow a complete retained tail.
                let (result, receiver) = self
                    .client
                    .as_ref()
                    .unwrap()
                    .resume_automation_with_subscription(session_id, after, remaining(deadline)?)?;
                self.subscriptions.insert(session_id, receiver);
                return Ok(serde_json::to_value(result)?);
            }
            let mut new_subscription = None;
            if let AutomationCommand::Subscribe { session_id, .. } = &command {
                if !self.subscriptions.contains_key(session_id) {
                    new_subscription = Some(*session_id);
                }
                self.subscriptions.entry(*session_id).or_insert_with(|| {
                    self.client
                        .as_ref()
                        .unwrap()
                        .subscribe_automation(*session_id)
                });
            }
            let detach = if let AutomationCommand::Detach { session_id } = &command {
                Some(*session_id)
            } else {
                None
            };
            let result = match remaining(deadline).and_then(|timeout| {
                self.client
                    .as_ref()
                    .unwrap()
                    .automation_with_timeout(command, timeout)
            }) {
                Ok(result) => result,
                Err(error) => {
                    if let Some(session_id) = new_subscription {
                        self.subscriptions.remove(&session_id);
                    }
                    return Err(error);
                }
            };
            if let Some(session_id) = detach {
                self.subscriptions.remove(&session_id);
            }
            return Ok(serde_json::to_value(result)?);
        }
        let command = match method {
            "logs" => Command::DiagnosticLogs {
                query: serde_json::from_value(args)?,
            },
            "timeline" => Command::DiagnosticTimeline {
                query: serde_json::from_value(args)?,
            },
            "counters" => Command::DiagnosticCounters,
            "verbosity" => Command::DiagnosticVerbosity {
                update: serde_json::from_value(args)?,
            },
            _ => bail!("unsupported debug RPA operation"),
        };
        let client = self.connect(deadline)?;
        let response =
            client.request_with_timeout(Uuid::nil(), Uuid::nil(), command, remaining(deadline)?)?;
        Ok(match response {
            ResponsePayload::DiagnosticLogs { result } => serde_json::to_value(result)?,
            ResponsePayload::DiagnosticTimeline { result } => serde_json::to_value(result)?,
            ResponsePayload::DiagnosticCounters { result } => serde_json::to_value(result)?,
            ResponsePayload::DiagnosticVerbosity { verbosity } => serde_json::to_value(verbosity)?,
            _ => bail!("unexpected debug RPA response"),
        })
    }
}

fn remaining(deadline: Instant) -> anyhow::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("debug RPA execution deadline elapsed");
    }
    Ok(remaining)
}

fn redact_capability(value: &mut Value, secret: &str) {
    if secret.is_empty() {
        return;
    }
    match value {
        Value::String(text) => *text = text.replace(secret, "[redacted]"),
        Value::Array(values) => {
            for value in values {
                redact_capability(value, secret);
            }
        }
        Value::Object(values) => {
            let original = std::mem::take(values);
            for (key, mut value) in original {
                redact_capability(&mut value, secret);
                values.insert(key.replace(secret, "[redacted]"), value);
            }
        }
        _ => {}
    }
}

impl Drop for NativeRpaClient {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.disconnect();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    use tungstenite::{Message, WebSocket};

    struct Fixture {
        directory: PathBuf,
        client: NativeRpaClient,
    }

    impl Fixture {
        fn new(address: String) -> Self {
            let directory =
                std::env::temp_dir().join(format!("waku-repl-deadline-{}", Uuid::new_v4()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .unwrap();
            let path = directory.join("endpoint.json");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            write!(
                file,
                "{}",
                json!({"address":address,"token":"private-fixture-token"})
            )
            .unwrap();
            Self {
                directory,
                client: NativeRpaClient {
                    path: Some(path),
                    client: None,
                    secret: None,
                    subscriptions: HashMap::new(),
                },
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn read_peer(socket: &mut WebSocket<TcpStream>) -> Value {
        loop {
            if let Message::Text(text) = socket.read().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    fn accept_peer(listener: &TcpListener) -> WebSocket<TcpStream> {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        assert_eq!(read_peer(&mut socket)["type"], "hello");
        socket
    }

    fn send_hello(socket: &mut WebSocket<TcpStream>) {
        socket.send(Message::Text(json!({
            "type":"hello", "protocolVersion":waku_protocol::PROTOCOL_VERSION, "daemonVersion":"test",
        }).to_string().into())).unwrap();
    }

    #[test]
    fn automation_repl_deadline_bounds_websocket_and_daemon_hello_handshakes() {
        for delay_hello in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut fixture = Fixture::new(listener.local_addr().unwrap().to_string());
            let (release, blocked) = crossbeam_channel::bounded::<()>(1);
            let server = std::thread::spawn(move || {
                if delay_hello {
                    let _socket = accept_peer(&listener);
                    let _ = blocked.recv_timeout(Duration::from_secs(2));
                } else {
                    let (_stream, _) = listener.accept().unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(2));
                }
            });
            let start = Instant::now();
            let result = fixture.client.call(
                "counters",
                json!({}),
                Some(start + Duration::from_millis(150)),
            );
            let elapsed = start.elapsed();
            let _ = release.send(());
            server.join().unwrap();
            assert_eq!(result["ok"], false);
            assert!(!result.to_string().contains("private-fixture-token"));
            assert!(fixture.client.client.is_none());
            assert!(
                elapsed < Duration::from_millis(500),
                "handshake exceeded execution deadline: {elapsed:?}"
            );
        }
    }

    #[test]
    fn automation_repl_connection_and_rpc_share_one_execution_budget() {
        for (method, args) in [
            ("automation", json!({"type":"instances"})),
            ("counters", json!({})),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut fixture = Fixture::new(listener.local_addr().unwrap().to_string());
            let (release, blocked) = crossbeam_channel::bounded::<()>(1);
            let server = std::thread::spawn(move || {
                let mut socket = accept_peer(&listener);
                std::thread::sleep(Duration::from_millis(300));
                send_hello(&mut socket);
                assert_eq!(read_peer(&mut socket)["type"], "request");
                let _ = blocked.recv_timeout(Duration::from_secs(2));
            });
            let start = Instant::now();
            let result =
                fixture
                    .client
                    .call(method, args, Some(start + Duration::from_millis(500)));
            let elapsed = start.elapsed();
            let _ = release.send(());
            server.join().unwrap();
            assert_eq!(result["ok"], false);
            assert!(
                fixture.client.client.is_some(),
                "Hello must finish before the RPC times out"
            );
            assert!(
                elapsed < Duration::from_millis(650),
                "connection time was added back to the RPC budget: {elapsed:?}"
            );
        }
    }

    #[test]
    fn automation_repl_bounded_connection_can_complete_a_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut fixture = Fixture::new(listener.local_addr().unwrap().to_string());
        let server = std::thread::spawn(move || {
            let mut socket = accept_peer(&listener);
            send_hello(&mut socket);
            let request = read_peer(&mut socket);
            socket.send(Message::Text(json!({
                "type":"response", "requestId":request["requestId"],
                "outcome":{"status":"ok","payload":{"type":"diagnosticCounters","result":{"observedAtMs":7,"processes":[]}}},
            }).to_string().into())).unwrap();
        });
        let result = fixture.client.call(
            "counters",
            json!({}),
            Some(Instant::now() + Duration::from_secs(1)),
        );
        server.join().unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["value"]["observedAtMs"], 7);
    }

    #[test]
    fn automation_repl_resume_replaces_pending_with_the_complete_retained_tail() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut fixture = Fixture::new(listener.local_addr().unwrap().to_string());
        let session_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let retained = waku_protocol::MAX_AUTOMATION_EVENTS;
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        let server = std::thread::spawn(move || {
            let mut socket = accept_peer(&listener);
            send_hello(&mut socket);
            let cursor = |sequence| json!({"epoch":epoch,"sequence":sequence});
            let event = |sequence| {
                json!({
                    "sessionId":session_id, "cursor":cursor(sequence), "stream":"ui", "payload":{"sequence":sequence},
                })
            };
            let first = read_peer(&mut socket);
            // These are committed into the client's pre-subscription queue
            // before its first RPC response returns.
            for sequence in 1..=2 {
                socket
                    .send(Message::Text(
                        json!({"type":"automationEvent","event":event(sequence)})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
            }
            socket.send(Message::Text(json!({
                "type":"response", "requestId":first["requestId"],
                "outcome":{"status":"ok","payload":{"type":"automation","result":{"type":"instances","instances":[]}}},
            }).to_string().into())).unwrap();
            let resume = read_peer(&mut socket);
            assert_eq!(resume["command"]["command"]["type"], "resume");
            let replay: Vec<_> = (1..=retained).map(event).collect();
            socket.send(Message::Text(json!({
                "type":"response", "requestId":resume["requestId"],
                "outcome":{"status":"ok","payload":{"type":"automation","result":{
                    "type":"attached", "session":{
                        "sessionId":session_id, "cursor":cursor(retained), "streams":["ui"],
                        "instance":{"instanceId":Uuid::new_v4(),"checkout":"/fixture","database":"/fixture/app.db","pid":42,"build":"fixture","version":"test","executorConnected":true,"scratch":true},
                    },
                    "replay":{"events":replay,"cursor":cursor(retained),"expiry":null},
                }}},
            }).to_string().into())).unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(2));
        });
        let initial = fixture.client.call(
            "automation",
            json!({"type":"instances"}),
            Some(Instant::now() + Duration::from_secs(2)),
        );
        let resumed = fixture.client.call(
            "automation",
            json!({"type":"resume","sessionId":session_id,"after":{"epoch":epoch,"sequence":0}}),
            Some(Instant::now() + Duration::from_secs(2)),
        );
        let events = fixture
            .client
            .call("events", json!({"sessionId":session_id}), None);
        let _ = release.send(());
        server.join().unwrap();
        assert_eq!(initial["ok"], true, "{initial}");
        assert_eq!(resumed["ok"], true, "{resumed}");
        let events = events["value"].as_array().unwrap();
        assert_eq!(events.len(), retained);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["type"], "event");
            assert_eq!(event["event"]["cursor"]["sequence"], index + 1);
        }
    }

    #[test]
    fn automation_repl_redacts_a_peers_echo_of_the_capability() {
        let secret = "private-\"-fixture";
        let mut value =
            json!({"error":{"message":format!("denied {secret}"), "bundle":{"observed":[secret]}}});
        redact_capability(&mut value, secret);
        assert_eq!(value["error"]["message"], "denied [redacted]");
        assert_eq!(value["error"]["bundle"]["observed"][0], "[redacted]");
    }

    #[test]
    fn automation_repl_drains_expiry_before_retiring_closed_subscription() {
        let session_id = Uuid::new_v4();
        let cursor = waku_protocol::AutomationCursor {
            epoch: Uuid::new_v4(),
            sequence: 3,
        };
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender
            .send(AutomationMessage::Expired(
                waku_protocol::AutomationExpiry {
                    requested: cursor,
                    oldest_sequence: 4,
                    cursor,
                },
            ))
            .unwrap();
        drop(sender);
        let mut client = NativeRpaClient {
            path: None,
            client: None,
            secret: None,
            subscriptions: HashMap::from([(session_id, receiver)]),
        };
        let result = client.call("events", json!({"sessionId":session_id}), None);
        assert_eq!(result["ok"], true);
        assert_eq!(result["value"][0]["type"], "expired");
        assert!(!client.subscriptions.contains_key(&session_id));
        assert_eq!(
            client.call("events", json!({"sessionId":session_id}), None)["ok"],
            false
        );
    }

    #[test]
    fn automation_repl_rejects_nonprivate_capability_without_disclosing_it() {
        let directory = std::env::temp_dir().join(format!("waku-repl-rpa-test-{}", Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("endpoint.json");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"address":"127.0.0.1:1","token":"private-fixture-token"}"#)
            .unwrap();
        drop(file);
        // Explicit permissions avoid depending on the process umask.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut client = NativeRpaClient {
            path: Some(path.clone()),
            client: None,
            secret: None,
            subscriptions: HashMap::new(),
        };
        let result = client.call("counters", json!({}), None);
        assert_eq!(result["ok"], false);
        assert!(
            result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("private to its owner")
        );
        assert!(!result.to_string().contains("private-fixture-token"));
        assert!(client.client.is_none());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
