//! Pure, bounded semantic recording and replay preparation. Runtime ownership,
//! executor dispatch and asynchronous wait completion belong to automation.rs.
//! No observations, typed values, transport IDs or default secrets are kept.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;
use uuid::Uuid;
use waku_protocol::*;

type Result<T> = std::result::Result<T, AutomationError>;
const MAX_NAME_BYTES: usize = 256;
const MAX_POINTER_BYTES: usize = 1024;
const MAX_BINDINGS: usize = 256;
const MAX_WAIT_MS: u64 = 5 * 60 * 1000;

/// Opaque completion correlation: stale or mismatched completions never append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordingTicket(Uuid);

struct Pending {
    ticket: RecordingTicket,
    step: AutomationReplayStep,
    captures_opened: bool,
}

pub(crate) struct Recorder {
    replay: AutomationReplay,
    state: AutomationRecordingStatus,
    pending: Option<Pending>,
    error: Option<AutomationError>,
    closed: bool,
    next_text: usize,
    next_id: usize,
    opened: HashMap<Uuid, usize>,
    seed_parameters: HashMap<Uuid, String>,
}

impl Recorder {
    pub(crate) fn new(name: String) -> Result<Self> {
        valid_name(&name)?;
        Ok(Self {
            replay: AutomationReplay {
                format_version: AUTOMATION_REPLAY_FORMAT_VERSION,
                name,
                steps: vec![],
            },
            state: AutomationRecordingStatus::Recording,
            pending: None,
            error: None,
            closed: false,
            next_text: 0,
            next_id: 0,
            opened: HashMap::new(),
            seed_parameters: HashMap::new(),
        })
    }

    /// Call before dispatching an actual top-level call or wait. An error means
    /// the caller must not dispatch it while this recording owns the session.
    pub(crate) fn begin(&mut self, operation: &AutomationStepOperation) -> Result<RecordingTicket> {
        if self.pending.is_some() {
            return Err(error(
                "recording_busy",
                "a recorded operation is still in flight",
            ));
        }
        if self.state != AutomationRecordingStatus::Recording {
            return Err(error(
                "recording_closed",
                "this recording no longer accepts operations",
            ));
        }
        let prepared = self.prepare(operation);
        let step = match prepared {
            Ok(step) => step,
            Err(failure) => {
                self.fail(failure.clone());
                return Err(failure);
            }
        };
        let ticket = RecordingTicket(Uuid::new_v4());
        self.pending = Some(Pending {
            ticket,
            captures_opened: is_new_session(operation),
            step,
        });
        Ok(ticket)
    }

    fn prepare(&mut self, operation: &AutomationStepOperation) -> Result<AutomationReplayStep> {
        validate_operation(operation)?;
        if self.replay.steps.len() >= MAX_AUTOMATION_STEPS {
            return Err(error("recording_limit", "recording exceeds 128 steps"));
        }
        let mut operation_json = serde_json::to_value(operation).map_err(serialization_error)?;
        let mut bindings = vec![];
        match operation {
            AutomationStepOperation::Call { call } => match call {
                AutomationCall::Type { .. } => {
                    self.bind_text(&mut operation_json, "/call/text", &mut bindings)?;
                }
                AutomationCall::Invoke { verb, args } => {
                    for field in ["id", "taskId", "projectId"] {
                        if let Some(value) = args.get(field) {
                            self.bind_uuid(
                                &mut operation_json,
                                &format!("/call/args/{field}"),
                                value,
                                &mut bindings,
                            )?;
                        }
                    }
                    for field in ["text", "title"] {
                        if args.get(field).is_some() {
                            self.bind_text(
                                &mut operation_json,
                                &format!("/call/args/{field}"),
                                &mut bindings,
                            )?;
                        }
                    }
                    debug_assert!(verb != "submitPrompt");
                }
                _ => {}
            },
            AutomationStepOperation::Wait { condition, .. } => match condition {
                AutomationCondition::Text { .. } | AutomationCondition::StateContains { .. } => {
                    self.bind_text(&mut operation_json, "/condition/contains", &mut bindings)?;
                }
                AutomationCondition::State { equals, .. } => {
                    self.bind_wait_value(
                        &mut operation_json,
                        "/condition/equals",
                        equals,
                        &mut bindings,
                    )?;
                }
                _ => {}
            },
        }
        let operation = serde_json::from_value(operation_json).map_err(serialization_error)?;
        let step = AutomationReplayStep {
            step: AutomationStep {
                name: format!("step{}", self.replay.steps.len() + 1),
                operation,
            },
            bindings,
        };
        let mut candidate = self.replay.clone();
        candidate.steps.push(step.clone());
        validate_step(&candidate, candidate.steps.len() - 1)?;
        check_size(&candidate)?;
        Ok(step)
    }

    fn bind_text(
        &mut self,
        operation: &mut Value,
        pointer: &str,
        bindings: &mut Vec<AutomationBinding>,
    ) -> Result<()> {
        self.next_text += 1;
        replace(operation, pointer, Value::String(String::new()))?;
        bindings.push(AutomationBinding {
            pointer: pointer.into(),
            source: AutomationBindingSource::Parameter {
                name: format!("text{}", self.next_text),
            },
        });
        Ok(())
    }

