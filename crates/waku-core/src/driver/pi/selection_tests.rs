//! Provider-free process fixtures for selection restoration after native forks.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

struct FakePi {
    root: PathBuf,
    binary: PathBuf,
}

impl FakePi {
    fn new(failure: &str, fail_leg: &str, reset_selection: bool) -> Self {
        let root = std::env::temp_dir().join(format!("doki-pi-selection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("fakePi");
        let config = json!({"log": root.join("wire.jsonl"), "failure": failure,
            "failLeg": fail_leg, "resetSelection": reset_selection});
        std::fs::write(
            &binary,
            format!(
                "#!/usr/bin/env node\nconst config = {config};\n{}",
                include_str!("../fixtures/pi_selection.js")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, binary }
    }

    fn launch(
        &self,
        flavor: PiFlavor,
        resume: bool,
    ) -> (PiDriver, crossbeam_channel::Receiver<DriverEvent>) {
        let (events, receiver) = crate::driver::test_event_channel();
        let driver = PiDriver::launch(
            flavor,
            DriverStartOptions {
                binary: self.binary.clone(),
                cwd: self.root.clone(),
                mode: RuntimeMode::FullAccess,
                model: Some("selected/auto/model-a".into()),
                reasoning_effort: Some("high".into()),
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: resume
                    .then(|| flavor.cursor("source".into(), Some(self.root.join("source.jsonl")))),
            },
            events,
            &[],
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            match receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                DriverEvent::Connected { .. } => return (driver, receiver),
                DriverEvent::Error(error) => panic!("initialization failed: {error}"),
                _ => {}
            }
        }
    }

    fn wire(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("wire.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn shutdown(&self, driver: PiDriver, receiver: &crossbeam_channel::Receiver<DriverEvent>) {
        drop(driver);
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if matches!(
                receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap(),
                DriverEvent::ProcessExited
            ) {
                return;
            }
        }
    }
}

impl Drop for FakePi {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("fakePi evidence retained at {}", self.root.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn selection_options() -> SessionOptions {
    SessionOptions {
        mode: RuntimeMode::FullAccess,
        model: Some("selected/auto/model-a".into()),
        reasoning_effort: Some("high".into()),
        service_tier: None,
        context_window: None,
    }
}

#[test]
fn fork_and_rollback_restore_current_selection_before_prompt() {
    for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
        for rollback in [false, true] {
            for turns in [0, 1] {
                if rollback && turns == 0 {
                    continue;
                }
                let fixture = FakePi::new("", "", true);
                let (driver, receiver) = fixture.launch(flavor, false);
                let cursor = if rollback {
                    driver.rollback(turns).unwrap().unwrap()
                } else {
                    driver.fork(turns).unwrap()
                };
                assert_eq!(cursor.native_id(), "forked");
                assert!(
                    driver.apply_options(selection_options()),
                    "unchanged options still use the writer cache"
                );
                driver.prompt("verify selected model".into());
                fixture.shutdown(driver, &receiver);
                let wire = fixture.wire();
                let prompt = wire.iter().find(|frame| frame["type"] == "prompt").unwrap();
                assert_eq!(
                    prompt["activeModel"], "selected/auto/model-a",
                    "{flavor:?}, rollback={rollback}, turns={turns}: {wire:?}"
                );
                assert_eq!(prompt["activeEffort"], "high");
                assert_eq!(
                    prompt["activeSession"],
                    if rollback { "forked" } else { "source" }
                );
                if flavor == PiFlavor::OhMyPi && turns == 0 {
                    assert!(!wire.iter().any(|frame| frame["type"] == "switch_session"));
                    assert_eq!(
                        wire.iter()
                            .filter(|frame| frame["type"] == "set_model")
                            .count(),
                        1,
                        "out-of-process clone leaves source selection untouched"
                    );
                } else {
                    let mutation = wire
                        .iter()
                        .position(|frame| {
                            matches!(frame["type"].as_str(), Some("fork" | "branch" | "clone"))
                        })
                        .unwrap();
                    let after = &wire[mutation + 1..];
                    assert!(after.iter().any(|frame| frame["type"] == "set_model"));
                    assert!(
                        after
                            .iter()
                            .any(|frame| frame["type"] == "set_thinking_level")
                    );
                    let reads = after
                        .iter()
                        .filter(|frame| frame["type"] == "get_state")
                        .count();
                    assert_eq!(
                        reads,
                        if rollback { 2 } else { 4 },
                        "read back both legs after setters"
                    );
                }
            }
        }
    }
}

#[test]
fn failed_selection_restore_rejects_operation_and_never_prompts() {
    for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
        for leg in ["fork", "switch"] {
            for failure in [
                "set_model",
                "set_thinking_level",
                "get_state",
                "state_readback",
                "model_readback",
                "effort_readback",
            ] {
                let fixture = FakePi::new(failure, leg, true);
                let (driver, receiver) = fixture.launch(flavor, false);
                let result = if leg == "fork" {
                    driver.rollback(1).map(|_| ())
                } else {
                    driver.fork(1).map(|_| ())
                };
                assert!(result.is_err(), "{flavor:?} {leg} {failure} must fail");
                driver.prompt("must not reach a model".into());
                fixture.shutdown(driver, &receiver);
                assert!(
                    !fixture.wire().iter().any(|frame| frame["type"] == "prompt"),
                    "{flavor:?} {leg} {failure}"
                );
            }
        }
    }
}

#[test]
fn resume_applies_selection_once_and_matching_forks_need_no_setters() {
    for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
        let fixture = FakePi::new("", "", false);
        let (driver, receiver) = fixture.launch(flavor, true);
        driver.fork(1).unwrap();
        driver.prompt("already selected".into());
        fixture.shutdown(driver, &receiver);
        let wire = fixture.wire();
        for setter in ["set_model", "set_thinking_level"] {
            assert_eq!(
                wire.iter().filter(|frame| frame["type"] == setter).count(),
                1,
                "{setter} is only needed at initialization"
            );
        }
    }
}

#[test]
fn failed_fork_still_restores_source_before_retiring_writer() {
    for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
        let fixture = FakePi::new("model_readback", "fork", true);
        let (driver, receiver) = fixture.launch(flavor, false);
        assert!(driver.fork(1).is_err());
        driver.prompt("must not reach a model".into());
        fixture.shutdown(driver, &receiver);
        let wire = fixture.wire();
        assert!(wire.iter().any(|frame| frame["type"] == "switch_session"));
        assert!(!wire.iter().any(|frame| frame["type"] == "prompt"));
        let final_state = wire.last().unwrap();
        assert_eq!(final_state["activeSession"], "source");
        assert_eq!(final_state["activeModel"], "selected/auto/model-a");
        assert_eq!(final_state["activeEffort"], "high");
    }
}

#[test]
fn confirmed_cancelled_fork_keeps_source_usable() {
    for flavor in [PiFlavor::Pi, PiFlavor::OhMyPi] {
        for rollback in [false, true] {
            let fixture = FakePi::new("cancel_fork", "fork", true);
            let (driver, receiver) = fixture.launch(flavor, false);
            let result = if rollback {
                driver.rollback(1).map(|_| ())
            } else {
                driver.fork(1).map(|_| ())
            };
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            driver.prompt("source still selected".into());
            fixture.shutdown(driver, &receiver);
            let wire = fixture.wire();
            let prompt = wire.iter().find(|frame| frame["type"] == "prompt").unwrap();
            assert_eq!(prompt["activeSession"], "source");
            assert_eq!(prompt["activeModel"], "selected/auto/model-a");
            assert_eq!(prompt["activeEffort"], "high");
        }
    }
}
