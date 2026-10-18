use super::*;
use crate::driver::DriverControl;
use crate::model::{Project, ProviderResumeCursor};
use crate::{Command, ResponsePayload};
use std::collections::VecDeque;
use std::net::TcpListener;
use waku_client::{DaemonClient, IssueRunClient};

#[derive(Clone, Copy)]
enum Behavior {
    Complete,
    Reject,
    Invalid,
    Fail,
    Hold,
    Permission,
    ErrorHold,
}
#[derive(Default)]
struct FakeHarness {
    script: Mutex<VecDeque<Behavior>>,
    prompts: Arc<Mutex<Vec<String>>>,
    starts: Mutex<Vec<(IssueStageRole, ProviderKind, PathBuf)>>,
    held: Arc<Mutex<Vec<Arc<FakeControl>>>>,
}
impl FakeHarness {
    fn new(script: Vec<Behavior>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            ..Self::default()
        })
    }
    fn release(&self) {
        let controls = self.held.lock().clone();
        for control in controls {
            control.complete(Behavior::Complete);
        }
    }
}
impl IssueHarness for FakeHarness {
    fn validate(
        &self,
        stage: &IssueWorkflowStage,
        _: &waku_protocol::DaemonSettings,
    ) -> anyhow::Result<()> {
        if stage.provider == ProviderKind::DeepSeek {
            bail!("Unsupported fixture harness");
        }
        Ok(())
    }
    fn start(
        &self,
        stage: &IssueWorkflowStage,
        cwd: PathBuf,
        _: &waku_protocol::DaemonSettings,
        events: DriverEventSender,
    ) -> anyhow::Result<DriverHandle> {
        self.starts.lock().push((stage.role, stage.provider, cwd));
        let behavior = self.script.lock().pop_front().unwrap_or(Behavior::Complete);
        let control = Arc::new(FakeControl {
            events,
            role: stage.role,
            behavior,
            prompt: Mutex::new(None),
            prompts: self.prompts.clone(),
            settled: AtomicBool::new(false),
        });
        if matches!(
            behavior,
            Behavior::Hold | Behavior::Permission | Behavior::ErrorHold
        ) {
            self.held.lock().push(control.clone());
        }
        Ok(DriverHandle::from_control(control))
    }
}
struct FakeControl {
    events: DriverEventSender,
    role: IssueStageRole,
    behavior: Behavior,
    prompt: Mutex<Option<String>>,
    prompts: Arc<Mutex<Vec<String>>>,
    settled: AtomicBool,
}
impl FakeControl {
    fn complete(&self, behavior: Behavior) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        let prompt = self.prompt.lock().clone().unwrap();
        let json = prompt
            .rsplit_once(RESULT_MARKER)
            .unwrap()
            .1
            .trim()
            .strip_suffix("```")
            .unwrap()
            .trim();
        let mut outcome: IssueStageOutcome = serde_json::from_str(json).unwrap();
        outcome.summary = format!("{:?} public result", self.role);
        outcome.evidence = vec!["Provider-free fixture verification passed".into()];
        outcome.decision = if matches!(behavior, Behavior::Reject) {
            IssueStageDecision::Rejected
        } else if self.role == IssueStageRole::Review {
            IssueStageDecision::Approved
        } else {
            IssueStageDecision::Completed
        };
        // Private reasoning must never enter the next provider's prompt.
        self.events
            .send(DriverEvent::ReasoningDelta(
                "PRIVATE_SENTINEL_NEVER_HAND_OFF".into(),
            ))
            .unwrap();
        let text = if matches!(behavior, Behavior::Invalid) {
            "Looks good to me, done!".into()
        } else {
            format!(
                "Public commentary\n{RESULT_MARKER}{}\n```",
                serde_json::to_string(&outcome).unwrap()
            )
        };
        self.events.send(DriverEvent::TextDelta(text)).unwrap();
        self.events
            .send(DriverEvent::TurnFinished {
                success: !matches!(behavior, Behavior::Fail),
                summary: None,
                interrupted: false,
            })
            .unwrap();
    }
}
impl DriverControl for FakeControl {
    fn prompt(&self, prompt: String) {
        self.prompts.lock().push(prompt.clone());
        *self.prompt.lock() = Some(prompt);
        self.events.send(DriverEvent::TurnStarted).unwrap();
        match self.behavior {
            Behavior::Hold => (),
            Behavior::ErrorHold => {
                self.events
                    .send(DriverEvent::Error("Fixture provider error".into()))
                    .unwrap();
            }
            Behavior::Permission => {
                self.events
                    .send(DriverEvent::Permission {
                        request_id: "approve-fixture".into(),
                        title: "Fixture command".into(),
                        detail: "No real command".into(),
                        options: vec![],
                    })
                    .unwrap();
            }
            behavior => self.complete(behavior),
        }
    }
    fn cancel(&self) {
        if !self.settled.swap(true, Ordering::AcqRel) {
            let _ = self.events.send(DriverEvent::TurnFinished {
                success: false,
                summary: Some("cancelled".into()),
                interrupted: true,
            });
        }
    }
    fn respond(&self, _: String, _: String) {
        self.complete(Behavior::Complete);
    }
    // The fake models a session-owned service so held test handles do not
    // prevent positive shutdown evidence, exactly like OpenCode.
    fn confirm_session_shutdown(&self) -> Option<anyhow::Result<()>> {
        Some(Ok(()))
    }
    fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        Ok(None)
    }
}

