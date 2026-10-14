//! Unattended provider-free suites and daemon-local schedules. Worker ownership
//! lives here, never on a requesting client's connection.
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use uuid::Uuid;
use waku_protocol::{
    AutomationCommand, AutomationError, AutomationJob, AutomationJobRun, AutomationProgram,
    AutomationResult, AutomationRun, AutomationScheduleResult, AutomationScratchSeed,
    AutomationStatus, MAX_AUTOMATION_JOB_RESULT_BYTES, MAX_AUTOMATION_JOB_RUNS,
};

use crate::automation_schedule::{
    AutomationScheduleStore, ClaimedAutomationSchedule, validate_automation_job,
};
use crate::automation_scratch::ScratchManager;

const TICK: Duration = Duration::from_millis(100);
type Runner = dyn Fn(
        &AutomationProgram,
        &AutomationScratchSeed,
        bool,
        &AtomicBool,
        Instant,
    ) -> Result<AutomationRun, AutomationError>
    + Send
    + Sync;

pub(crate) struct AutomationJobManager {
    inner: Arc<Inner>,
    scheduler: Mutex<Option<JoinHandle<()>>>,
}
struct Inner {
    state: Mutex<State>,
    schedules: Mutex<AutomationScheduleStore>,
    wake: Condvar,
    runner: Arc<Runner>,
}
#[derive(Default)]
struct State {
    runs: HashMap<Uuid, Entry>,
    order: VecDeque<Uuid>,
    bytes: usize,
    stopped: bool,
    // Failed summary writes retain their claim and retry without executing
    // programs again. Their entries cannot be evicted while durability waits.
    pending_finishes: HashMap<Uuid, PendingFinish>,
    scheduler_error: Option<AutomationError>,
    cancelled_claims: HashSet<Uuid>,
}
struct Entry {
    run: AutomationJobRun,
    bytes: usize,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
#[derive(Clone)]
struct PendingFinish {
    schedule_id: Uuid,
    result: AutomationScheduleResult,
}

impl AutomationJobManager {
    pub(crate) fn new(
        scratch: Arc<ScratchManager>,
        directory: &Path,
    ) -> Result<Self, AutomationError> {
        if !cfg!(debug_assertions) {
            return Err(AutomationError::new(
                "automation_disabled",
                "automation jobs are available only in debug builds",
            ));
        }
        Self::with_runner(
            directory,
            Arc::new(move |program, seed, headless, cancel, deadline| {
                scratch.run_program(program, seed, headless, cancel, deadline)
            }),
            true,
        )
    }

    fn with_runner(
        directory: &Path,
        runner: Arc<Runner>,
        start_scheduler: bool,
    ) -> Result<Self, AutomationError> {
        let schedules =
            AutomationScheduleStore::open(directory.join("automation-schedules.json"), now_ms())?;
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            schedules: Mutex::new(schedules),
            wake: Condvar::new(),
            runner,
        });
        let scheduler = if start_scheduler {
            let weak = Arc::downgrade(&inner);
            Some(
                std::thread::Builder::new()
                    .name("waku-automation-scheduler".into())
                    .spawn(move || scheduler_loop(weak))
                    .map_err(|error| {
                        AutomationError::new(
                            "job_worker_failed",
                            format!("could not start automation scheduler: {error}"),
                        )
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            inner,
            scheduler: Mutex::new(scheduler),
        })
    }

    pub(crate) fn command(
        &self,
        command: &AutomationCommand,
    ) -> Result<AutomationResult, AutomationError> {
        if self.inner.state.lock().stopped {
            return Err(AutomationError::new(
                "automation_stopped",
                "automation job manager has shut down",
            ));
        }
        match command {
            AutomationCommand::JobStart { job } => Ok(AutomationResult::Job {
                run: start_job(&self.inner, job.clone(), Uuid::new_v4(), now_ms(), None)?,
            }),
            AutomationCommand::JobResult { run_id } => {
                let state = self.inner.state.lock();
                let entry = state.runs.get(run_id).ok_or_else(|| unknown_run(*run_id))?;
                let mut run = entry.run.clone();
                if state.pending_finishes.contains_key(run_id)
                    && !run
                        .error
                        .as_ref()
                        .is_some_and(|error| error.code == "schedule_result_persistence_failed")
                {
                    run.status = AutomationStatus::Pending;
                    run.finished_at_ms = None;
                }
                Ok(AutomationResult::Job { run })
            }
            AutomationCommand::JobAbort { run_id } => {
                let state = self.inner.state.lock();
                let entry = state.runs.get(run_id).ok_or_else(|| unknown_run(*run_id))?;
                if matches!(entry.run.status, AutomationStatus::Pending) {
                    entry.cancel.store(true, Ordering::Release);
                }
                Ok(AutomationResult::Job {
                    run: entry.run.clone(),
                })
            }
            AutomationCommand::ScheduleCreate { definition } => {
                let schedule = self
                    .inner
                    .schedules
                    .lock()
                    .create(definition.clone(), now_ms())?;
                self.inner.wake.notify_all();
                Ok(AutomationResult::Schedule { schedule })
            }
            AutomationCommand::ScheduleList => {
                if let Some(error) = self.inner.state.lock().scheduler_error.clone() {
                    return Err(error);
                }
                Ok(AutomationResult::Schedules {
                    schedules: self.inner.schedules.lock().list(),
                })
            }
            AutomationCommand::ScheduleGet { schedule_id } => Ok(AutomationResult::Schedule {
                schedule: self.inner.schedules.lock().get(*schedule_id)?,
            }),
            AutomationCommand::ScheduleSetEnabled {
                schedule_id,
                enabled,
            } => Ok(AutomationResult::Schedule {
                schedule: self.inner.schedules.lock().set_enabled(
                    *schedule_id,
                    *enabled,
                    now_ms(),
                )?,
            }),
            AutomationCommand::ScheduleCancel { schedule_id } => {
                let schedule = self.inner.schedules.lock().cancel(*schedule_id, now_ms())?;
                if let Some(run_id) = schedule.active_run_id {
                    let mut state = self.inner.state.lock();
                    if let Some(entry) = state.runs.get(&run_id) {
                        entry.cancel.store(true, Ordering::Release);
                    } else {
                        // Cancellation can arrive after durable claim but
                        // before the scheduler has installed its worker.
                        state.cancelled_claims.insert(run_id);
                    }
                }
                Ok(AutomationResult::Schedule { schedule })
            }
            AutomationCommand::ScheduleRemove { schedule_id } => {
                self.inner.schedules.lock().remove(*schedule_id)?;
                Ok(AutomationResult::ScheduleRemoved {
                    schedule_id: *schedule_id,
                })
            }
            _ => Err(AutomationError::new(
                "invalid_job_command",
                "command is not a job or schedule operation",
            )),
        }
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut state = self.inner.state.lock();
            state.stopped = true;
            for entry in state.runs.values() {
                entry.cancel.store(true, Ordering::Release);
            }
            self.inner.wake.notify_all();
        }
        if let Some(scheduler) = self.scheduler.lock().take() {
            let _ = scheduler.join();
        }
        let workers: Vec<_> = self
            .inner
            .state
            .lock()
            .runs
            .values_mut()
            .filter_map(|entry| entry.worker.take())
            .collect();
        for worker in workers {
            let _ = worker.join();
        }
        // One last attempt persists a terminal summary produced during shutdown.
        retry_finishes(&self.inner);
    }
}
impl Drop for AutomationJobManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn scheduler_loop(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        if inner.state.lock().stopped {
            return;
        }
        poll_schedules(&inner, now_ms());
        let mut state = inner.state.lock();
        if state.stopped {
            return;
        }
        inner.wake.wait_for(&mut state, TICK);
    }
}

