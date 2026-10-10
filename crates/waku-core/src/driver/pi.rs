//! Pi RPC transport, shared by Pi and Oh My Pi.
//!
//! Oh My Pi is a fork of Pi that kept the newline-delimited RPC transport but
//! renamed part of the surface: forking is `branch`, a run settles on
//! `agent_end` instead of `agent_settled`, and oversized frames are chunked
//! once protocol v2 is negotiated. [`PiFlavor`] carries those differences so
//! both providers share one transport instead of two near-copies.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{activity, computer_use as computer_use_runtime};
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityItem, ActivityKind, BackgroundWorkEvent, BackgroundWorkItem, BackgroundWorkKind,
    BackgroundWorkStatus, DriverEvent, ExtensionWidgetPlacement, NotificationSeverity,
    ProviderResumeCursor, ReportedCommand, RuntimeMode, UserInputAnswer, UserInputOption,
    UserInputQuestion,
};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The handshake races the agent's own startup, so it needs more headroom than
/// a request against the already-running process. Pi loads extensions and
/// resources and, when model networking is on, refreshes its model catalog
/// before it reads stdin — it budgets 15 s for that refresh alone — and a
/// timed-out `get_state` there fails the whole session.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oh My Pi has to start a whole second agent to clone a session, so it needs
/// more headroom than a request against the already-running process.
const CLONE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oh My Pi refuses to reassemble beyond this, so neither should Waku.
const MAX_REASSEMBLED_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Which dialect of the Pi RPC protocol a session speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiFlavor {
    Pi,
    OhMyPi,
}

impl PiFlavor {
    fn display_name(self) -> &'static str {
        match self {
            Self::Pi => "Pi",
            Self::OhMyPi => "Oh My Pi",
        }
    }

    /// Pi has no permission system and only needs project-local files trusted;
    /// Oh My Pi does have one, and Waku only ever runs these in Full access.
    fn full_access_arg(self) -> &'static str {
        match self {
            Self::Pi => "--approve",
            Self::OhMyPi => "--yolo",
        }
    }

    /// The event that means the run is over and no more work is scheduled.
    fn settled_event(self) -> &'static str {
        match self {
            Self::Pi => "agent_settled",
            Self::OhMyPi => "agent_end",
        }
    }

    fn session_info_event(self) -> &'static str {
        match self {
            Self::Pi => "session_info_changed",
            Self::OhMyPi => "session_info_update",
        }
    }

    fn session_info_title_field(self) -> &'static str {
        match self {
            Self::Pi => "name",
            Self::OhMyPi => "title",
        }
    }

    fn branch_messages_command(self) -> &'static str {
        match self {
            Self::Pi => "get_fork_messages",
            Self::OhMyPi => "get_branch_messages",
        }
    }

    fn branch_command(self) -> &'static str {
        match self {
            Self::Pi => "fork",
            Self::OhMyPi => "branch",
        }
    }

    /// Oh My Pi dropped Pi's opt-out env var; it gates its update check on a
    /// setting instead, and does it off the startup path either way.
    fn skips_version_check_by_env(self) -> bool {
        matches!(self, Self::Pi)
    }

    /// Only Oh My Pi chunks oversized frames, and only after it is asked to.
    fn negotiates_protocol_v2(self) -> bool {
        matches!(self, Self::OhMyPi)
    }

    /// Waku's computer-use bridge is a Pi extension written against Pi's
    /// extension API. Oh My Pi ships its own `/computer` instead.
    fn supports_waku_computer_use(self) -> bool {
        matches!(self, Self::Pi)
    }

    fn cursor(self, session_id: String, session_file: Option<PathBuf>) -> ProviderResumeCursor {
        match self {
            Self::Pi => ProviderResumeCursor::Pi {
                session_id,
                session_file,
            },
            Self::OhMyPi => ProviderResumeCursor::OhMyPi {
                session_id,
                session_file,
            },
        }
    }

    fn session_file_from_cursor(self, cursor: &ProviderResumeCursor) -> Option<&PathBuf> {
        match (self, cursor) {
            (Self::Pi, ProviderResumeCursor::Pi { session_file, .. })
            | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { session_file, .. }) => {
                session_file.as_ref()
            }
            _ => None,
        }
    }

    fn owns_cursor(self, cursor: &ProviderResumeCursor) -> bool {
        matches!(
            (self, cursor),
            (Self::Pi, ProviderResumeCursor::Pi { .. })
                | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { .. })
        )
    }
}

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
    /// Take back whatever the provider's queue still holds without waiting for
    /// its answer; the reader owns stdout and cannot block on a response.
    RetractQueuedMessages,
    CancelExtensionRequest(String),
    /// A dialog answer already in the `extension_ui_response` shape the
    /// provider reads. The reader owns stdout, so the answer travels the same
    /// way every other write does.
    ExtensionUiResponse(Value),
    Options(SessionOptions),
    Rollback {
        turns: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Fork {
        turns_to_remove: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Shutdown,
}

enum PendingResponse {
    Request(Sender<Result<Value, String>>),
    Prompt,
}

type PendingResponses = Arc<Mutex<HashMap<String, PendingResponse>>>;

pub struct PiDriver {
    flavor: PiFlavor,
    commands: Sender<CommandMessage>,
    computer_use: Option<computer_use_runtime::ComputerUseRuntime>,
    /// The dialogs still waiting for the user, keyed by the provider's request
    /// id, shared with the reader thread that opened them. The stored method is
    /// what tells the answer which `extension_ui_response` shape it must take.
    dialogs: PiDialogs,
}

/// The dialogs the transport has handed to the client and not yet answered.
type PiDialogs = Arc<Mutex<HashMap<String, PiExtensionDialog>>>;

/// The dialog methods whose answers travel back as an `extension_ui_response`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PiExtensionDialog {
    Select,
    Confirm,
    Input,
    Editor,
}

impl PiExtensionDialog {
    /// `None` for a method Pi does not document: its answer shape is unknown,
    /// so it is cancelled rather than presented.
    fn from_method(method: &str) -> Option<Self> {
        match method {
            "select" => Some(Self::Select),
            "confirm" => Some(Self::Confirm),
            "input" => Some(Self::Input),
            "editor" => Some(Self::Editor),
            _ => None,
        }
    }

    /// The one question the client asks for this dialog. The question id is
    /// the provider's request id, so the answer needs no other correlation.
    fn question(self, id: &str, request: &Value) -> UserInputQuestion {
        let title = request
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = |field: &str| {
            request
                .get(field)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(title)
        };
        let (header, question, options) = match self {
            // Pi's select, input and editor carry only a title; it is the
            // question, so the card leads with it rather than an empty label.
            Self::Select => {
                let options = request
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|label| UserInputOption {
                                label: label.to_owned(),
                                description: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ("", title, options)
            }
            Self::Confirm => (
                title,
                text("message"),
                [(PI_CONFIRM_ACCEPT, None), (PI_CONFIRM_DECLINE, None)]
                    .into_iter()
                    .map(|(label, description)| UserInputOption {
                        label: label.to_owned(),
                        description: description.map(str::to_owned),
                    })
                    .collect(),
            ),
            // The placeholder and prefill Pi sends for these have no home on
            // the question card; the typed answer is what the provider gets.
            Self::Input | Self::Editor => ("", title, Vec::new()),
        };
        UserInputQuestion {
            id: id.to_owned(),
            header: header.to_owned(),
            question: question.to_owned(),
            options,
            multi_select: false,
        }
    }

    /// The `extension_ui_response` for the labels the client chose. No label is
    /// the dismissal, which is Pi's cancellation: the extension receives
    /// `undefined` for a value dialog and `false` for a confirmation.
    fn response(self, id: &str, answers: &[String]) -> Value {
        let answer = answers.iter().find(|answer| !answer.trim().is_empty());
        match (self, answer) {
            (_, None) => json!({
                "type": "extension_ui_response",
                "id": id,
                "cancelled": true,
            }),
            (Self::Confirm, Some(answer)) => json!({
                "type": "extension_ui_response",
                "id": id,
                "confirmed": answer == PI_CONFIRM_ACCEPT,
            }),
            (_, Some(answer)) => json!({
                "type": "extension_ui_response",
                "id": id,
                "value": answer,
            }),
        }
    }
}

/// The labels of the confirm dialog's two answers. They are the card's text and
/// the transport's signal at once, so `response` compares the answer to the
/// accept label rather than trusting the order Pi never guaranteed.
const PI_CONFIRM_ACCEPT: &str = "Yes";
const PI_CONFIRM_DECLINE: &str = "No";

fn configure_pi_computer_use_command(
    command: &mut std::process::Command,
    config: Option<(&computer_use_runtime::ComputerUseConfig, &Path)>,
) {
    if let Some((config, extension)) = config {
        command
            .arg("--extension")
            .arg(extension)
            .arg("--skill")
            .arg(&config.skill_path)
            .env("WAKU_JS_REPL_SERVER", &config.repl_path)
            .env("WAKU_COMPUTER_USE_SERVER", &config.server_path)
            .env(
                "WAKU_COMPUTER_USE_PROCESS_DIRECTORY",
                &config.process_directory,
            );
    }
}

impl PiDriver {
    pub fn start(
        flavor: PiFlavor,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        Self::launch(flavor, options, events, &[])
    }

    /// Starts a session that also loads `extensions` — the same `--extension`
    /// flag the launch already hands Waku's own Pi extension. The live tests
    /// pass their fixture this way, so the provider's own records are provoked
    /// through the launch path the product uses rather than replayed from a
    /// canned frame.
    pub(crate) fn launch(
        flavor: PiFlavor,
        options: DriverStartOptions,
        events: DriverEventSender,
        extensions: &[PathBuf],
    ) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            computer_use_enabled,
            provider_cursor,
        } = options;
        if mode != RuntimeMode::FullAccess {
            return Err(anyhow!(
                "{} currently supports Full access only",
                flavor.display_name()
            ));
        }
        let resume_session_file = match provider_cursor {
            Some(cursor) if flavor.owns_cursor(&cursor) => {
                let Some(session_file) = flavor.session_file_from_cursor(&cursor).cloned() else {
                    return Err(anyhow!(
                        "cannot resume {} because its native session file is missing",
                        flavor.display_name()
                    ));
                };
                Some(session_file)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume {} from a {} cursor",
                    flavor.display_name(),
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };
        if let Some(model) = model.as_deref() {
            parse_model_slug(model)?;
        }

        let computer_use = (computer_use_enabled && flavor.supports_waku_computer_use())
            .then(|| computer_use_runtime::ComputerUseRuntime::start(events.clone()))
            .transpose()?;
        let pi_extension = computer_use
            .as_ref()
            .map(|_| crate::computer_use::pi_extension_path())
            .transpose()?;
        let mut command = crate::command_env::command(&binary);
        command.args(["--mode", "rpc", flavor.full_access_arg()]);
        for extension in extensions {
            command.arg("--extension").arg(extension);
        }
        if flavor.skips_version_check_by_env() {
            command.env("PI_SKIP_VERSION_CHECK", "1");
        }
        configure_pi_computer_use_command(
            &mut command,
            computer_use
                .as_ref()
                .zip(pi_extension.as_deref())
                .map(|(runtime, extension)| (&runtime.config, extension)),
        );
        let command = command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .with_context(|| format!("failed to start `{} --mode rpc`", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{} stdin unavailable", flavor.display_name()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{} stdout unavailable", flavor.display_name()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("{} stderr unavailable", flavor.display_name()))?;

        let (commands, command_rx) = unbounded();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let run = RunLiveness::default();
        let reader_run = run.clone();
        let reader_pending = pending.clone();
        let reader_commands = commands.clone();
        let reader_events = events.clone();
        let dialogs: PiDialogs = Arc::new(Mutex::new(HashMap::new()));
        let reader_dialogs = dialogs.clone();
        let reader_thread =
            thread::Builder::new()
                .name("waku-pi-reader".into())
                .spawn(move || {
                    let mut stream_state = PiStreamState {
                        run: reader_run,
                        dialogs: reader_dialogs,
                        ..PiStreamState::default()
                    };
                    let mut chunks = ChunkAssembly::default();
                    for line in BufReader::new(stdout).lines() {
                        match line {
                            Ok(line) if !line.trim().is_empty() => {
                                match serde_json::from_str::<Value>(&line) {
                                    Ok(value) => {
                                        // A chunked frame arrives as an
                                        // uninterrupted run of `rpc_chunk`
                                        // envelopes that reassemble into one
                                        // logical message.
                                        match chunks.accept(value) {
                                            Ok(Some(value)) => handle_pi_message(
                                                flavor,
                                                value,
                                                &reader_pending,
                                                &reader_commands,
                                                &reader_events,
                                                &mut stream_state,
                                            ),
                                            Ok(None) => {}
                                            Err(error) => {
                                                let _ =
                                                    reader_events.send(DriverEvent::Error(tr!(
                                                        "errors.provider_transport_read",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        let _ = reader_events.send(DriverEvent::Error(tr!(
                                            "errors.provider_invalid_json",
                                            provider = flavor.display_name(),
                                            error = error
                                        )));
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = reader_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_read",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                break;
                            }
                        }
                    }
                    // Unblock anything waiting on an RPC reply immediately; the
                    // process thread owns the `ProcessExited` announcement so a
                    // non-zero exit can be reported before the runtime is torn down.
                    fail_pending(
                        &reader_pending,
                        &format!("{} RPC process exited", flavor.display_name()),
                    );
                })?;

        let writer_pending = pending;
        let writer_events = events.clone();
        thread::Builder::new()
            .name("waku-pi-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut next_request_id = 0_u64;
                let initialize = (|| -> Result<Value, String> {
                    // The agent does not answer until it has finished loading,
                    // so every handshake request gets the startup timeout.
                    // Negotiate before anything else so a large first response
                    // arrives chunked rather than shrunk to an error frame.
                    if flavor.negotiates_protocol_v2() {
                        send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "negotiate_protocol", "protocolVersion": 2}),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    let _ = send_request_with_timeout(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                        INITIALIZE_TIMEOUT,
                    )?;
                    if let Some(session_file) = resume_session_file {
                        let response = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "switch_session",
                                "sessionPath": session_file
                            }),
                            INITIALIZE_TIMEOUT,
                        )?;
                        if response.pointer("/data/cancelled").and_then(Value::as_bool)
                            == Some(true)
                        {
                            return Err(format!(
                                "{} session switch was cancelled",
                                flavor.display_name()
                            ));
                        }
                    }
                    if let Some(model) = model.as_deref() {
                        let (provider, model_id) =
                            parse_model_slug(model).map_err(|error| error.to_string())?;
                        let _ = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "set_model",
                                "provider": provider,
                                "modelId": model_id
                            }),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    if let Some(level) = reasoning_effort.as_deref() {
                        let _ = send_request_with_timeout(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "set_thinking_level", "level": level}),
                            INITIALIZE_TIMEOUT,
                        )?;
                    }
                    send_request_with_timeout(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                        INITIALIZE_TIMEOUT,
                    )
                })();

                let state = match initialize {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = writer_events.send(DriverEvent::Error(tr!(
                            "errors.initialize_provider",
                            provider = flavor.display_name(),
                            error = error
                        )));
                        let _ = writer_events.send(DriverEvent::TurnFinished {
                            interrupted: false,
                            success: false,
                            summary: Some(tr!(
                                "errors.provider_initialize_session",
                                provider = flavor.display_name()
                            )),
                        });
                        return;
                    }
                };
                let Some(mut cursor) = cursor_from_state(flavor, &state) else {
                    let _ = writer_events.send(DriverEvent::Error(tr!(
                        "errors.provider_no_session_id",
                        provider = flavor.display_name()
                    )));
                    let _ = writer_events.send(DriverEvent::TurnFinished {
                        interrupted: false,
                        success: false,
                        summary: Some(tr!(
                            "errors.provider_initialize_session",
                            provider = flavor.display_name()
                        )),
                    });
                    return;
                };
                let initial_usage = send_request(
                    &mut stdin,
                    &writer_pending,
                    &mut next_request_id,
                    json!({"type": "get_session_stats"}),
                )
                .ok()
                .and_then(|stats| pi_context_usage(&state, Some(&stats)))
                .or_else(|| pi_context_usage(&state, None));
                let _ = writer_events.send(DriverEvent::Connected {
                    provider_cursor: Some(cursor.clone()),
                });
                if let Some((context_tokens, context_window)) = initial_usage {
                    let _ = writer_events.send(DriverEvent::UsageUpdated {
                        context_tokens,
                        context_window,
                    });
                }
                if let Some(title) = state
                    .pointer("/data/sessionName")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    let _ =
                        writer_events.send(DriverEvent::AutoTitleUpdated(Some(title.to_owned())));
                }

                // Pi reports extension commands, prompts and skills on request;
                // Oh My Pi pushes its registry at startup and after reloads.
                if flavor == PiFlavor::Pi
                    && let Ok(catalog) = send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_commands"}),
                    )
                {
                    publish_commands(
                        crate::slash_command_catalog::parse_pi_commands(&catalog),
                        &writer_events,
                    );
                }

