//! The daemon's side of the instance's diagnostic stream.
//!
//! The hub assigns every record its epoch and sequence (see `server`); this
//! module owns what surrounds that stream: the rotating NDJSON files under the
//! instance data directory, written by a background thread so a slow disk
//! never delays the daemon's event forwarders, and the classification of the
//! driver events the daemon observed into records attributed to the provider
//! runtime that produced them.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use parking_lot::{Condvar, Mutex};

use anyhow::Context as _;
use uuid::Uuid;

use crate::model::{DriverEvent, ProviderKind};
use crate::protocol::{DiagnosticCorrelation, DiagnosticEvent, DiagnosticOrigin, DiagnosticRecord};

/// The directory the rotating files live in, under the instance data
/// directory (the checkout's `temp/` in development).
const LOG_DIRECTORY: &str = "diagnostics";

/// How much one log file holds before the writer starts the next one. The
/// check runs before a record is written, so a file may end up one record
/// past the bound.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// How many log files are kept. Older files are removed as new ones start, so
/// disk use is bounded even across daemon restarts.
const MAX_FILES: usize = 3;
/// How many records may wait for the writer. The hub's ring is the
/// authoritative bound; this only keeps a stalled disk from growing the
/// daemon's heap.
const MAX_QUEUED_WRITES: usize = 4096;

/// The background writer for the instance's diagnostic files.
pub(crate) struct DiagnosticLog {
    state: Arc<LogState>,
}

struct LogState {
    queued: Mutex<LogQueue>,
    /// Wakes the writer when a record arrives or the last handle drops.
    ready: Condvar,
    /// The highest sequence the writer has put on disk.
    written_at: AtomicU64,
    stopping: AtomicBool,
}

#[derive(Default)]
struct LogQueue {
    entries: VecDeque<DiagnosticEvent>,
    last_queued: u64,
}

impl DiagnosticLog {
    /// Open the logs under `instance_directory`. Each daemon lifetime starts a
    /// new file — its sequence restarts with the epoch, so interleaving
    /// lifetimes in one file would break the file's order — and the newest
    /// files survive a restart. Opened on the caller's thread, so a directory
    /// that cannot hold the log fails here instead of inside the writer.
    pub(crate) fn open(instance_directory: &Path) -> anyhow::Result<Self> {
        let directory = instance_directory.join(LOG_DIRECTORY);
        fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        let file = RollingFile::open(&directory)?;
        let state = Arc::new(LogState {
            queued: Mutex::new(LogQueue::default()),
            ready: Condvar::new(),
            written_at: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        });
        let writer_state = state.clone();
        thread::Builder::new()
            .name("waku-diagnostics-writer".into())
            .spawn(move || run_writer(file, writer_state))
            .context("could not start the diagnostic writer")?;
        Ok(Self { state })
    }

    /// Queue one sequenced record. A writer that cannot keep up drops the
    /// oldest queued record: retention is bounded by dropping older records,
    /// and a slow disk must never stall the caller.
    pub(crate) fn append(&self, event: &DiagnosticEvent) {
        let mut queued = self.state.queued.lock();
        queued.last_queued = event.sequence;
        queued.entries.push_back(event.clone());
        trim_queue(&mut queued.entries);
        drop(queued);
        self.state.ready.notify_one();
    }