fn poll_schedules(inner: &Arc<Inner>, now: u64) {
    retry_finishes(inner);
    if inner.state.lock().stopped {
        return;
    }
    let claims = match inner.schedules.lock().claim_due(now) {
        Ok(claims) => claims,
        Err(error) => {
            inner.state.lock().scheduler_error = Some(error);
            return;
        }
    };
    {
        let mut state = inner.state.lock();
        if state.pending_finishes.is_empty() {
            state.scheduler_error = None;
        }
    }
    for claim in claims {
        dispatch_claim(inner, claim);
    }
}

fn dispatch_claim(inner: &Arc<Inner>, claim: ClaimedAutomationSchedule) {
    if let Err(error) = start_job(
        inner,
        claim.job.clone(),
        claim.run_id,
        claim.started_at_ms,
        Some(claim.schedule_id),
    ) {
        let summary = AutomationScheduleResult {
            run_id: claim.run_id,
            status: AutomationStatus::Failed,
            started_at_ms: claim.started_at_ms,
            finished_at_ms: now_ms().max(claim.started_at_ms),
            error: Some(error),
        };
        inner.state.lock().pending_finishes.insert(
            claim.run_id,
            PendingFinish {
                schedule_id: claim.schedule_id,
                result: summary.clone(),
            },
        );
        persist_summary(inner, claim.schedule_id, summary);
    }
}

fn start_job(
    inner: &Arc<Inner>,
    job: AutomationJob,
    run_id: Uuid,
    started_at_ms: u64,
    schedule_id: Option<Uuid>,
) -> Result<AutomationJobRun, AutomationError> {
    validate_automation_job(&job)?;
    let run = AutomationJobRun {
        run_id,
        name: job.name.clone(),
        status: AutomationStatus::Pending,
        started_at_ms,
        finished_at_ms: None,
        programs: vec![],
        error: None,
    };
    let bytes = charged_size(&run)?;
    let mut state = inner.state.lock();
    if state.stopped {
        return Err(AutomationError::new(
            "automation_stopped",
            "automation job manager has shut down",
        ));
    }
    make_room(&mut state, bytes, true, None)?;
    let cancel = Arc::new(AtomicBool::new(state.cancelled_claims.remove(&run_id)));
    state.runs.insert(
        run_id,
        Entry {
            run: run.clone(),
            bytes,
            cancel: cancel.clone(),
            worker: None,
        },
    );
    state.order.push_back(run_id);
    state.bytes += bytes;
    let worker_inner = inner.clone();
    let worker_run = run.clone();
    match std::thread::Builder::new().name(format!("waku-job-{run_id}")).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute_job(&worker_inner, job, worker_run.clone(), &cancel)));
        let terminal = match outcome {
            Ok(run) => run,
            Err(_) => failed_run(worker_run, AutomationError::new("job_worker_panicked", "automation worker panicked; owned scratch cleanup must finish before another run")),
        };
        complete_job(&worker_inner, terminal, schedule_id);
    }) {
        Ok(worker) => { state.runs.get_mut(&run_id).expect("inserted job").worker = Some(worker); }
        Err(error) => {
            state.runs.remove(&run_id);
            state.order.retain(|id| *id != run_id);
            state.bytes -= bytes;
            return Err(AutomationError::new("job_worker_failed", format!("could not start automation job: {error}")));
        }
    }
    Ok(run)
}