                // Both flavors expose setters for these, so changing either is
                // an RPC on the live session rather than a restart.
                let mut current_model = model;
                let mut current_effort = reasoning_effort;
                while let Ok(message) = command_rx.recv() {
                    match message {
                        CommandMessage::Prompt(prompt) => {
                            dispatch_prompt(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &writer_events,
                                &run,
                                flavor,
                                prompt,
                            );
                        }
                        CommandMessage::Steer(prompt) => {
                            send_steer(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &writer_events,
                                &run,
                                flavor,
                                prompt,
                            );
                        }
                        CommandMessage::Cancel => {
                            if stop_session(&mut stdin, &writer_events, flavor).is_err() {
                                break;
                            }
                        }
                        CommandMessage::Options(options) => {
                            if options.model != current_model {
                                match options.model.as_deref().map(parse_model_slug).transpose() {
                                    Ok(Some((provider, model_id))) => {
                                        match send_request(
                                            &mut stdin,
                                            &writer_pending,
                                            &mut next_request_id,
                                            json!({
                                                "type": "set_model",
                                                "provider": provider,
                                                "modelId": model_id
                                            }),
                                        ) {
                                            Ok(response) => {
                                                if let Some(window) = response
                                                    .pointer("/data/contextWindow")
                                                    .and_then(Value::as_u64)
                                                    .filter(|window| *window > 0)
                                                {
                                                    let _ = writer_events.send(
                                                        DriverEvent::UsageUpdated {
                                                            context_tokens: None,
                                                            context_window: Some(window),
                                                        },
                                                    );
                                                }
                                            }
                                            Err(error) => {
                                                let _ =
                                                    writer_events.send(DriverEvent::Error(tr!(
                                                        "errors.switch_provider_model",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        let _ = writer_events
                                            .send(DriverEvent::Error(error.to_string()));
                                    }
                                }
                                current_model = options.model;
                            }
                            if options.reasoning_effort != current_effort {
                                if let Some(level) = options.reasoning_effort.as_deref()
                                    && let Err(error) = send_request(
                                        &mut stdin,
                                        &writer_pending,
                                        &mut next_request_id,
                                        json!({"type": "set_thinking_level", "level": level}),
                                    )
                                {
                                    let _ = writer_events.send(DriverEvent::Error(tr!(
                                        "errors.change_provider_thinking",
                                        provider = flavor.display_name(),
                                        error = error
                                    )));
                                }
                                current_effort = options.reasoning_effort;
                            }
                        }
                        CommandMessage::RetractQueuedMessages => {
                            // The queue report already handed the text back, so
                            // the answer is not read; it only has to leave the
                            // provider's queue before the next prompt is written.
                            if write_clear_queue(&mut stdin).is_err() {
                                break;
                            }
                        }
                        CommandMessage::CancelExtensionRequest(id) => {
                            if write_json_line(
                                &mut stdin,
                                &json!({
                                    "type": "extension_ui_response",
                                    "id": id,
                                    "cancelled": true
                                }),
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        CommandMessage::ExtensionUiResponse(response) => {
                            if write_json_line(&mut stdin, &response).is_err() {
                                break;
                            }
                        }
                        CommandMessage::Rollback { turns, response } => {
                            let result = fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &binary,
                                &cwd,
                                &cursor,
                                turns,
                                false,
                            );
                            if let Ok(next_cursor) = &result {
                                cursor = next_cursor.clone();
                            }
                            let _ = response.send(result);
                        }
                        CommandMessage::Fork {
                            turns_to_remove,
                            response,
                        } => {
                            let result = fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &binary,
                                &cwd,
                                &cursor,
                                turns_to_remove,
                                true,
                            );
                            let _ = response.send(result);
                        }
                        CommandMessage::Shutdown => break,
                    }
                }
            })?;

        let last_visible_stderr = Arc::new(Mutex::new(None::<String>));
        let stderr_last_error = last_visible_stderr.clone();
        let stderr_events = events.clone();
        let stderr_thread =
            thread::Builder::new()
                .name("waku-pi-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if line.to_ascii_lowercase().contains("error") {
                            let error = format!("{}: {}", flavor.display_name(), line.trim());
                            *stderr_last_error.lock() = Some(error.clone());
                            let _ = stderr_events.send(DriverEvent::Error(error));
                        }
                    }
                })?;

        // Nothing signals or kills the agent process: it exits when the writer
        // thread drops its stdin. Something still has to reap it, or every
        // session that ever ran leaves a zombie behind for the life of the app.
        thread::Builder::new()
            .name("waku-pi-process".into())
            .spawn(move || {
                let status = child.wait();
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                match status {
                    Ok(status) if !status.success() && last_visible_stderr.lock().is_none() => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.provider_rpc_exited",
                            provider = flavor.display_name(),
                            status = status
                        )));
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.read_provider_exit_status",
                            provider = format!("{} RPC", flavor.display_name()),
                            error = error
                        )));
                    }
                    _ => {}
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            flavor,
            commands,
            computer_use,
            dialogs,
        })
    }
}

impl DriverControl for PiDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn supports_steer(&self) -> bool {
        true
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn cancel_computer_use(&self) {
        if let Some(computer_use) = self.computer_use.as_ref() {
            computer_use.stop();
        }
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn respond_user_input(&self, request_id: String, answers: Vec<UserInputAnswer>) {
        // Only a dialog the user is looking at can be answered. An answer to
        // anything else, or a second answer to the same request, is dropped
        // rather than written as a response the provider never asked for.
        let Some(dialog) = self.dialogs.lock().remove(&request_id) else {
            return;
        };
        // A Pi dialog asks one question, and its id is the request's own —
        // the driver put it there — so the answer list holds exactly that
        // question's labels, empty when the user dismissed it.
        let labels = answers
            .into_iter()
            .next()
            .map(|answer| answer.answers)
            .unwrap_or_default();
        let _ = self.commands.send(CommandMessage::ExtensionUiResponse(
            dialog.response(&request_id, &labels),
        ));
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        // Both flavors have setters for the model and thinking level, so those
        // apply to the live session. Neither exposes one for permissions — and
        // Waku only runs them with Full access anyway, so a mode change asks
        // for a fresh start, which is where that is reported.
        if options.mode != RuntimeMode::FullAccess {
            return false;
        }
        self.commands.send(CommandMessage::Options(options)).is_ok()
    }

    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if turns == 0 {
            return Ok(None);
        }
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Rollback {
                turns,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before rollback",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation rollback",
                    self.flavor.display_name()
                )
            })?
            .map(Some)
            .map_err(anyhow::Error::msg)
    }

    fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Fork {
                turns_to_remove,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before forking",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation fork",
                    self.flavor.display_name()
                )
            })?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for PiDriver {
    fn drop(&mut self) {
        self.cancel_computer_use();
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

fn send_request(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    request: Value,
) -> Result<Value, String> {
    send_request_with_timeout(stdin, pending, next_request_id, request, RPC_TIMEOUT)
}

fn send_request_with_timeout(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    mut request: Value,
    timeout: Duration,
) -> Result<Value, String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    request["id"] = Value::String(id.clone());
    let (response_tx, response_rx) = bounded(1);
    pending
        .lock()
        .insert(id.clone(), PendingResponse::Request(response_tx));
    if let Err(error) = write_json_line(stdin, &request) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    match response_rx.recv_timeout(timeout) {
        Ok(response) => response,
        Err(_) => {
            pending.lock().remove(&id);
            Err(format!(
                "{} timed out after {}s",
                request["type"].as_str().unwrap_or("request"),
                timeout.as_secs()
            ))
        }
    }
}

fn send_prompt(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    prompt: &str,
) -> Result<(), String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    {
        let mut pending = pending.lock();
        // A response from an older prompt cannot settle the next turn.
        pending.retain(|_, response| matches!(response, PendingResponse::Request(_)));
        pending.insert(id.clone(), PendingResponse::Prompt);
    }
    // OMP built-ins can hold the prompt response until compaction or another
    // command finishes. Do not apply the short control-RPC timeout or block
    // the writer from sending abort while waiting for that response.
    //
    // Pi rejects a prompt outright while it is still streaming — "Agent is
    // already processing. Specify streamingBehavior ('steer' or 'followUp') to
    // queue the message." — and Waku can prompt into that state: stopping a
    // turn settles it here immediately, while Pi keeps streaming until its
    // own abort finishes unwinding. Pi reads the option only while it is
    // streaming, so one constant covers both cases: a prompt against an idle
    // agent starts a run as before, and a submission that races the tail of
    // the previous turn is queued and delivered — inside that run when it
    // reaches a boundary, as its own run when an abort ended the first —
    // rather than failed into the transcript.
    if let Err(error) = write_json_line(
        stdin,
        &json!({
            "id": id,
            "type": "prompt",
            "message": prompt,
            "streamingBehavior": "followUp",
        }),
    ) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    Ok(())
}

/// The compaction this prompt asks this flavour to run, when it is one.
///
/// Pi's built-in slash commands are the interactive CLI's dispatch, so the
/// transport is what recognises the invocation; Oh My Pi's RPC surface has no
/// verified compaction command and keeps the prompt.
fn pi_compact_invocation(
    flavor: PiFlavor,
    prompt: &str,
) -> Option<waku_protocol::composer::CompactInvocation> {
    if flavor != PiFlavor::Pi {
        return None;
    }
    waku_protocol::composer::parse_compact_invocation(prompt)
}

/// Delivers a submitted prompt, or runs the one provider command Waku bridges
/// in its place.
///
/// Pi's own slash commands are the interactive CLI's dispatch; its RPC prompt
/// path expands extension commands, skills and prompt templates and nothing
/// else, so a typed `/compact` would otherwise land in the model's context as
/// literal text. The transport recognises the invocation here — where Pi
/// itself dispatches built-ins, ahead of extension commands — and writes the
/// compaction request instead.
///
/// A live run is never aborted for it: Pi's `session.compact()` aborts the
/// agent operation before it starts, so a compaction that races a run is
/// reported as that submission's delivery failure, exactly like a refused
/// prompt, rather than stopping work the user did not stop.
fn dispatch_prompt(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    events: &impl DriverEventSink,
    run: &RunLiveness,
    flavor: PiFlavor,
    prompt: String,
) {
    let delivered = if let Some(invocation) = pi_compact_invocation(flavor, &prompt) {
        if run.is_live() {
            Err(tr!("errors.compact_turn_running"))
        } else {
            write_compact(stdin, invocation.instructions())
        }
    } else {
        send_prompt(stdin, pending, next_request_id, &prompt)
    };
    if let Err(error) = delivered {
        // The message never reached the provider, so this is its delivery
        // failure: the reason settles the turn with it, rather than arriving
        // as an error the app would render as its answer.
        let _ = events.send(DriverEvent::TurnFinished {
            interrupted: false,
            success: false,
            summary: Some(tr!(
                "errors.provider_rejected_prompt_detail",
                provider = flavor.display_name(),
                error = error
            )),
        });
    }
}

/// Writes the provider's compaction request.
///
/// No request id, like `abort` and `clear_queue`: every outcome the provider
/// reports — progress, usage and the end itself — arrives as an event, so
/// there is nothing to await. Summarizing a full context routinely outlasts
/// the control timeout besides, and the writer has to stay free for the stop
/// that cancels it.
fn write_compact(stdin: &mut impl Write, instructions: Option<String>) -> Result<(), String> {
    let mut request = json!({"type": "compact"});
    if let Some(instructions) = instructions {
        request["customInstructions"] = Value::String(instructions);
    }
    write_json_line(stdin, &request).map_err(|error| format!("transport write failed: {error}"))
}