    fn bind_uuid(
        &mut self,
        operation: &mut Value,
        pointer: &str,
        value: &Value,
        bindings: &mut Vec<AutomationBinding>,
    ) -> Result<()> {
        let id = value
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| error("unrecordable", "identity arguments must be UUID strings"))?;
        let source = if let Some(step) = self.opened.get(&id) {
            AutomationBindingSource::StepResult {
                step: *step,
                pointer: "/value/opened".into(),
            }
        } else {
            let name = self
                .seed_parameters
                .entry(id)
                .or_insert_with(|| {
                    self.next_id += 1;
                    format!("id{}", self.next_id)
                })
                .clone();
            AutomationBindingSource::Parameter { name }
        };
        // A valid typed placeholder permits complete preflight validation while
        // never treating the original instance's UUID as a portable identity.
        replace(operation, pointer, Value::String(Uuid::nil().to_string()))?;
        bindings.push(AutomationBinding {
            pointer: pointer.into(),
            source,
        });
        Ok(())
    }

    fn bind_wait_value(
        &mut self,
        operation: &mut Value,
        pointer: &str,
        value: &Value,
        bindings: &mut Vec<AutomationBinding>,
    ) -> Result<()> {
        match value {
            Value::String(text) if Uuid::parse_str(text).is_ok() => {
                self.bind_uuid(operation, pointer, value, bindings)
            }
            Value::String(_) => self.bind_text(operation, pointer, bindings),
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    self.bind_wait_value(
                        operation,
                        &format!("{pointer}/{index}"),
                        value,
                        bindings,
                    )?;
                }
                Ok(())
            }
            Value::Object(values) => {
                for (key, value) in values {
                    self.bind_wait_value(
                        operation,
                        &format!("{pointer}/{}", escape_pointer(key)),
                        value,
                        bindings,
                    )?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Result values must use the existing scenario result shape. For calls,
    /// pass at least {"value": call_result.execution.value}; waits may use null.
    /// Values and errors are inspected synchronously, never retained verbatim.
    pub(crate) fn complete(
        &mut self,
        ticket: RecordingTicket,
        outcome: Result<Value>,
    ) -> Result<()> {
        if self.pending.as_ref().map(|pending| pending.ticket) != Some(ticket) {
            return Err(error(
                "recording_ticket",
                "recorded completion does not match the in-flight operation",
            ));
        }
        let pending = self.pending.take().expect("ticket checked");
        match outcome {
            Ok(result) => {
                if pending.captures_opened {
                    let Some(opened) = result
                        .pointer("/value/opened")
                        .and_then(Value::as_str)
                        .and_then(|s| Uuid::parse_str(s).ok())
                    else {
                        let failure = error(
                            "recording_result",
                            "newSession completed without a usable opened UUID",
                        );
                        self.fail(failure.clone());
                        return Err(failure);
                    };
                    self.opened.insert(opened, self.replay.steps.len());
                }
                self.replay.steps.push(pending.step);
                Ok(())
            }
            Err(failure) => {
                let failure = sanitized_failure(&failure, self.replay.steps.len());
                self.fail(failure.clone());
                Err(failure)
            }
        }
    }

    fn fail(&mut self, failure: AutomationError) {
        self.state = AutomationRecordingStatus::Failed;
        self.error = Some(failure);
    }

    /// Failed recordings block further effects until explicitly stopped or cancelled.
    pub(crate) fn blocks_session(&self) -> bool {
        !self.closed
    }

    pub(crate) fn status(&self) -> AutomationRecording {
        let mut replay = self.replay.clone();
        if let Some(pending) = &self.pending {
            replay.steps.push(pending.step.clone());
        }
        AutomationRecording {
            status: self.state,
            completed_steps: self.replay.steps.len(),
            pending: self.pending.is_some(),
            required_inputs: required_inputs(&replay).unwrap_or_default(),
            error: self.error.clone(),
        }
    }

    /// Export only successful completed operations. Failure state remains
    /// visible after export; stopping does not turn an incomplete run into a pass.
    pub(crate) fn stop(&mut self) -> Result<AutomationReplay> {
        if self.pending.is_some() {
            return Err(error(
                "recording_busy",
                "finish the in-flight operation before stopping",
            ));
        }
        if self.state == AutomationRecordingStatus::Cancelled {
            return Err(error("recording_cancelled", "the recording was cancelled"));
        }
        if self.state == AutomationRecordingStatus::Recording {
            self.state = AutomationRecordingStatus::Stopped;
        }
        self.closed = true;
        Ok(self.replay.clone())
    }

    /// Discard recorded steps and prevent late completions from resurrecting them.
    pub(crate) fn cancel(&mut self) {
        self.state = AutomationRecordingStatus::Cancelled;
        self.closed = true;
        self.pending = None;
        self.replay.steps.clear();
        self.opened.clear();
        self.seed_parameters.clear();
        self.error = None;
    }
}

/// Validate the entire document and every supplied parameter before any replay
/// side effect. Result bindings are resolved immediately before their step.
pub(crate) fn preflight(
    replay: &AutomationReplay,
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<AutomationReplayInput>> {
    validate_replay(replay)?;
    if serde_json::to_vec(parameters)
        .map_or(true, |bytes| bytes.len() > MAX_AUTOMATION_REPLAY_BYTES)
    {
        return Err(error("invalid_parameter", "replay inputs exceed 1 MiB"));
    }
    let inputs = required_inputs(replay)?;
    if parameters
        .keys()
        .any(|name| !inputs.iter().any(|input| input.name == *name))
    {
        return Err(error(
            "invalid_parameter",
            "replay contains an unused input parameter",
        ));
    }
    for input in &inputs {
        let value = parameters.get(&input.name).ok_or_else(|| {
            error(
                "missing_parameter",
                format!(
                    "required replay parameter `{}` was not supplied",
                    input.name
                ),
            )
        })?;
        validate_input(value, input.kind)?;
    }
    // Parameter values may invalidate verb arguments (page/chord/etc are not
    // bindable). Validate every resolved operation now, with typed placeholders
    // still in place for as-yet-unavailable step results.
    for (index, step) in replay.steps.iter().enumerate() {
        let mut operation =
            serde_json::to_value(&step.step.operation).map_err(serialization_error)?;
        for binding in &step.bindings {
            if let AutomationBindingSource::Parameter { name } = &binding.source {
                replace(&mut operation, &binding.pointer, parameters[name].clone())?;
            }
        }
        let operation = decode_operation(operation)?;
        validate_operation(&operation).map_err(|failure| with_step(failure, index))?;
    }
    Ok(inputs)
}

/// Resolve one step using exact JSON-pointer replacement. Earlier results are
/// scenario step result values, not AutomationStepResult wrappers. No recursive
/// evaluation, substring substitution, environment lookup or expression exists.
pub(crate) fn resolve_step(
    replay: &AutomationReplay,
    index: usize,
    parameters: &BTreeMap<String, Value>,
    earlier_results: &[Value],
) -> Result<AutomationStep> {
    let step = replay
        .steps
        .get(index)
        .ok_or_else(|| error("invalid_replay", "replay step index is out of range"))?;
    validate_step(replay, index)?;
    let mut operation = serde_json::to_value(&step.step.operation).map_err(serialization_error)?;
    for binding in &step.bindings {
        let value = match &binding.source {
            AutomationBindingSource::Parameter { name } => {
                parameters.get(name).ok_or_else(|| {
                    error(
                        "missing_parameter",
                        format!("required replay parameter `{name}` was not supplied"),
                    )
                })?
            }
            AutomationBindingSource::StepResult {
                step: source,
                pointer,
            } => earlier_results
                .get(*source)
                .and_then(|value| value.pointer(pointer))
                .ok_or_else(|| {
                    error(
                        "missing_result_binding",
                        format!(
                            "step {} result does not contain the requested binding value",
                            source + 1
                        ),
                    )
                })?,
        };
        validate_input(value, binding_kind(&step.step.operation, &binding.pointer)?)?;
        replace(&mut operation, &binding.pointer, value.clone())?;
    }
    let operation = decode_operation(operation)?;
    validate_operation(&operation)?;
    Ok(AutomationStep {
        name: step.step.name.clone(),
        operation,
    })
}

pub(crate) fn required_inputs(replay: &AutomationReplay) -> Result<Vec<AutomationReplayInput>> {
    let mut inputs = BTreeMap::new();
    for step in &replay.steps {
        for binding in &step.bindings {
            if let AutomationBindingSource::Parameter { name } = &binding.source {
                valid_parameter_name(name)?;
                let kind = binding_kind(&step.step.operation, &binding.pointer)?;
                if let Some(previous) = inputs.get(name) {
                    // String + UUID is valid, with UUID the stricter contract.
                    let kind = match (*previous, kind) {
                        (AutomationReplayInputKind::Uuid, AutomationReplayInputKind::String)
                        | (AutomationReplayInputKind::String, AutomationReplayInputKind::Uuid) => {
                            AutomationReplayInputKind::Uuid
                        }
                        (AutomationReplayInputKind::Json, next) => next,
                        (previous, AutomationReplayInputKind::Json) => previous,
                        (previous, next) if previous == next => next,
                        _ => {
                            return Err(error(
                                "invalid_binding",
                                "parameter has incompatible uses",
                            ));
                        }
                    };
                    inputs.insert(name.clone(), kind);
                } else {
                    inputs.insert(name.clone(), kind);
                }
            }
        }
    }
    Ok(inputs
        .into_iter()
        .map(|(name, kind)| {
            let uses = replay.steps.iter().enumerate().flat_map(|(step, replay_step)| {
                replay_step.bindings.iter().filter_map(|binding| {
                    matches!(&binding.source, AutomationBindingSource::Parameter { name: used } if used == &name)
                        .then(|| AutomationReplayInputUse { step, pointer: binding.pointer.clone() })
                }).collect::<Vec<_>>()
            }).collect();
            AutomationReplayInput { name, kind, uses }
        })
        .collect())
}

fn validate_replay(replay: &AutomationReplay) -> Result<()> {
    if replay.format_version != AUTOMATION_REPLAY_FORMAT_VERSION {
        return Err(error(
            "replay_version",
            "unsupported replay format version; expected 1",
        ));
    }
    valid_name(&replay.name)?;
    if replay.steps.is_empty() || replay.steps.len() > MAX_AUTOMATION_STEPS {
        return Err(error(
            "invalid_replay",
            "replay must contain between 1 and 128 steps",
        ));
    }
    check_size(replay)?;
    for index in 0..replay.steps.len() {
        validate_step(replay, index).map_err(|failure| with_step(failure, index))?;
    }
    Ok(())
}

fn validate_step(replay: &AutomationReplay, index: usize) -> Result<()> {
    let step = &replay.steps[index];
    valid_name(&step.step.name)?;
    validate_operation(&step.step.operation)?;
    if step.bindings.len() > MAX_BINDINGS {
        return Err(error(
            "invalid_binding",
            "a replay step has too many bindings",
        ));
    }
    let operation = serde_json::to_value(&step.step.operation).map_err(serialization_error)?;
    let mut pointers = HashSet::new();
    for binding in &step.bindings {
        valid_pointer(&binding.pointer, false)?;
        binding_kind(&step.step.operation, &binding.pointer)?;
        if operation.pointer(&binding.pointer).is_none() {
            return Err(error(
                "invalid_binding",
                "binding destination does not exist in the operation",
            ));
        }
        if !pointers.insert(binding.pointer.as_str()) {
            return Err(error("invalid_binding", "duplicate binding destination"));
        }
        for previous in &pointers {
            if *previous != binding.pointer
                && (binding.pointer.starts_with(&format!("{previous}/"))
                    || previous.starts_with(&format!("{}/", binding.pointer)))
            {
                return Err(error("invalid_binding", "overlapping binding destinations"));
            }
        }
        match &binding.source {
            AutomationBindingSource::Parameter { name } => valid_parameter_name(name)?,
            AutomationBindingSource::StepResult {
                step: source,
                pointer,
            } => {
                if *source >= index {
                    return Err(error(
                        "invalid_binding",
                        "result binding must reference an earlier step",
                    ));
                }
                valid_pointer(pointer, true)?;
            }
        }
    }
    // Raw authored text is never accepted in portable replays. Exact explicit
    // bindings are required, so imported files cannot evade recorder redaction.
    for pointer in required_bound_fields(&step.step.operation) {
        if !pointers.contains(pointer) {
            return Err(error(
                "invalid_binding",
                "text and identity fields require explicit replay bindings",
            ));
        }
    }
    if let AutomationStepOperation::Wait {
        condition: AutomationCondition::State { equals, .. },
        ..
    } = &step.step.operation
    {
        require_bound_strings(equals, "/condition/equals", &pointers)?;
    }
    Ok(())
}

fn require_bound_strings(value: &Value, pointer: &str, bindings: &HashSet<&str>) -> Result<()> {
    if bindings.contains(pointer) {
        return Ok(());
    }
    match value {
        Value::String(_) => {
            return Err(error(
                "invalid_binding",
                "string equality values require explicit replay bindings",
            ));
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                require_bound_strings(value, &format!("{pointer}/{index}"), bindings)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                require_bound_strings(
                    value,
                    &format!("{pointer}/{}", escape_pointer(key)),
                    bindings,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn required_bound_fields(operation: &AutomationStepOperation) -> Vec<&'static str> {
    match operation {
        AutomationStepOperation::Call {
            call: AutomationCall::Type { .. },
        } => vec!["/call/text"],
        AutomationStepOperation::Call {
            call: AutomationCall::Invoke { args, .. },
        } => [
            ("id", "/call/args/id"),
            ("taskId", "/call/args/taskId"),
            ("projectId", "/call/args/projectId"),
            ("text", "/call/args/text"),
            ("title", "/call/args/title"),
        ]
        .into_iter()
        .filter_map(|(key, pointer)| args.get(key).is_some().then_some(pointer))
        .collect(),
        AutomationStepOperation::Wait {
            condition: AutomationCondition::Text { .. } | AutomationCondition::StateContains { .. },
            ..
        } => vec!["/condition/contains"],
        _ => vec![],
    }
}

fn binding_kind(
    operation: &AutomationStepOperation,
    pointer: &str,
) -> Result<AutomationReplayInputKind> {
    use AutomationReplayInputKind::*;
    let kind = match operation {
        AutomationStepOperation::Call {
            call: AutomationCall::Type { .. },
        } if pointer == "/call/text" => String,
        AutomationStepOperation::Call {
            call: AutomationCall::Invoke { args, .. },
        } => match pointer {
            "/call/args/id" if args.get("id").is_some() => Uuid,
            "/call/args/taskId" if args.get("taskId").is_some() => Uuid,
            "/call/args/projectId" if args.get("projectId").is_some() => Uuid,
            "/call/args/title" if args.get("title").is_some() => String,
            "/call/args/text" if args.get("text").is_some() => String,
            _ => {
                return Err(error(
                    "invalid_binding",
                    "binding destination is not a supported argument",
                ));
            }
        },
        AutomationStepOperation::Wait {
            condition: AutomationCondition::Text { .. } | AutomationCondition::StateContains { .. },
            ..
        } if pointer == "/condition/contains" => String,
        AutomationStepOperation::Wait {
            condition: AutomationCondition::State { equals, .. },
            ..
        } => {
            let suffix = pointer
                .strip_prefix("/condition/equals")
                .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                .ok_or_else(|| {
                    error("invalid_binding", "only wait equality values may be bound")
                })?;
            match equals.pointer(suffix) {
                Some(Value::String(value)) if uuid::Uuid::parse_str(value).is_ok() => Uuid,
                Some(Value::String(_)) => String,
                Some(_) => Json,
                None => {
                    return Err(error(
                        "invalid_binding",
                        "binding destination does not exist in wait equality",
                    ));
                }
            }
        }
        _ => {
            return Err(error(
                "invalid_binding",
                "binding may only replace supported data values, never operation structure",
            ));
        }
    };
    Ok(kind)
}

fn validate_input(value: &Value, kind: AutomationReplayInputKind) -> Result<()> {
    match kind {
        AutomationReplayInputKind::String if !value.is_string() => Err(error(
            "invalid_parameter",
            "this replay input must be a string",
        )),
        AutomationReplayInputKind::Uuid
            if value
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_none() =>
        {
            Err(error(
                "invalid_parameter",
                "this replay input must be a UUID string",
            ))
        }
        _ => Ok(()),
    }
}

fn validate_operation(operation: &AutomationStepOperation) -> Result<()> {
    match operation {
        AutomationStepOperation::Call { call } => match call {
            AutomationCall::Describe => Ok(()),
            AutomationCall::Snapshot { sections } => {
                const SECTIONS: &[&str] = &[
                    "tree",
                    "task",
                    "tasks",
                    "sessions",
                    "projects",
                    "route",
                    "board",
                    "composer",
                    "toast",
                    "panels",
                    "ui",
                    "selection",
                    "providers",
                    "runtime",
                    "workspace",
                    "draft",
                ];
                if sections.is_empty()
                    || sections
                        .iter()
                        .any(|section| !SECTIONS.contains(&section.as_str()))
                {
                    return Err(error(
                        "unrecordable",
                        "record explicit lightweight snapshot sections; full state/model/session reads are not portable recordings",
                    ));
                }
                Ok(())
            }
            AutomationCall::Transcript { .. } => {
                Err(error("unrecordable", "transcript reads are not recordable"))
            }
            AutomationCall::Act { action, target } => {
                stable_target(target)?;
                if matches!(action, AutomationAction::Click)
                    && matches!(target, AutomationTarget::Id(id) if matches!(id.as_str(), "send-or-stop" | "composer.send" | "composer.send-or-stop"))
                {
                    return Err(error(
                        "unrecordable",
                        "model-send controls are not supported in portable recordings",
                    ));
                }
                Ok(())
            }
            AutomationCall::Type { text, target } => {
                if let Some(target) = target {
                    stable_target(target)?;
                }
                if text.chars().any(char::is_control) {
                    return Err(error(
                        "unrecordable",
                        "recorded typing cannot include control keys or submission characters; use setDraft for multiline text",
                    ));
                }
                Ok(())
            }
            AutomationCall::Key { chord } => safe_chord(chord),
            AutomationCall::Invoke { verb, args } => validate_invoke(verb, args),
        },
        AutomationStepOperation::Wait {
            condition,
            timeout_ms,
        } => {
            if *timeout_ms > MAX_WAIT_MS {
                return Err(error(
                    "unrecordable",
                    "recorded wait timeout exceeds five minutes",
                ));
            }
            match condition {
                AutomationCondition::Node { id, .. } | AutomationCondition::Text { id, .. } => {
                    valid_semantic_id(id)
                }
                AutomationCondition::State { pointer, .. }
                | AutomationCondition::StateContains { pointer, .. } => {
                    valid_pointer(pointer, false)?;
                    if contains_uuid(pointer) {
                        return Err(error(
                            "unrecordable",
                            "UUID-bearing state pointers are not portable",
                        ));
                    }
                    let section = pointer.split('/').nth(1).unwrap_or("");
                    if matches!(
                        section,
                        "model" | "session" | "sessions" | "transcript" | "diagnostics"
                    ) {
                        return Err(error(
                            "unrecordable",
                            "wait on compact state; transcript/model/raw diagnostic waits are not recordable",
                        ));
                    }
                    Ok(())
                }
                AutomationCondition::Frame { .. } => Err(error(
                    "unrecordable",
                    "frame numbers are instance-local and cannot be recorded",
                )),
                AutomationCondition::Diagnostic { .. } => Err(error(
                    "unrecordable",
                    "raw diagnostic waits are not recordable",
                )),
            }
        }
    }
}

fn validate_invoke(verb: &str, args: &Value) -> Result<()> {
    let empty = serde_json::Map::new();
    let fields = match args {
        Value::Null => &empty,
        Value::Object(fields) => fields,
        _ => {
            return Err(error(
                "unrecordable",
                "recorded invoke arguments must be an object",
            ));
        }
    };
    let allowed: &[&str] = match verb {
        "newSession" => &["taskId", "projectId"],
        "openSession" | "openTask" | "selectProject" => &["id"],
        "renameSession" => &["id", "title"],
        "setDraft" => &["id", "text"],
        "openSettings" => &["page"],
        "togglePanel" => &["panel"],
        _ => {
            return Err(error(
                "unrecordable",
                "this invoke verb is not supported in portable recordings",
            ));
        }
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(error(
            "unrecordable",
            "recorded invoke contains unsupported arguments",
        ));
    }
    for field in ["id", "taskId", "projectId"] {
        if let Some(value) = fields.get(field) {
            validate_input(value, AutomationReplayInputKind::Uuid)?;
        }
    }
    for field in ["text", "title"] {
        if let Some(value) = fields.get(field) {
            validate_input(value, AutomationReplayInputKind::String)?;
        }
    }
    let required: &[&str] = match verb {
        "openSession" | "openTask" | "selectProject" => &["id"],
        "renameSession" => &["id", "title"],
        "setDraft" => &["id", "text"],
        "togglePanel" => &["panel"],
        _ => &[],
    };
    if required.iter().any(|key| !fields.contains_key(*key)) {
        return Err(error(
            "unrecordable",
            "recorded invoke is missing a required argument",
        ));
    }
    if fields.contains_key("taskId") && fields.contains_key("projectId") {
        return Err(error(
            "unrecordable",
            "newSession requires either taskId or projectId, not both",
        ));
    }
    if verb == "openSettings"
        && fields.get("page").is_some_and(|value| {
            !matches!(
                value.as_str(),
                Some(
                    "general"
                        | "providers"
                        | "skills"
                        | "usage"
                        | "daemon"
                        | "computerUse"
                        | "appearance"
                )
            )
        })
    {
        return Err(error("unrecordable", "unknown recorded settings page"));
    }
    if verb == "togglePanel"
        && !matches!(
            fields.get("panel").and_then(Value::as_str),
            Some("sidebar" | "rightPanel")
        )
    {
        return Err(error("unrecordable", "unknown recorded panel"));
    }
    Ok(())
}

fn stable_target(target: &AutomationTarget) -> Result<()> {
    match target {
        AutomationTarget::Id(id) => valid_semantic_id(id),
        AutomationTarget::Node(_) => Err(error(
            "unrecordable",
            "frame-local node targets cannot be recorded; use a stable semantic ID",
        )),
    }
}

fn valid_semantic_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_NAME_BYTES || id.chars().any(char::is_control) {
        return Err(error(
            "unrecordable",
            "recorded semantic IDs must be nonempty and bounded",
        ));
    }
    // UUID-bearing semantic IDs are instance-local, even with a stable prefix.
    // There is deliberately no substring templating to rewrite them.
    if contains_uuid(id) {
        return Err(error(
            "unrecordable",
            "UUID-bearing targets are not portable; use a semantic invoke with an identity binding",
        ));
    }
    Ok(())
}

fn contains_uuid(value: &str) -> bool {
    [36, 32].into_iter().any(|width| {
        value.as_bytes().windows(width).any(|bytes| {
            std::str::from_utf8(bytes)
                .ok()
                .is_some_and(|s| Uuid::parse_str(s).is_ok())
        })
    })
}

fn safe_chord(chord: &str) -> Result<()> {
    const KEYS: &[&str] = &[
        "escape",
        "tab",
        "shift-tab",
        "backspace",
        "delete",
        "left",
        "right",
        "up",
        "down",
        "home",
        "end",
        "pageup",
        "pagedown",
        "secondary-a",
        "secondary-z",
        "secondary-shift-z",
        "ctrl-a",
        "ctrl-z",
        "ctrl-shift-z",
        "cmd-a",
        "cmd-z",
        "cmd-shift-z",
        "shift-left",
        "shift-right",
        "shift-up",
        "shift-down",
        "alt-left",
        "alt-right",
        "secondary-left",
        "secondary-right",
    ];
    if chord.trim().is_empty()
        || chord.len() > MAX_NAME_BYTES
        || chord.split_whitespace().any(|part| !KEYS.contains(&part))
    {
        return Err(error(
            "unrecordable",
            "only editing/navigation chords are recordable; text and submission chords require explicit semantic operations",
        ));
    }
    Ok(())
}

fn valid_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        Err(error(
            "invalid_replay",
            "replay and step names must be nonempty and at most 256 bytes",
        ))
    } else {
        Ok(())
    }
}

fn valid_parameter_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        Err(error(
            "invalid_binding",
            "parameter names must contain 1–64 ASCII letters, numbers or underscores",
        ))
    } else {
        Ok(())
    }
}