fn execute_job(
    inner: &Inner,
    job: AutomationJob,
    mut run: AutomationJobRun,
    cancel: &AtomicBool,
) -> AutomationJobRun {
    let deadline = Instant::now() + Duration::from_millis(job.timeout_ms);
    for program in &job.programs {
        if let Some(error) = interrupted(cancel, deadline) {
            return stopped_run(run, error);
        }
        match (inner.runner)(program, &job.seed, job.headless, cancel, deadline) {
            Ok(program_run) => {
                let status = program_run.status.clone();
                let error = program_run.error.clone();
                run.programs.push(program_run);
                if serialized_size(&run).is_err() {
                    run.programs.pop();
                    return failed_run(
                        run,
                        AutomationError::new(
                            "job_result_too_large",
                            "program evidence exceeds the bounded job result capacity; oversized program evidence was not retained",
                        ),
                    );
                }
                if let Some(error) = interrupted(cancel, deadline) {
                    return stopped_run(run, error);
                }
                match status {
                    AutomationStatus::Succeeded => {}
                    AutomationStatus::Aborted => {
                        return stopped_run(
                            run,
                            error.unwrap_or_else(|| {
                                AutomationError::new("job_aborted", "program was aborted")
                            }),
                        );
                    }
                    AutomationStatus::Failed => {
                        return failed_run(
                            run,
                            error.unwrap_or_else(|| {
                                AutomationError::new("job_program_failed", "program failed")
                            }),
                        );
                    }
                    AutomationStatus::Pending => {
                        return failed_run(
                            run,
                            AutomationError::new(
                                "invalid_job_result",
                                "scratch runner returned a nonterminal program",
                            ),
                        );
                    }
                }
            }
            Err(error) => {
                if let Some(interrupted) = interrupted(cancel, deadline) {
                    return stopped_run(run, interrupted);
                }
                return failed_run(run, error);
            }
        }
    }
    run.status = AutomationStatus::Succeeded;
    run.finished_at_ms = Some(now_ms().max(run.started_at_ms));
    run
}

fn complete_job(inner: &Arc<Inner>, mut run: AutomationJobRun, schedule_id: Option<Uuid>) {
    // Reserve a bounded terminal result before writing the corresponding
    // summary, so disk never reports success for evidence the cache discarded.
    let run_id = run.run_id;
    let mut state = inner.state.lock();
    let old_bytes = state.runs.get(&run_id).map_or(0, |entry| entry.bytes);
    let bytes = match charged_size(&run).and_then(|bytes| {
        make_room(
            &mut state,
            bytes.saturating_sub(old_bytes),
            false,
            Some(run_id),
        )?;
        Ok(bytes)
    }) {
        Ok(bytes) => bytes,
        Err(error) => {
            run.programs.clear();
            run = failed_run(
                run,
                AutomationError::new(
                    "job_result_capacity",
                    format!("terminal evidence could not be retained: {}", error.message),
                ),
            );
            charged_size(&run).expect("compact result fits")
        }
    };
    if let Some(entry) = state.runs.get_mut(&run_id) {
        entry.run = run.clone();
        entry.bytes = bytes;
    }
    state.bytes = state.bytes.saturating_sub(old_bytes) + bytes;
    // Protect the terminal entry from eviction while its durable summary is
    // being written. A failed write leaves this marker for retry ticks.
    if let Some(schedule_id) = schedule_id {
        state.pending_finishes.insert(
            run_id,
            PendingFinish {
                schedule_id,
                result: summary(&run),
            },
        );
    }
    drop(state);
    if let Some(schedule_id) = schedule_id {
        persist_summary(inner, schedule_id, summary(&run));
    }
}

fn persist_summary(inner: &Arc<Inner>, schedule_id: Uuid, result: AutomationScheduleResult) {
    persist_summary_after_write(inner, schedule_id, result, || {});
}