    /// Wait until every record queued so far is on disk. Tests read the files
    /// through this; the daemon never needs it.
    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let last_queued = self.state.queued.lock().last_queued;
            if self.state.written_at.load(Ordering::Acquire) >= last_queued {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the diagnostic writer did not catch up"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Stops the writer with the log: the thread ends once the queue is drained,
/// so a dropped log does not leave a thread (or its open file) behind.
impl Drop for DiagnosticLog {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        self.state.ready.notify_one();
    }
}

/// Drop the oldest queued records past the bound: retention drops older
/// records first, and a stalled writer must never grow the queue.
fn trim_queue(entries: &mut VecDeque<DiagnosticEvent>) {
    while entries.len() > MAX_QUEUED_WRITES {
        entries.pop_front();
    }
}

fn run_writer(mut file: RollingFile, state: Arc<LogState>) {
    loop {
        let batch = {
            let mut queued = state.queued.lock();
            while queued.entries.is_empty() {
                if state.stopping.load(Ordering::Acquire) {
                    return;
                }
                state.ready.wait(&mut queued);
            }
            queued.entries.drain(..).collect::<Vec<_>>()
        };
        let last = batch.last().map(|event| event.sequence).unwrap_or_default();
        for event in &batch {
            if let Err(error) = file.write_event(event) {
                // A file that cannot be written is not ground truth, and a
                // broken disk cannot be fixed from this thread; the ring
                // still serves what memory holds.
                eprintln!("waku-daemon: diagnostic log stopped: {error:#}");
                return;
            }
        }
        state.written_at.store(last, Ordering::Release);
    }
}

/// The newest file, rotated by size, with the older files pruned to the count
/// bound as new ones start.
struct RollingFile {
    directory: PathBuf,
    index: u64,
    bytes: u64,
    file: File,
}

impl RollingFile {
    fn open(directory: &Path) -> anyhow::Result<Self> {
        // A new file per daemon lifetime: the record sequence restarts with
        // each epoch, so one file must never interleave lifetimes, and the
        // file index also orders them.
        let index = newest_index(directory)?.saturating_add(1).max(1);
        let path = file_path(directory, index);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        let mut rolling = Self {
            directory: directory.to_owned(),
            index,
            bytes: 0,
            file,
        };
        rolling.prune()?;
        Ok(rolling)
    }

    fn write_event(&mut self, event: &DiagnosticEvent) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(event).context("could not encode a diagnostic record")?;
        line.push(b'\n');
        if self.bytes + line.len() as u64 > MAX_FILE_BYTES {
            self.rotate()?;
        }
        self.file
            .write_all(&line)
            .context("could not append to the diagnostic log")?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> anyhow::Result<()> {
        self.index = self.index.saturating_add(1);
        let path = file_path(&self.directory, self.index);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        self.bytes = 0;
        self.prune()?;
        Ok(())
    }

    /// Keep only the newest `MAX_FILES` files. Removing the oldest is part of
    /// rotation, so a long-running daemon cannot fill the disk.
    fn prune(&self) -> anyhow::Result<()> {
        let mut files = log_files(&self.directory)?;
        files.sort_unstable_by_key(|(index, _)| *index);
        let excess = files.len().saturating_sub(MAX_FILES);
        for (_, path) in files.into_iter().take(excess) {
            fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
        Ok(())
    }
}

fn file_path(directory: &Path, index: u64) -> PathBuf {
    directory.join(format!("diagnostics-{index}.ndjson"))
}

fn log_files(directory: &Path) -> anyhow::Result<Vec<(u64, PathBuf)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("could not read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(index) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("diagnostics-"))
            .and_then(|name| name.strip_suffix(".ndjson"))
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        files.push((index, path));
    }
    Ok(files)
}

fn newest_index(directory: &Path) -> anyhow::Result<u64> {
    Ok(log_files(directory)?
        .into_iter()
        .map(|(index, _)| index)
        .max()
        .unwrap_or(0))
}

/// The provider runtime a record came from: the provider, the task it works
/// for, and the daemon's runtime generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderRuntime {
    pub(crate) provider: ProviderKind,
    pub(crate) session_id: Uuid,
    pub(crate) runtime_id: Uuid,
}

/// The record for one driver event the daemon observed, attributed to the
/// provider runtime it came from, or `None` for high-volume detail. The turn
/// comes from the milestone when the event names one and from the runtime's
/// [`ActiveTurn`] otherwise.
pub(crate) fn driver_record(
    runtime: ProviderRuntime,
    active_turn: Option<Uuid>,
    event: &DriverEvent,
) -> Option<DiagnosticRecord> {
    let milestone = waku_protocol::driver_milestone(event)?;
    Some(milestone.record(
        DiagnosticOrigin::Provider {
            provider: runtime.provider,
            runtime_id: runtime.runtime_id,
        },
        DiagnosticCorrelation {
            task_id: Some(runtime.session_id),
            turn_id: active_turn,
            ..DiagnosticCorrelation::default()
        },
    ))
}