struct Fixture {
    root: PathBuf,
    client: DaemonClient,
    runs: IssueRunClient,
    task: Task,
    server: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(harness: Arc<FakeHarness>) -> Self {
        let root = std::env::temp_dir().join(format!("doki-issue-flow-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("repo")).unwrap();
        let project = Project::from_path(root.join("repo"));
        let mut task = Task::new("Implement fixture issue", TaskState::Backlog, 0);
        task.description = "A public issue brief".into();
        task.workspace = Some(TaskWorkspace::Local {
            project_id: project.id,
        });
        let (client, server) = Self::connect(&root, harness);
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    tasks: vec![task.clone()],
                    sessions: vec![],
                    live_session_ids: vec![],
                    skeleton_session_ids: vec![],
                },
            )
            .unwrap();
        Self {
            root,
            runs: IssueRunClient::new(client.clone()),
            client,
            task,
            server: Some(server),
        }
    }
    fn connect(
        root: &Path,
        harness: Arc<FakeHarness>,
    ) -> (DaemonClient, std::thread::JoinHandle<()>) {
        let backend = crate::daemon::WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_issue_harness(harness);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            crate::server::serve(
                listener,
                "fixture-token".into(),
                Arc::new(backend),
                Arc::new(AtomicBool::new(false)),
                crate::server::ServerOptions {
                    allow_shutdown: true,
                    ..Default::default()
                },
            )
            .unwrap()
        });
        let client = DaemonClient::connect(&address.to_string(), "fixture-token".into()).unwrap();
        (client, server)
    }
    fn start(&self) -> IssueRun {
        self.runs
            .start(
                self.task.id,
                Uuid::new_v4(),
                IssueWorkflow::plan_execute_review_report(ProviderKind::Codex),
            )
            .unwrap()
    }
    fn wait(&self, status: IssueRunStatus) -> IssueRun {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = self
                .runs
                .list(Some(self.task.id))
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            if run.status == status {
                return run;
            }
            assert!(
                Instant::now() < deadline,
                "expected {status:?}, got {:?}: {:?}",
                run.status,
                run.error
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_starts(&self, harness: &FakeHarness, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while harness.starts.lock().len() < count {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn control_when_settled(&self, run_id: Uuid, action: IssueRunAction) -> IssueRun {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.runs.control(run_id, action) {
                Ok(run) => return run,
                Err(error) => assert!(
                    Instant::now() < deadline,
                    "control {action:?} never settled: {error}"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) {
        self.client.shutdown();
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
    fn restart(&mut self, harness: Arc<FakeHarness>) {
        self.stop();
        let (client, server) = Self::connect(&self.root, harness);
        self.runs = IssueRunClient::new(client.clone());
        self.client = client;
        self.server = Some(server);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn daemon_happy_path_persists_report_sessions_and_distinct_harnesses() {
    let harness = FakeHarness::new(vec![]);
    let fixture = Fixture::new(harness.clone());
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    workflow.stages[0].provider = ProviderKind::Claude;
    workflow.stages[2].provider = ProviderKind::Pi;
    let request_id = Uuid::new_v4();
    let initial = fixture
        .runs
        .start(fixture.task.id, request_id, workflow.clone())
        .unwrap();
    let run = fixture.wait(IssueRunStatus::Completed);
    assert_eq!(run.attempts.len(), 4);
    assert!(run.report.is_some());
    assert_eq!(
        fixture
            .runs
            .start(fixture.task.id, request_id, workflow)
            .unwrap()
            .id,
        initial.id
    );
    assert_eq!(harness.starts.lock().len(), 4);
    assert_eq!(harness.starts.lock()[0].1, ProviderKind::Claude);
    assert_eq!(harness.starts.lock()[2].1, ProviderKind::Pi);
    let prompts = harness.prompts.lock();
    assert!(prompts[1].contains("Plan public result"));
    assert!(!prompts.iter().any(|p| p.contains("PRIVATE_SENTINEL")));
    drop(prompts);
    let ResponsePayload::TaskState {
        sessions, tasks, ..
    } = fixture
        .client
        .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(sessions.len(), 4);
    assert_eq!(tasks[0].state, TaskState::Doing);
    for session in sessions {
        let ResponsePayload::Session {
            session: Some(session),
        } = fixture
            .client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::HydrateSession {
                    session_id: session.id,
                },
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].status, TurnStatus::Completed);
        assert_eq!(session.messages.len(), 2);
    }
    let restored: Vec<Record> =
        serde_json::from_slice(&fs::read(fixture.root.join("app.issue-runs.json")).unwrap())
            .unwrap();
    assert_eq!(restored[0].run.report, run.report);
}

#[test]
fn daemon_review_rejects_then_executes_and_reviews_again_with_bound() {
    let harness = FakeHarness::new(vec![
        Behavior::Complete,
        Behavior::Complete,
        Behavior::Reject,
    ]);
    let fixture = Fixture::new(harness.clone());
    fixture.start();
    let run = fixture.wait(IssueRunStatus::Completed);
    assert_eq!(run.review_retries, 1);
    assert_eq!(
        harness
            .starts
            .lock()
            .iter()
            .map(|s| s.0)
            .collect::<Vec<_>>(),
        vec![
            IssueStageRole::Plan,
            IssueStageRole::Execution,
            IssueStageRole::Review,
            IssueStageRole::Execution,
            IssueStageRole::Review,
            IssueStageRole::Report
        ]
    );
    assert!(harness.prompts.lock()[3].contains("Review public result"));
    let harness = FakeHarness::new(vec![
        Behavior::Complete,
        Behavior::Complete,
        Behavior::Reject,
        Behavior::Complete,
        Behavior::Reject,
    ]);
    let fixture = Fixture::new(harness.clone());
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    workflow.max_review_retries = 1;
    fixture
        .runs
        .start(fixture.task.id, Uuid::new_v4(), workflow)
        .unwrap();
    let run = fixture.wait(IssueRunStatus::Blocked);
    assert_eq!(run.review_retries, 1);
    assert_eq!(run.attempts.len(), 5);
    assert!(run.report.is_none());
}

#[test]
fn daemon_pause_drains_stage_resume_deduplicates_and_cancel_stops() {
    let harness = FakeHarness::new(vec![Behavior::Hold, Behavior::Hold]);
    let fixture = Fixture::new(harness.clone());
    let run = fixture.start();
    fixture.wait_starts(&harness, 1);
    fixture.runs.control(run.id, IssueRunAction::Pause).unwrap();
    assert_eq!(
        fixture.runs.list(None).unwrap()[0].status,
        IssueRunStatus::PauseRequested
    );
    harness.release();
    fixture.wait(IssueRunStatus::Paused);
    assert_eq!(harness.starts.lock().len(), 1);
    // The worker has drained and retires before Resume can start another one.
    fixture.control_when_settled(run.id, IssueRunAction::Resume);
    fixture.wait_starts(&harness, 2);
    assert_eq!(fixture.start().id, run.id);
    assert_eq!(harness.starts.lock().len(), 2);
    fixture
        .runs
        .control(run.id, IssueRunAction::Cancel)
        .unwrap();
    let cancelled = fixture.wait(IssueRunStatus::Cancelled);
    assert_eq!(cancelled.attempts.len(), 2);
    assert!(cancelled.report.is_none());
}

#[test]
fn daemon_blocks_unstructured_success_and_retries_failed_attempt() {
    for behavior in [Behavior::Invalid, Behavior::Fail] {
        let harness = FakeHarness::new(vec![behavior]);
        let fixture = Fixture::new(harness.clone());
        let run = fixture.start();
        let status = if matches!(behavior, Behavior::Invalid) {
            IssueRunStatus::Blocked
        } else {
            IssueRunStatus::Failed
        };
        let failed = fixture.wait(status);
        assert_eq!(failed.attempts.len(), 1);
        assert!(failed.report.is_none());
        fixture.control_when_settled(run.id, IssueRunAction::Retry);
        let done = fixture.wait(IssueRunStatus::Completed);
        assert_eq!(done.attempts.len(), 5);
    }
}

#[test]
fn daemon_permission_blocks_without_retrying_or_auto_approval() {
    let harness = FakeHarness::new(vec![Behavior::Permission]);
    let fixture = Fixture::new(harness.clone());
    let run = fixture.start();
    let blocked = fixture.wait(IssueRunStatus::Blocked);
    assert!(blocked.error.unwrap().contains("Waiting for your response"));
    assert!(fixture.runs.control(run.id, IssueRunAction::Retry).is_err());
    assert_eq!(harness.starts.lock().len(), 1);
    let session_id = blocked.attempts[0].session_id;
    let ResponsePayload::SessionRuntime {
        runtime_id: Some(runtime),
        ..
    } = fixture
        .client
        .request(session_id, Uuid::nil(), Command::AttachSession)
        .unwrap()
    else {
        panic!()
    };
    fixture
        .client
        .request(
            session_id,
            runtime,
            Command::Respond {
                request_id: "approve-fixture".into(),
                option_id: "allow".into(),
                message: None,
            },
        )
        .unwrap();
    fixture.wait(IssueRunStatus::Completed);
}

#[test]
fn daemon_restart_and_reconnect_do_not_replay_reserved_attempts() {
    let harness = FakeHarness::new(vec![Behavior::Hold]);
    let mut fixture = Fixture::new(harness.clone());
    let workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    let request_id = Uuid::new_v4();
    let run = fixture
        .runs
        .start(fixture.task.id, request_id, workflow.clone())
        .unwrap();
    fixture.wait_starts(&harness, 1);
    fixture.stop();
    // Older journals omit newly introduced private bookkeeping fields.
    let path = fixture.root.join("app.issue-runs.json");
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for record in legacy.as_array_mut().unwrap() {
        let record = record.as_object_mut().unwrap();
        record.remove("completion_pending");
        record.remove("request_aliases");
    }
    fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let second = FakeHarness::new(vec![]);
    fixture.restart(second.clone());
    let blocked = fixture.wait(IssueRunStatus::Blocked);
    assert_eq!(blocked.id, run.id);
    assert_eq!(second.starts.lock().len(), 0);
    assert_eq!(
        fixture
            .runs
            .start(fixture.task.id, request_id, workflow)
            .unwrap()
            .id,
        run.id
    );
    assert_eq!(second.starts.lock().len(), 0);
    fixture.runs.control(run.id, IssueRunAction::Retry).unwrap();
    let done = fixture.wait(IssueRunStatus::Completed);
    assert_eq!(done.attempts.len(), 5);
}

#[test]
fn daemon_abrupt_restart_recovers_busy_session_without_replaying_work() {
    let harness = FakeHarness::new(vec![Behavior::Hold]);
    let mut fixture = Fixture::new(harness.clone());
    let initial = fixture.start();
    fixture.wait_starts(&harness, 1);
    fixture.stop();
    // Reconstruct the last durable state of an abruptly terminated daemon:
    // journal reservation and busy turn exist, with no completion evidence.
    let path = fixture.root.join("app.issue-runs.json");
    let mut records: Vec<Record> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    records[0].run.status = IssueRunStatus::Running;
    records[0].run.attempts[0].status = IssueAttemptStatus::Running;
    let session_id = records[0].run.attempts[0].session_id;
    fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();
    let store = StateStore::daemon(fixture.root.join("app.db"));
    let mut state = store.load().unwrap();
    let session = state.session_mut(session_id).unwrap();
    store.hydrate(session).unwrap();
    session.status = SessionStatus::Working;
    session.turns[0].status = TurnStatus::Running;
    session.turns[0].completed_at = None;
    store.save(&mut state).unwrap();
    drop(store);
    let replacement = FakeHarness::new(vec![]);
    fixture.restart(replacement.clone());
    let blocked = fixture.wait(IssueRunStatus::Blocked);
    assert_eq!(blocked.id, initial.id);
    assert_eq!(blocked.attempts[0].status, IssueAttemptStatus::Interrupted);
    assert!(replacement.starts.lock().is_empty());
    let ResponsePayload::Session {
        session: Some(session),
    } = fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::HydrateSession { session_id },
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(session.turns[0].status, TurnStatus::Interrupted);
}

#[test]
fn result_contract_rejects_stale_ids_review_prose_and_missing_evidence() {
    let run_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    let mut outcome = IssueStageOutcome {
        run_id,
        attempt_id,
        decision: IssueStageDecision::Approved,
        summary: "Reviewed".into(),
        evidence: vec!["Tests passed".into()],
    };
    let encoded = |outcome: &IssueStageOutcome| {
        format!(
            "{RESULT_MARKER}{}\n```",
            serde_json::to_string(outcome).unwrap()
        )
    };
    assert!(
        parse_outcome(
            &encoded(&outcome),
            None,
            run_id,
            attempt_id,
            IssueStageRole::Review
        )
        .is_ok()
    );
    assert!(
        parse_outcome(
            &encoded(&outcome),
            None,
            Uuid::new_v4(),
            attempt_id,
            IssueStageRole::Review
        )
        .is_err()
    );
    outcome.decision = IssueStageDecision::Completed;
    assert!(
        parse_outcome(
            &encoded(&outcome),
            None,
            run_id,
            attempt_id,
            IssueStageRole::Review
        )
        .is_err()
    );
    outcome.evidence.clear();
    assert!(
        parse_outcome(
            &encoded(&outcome),
            None,
            run_id,
            attempt_id,
            IssueStageRole::Execution
        )
        .is_err()
    );
}

#[test]
fn daemon_unsupported_harness_blocks_without_starting_a_provider() {
    let harness = FakeHarness::new(vec![]);
    let fixture = Fixture::new(harness.clone());
    let workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::DeepSeek);
    fixture
        .runs
        .start(fixture.task.id, Uuid::new_v4(), workflow)
        .unwrap();
    let run = fixture.wait(IssueRunStatus::Blocked);
    assert!(run.error.unwrap().contains("Unsupported fixture harness"));
    assert!(run.attempts.is_empty());
    assert!(harness.starts.lock().is_empty());
}

#[test]
fn daemon_concurrent_starts_and_late_request_retries_share_one_run() {
    let harness = FakeHarness::new(vec![Behavior::Hold]);
    let fixture = Fixture::new(harness.clone());
    let workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    let requests = [Uuid::new_v4(), Uuid::new_v4()];
    let workers = requests
        .into_iter()
        .map(|request_id| {
            let client = fixture.runs.clone();
            let workflow = workflow.clone();
            let task_id = fixture.task.id;
            std::thread::spawn(move || client.start(task_id, request_id, workflow).unwrap())
        })
        .collect::<Vec<_>>();
    let runs = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(runs[0].id, runs[1].id);
    fixture.wait_starts(&harness, 1);
    harness.release();
    fixture.wait(IssueRunStatus::Completed);
    for request_id in requests {
        assert_eq!(
            fixture
                .runs
                .start(fixture.task.id, request_id, workflow.clone())
                .unwrap()
                .id,
            runs[0].id
        );
    }
    assert_eq!(harness.starts.lock().len(), 4);
    let mut conflicting = workflow;
    conflicting.max_review_retries = 0;
    assert!(
        fixture
            .runs
            .start(fixture.task.id, requests[0], conflicting)
            .is_err()
    );
}

#[test]
fn daemon_terminal_mapping_archives_only_after_durable_report_and_preserves_evidence() {
    let harness = FakeHarness::new(vec![]);
    let fixture = Fixture::new(harness);
    let project_id = fixture.task.project_id().unwrap();
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    workflow.stages.last_mut().unwrap().board_stage_id = Some(crate::model::canonical_stage_id(
        project_id,
        TaskState::Done,
    ));
    fixture
        .runs
        .start(fixture.task.id, Uuid::new_v4(), workflow)
        .unwrap();
    let run = fixture.wait(IssueRunStatus::Completed);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (sessions, tasks) = loop {
        let ResponsePayload::TaskState {
            sessions, tasks, ..
        } = fixture
            .client
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!()
        };
        if tasks[0].state == TaskState::Done {
            break (sessions, tasks);
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(run.report.is_some());
    assert!(tasks[0].completed_at.is_some());
    let session_id = sessions[0].id;
    fixture
        .client
        .request(session_id, Uuid::new_v4(), Command::CloseSessionAndWait)
        .unwrap();
    // Even a newer stale client projection cannot erase automatic evidence.
    let mut stale = sessions[0].clone();
    stale.updated_at = u64::MAX;
    stale.title = "stale projection".into();
    stale.messages.clear();
    stale.turns.clear();
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![],
                tasks: vec![],
                sessions: vec![stale],
                live_session_ids: vec![],
                skeleton_session_ids: vec![],
            },
        )
        .unwrap();
    let ResponsePayload::Session {
        session: Some(session),
    } = fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::HydrateSession { session_id },
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.messages.len(), 2);
}

#[test]
fn daemon_corrupt_run_journal_disables_automation_without_breaking_manual_state() {
    let harness = FakeHarness::new(vec![]);
    let mut fixture = Fixture::new(harness.clone());
    fixture.stop();
    let path = fixture.root.join("app.issue-runs.json");
    fs::write(&path, b"broken journal").unwrap();
    let (client, server) = Fixture::connect(&fixture.root, harness);
    fixture.runs = IssueRunClient::new(client.clone());
    fixture.client = client;
    fixture.server = Some(server);
    assert!(
        fixture
            .runs
            .list(None)
            .unwrap_err()
            .to_string()
            .contains("journal is invalid")
    );
    assert!(matches!(
        fixture
            .client
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap(),
        ResponsePayload::TaskState { .. }
    ));
    assert_eq!(fs::read(path).unwrap(), b"broken journal");
}

#[test]
fn result_contract_rejects_trailing_text_unknown_fields_and_oversized_results() {
    let run_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    let outcome = IssueStageOutcome {
        run_id,
        attempt_id,
        decision: IssueStageDecision::Approved,
        summary: "Reviewed".into(),
        evidence: vec!["Tests passed".into()],
    };
    let text = format!(
        "{RESULT_MARKER}{}\n```",
        serde_json::to_string(&outcome).unwrap()
    );
    assert!(
        parse_outcome(
            &(text.clone() + " trailing prose"),
            None,
            run_id,
            attempt_id,
            IssueStageRole::Review
        )
        .is_err()
    );
    assert!(parse_outcome(&text, None, run_id, Uuid::new_v4(), IssueStageRole::Review).is_err());
    assert!(parse_outcome(&text, None, run_id, attempt_id, IssueStageRole::Execution).is_err());
    let mut value = serde_json::to_value(&outcome).unwrap();
    value["unexpected"] = true.into();
    let invalid = format!("{RESULT_MARKER}{value}\n```");
    assert!(parse_outcome(&invalid, None, run_id, attempt_id, IssueStageRole::Review).is_err());
    assert!(
        parse_outcome(
            &"x".repeat(MAX_PUBLIC_OUTPUT + 1),
            None,
            run_id,
            attempt_id,
            IssueStageRole::Review
        )
        .is_err()
    );
}

#[test]
fn daemon_active_run_refuses_prior_manual_rewind_and_workspace_mutations() {
    use crate::workspace::{WorkspaceOperation, WorkspaceResult};

    let harness = FakeHarness::new(vec![Behavior::Hold]);
    let fixture = Fixture::new(harness.clone());
    let cwd = fixture.root.join("repo");
    fs::write(cwd.join("tracked.txt"), "automatic stage owns these bytes").unwrap();
    let mut manual = AgentSession::new(fixture.task.project_id().unwrap(), ProviderKind::Codex);
    manual.task_id = Some(fixture.task.id);
    manual.begin_turn("old manual work");
    manual.finish_active_turn(TurnStatus::Completed);
    manual.status = SessionStatus::Idle;
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![],
                tasks: vec![],
                sessions: vec![manual.clone()],
                live_session_ids: vec![],
                skeleton_session_ids: vec![],
            },
        )
        .unwrap();
    let run = fixture.start();
    fixture.wait_starts(&harness, 1);
    for command in [
        Command::RewindSessionToMessage { turn_count: 1 },
        Command::ForkSessionFromResponse { turn_count: 1 },
    ] {
        let error = fixture
            .client
            .request(manual.id, Uuid::nil(), command)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unfinished automatic run"), "{error}");
    }
    // Path-only commands cannot evade the lease with nil or unrelated IDs.
    for operation in [
        WorkspaceOperation::WriteTextFile {
            root: cwd.clone(),
            relative_path: "tracked.txt".into(),
            content: "overwrite".into(),
        },
        WorkspaceOperation::RestoreRef {
            cwd: cwd.clone(),
            git_ref: "refs/waku/old".into(),
        },
        WorkspaceOperation::CheckoutBranch {
            cwd: cwd.clone(),
            branch: "old".into(),
            create: false,
        },
        WorkspaceOperation::DeleteSessionRefs {
            cwd: cwd.clone(),
            session_id: manual.id,
        },
        WorkspaceOperation::CaptureTurn {
            cwd: cwd.clone(),
            session_id: manual.id,
            turn_count: 1,
        },
    ] {
        let error = fixture
            .client
            .request(Uuid::nil(), Uuid::nil(), Command::Workspace { operation })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("automatic run owns this workspace"),
            "{error}"
        );
    }
    // Reads and unrelated writes remain usable while the provider is held.
    let result = fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::Workspace {
                operation: WorkspaceOperation::ReadTextFile {
                    root: cwd.clone(),
                    relative_path: "tracked.txt".into(),
                },
            },
        )
        .unwrap();
    assert!(
        matches!(result, ResponsePayload::Workspace { result: WorkspaceResult::TextFile { content } } if content == "automatic stage owns these bytes")
    );
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::Workspace {
                operation: WorkspaceOperation::WriteTextFile {
                    root: outside.clone(),
                    relative_path: "allowed.txt".into(),
                    content: "allowed".into(),
                },
            },
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(outside.join("allowed.txt")).unwrap(),
        "allowed"
    );
    assert_eq!(
        fs::read_to_string(cwd.join("tracked.txt")).unwrap(),
        "automatic stage owns these bytes"
    );
    let ResponsePayload::Session {
        session: Some(restored),
    } = fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::HydrateSession {
                session_id: manual.id,
            },
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(restored.turns.len(), 1);
    assert_eq!(restored.messages.len(), manual.messages.len());
    assert_eq!(restored.messages[0].content, manual.messages[0].content);
    fixture
        .runs
        .control(run.id, IssueRunAction::Cancel)
        .unwrap();
    fixture.wait(IssueRunStatus::Cancelled);
}