fn persist_summary_after_write(
    inner: &Arc<Inner>,
    schedule_id: Uuid,
    result: AutomationScheduleResult,
    after_write: impl FnOnce(),
) {
    let run_id = result.run_id;
    // Serialize the store write AND its bookkeeping with schedules → state
    // ordering. Otherwise a delayed failed writer could replace the state of
    // a retry that already succeeded, reviving a stale pending completion.
    let mut schedules = inner.schedules.lock();
    let result = {
        let state = inner.state.lock();
        let Some(pending) = state.pending_finishes.get(&run_id) else {
            // The original completion or another retry already committed.
            // This also protects against a stale retry after a later run has
            // replaced the schedule's last_result.
            return;
        };
        if pending.schedule_id != schedule_id {
            return;
        }
        // A retry may have captured its argument before a previous failure
        // updated the pending summary. Persist only the current generation.
        pending.result.clone()
    };
    let persisted = schedules
        .finish(schedule_id, run_id, result.clone(), now_ms())
        .map(|_| ());
    // The regression test pauses here, while ownership of schedules is still
    // held, to force an original-write/retry interleaving without sleeps.
    after_write();
    let mut state = inner.state.lock();
    match persisted {
        Ok(_) => {
            state.pending_finishes.remove(&run_id);
            state.cancelled_claims.remove(&run_id);
        }
        Err(error) => {
            let failure = AutomationError::new(
                "schedule_result_persistence_failed",
                format!(
                    "programs finished but the schedule result is not durable yet: {} ({})",
                    compact_text(&error.message, 1024),
                    compact_text(&error.code, 128)
                ),
            );
            let mut pending = result;
            pending.status = AutomationStatus::Failed;
            pending.error = Some(failure.clone());
            state.pending_finishes.insert(
                run_id,
                PendingFinish {
                    schedule_id,
                    result: pending,
                },
            );
            if let Some(entry) = state.runs.get_mut(&run_id) {
                entry.run.status = AutomationStatus::Failed;
                entry.run.error = Some(failure);
                // Admission reserved 8 KiB for this bounded durability error.
                // Replacing an existing large error only reduces actual use.
            }
            state.scheduler_error = Some(error);
        }
    }
    drop(state);
    drop(schedules);
}

fn retry_finishes(inner: &Arc<Inner>) {
    let pending: Vec<_> = inner
        .state
        .lock()
        .pending_finishes
        .values()
        .cloned()
        .collect();
    for pending in pending {
        persist_summary(inner, pending.schedule_id, pending.result);
    }
}

fn summary(run: &AutomationJobRun) -> AutomationScheduleResult {
    AutomationScheduleResult {
        run_id: run.run_id,
        status: run.status.clone(),
        started_at_ms: run.started_at_ms,
        finished_at_ms: run.finished_at_ms.unwrap_or(run.started_at_ms),
        error: run.error.as_ref().map(|error| {
            AutomationError::new(
                compact_text(&error.code, 128),
                compact_text(&error.message, 2048),
            )
        }),
    }
}

fn make_room(
    state: &mut State,
    additional_bytes: usize,
    new_entry: bool,
    preserve: Option<Uuid>,
) -> Result<(), AutomationError> {
    while state.bytes.saturating_add(additional_bytes) > MAX_AUTOMATION_JOB_RESULT_BYTES
        || (new_entry && state.runs.len() >= MAX_AUTOMATION_JOB_RUNS)
    {
        let victim = state.order.iter().copied().find(|id| {
            Some(*id) != preserve
                && !state.pending_finishes.contains_key(id)
                && state
                    .runs
                    .get(id)
                    .is_some_and(|entry| !matches!(entry.run.status, AutomationStatus::Pending))
        });
        let Some(victim) = victim else {
            return Err(AutomationError::new(
                "job_capacity",
                "all automation job capacity is in use; wait for a running job to finish",
            ));
        };
        if let Some(entry) = state.runs.remove(&victim) {
            state.bytes = state.bytes.saturating_sub(entry.bytes);
        }
        state.order.retain(|id| *id != victim);
    }
    Ok(())
}