fn valid_pointer(pointer: &str, allow_root: bool) -> Result<()> {
    if pointer.len() > MAX_POINTER_BYTES
        || (!pointer.starts_with('/') && !(allow_root && pointer.is_empty()))
    {
        return Err(error("invalid_binding", "expected a bounded JSON pointer"));
    }
    let mut chars = pointer.chars();
    while let Some(character) = chars.next() {
        if character.is_control() || (character == '~' && !matches!(chars.next(), Some('0' | '1')))
        {
            return Err(error(
                "invalid_binding",
                "JSON pointer has an invalid escape or control character",
            ));
        }
    }
    Ok(())
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}
fn replace(value: &mut Value, pointer: &str, replacement: Value) -> Result<()> {
    let slot = value
        .pointer_mut(pointer)
        .ok_or_else(|| error("invalid_binding", "binding destination is missing"))?;
    *slot = replacement;
    Ok(())
}
fn decode_operation(value: Value) -> Result<AutomationStepOperation> {
    serde_json::from_value(value).map_err(|_| {
        error(
            "invalid_binding",
            "bound values do not form a valid automation operation",
        )
    })
}
fn check_size(replay: &AutomationReplay) -> Result<()> {
    if serde_json::to_vec(replay).map_or(true, |bytes| bytes.len() > MAX_AUTOMATION_REPLAY_BYTES) {
        Err(error("recording_limit", "portable replay exceeds 1 MiB"))
    } else {
        Ok(())
    }
}
fn is_new_session(operation: &AutomationStepOperation) -> bool {
    matches!(operation, AutomationStepOperation::Call { call: AutomationCall::Invoke { verb, .. } } if verb == "newSession")
}
fn error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message)
}
fn serialization_error(_: serde_json::Error) -> AutomationError {
    error("invalid_replay", "unable to serialize semantic operation")
}
fn with_step(failure: AutomationError, step: usize) -> AutomationError {
    error(
        &failure.code,
        format!("step {}: {}", step + 1, failure.message),
    )
}
fn sanitized_failure(failure: &AutomationError, step: usize) -> AutomationError {
    let code = if failure.code.len() <= 96
        && failure
            .code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        failure.code.as_str()
    } else {
        "operation_failed"
    };
    error(
        code,
        format!(
            "recorded step {} failed; inspect its original call/wait response for detailed evidence",
            step + 1
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn invoke(verb: &str, args: Value) -> AutomationStepOperation {
        AutomationStepOperation::Call {
            call: AutomationCall::Invoke {
                verb: verb.into(),
                args,
            },
        }
    }
    fn type_text(text: &str) -> AutomationStepOperation {
        AutomationStepOperation::Call {
            call: AutomationCall::Type {
                text: text.into(),
                target: Some(AutomationTarget::Id("composer-field".into())),
            },
        }
    }
    fn wait_value(pointer: &str, equals: Value) -> AutomationStepOperation {
        AutomationStepOperation::Wait {
            condition: AutomationCondition::State {
                pointer: pointer.into(),
                equals,
            },
            timeout_ms: 1000,
        }
    }
    fn record(recorder: &mut Recorder, operation: AutomationStepOperation, result: Value) {
        let ticket = recorder.begin(&operation).unwrap();
        recorder.complete(ticket, Ok(result)).unwrap();
    }
    fn single_replay(operation: AutomationStepOperation) -> AutomationReplay {
        AutomationReplay {
            format_version: AUTOMATION_REPLAY_FORMAT_VERSION,
            name: "test".into(),
            steps: vec![AutomationReplayStep {
                step: AutomationStep {
                    name: "one".into(),
                    operation,
                },
                bindings: vec![],
            }],
        }
    }

    #[test]
    fn typed_secrets_are_only_required_inputs_without_defaults() {
        let mut recorder = Recorder::new("draft".into()).unwrap();
        record(
            &mut recorder,
            type_text("private-token-abcdefghijklmnopqrstuvwxyz"),
            json!({"value":{"typed":40},"observed":{"composer":{"text":"another private value"}}}),
        );
        let status = recorder.status();
        assert_eq!(status.required_inputs.len(), 1);
        assert_eq!(status.required_inputs[0].name, "text1");
        assert_eq!(
            status.required_inputs[0].kind,
            AutomationReplayInputKind::String
        );
        let replay = recorder.stop().unwrap();
        let wire = serde_json::to_string(&replay).unwrap();
        assert!(!wire.contains("private"));
        assert!(!wire.contains("observed"));
        assert!(!wire.contains("default"));
        assert_eq!(replay.steps[0].bindings[0].pointer, "/call/text");
        assert_eq!(
            preflight(&replay, &BTreeMap::new()).unwrap_err().code,
            "missing_parameter"
        );
        let parameters = BTreeMap::from([("text1".into(), json!("replacement"))]);
        preflight(&replay, &parameters).unwrap();
        let step = resolve_step(&replay, 0, &parameters, &[]).unwrap();
        assert_eq!(
            serde_json::to_value(step).unwrap()["call"]["text"],
            "replacement"
        );
    }

    #[test]
    fn opened_id_references_follow_the_new_replay_session() {
        let mut recorder = Recorder::new("portable session".into()).unwrap();
        let recorded_id = Uuid::new_v4();
        record(
            &mut recorder,
            invoke("newSession", json!({})),
            json!({"value":{"opened":recorded_id}}),
        );
        record(
            &mut recorder,
            invoke("openSession", json!({"id":recorded_id})),
            json!(null),
        );
        record(
            &mut recorder,
            wait_value("/selection/id", json!(recorded_id)),
            json!(null),
        );
        record(
            &mut recorder,
            invoke("setDraft", json!({"id":recorded_id,"text":"secret draft"})),
            json!(null),
        );
        let replay = recorder.stop().unwrap();
        let wire = serde_json::to_string(&replay).unwrap();
        assert!(!wire.contains(&recorded_id.to_string()));
        assert!(!wire.contains("secret draft"));
        for index in [1, 2, 3] {
            assert!(
                matches!(&replay.steps[index].bindings[0].source, AutomationBindingSource::StepResult { step: 0, pointer } if pointer == "/value/opened")
            );
        }
        let parameters = BTreeMap::from([("text1".into(), json!("new draft"))]);
        preflight(&replay, &parameters).unwrap();
        let replay_id = Uuid::new_v4();
        let results = vec![
            json!({"value":{"opened":replay_id}}),
            Value::Null,
            Value::Null,
        ];
        let step = resolve_step(&replay, 1, &parameters, &results).unwrap();
        assert_eq!(
            serde_json::to_value(step).unwrap()["call"]["args"]["id"],
            replay_id.to_string()
        );
        let step = resolve_step(&replay, 2, &parameters, &results).unwrap();
        assert_eq!(
            serde_json::to_value(step).unwrap()["condition"]["equals"],
            replay_id.to_string()
        );
        let step = resolve_step(&replay, 3, &parameters, &results).unwrap();
        assert_eq!(
            serde_json::to_value(step).unwrap()["call"]["args"],
            json!({"id":replay_id,"text":"new draft"})
        );
    }

    #[test]
    fn seed_ids_are_required_and_deduplicated_without_seed_defaults() {
        let project = Uuid::new_v4();
        let task = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut recorder = Recorder::new("seed".into()).unwrap();
        record(
            &mut recorder,
            invoke("selectProject", json!({"id":project})),
            Value::Null,
        );
        record(
            &mut recorder,
            invoke("newSession", json!({"projectId":project})),
            json!({"value":{"opened":session}}),
        );
        record(
            &mut recorder,
            invoke("openTask", json!({"id":task})),
            Value::Null,
        );
        record(
            &mut recorder,
            wait_value("/route/selectedTask", json!(task)),
            Value::Null,
        );
        let replay = recorder.stop().unwrap();
        let inputs = required_inputs(&replay).unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(
            inputs
                .iter()
                .all(|input| input.kind == AutomationReplayInputKind::Uuid)
        );
        let wire = serde_json::to_string(&replay).unwrap();
        assert!(!wire.contains(&project.to_string()));
        assert!(!wire.contains(&task.to_string()));
        let parameters = BTreeMap::from([
            ("id1".into(), json!(Uuid::new_v4())),
            ("id2".into(), json!(Uuid::new_v4())),
        ]);
        preflight(&replay, &parameters).unwrap();
    }

    #[test]
    fn task_new_session_and_headless_titles_are_parameterized() {
        let task = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut recorder = Recorder::new("task".into()).unwrap();
        record(
            &mut recorder,
            invoke("newSession", json!({"taskId":task})),
            json!({"value":{"opened":session}}),
        );
        record(
            &mut recorder,
            invoke(
                "renameSession",
                json!({"id":session,"title":"private title"}),
            ),
            Value::Null,
        );
        record(
            &mut recorder,
            invoke(
                "setDraft",
                json!({"id":session,"text":"private\nmultiline draft"}),
            ),
            Value::Null,
        );
        let replay = recorder.stop().unwrap();
        assert!(!serde_json::to_string(&replay).unwrap().contains("private"));
        let parameters = BTreeMap::from([
            ("id1".into(), json!(Uuid::new_v4())),
            ("text1".into(), json!("new title")),
            ("text2".into(), json!("new\ntext")),
        ]);
        preflight(&replay, &parameters).unwrap();
        assert_eq!(required_inputs(&replay).unwrap().len(), 3);
    }

    #[test]
    fn one_in_flight_operation_and_matching_ticket_are_enforced() {
        let mut recorder = Recorder::new("ordered".into()).unwrap();
        let first = recorder.begin(&type_text("one")).unwrap();
        assert_eq!(
            recorder.begin(&type_text("two")).unwrap_err().code,
            "recording_busy"
        );
        assert_eq!(recorder.stop().unwrap_err().code, "recording_busy");
        assert_eq!(
            recorder
                .complete(RecordingTicket(Uuid::new_v4()), Ok(Value::Null))
                .unwrap_err()
                .code,
            "recording_ticket"
        );
        assert!(recorder.status().pending);
        recorder.complete(first, Ok(Value::Null)).unwrap();
        assert_eq!(
            recorder.complete(first, Ok(Value::Null)).unwrap_err().code,
            "recording_ticket"
        );
        let replay = recorder.stop().unwrap();
        assert_eq!(replay.steps.len(), 1);
        assert_eq!(recorder.status().status, AutomationRecordingStatus::Stopped);
        assert_eq!(
            recorder.begin(&type_text("late")).unwrap_err().code,
            "recording_closed"
        );
    }

    #[test]
    fn operation_failure_stops_appending_and_keeps_bounded_sanitized_evidence() {
        let mut recorder = Recorder::new("failure".into()).unwrap();
        record(&mut recorder, type_text("first secret"), Value::Null);
        let ticket = recorder.begin(&type_text("second secret")).unwrap();
        let mut failure = error("invalid_type", "second secret ".repeat(100_000));
        failure.target = Some("second secret".into());
        failure.nearest = Some(vec!["second secret".into()]);
        recorder.complete(ticket, Err(failure)).unwrap_err();
        assert_eq!(recorder.status().status, AutomationRecordingStatus::Failed);
        assert_eq!(recorder.status().completed_steps, 1);
        assert_eq!(recorder.status().error.unwrap().code, "invalid_type");
        let status = serde_json::to_string(&recorder.status()).unwrap();
        assert!(status.len() < 1024);
        assert!(!status.contains("secret"));
        assert_eq!(
            recorder.begin(&type_text("third")).unwrap_err().code,
            "recording_closed"
        );
        assert_eq!(recorder.stop().unwrap().steps.len(), 1);
        assert_eq!(recorder.status().status, AutomationRecordingStatus::Failed);
    }

    #[test]
    fn malformed_opened_result_is_a_terminal_recording_failure() {
        for result in [
            Value::Null,
            json!({"value":{"opened":null}}),
            json!({"value":{"opened":"not-a-uuid"}}),
        ] {
            let mut recorder = Recorder::new("bad result".into()).unwrap();
            let ticket = recorder.begin(&invoke("newSession", json!({}))).unwrap();
            assert_eq!(
                recorder.complete(ticket, Ok(result)).unwrap_err().code,
                "recording_result"
            );
            assert_eq!(recorder.status().status, AutomationRecordingStatus::Failed);
            assert!(recorder.stop().unwrap().steps.is_empty());
        }
    }

    #[test]
    fn tickets_are_not_reused_across_replaced_recorders() {
        let mut old = Recorder::new("old".into()).unwrap();
        let old_ticket = old.begin(&type_text("old secret")).unwrap();
        old.cancel();
        let mut replacement = Recorder::new("new".into()).unwrap();
        let new_ticket = replacement.begin(&type_text("new secret")).unwrap();
        assert_ne!(old_ticket, new_ticket);
        assert_eq!(
            replacement
                .complete(old_ticket, Ok(Value::Null))
                .unwrap_err()
                .code,
            "recording_ticket"
        );
        assert!(replacement.status().pending);
        replacement.complete(new_ticket, Ok(Value::Null)).unwrap();
        assert_eq!(replacement.stop().unwrap().steps.len(), 1);
    }

    #[test]
    fn cancellation_discards_steps_and_late_completions() {
        let mut recorder = Recorder::new("cancel".into()).unwrap();
        record(&mut recorder, type_text("secret one"), Value::Null);
        let ticket = recorder.begin(&type_text("secret two")).unwrap();
        recorder.cancel();
        assert_eq!(
            recorder.status().status,
            AutomationRecordingStatus::Cancelled
        );
        assert_eq!(recorder.status().completed_steps, 0);
        assert!(recorder.status().required_inputs.is_empty());
        assert_eq!(
            recorder.complete(ticket, Ok(Value::Null)).unwrap_err().code,
            "recording_ticket"
        );
        assert_eq!(recorder.stop().unwrap_err().code, "recording_cancelled");
    }

    #[test]
    fn unsupported_operations_are_rejected_before_recording_reservation() {
        let unsupported = vec![
            AutomationStepOperation::Call {
                call: AutomationCall::Transcript {
                    conversation_id: Uuid::new_v4(),
                    query: TranscriptQuery::Index {
                        collection: TranscriptCollection::Messages,
                        offset: 0,
                        limit: 1,
                    },
                },
            },
            AutomationStepOperation::Wait {
                condition: AutomationCondition::Diagnostic {
                    query: DiagnosticQuery::default(),
                },
                timeout_ms: 1000,
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Act {
                    action: AutomationAction::Click,
                    target: AutomationTarget::Node("77".into()),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Type {
                    text: "x".into(),
                    target: Some(AutomationTarget::Node("77".into())),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Act {
                    action: AutomationAction::Click,
                    target: AutomationTarget::Id("send-or-stop".into()),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Act {
                    action: AutomationAction::Click,
                    target: AutomationTarget::Id(format!("session-{}", Uuid::new_v4())),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Key {
                    chord: "enter".into(),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Key {
                    chord: "h e l l o".into(),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Key {
                    chord: "secondary-v".into(),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Snapshot { sections: vec![] },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Snapshot {
                    sections: vec!["session".into()],
                },
            },
            invoke("submitPrompt", json!({"text":"secret"})),
            invoke("answerPermission", json!({"requestId":"private"})),
            invoke("newTask", json!({"title":"private"})),
            invoke("unknown", json!({})),
            invoke("newSession", json!({"hidden":"secret"})),
            invoke("openSettings", json!({"page":"unknown"})),
            invoke("setDraft", json!({"text":"missing id"})),
            type_text("hello\n"),
            AutomationStepOperation::Wait {
                condition: AutomationCondition::Frame { after: 9 },
                timeout_ms: 100,
            },
            wait_value("/session/messages", json!([])),
            wait_value("/model", json!({})),
            wait_value(&format!("/tasks/{}/status", Uuid::new_v4()), json!(true)),
        ];
        for operation in unsupported {
            let mut recorder = Recorder::new("rejected".into()).unwrap();
            assert!(
                recorder.begin(&operation).is_err(),
                "unexpected accepted operation: {operation:?}"
            );
            assert!(!recorder.status().pending);
            assert_eq!(recorder.status().completed_steps, 0);
            assert_eq!(recorder.status().status, AutomationRecordingStatus::Failed);
        }
    }

    #[test]
    fn safe_navigation_observation_and_waits_remain_recordable() {
        let operations = vec![
            AutomationStepOperation::Call {
                call: AutomationCall::Describe,
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Snapshot {
                    sections: vec!["selection".into(), "draft".into()],
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Act {
                    action: AutomationAction::Focus,
                    target: AutomationTarget::Id("composer-field".into()),
                },
            },
            AutomationStepOperation::Call {
                call: AutomationCall::Key {
                    chord: "secondary-a backspace".into(),
                },
            },
            invoke("openSettings", json!({"page":"general"})),
            invoke("togglePanel", json!({"panel":"sidebar"})),
            AutomationStepOperation::Wait {
                condition: AutomationCondition::Node {
                    id: "composer-field".into(),
                    present: true,
                },
                timeout_ms: 1000,
            },
            wait_value("/board/savePending", json!(false)),
        ];
        let mut recorder = Recorder::new("safe".into()).unwrap();
        for operation in operations {
            record(&mut recorder, operation, Value::Null);
        }
        let replay = recorder.stop().unwrap();
        preflight(&replay, &BTreeMap::new()).unwrap();
        assert_eq!(replay.steps.len(), 8);
    }

    #[test]
    fn wait_text_and_nested_equality_bind_exact_values_not_substrings() {
        let id = Uuid::new_v4();
        let mut recorder = Recorder::new("waits".into()).unwrap();
        record(
            &mut recorder,
            wait_value(
                "/draft",
                json!({"text":"hello ${id1}", "ids":[id], "a/b~c":"private", "count":2}),
            ),
            Value::Null,
        );
        let replay = recorder.stop().unwrap();
        assert_eq!(replay.steps[0].bindings.len(), 3);
        // serde_json's preserve_order feature is unified across the workspace.
        // Parameter names follow observed field order, so use the exported bindings.
        let name_at = |pointer: &str| match &replay.steps[0]
            .bindings
            .iter()
            .find(|binding| binding.pointer == pointer)
            .unwrap()
            .source
        {
            AutomationBindingSource::Parameter { name } => name.clone(),
            _ => panic!("expected required parameter"),
        };
        let remapped_id = json!(Uuid::new_v4());
        let parameters = BTreeMap::from([
            (name_at("/condition/equals/ids/0"), remapped_id.clone()),
            (
                name_at("/condition/equals/a~1b~0c"),
                json!("literal ${text2}"),
            ),
            (name_at("/condition/equals/text"), json!("${id1}")),
        ]);
        preflight(&replay, &parameters).unwrap();
        let resolved =
            serde_json::to_value(resolve_step(&replay, 0, &parameters, &[]).unwrap()).unwrap();
        assert_eq!(resolved["condition"]["equals"]["a/b~c"], "literal ${text2}");
        assert_eq!(resolved["condition"]["equals"]["text"], "${id1}");
        assert_eq!(resolved["condition"]["equals"]["count"], 2);
        assert_eq!(resolved["condition"]["equals"]["ids"][0], remapped_id);
    }

    #[test]
    fn imported_files_cannot_bind_operation_structure_or_future_results() {
        let mut recorder = Recorder::new("binding validation".into()).unwrap();
        record(&mut recorder, type_text("secret"), Value::Null);
        let good = recorder.stop().unwrap();
        let parameters = BTreeMap::from([("text1".into(), json!("text"))]);
        for pointer in [
            "",
            "/call",
            "/call/type",
            "/type",
            "/name",
            "/call/target/id",
            "/call/nonexistent",
            "/call/text~2",
        ] {
            let mut replay = good.clone();
            replay.steps[0].bindings[0].pointer = pointer.into();
            assert!(
                preflight(&replay, &parameters).is_err(),
                "accepted structural pointer {pointer}"
            );
        }
        for source in [0, 1, usize::MAX] {
            let mut replay = good.clone();
            replay.steps[0].bindings[0].source = AutomationBindingSource::StepResult {
                step: source,
                pointer: "/value/opened".into(),
            };
            assert_eq!(
                preflight(&replay, &parameters).unwrap_err().code,
                "invalid_binding"
            );
        }
        let mut replay = good.clone();
        let duplicate = replay.steps[0].bindings[0].clone();
        replay.steps[0].bindings.push(duplicate);
        assert_eq!(
            preflight(&replay, &parameters).unwrap_err().code,
            "invalid_binding"
        );
        let mut replay = good;
        replay.steps[0].bindings.clear();
        assert_eq!(
            preflight(&replay, &parameters).unwrap_err().code,
            "invalid_binding"
        );
    }

    #[test]
    fn parameter_preflight_validates_all_steps_before_any_effect() {
        let mut recorder = Recorder::new("preflight".into()).unwrap();
        record(
            &mut recorder,
            invoke("openSettings", json!({})),
            Value::Null,
        );
        record(
            &mut recorder,
            invoke("openSession", json!({"id":Uuid::new_v4()})),
            Value::Null,
        );
        record(&mut recorder, type_text("secret"), Value::Null);
        let replay = recorder.stop().unwrap();
        for parameters in [
            BTreeMap::new(),
            BTreeMap::from([
                ("id1".into(), json!("bad uuid")),
                ("text1".into(), json!("x")),
            ]),
            BTreeMap::from([
                ("id1".into(), json!(Uuid::new_v4())),
                ("text1".into(), json!(3)),
            ]),
            BTreeMap::from([
                ("id1".into(), json!(Uuid::new_v4())),
                ("text1".into(), json!("hello\n")),
            ]),
        ] {
            assert!(preflight(&replay, &parameters).is_err());
        }
        assert!(
            preflight(
                &single_replay(wait_value("/selection/id", json!(Uuid::new_v4()))),
                &BTreeMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn missing_or_invalid_result_bindings_fail_before_dependent_dispatch() {
        let mut recorder = Recorder::new("results".into()).unwrap();
        let id = Uuid::new_v4();
        record(
            &mut recorder,
            invoke("newSession", json!({})),
            json!({"value":{"opened":id}}),
        );
        record(
            &mut recorder,
            invoke("openSession", json!({"id":id})),
            Value::Null,
        );
        let replay = recorder.stop().unwrap();
        preflight(&replay, &BTreeMap::new()).unwrap();
        for results in [vec![], vec![json!({"value":{}})]] {
            assert_eq!(
                resolve_step(&replay, 1, &BTreeMap::new(), &results)
                    .unwrap_err()
                    .code,
                "missing_result_binding"
            );
        }
        assert_eq!(
            resolve_step(
                &replay,
                1,
                &BTreeMap::new(),
                &[json!({"value":{"opened":42}})]
            )
            .unwrap_err()
            .code,
            "invalid_parameter"
        );
    }

    #[test]
    fn limits_are_checked_before_accepting_a_step() {
        let mut recorder = Recorder::new("bounded".into()).unwrap();
        for _ in 0..MAX_AUTOMATION_STEPS {
            record(
                &mut recorder,
                wait_value("/board/savePending", json!(false)),
                Value::Null,
            );
        }
        assert_eq!(
            recorder
                .begin(&wait_value("/board/savePending", json!(false)))
                .unwrap_err()
                .code,
            "recording_limit"
        );
        assert_eq!(recorder.status().completed_steps, MAX_AUTOMATION_STEPS);
        let mut oversized = Recorder::new("large".into()).unwrap();
        let operation = wait_value(
            "/board",
            json!({"numbers":vec![100_000; MAX_AUTOMATION_REPLAY_BYTES / 4]}),
        );
        assert_eq!(
            oversized.begin(&operation).unwrap_err().code,
            "recording_limit"
        );
        assert_eq!(oversized.status().completed_steps, 0);
        let mut many = Recorder::new("many bindings".into()).unwrap();
        assert_eq!(
            many.begin(&wait_value(
                "/draft",
                json!(vec!["secret"; MAX_BINDINGS + 1])
            ))
            .unwrap_err()
            .code,
            "invalid_binding"
        );
        assert!(!many.status().pending);
    }

    #[test]
    fn version_empty_replay_and_invalid_pointers_are_rejected() {
        let mut replay = single_replay(wait_value("/board/savePending", json!(false)));
        replay.format_version = 2;
        assert_eq!(
            preflight(&replay, &BTreeMap::new()).unwrap_err().code,
            "replay_version"
        );
        replay.format_version = 1;
        replay.steps.clear();
        assert_eq!(
            preflight(&replay, &BTreeMap::new()).unwrap_err().code,
            "invalid_replay"
        );
        for pointer in ["not/a/pointer", "/bad~", "/bad~2", "/bad\n"] {
            assert!(
                Recorder::new("pointer".into())
                    .unwrap()
                    .begin(&wait_value(pointer, json!(true)))
                    .is_err()
            );
        }
        assert!(Recorder::new("".into()).is_err());
        assert!(Recorder::new("x".repeat(MAX_NAME_BYTES + 1)).is_err());
    }

    #[test]
    fn preflight_bounds_parameters_and_rejects_unused_values() {
        let replay = single_replay(wait_value("/selection/persistent", json!(false)));
        assert_eq!(
            preflight(
                &replay,
                &BTreeMap::from([("unused".into(), json!("private"))])
            )
            .unwrap_err()
            .code,
            "invalid_parameter"
        );
        let large = BTreeMap::from([(
            "unused".into(),
            json!("x".repeat(MAX_AUTOMATION_REPLAY_BYTES)),
        )]);
        assert_eq!(
            preflight(&replay, &large).unwrap_err().code,
            "invalid_parameter"
        );
    }
}