/// Tracks the turn a provider runtime is in from the submission and settlement
/// events the daemon itself forwards, so the records for the events between
/// them carry the turn identity, not only the task.
#[derive(Default)]
pub(crate) struct ActiveTurn(Option<Uuid>);

impl ActiveTurn {
    /// The turn `event` belongs to, advancing the tracker. The submission that
    /// opens a turn and the settlement that closes it both belong to that
    /// turn; every other event belongs to the turn already open.
    pub(crate) fn turn_for(&mut self, event: &DriverEvent) -> Option<Uuid> {
        let current = self.0;
        match event {
            DriverEvent::PromptSubmitted { turn_id, .. } => {
                self.0 = Some(*turn_id);
                Some(*turn_id)
            }
            DriverEvent::TurnFinished { .. } | DriverEvent::ProcessExited => {
                self.0 = None;
                current
            }
            _ => current,
        }
    }
}

/// A provider process's raw output, attributed to the runtime that produced
/// it. Stderr lines are recorded at debug level: the provider's own error
/// events are the records that explain a failure.
#[derive(Clone)]
pub(crate) struct ProviderDiagnostics {
    runtime: ProviderRuntime,
    /// The turn the runtime is in, kept by the daemon's event forwarder so a
    /// stderr line arriving mid-turn joins that turn.
    turn: Arc<Mutex<Option<Uuid>>>,
    record: Arc<dyn Fn(DiagnosticRecord) + Send + Sync>,
}