fn interrupted(cancel: &AtomicBool, deadline: Instant) -> Option<AutomationError> {
    if cancel.load(Ordering::Acquire) {
        Some(AutomationError::new(
            "job_aborted",
            "automation job was cancelled",
        ))
    } else if Instant::now() >= deadline {
        Some(AutomationError::new(
            "job_timeout",
            "automation job exceeded its timeout",
        ))
    } else {
        None
    }
}
fn stopped_run(mut run: AutomationJobRun, error: AutomationError) -> AutomationJobRun {
    run.status = if error.code == "job_timeout" {
        AutomationStatus::Failed
    } else {
        AutomationStatus::Aborted
    };
    run.error = Some(error);
    run.finished_at_ms = Some(now_ms().max(run.started_at_ms));
    run
}
fn failed_run(mut run: AutomationJobRun, error: AutomationError) -> AutomationJobRun {
    run.status = AutomationStatus::Failed;
    run.error = Some(error);
    run.finished_at_ms = Some(now_ms().max(run.started_at_ms));
    run
}
fn unknown_run(run_id: Uuid) -> AutomationError {
    AutomationError::new(
        "unknown_job_run",
        format!("job {run_id} is unknown or its bounded result was evicted"),
    )
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn serialized_size(value: &impl Serialize) -> Result<usize, AutomationError> {
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_AUTOMATION_JOB_RESULT_BYTES {
                return Err(io::Error::other("job result size limit exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|error| {
        AutomationError::new(
            "job_result_too_large",
            format!("could not retain bounded job result: {error}"),
        )
    })?;
    Ok(counter.0)
}

// Charge a little more than actual bytes so a late persistence error can be
// reported without exceeding the retained evidence budget.
fn charged_size(value: &impl Serialize) -> Result<usize, AutomationError> {
    let bytes = serialized_size(value)?.saturating_add(8192).max(16 * 1024);
    if bytes > MAX_AUTOMATION_JOB_RESULT_BYTES {
        return Err(AutomationError::new(
            "job_result_too_large",
            "job result leaves no room for terminal metadata",
        ));
    }
    Ok(bytes)
}
fn compact_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use waku_protocol::{
        AutomationCall, AutomationCursor, AutomationScenario, AutomationScheduleDefinition,
        AutomationStep, AutomationStepOperation,
    };

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("waku-job-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn job(count: usize) -> AutomationJob {
        AutomationJob {
            name: "suite".into(),
            seed: AutomationScratchSeed::default(),
            headless: true,
            timeout_ms: 5000,
            programs: (0..count)
                .map(|index| AutomationProgram::Scenario {
                    scenario: AutomationScenario {
                        name: format!("program-{index}"),
                        steps: vec![AutomationStep {
                            name: "describe".into(),
                            operation: AutomationStepOperation::Call {
                                call: AutomationCall::Describe,
                            },
                        }],
                    },
                })
                .collect(),
        }
    }
    fn program_run(name: &str, status: AutomationStatus) -> AutomationRun {
        AutomationRun {
            run_id: Uuid::new_v4(),
            name: name.into(),
            status,
            steps: vec![],
            cursor: AutomationCursor {
                epoch: Uuid::new_v4(),
                sequence: 0,
            },
            error: None,
        }
    }
    fn success_runner() -> Arc<Runner> {
        Arc::new(|program, _, _, _, _| {
            let name = match program {
                AutomationProgram::Scenario { scenario } => &scenario.name,
                AutomationProgram::Replay { replay, .. } => &replay.name,
            };
            Ok(program_run(name, AutomationStatus::Succeeded))
        })
    }
    fn start(manager: &AutomationJobManager, job: AutomationJob) -> AutomationJobRun {
        let AutomationResult::Job { run } = manager
            .command(&AutomationCommand::JobStart { job })
            .unwrap()
        else {
            panic!("wrong result")
        };
        run
    }
    fn read(manager: &AutomationJobManager, run_id: Uuid) -> AutomationJobRun {
        let AutomationResult::Job { run } = manager
            .command(&AutomationCommand::JobResult { run_id })
            .unwrap()
        else {
            panic!("wrong result")
        };
        run
    }
    fn done(manager: &AutomationJobManager, run_id: Uuid) -> AutomationJobRun {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let run = read(manager, run_id);
            if !matches!(run.status, AutomationStatus::Pending) {
                return run;
            }
            assert!(Instant::now() < deadline, "job did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn schedule(manager: &AutomationJobManager) -> waku_protocol::AutomationSchedule {
        let AutomationResult::Schedule { schedule } = manager
            .command(&AutomationCommand::ScheduleCreate {
                definition: AutomationScheduleDefinition {
                    name: "scheduled".into(),
                    job: job(1),
                    first_run_at_ms: 0,
                    interval_ms: Some(1000),
                },
            })
            .unwrap()
        else {
            panic!("wrong result")
        };
        schedule
    }

    #[test]
    fn job_start_returns_pending_and_daemon_owns_ordered_completion() {
        let directory = TestDirectory::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let capture = observed.clone();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(move |program, seed, headless, _, _| {
                assert!(seed.sessions.is_empty());
                assert!(headless);
                let AutomationProgram::Scenario { scenario } = program else {
                    unreachable!()
                };
                capture.lock().push(scenario.name.clone());
                Ok(program_run(&scenario.name, AutomationStatus::Succeeded))
            }),
            false,
        )
        .unwrap();
        let pending = start(&manager, job(3));
        assert!(matches!(pending.status, AutomationStatus::Pending));
        assert!(pending.finished_at_ms.is_none());
        let finished = done(&manager, pending.run_id);
        assert!(matches!(finished.status, AutomationStatus::Succeeded));
        assert_eq!(finished.programs.len(), 3);
        assert!(finished.finished_at_ms.is_some());
        assert_eq!(*observed.lock(), ["program-0", "program-1", "program-2"]);
    }

    #[test]
    fn first_failure_stops_later_programs_and_keeps_evidence() {
        let directory = TestDirectory::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let capture = calls.clone();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(move |_, _, _, _, _| {
                let index = capture.fetch_add(1, Ordering::Relaxed);
                let mut run = program_run(
                    "failure",
                    if index == 1 {
                        AutomationStatus::Failed
                    } else {
                        AutomationStatus::Succeeded
                    },
                );
                if index == 1 {
                    run.error = Some(AutomationError::new(
                        "assertion_failed",
                        "expected composer text",
                    ));
                }
                Ok(run)
            }),
            false,
        )
        .unwrap();
        let finished = done(&manager, start(&manager, job(3)).run_id);
        assert!(matches!(finished.status, AutomationStatus::Failed));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(finished.programs.len(), 2);
        assert_eq!(
            finished.programs[1].error.as_ref().unwrap().code,
            "assertion_failed"
        );
        assert_eq!(finished.error.unwrap().code, "assertion_failed");
    }

    #[test]
    fn abort_is_observable_and_stops_before_next_program() {
        let directory = TestDirectory::new();
        let entered = Arc::new(AtomicBool::new(false));
        let capture = entered.clone();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(move |_, _, _, cancel, _| {
                capture.store(true, Ordering::Release);
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(program_run("aborted", AutomationStatus::Aborted))
            }),
            false,
        )
        .unwrap();
        let pending = start(&manager, job(2));
        while !entered.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        manager
            .command(&AutomationCommand::JobAbort {
                run_id: pending.run_id,
            })
            .unwrap();
        let finished = done(&manager, pending.run_id);
        assert!(matches!(finished.status, AutomationStatus::Aborted));
        assert_eq!(finished.programs.len(), 1);
        assert_eq!(finished.error.unwrap().code, "job_aborted");
    }

    #[test]
    fn timeout_is_failure_and_keeps_completed_program_evidence() {
        let directory = TestDirectory::new();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(|_, _, _, _, _| {
                std::thread::sleep(Duration::from_millis(5));
                Ok(program_run("late", AutomationStatus::Succeeded))
            }),
            false,
        )
        .unwrap();
        let mut input = job(2);
        input.timeout_ms = 1;
        let finished = done(&manager, start(&manager, input).run_id);
        assert!(matches!(finished.status, AutomationStatus::Failed));
        assert_eq!(finished.error.unwrap().code, "job_timeout");
        assert_eq!(finished.programs.len(), 1);
    }

    #[test]
    fn shutdown_cancels_workers_and_rejects_new_commands() {
        let directory = TestDirectory::new();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(|_, _, _, cancel, _| {
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(AutomationError::new("job_aborted", "cancelled"))
            }),
            false,
        )
        .unwrap();
        let pending = start(&manager, job(1));
        manager.shutdown();
        assert!(matches!(
            manager.inner.state.lock().runs[&pending.run_id].run.status,
            AutomationStatus::Aborted
        ));
        assert_eq!(
            manager
                .command(&AutomationCommand::JobStart { job: job(1) })
                .unwrap_err()
                .code,
            "automation_stopped"
        );
    }

    #[test]
    fn active_jobs_are_bounded_and_completed_results_are_evicted_oldest_first() {
        let directory = TestDirectory::new();
        let release = Arc::new(AtomicBool::new(false));
        let capture = release.clone();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(move |_, _, _, cancel, _| {
                while !capture.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(program_run("finished", AutomationStatus::Succeeded))
            }),
            false,
        )
        .unwrap();
        let ids: Vec<_> = (0..MAX_AUTOMATION_JOB_RUNS)
            .map(|_| start(&manager, job(1)).run_id)
            .collect();
        assert_eq!(
            manager
                .command(&AutomationCommand::JobStart { job: job(1) })
                .unwrap_err()
                .code,
            "job_capacity"
        );
        release.store(true, Ordering::Release);
        for id in &ids {
            done(&manager, *id);
        }
        done(&manager, start(&manager, job(1)).run_id);
        assert_eq!(
            manager
                .command(&AutomationCommand::JobResult { run_id: ids[0] })
                .unwrap_err()
                .code,
            "unknown_job_run"
        );
        assert_eq!(
            manager.inner.state.lock().runs.len(),
            MAX_AUTOMATION_JOB_RUNS
        );
        assert!(manager.inner.state.lock().bytes <= MAX_AUTOMATION_JOB_RESULT_BYTES);
    }

    #[test]
    fn oversized_evidence_is_explicit_failure_instead_of_silent_truncation() {
        let directory = TestDirectory::new();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(|_, _, _, _, _| {
                let mut run = program_run("large", AutomationStatus::Succeeded);
                run.name = "x".repeat(MAX_AUTOMATION_JOB_RESULT_BYTES);
                Ok(run)
            }),
            false,
        )
        .unwrap();
        let finished = done(&manager, start(&manager, job(1)).run_id);
        assert!(matches!(finished.status, AutomationStatus::Failed));
        assert_eq!(finished.error.unwrap().code, "job_result_too_large");
        assert!(finished.programs.is_empty());
        assert!(manager.inner.state.lock().bytes <= MAX_AUTOMATION_JOB_RESULT_BYTES);
    }

    #[test]
    fn worker_panic_becomes_terminal_failure() {
        let directory = TestDirectory::new();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(|_, _, _, _, _| panic!("test panic")),
            false,
        )
        .unwrap();
        let finished = done(&manager, start(&manager, job(1)).run_id);
        assert_eq!(finished.error.unwrap().code, "job_worker_panicked");
    }

    #[test]
    fn schedule_completion_persists_and_never_reexecutes_on_a_retry_tick() {
        let directory = TestDirectory::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let capture = calls.clone();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(move |_, _, _, _, _| {
                capture.fetch_add(1, Ordering::Relaxed);
                Ok(program_run("scheduled", AutomationStatus::Succeeded))
            }),
            false,
        )
        .unwrap();
        let schedule = schedule(&manager);
        let claim = manager
            .inner
            .schedules
            .lock()
            .claim_due(0)
            .unwrap()
            .remove(0);
        dispatch_claim(&manager.inner, claim.clone());
        let finished = done(&manager, claim.run_id);
        assert!(matches!(finished.status, AutomationStatus::Succeeded));
        let summary = manager
            .inner
            .schedules
            .lock()
            .get(schedule.id)
            .unwrap()
            .last_result
            .unwrap();
        persist_summary(&manager.inner, schedule.id, summary);
        assert!(manager.inner.state.lock().pending_finishes.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancel_between_claim_and_dispatch_stops_program_before_it_starts() {
        let directory = TestDirectory::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let capture = calls.clone();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(move |_, _, _, _, _| {
                capture.fetch_add(1, Ordering::Relaxed);
                Ok(program_run("should not run", AutomationStatus::Succeeded))
            }),
            false,
        )
        .unwrap();
        let schedule = schedule(&manager);
        let claim = manager
            .inner
            .schedules
            .lock()
            .claim_due(0)
            .unwrap()
            .remove(0);
        manager
            .command(&AutomationCommand::ScheduleCancel {
                schedule_id: schedule.id,
            })
            .unwrap();
        dispatch_claim(&manager.inner, claim.clone());
        assert!(matches!(
            done(&manager, claim.run_id).status,
            AutomationStatus::Aborted
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(
            !manager
                .inner
                .schedules
                .lock()
                .get(schedule.id)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn disabling_future_schedule_does_not_abort_active_worker() {
        let directory = TestDirectory::new();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(|_, _, _, cancel, _| {
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(program_run("aborted", AutomationStatus::Aborted))
            }),
            false,
        )
        .unwrap();
        let schedule = schedule(&manager);
        let claim = manager
            .inner
            .schedules
            .lock()
            .claim_due(0)
            .unwrap()
            .remove(0);
        dispatch_claim(&manager.inner, claim.clone());
        manager
            .command(&AutomationCommand::ScheduleSetEnabled {
                schedule_id: schedule.id,
                enabled: false,
            })
            .unwrap();
        assert!(
            !manager.inner.state.lock().runs[&claim.run_id]
                .cancel
                .load(Ordering::Acquire)
        );
        manager
            .command(&AutomationCommand::ScheduleCancel {
                schedule_id: schedule.id,
            })
            .unwrap();
        assert!(matches!(
            done(&manager, claim.run_id).status,
            AutomationStatus::Aborted
        ));
    }

    #[test]
    fn scheduled_capacity_failure_is_durable_and_not_dropped() {
        let directory = TestDirectory::new();
        let manager = AutomationJobManager::with_runner(
            &directory.0,
            Arc::new(|_, _, _, cancel, _| {
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(AutomationError::new("job_aborted", "cancelled"))
            }),
            false,
        )
        .unwrap();
        for _ in 0..MAX_AUTOMATION_JOB_RUNS {
            start(&manager, job(1));
        }
        let schedule = schedule(&manager);
        poll_schedules(&manager.inner, 0);
        let finished = manager.inner.schedules.lock().get(schedule.id).unwrap();
        assert!(finished.active_run_id.is_none());
        assert!(manager.inner.state.lock().pending_finishes.is_empty());
        assert_eq!(
            finished.last_result.unwrap().error.unwrap().code,
            "job_capacity"
        );
    }

    #[test]
    fn failed_schedule_summary_write_retries_without_executing_program_again() {
        let directory = TestDirectory::new();
        let manager =
            AutomationJobManager::with_runner(&directory.0, success_runner(), false).unwrap();
        let schedule = schedule(&manager);
        let claim = manager
            .inner
            .schedules
            .lock()
            .claim_due(0)
            .unwrap()
            .remove(0);
        let path = directory.0.join("automation-schedules.json");
        let backup = directory.0.join("backup.json");
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        dispatch_claim(&manager.inner, claim.clone());
        let finished = done(&manager, claim.run_id);
        assert_eq!(
            finished.error.unwrap().code,
            "schedule_result_persistence_failed"
        );
        assert_eq!(
            manager
                .inner
                .schedules
                .lock()
                .get(schedule.id)
                .unwrap()
                .active_run_id,
            Some(claim.run_id)
        );
        assert_eq!(manager.inner.state.lock().pending_finishes.len(), 1);
        assert!(manager.command(&AutomationCommand::ScheduleList).is_err());
        fs::remove_dir(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
        retry_finishes(&manager.inner);
        assert!(manager.inner.state.lock().pending_finishes.is_empty());
        let result = manager
            .inner
            .schedules
            .lock()
            .get(schedule.id)
            .unwrap()
            .last_result
            .unwrap();
        assert_eq!(
            result.error.unwrap().code,
            "schedule_result_persistence_failed"
        );
        assert_eq!(read(&manager, claim.run_id).programs.len(), 1);
    }

    #[test]
    fn failed_write_cannot_overwrite_retry_bookkeeping_or_revive_after_next_run() {
        use std::sync::mpsc;

        let directory = TestDirectory::new();
        let manager =
            AutomationJobManager::with_runner(&directory.0, success_runner(), false).unwrap();
        let scheduled = schedule(&manager);
        let claim = manager
            .inner
            .schedules
            .lock()
            .claim_due(0)
            .unwrap()
            .remove(0);
        let terminal = AutomationJobRun {
            run_id: claim.run_id,
            name: claim.job.name.clone(),
            status: AutomationStatus::Succeeded,
            started_at_ms: claim.started_at_ms,
            finished_at_ms: Some(now_ms()),
            programs: vec![],
            error: None,
        };
        let original_summary = summary(&terminal);
        {
            let mut state = manager.inner.state.lock();
            let bytes = charged_size(&terminal).unwrap();
            state.bytes = bytes;
            state.order.push_back(claim.run_id);
            state.runs.insert(
                claim.run_id,
                Entry {
                    run: terminal,
                    bytes,
                    cancel: Arc::new(AtomicBool::new(false)),
                    worker: None,
                },
            );
            state.pending_finishes.insert(
                claim.run_id,
                PendingFinish {
                    schedule_id: scheduled.id,
                    result: original_summary.clone(),
                },
            );
        }

        // Force a real, recoverable pre-rename error, then pause the original
        // writer after the attempt but before its in-memory outcome is applied.
        let path = directory.0.join("automation-schedules.json");
        let backup = directory.0.join("backup.json");
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        let (write_attempted, attempted) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let original_inner = manager.inner.clone();
        let first_summary = original_summary.clone();
        let original = std::thread::spawn(move || {
            persist_summary_after_write(&original_inner, scheduled.id, first_summary, || {
                write_attempted.send(()).unwrap();
                resumed.recv_timeout(Duration::from_secs(3)).unwrap();
            });
        });
        attempted.recv_timeout(Duration::from_secs(3)).unwrap();
        fs::remove_dir(&path).unwrap();
        fs::rename(&backup, &path).unwrap();

        // Snapshot the same pending result as retry_finishes, while the old
        // writer is paused. The retry must not acquire the schedules mutex
        // until both the failed write and its bookkeeping are complete.
        let (retry_blocked, blocked) = mpsc::channel();
        let retry_inner = manager.inner.clone();
        let retry = std::thread::spawn(move || {
            let pending = retry_inner.state.lock().pending_finishes[&claim.run_id].clone();
            let cannot_overtake = retry_inner.schedules.try_lock().is_none();
            retry_blocked.send(cannot_overtake).unwrap();
            persist_summary(&retry_inner, pending.schedule_id, pending.result);
        });
        let cannot_overtake = blocked.recv_timeout(Duration::from_secs(3)).unwrap();
        resume.send(()).unwrap();
        original.join().unwrap();
        retry.join().unwrap();
        assert!(
            cannot_overtake,
            "retry overtook the original writer's outcome bookkeeping"
        );
        assert!(manager.inner.state.lock().pending_finishes.is_empty());
        let persisted = manager.inner.schedules.lock().get(scheduled.id).unwrap();
        assert!(persisted.active_run_id.is_none());
        // The initial I/O error remains an explicit failure, but its retry
        // durably completes using the latest summary rather than stale success.
        assert_eq!(
            persisted.last_result.unwrap().error.unwrap().code,
            "schedule_result_persistence_failed"
        );

        // Complete another recurrence, then deliver an even older retry. It
        // must not reinsert the original run as permanently mismatched work.
        let next_at = persisted.next_run_at_ms.unwrap();
        let next = manager
            .inner
            .schedules
            .lock()
            .claim_due(next_at)
            .unwrap()
            .remove(0);
        let next_summary = AutomationScheduleResult {
            run_id: next.run_id,
            status: AutomationStatus::Succeeded,
            started_at_ms: next.started_at_ms,
            finished_at_ms: next.started_at_ms,
            error: None,
        };
        manager.inner.state.lock().pending_finishes.insert(
            next.run_id,
            PendingFinish {
                schedule_id: scheduled.id,
                result: next_summary.clone(),
            },
        );
        persist_summary(&manager.inner, scheduled.id, next_summary);
        persist_summary(&manager.inner, scheduled.id, original_summary);
        assert!(manager.inner.state.lock().pending_finishes.is_empty());
        let latest = manager
            .inner
            .schedules
            .lock()
            .get(scheduled.id)
            .unwrap()
            .last_result
            .unwrap();
        assert_eq!(latest.run_id, next.run_id);
        assert!(matches!(latest.status, AutomationStatus::Succeeded));
    }

    #[test]
    fn background_scheduler_runs_without_an_attached_requesting_client() {
        let directory = TestDirectory::new();
        let manager =
            AutomationJobManager::with_runner(&directory.0, success_runner(), true).unwrap();
        let scheduled = schedule(&manager);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = manager.inner.schedules.lock().get(scheduled.id).unwrap();
            if let Some(result) = state.last_result {
                assert!(matches!(result.status, AutomationStatus::Succeeded));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "background schedule did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn invalid_timeout_is_rejected_before_worker_admission() {
        let directory = TestDirectory::new();
        let manager =
            AutomationJobManager::with_runner(&directory.0, success_runner(), false).unwrap();
        for timeout in [0, waku_protocol::MAX_AUTOMATION_JOB_TIMEOUT_MS + 1] {
            let mut input = job(1);
            input.timeout_ms = timeout;
            assert_eq!(
                manager
                    .command(&AutomationCommand::JobStart { job: input })
                    .unwrap_err()
                    .code,
                "invalid_schedule"
            );
        }
        assert!(manager.inner.state.lock().runs.is_empty());
    }
}