/// Hands a steering message to the provider.
///
/// The provider queues a steer whether or not a run is open, and a message it
/// parks there is spliced into the boundary of whatever turn runs next — a
/// message landing in the middle of a conversation it did not belong to. So the
/// steer record is written only for the run that is still live; with no run to
/// join, the message takes the prompt path and is delivered as the next turn
/// instead. Both flavors acknowledge the same way: accepted once the message is
/// with the provider, rejected when the write failed. A converted steer is
/// acknowledged as accepted too, because that is what the app needs to keep the
/// message in the transcript — reporting it as a rejected steer would have the
/// app submit the same text a second time.
///
/// A Pi `/compact` is not a steer at all: it is a command, and the transport
/// runs it itself. Steering it would write the text into the model's context
/// or park it across turn boundaries, so it is rejected here — the client
/// queues the message instead, and the queue runs it through the prompt path
/// where the command is recognised.
fn send_steer(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    events: &impl DriverEventSink,
    run: &RunLiveness,
    flavor: PiFlavor,
    prompt: String,
) {
    if pi_compact_invocation(flavor, &prompt).is_some() {
        let _ = events.send(DriverEvent::SteerRejected {
            message: prompt,
            reason: tr!("errors.compact_queued_until_settled"),
        });
        return;
    }
    let delivered = if run.is_live() {
        send_request(
            stdin,
            pending,
            next_request_id,
            json!({"type": "steer", "message": prompt}),
        )
        .map(|_| ())
    } else {
        send_prompt(stdin, pending, next_request_id, &prompt)
    };
    match delivered {
        Ok(_) => {
            let _ = events.send(DriverEvent::SteerAccepted { message: prompt });
        }
        Err(error) => {
            let _ = events.send(DriverEvent::SteerRejected {
                message: prompt,
                reason: error,
            });
        }
    }
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Takes back the messages the provider's queue still holds and returns their
/// text. No request id: the answer is not awaited, because the reader thread
/// owns stdout and would otherwise deadlock waiting for it.
fn write_clear_queue(writer: &mut impl Write) -> std::io::Result<()> {
    write_json_line(writer, &json!({"type": "clear_queue"}))
}

/// Stops the run: the queue first, then the abort. The order is the point —
/// an abort continues whatever the queue still holds, so a message the user
/// stopped would run afterwards if the abort went first.
///
/// The abort carries no request id because it is never awaited: pi answers it
/// only once the session is idle, which routinely outlasts the control timeout
/// the other requests use. A waiter would report a slow stop as a transport
/// error and hold the next prompt behind it, while the run's own settlement is
/// what ends the turn either way.
///
/// A write that fails at all is reported and returned so the command loop can
/// end, exactly as a dead pipe does for its other commands.
fn stop_session(
    writer: &mut impl Write,
    events: &impl DriverEventSink,
    flavor: PiFlavor,
) -> std::io::Result<()> {
    let stopped =
        write_clear_queue(writer).and_then(|()| write_json_line(writer, &json!({"type": "abort"})));
    if let Err(error) = stopped {
        let _ = events.send(DriverEvent::Error(tr!(
            "errors.stop_provider",
            provider = flavor.display_name(),
            error = error
        )));
        return Err(error);
    }
    Ok(())
}

fn fail_pending(pending: &PendingResponses, message: &str) {
    for (_, response) in pending.lock().drain() {
        if let PendingResponse::Request(response) = response {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

fn parse_model_slug(model: &str) -> anyhow::Result<(&str, &str)> {
    let Some((provider, model_id)) = model.trim().split_once('/') else {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    };
    if provider.is_empty() || model_id.is_empty() {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    }
    Ok((provider, model_id))
}

fn cursor_from_state(flavor: PiFlavor, response: &Value) -> Option<ProviderResumeCursor> {
    let session_id = response
        .pointer("/data/sessionId")
        .and_then(Value::as_str)?;
    let session_file = response
        .pointer("/data/sessionFile")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Some(flavor.cursor(session_id.to_owned(), session_file))
}

/// Reassembles the `rpc_chunk` runs Oh My Pi emits for frames over its 1 MiB
/// stdout ceiling. Without this a large tool result degrades to an error frame
/// and the activity row renders empty.
#[derive(Default)]
struct ChunkAssembly {
    active: Option<PendingChunks>,
}

struct PendingChunks {
    chunk_id: String,
    count: u64,
    next_index: u64,
    byte_length: usize,
    data: Vec<u8>,
}

impl ChunkAssembly {
    /// Returns the logical message to dispatch, or `None` while a chunked
    /// frame is still arriving.
    fn accept(&mut self, value: Value) -> Result<Option<Value>, String> {
        if value.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            // The run must be uninterrupted, so anything else invalidates a
            // partial frame rather than silently splicing around it.
            if self.active.take().is_some() {
                return Err("chunked frame was interrupted".to_owned());
            }
            return Ok(Some(value));
        }
        let (chunk_id, index, count, byte_length, data) = (|| {
            Some((
                value.get("chunkId").and_then(Value::as_str)?,
                value.get("index").and_then(Value::as_u64)?,
                value.get("count").and_then(Value::as_u64)?,
                value.get("byteLength").and_then(Value::as_u64)?,
                value.get("data").and_then(Value::as_str)?,
            ))
        })()
        .ok_or_else(|| "chunk frame was malformed".to_owned())?;
        let byte_length = usize::try_from(byte_length)
            .map_err(|_| "chunked frame exceeds the reassembly limit".to_owned())?;
        if count == 0 || index >= count {
            self.active = None;
            return Err("chunk frame was malformed".to_owned());
        }
        if byte_length > MAX_REASSEMBLED_FRAME_BYTES {
            self.active = None;
            return Err("chunked frame exceeds the reassembly limit".to_owned());
        }
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|error| format!("chunk payload was not valid base64: {error}"))?;

        let pending = match self.active.take() {
            Some(pending)
                if pending.chunk_id == chunk_id
                    && pending.count == count
                    && pending.byte_length == byte_length
                    && pending.next_index == index =>
            {
                pending
            }
            Some(_) => {
                return Err("chunked frame was interrupted".to_owned());
            }
            None if index == 0 => PendingChunks {
                chunk_id: chunk_id.to_owned(),
                count,
                next_index: 0,
                byte_length,
                data: Vec::with_capacity(byte_length),
            },
            None => return Err("chunked frame started mid-sequence".to_owned()),
        };
        let mut pending = pending;
        pending.data.extend_from_slice(&decoded);
        pending.next_index += 1;
        if pending.data.len() > pending.byte_length {
            return Err("chunked frame overran its declared length".to_owned());
        }
        if pending.next_index < pending.count {
            self.active = Some(pending);
            return Ok(None);
        }
        if pending.data.len() != pending.byte_length {
            return Err("chunked frame did not match its declared length".to_owned());
        }
        let text = String::from_utf8(pending.data)
            .map_err(|_| "chunked frame was not valid UTF-8".to_owned())?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("chunked frame was not valid JSON: {error}"))
    }
}

/// One row of Pi's compaction activity.
fn compaction_activity(
    id: String,
    title: String,
    output: Option<&Value>,
    failed: bool,
    complete: bool,
) -> ActivityItem {
    activity::tool_activity(
        Some(id),
        ActivityKind::Tool,
        title,
        None,
        output,
        None,
        failed,
        complete,
    )
}

/// Reports the context occupancy the provider just computed. The window is
/// unchanged, and `None` keeps the one the meter already has.
fn report_context_tokens(events: &impl DriverEventSink, tokens: u64) {
    let _ = events.send(DriverEvent::UsageUpdated {
        context_tokens: Some(tokens),
        context_window: None,
    });
}

/// Pi already computes context occupancy for its own footer. Prefer that
/// native value when session stats are available, and use the active model in
/// `get_state` for the window before the first assistant message arrives.
fn pi_context_usage(state: &Value, stats: Option<&Value>) -> Option<(Option<u64>, Option<u64>)> {
    let context = stats.and_then(|stats| stats.pointer("/data/contextUsage"));
    let tokens = context
        .and_then(|context| context.get("tokens"))
        .and_then(Value::as_u64);
    let window = context
        .and_then(|context| context.get("contextWindow"))
        .and_then(Value::as_u64)
        .or_else(|| {
            state
                .pointer("/data/model/contextWindow")
                .and_then(Value::as_u64)
        })
        .filter(|window| *window > 0);
    (tokens.is_some() || window.is_some()).then_some((tokens, window))
}

/// Pi's providers normally fill `totalTokens`, but Pi itself deliberately
/// falls back to the four component counters when a provider leaves it zero.
/// Keep Waku's meter aligned with that provider-native calculation.
fn pi_message_context_tokens(message: &Value) -> Option<u64> {
    let usage = message.get("usage")?;
    usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .or_else(|| {
            let total = ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0_u64, u64::saturating_add);
            (total > 0).then_some(total)
        })
}

#[allow(clippy::too_many_arguments)]
fn fork_pi_session(
    flavor: PiFlavor,
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    binary: &Path,
    cwd: &Path,
    original_cursor: &ProviderResumeCursor,
    turns_to_remove: usize,
    restore_original: bool,
) -> Result<ProviderResumeCursor, String> {
    let name = flavor.display_name();
    let messages = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": flavor.branch_messages_command()}),
    )?;
    let messages = messages
        .pointer("/data/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{name} returned an invalid fork-message list"))?;

    // Keeping every turn is a whole-session copy, which Pi does in place and
    // Oh My Pi only does at launch. The out-of-process copy leaves this
    // session untouched, so it never needs restoring afterwards.
    if turns_to_remove == 0 && flavor == PiFlavor::OhMyPi {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| format!("{name}'s original session file is unavailable"))?;
        return clone_ohmypi_session(binary, cwd, session_file);
    }

    let request = pi_fork_request(flavor, messages, turns_to_remove)?;
    let fork = send_request(stdin, pending, next_request_id, request)?;
    if fork.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
        return Err(format!("{name} session fork was cancelled"));
    }
    let fork_state = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": "get_state"}),
    )?;
    let fork_cursor = cursor_from_state(flavor, &fork_state)
        .ok_or_else(|| format!("{name} did not report the forked session cursor"))?;

    if restore_original {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| format!("{name}'s original session file is unavailable"))?;
        let switched = send_request(
            stdin,
            pending,
            next_request_id,
            json!({
                "type": "switch_session",
                "sessionPath": session_file
            }),
        )?;
        if switched.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(format!(
                "{name} could not return to the source session after forking"
            ));
        }
        let restored_state = send_request(
            stdin,
            pending,
            next_request_id,
            json!({"type": "get_state"}),
        )?;
        let restored_cursor = cursor_from_state(flavor, &restored_state)
            .ok_or_else(|| format!("{name} did not report the restored source session"))?;
        if restored_cursor.native_id() != original_cursor.native_id() {
            return Err(format!(
                "{name} returned to the wrong source session after forking"
            ));
        }
    }

    Ok(fork_cursor)
}

fn pi_fork_request(
    flavor: PiFlavor,
    messages: &[Value],
    turns_to_remove: usize,
) -> Result<Value, String> {
    let name = flavor.display_name();
    if turns_to_remove > messages.len() {
        return Err(format!(
            "{name} has only {} native turns, but Waku needs to remove {turns_to_remove}",
            messages.len()
        ));
    }
    let retained_turns = messages.len() - turns_to_remove;
    if turns_to_remove == 0 {
        // Only reachable for Pi; Oh My Pi copies out of process instead.
        Ok(json!({"type": "clone"}))
    } else {
        let entry_id = messages
            .get(retained_turns)
            .and_then(|message| message.get("entryId"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} returned a fork message without an entry ID"))?;
        Ok(json!({"type": flavor.branch_command(), "entryId": entry_id}))
    }
}

/// Copies a whole Oh My Pi session by launching a throwaway agent with
/// `--fork`, which is the only place it exposes a full-session copy, then
/// reading back the session the copy landed in.
fn clone_ohmypi_session(
    binary: &Path,
    cwd: &Path,
    session_file: &Path,
) -> Result<ProviderResumeCursor, String> {
    let mut command = crate::command_env::command(binary);
    let command = command
        .args(["--mode", "rpc"])
        .arg(PiFlavor::OhMyPi.full_access_arg())
        .arg("--fork")
        .arg(session_file)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::command_env::spawn(command)
        .map_err(|error| format!("could not start Oh My Pi to copy the session: {error}"))?;
    let result = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Oh My Pi stdin unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Oh My Pi stdout unavailable".to_owned())?;
        let (tx, rx) = bounded(1);
        thread::Builder::new()
            .name("waku-ohmypi-clone".into())
            .spawn(move || {
                let mut chunks = ChunkAssembly::default();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Ok(Some(value)) = chunks.accept(value) else {
                        continue;
                    };
                    if value.get("id").and_then(Value::as_str) == Some("waku-clone") {
                        let _ = tx.send(value);
                        break;
                    }
                }
            })
            .map_err(|error| format!("could not read the Oh My Pi session copy: {error}"))?;
        write_json_line(
            &mut stdin,
            &json!({"id": "waku-clone", "type": "get_state"}),
        )
        .map_err(|error| format!("could not ask Oh My Pi for the copied session: {error}"))?;
        let state = rx
            .recv_timeout(CLONE_TIMEOUT)
            .map_err(|_| "timed out waiting for Oh My Pi to copy the session".to_owned())?;
        if state.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Oh My Pi could not copy the session")
                .to_owned());
        }
        cursor_from_state(PiFlavor::OhMyPi, &state)
            .ok_or_else(|| "Oh My Pi did not report the copied session cursor".to_owned())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Whether the provider has a run open right now.
///
/// The reader thread owns the value — the provider's own run-start and
/// settlement events are what change it — and the writer thread holds a clone,
/// because only a live run may be offered a steering message.
#[derive(Clone, Default)]
struct RunLiveness(Arc<AtomicBool>);