impl ProviderDiagnostics {
    pub(crate) fn new(
        runtime: ProviderRuntime,
        record: impl Fn(DiagnosticRecord) + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            turn: Arc::new(Mutex::new(None)),
            record: Arc::new(record),
        }
    }

    /// Note the turn the runtime has reached, for the lines that arrive after
    /// it.
    pub(crate) fn set_turn(&self, turn: Option<Uuid>) {
        *self.turn.lock() = turn;
    }

    pub(crate) fn stderr(&self, line: &str) {
        // A release daemon records nothing; skip the record entirely.
        if !cfg!(debug_assertions) {
            return;
        }
        let record = DiagnosticRecord::new(
            crate::protocol::DiagnosticLevel::Debug,
            DiagnosticOrigin::Provider {
                provider: self.runtime.provider,
                runtime_id: self.runtime.runtime_id,
            },
            "stderr",
            line,
        )
        .with_correlation(DiagnosticCorrelation {
            task_id: Some(self.runtime.session_id),
            turn_id: *self.turn.lock(),
            ..DiagnosticCorrelation::default()
        });
        (self.record)(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DiagnosticEvent, DiagnosticLevel};
    use uuid::Uuid;

    fn event(sequence: u64, message: &str) -> DiagnosticEvent {
        DiagnosticEvent {
            epoch: Uuid::from_u128(1),
            sequence,
            record: DiagnosticRecord::new(
                DiagnosticLevel::Info,
                DiagnosticOrigin::Daemon { pid: 1 },
                "test",
                message,
            ),
        }
    }

    #[test]
    fn the_log_keeps_the_newest_files_within_the_size_bound() {
        let directory = std::env::temp_dir().join(format!("waku-diagnostics-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let log = DiagnosticLog::open(&directory).unwrap();
        let files = directory.join(LOG_DIRECTORY);
        // Fill past a whole file: the writer must rotate and prune.
        let filler = "x".repeat(1024);
        let count = (MAX_FILE_BYTES / 1024 + 2) as u64;
        for sequence in 1..=count {
            log.append(&event(sequence, &filler));
        }
        log.flush();
        let written = log_files(&files).unwrap();
        assert!(
            written.len() <= MAX_FILES,
            "rotation keeps at most {MAX_FILES} files, found {}",
            written.len()
        );
        assert!(
            written.len() >= 2,
            "the volume must have rotated at least once"
        );
        for (_, path) in &written {
            let bytes = std::fs::metadata(path).unwrap().len();
            assert!(
                bytes <= MAX_FILE_BYTES + 2048,
                "{} holds {bytes} bytes, past the rotation bound",
                path.display()
            );
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_stderr_line_is_attributed_to_its_provider_and_runtime() {
        let runtime_id = Uuid::from_u128(9);
        let session_id = Uuid::from_u128(10);
        let (records, received) = std::sync::mpsc::channel();
        let diagnostics = crate::diagnostics::ProviderDiagnostics::new(
            ProviderRuntime {
                provider: ProviderKind::Pi,
                session_id,
                runtime_id,
            },
            move |record| {
                let _ = records.send(record);
            },
        );
        diagnostics.stderr("warning: extension not loaded");
        let record = received.recv().unwrap();
        assert_eq!(record.level, DiagnosticLevel::Debug);
        assert_eq!(record.source, "stderr");
        assert_eq!(record.message, "warning: extension not loaded");
        assert_eq!(record.correlation.task_id, Some(session_id));
        assert_eq!(
            record.origin,
            DiagnosticOrigin::Provider {
                provider: ProviderKind::Pi,
                runtime_id,
            }
        );
    }

    #[test]
    fn a_driver_event_becomes_a_provider_record_with_the_task_it_belongs_to() {
        let runtime_id = Uuid::from_u128(2);
        let session_id = Uuid::from_u128(3);
        let turn_id = Uuid::from_u128(4);
        let record = driver_record(
            ProviderRuntime {
                provider: ProviderKind::Codex,
                session_id,
                runtime_id,
            },
            Some(turn_id),
            &DriverEvent::Error("codex: connection reset".into()),
        )
        .unwrap();
        assert_eq!(record.level, DiagnosticLevel::Error);
        assert_eq!(record.fields["error"], "codex: connection reset");
        assert_eq!(record.correlation.task_id, Some(session_id));
        assert_eq!(record.correlation.turn_id, Some(turn_id));
        assert_eq!(
            record.origin,
            DiagnosticOrigin::Provider {
                provider: ProviderKind::Codex,
                runtime_id,
            }
        );
        assert!(
            driver_record(
                ProviderRuntime {
                    provider: ProviderKind::Codex,
                    session_id,
                    runtime_id,
                },
                None,
                &DriverEvent::TextDelta("chunk".into())
            )
            .is_none(),
            "high-volume detail stays out of the bounded stream"
        );
    }

    #[test]
    fn the_active_turn_opens_at_submission_and_closes_at_settlement() {
        let turn_id = Uuid::from_u128(7);
        let mut active = ActiveTurn::default();
        assert_eq!(active.turn_for(&DriverEvent::TurnStarted), None);
        assert_eq!(
            active.turn_for(&DriverEvent::PromptSubmitted {
                message: "ship it".into(),
                turn_id,
                message_id: Uuid::from_u128(8),
            }),
            Some(turn_id)
        );
        assert_eq!(active.turn_for(&DriverEvent::TurnStarted), Some(turn_id));
        assert_eq!(
            active.turn_for(&DriverEvent::Activity {
                id: None,
                kind: crate::model::ActivityKind::Tool,
                title: "run tests".into(),
                detail: None,
                complete: true,
            }),
            Some(turn_id)
        );
        // The settlement itself belongs to the turn it closes.
        assert_eq!(
            active.turn_for(&DriverEvent::TurnFinished {
                success: true,
                summary: None,
                interrupted: false,
            }),
            Some(turn_id)
        );
        assert_eq!(active.turn_for(&DriverEvent::TurnStarted), None);
    }
}