#[test]
fn daemon_two_issues_cannot_start_in_same_checkout_but_distinct_workspaces_can() {
    let harness = FakeHarness::new(vec![Behavior::Hold, Behavior::Hold]);
    let fixture = Fixture::new(harness.clone());
    let first = fixture.start();
    fixture.wait_starts(&harness, 1);
    let mut other = Task::new("Another issue", TaskState::Backlog, 1);
    other.workspace = fixture.task.workspace.clone();
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![],
                tasks: vec![other.clone()],
                sessions: vec![],
                live_session_ids: vec![],
                skeleton_session_ids: vec![],
            },
        )
        .unwrap();
    let error = fixture
        .runs
        .start(
            other.id,
            Uuid::new_v4(),
            IssueWorkflow::plan_execute_review_report(ProviderKind::Codex),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("automatic run owns this workspace"),
        "{error}"
    );
    assert_eq!(harness.starts.lock().len(), 1);
    let separate = fixture.root.join("other-worktree");
    fs::create_dir_all(&separate).unwrap();
    other.workspace = Some(TaskWorkspace::Worktree {
        project_id: fixture.task.project_id().unwrap(),
        path: separate.clone(),
        branch: "fixture".into(),
    });
    other.touch();
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![],
                tasks: vec![other.clone()],
                sessions: vec![],
                live_session_ids: vec![],
                skeleton_session_ids: vec![],
            },
        )
        .unwrap();
    let second = fixture
        .runs
        .start(
            other.id,
            Uuid::new_v4(),
            IssueWorkflow::plan_execute_review_report(ProviderKind::Codex),
        )
        .unwrap();
    fixture.wait_starts(&harness, 2);
    assert_eq!(harness.starts.lock()[1].2, separate);
    fixture
        .runs
        .control(first.id, IssueRunAction::Cancel)
        .unwrap();
    fixture
        .runs
        .control(second.id, IssueRunAction::Cancel)
        .unwrap();
    fixture.wait(IssueRunStatus::Cancelled);
}