impl RunLiveness {
    fn is_live(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn open(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn close(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct PiStreamState {
    /// The provider's run. Shared with the writer thread, so a reset of the
    /// per-run stream state keeps the same handle rather than a fresh one.
    run: RunLiveness,
    /// The extension dialogs still unanswered, shared with the driver handle
    /// that answers them; like the run handle it outlives a run's reset.
    dialogs: PiDialogs,
    message_saw_text: bool,
    message_saw_reasoning: bool,
    failed: bool,
    tools: HashMap<String, (ActivityKind, String)>,
    /// The messages the provider's last queue report still held, in the order
    /// it reported them. Empty on a provider that reports no queue.
    queued: Vec<String>,
    /// The activity row of the compaction currently running, so its end event
    /// completes the same row. Pi names no compaction, so Waku counts them.
    open_compaction: Option<String>,
    compaction_sequence: u64,
}

impl PiStreamState {
    /// Clears the per-run stream state between runs. The run handle and the
    /// dialogs survive, because the writer thread reads liveness through its
    /// own clone of the run and a dialog the client has not answered yet must
    /// not be forgotten. The compaction counter survives too: a row's id is
    /// matched against every row this session already has, so a second
    /// compaction must not reuse the first one's id even in a later turn.
    fn reset(&mut self) {
        let run = self.run.clone();
        let dialogs = self.dialogs.clone();
        let compaction_sequence = self.compaction_sequence;
        *self = Self {
            run,
            dialogs,
            compaction_sequence,
            ..Self::default()
        };
    }
}

fn handle_pi_message(
    flavor: PiFlavor,
    value: Value,
    pending: &PendingResponses,
    commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response" {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            return;
        };
        let prompt_response = matches!(pending.lock().get(id), Some(PendingResponse::Prompt));
        if prompt_response {
            let success = value.get("success").and_then(Value::as_bool) == Some(true);
            // Pi answers a prompt with what became of it: `started` and
            // `queued` mean the work is on its way and the run settles the
            // turn, while `handled` means an extension command or an input
            // handler consumed the prompt and no run will start for it, so the
            // turn settles here. Oh My Pi says the last part in two steps — an
            // acknowledgement, then a second response carrying
            // `agentInvoked: false` — which keeps that field as one more
            // locally-handled signal.
            let handled_locally = value.pointer("/data/disposition").and_then(Value::as_str)
                == Some("handled")
                || value.pointer("/data/agentInvoked").and_then(Value::as_bool) == Some(false);
            // A command that starts a run of its own — the shape an extension
            // uses to wake the session — has already announced that run by the
            // time this answer arrives, and the run settles its own turn.
            // Settling here would end a turn the run is still writing into and
            // then open a second one for the same work.
            if success && (!handled_locally || state.run.is_live()) {
                return;
            }
            pending.lock().remove(id);
            // A prompt answer never opens a turn. A run announces itself with
            // `agent_start`/`turn_start`, and the two answers that settle here
            // — a refusal and a locally handled command — have no run behind
            // them at all.
            //
            // A refusal is the delivery failure of the message that asked for
            // the run: it never reached the conversation, so the provider's
            // own reason travels as the settlement's summary, which is what
            // marks the message undelivered. Sending it as a transport error
            // instead would have the client store it as an answer to a
            // message the agent never saw.
            let summary = (!success).then(|| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        tr!(
                            "errors.provider_rejected_prompt",
                            provider = flavor.display_name()
                        )
                    })
            });
            let _ = events.send(DriverEvent::TurnFinished {
                interrupted: false,
                success,
                summary,
            });
            // No run stands behind this answer — a refusal, or an extension
            // command that consumed the prompt — so nothing is live until the
            // provider announces a run of its own.
            state.run.close();
            state.reset();
            return;
        }
        let Some(PendingResponse::Request(response)) = pending.lock().remove(id) else {
            return;
        };
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            let _ = response.send(Ok(value));
        } else {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} RPC command failed", flavor.display_name()));
            let _ = response.send(Err(error));
        }
        return;
    }

    if event_type == "available_commands_update" {
        publish_commands(
            crate::slash_command_catalog::parse_oh_my_pi_commands(&value),
            events,
        );
        return;
    }

    if event_type == flavor.session_info_event() {
        let title = value
            .get(flavor.session_info_title_field())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        return;
    }

    // Oh My Pi reuses `agent_end` for intermediate settles, flagging the real
    // one with `isTerminal`. Anything else here would end the turn early while
    // maintenance or async delivery still has work queued.
    if event_type == flavor.settled_event() {
        if value.get("isTerminal").and_then(Value::as_bool) == Some(false) {
            return;
        }
        if state.run.is_live() {
            pending
                .lock()
                .retain(|_, response| matches!(response, PendingResponse::Request(_)));
            // A settlement must not leave text parked in the provider's queue.
            // Measured against pi 1.0.0: an aborted run settles without
            // draining its queue, and pi does not run that message afterwards
            // — it splices it into whatever the user sends next. So the queue
            // is taken back and its text handed to the user, and the turn
            // still settles here, once and at once.
            let retracted = std::mem::take(&mut state.queued);
            if !retracted.is_empty() {
                let _ = commands.send(CommandMessage::RetractQueuedMessages);
                let _ = events.send(DriverEvent::QueuedMessagesRetracted {
                    messages: retracted,
                });
            }
            let success = !state.failed;
            let _ = events.send(DriverEvent::TurnFinished {
                interrupted: false,
                success,
                summary: (!success).then(|| {
                    tr!(
                        "errors.provider_complete_turn",
                        provider = flavor.display_name()
                    )
                }),
            });
        }
        // The run is over, so a steering message has nothing left to join,
        // and a dialog that was still unanswered belonged to it: the provider
        // resolves an abandoned dialog on its own timeout, and that request is
        // no longer answerable.
        state.run.close();
        state.dialogs.lock().clear();
        state.reset();
        return;
    }

    match event_type {
        "queue_update" => {
            // Each report is the provider's complete queue, so the client's
            // pending list is the provider's own rather than a guess.
            let steering = message_texts(value.get("steering"));
            let follow_up = message_texts(value.get("followUp"));
            state.queued = steering.iter().chain(&follow_up).cloned().collect();
            let _ = events.send(DriverEvent::ProviderQueue {
                steering,
                follow_up,
            });
        }
        "compaction_start" => {
            // Pi compacts both on request and on its own (the threshold or an
            // overflow). Both are the same two events, so both get the same
            // row; an automatic compaction belongs to the run already open.
            state.compaction_sequence += 1;
            let id = format!("pi-compaction-{}", state.compaction_sequence);
            state.open_compaction = Some(id.clone());
            let item =
                compaction_activity(id, tr!("activity.compacting_context"), None, false, false);
            let _ = events.send(DriverEvent::RichActivity(item));
        }
        "compaction_end" => {
            let manual = value.get("reason").and_then(Value::as_str) == Some("manual");
            let aborted = value.get("aborted").and_then(Value::as_bool) == Some(true);
            let result = value.get("result");
            let error = value
                .get("errorMessage")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .filter(|error| !error.trim().is_empty());
            let compaction_tokens = result
                .and_then(|result| result.get("estimatedTokensAfter"))
                .and_then(Value::as_u64);
            // The row belongs to the start that opened it. Its start can be
            // gone when a settlement reset the stream state while a compaction
            // was unwinding, and an end with no row completes nothing rather
            // than fabricating a finished one.
            if let Some(id) = state.open_compaction.take() {
                let item = if aborted {
                    // The stop that aborted it already said what happened; the
                    // row only has to stop being live.
                    compaction_activity(id, tr!("activity.compacting_context"), None, false, true)
                } else if let Some(result) = result {
                    // The summary is the compaction's own text; the row keeps
                    // it for the detail view.
                    compaction_activity(
                        id,
                        tr!("activity.compacted_context"),
                        result.get("summary"),
                        false,
                        true,
                    )
                } else {
                    let message = error.map(str::to_owned).unwrap_or_else(|| {
                        tr!(
                            "errors.provider_reported_error",
                            provider = flavor.display_name()
                        )
                    });
                    compaction_activity(
                        id,
                        tr!("activity.compaction_failed"),
                        Some(&Value::String(message)),
                        true,
                        true,
                    )
                };
                let _ = events.send(DriverEvent::RichActivity(item));
            }
            if let Some(tokens) = compaction_tokens {
                report_context_tokens(events, tokens);
            }
            if manual {
                // A manual compaction has no run behind it, so its end event
                // settles the turn that submitted it. A failed compaction is
                // still a completed turn: the failed row is the record, and
                // the task must not read as failed because maintenance did.
                let _ = events.send(DriverEvent::TurnFinished {
                    interrupted: aborted,
                    success: true,
                    summary: None,
                });
            }
        }
        "command_output" => {
            if let Some(text) = value
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                if !state.run.is_live() {
                    state.run.open();
                    let _ = events.send(DriverEvent::TurnStarted);
                }
                // Each command_output is a complete output block, unlike
                // assistant deltas. Keep successive reports separated.
                let _ = events.send(DriverEvent::TextDelta(format!("{text}\n\n")));
            }
        }
        "agent_start" | "turn_start" => {
            if !state.run.is_live() {
                state.run.open();
                state.failed = false;
                let _ = events.send(DriverEvent::TurnStarted);
            }
        }
        "message_start" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                state.message_saw_text = false;
                state.message_saw_reasoning = false;
            }
        }
        "message_update" => {
            let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
            match update.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        state.message_saw_text = true;
                        let _ = events.send(DriverEvent::TextDelta(delta.to_owned()));
                    }
                }
                Some("thinking_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        state.message_saw_reasoning = true;
                        let _ = events.send(DriverEvent::ReasoningDelta(delta.to_owned()));
                    }
                }
                Some("error") => {
                    state.failed = true;
                    let _ = events.send(DriverEvent::Error(pi_error_message(flavor, update)));
                }
                _ => {}
            }
        }
        "message_end" => {
            let message = value.get("message");
            match message
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
            {
                Some("assistant") => {
                    // This is the context the next call starts from, not the
                    // cumulative billed total for the whole session.
                    if let Some(tokens) = message.and_then(pi_message_context_tokens) {
                        report_context_tokens(events, tokens);
                    }
                    emit_completed_message_fallback(message, events, state);
                }
                Some("custom") => {
                    if let Some(message) = message {
                        emit_extension_message(message, events);
                    }
                }
                _ => {}
            }
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            let id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tool_name = value.get("toolName").and_then(Value::as_str);
            let (kind, mut title) = id
                .as_ref()
                .and_then(|id| state.tools.get(id))
                .cloned()
                .unwrap_or_else(|| {
                    tool_name
                        .map(|tool_name| (classify_tool(tool_name), tool_title(tool_name)))
                        .unwrap_or_else(|| (ActivityKind::Tool, tr!("activity.tool")))
                });
            if event_type == "tool_execution_start"
                && let Some(input_title) = activity::input_title(value.get("args"))
            {
                title = input_title;
            }
            if event_type == "tool_execution_start"
                && let Some(id) = id.as_ref()
            {
                state.tools.insert(id.clone(), (kind, title.clone()));
            }
            let arguments = (event_type == "tool_execution_start")
                .then(|| value.get("args"))
                .flatten();
            let output = match event_type {
                "tool_execution_update" => value.get("partialResult"),
                "tool_execution_end" => value.get("result"),
                _ => None,
            };
            let complete = event_type == "tool_execution_end";
            let failed = value.get("isError").and_then(Value::as_bool) == Some(true);
            let item = activity::tool_activity(
                id.clone(),
                kind,
                title,
                arguments,
                output,
                output,
                failed,
                complete,
            )
            .with_tool_name(tool_name);
            let _ = events.send(DriverEvent::RichActivity(item));
            if complete && let Some(id) = id {
                state.tools.remove(&id);
            }
        }
        "auto_retry_end" => {
            if value.get("success").and_then(Value::as_bool) == Some(true) {
                state.failed = false;
            } else {
                state.failed = true;
                let message = value
                    .get("finalError")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("{} exhausted its automatic retries", flavor.display_name())
                    });
                let _ = events.send(DriverEvent::Error(message));
            }
        }
        "extension_ui_request" => {
            let method = value.get("method").and_then(Value::as_str);
            let id = value.get("id").and_then(Value::as_str);
            // A dialog blocks the extension until the user answers it. It
            // becomes an ordinary user-input request; the answer comes back
            // through the transport's own response method, and the provider's
            // own timeout dismisses an abandoned one.
            if let (Some(dialog), Some(id)) = (method.and_then(PiExtensionDialog::from_method), id)
            {
                let question = dialog.question(id, &value);
                state.dialogs.lock().insert(id.to_owned(), dialog);
                let _ = events.send(DriverEvent::UserInputRequested {
                    request_id: id.to_owned(),
                    questions: vec![question],
                });
                return;
            }
            match method {
                // The rest are the extension's own status surfaces: none
                // expects an answer, so each is forwarded to the app instead
                // of being dropped.
                Some("notify") => {
                    if let Some(message) = value.get("message").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::ExtensionNotification {
                            message: message.to_owned(),
                            severity: pi_notification_severity(value.get("notifyType")),
                        });
                    }
                }
                Some("setStatus") => {
                    if let Some(key) = value.get("statusKey").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::ExtensionStatus {
                            key: key.to_owned(),
                            text: value
                                .get("statusText")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        });
                    }
                }
                Some("setWidget") => {
                    if let Some(key) = value.get("widgetKey").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::ExtensionWidget {
                            key: key.to_owned(),
                            // Pi sends the lines themselves in RPC mode, and
                            // their absence is the extension's own clear.
                            lines: value.get("widgetLines").and_then(Value::as_array).map(
                                |lines| {
                                    lines
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                },
                            ),
                            placement: match value.get("widgetPlacement").and_then(Value::as_str) {
                                Some("belowEditor") => ExtensionWidgetPlacement::BelowEditor,
                                _ => ExtensionWidgetPlacement::AboveEditor,
                            },
                        });
                    }
                }
                Some("setTitle") => {
                    if let Some(title) = value.get("title").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::ExtensionTitle {
                            title: title.to_owned(),
                        });
                    }
                }
                Some("set_editor_text") => {
                    if let Some(text) = value.get("text").and_then(Value::as_str) {
                        let _ = events.send(DriverEvent::ExtensionEditorText {
                            text: text.to_owned(),
                        });
                    }
                }
                // A method the client does not know is not one it can present,
                // and if it is a dialog the provider is blocking on it. Cancel
                // it so the extension continues instead of waiting out its
                // timeout; nothing else Pi sends needs an answer.
                _ => {
                    if let Some(id) = id {
                        let _ =
                            commands.send(CommandMessage::CancelExtensionRequest(id.to_owned()));
                    }
                }
            }
        }
        "extension_error" => {
            let _ = events.send(DriverEvent::Error(pi_error_message(flavor, &value)));
        }
        _ => {}
    }
}

/// The severity of an extension's notification. `notifyType` is optional, and
/// an omission means the same thing to Pi as it does here: informational.
fn pi_notification_severity(notify_type: Option<&Value>) -> NotificationSeverity {
    match notify_type.and_then(Value::as_str) {
        Some("warning") => NotificationSeverity::Warning,
        Some("error") => NotificationSeverity::Error,
        _ => NotificationSeverity::Info,
    }
}

/// The text entries of one queue in a `queue_update` report. Anything that is
/// not a string is not a message the client can show.
fn message_texts(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn publish_commands(
    commands: Vec<waku_protocol::composer::SlashCommand>,
    events: &impl DriverEventSink,
) {
    let commands = commands
        .into_iter()
        .map(|command| ReportedCommand {
            name: command.name,
            description: command.description,
        })
        .collect();
    let _ = events.send(DriverEvent::AvailableCommands(commands));
}

fn emit_extension_message(message: &Value, events: &impl DriverEventSink) {
    let custom_type = message
        .get("customType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = pi_custom_message_text(message.get("content"));
    let display = message
        .get("display")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    // pi-subagents reports its detached children through its own custom
    // messages. Those records are the only sign a background child settled,
    // and the client already has a surface for work that outlives the turn.
    // The flag decides the conversation rather than that surface: pi-subagents
    // marks a completed child not for display and a failed or stopped one for
    // display, and both are the same kind of event, so the child's outcome
    // goes to the detached-work surface either way and never becomes a chat
    // row.
    if let Some(item) = pi_subagent_background_item(custom_type, &text) {
        let _ = events.send(DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(
            item,
        )));
        return;
    }
    let _ = events.send(DriverEvent::ExtensionMessage {
        custom_type: custom_type.to_owned(),
        text,
        display,
    });
}

/// The custom message's text. Pi normalizes missing content to an empty array,
/// but a plain string is still a legal `CustomMessage` body, so both shapes are
/// decoded.
fn pi_custom_message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// pi-subagents' child and background notifications as detached work.
///
/// The two types are its own: a workflow child settling and a background task
/// finishing. Their first line names the child and its outcome — `Workflow
/// child completed: **build**` — so the item is named after the child, carries
/// the outcome as its status, and keeps the whole message as its detail;
/// anything else is an extension message with no detached work behind it.
///
/// The child's key is that line's bold span, and a child may be called
/// anything — `retry-failed` completes — so the outcome is read from the line
/// with the span left out.
fn pi_subagent_background_item(custom_type: &str, text: &str) -> Option<BackgroundWorkItem> {
    if !matches!(
        custom_type,
        "subagent-incremental-child-notify" | "subagent-notify"
    ) {
        return None;
    }
    let headline = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    // The child's key is the bold span; the outcome is read around it.
    let (before, rest) = headline.split_once("**").unwrap_or((headline, ""));
    let (key, after) = rest.split_once("**").unwrap_or(("", ""));
    let outcome = format!("{before}{after}");
    let status = if outcome.contains("failed") {
        BackgroundWorkStatus::Failed
    } else if outcome.contains("completed") {
        BackgroundWorkStatus::Completed
    } else if outcome.contains("stopped") {
        BackgroundWorkStatus::Stopped
    } else {
        // `paused` and anything a later version words differently: the
        // provider still holds the run, and the surface has no state of its
        // own for a child that needs attention.
        BackgroundWorkStatus::Running
    };
    let key = key.trim();
    let child = if key.is_empty() { headline } else { key };
    let mut item = BackgroundWorkItem::new(
        BackgroundWorkKind::Subagent,
        child.to_owned(),
        child.to_owned(),
        status,
    );
    item.background = true;
    item.detail = Some(text.to_owned());
    Some(item)
}

fn emit_completed_message_fallback(
    message: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let Some(content) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") if !state.message_saw_text => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    state.message_saw_text = true;
                    let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking") if !state.message_saw_reasoning => {
                if let Some(thinking) = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|thinking| !thinking.is_empty())
                {
                    state.message_saw_reasoning = true;
                    let _ = events.send(DriverEvent::ReasoningDelta(thinking.to_owned()));
                }
            }
            _ => {}
        }
    }
}

fn pi_error_message(flavor: PiFlavor, value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("errorMessage").and_then(Value::as_str))
        .or_else(|| value.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            tr!(
                "errors.provider_reported_error",
                provider = flavor.display_name()
            )
        })
}

fn classify_tool(name: &str) -> ActivityKind {
    ActivityKind::from_tool_name(name)
}

