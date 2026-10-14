//! Opt-in checks against official installed Pi, without provider credentials or
//! model execution. Run separately for each pinned package cohort.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;
use uuid::Uuid;

const STEP_TIMEOUT: Duration = Duration::from_secs(35);

struct IsolatedPi {
    root: PathBuf,
    binary: PathBuf,
    cwd: PathBuf,
    extension: PathBuf,
}

impl IsolatedPi {
    fn new() -> Self {
        let binary = std::env::var("WAKU_TEST_PI_BINARY")
            .expect("set WAKU_TEST_PI_BINARY to an official pinned Pi executable");
        let root = std::env::temp_dir().join(format!("waku-pi-compat-{}", Uuid::new_v4()));
        let home = root.join("home");
        let cwd = root.join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\"'\"'"));
        let wrapper = root.join("pi-isolated");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec env -i HOME={} PATH={} PI_SKIP_VERSION_CHECK=1 PI_OFFLINE=1 PI_TELEMETRY=0 {} \\\n                 --no-tools --no-extensions --no-mcp --no-skills \\\n                 --no-prompt-templates --no-themes --no-context-files \"$@\"\n",
                quote(&home.to_string_lossy()),
                quote(&std::env::var("PATH").unwrap()),
                quote(&binary),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let extension = root.join("probe.js");
        std::fs::write(
            &extension,
            include_str!("../fixtures/pi_compatibility_probe.js"),
        )
        .unwrap();
        Self {
            root,
            binary: wrapper,
            cwd,
            extension,
        }
    }

    fn launch(
        &self,
        cursor: Option<ProviderResumeCursor>,
    ) -> (PiDriver, crossbeam_channel::Receiver<DriverEvent>) {
        let (events, receiver) = crate::driver::test_event_channel();
        let driver = PiDriver::launch(
            PiFlavor::Pi,
            DriverStartOptions {
                binary: self.binary.clone(),
                cwd: self.cwd.clone(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: cursor,
            },
            events,
            std::slice::from_ref(&self.extension),
        )
        .expect("Pi launches");
        (driver, receiver)
    }
}

impl Drop for IsolatedPi {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn next(receiver: &crossbeam_channel::Receiver<DriverEvent>, deadline: Instant) -> DriverEvent {
    let event = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .expect("Pi must answer within the step deadline");
    if let DriverEvent::Error(error) = &event {
        panic!("Pi reported: {error}");
    }
    event
}

fn connected(receiver: &crossbeam_channel::Receiver<DriverEvent>) -> ProviderResumeCursor {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        match next(receiver, deadline) {
            DriverEvent::Connected {
                provider_cursor: Some(cursor),
            } => return cursor,
            DriverEvent::ProcessExited => panic!("Pi exited before its handshake"),
            _ => {}
        }
    }
}

fn shutdown(driver: PiDriver, receiver: &crossbeam_channel::Receiver<DriverEvent>) {
    drop(driver);
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        if matches!(next(receiver, deadline), DriverEvent::ProcessExited) {
            return;
        }
    }
}