#[test]
fn workspace_guard_retains_reservation_and_resolves_aliases_and_ancestors() {
    let root = std::env::temp_dir().join(format!("doki-workspace-guard-{}", Uuid::new_v4()));
    fs::create_dir_all(root.join("repo/subproject")).unwrap();
    let records = Mutex::new(Vec::<Record>::new());
    let guard = IssueMutationGuard {
        records: records.lock(),
    };
    guard.ensure_workspace_mutation_allowed(&root).unwrap();
    assert!(
        records.try_lock().is_none(),
        "reservation remains locked for the caller's whole mutation"
    );
    drop(guard);
    assert!(records.try_lock().is_some());
    let repo = canonical_mutation_path(&root.join("repo")).unwrap();
    let child = canonical_mutation_path(&root.join("repo/subproject/new-file")).unwrap();
    assert!(paths_overlap(&repo, &child));
    assert!(!paths_overlap(
        &repo,
        &canonical_mutation_path(&root.join("sibling")).unwrap()
    ));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("repo"), root.join("alias")).unwrap();
        assert_eq!(
            canonical_mutation_path(&root.join("alias/subproject/new-file")).unwrap(),
            child
        );
    }
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(root.join("repo"))
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("repo/sibling")).unwrap();
    assert_eq!(
        workspace_lease_root(&root.join("repo/subproject")).unwrap(),
        workspace_lease_root(&root.join("repo/sibling")).unwrap()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn daemon_provider_error_without_terminal_event_is_visible_and_still_owned() {
    let harness = FakeHarness::new(vec![Behavior::ErrorHold]);
    let fixture = Fixture::new(harness.clone());
    let initial = fixture.start();
    let blocked = fixture.wait(IssueRunStatus::Blocked);
    assert!(
        blocked
            .error
            .as_deref()
            .unwrap()
            .contains("Fixture provider error")
    );
    assert!(blocked.error.as_deref().unwrap().contains("still active"));
    assert_eq!(blocked.attempts[0].status, IssueAttemptStatus::Running);
    assert!(blocked.attempts[0].finished_at.is_none());
    assert!(
        fixture
            .runs
            .control(initial.id, IssueRunAction::Retry)
            .unwrap_err()
            .to_string()
            .contains("still active")
    );
    assert_eq!(harness.starts.lock().len(), 1);
    // An actual successful terminal result, not the error text, may settle it.
    harness.release();
    let completed = fixture.wait(IssueRunStatus::Completed);
    assert_eq!(completed.attempts.len(), 4);
    assert!(completed.error.is_none());
}

#[test]
fn daemon_restart_reconciles_only_pending_terminal_mapping_without_replaying_report() {
    let harness = FakeHarness::new(vec![]);
    let mut fixture = Fixture::new(harness.clone());
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    let stage_id =
        crate::model::canonical_stage_id(fixture.task.project_id().unwrap(), TaskState::Done);
    workflow.stages.last_mut().unwrap().board_stage_id = Some(stage_id);
    fixture
        .runs
        .start(fixture.task.id, Uuid::new_v4(), workflow)
        .unwrap();
    let completed = fixture.wait(IssueRunStatus::Completed);
    fixture.stop();
    let journal = fixture.root.join("app.issue-runs.json");
    let mut records: Vec<Record> = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    // Exact crash boundary: report committed, mapping not yet written to DB.
    records[0].completion_pending = true;
    fs::write(&journal, serde_json::to_vec(&records).unwrap()).unwrap();
    let store = StateStore::daemon(fixture.root.join("app.db"));
    let mut state = store.load().unwrap();
    state.tasks[0].set_state(TaskState::Doing);
    store.save(&mut state).unwrap();
    drop(store);
    let replacement = FakeHarness::new(vec![]);
    fixture.restart(replacement.clone());
    let ResponsePayload::TaskState { tasks, .. } = fixture
        .client
        .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(tasks[0].state, TaskState::Done);
    assert_eq!(tasks[0].stage_id, Some(stage_id));
    let recovered = fixture.wait(IssueRunStatus::Completed);
    assert_eq!(recovered.report, completed.report);
    assert_eq!(recovered.attempts.len(), 4);
    assert!(replacement.starts.lock().is_empty());
    let records: Vec<Record> = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    assert!(!records[0].completion_pending);
    // Once finalized, a user's explicit reopen must survive later restarts.
    let mut reopened = tasks[0].clone();
    reopened.set_state(TaskState::Doing);
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![],
                tasks: vec![reopened],
                sessions: vec![],
                live_session_ids: vec![],
                skeleton_session_ids: vec![],
            },
        )
        .unwrap();
    fixture.restart(replacement.clone());
    let ResponsePayload::TaskState { tasks, .. } = fixture
        .client
        .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(tasks[0].state, TaskState::Doing);
    assert!(replacement.starts.lock().is_empty());
}