fn tool_title(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "bash" => tr!("activity.run_command"),
        "edit" => tr!("activity.edit_file"),
        "write" => tr!("activity.write_file"),
        "read" => tr!("activity.read_file"),
        "grep" => tr!("activity.search_files"),
        "find" => tr!("activity.find_files"),
        "ls" => tr!("activity.list_files"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::TryRecvError;

    #[test]
    fn live_command_updates_reach_the_composer_and_clear_removed_commands() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for entries in [
            json!([
                {"name": "compact", "source": "builtin", "description": "Compact context"},
                {"name": "skill:verify", "source": "skill", "description": "Verify changes"}
            ]),
            json!([]),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                json!({"type": "available_commands_update", "commands": entries}),
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        let DriverEvent::AvailableCommands(reported) = event_rx.recv().unwrap() else {
            panic!("missing command update")
        };
        assert_eq!(
            reported
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact", "verify"]
        );
        assert_eq!(reported[0].description, "Compact context");
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::AvailableCommands(commands) if commands.is_empty())
        );
        assert!(!state.run.is_live());
    }

    #[test]
    fn every_prompt_asks_the_provider_to_queue_it_while_streaming() {
        // Pi refuses a prompt that arrives while it is still streaming unless
        // the request says how to queue it, and Waku can prompt into exactly
        // that window: stopping a turn settles it here at once, while Pi keeps
        // streaming until its own abort finishes unwinding. Pi reads the
        // option only while it is streaming, so one constant covers both cases
        // and no branch of ours has to guess the provider's state.
        let (pending, _commands, _command_rx, _state) = harness();
        let mut next = 0;
        let mut wire = Vec::new();
        send_prompt(&mut wire, &pending, &mut next, "idle session").unwrap();
        // The second prompt goes out before the first has settled, which is
        // the window this option exists for.
        send_prompt(&mut wire, &pending, &mut next, "still streaming").unwrap();

        let requests: Vec<Value> = String::from_utf8(wire)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["message"], "idle session");
        assert_eq!(requests[1]["message"], "still streaming");
        for request in requests {
            assert_eq!(request["type"], "prompt");
            assert_eq!(request["streamingBehavior"], "followUp");
        }
    }

    #[test]
    fn a_prompt_the_provider_queued_waits_for_the_run() {
        // `queued` means the provider took the message for the turn that is
        // still running, so the answer itself settles nothing: the run settles
        // once its queue has drained.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "and also").unwrap();
        for frame in [
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true, "data": {"disposition": "queued"}}),
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn a_prompt_an_extension_handled_settles_without_a_run() {
        // An extension command runs inside the provider and starts no run, so
        // a client that waits for a settle would strand the turn forever.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "/mycommand").unwrap();
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true, "data": {"disposition": "handled"}}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished {
                success: true,
                summary: None,
                ..
            }
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "a handled prompt opens no turn of its own"
        );
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn a_command_that_starts_a_run_keeps_its_own_turn() {
        // The shape an extension command uses to wake the session: the handler
        // starts a run inline, so the run's start arrives before the answer
        // that says the prompt was handled. That run owns the turn; settling on
        // the answer would end it early and open a second one for the same
        // work, which the app renders as an empty completed turn.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "/council").unwrap();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true, "data": {"disposition": "handled"}}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            event_rx.try_recv().is_err(),
            "an answer settles nothing while the run it started is live"
        );
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "agent_settled"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(event_rx.try_recv().is_err(), "one turn, one settlement");
    }

    #[test]
    fn a_run_settles_the_turn_the_provider_never_answered() {
        // Pi writes no response at all for a prompt submitted while it is
        // emitting its settle, so settlement cannot depend on one arriving.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "hello").unwrap();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn the_provider_queue_report_reaches_the_stream() {
        // The client's pending list is the provider's own queue, so every
        // report is forwarded — each one is the complete queue, so the last
        // report wins.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "queue_update", "steering": ["stop"], "followUp": ["and also"]}),
            json!({"type": "queue_update", "steering": [], "followUp": []}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        let DriverEvent::ProviderQueue {
            steering,
            follow_up,
        } = event_rx.recv().unwrap()
        else {
            panic!("the provider's queue report should reach the stream");
        };
        assert_eq!(steering, ["stop"]);
        assert_eq!(follow_up, ["and also"]);
        let DriverEvent::ProviderQueue {
            steering,
            follow_up,
        } = event_rx.recv().unwrap()
        else {
            panic!("the drained report should reach the stream too");
        };
        assert!(steering.is_empty() && follow_up.is_empty());
        assert!(event_rx.try_recv().is_err(), "a report is one event");
    }

    #[test]
    fn retracting_a_queue_asks_the_provider_to_clear_it() {
        // `clear_queue` is what takes a parked message out of the provider's
        // queue. It carries no request id: the driver's reader thread owns
        // stdout, so the answer can never be awaited from there.
        let mut wire = Vec::new();
        write_clear_queue(&mut wire).unwrap();
        let request: Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(request["type"], "clear_queue");
        assert!(request.get("id").is_none());
    }

    #[test]
    fn stopping_a_turn_clears_the_queue_before_it_aborts_and_waits_for_nothing() {
        // Pi continues whatever its queue still holds when an abort lands, so
        // clearing it is what keeps a message the user stopped from running
        // afterwards: the clear has to be on the wire first. Pi answers
        // `abort` only once its session is idle, which routinely outlasts the
        // control timeout every other request uses, so nothing may wait on
        // that answer — a waiter would report the slow stop as a transport
        // error and hold the next prompt behind it.
        let (pending, _commands, _command_rx, _state) = harness();
        let (events, event_rx) = unbounded();
        let mut wire = Vec::new();
        let mut next_request_id = 0;

        stop_session(&mut wire, &events, PiFlavor::Pi).unwrap();
        send_prompt(&mut wire, &pending, &mut next_request_id, "and also").unwrap();

        assert!(
            event_rx.try_recv().is_err(),
            "a stop the provider never answers is not a transport error"
        );
        let writes = wire_lines(&wire);
        assert_eq!(writes.len(), 3, "a stop is the clear and the abort");
        assert_eq!(writes[0]["type"], "clear_queue");
        assert_eq!(writes[1]["type"], "abort");
        assert!(
            writes[1].get("id").is_none(),
            "an answer is correlated by id, so an id-less abort has none to await"
        );
        assert_eq!(
            writes[2]["type"], "prompt",
            "the next prompt is not held behind the stop"
        );
    }

    #[test]
    fn a_settlement_that_still_holds_a_queued_message_hands_it_back() {
        // Measured against pi 1.0.0: an aborted run settles without draining
        // its queue, and pi never runs that message afterwards — it splices
        // it into whatever the user sends next. A settlement therefore may
        // not leave text parked in the queue: it is cleared and handed back
        // to the user, and the turn still settles once, immediately.
        let (pending, commands, command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "queue_update", "steering": [], "followUp": ["and also"]}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ProviderQueue { .. }
        ));
        let DriverEvent::QueuedMessagesRetracted { messages } = event_rx.recv().unwrap() else {
            panic!("the settlement takes the queued message back")
        };
        assert_eq!(messages, ["and also"]);
        let DriverEvent::TurnFinished { success, .. } = event_rx.recv().unwrap() else {
            panic!("the turn still settles")
        };
        assert!(success, "the interruption is not a failure of this turn");
        assert!(
            event_rx.try_recv().is_err(),
            "the turn settles once, immediately"
        );
        assert!(matches!(
            command_rx.try_recv().unwrap(),
            CommandMessage::RetractQueuedMessages
        ));
    }

    #[test]
    fn a_message_delivered_at_the_boundary_is_never_retracted() {
        // The normal case: pi drains the queue into the running turn before it
        // settles, so the settlement sees no queue and touches nothing.
        let (pending, commands, command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "queue_update", "steering": [], "followUp": ["and also"]}),
            json!({"type": "turn_start"}),
            json!({"type": "queue_update", "steering": [], "followUp": []}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ProviderQueue { follow_up, .. } if follow_up == ["and also"]
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ProviderQueue { ref steering, ref follow_up } if steering.is_empty() && follow_up.is_empty()
        ));
        let DriverEvent::TurnFinished { success, .. } = event_rx.recv().unwrap() else {
            panic!("the delivered message's turn still settles");
        };
        assert!(success);
        assert!(matches!(command_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn local_slash_command_output_and_completion_do_not_need_an_agent_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        send_prompt(&mut Vec::new(), &pending, &mut 0, "/context").unwrap();
        for frame in [
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true}),
            json!({"type": "command_output", "text": "Available commands\n"}),
            json!({"type": "command_output", "text": "/compact"}),
            json!({"type": "response", "id": "waku-1", "command": "prompt", "success": true, "data": {"agentInvoked": false}}),
            json!({"type": "agent_end", "isTerminal": true}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "Available commands\n\n\n")
        );
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TextDelta(text) if text == "/compact\n\n")
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(event_rx.try_recv().is_err());
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn a_refused_prompt_settles_as_its_messages_delivery_failure() {
        // The provider refused the prompt before accepting it, so the message
        // never reached the conversation. The settlement says so, carrying the
        // provider's own reason; a transport error would be shown as a reply
        // to a message the agent never saw.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        let mut wire = Vec::new();
        let mut next_request_id = 0;
        send_prompt(&mut wire, &pending, &mut next_request_id, "run the tests").unwrap();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "response",
                "id": "waku-1",
                "success": false,
                "error": "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.",
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        let DriverEvent::TurnFinished {
            success,
            summary,
            interrupted,
        } = event_rx.recv().unwrap()
        else {
            panic!("a refused prompt settles the turn")
        };
        assert!(!success);
        assert!(!interrupted);
        assert_eq!(
            summary.as_deref(),
            Some(
                "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
            ),
            "the provider's own refusal is what the message went undelivered by"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "no error and no turn start: nothing ran, so nothing answers the message"
        );
        assert!(pending.lock().is_empty());
    }

    #[test]
    fn asynchronous_prompt_errors_settle_only_the_current_prompt() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        let mut next = 0;
        let mut wire = Vec::new();
        send_prompt(&mut wire, &pending, &mut next, "/old").unwrap();
        send_prompt(&mut wire, &pending, &mut next, "/new").unwrap();
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-1", "success": false, "error": "stale"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(event_rx.try_recv().is_err());
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "response", "id": "waku-2", "success": false, "error": "command failed"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(
            matches!(event_rx.recv().unwrap(), DriverEvent::TurnFinished { success: false, summary: Some(reason), .. } if reason == "command failed")
        );
        assert!(
            event_rx.try_recv().is_err(),
            "a refused prompt opens no turn of its own"
        );
        assert!(pending.lock().is_empty());
        assert_eq!(String::from_utf8(wire).unwrap().lines().count(), 2);
    }

    #[test]
    fn a_steer_that_misses_the_run_is_delivered_as_a_prompt() {
        // The provider queues a steer whether or not a run is open — measured
        // against pi 1.0.0, a message steered into an idle session waits in
        // the steering queue and is spliced into the boundary of whatever turn
        // runs next. A steer for a run that has already settled therefore goes
        // out as a prompt for the next turn instead, and still reaches the app
        // as an accepted message.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { .. }
        ));

        let mut wire = Vec::new();
        let mut next_request_id = 0;
        send_steer(
            &mut wire,
            &pending,
            &mut next_request_id,
            &events,
            &state.run,
            PiFlavor::Pi,
            "stop doing that".to_owned(),
        );

        let frames = wire_frames(&wire);
        assert_eq!(
            frames.len(),
            1,
            "a steer the provider would park is never written"
        );
        assert_eq!(frames[0]["type"], "prompt");
        assert_eq!(frames[0]["message"], "stop doing that");
        assert_eq!(
            frames[0]["streamingBehavior"], "followUp",
            "the next turn's prompt waits for the settled run the provider still owes"
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::SteerAccepted { message } if message == "stop doing that"
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn a_steer_into_the_live_run_is_written_as_a_steer() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));

        let frames = steered_frames(&pending, &events, &state.run, "stop doing that");

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], "steer");
        assert_eq!(frames[0]["message"], "stop doing that");
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::SteerAccepted { message } if message == "stop doing that"
        ));
    }

    #[test]
    fn oh_my_pi_keeps_its_steering_and_gates_on_its_own_run() {
        // The second flavor's steering is untouched: a steer into its live run
        // is the same record and the same acknowledgement it gets today. Its
        // run lifecycle is spelled differently (`agent_end`), and the gate
        // reads that lifecycle too, so a steer arriving after its run settled
        // takes the prompt path instead of waiting in the provider's queue.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));

        let live = steered_frames(&pending, &events, &state.run, "stop doing that");
        assert_eq!(live[0]["type"], "steer");
        assert_eq!(live[0]["message"], "stop doing that");
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::SteerAccepted { message } if message == "stop doing that"
        ));

        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "agent_end", "messages": []}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { .. }
        ));
        let mut wire = Vec::new();
        let mut next_request_id = 0;
        send_steer(
            &mut wire,
            &pending,
            &mut next_request_id,
            &events,
            &state.run,
            PiFlavor::Pi,
            "never mind".to_owned(),
        );
        let frames = wire_frames(&wire);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], "prompt");
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::SteerAccepted { message } if message == "never mind"
        ));
    }

    fn wire_frames(wire: &[u8]) -> Vec<Value> {
        String::from_utf8(wire.to_vec())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// What one steering message writes to the transport. The provider answers
    /// a request the way its RPC does, so the writer's own wait is satisfied.
    fn steered_frames(
        pending: &PendingResponses,
        events: &Sender<DriverEvent>,
        run: &RunLiveness,
        prompt: &str,
    ) -> Vec<Value> {
        let (wire_tx, wire_rx) = unbounded();
        let writer_pending = pending.clone();
        let writer_events = events.clone();
        let writer_run = run.clone();
        let writer_prompt = prompt.to_owned();
        let writer = thread::spawn(move || {
            let mut wire = WireRecorder::new(wire_tx);
            let mut next_request_id = 0;
            send_steer(
                &mut wire,
                &writer_pending,
                &mut next_request_id,
                &writer_events,
                &writer_run,
                PiFlavor::Pi,
                writer_prompt,
            );
        });
        let mut frames = Vec::new();
        while let Ok(bytes) = wire_rx.recv() {
            let frame: Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(id) = frame.get("id").and_then(Value::as_str)
                && let Some(PendingResponse::Request(response)) = pending.lock().remove(id)
            {
                let _ = response.send(Ok(json!({
                    "type": "response",
                    "id": id,
                    "success": true,
                    "data": {"disposition": "queued"},
                })));
            }
            frames.push(frame);
        }
        writer.join().unwrap();
        frames
    }

    /// Collects the lines a writer sends, so a test can answer them.
    struct WireRecorder {
        lines: Sender<Vec<u8>>,
        partial: Vec<u8>,
    }

    impl WireRecorder {
        fn new(lines: Sender<Vec<u8>>) -> Self {
            Self {
                lines,
                partial: Vec::new(),
            }
        }
    }

    impl Write for WireRecorder {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            // One frame is one line, but the transport writes it in as many
            // calls as it likes.
            self.partial.extend_from_slice(buffer);
            while let Some(end) = self.partial.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.partial.drain(..=end).collect();
                let _ = self.lines.send(line[..line.len() - 1].to_vec());
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn harness() -> (
        PendingResponses,
        Sender<CommandMessage>,
        crossbeam_channel::Receiver<CommandMessage>,
        PiStreamState,
    ) {
        let (commands, receiver) = unbounded();
        (
            Arc::new(Mutex::new(HashMap::new())),
            commands,
            receiver,
            PiStreamState::default(),
        )
    }

    /// A driver handle and the reader state that share one dialog store, so a
    /// dialog the reader opens is the one the handle answers, and the reply
    /// travels the transport's own command channel.
    struct DialogHarness {
        driver: PiDriver,
        pending: PendingResponses,
        commands: Sender<CommandMessage>,
        command_rx: crossbeam_channel::Receiver<CommandMessage>,
        events: Sender<DriverEvent>,
        event_rx: crossbeam_channel::Receiver<DriverEvent>,
        state: PiStreamState,
    }

    impl DialogHarness {
        fn new() -> Self {
            let (commands, command_rx) = unbounded();
            let dialogs: PiDialogs = Arc::new(Mutex::new(HashMap::new()));
            let (events, event_rx) = unbounded();
            let driver = PiDriver {
                flavor: PiFlavor::Pi,
                commands: commands.clone(),
                computer_use: None,
                dialogs: dialogs.clone(),
            };
            Self {
                driver,
                pending: Arc::new(Mutex::new(HashMap::new())),
                commands,
                command_rx,
                events,
                event_rx,
                state: PiStreamState {
                    dialogs,
                    ..PiStreamState::default()
                },
            }
        }

        /// Feeds one inbound frame the way the reader thread does.
        fn open(&mut self, request: Value) {
            handle_pi_message(
                PiFlavor::Pi,
                request,
                &self.pending,
                &self.commands,
                &self.events,
                &mut self.state,
            );
        }

        /// The single question the last `open` put in front of the client.
        fn question(&mut self) -> UserInputQuestion {
            let event = self
                .event_rx
                .try_recv()
                .expect("the dialog must reach the client");
            let DriverEvent::UserInputRequested {
                request_id,
                mut questions,
            } = event
            else {
                panic!("a dialog must arrive as a user-input request")
            };
            assert_eq!(questions.len(), 1, "a dialog is one question");
            assert_eq!(
                questions[0].id, request_id,
                "the question id is the provider's request id, so the answer correlates"
            );
            questions.remove(0)
        }
    }

    /// Drives one `extension_ui_request` frame through the inbound stream and
    /// returns the events it produced, in order.
    fn extension_ui_events(request: Value) -> Vec<DriverEvent> {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            request,
            &pending,
            &commands,
            &events,
            &mut state,
        );
        event_rx.try_iter().collect()
    }

    /// The commands a writer produced, in the order the provider reads them.
    fn wire_lines(wire: &[u8]) -> Vec<Value> {
        std::str::from_utf8(wire)
            .expect("the wire is utf-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each write is one JSON command"))
            .collect()
    }

    /// How long one live step may wait. A real turn answers in seconds, so the
    /// budget covers a cold model and a cold extension load rather than a slow
    /// transport, and it is per step rather than for a whole test.
    const LIVE_STEP_TIMEOUT: Duration = Duration::from_secs(90);

    /// A detached workflow has to spawn its own runner, start a child session
    /// and post the completion, so it gets a budget of its own.
    const LIVE_WORKFLOW_TIMEOUT: Duration = Duration::from_secs(240);

    /// The probe fixture as an extension Pi loads from a path of its own.
    ///
    /// A path per test, because Pi resolves a loaded extension by path: two
    /// sessions naming the same file would have the second read what the first
    /// compiled.
    fn live_probe_extension(name: &str) -> PathBuf {
        live_fixture(name, include_str!("fixtures/pi_live_probe.js"))
    }

    /// A fixture extension, written where Pi can load it.
    fn live_fixture(name: &str, source: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("waku-pi-live-{name}"));
        std::fs::create_dir_all(&directory).expect("the fixture extension needs a directory");
        let path = directory.join(format!("{name}.js"));
        std::fs::write(&path, source).expect("the fixture extension needs its body");
        path
    }

    /// A driver on a real `pi --mode rpc`, started the way the app starts one,
    /// with `extension` loaded through the launch's own `--extension` flag, and
    /// its native session already announced.
    ///
    /// `None` means this machine has no Pi, and the test that asked is skipped
    /// rather than failed: the provider is a real process with a real model
    /// behind it, and a suite that cannot run without one is a suite nobody
    /// runs. Pi itself has no such excuse when it is installed.
    fn real_pi_session(
        extension: Option<PathBuf>,
    ) -> Option<(PiDriver, crossbeam_channel::Receiver<DriverEvent>)> {
        let Some(binary) = crate::command_env::find_executable("pi") else {
            eprintln!("skipping a live Pi test: the `pi` CLI is not installed");
            return None;
        };
        let extensions = extension.into_iter().collect::<Vec<_>>();
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::launch(
            PiFlavor::Pi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            events,
            &extensions,
        )
        .expect("the Pi RPC session should start");
        loop {
            match next_live_step(&event_rx, "native session") {
                DriverEvent::Connected { .. } => break,
                DriverEvent::Error(error) => panic!("Pi failed to start: {error}"),
                _ => {}
            }
        }
        Some((driver, event_rx))
    }

    /// A run the extension starts on its own reaches the transcript: the
    /// message that caused it has a home, one turn opens with no Waku prompt,
    /// and the reply streams into that same turn — the shape a background
    /// subagent's completion wake takes.
    #[test]
    fn pi_streams_a_run_an_extension_starts_against_the_real_rpc() {
        let Some((driver, event_rx)) = real_pi_session(Some(live_probe_extension("wake"))) else {
            return;
        };
        driver.prompt("/waku-live-wake".to_owned());

        let mut started = 0;
        let mut woken = None;
        let mut text = String::new();
        loop {
            match next_live_step(&event_rx, "the run the extension started to settle") {
                DriverEvent::TurnStarted => started += 1,
                DriverEvent::ExtensionMessage {
                    custom_type,
                    text: body,
                    display,
                } => {
                    woken = Some((custom_type, body, display));
                }
                DriverEvent::TextDelta(delta) => text.push_str(&delta),
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                DriverEvent::TurnFinished {
                    success, summary, ..
                } => {
                    assert!(success, "the wake's run should finish: {summary:?}");
                    assert_eq!(
                        started, 1,
                        "a run with no Waku prompt of its own still opens one turn"
                    );
                    assert!(
                        woken.is_some() && text.contains("WAKU-LIVE-WAKE"),
                        "the run's message and reply must land in the turn that settles"
                    );
                    break;
                }
                _ => {}
            }
        }

        // A second turn for the same run would arrive right behind the first
        // settle — the empty completed turn the answer used to open — so the
        // window after it is where that has to be ruled out.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while let Ok(event) =
            event_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            match event {
                DriverEvent::TurnStarted => {
                    panic!("one run must open one turn, not settle an empty one and start another")
                }
                DriverEvent::TurnFinished { .. } => panic!("one run must settle once"),
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                _ => {}
            }
        }

        assert_eq!(started, 1, "a run settles its turn once");
        assert_eq!(
            woken,
            Some((
                "waku-live-wake".to_owned(),
                "Reply with exactly: WAKU-LIVE-WAKE. Do not use any tools.".to_owned(),
                true
            )),
            "the extension message that started the run must reach the client"
        );
        assert!(
            text.contains("WAKU-LIVE-WAKE"),
            "the run's reply must stream into the transcript: {text:?}"
        );
    }

    /// The extension's own messages reach their surfaces — a subagent's child
    /// notification as detached work, anything else as an extension message,
    /// and one marked not for display as such — and the question it asks is
    /// answered instead of cancelled on the user's behalf.
    #[test]
    fn pi_surfaces_extension_messages_and_answers_its_question_against_the_real_rpc() {
        let Some((driver, event_rx)) = real_pi_session(Some(live_probe_extension("probe"))) else {
            return;
        };
        driver.prompt("/waku-live-probe".to_owned());

        let mut notice = None;
        let mut child = None;
        let mut hidden = None;
        let mut question = None;
        let mut answer = None;
        let mut started = false;
        loop {
            match next_live_step(&event_rx, "the extension's messages and question") {
                DriverEvent::ExtensionMessage {
                    custom_type,
                    text,
                    display,
                } => match custom_type.as_str() {
                    "waku-live-notice" => notice = Some((text, display)),
                    "waku-live-hidden" => hidden = Some((text, display)),
                    "waku-live-answer" => answer = Some(text),
                    other => panic!("an extension message was not classified: {other}"),
                },
                DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) => {
                    child = Some(item)
                }
                DriverEvent::UserInputRequested {
                    request_id,
                    questions,
                } => {
                    assert!(question.is_none(), "the probe asks exactly one question");
                    assert_eq!(questions.len(), 1, "a dialog is one question");
                    assert_eq!(
                        questions[0].id, request_id,
                        "the question id is the provider's request id"
                    );
                    assert!(
                        questions[0]
                            .options
                            .iter()
                            .any(|option| option.label == PI_CONFIRM_ACCEPT),
                        "a confirmation offers its own answers"
                    );
                    question = Some(questions[0].question.clone());
                    driver.respond_user_input(
                        request_id.clone(),
                        vec![UserInputAnswer {
                            question_id: request_id,
                            answers: vec![PI_CONFIRM_ACCEPT.to_owned()],
                        }],
                    );
                }
                DriverEvent::TurnStarted => started = true,
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "an extension command settles the turn it consumed");
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(
            notice,
            Some(("probe notice".to_owned(), true)),
            "a plain extension message reaches the client"
        );
        let child =
            child.expect("the subagent notification must land on the detached-work surface");
        assert_eq!(child.key.kind, BackgroundWorkKind::Subagent);
        assert_eq!(child.key.provider_id, "probe-child");
        assert_eq!(child.title, "probe-child");
        assert_eq!(child.status, BackgroundWorkStatus::Completed);
        assert!(child.background, "a workflow child outlives the turn");
        assert_eq!(
            hidden,
            Some(("hidden probe".to_owned(), false)),
            "a message marked not for display is still delivered as one"
        );
        assert!(
            question.is_some_and(|question| question == "Answer the probe?"),
            "the extension's question reaches the client"
        );
        assert_eq!(
            answer,
            Some("answer=true".to_owned()),
            "the extension receives the user's answer, not a cancellation"
        );
        assert!(
            !started,
            "a message an extension appends without a run opens no turn"
        );
    }

    /// The next event from the live provider. A provider that goes quiet is a
    /// failure, and naming what was awaited is what makes a timeout readable.
    fn next_live_event(
        event_rx: &crossbeam_channel::Receiver<DriverEvent>,
        awaited: &str,
        budget: Duration,
    ) -> DriverEvent {
        event_rx.recv_timeout(budget).unwrap_or_else(|error| {
            panic!("the live Pi session produced no {awaited} within {budget:?}: {error}")
        })
    }

    /// The next event within the budget one step of a live turn gets.
    fn next_live_step(
        event_rx: &crossbeam_channel::Receiver<DriverEvent>,
        awaited: &str,
    ) -> DriverEvent {
        next_live_event(event_rx, awaited, LIVE_STEP_TIMEOUT)
    }

    /// The acceptance path this change exists for: `npm:pi-subagents` runs one
    /// background workflow, and the child's completion wakes the session — the
    /// completion lands where detached work is shown, the wake opens a turn with
    /// no Waku prompt, and the reply it produced streams into the transcript.
    ///
    /// Ignored because it needs Pi 1.0 authenticated with `npm:pi-subagents`
    /// installed, and because the extension registers nothing in a process that
    /// is itself a Pi subagent: it stands down when `PI_SUBAGENT_CHILD` is set,
    /// which is the case in a session that is one. Run it with
    /// `env -u PI_SUBAGENT_CHILD cargo test -p waku-core --lib -- --ignored
    /// pi_subagents`.
    #[test]
    #[ignore = "runs one real pi-subagents background workflow"]
    fn pi_subagents_bring_a_background_child_wakes_the_parent_against_the_real_rpc() {
        let extension = live_fixture(
            "subagents_wake",
            include_str!("fixtures/pi_subagents_wake.js"),
        );
        let Some((driver, event_rx)) = real_pi_session(Some(extension)) else {
            return;
        };

        // The command's own turn settles with no run: the workflow it launched
        // is detached, so the wake arrives later, as a run of its own.
        driver.prompt("/waku-subagents-wake".to_owned());

        let mut command_settled = false;
        let mut completion = false;
        let mut child = None;
        let mut woke = false;
        let mut reply = String::new();
        loop {
            match next_live_event(
                &event_rx,
                "the background child's completion",
                LIVE_WORKFLOW_TIMEOUT,
            ) {
                DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) => {
                    completion = true;
                    if item.key.kind == BackgroundWorkKind::Subagent {
                        child = Some(item);
                    }
                }
                DriverEvent::TurnStarted => {
                    assert!(
                        completion,
                        "the wake must follow the completion, not open a turn of its own"
                    );
                    woke = true;
                }
                DriverEvent::TextDelta(delta) if woke => reply.push_str(&delta),
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                DriverEvent::TurnFinished {
                    success, summary, ..
                } => {
                    if !woke {
                        assert!(
                            success,
                            "the command's own turn settles cleanly: {summary:?}"
                        );
                        command_settled = true;
                        continue;
                    }
                    assert!(success, "the wake's run should finish: {summary:?}");
                    break;
                }
                _ => {}
            }
        }

        assert!(
            command_settled,
            "the command that launched the workflow settles"
        );
        let child = child.expect("the child's completion must land on the detached-work surface");
        assert_eq!(child.status, BackgroundWorkStatus::Completed);
        assert!(
            child.background,
            "a workflow child outlives the turn it was launched in"
        );
        assert!(
            child
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("Background task completed")),
            "the completion says what became of the child: {:?}",
            child.detail
        );
        assert!(
            !reply.trim().is_empty(),
            "the reply the wake produced must stream into the transcript"
        );
    }

    /// Drives the installed Pi RPC through one real provider turn and reads the
    /// context usage the provider reports off it.
    #[test]
    fn pi_context_usage_against_the_real_rpc() {
        let Some((driver, event_rx)) = real_pi_session(None) else {
            return;
        };

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut context_tokens = None;
        let mut context_window = None;
        loop {
            match next_live_step(&event_rx, "the probe turn to settle") {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Pi should finish the probe turn");
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    /// A prompt sent while the turn is still streaming is queued on that turn,
    /// reported as pending, and delivered when the turn reaches its boundary —
    /// the prompt Pi refused outright before this change, leaving the user's
    /// words in a transcript the agent never saw.
    #[test]
    fn pi_queues_and_delivers_a_prompt_sent_mid_stream_against_the_real_rpc() {
        const QUEUED: &str = "Reply with exactly QUEUED-OK and nothing else. Do not use any tools.";

        let Some((driver, event_rx)) = real_pi_session(None) else {
            return;
        };
        driver.prompt(
            "Write a 200-word description of a lighthouse keeper's morning. Plain prose, one \
             paragraph, no tools, do not shorten it."
                .to_owned(),
        );

        let mut sent = false;
        let mut text = String::new();
        let mut pending = false;
        let mut released = false;
        loop {
            match next_live_step(&event_rx, "the busy turn to settle") {
                DriverEvent::TextDelta(delta) => {
                    // The queued prompt has to race a turn that is streaming, so
                    // it goes out with the first text the turn produces.
                    if !sent {
                        sent = true;
                        driver.prompt(QUEUED.to_owned());
                    }
                    text.push_str(&delta);
                }
                DriverEvent::ProviderQueue { follow_up, .. } => {
                    if follow_up.iter().any(|message| message == QUEUED) {
                        pending = true;
                    } else if pending {
                        released = true;
                    }
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                DriverEvent::TurnFinished {
                    success, summary, ..
                } => {
                    assert!(
                        success,
                        "a queued prompt must be delivered, not refused: {summary:?}"
                    );
                    break;
                }
                _ => {}
            }
        }

        assert!(
            sent,
            "the turn must stream text before the queued prompt goes out"
        );
        assert!(
            pending,
            "the provider's queue report must show the message while it is pending"
        );
        assert!(
            released,
            "the queue report must show the message leaving when it is delivered"
        );
        assert!(
            text.contains("QUEUED-OK"),
            "the queued message must be delivered into the turn: {text:?}"
        );
    }

    /// A stop takes the message the user had queued out of the provider's queue
    /// before it aborts, so a message they stopped never runs afterwards.
    #[test]
    fn pi_stopping_a_turn_takes_its_queued_message_back_against_the_real_rpc() {
        const QUEUED: &str = "Reply with exactly QUEUED-OK and nothing else. Do not use any tools.";

        let Some((driver, event_rx)) = real_pi_session(None) else {
            return;
        };
        driver.prompt(
            "Write a 300-word description of a lighthouse keeper's morning. Plain prose, one \
             paragraph, no tools, do not shorten it."
                .to_owned(),
        );

        let mut sent = false;
        let mut pending = false;
        let mut cleared = false;
        loop {
            match next_live_step(&event_rx, "the stopped turn to settle") {
                DriverEvent::TextDelta(_) => {
                    if !sent {
                        sent = true;
                        driver.prompt(QUEUED.to_owned());
                    }
                }
                DriverEvent::ProviderQueue {
                    steering,
                    follow_up,
                } => {
                    if follow_up.iter().any(|message| message == QUEUED) {
                        if !pending {
                            pending = true;
                            // Stopped while the provider still holds it.
                            driver.cancel();
                        }
                    } else if pending && steering.is_empty() && follow_up.is_empty() {
                        cleared = true;
                    }
                }
                // The other shape the retraction takes: the run settled with
                // the message still reported queued, so the settlement itself
                // took it back.
                DriverEvent::QueuedMessagesRetracted { messages } => {
                    if messages.iter().any(|message| message == QUEUED) {
                        cleared = true;
                    }
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "a stop must report no transport error");
                    break;
                }
                _ => {}
            }
        }

        assert!(
            sent,
            "the turn must stream text before the message is queued"
        );
        assert!(
            pending,
            "the message must be in the provider's queue before the stop"
        );
        assert!(cleared, "the stop must clear the provider's queue");

        // An abort keeps unwinding after the settle, so the window that would
        // show a stopped message running is the one after it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while let Ok(event) =
            event_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            match event {
                DriverEvent::TextDelta(delta) => assert!(
                    !delta.contains("QUEUED-OK"),
                    "a stopped message must not run afterwards: {delta:?}"
                ),
                DriverEvent::TurnStarted => {
                    panic!("a stopped message must not start a run of its own")
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                _ => {}
            }
        }
    }

    #[test]
    fn model_and_thinking_changes_reach_the_running_session_but_mode_changes_do_not() {
        let (commands, command_rx) = unbounded();
        let driver = PiDriver {
            flavor: PiFlavor::Pi,
            commands,
            computer_use: None,
            dialogs: PiDialogs::default(),
        };
        let options = |mode| SessionOptions {
            mode,
            model: Some("anthropic/claude-opus-5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: None,
            context_window: None,
        };

        assert!(driver.apply_options(options(RuntimeMode::FullAccess)));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::Options(_))
        ));

        // Pi has no permission setter and only runs with Full access.
        assert!(!driver.apply_options(options(RuntimeMode::Ask)));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn pi_computer_use_uses_only_session_scoped_extension_and_skill_arguments() {
        let config = computer_use_runtime::ComputerUseConfig {
            server_path: PathBuf::from("/tmp/Waku Computer Use"),
            repl_path: PathBuf::from("/Applications/Waku.app/Resources/waku_js_repl"),
            skill_path: PathBuf::from("/Applications/Waku.app/Resources/skills/SKILL.md"),
            process_directory: PathBuf::from("/tmp/waku-computer-use/session"),
        };
        let mut command = std::process::Command::new("pi");

        configure_pi_computer_use_command(
            &mut command,
            Some((
                &config,
                Path::new("/Applications/Waku.app/Resources/computer-use/pi-extension.ts"),
            )),
        );

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--extension",
                "/Applications/Waku.app/Resources/computer-use/pi-extension.ts",
                "--skill",
                "/Applications/Waku.app/Resources/skills/SKILL.md",
            ]
        );
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            environment.get("WAKU_JS_REPL_SERVER"),
            Some(&Some(
                "/Applications/Waku.app/Resources/waku_js_repl".into()
            ))
        );
        assert_eq!(
            environment.get("WAKU_COMPUTER_USE_PROCESS_DIRECTORY"),
            Some(&Some("/tmp/waku-computer-use/session".into()))
        );
    }

    #[test]
    fn pi_fork_selects_the_first_removed_user_turn_or_clones_the_tip() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 0).unwrap(),
            json!({"type": "clone"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 2).unwrap(),
            json!({"type": "fork", "entryId": "turn-2"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 3).unwrap(),
            json!({"type": "fork", "entryId": "turn-1"})
        );
        assert!(pi_fork_request(PiFlavor::Pi, &messages, 4).is_err());
    }

    #[test]
    fn streams_pi_text_reasoning_tools_and_settles_once() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "thinking_delta", "delta": "checking"}
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-1",
                "toolName": "read",
                "args": {"path": "src/main.rs", "title": "Inspect Pi source"}
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"content": "..."},
                "isError": false
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "willRetry": false}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "checking"
        ));
        let DriverEvent::RichActivity(started) = event_rx.recv().unwrap() else {
            panic!("expected a rich Pi tool activity");
        };
        assert_eq!(started.title, "Inspect Pi source");
        assert_eq!(started.kind, ActivityKind::FileRead);
        assert_eq!(started.display_target.as_deref(), Some("src/main.rs"));
        assert!(
            started
                .arguments
                .as_deref()
                .is_some_and(|arguments| arguments.contains("src/main.rs"))
        );
        assert!(!started.complete);
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("..."))
        );
        assert!(completed.complete);
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Drives the installed Oh My Pi RPC through one real provider turn.
    /// Ignored by default because it needs the CLI, credentials, and network.
    #[test]
    #[ignore = "requires an installed, authenticated omp"]
    fn ohmypi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("omp").expect("omp is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::OhMyPi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            events,
        )
        .expect("the Oh My Pi RPC session should start");

        let mut cursor = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(60)) {
            match event {
                DriverEvent::Connected { provider_cursor } => {
                    cursor = provider_cursor;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(
            matches!(
                cursor,
                Some(ProviderResumeCursor::OhMyPi {
                    session_file: Some(_),
                    ..
                })
            ),
            "Oh My Pi should report its own cursor with a session file, got {cursor:?}"
        );

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        let mut context_tokens = None;
        let mut context_window = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(180)) {
            match event {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Oh My Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Oh My Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    fn ohmypi_branches_where_pi_forks_and_never_asks_it_to_clone() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::OhMyPi, &messages, 2).unwrap(),
            json!({"type": "branch", "entryId": "turn-2"})
        );
        assert!(pi_fork_request(PiFlavor::OhMyPi, &messages, 4).is_err());
    }

    /// Oh My Pi reuses `agent_end` for intermediate settles, so only the
    /// terminal one may end the turn.
    #[test]
    fn ohmypi_settles_on_the_terminal_agent_end_only() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "isTerminal": false}),
            json!({"type": "agent_end", "messages": []}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Pi's own settle event carries no meaning for Oh My Pi, and vice versa.
    #[test]
    fn each_flavor_ignores_the_other_settle_and_title_events() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for (flavor, value) in [
            (PiFlavor::OhMyPi, json!({"type": "agent_start"})),
            (PiFlavor::OhMyPi, json!({"type": "agent_settled"})),
            (
                PiFlavor::OhMyPi,
                json!({"type": "session_info_changed", "name": "Pi's spelling"}),
            ),
            (PiFlavor::Pi, json!({"type": "agent_end"})),
            (
                PiFlavor::Pi,
                json!({"type": "session_info_update", "title": "Oh My Pi's spelling"}),
            ),
        ] {
            handle_pi_message(flavor, value, &pending, &commands, &events, &mut state);
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn ohmypi_session_titles_arrive_on_its_own_event() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "session_info_update", "title": "Named by Oh My Pi"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Oh My Pi"
        ));
    }

    #[test]
    fn chunked_frames_reassemble_and_reject_broken_runs() {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        let payload = json!({"type": "response", "id": "waku-1", "success": true});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (first, second) = bytes.split_at(bytes.len() / 2);
        let chunk = |index: u64, data: &[u8]| {
            json!({
                "type": "rpc_chunk",
                "chunkId": "rpc-1",
                "index": index,
                "count": 2,
                "byteLength": bytes.len(),
                "data": encode(data),
            })
        };

        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert_eq!(assembly.accept(chunk(1, second)).unwrap(), Some(payload));

        // An ordinary frame passes straight through.
        let mut assembly = ChunkAssembly::default();
        let plain = json!({"type": "agent_start"});
        assert_eq!(assembly.accept(plain.clone()).unwrap(), Some(plain.clone()));

        // Anything interleaved into a run invalidates it rather than splicing.
        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert!(assembly.accept(plain).is_err());
        assert!(assembly.active.is_none());

        // A run that starts mid-sequence is not a frame Waku can trust.
        let mut assembly = ChunkAssembly::default();
        assert!(assembly.accept(chunk(1, second)).is_err());
    }

    #[test]
    fn session_name_changes_are_forwarded_as_automatic_titles() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": "Named by Pi"}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Pi"
        ));

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": null}),
            &pending,
            &commands,
            &events,
            &mut state,
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(None)
        ));
    }

    #[test]
    fn tool_only_intermediate_message_does_not_emit_empty_text() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "toolCall", "id": "tool-1", "name": "read"}]
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn completed_message_is_used_when_deltas_were_not_streamed() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "reason"},
                        {"type": "text", "text": "answer"}
                    ]
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "reason"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "answer"
        ));
    }

    #[test]
    fn context_usage_uses_pi_components_when_total_is_zero() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "usage": {
                        "input": 33,
                        "output": 27,
                        "cacheRead": 5888,
                        "cacheWrite": 4,
                        "totalTokens": 0
                    }
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(5952),
                context_window: None
            }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn session_stats_supply_pi_context_tokens_and_window() {
        let state = json!({"data": {"model": {"contextWindow": 200_000}}});
        let stats = json!({
            "data": {
                "contextUsage": {
                    "tokens": 6109,
                    "contextWindow": 1_000_000,
                    "percent": 0.6109
                }
            }
        });

        assert_eq!(
            pi_context_usage(&state, Some(&stats)),
            Some((Some(6109), Some(1_000_000)))
        );
        assert_eq!(pi_context_usage(&state, None), Some((None, Some(200_000))));
    }

    #[test]
    fn recoverable_tool_error_does_not_fail_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"error": "missing"},
                "isError": true
            }),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(completed.failed);
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("missing"))
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    #[test]
    fn successful_auto_retry_recovers_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "error", "error": "temporary"}
            }),
            json!({"type": "auto_retry_end", "success": true}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::Error(_)));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    #[test]
    fn a_workflow_child_completion_lands_on_the_background_work_surface() {
        // pi-subagents reports a background child's settle as a custom
        // message. That message is the only signal the parent's session gets
        // that the child finished, and detached work is already the surface
        // for it, so it must not be dropped with the rest of the ignored
        // stream.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({
                "type": "message_start",
                "message": {
                    "role": "custom",
                    "customType": "subagent-incremental-child-notify",
                    "display": false,
                    "content": "Workflow child completed: **build**\nWorkflow run: wf-1\nStatus: workflow finished"
                }
            }),
            json!({
                "type": "message_end",
                "message": {
                    "role": "custom",
                    "customType": "subagent-incremental-child-notify",
                    "display": false,
                    "content": "Workflow child completed: **build**\nWorkflow run: wf-1\nStatus: workflow finished"
                }
            }),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) =
            event_rx.recv().unwrap()
        else {
            panic!("a subagent child completion must land on the background-work surface")
        };
        assert_eq!(item.key.kind, BackgroundWorkKind::Subagent);
        assert_eq!(item.title, "build", "the surface names the child");
        assert_eq!(item.status, BackgroundWorkStatus::Completed);
        assert!(
            item.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("workflow finished"))
        );
        assert!(
            event_rx.try_recv().is_err(),
            "one completion must produce exactly one surface update"
        );
        assert!(!state.run.is_live());
    }

    #[test]
    fn a_failed_background_task_lands_on_the_background_work_surface() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "custom",
                    "customType": "subagent-notify",
                    "display": true,
                    "content": "Background task failed: **code-auditor**\n\n(no output)"
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) =
            event_rx.recv().unwrap()
        else {
            panic!("a background task notification must land on the background-work surface")
        };
        assert_eq!(item.key.kind, BackgroundWorkKind::Subagent);
        assert_eq!(item.title, "code-auditor");
        assert_eq!(item.status, BackgroundWorkStatus::Failed);
    }

    #[test]
    fn a_child_named_after_an_outcome_settles_by_its_own_header() {
        // pi-subagents words the header as "<what happened>: **<child key>**",
        // and the key is the workflow's own: one called `retry-failed`
        // completed, so the outcome cannot be read from the whole line.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "custom",
                    "customType": "subagent-incremental-child-notify",
                    "display": false,
                    "content": "Workflow child completed: **retry-failed**\nWorkflow run: wf-1\nStatus: workflow finished"
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        let DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) =
            event_rx.recv().unwrap()
        else {
            panic!("a settled child belongs on the detached-work surface")
        };
        assert_eq!(item.title, "retry-failed");
        assert_eq!(item.status, BackgroundWorkStatus::Completed);
    }

    #[test]
    fn another_extension_message_becomes_a_notice() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "custom",
                    "customType": "subagent_control_notice",
                    "display": true,
                    "content": "Workflow paused."
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::ExtensionMessage { custom_type, text, display }
                if custom_type == "subagent_control_notice"
                    && text == "Workflow paused."
                    && display
        ));
    }

    #[test]
    fn an_extension_message_marked_not_for_display_is_still_delivered() {
        // Pi hides these from its own conversation, so the client adds no
        // conversation row for one. Which is only possible if the flag survives
        // the transport. The one exception is pi-subagents' detached-child
        // records, which the client shows on the work surface instead.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "custom",
                    "customType": "subagent-compaction-resume",
                    "display": false,
                    "content": "Context compaction resumed."
                }
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::ExtensionMessage { custom_type, text, display }
                if custom_type == "subagent-compaction-resume"
                    && text == "Context compaction resumed."
                    && !display
        ));
    }

    #[test]
    fn an_extension_notification_carries_its_severity() {
        // pi-subagents reports a failed child through `notify`, and the
        // severity is what tells the user whether anything went wrong: the
        // app's own notice surface shows the difference.
        let events = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-5",
            "method": "notify",
            "message": "Subagent failed: **code-auditor**",
            "notifyType": "error"
        }));

        assert!(matches!(
            events.as_slice(),
            [DriverEvent::ExtensionNotification { message, severity }]
                if message == "Subagent failed: **code-auditor**"
                    && *severity == NotificationSeverity::Error
        ));
    }

    #[test]
    fn an_extension_status_lives_on_its_key_and_clears_when_the_extension_clears_it() {
        // Pi keys its status entries by the extension's own key, and reports a
        // clear as the same method with no text. Both halves have to reach the
        // client: the first is the progress line the user watches, the second
        // is what takes it down again.
        let set = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-7",
            "method": "setStatus",
            "statusKey": "subagent-slash",
            "statusText": "running..."
        }));
        assert!(matches!(
            set.as_slice(),
            [DriverEvent::ExtensionStatus { key, text }]
                if key == "subagent-slash" && text.as_deref() == Some("running...")
        ));

        let cleared = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-8",
            "method": "setStatus",
            "statusKey": "subagent-slash"
        }));
        assert!(matches!(
            cleared.as_slice(),
            [DriverEvent::ExtensionStatus { key, text }]
                if key == "subagent-slash" && text.is_none()
        ));
    }

    #[test]
    fn a_notification_without_a_severity_is_informational() {
        // `notifyType` is optional, and an omission means to Pi exactly what it
        // means here.
        let events = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-6",
            "method": "notify",
            "message": "Model registry refreshed."
        }));

        assert!(matches!(
            events.as_slice(),
            [DriverEvent::ExtensionNotification { severity, .. }]
                if *severity == NotificationSeverity::Info
        ));
    }

    #[test]
    fn an_extension_widget_keeps_its_lines_and_its_placement() {
        let set = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-9",
            "method": "setWidget",
            "widgetKey": "subagent-fleet",
            "widgetLines": ["--- fleet ---", "2 running"],
            "widgetPlacement": "belowEditor"
        }));
        let [event] = set.as_slice() else {
            panic!("a widget update must be the only event its request produces")
        };
        let DriverEvent::ExtensionWidget {
            key,
            lines,
            placement,
        } = event
        else {
            panic!("a setWidget request must reach the client as a widget update")
        };
        assert_eq!(key, "subagent-fleet");
        assert_eq!(
            lines.as_deref(),
            Some(["--- fleet ---".to_owned(), "2 running".to_owned()].as_slice())
        );
        assert_eq!(*placement, ExtensionWidgetPlacement::BelowEditor);

        // Pi defaults the placement to the editor's own edge.
        let defaulted = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-10",
            "method": "setWidget",
            "widgetKey": "subagent-fleet",
            "widgetLines": ["1 running"]
        }));
        let [DriverEvent::ExtensionWidget { placement, .. }] = defaulted.as_slice() else {
            panic!("a widget update must be the only event its request produces")
        };
        assert_eq!(*placement, ExtensionWidgetPlacement::AboveEditor);
    }

    #[test]
    fn an_extension_widget_lives_on_its_key_and_clears_when_the_extension_clears_it() {
        let events = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-11",
            "method": "setWidget",
            "widgetKey": "subagent-fleet"
        }));

        assert!(matches!(
            events.as_slice(),
            [DriverEvent::ExtensionWidget { key, lines, .. }]
                if key == "subagent-fleet" && lines.is_none()
        ));
    }

    #[test]
    fn an_extension_title_and_editor_text_reach_the_client() {
        // Neither surface has a fallback in the transport: the app is the only
        // place they can be shown.
        let titled = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-12",
            "method": "setTitle",
            "title": "pi - waku-pi1-wt/t16"
        }));
        assert!(matches!(
            titled.as_slice(),
            [DriverEvent::ExtensionTitle { title }] if title == "pi - waku-pi1-wt/t16"
        ));

        let edited = extension_ui_events(json!({
            "type": "extension_ui_request",
            "id": "uuid-13",
            "method": "set_editor_text",
            "text": "review the diff"
        }));
        assert!(matches!(
            edited.as_slice(),
            [DriverEvent::ExtensionEditorText { text }] if text == "review the diff"
        ));
    }

    #[test]
    fn a_dialog_reaches_the_client_instead_of_being_cancelled_on_arrival() {
        // The provider blocks the extension until the question is answered, so
        // cancelling it on arrival is what made every dialog unusable. It has
        // to become a request the user can see; nothing is written back until
        // they answer or dismiss it.
        let mut harness = DialogHarness::new();
        harness.open(json!({
            "type": "extension_ui_request",
            "id": "uuid-1",
            "method": "select",
            "title": "Allow dangerous command?",
            "options": ["Allow", "Block"]
        }));

        let question = harness.question();
        assert_eq!(question.question, "Allow dangerous command?");
        assert_eq!(
            question
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Allow", "Block"]
        );
        assert!(
            harness.command_rx.try_recv().is_err(),
            "no response is written before the user answers"
        );
    }

    #[test]
    fn an_unknown_method_is_cancelled_so_the_provider_is_not_left_waiting() {
        // A method the client cannot present is answered immediately: if it is
        // a dialog the extension is blocked on it, and the client has no way
        // to show it.
        let mut harness = DialogHarness::new();
        harness.open(json!({
            "type": "extension_ui_request",
            "id": "uuid-9",
            "method": "custom",
            "title": "Pick a canvas"
        }));

        assert!(
            harness.event_rx.try_recv().is_err(),
            "an unknown method has no surface"
        );
        assert!(matches!(
            harness.command_rx.try_recv(),
            Ok(CommandMessage::CancelExtensionRequest(id)) if id == "uuid-9"
        ));
    }

    #[test]
    fn answering_a_dialog_returns_the_value_the_provider_expects() {
        // select, input and editor all answer with `value`; the chosen option
        // label is what Pi compares against its own option list.
        for (request, answer, expected) in [
            (
                json!({
                    "type": "extension_ui_request",
                    "id": "uuid-1",
                    "method": "select",
                    "title": "Allow dangerous command?",
                    "options": ["Allow", "Block"]
                }),
                "Block",
                "Block",
            ),
            (
                json!({
                    "type": "extension_ui_request",
                    "id": "uuid-3",
                    "method": "input",
                    "title": "Enter a value",
                    "placeholder": "type something..."
                }),
                "deploy to preview",
                "deploy to preview",
            ),
            (
                json!({
                    "type": "extension_ui_request",
                    "id": "uuid-4",
                    "method": "editor",
                    "title": "Edit some text",
                    "prefill": "Line 1"
                }),
                "Line 1\nedited",
                "Line 1\nedited",
            ),
        ] {
            let id = request["id"].as_str().unwrap().to_owned();
            let mut harness = DialogHarness::new();
            harness.open(request);
            let _ = harness.question();
            harness.driver.respond_user_input(
                id.clone(),
                vec![UserInputAnswer {
                    question_id: id.clone(),
                    answers: vec![answer.to_owned()],
                }],
            );
            assert!(
                matches!(
                    harness.command_rx.try_recv(),
                    Ok(CommandMessage::ExtensionUiResponse(value))
                        if value == json!({
                            "type": "extension_ui_response",
                            "id": id,
                            "value": expected,
                        })
                ),
                "{answer:?} must answer as {expected:?}"
            );
        }
    }

    #[test]
    fn a_confirmation_answers_with_a_boolean() {
        let mut harness = DialogHarness::new();
        harness.open(json!({
            "type": "extension_ui_request",
            "id": "uuid-2",
            "method": "confirm",
            "title": "Clear session?",
            "message": "All messages will be lost."
        }));
        let question = harness.question();
        assert_eq!(question.question, "All messages will be lost.");
        let labels: Vec<&str> = question
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect();
        assert_eq!(labels, ["Yes", "No"]);

        harness.driver.respond_user_input(
            "uuid-2".into(),
            vec![UserInputAnswer {
                question_id: "uuid-2".into(),
                answers: vec!["No".into()],
            }],
        );
        assert!(matches!(
            harness.command_rx.try_recv(),
            Ok(CommandMessage::ExtensionUiResponse(value))
                if value == json!({
                    "type": "extension_ui_response",
                    "id": "uuid-2",
                    "confirmed": false,
                })
        ));
    }

    #[test]
    fn dismissing_a_dialog_cancels_it_with_the_provider() {
        // Dismissing is the cancellation the extension reads as `undefined`,
        // not a value from an option the user never chose.
        let mut harness = DialogHarness::new();
        harness.open(json!({
            "type": "extension_ui_request",
            "id": "uuid-1",
            "method": "select",
            "title": "Allow dangerous command?",
            "options": ["Allow", "Block"]
        }));
        let _ = harness.question();

        harness.driver.respond_user_input(
            "uuid-1".into(),
            vec![UserInputAnswer {
                question_id: "uuid-1".into(),
                answers: Vec::new(),
            }],
        );
        assert!(matches!(
            harness.command_rx.try_recv(),
            Ok(CommandMessage::ExtensionUiResponse(value))
                if value == json!({
                    "type": "extension_ui_response",
                    "id": "uuid-1",
                    "cancelled": true,
                })
        ));
    }

    #[test]
    fn an_answer_to_a_dialog_that_is_not_open_is_dropped() {
        // The provider correlates responses by id; one for a request it never
        // sent, or a second answer to the same one, would be a reply to a
        // question that is no longer being asked.
        let mut harness = DialogHarness::new();
        harness.open(json!({
            "type": "extension_ui_request",
            "id": "uuid-1",
            "method": "select",
            "title": "Allow dangerous command?",
            "options": ["Allow", "Block"]
        }));
        let _ = harness.question();
        let answer = |answers: Vec<String>| {
            vec![UserInputAnswer {
                question_id: "uuid-1".into(),
                answers,
            }]
        };

        harness
            .driver
            .respond_user_input("uuid-other".into(), answer(vec!["Allow".into()]));
        harness
            .driver
            .respond_user_input("uuid-1".into(), answer(vec!["Allow".into()]));
        assert!(matches!(
            harness.command_rx.try_recv(),
            Ok(CommandMessage::ExtensionUiResponse(value))
                if value == json!({
                    "type": "extension_ui_response",
                    "id": "uuid-1",
                    "value": "Allow",
                })
        ));

        harness
            .driver
            .respond_user_input("uuid-1".into(), answer(vec!["Block".into()]));
        assert!(
            harness.command_rx.try_recv().is_err(),
            "a dialog is answered once"
        );
    }

    #[test]
    fn a_dialog_the_run_settled_without_answering_is_no_longer_answerable() {
        // A settlement ends the run the dialog belonged to. Whatever the
        // client still shows for it, the provider has moved on (its own
        // timeout resolves an abandoned dialog), so an answer must not be
        // written back as if the question were still open.
        let mut harness = DialogHarness::new();
        harness.open(json!({
            "type": "extension_ui_request",
            "id": "uuid-1",
            "method": "confirm",
            "title": "Clear session?",
            "message": "All messages will be lost."
        }));
        let _ = harness.question();

        harness.open(json!({"type": "agent_settled"}));
        harness.driver.respond_user_input(
            "uuid-1".into(),
            vec![UserInputAnswer {
                question_id: "uuid-1".into(),
                answers: vec!["Yes".into()],
            }],
        );
        assert!(
            harness.command_rx.try_recv().is_err(),
            "the settled run's dialog is not answered afterwards"
        );
    }

    #[test]
    fn a_pi_compact_prompt_becomes_the_providers_compaction_command() {
        // Pi's RPC prompt path expands extension commands, skills and prompt
        // templates and nothing else: a typed `/compact` would otherwise reach
        // the model as literal text. The transport recognises the invocation
        // and writes the compaction request instead, answer registered for the
        // reader thread.
        let (pending, _commands, _command_rx, _state) = harness();
        let (events, event_rx) = unbounded();
        let mut wire = Vec::new();
        let mut next_request_id = 0;
        dispatch_prompt(
            &mut wire,
            &pending,
            &mut next_request_id,
            &events,
            &RunLiveness::default(),
            PiFlavor::Pi,
            "/compact focus on the API".into(),
        );

        let writes = wire_lines(&wire);
        assert_eq!(writes.len(), 1, "a compaction is not also a prompt");
        assert_eq!(writes[0]["type"], "compact");
        assert_eq!(writes[0]["customInstructions"], "focus on the API");
        assert!(
            writes[0].get("id").is_none(),
            "the command answers through its events, so it awaits nothing"
        );
        assert!(pending.lock().is_empty());
        assert!(
            event_rx.try_recv().is_err(),
            "the command settles no turn yet"
        );

        // The bare form carries no instructions at all.
        let mut wire = Vec::new();
        dispatch_prompt(
            &mut wire,
            &pending,
            &mut next_request_id,
            &events,
            &RunLiveness::default(),
            PiFlavor::Pi,
            "/compact".into(),
        );
        let writes = wire_lines(&wire);
        assert_eq!(writes[0]["type"], "compact");
        assert!(writes[0].get("customInstructions").is_none());

        // Oh My Pi's flavour has no verified compaction command, so its own
        // behaviour is unchanged.
        let mut wire = Vec::new();
        dispatch_prompt(
            &mut wire,
            &pending,
            &mut next_request_id,
            &events,
            &RunLiveness::default(),
            PiFlavor::OhMyPi,
            "/compact".into(),
        );
        assert_eq!(wire_lines(&wire)[0]["type"], "prompt");
    }

    #[test]
    fn a_compact_prompt_never_aborts_a_live_run() {
        // Pi's `session.compact()` aborts the agent's operation first. A
        // compaction a submission asked for must not stop work the user did
        // not stop, so it is refused as that submission's delivery failure.
        let (pending, _commands, _command_rx, _state) = harness();
        let (events, event_rx) = unbounded();
        let run = RunLiveness::default();
        run.open();
        let mut wire = Vec::new();
        dispatch_prompt(
            &mut wire,
            &pending,
            &mut 0,
            &events,
            &run,
            PiFlavor::Pi,
            "/compact".into(),
        );

        assert!(wire.is_empty(), "no command may reach a run in flight");
        let DriverEvent::TurnFinished {
            success,
            summary,
            interrupted,
        } = event_rx.recv().unwrap()
        else {
            panic!("a refused compaction settles its submission")
        };
        assert!(!success);
        assert!(!interrupted);
        assert!(
            summary
                .as_deref()
                .is_some_and(|summary| summary.contains("Stop the running turn")),
            "the refusal names what to do about it"
        );
    }

    #[test]
    fn a_compact_steer_is_rejected_so_the_client_queues_it() {
        // A steer joins the live turn's boundary; a compaction there would
        // reach the model or wait for a later turn. The transport rejects it
        // as a steer, which is what has the client queue it and run it
        // through the prompt path where the command is recognised.
        let (pending, _commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        state.run.open();
        let mut wire = Vec::new();
        send_steer(
            &mut wire,
            &pending,
            &mut 0,
            &events,
            &state.run,
            PiFlavor::Pi,
            "/compact".to_owned(),
        );

        assert!(wire.is_empty(), "a compact steer writes nothing");
        let DriverEvent::SteerRejected { message, reason } = event_rx.recv().unwrap() else {
            panic!("a compact steer is rejected so the client queues it")
        };
        assert_eq!(message, "/compact");
        assert!(reason.contains("compaction waits"));
    }

    #[test]
    fn a_manual_compaction_reports_progress_usage_and_settlement() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "compaction_start", "reason": "manual"}),
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "result": {
                    "summary": "Kept notes about the API",
                    "firstKeptEntryId": "abc123",
                    "tokensBefore": 150000,
                    "estimatedTokensAfter": 32000,
                },
                "aborted": false,
                "willRetry": false,
            }),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        let DriverEvent::RichActivity(start) = event_rx.recv().unwrap() else {
            panic!("a compaction starts with its own row")
        };
        assert_eq!(start.title, tr!("activity.compacting_context"));
        assert!(!start.complete);
        let DriverEvent::RichActivity(end) = event_rx.recv().unwrap() else {
            panic!("the compaction's end completes the row")
        };
        assert_eq!(end.source_id, start.source_id, "one row per compaction");
        assert_eq!(end.title, tr!("activity.compacted_context"));
        assert!(end.complete && !end.failed);
        assert_eq!(end.output.as_deref(), Some("Kept notes about the API"));
        let DriverEvent::UsageUpdated {
            context_tokens,
            context_window,
        } = event_rx.recv().unwrap()
        else {
            panic!("the meter follows the provider's post-compaction estimate")
        };
        assert_eq!(context_tokens, Some(32_000));
        assert_eq!(context_window, None, "the known window is kept");
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished {
                success: true,
                interrupted: false,
                ..
            }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn a_failed_compaction_keeps_the_reason_and_still_settles_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "compaction_start", "reason": "manual"}),
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "aborted": false,
                "willRetry": false,
                "errorMessage": "No model selected",
            }),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::RichActivity(item) if !item.complete
        ));
        let DriverEvent::RichActivity(failed) = event_rx.recv().unwrap() else {
            panic!("the failure completes the row")
        };
        assert!(failed.complete && failed.failed);
        assert_eq!(failed.title, tr!("activity.compaction_failed"));
        assert_eq!(failed.detail.as_deref(), Some("No model selected"));
        assert!(
            matches!(
                event_rx.recv().unwrap(),
                DriverEvent::TurnFinished {
                    success: true,
                    interrupted: false,
                    ..
                }
            ),
            "maintenance failing is not the task failing"
        );
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn an_automatic_compaction_is_visible_but_settles_no_turn() {
        // Threshold and overflow compaction happen inside a run, so the run's
        // own settlement is the turn's; the events only add the row and the
        // refreshed meter.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "compaction_start", "reason": "threshold"}),
            json!({
                "type": "compaction_end",
                "reason": "threshold",
                "result": {"summary": "Summary", "estimatedTokensAfter": 41000},
                "aborted": false,
                "willRetry": false,
            }),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::RichActivity(item) if !item.complete
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::RichActivity(item) if item.complete
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(41_000),
                ..
            }
        ));
        assert!(event_rx.try_recv().is_err(), "no turn is settled for it");
    }

    #[test]
    fn a_compaction_end_without_its_start_completes_no_row() {
        // The row belongs to a start this session saw. An end on its own is
        // not a compaction anyone watched, so it reports no row — only the
        // meter it can still refresh and the settlement it owns.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "result": {"summary": "Summary", "estimatedTokensAfter": 32000},
                "aborted": false,
                "willRetry": false,
            }),
            &pending,
            &commands,
            &events,
            &mut state,
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(32_000),
                ..
            }
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished {
                success: true,
                interrupted: false,
                ..
            }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn consecutive_compactions_do_not_reuse_a_row_id() {
        // The app matches an end event to its row by id across the whole
        // transcript, so a counter that reset with the run would over the
        // next turn reopen the previous compaction's completed row instead
        // of adding one.
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for frame in [
            json!({"type": "compaction_start", "reason": "manual"}),
            json!({"type": "agent_settled"}),
            json!({"type": "compaction_start", "reason": "manual"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                frame,
                &pending,
                &commands,
                &events,
                &mut state,
            );
        }

        let DriverEvent::RichActivity(first) = event_rx.recv().unwrap() else {
            panic!("the first compaction opens a row")
        };
        let DriverEvent::RichActivity(second) = event_rx.recv().unwrap() else {
            panic!("the second compaction opens its own row")
        };
        assert_ne!(first.source_id, second.source_id);
    }
}
