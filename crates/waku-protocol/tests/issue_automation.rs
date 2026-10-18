use serde_json::json;
use uuid::Uuid;
use waku_protocol::model::ProviderKind;
use waku_protocol::{
    Command, IssueRunAction, IssueRunStatus, IssueStageDecision, IssueStageOutcome, IssueStageRole,
    IssueWorkflow,
};

#[test]
fn preset_has_configurable_harnesses_and_no_implicit_board_transition() {
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    assert_eq!(workflow.max_review_retries, 2);
    assert_eq!(
        workflow
            .stages
            .iter()
            .map(|stage| stage.role)
            .collect::<Vec<_>>(),
        [
            IssueStageRole::Plan,
            IssueStageRole::Execution,
            IssueStageRole::Review,
            IssueStageRole::Report,
        ]
    );
    assert!(
        workflow
            .stages
            .iter()
            .all(|stage| stage.board_stage_id.is_none())
    );
    workflow.stages[1].provider = ProviderKind::Claude;
    workflow.stages[2].provider = ProviderKind::Pi;
    workflow.stages[2].model = Some("independent-review-model".into());
    workflow.stages[2].instructions = "Review the acceptance criteria".into();
    workflow.validate().unwrap();
    assert_eq!(
        serde_json::from_value::<IssueWorkflow>(serde_json::to_value(&workflow).unwrap()).unwrap(),
        workflow
    );
}

#[test]
fn generic_sequence_accepts_custom_stages_and_requires_valid_review_order() {
    let mut workflow = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    let mut custom = workflow.stages[0].clone();
    custom.id = Uuid::new_v4();
    custom.role = IssueStageRole::Custom;
    custom.name = "Security evidence".into();
    workflow.stages.insert(2, custom);
    workflow.validate().unwrap();
    workflow.stages.swap(0, 3);
    assert!(
        workflow
            .validate()
            .unwrap_err()
            .contains("earlier execution")
    );

    let mut report_only = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    report_only.stages.drain(..3);
    report_only.validate().unwrap();
    report_only.stages[0].role = IssueStageRole::Custom;
    assert!(
        report_only
            .validate()
            .unwrap_err()
            .contains("final workflow stage")
    );
}

#[test]
fn workflow_limits_bound_input_and_retry_work() {
    let valid = IssueWorkflow::plan_execute_review_report(ProviderKind::Codex);
    let mut workflow = valid.clone();
    workflow.max_review_retries = 11;
    assert!(workflow.validate().is_err());
    workflow = valid.clone();
    workflow.stages[1].id = workflow.stages[0].id;
    assert!(workflow.validate().is_err());
    workflow = valid.clone();
    workflow.stages[0].name = " ".into();
    assert!(workflow.validate().is_err());
    workflow = valid.clone();
    workflow.stages[0].name = "x".repeat(257);
    assert!(workflow.validate().is_err());
    workflow = valid.clone();
    workflow.stages[0].instructions = "x".repeat(16_385);
    assert!(workflow.validate().is_err());
    workflow = valid.clone();
    workflow.stages[0].model = Some("x".repeat(513));
    assert!(workflow.validate().is_err());
    workflow = valid.clone();
    workflow.stages[0].model = Some(" ".into());
    assert!(workflow.validate().is_err());
    workflow.stages.clear();
    assert!(workflow.validate().is_err());
    workflow.stages = (0..33)
        .map(|_| {
            let mut stage = valid.stages.last().unwrap().clone();
            stage.id = Uuid::new_v4();
            stage
        })
        .collect();
    assert!(workflow.validate().is_err());
}

#[test]
fn commands_roundtrip_with_separate_durable_start_intent() {
    let task_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let command = Command::StartIssueRun {
        task_id,
        request_id,
        workflow: IssueWorkflow::plan_execute_review_report(ProviderKind::Codex),
    };
    let wire = serde_json::to_value(command).unwrap();
    assert_eq!(wire["type"], "startIssueRun");
    assert_eq!(wire["taskId"], task_id.to_string());
    assert_eq!(wire["requestId"], request_id.to_string());
    let decoded: Command = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    for action in [
        IssueRunAction::Pause,
        IssueRunAction::Resume,
        IssueRunAction::Cancel,
        IssueRunAction::Retry,
    ] {
        let wire = serde_json::to_value(Command::ControlIssueRun {
            run_id: task_id,
            action,
        })
        .unwrap();
        let decoded: Command = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }
    let wire = serde_json::to_value(Command::ListIssueRuns { task_id: None }).unwrap();
    assert_eq!(wire, json!({"type": "listIssueRuns", "taskId": null}));
}

#[test]
fn pending_controls_and_attention_states_are_not_terminal() {
    for status in [
        IssueRunStatus::Running,
        IssueRunStatus::PauseRequested,
        IssueRunStatus::CancelRequested,
        IssueRunStatus::Paused,
        IssueRunStatus::Blocked,
        IssueRunStatus::Failed,
    ] {
        assert!(!status.is_terminal());
        assert_eq!(
            serde_json::from_value::<IssueRunStatus>(serde_json::to_value(status).unwrap())
                .unwrap(),
            status
        );
    }
    assert!(IssueRunStatus::Completed.is_terminal());
    assert!(IssueRunStatus::Cancelled.is_terminal());
    assert!(IssueRunStatus::CancelRequested.is_active());
    assert!(!IssueRunStatus::Blocked.is_active());
}

#[test]
fn structured_outcome_requires_evidence_fields_and_rejects_extra_approval_flags() {
    let outcome = IssueStageOutcome {
        run_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
        decision: IssueStageDecision::Approved,
        summary: "Reviewed the delivered change".into(),
        evidence: vec!["src/example.rs; provider-free test passed".into()],
    };
    let wire = serde_json::to_value(&outcome).unwrap();
    assert_eq!(
        serde_json::from_value::<IssueStageOutcome>(wire.clone()).unwrap(),
        outcome
    );
    let mut extra = wire.clone();
    extra["approved"] = json!(true);
    assert!(serde_json::from_value::<IssueStageOutcome>(extra).is_err());
    let mut missing = wire;
    missing.as_object_mut().unwrap().remove("evidence");
    assert!(serde_json::from_value::<IssueStageOutcome>(missing).is_err());
}