#[test]
fn daemon_invalid_pending_terminal_mapping_releases_manual_recovery() {
    let harness = FakeHarness::new(vec![]);
    let mut fixture = Fixture::new(harness);
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    workflow.stages.last_mut().unwrap().board_stage_id = Some(crate::model::canonical_stage_id(
        fixture.task.project_id().unwrap(),
        TaskState::Done,
    ));
    fixture
        .runs
        .start(fixture.task.id, Uuid::new_v4(), workflow)
        .unwrap();
    fixture.wait(IssueRunStatus::Completed);
    fixture.stop();
    let journal = fixture.root.join("app.issue-runs.json");
    let mut records: Vec<Record> = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    records[0].completion_pending = true;
    records[0].completion_stage.as_mut().unwrap().id = Uuid::new_v4();
    fs::write(&journal, serde_json::to_vec(&records).unwrap()).unwrap();
    let store = StateStore::daemon(fixture.root.join("app.db"));
    let mut state = store.load().unwrap();
    state.tasks[0].set_state(TaskState::Doing);
    store.save(&mut state).unwrap();
    drop(store);
    let replacement = FakeHarness::new(vec![Behavior::Hold]);
    fixture.restart(replacement.clone());
    let completed = fixture.wait(IssueRunStatus::Completed);
    assert!(
        completed
            .error
            .as_deref()
            .unwrap()
            .contains("move the issue manually")
    );
    assert!(replacement.starts.lock().is_empty());
    let records: Vec<Record> = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    assert!(!records[0].completion_pending);
    let ResponsePayload::TaskState { mut tasks, .. } = fixture
        .client
        .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
        .unwrap()
    else {
        panic!()
    };
    tasks[0].set_state(TaskState::Done);
    fixture
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![],
                tasks,
                sessions: vec![],
                live_session_ids: vec![],
                skeleton_session_ids: vec![],
            },
        )
        .unwrap();
    let fresh = fixture.start();
    assert_ne!(fresh.id, completed.id);
    fixture.wait_starts(&replacement, 1);
    fixture
        .runs
        .control(fresh.id, IssueRunAction::Cancel)
        .unwrap();
    fixture.wait(IssueRunStatus::Cancelled);
}