#[test]
#[ignore = "requires WAKU_TEST_PI_BINARY pointing to an official pinned Pi executable"]
fn official_pi_handshake_dialog_clone_resume_and_shutdown_without_a_model() {
    let fixture = IsolatedPi::new();
    let (driver, receiver) = fixture.launch(None);
    let original = connected(&receiver);
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        match next(&receiver, deadline) {
            DriverEvent::AvailableCommands(commands) => {
                assert!(
                    commands
                        .iter()
                        .any(|command| command.name == "waku-compatibility-probe")
                );
                break;
            }
            DriverEvent::ProcessExited => panic!("Pi exited before its command catalog"),
            _ => {}
        }
    }
    driver.prompt("/waku-compatibility-probe".into());
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut notice = false;
    let mut answer = false;
    let mut title = false;
    let mut dialog = false;
    loop {
        match next(&receiver, deadline) {
            DriverEvent::AutoTitleUpdated(value) => {
                title |= value.as_deref() == Some("Doki Pi compatibility")
            }
            DriverEvent::ExtensionMessage {
                custom_type,
                text,
                display,
            } => match custom_type.as_str() {
                "waku-compatibility-notice" => {
                    assert!(display);
                    assert_eq!(text, "Extension message without calling a model");
                    notice = true;
                }
                "waku-compatibility-answer" => {
                    assert!(!display);
                    assert_eq!(text, "true");
                    answer = true;
                }
                _ => panic!("unexpected extension message: {custom_type}"),
            },
            DriverEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                assert!(!dialog);
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].id, request_id);
                driver.respond_user_input(
                    request_id.clone(),
                    vec![UserInputAnswer {
                        question_id: request_id,
                        answers: vec![PI_CONFIRM_ACCEPT.into()],
                    }],
                );
                dialog = true;
            }
            DriverEvent::TurnFinished {
                interrupted,
                success,
                ..
            } => {
                assert!(!interrupted && success);
                break;
            }
            DriverEvent::TurnStarted => panic!("the fixture must never invoke a model"),
            DriverEvent::ProcessExited => panic!("Pi exited during the probe"),
            _ => {}
        }
    }
    assert!(notice && answer && title && dialog);
    // Pi does not persist an extension-only session. Exercise that real
    // restriction, then resume an explicit native-format conversation fixture.
    assert!(
        driver
            .fork(0)
            .unwrap_err()
            .to_string()
            .contains("not been saved")
    );
    shutdown(driver, &receiver);
    let session_id = Uuid::new_v4().to_string();
    let session_file = fixture.root.join("saved-conversation.jsonl");
    let entries = [
        json!({"type": "session", "version": 3, "id": session_id,
            "timestamp": "2026-10-14T00:00:00.000Z", "cwd": fixture.cwd}),
        json!({"type": "message", "id": "1234abcd", "parentId": null,
        "timestamp": "2026-10-14T00:00:01.000Z", "message": {
            "role": "user", "content": "Saved provider-free fixture", "timestamp": 1791936001000_u64
        }}),
    ];
    std::fs::write(
        &session_file,
        entries
            .iter()
            .map(|entry| format!("{entry}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let saved = PiFlavor::Pi.cursor(session_id, Some(session_file));
    assert_ne!(saved, original);
    let (driver, receiver) = fixture.launch(Some(saved.clone()));
    assert_eq!(connected(&receiver), saved);
    let cloned = driver.fork(0).expect("clone the saved native conversation");
    assert_ne!(cloned, saved);
    shutdown(driver, &receiver);

    for cursor in [saved, cloned] {
        let (driver, receiver) = fixture.launch(Some(cursor.clone()));
        assert_eq!(
            connected(&receiver),
            cursor,
            "resume adopts the requested native session"
        );
        shutdown(driver, &receiver);
    }
}

/// The 1.2-only gate proves a handled command does not finish while startup is
/// busy. The extension checks Pi's unknown placeholder before opening a run;
/// releasing its cancelled gate fails locally and cannot resolve a provider.
#[test]
#[ignore = "requires WAKU_TEST_PI_BINARY pointing to official pinned Pi 1.2.0"]
fn official_pi_12_custom_startup_cancel_and_failure_settle_without_a_model() {
    for invalid_section in [false, true] {
        let fixture = IsolatedPi::new();
        let (driver, receiver) = fixture.launch(None);
        connected(&receiver);
        driver.prompt(
            if invalid_section {
                "/waku-compatibility-startup invalid"
            } else {
                "/waku-compatibility-startup"
            }
            .into(),
        );
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut gate = None;
        let mut started = false;
        while gate.is_none() || !started {
            match next(&receiver, deadline) {
                DriverEvent::UserInputRequested { request_id, .. } => gate = Some(request_id),
                DriverEvent::TurnStarted => {
                    assert!(!started);
                    started = true;
                }
                DriverEvent::TurnFinished { .. } => {
                    panic!("handled must not finish a busy startup")
                }
                DriverEvent::ProcessExited => panic!("Pi exited at the startup gate"),
                _ => {}
            }
        }
        if !invalid_section {
            driver.cancel();
        }
        let request_id = gate.unwrap();
        driver.respond_user_input(
            request_id.clone(),
            vec![UserInputAnswer {
                question_id: request_id,
                answers: vec![PI_CONFIRM_ACCEPT.into()],
            }],
        );
        let mut local_provider_refusal = false;
        loop {
            let event = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            match event {
                DriverEvent::Error(error) => {
                    assert!(
                        error.contains(if invalid_section {
                            "Invalid system prompt section name: Invalid Section"
                        } else {
                            "Unknown provider: unknown"
                        }),
                        "unexpected error: {error}"
                    );
                    local_provider_refusal = true;
                }
                DriverEvent::TurnStarted => panic!("agent_start must reuse the preflight turn"),
                DriverEvent::TurnFinished {
                    interrupted,
                    success,
                    ..
                } => {
                    assert_eq!(interrupted, !invalid_section);
                    if invalid_section {
                        assert!(!success);
                    }
                    break;
                }
                DriverEvent::ProcessExited => panic!("Pi exited before settlement"),
                _ => {}
            }
        }
        if invalid_section {
            assert!(
                local_provider_refusal,
                "the invalid section must report its local error"
            );
        }
        shutdown(driver, &receiver);
    }
}

#[test]
#[ignore = "requires WAKU_TEST_PI_BINARY pointing to official pinned Pi 1.2.0"]
fn official_pi_12_fast_custom_startup_failure_is_not_successful() {
    let fixture = IsolatedPi::new();
    let (driver, receiver) = fixture.launch(None);
    connected(&receiver);
    driver.prompt("/waku-compatibility-startup invalid-fast".into());
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut saw_error = false;
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        match event {
            DriverEvent::Error(error) => {
                assert!(
                    error.contains("Invalid system prompt section name: Invalid Section"),
                    "{error}"
                );
                saw_error = true;
            }
            DriverEvent::TurnFinished {
                success,
                interrupted,
                ..
            } => {
                assert!(saw_error);
                assert!(!success && !interrupted);
                break;
            }
            DriverEvent::UserInputRequested { .. } => {
                panic!("the fast failure must not wait on a gate")
            }
            DriverEvent::ProcessExited => panic!("Pi exited before local failure completion"),
            _ => {}
        }
    }
    shutdown(driver, &receiver);
}