#[test]
fn workspace_leases_keep_physically_nested_git_worktrees_independent() {
    let root = std::env::temp_dir().join(format!("doki-nested-worktree-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let nested = repo.join(".worktrees/nested");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "fixture",
    ]);
    let output = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["worktree", "add", "--quiet", "-b", "nested"])
        .arg(&nested)
        .arg("HEAD")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::create_dir_all(repo.join("subproject")).unwrap();
    fs::create_dir_all(nested.join("subproject")).unwrap();
    let parent = workspace_lease_root(&repo.join("subproject")).unwrap();
    let child = workspace_lease_root(&nested.join("subproject/new-file")).unwrap();
    assert!(parent.git && child.git);
    assert!(!parent.overlaps(&child));
    assert!(!child.overlaps(&parent));
    // A non-Git parent operation can still touch both contained workspaces.
    assert!(workspace_lease_root(&root).unwrap().overlaps(&child));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn daemon_client_checkpoint_callbacks_for_automatic_attempts_are_read_only() {
    use crate::workspace::{WorkspaceOperation, WorkspaceResult};
    let harness = FakeHarness::new(vec![Behavior::Complete, Behavior::Hold]);
    let fixture = Fixture::new(harness.clone());
    let initial = fixture.start();
    fixture.wait_starts(&harness, 2);
    let run = fixture.runs.list(Some(fixture.task.id)).unwrap().remove(0);
    assert_eq!(run.attempts.len(), 2);
    assert_eq!(run.attempts[0].status, IssueAttemptStatus::Completed);
    assert_eq!(run.attempts[1].status, IssueAttemptStatus::Running);
    for attempt in &run.attempts {
        // This is the callback emitted by the normal native TurnFinished path;
        // it can arrive after a later stage starts in the same checkout.
        let response = fixture
            .client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::Workspace {
                    operation: WorkspaceOperation::CaptureTurn {
                        cwd: fixture.root.join("repo"),
                        session_id: attempt.session_id,
                        turn_count: 1,
                    },
                },
            )
            .unwrap();
        assert!(matches!(response, ResponsePayload::Workspace {
            result: WorkspaceResult::Checkpoint { checkpoint }
        } if checkpoint.status == crate::model::CheckpointStatus::Unavailable && checkpoint.turn_count == 1));
    }
    assert!(!fixture.root.join("repo/.git").exists());
    assert_eq!(harness.starts.lock().len(), 2);
    fixture
        .runs
        .control(initial.id, IssueRunAction::Cancel)
        .unwrap();
    fixture.wait(IssueRunStatus::Cancelled);
}
