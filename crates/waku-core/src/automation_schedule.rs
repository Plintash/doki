//! Durable local schedule state machine. The daemon supplies the clock and
//! executes returned claims. This module never spawns apps, changes OS
//! schedules, uses credentials, or performs automation itself.
//!
//! Every transition is committed through a same-directory, fsynced temporary
//! file before work can leave the store. A held file lock prevents a second
//! daemon/store from claiming this file. Corrupt or unsupported state is an
//! explicit error, never an excuse to start with an empty schedule list.
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use waku_protocol::{
    AUTOMATION_REPLAY_FORMAT_VERSION, AutomationError, AutomationJob, AutomationProgram,
    AutomationSchedule, AutomationScheduleDefinition, AutomationScheduleResult, AutomationStatus,
    MAX_AUTOMATION_JOB_BYTES, MAX_AUTOMATION_JOB_PROGRAMS, MAX_AUTOMATION_JOB_TIMEOUT_MS,
    MAX_AUTOMATION_SCHEDULE_RESULT_BYTES, MAX_AUTOMATION_SCHEDULES, MAX_AUTOMATION_STEPS,
    MIN_AUTOMATION_SCHEDULE_INTERVAL_MS,
};

const STORE_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 256;
const MAX_STORE_BYTES: usize = MAX_AUTOMATION_SCHEDULES
    * (MAX_AUTOMATION_JOB_BYTES + MAX_AUTOMATION_SCHEDULE_RESULT_BYTES + 4096);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSchedules {
    format_version: u32,
    schedules: Vec<StoredSchedule>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSchedule {
    schedule: AutomationSchedule,
    active_started_at_ms: Option<u64>,
}

/// A claim has already been durably recorded. Only the owner of this run ID
/// may finish it. The caller must not execute anything if claim_due errors.
#[derive(Clone, Debug)]
pub struct ClaimedAutomationSchedule {
    pub schedule_id: Uuid,
    pub run_id: Uuid,
    pub job: AutomationJob,
    pub started_at_ms: u64,
}

pub struct AutomationScheduleStore {
    path: PathBuf,
    data: StoredSchedules,
    // Lock a stable sidecar, not the state file that gets replaced by rename.
    _lock: File,
    // A post-rename sync error means that durability is uncertain. Refuse
    // further mutations until reopened, rather than issuing duplicate claims.
    poisoned: bool,
    #[cfg(test)]
    fail_before_rename: bool,
    #[cfg(test)]
    fail_after_rename: bool,
}

impl AutomationScheduleStore {
    pub fn open(path: impl AsRef<Path>, now: u64) -> Result<Self, AutomationError> {
        let path = path.as_ref().to_path_buf();
        let parent = parent_directory(&path);
        crate::fs_ext::create_private_dir_all(parent)
            .map_err(|error| io_error("create schedule directory", error))?;
        let filename = path.file_name().ok_or_else(|| {
            AutomationError::new("invalid_schedule_path", "schedule path must name a file")
        })?;
        let lock_path = parent.join(format!(".{}.lock", filename.to_string_lossy()));
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            lock_options.mode(0o600);
        }
        let lock = lock_options
            .open(lock_path)
            .map_err(|error| io_error("open schedule lock", error))?;
        lock.try_lock().map_err(|error| {
            AutomationError::new(
                "schedule_store_locked",
                format!("could not acquire exclusive schedule ownership: {error}"),
            )
        })?;
        let data = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(corrupt("schedule store must be a regular file"));
                }
                if metadata.len() > MAX_STORE_BYTES as u64 {
                    return Err(corrupt("schedule store exceeds the size limit"));
                }
                let file =
                    File::open(&path).map_err(|error| io_error("read schedule store", error))?;
                let mut bytes = Vec::new();
                file.take(MAX_STORE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| io_error("read schedule store", error))?;
                if bytes.len() > MAX_STORE_BYTES {
                    return Err(corrupt("schedule store exceeds the size limit"));
                }
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|error| corrupt(format!("invalid schedule JSON: {error}")))?;
                if value
                    .get("formatVersion")
                    .and_then(serde_json::Value::as_u64)
                    != Some(STORE_VERSION as u64)
                {
                    return Err(AutomationError::new(
                        "unsupported_schedule_store_version",
                        "schedule store format version is missing or unsupported",
                    ));
                }
                let data: StoredSchedules = serde_json::from_value(value)
                    .map_err(|error| corrupt(format!("invalid schedule state: {error}")))?;
                validate_stored(&data)?;
                data
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredSchedules {
                format_version: STORE_VERSION,
                schedules: vec![],
            },
            Err(error) => return Err(io_error("inspect schedule store", error)),
        };
        let mut store = Self {
            path,
            data,
            _lock: lock,
            poisoned: false,
            #[cfg(test)]
            fail_before_rename: false,
            #[cfg(test)]
            fail_after_rename: false,
        };
        let mut recovered = store.data.clone();
        let mut changed = false;
        for entry in &mut recovered.schedules {
            if let Some(run_id) = entry.schedule.active_run_id.take() {
                let started_at_ms = entry
                    .active_started_at_ms
                    .take()
                    .expect("validated active timestamp");
                entry.schedule.last_result = Some(AutomationScheduleResult {
                    run_id,
                    status: AutomationStatus::Failed,
                    started_at_ms,
                    finished_at_ms: now.max(started_at_ms),
                    error: Some(AutomationError::new(
                        "schedule_interrupted",
                        "daemon stopped before the claimed job recorded its result; this run was not retried",
                    )),
                });
                skip_missed(&mut entry.schedule, now);
                changed = true;
            }
        }
        if changed {
            store.commit(recovered)?;
        }
        Ok(store)
    }

    pub fn create(
        &mut self,
        definition: AutomationScheduleDefinition,
        _now: u64,
    ) -> Result<AutomationSchedule, AutomationError> {
        self.ensure_writable()?;
        validate_definition(&definition)?;
        if self.data.schedules.len() >= MAX_AUTOMATION_SCHEDULES {
            return Err(AutomationError::new(
                "schedule_limit",
                format!("at most {MAX_AUTOMATION_SCHEDULES} schedules may be stored"),
            ));
        }
        let schedule = AutomationSchedule {
            id: Uuid::new_v4(),
            next_run_at_ms: Some(definition.first_run_at_ms),
            definition,
            enabled: true,
            active_run_id: None,
            last_result: None,
        };
        let mut next = self.data.clone();
        next.schedules.push(StoredSchedule {
            schedule: schedule.clone(),
            active_started_at_ms: None,
        });
        self.commit(next)?;
        Ok(schedule)
    }

    pub fn list(&self) -> Vec<AutomationSchedule> {
        self.data
            .schedules
            .iter()
            .map(|entry| entry.schedule.clone())
            .collect()
    }

    pub fn get(&self, id: Uuid) -> Result<AutomationSchedule, AutomationError> {
        self.data
            .schedules
            .iter()
            .find(|entry| entry.schedule.id == id)
            .map(|entry| entry.schedule.clone())
            .ok_or_else(|| not_found(id))
    }

    /// Disable/cancel affects future claims, not work that already owns a
    /// claim. Re-enabling an exhausted one-shot is an error, never a retry.
    pub fn set_enabled(
        &mut self,
        id: Uuid,
        enabled: bool,
        now: u64,
    ) -> Result<AutomationSchedule, AutomationError> {
        self.ensure_writable()?;
        let mut next = self.data.clone();
        let entry = find_mut(&mut next, id)?;
        if enabled && entry.schedule.next_run_at_ms.is_none() {
            return Err(AutomationError::new(
                "schedule_exhausted",
                "schedule has no remaining run; create a new schedule to run it again",
            ));
        }
        if entry.schedule.enabled == enabled {
            return Ok(entry.schedule.clone());
        }
        if enabled && entry.schedule.definition.interval_ms.is_some() {
            skip_missed(&mut entry.schedule, now);
            if entry.schedule.next_run_at_ms.is_none() {
                return Err(AutomationError::new(
                    "schedule_exhausted",
                    "schedule cadence exceeds the supported clock range",
                ));
            }
        }
        entry.schedule.enabled = enabled;
        let schedule = entry.schedule.clone();
        self.commit(next)?;
        Ok(schedule)
    }

    pub fn cancel(&mut self, id: Uuid, now: u64) -> Result<AutomationSchedule, AutomationError> {
        self.set_enabled(id, false, now)
    }

    /// Removal is deliberately stricter than cancellation: no active job
    /// loses its durable ownership or result sink.
    pub fn remove(&mut self, id: Uuid) -> Result<(), AutomationError> {
        self.ensure_writable()?;
        let schedule = self.get(id)?;
        if schedule.enabled || schedule.active_run_id.is_some() {
            return Err(AutomationError::new(
                "schedule_busy",
                "disable the schedule and wait for its active run before removing it",
            ));
        }
        let mut next = self.data.clone();
        next.schedules.retain(|entry| entry.schedule.id != id);
        self.commit(next)
    }

    pub fn claim_due(
        &mut self,
        now: u64,
    ) -> Result<Vec<ClaimedAutomationSchedule>, AutomationError> {
        self.ensure_writable()?;
        // Timer ticks without due work must not clone up to 32 MiB of jobs.
        if !self.data.schedules.iter().any(|entry| {
            entry.schedule.enabled
                && entry.schedule.active_run_id.is_none()
                && entry.schedule.next_run_at_ms.is_some_and(|due| due <= now)
        }) {
            return Ok(Vec::new());
        }
        let mut next = self.data.clone();
        let mut claims = Vec::new();
        for entry in &mut next.schedules {
            let schedule = &mut entry.schedule;
            if !schedule.enabled
                || schedule.active_run_id.is_some()
                || !schedule.next_run_at_ms.is_some_and(|due| due <= now)
            {
                continue;
            }
            let run_id = Uuid::new_v4();
            schedule.active_run_id = Some(run_id);
            entry.active_started_at_ms = Some(now);
            schedule.next_run_at_ms = future_slot(&schedule.definition, now);
            if schedule.next_run_at_ms.is_none() {
                schedule.enabled = false;
            }
            claims.push(ClaimedAutomationSchedule {
                schedule_id: schedule.id,
                run_id,
                job: schedule.definition.job.clone(),
                started_at_ms: now,
            });
        }
        if !claims.is_empty() {
            self.commit(next)?;
        }
        Ok(claims)
    }

    /// The supplied clock and persisted claim own the result timestamps. Only
    /// terminal status and compact error code/message are stored from result.
    pub fn finish(
        &mut self,
        id: Uuid,
        run_id: Uuid,
        mut result: AutomationScheduleResult,
        now: u64,
    ) -> Result<AutomationSchedule, AutomationError> {
        self.ensure_writable()?;
        if result.run_id != run_id || matches!(result.status, AutomationStatus::Pending) {
            return Err(AutomationError::new(
                "invalid_schedule_result",
                "result must be terminal and match the claimed run ID",
            ));
        }
        let mut next = self.data.clone();
        let entry = find_mut(&mut next, id)?;
        if entry.schedule.active_run_id != Some(run_id) {
            return Err(AutomationError::new(
                "schedule_run_mismatch",
                "only the current active run may finish this schedule",
            ));
        }
        result.started_at_ms = entry
            .active_started_at_ms
            .take()
            .expect("active claim has timestamp");
        result.finished_at_ms = now.max(result.started_at_ms);
        result.error = result.error.map(|error| {
            AutomationError::new(
                bounded_text(&error.code, 128),
                bounded_text(&error.message, 2048),
            )
        });
        entry.schedule.active_run_id = None;
        entry.schedule.last_result = Some(result);
        skip_missed(&mut entry.schedule, now);
        let schedule = entry.schedule.clone();
        self.commit(next)?;
        Ok(schedule)
    }

    fn ensure_writable(&self) -> Result<(), AutomationError> {
        if self.poisoned {
            return Err(AutomationError::new(
                "schedule_store_unavailable",
                "a schedule commit has uncertain durability; reopen the store before doing more work",
            ));
        }
        Ok(())
    }

    fn commit(&mut self, next: StoredSchedules) -> Result<(), AutomationError> {
        self.ensure_writable()?;
        validate_stored(&next)?;
        let bytes = serde_json::to_vec(&next)
            .map_err(|error| corrupt(format!("could not serialize schedules: {error}")))?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(corrupt("schedule store exceeds the size limit"));
        }
        let parent = parent_directory(&self.path);
        let temporary = parent.join(format!(".waku-schedules-{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let write = (|| -> std::io::Result<()> {
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(test)]
            if self.fail_before_rename {
                return Err(std::io::Error::other("injected pre-rename failure"));
            }
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if let Err(error) = write {
            let _ = fs::remove_file(&temporary);
            return Err(io_error("persist schedule state", error));
        }
        // Rename made the new state visible. Keep memory consistent even if
        // syncing the directory fails, then fail closed until restart.
        self.data = next;
        let sync = (|| -> std::io::Result<()> {
            #[cfg(test)]
            if self.fail_after_rename {
                return Err(std::io::Error::other("injected directory sync failure"));
            }
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = sync {
            self.poisoned = true;
            return Err(AutomationError::new(
                "schedule_commit_uncertain",
                format!(
                    "schedule state was replaced but durability could not be confirmed; no claim may execute: {error}"
                ),
            ));
        }
        Ok(())
    }
}

/// Shared admission bounds for both immediate and scheduled daemon jobs.
pub fn validate_automation_job(job: &AutomationJob) -> Result<(), AutomationError> {
    validate_name(&job.name, "job")?;
    if job.timeout_ms == 0 || job.timeout_ms > MAX_AUTOMATION_JOB_TIMEOUT_MS {
        return Err(invalid(format!(
            "job timeout must be 1..={MAX_AUTOMATION_JOB_TIMEOUT_MS} ms"
        )));
    }
    if job.programs.is_empty() || job.programs.len() > MAX_AUTOMATION_JOB_PROGRAMS {
        return Err(invalid(format!(
            "job must contain 1..={MAX_AUTOMATION_JOB_PROGRAMS} programs"
        )));
    }
    let bytes =
        serde_json::to_vec(job).map_err(|error| invalid(format!("invalid job: {error}")))?;
    if bytes.len() > MAX_AUTOMATION_JOB_BYTES {
        return Err(invalid("job exceeds the 1 MiB size limit"));
    }
    for program in &job.programs {
        let (name, count) = match program {
            AutomationProgram::Scenario { scenario } => (&scenario.name, scenario.steps.len()),
            AutomationProgram::Replay { replay, parameters } => {
                if replay.format_version != AUTOMATION_REPLAY_FORMAT_VERSION {
                    return Err(invalid("replay format version is unsupported"));
                }
                crate::automation_recording::preflight(replay, parameters)?;
                (&replay.name, replay.steps.len())
            }
        };
        validate_name(name, "program")?;
        if count == 0 || count > MAX_AUTOMATION_STEPS {
            return Err(invalid(format!(
                "each program must contain 1..={MAX_AUTOMATION_STEPS} steps"
            )));
        }
    }
    Ok(())
}

fn validate_definition(definition: &AutomationScheduleDefinition) -> Result<(), AutomationError> {
    validate_name(&definition.name, "schedule")?;
    validate_automation_job(&definition.job)?;
    if definition
        .interval_ms
        .is_some_and(|interval| interval < MIN_AUTOMATION_SCHEDULE_INTERVAL_MS)
    {
        return Err(invalid(format!(
            "schedule interval must be at least {MIN_AUTOMATION_SCHEDULE_INTERVAL_MS} ms"
        )));
    }
    Ok(())
}

fn validate_stored(data: &StoredSchedules) -> Result<(), AutomationError> {
    if data.format_version != STORE_VERSION || data.schedules.len() > MAX_AUTOMATION_SCHEDULES {
        return Err(corrupt("unsupported version or too many schedules"));
    }
    let mut ids = HashSet::new();
    let mut active_ids = HashSet::new();
    for entry in &data.schedules {
        let schedule = &entry.schedule;
        validate_definition(&schedule.definition).map_err(|error| corrupt(error.message))?;
        if schedule.id.is_nil() || !ids.insert(schedule.id) {
            return Err(corrupt("duplicate or nil schedule ID"));
        }
        if schedule.active_run_id.is_some() != entry.active_started_at_ms.is_some() {
            return Err(corrupt(
                "active run and its start timestamp must be present together",
            ));
        }
        if let Some(run_id) = schedule.active_run_id {
            if run_id.is_nil() || !active_ids.insert(run_id) {
                return Err(corrupt("duplicate or nil active run ID"));
            }
        }
        if schedule.enabled && schedule.next_run_at_ms.is_none() {
            return Err(corrupt("exhausted schedule cannot be enabled"));
        }
        if schedule.next_run_at_ms.is_none()
            && schedule.active_run_id.is_none()
            && schedule.last_result.is_none()
        {
            return Err(corrupt(
                "exhausted schedule must retain its claim or terminal result",
            ));
        }
        if let Some(next) = schedule.next_run_at_ms {
            let first = schedule.definition.first_run_at_ms;
            if next < first
                || match schedule.definition.interval_ms {
                    Some(interval) => (next - first) % interval != 0,
                    None => {
                        next != first
                            || schedule.active_run_id.is_some()
                            || schedule.last_result.is_some()
                    }
                }
            {
                return Err(corrupt(
                    "next run is inconsistent with the original cadence",
                ));
            }
        }
        if let Some(result) = &schedule.last_result {
            if result.run_id.is_nil()
                || Some(result.run_id) == schedule.active_run_id
                || matches!(result.status, AutomationStatus::Pending)
                || result.finished_at_ms < result.started_at_ms
            {
                return Err(corrupt("invalid terminal schedule result"));
            }
            if serde_json::to_vec(result)
                .map_err(|error| corrupt(error.to_string()))?
                .len()
                > MAX_AUTOMATION_SCHEDULE_RESULT_BYTES
            {
                return Err(corrupt("schedule result exceeds the size limit"));
            }
        }
    }
    Ok(())
}

fn validate_name(name: &str, kind: &str) -> Result<(), AutomationError> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(invalid(format!(
            "{kind} name must contain 1..={MAX_NAME_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn skip_missed(schedule: &mut AutomationSchedule, now: u64) {
    if schedule.next_run_at_ms.is_some_and(|next| next <= now) {
        schedule.next_run_at_ms = future_slot(&schedule.definition, now);
    }
    if schedule.next_run_at_ms.is_none() {
        schedule.enabled = false;
    }
}

fn future_slot(definition: &AutomationScheduleDefinition, now: u64) -> Option<u64> {
    let interval = definition.interval_ms?;
    if now < definition.first_run_at_ms {
        return Some(definition.first_run_at_ms);
    }
    // u128 prevents intermediate overflow. Overflow of the representable
    // clock simply exhausts the schedule instead of wrapping into a hot loop.
    let first = definition.first_run_at_ms as u128;
    let interval = interval as u128;
    let next = first + (((now as u128 - first) / interval) + 1) * interval;
    u64::try_from(next).ok()
}

fn find_mut(data: &mut StoredSchedules, id: Uuid) -> Result<&mut StoredSchedule, AutomationError> {
    data.schedules
        .iter_mut()
        .find(|entry| entry.schedule.id == id)
        .ok_or_else(|| not_found(id))
}
fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
fn bounded_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn invalid(message: impl Into<String>) -> AutomationError {
    AutomationError::new("invalid_schedule", message)
}
fn corrupt(message: impl Into<String>) -> AutomationError {
    AutomationError::new("corrupt_schedule_store", message)
}
fn not_found(id: Uuid) -> AutomationError {
    AutomationError::new("unknown_schedule", format!("schedule {id} does not exist"))
}
fn io_error(operation: &str, error: std::io::Error) -> AutomationError {
    AutomationError::new(
        "schedule_store_io",
        format!("could not {operation}: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use waku_protocol::{
        AutomationCall, AutomationScenario, AutomationScratchSeed, AutomationStep,
        AutomationStepOperation,
    };

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("waku-schedule-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("schedules.json")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn definition(first: u64, interval: Option<u64>) -> AutomationScheduleDefinition {
        AutomationScheduleDefinition {
            name: "scheduled smoke".into(),
            job: AutomationJob {
                name: "smoke".into(),
                programs: vec![AutomationProgram::Scenario {
                    scenario: AutomationScenario {
                        name: "observe".into(),
                        steps: vec![AutomationStep {
                            name: "describe".into(),
                            operation: AutomationStepOperation::Call {
                                call: AutomationCall::Describe,
                            },
                        }],
                    },
                }],
                seed: AutomationScratchSeed::default(),
                headless: true,
                timeout_ms: waku_protocol::DEFAULT_AUTOMATION_JOB_TIMEOUT_MS,
            },
            first_run_at_ms: first,
            interval_ms: interval,
        }
    }
    fn result(claim: &ClaimedAutomationSchedule) -> AutomationScheduleResult {
        AutomationScheduleResult {
            run_id: claim.run_id,
            status: AutomationStatus::Succeeded,
            started_at_ms: claim.started_at_ms,
            finished_at_ms: claim.started_at_ms,
            error: None,
        }
    }
    fn wire(schedule: &AutomationSchedule) -> serde_json::Value {
        serde_json::to_value(schedule).unwrap()
    }

    #[test]
    fn future_one_shot_is_claimed_exactly_once_and_finish_survives_reopen() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1000, None), 0).unwrap();
        assert!(store.claim_due(999).unwrap().is_empty());
        let claims = store.claim_due(1000).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].schedule_id, schedule.id);
        assert_eq!(claims[0].job.name, "smoke");
        assert!(store.claim_due(1000).unwrap().is_empty());
        let finished = store
            .finish(schedule.id, claims[0].run_id, result(&claims[0]), 1100)
            .unwrap();
        assert!(finished.active_run_id.is_none());
        assert!(finished.next_run_at_ms.is_none());
        assert!(!finished.enabled);
        assert_eq!(finished.last_result.as_ref().unwrap().finished_at_ms, 1100);
        assert!(store.claim_due(100_000).unwrap().is_empty());
        assert_eq!(
            store.set_enabled(schedule.id, true, 2000).unwrap_err().code,
            "schedule_exhausted"
        );
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 3000).unwrap();
        assert_eq!(wire(&reopened.get(schedule.id).unwrap()), wire(&finished));
        assert!(reopened.claim_due(3000).unwrap().is_empty());
    }

    #[test]
    fn missed_recurring_slots_coalesce_and_keep_original_cadence() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1500, Some(1000)), 0).unwrap();
        let claims = store.claim_due(10_700).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(store.get(schedule.id).unwrap().next_run_at_ms, Some(11_500));
        store
            .finish(schedule.id, claims[0].run_id, result(&claims[0]), 10_900)
            .unwrap();
        assert!(store.claim_due(11_499).unwrap().is_empty());
        assert_eq!(store.claim_due(11_500).unwrap().len(), 1);
    }

    #[test]
    fn long_running_job_never_overlaps_and_drops_missed_slots_at_finish() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1000, Some(1000)), 0).unwrap();
        let claim = store.claim_due(1000).unwrap().remove(0);
        assert!(store.claim_due(5000).unwrap().is_empty());
        assert!(store.claim_due(9000).unwrap().is_empty());
        let finished = store
            .finish(schedule.id, claim.run_id, result(&claim), 9000)
            .unwrap();
        assert_eq!(finished.next_run_at_ms, Some(10_000));
        assert!(store.claim_due(9000).unwrap().is_empty());
        assert_eq!(store.claim_due(10_000).unwrap().len(), 1);
    }

    #[test]
    fn interrupted_one_shot_is_visible_and_never_retried() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, None), 0).unwrap();
        let claim = store.claim_due(1000).unwrap().remove(0);
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 5000).unwrap();
        let recovered = reopened.get(schedule.id).unwrap();
        assert!(!recovered.enabled);
        assert!(recovered.active_run_id.is_none());
        assert!(recovered.next_run_at_ms.is_none());
        let last = recovered.last_result.unwrap();
        assert_eq!(last.run_id, claim.run_id);
        assert!(matches!(last.status, AutomationStatus::Failed));
        assert_eq!(last.started_at_ms, 1000);
        assert_eq!(last.finished_at_ms, 5000);
        assert_eq!(last.error.unwrap().code, "schedule_interrupted");
        assert!(reopened.claim_due(5000).unwrap().is_empty());
        drop(reopened);
        assert_eq!(
            AutomationScheduleStore::open(directory.path(), 6000)
                .unwrap()
                .get(schedule.id)
                .unwrap()
                .last_result
                .unwrap()
                .finished_at_ms,
            5000
        );
    }

    #[test]
    fn interrupted_recurring_job_resumes_only_at_next_future_slot() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1500, Some(2000)), 0).unwrap();
        store.claim_due(1500).unwrap();
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 9500).unwrap();
        let recovered = reopened.get(schedule.id).unwrap();
        assert!(recovered.enabled);
        assert_eq!(recovered.next_run_at_ms, Some(11_500));
        assert_eq!(
            recovered.last_result.unwrap().error.unwrap().code,
            "schedule_interrupted"
        );
        assert!(reopened.claim_due(9500).unwrap().is_empty());
        assert_eq!(reopened.claim_due(11_500).unwrap().len(), 1);
    }

    #[test]
    fn overdue_unclaimed_jobs_still_run_after_restart() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        store.create(definition(1000, None), 0).unwrap();
        store.create(definition(1000, Some(1000)), 0).unwrap();
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 7000).unwrap();
        assert_eq!(reopened.claim_due(7000).unwrap().len(), 2);
        assert!(reopened.claim_due(7000).unwrap().is_empty());
    }

    #[test]
    fn cancel_is_durable_without_losing_an_active_result_sink() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1000, Some(1000)), 0).unwrap();
        let claim = store.claim_due(1000).unwrap().remove(0);
        let cancelled = store.cancel(schedule.id, 1001).unwrap();
        assert!(!cancelled.enabled);
        assert_eq!(cancelled.active_run_id, Some(claim.run_id));
        assert_eq!(store.remove(schedule.id).unwrap_err().code, "schedule_busy");
        store
            .finish(schedule.id, claim.run_id, result(&claim), 5000)
            .unwrap();
        assert!(store.claim_due(6000).unwrap().is_empty());
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 6000).unwrap();
        assert!(!reopened.get(schedule.id).unwrap().enabled);
        assert!(reopened.get(schedule.id).unwrap().last_result.is_some());
        reopened.remove(schedule.id).unwrap();
        assert!(reopened.list().is_empty());
    }

    #[test]
    fn cancelled_interrupted_run_stays_cancelled_on_restart() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, Some(1000)), 0).unwrap();
        store.claim_due(0).unwrap();
        store.cancel(schedule.id, 0).unwrap();
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 9000).unwrap();
        assert!(!reopened.get(schedule.id).unwrap().enabled);
        assert!(reopened.claim_due(10_000).unwrap().is_empty());
    }

    #[test]
    fn reenable_recurring_schedule_skips_disabled_window() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(500, Some(1000)), 0).unwrap();
        store.cancel(schedule.id, 0).unwrap();
        let enabled = store.set_enabled(schedule.id, true, 3500).unwrap();
        assert_eq!(enabled.next_run_at_ms, Some(4500));
        assert!(store.claim_due(3500).unwrap().is_empty());
        assert_eq!(store.claim_due(4500).unwrap().len(), 1);
    }

    #[test]
    fn cancellation_prevents_future_one_shot_without_consuming_it() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1000, None), 0).unwrap();
        store.cancel(schedule.id, 0).unwrap();
        assert!(store.claim_due(1000).unwrap().is_empty());
        store.set_enabled(schedule.id, true, 2000).unwrap();
        assert_eq!(store.claim_due(2000).unwrap().len(), 1);
    }

    #[test]
    fn wrong_or_pending_finish_cannot_release_or_overwrite_a_claim() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, Some(1000)), 0).unwrap();
        let claim = store.claim_due(0).unwrap().remove(0);
        let mut wrong = result(&claim);
        wrong.run_id = Uuid::new_v4();
        assert_eq!(
            store
                .finish(schedule.id, wrong.run_id, wrong, 1)
                .unwrap_err()
                .code,
            "schedule_run_mismatch"
        );
        let mut pending = result(&claim);
        pending.status = AutomationStatus::Pending;
        assert_eq!(
            store
                .finish(schedule.id, claim.run_id, pending, 1)
                .unwrap_err()
                .code,
            "invalid_schedule_result"
        );
        assert_eq!(
            store.get(schedule.id).unwrap().active_run_id,
            Some(claim.run_id)
        );
        store
            .finish(schedule.id, claim.run_id, result(&claim), 1)
            .unwrap();
        assert_eq!(
            store
                .finish(schedule.id, claim.run_id, result(&claim), 2)
                .unwrap_err()
                .code,
            "schedule_run_mismatch"
        );
    }

    #[test]
    fn atomic_failure_does_not_publish_new_schedule_or_claim() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, None), 0).unwrap();
        let before = fs::read(directory.path()).unwrap();
        store.fail_before_rename = true;
        assert_eq!(
            store.create(definition(0, None), 0).unwrap_err().code,
            "schedule_store_io"
        );
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.claim_due(0).unwrap_err().code, "schedule_store_io");
        assert!(store.get(schedule.id).unwrap().active_run_id.is_none());
        assert_eq!(fs::read(directory.path()).unwrap(), before);
        assert!(!fs::read_dir(&directory.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "tmp")
        }));
        store.fail_before_rename = false;
        assert_eq!(store.claim_due(0).unwrap().len(), 1);
    }

    #[test]
    fn failed_finish_keeps_durable_claim_until_successful_retry() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, None), 0).unwrap();
        let claim = store.claim_due(0).unwrap().remove(0);
        let before = fs::read(directory.path()).unwrap();
        store.fail_before_rename = true;
        assert_eq!(
            store
                .finish(schedule.id, claim.run_id, result(&claim), 1)
                .unwrap_err()
                .code,
            "schedule_store_io"
        );
        assert_eq!(fs::read(directory.path()).unwrap(), before);
        assert_eq!(
            store.get(schedule.id).unwrap().active_run_id,
            Some(claim.run_id)
        );
        store.fail_before_rename = false;
        store
            .finish(schedule.id, claim.run_id, result(&claim), 2)
            .unwrap();
        assert!(store.get(schedule.id).unwrap().active_run_id.is_none());
    }

    #[test]
    fn uncertain_post_rename_commit_fails_closed_and_is_recovered_as_interrupted() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, None), 0).unwrap();
        store.fail_after_rename = true;
        assert_eq!(
            store.claim_due(0).unwrap_err().code,
            "schedule_commit_uncertain"
        );
        assert_eq!(
            store.claim_due(0).unwrap_err().code,
            "schedule_store_unavailable"
        );
        drop(store);
        let mut reopened = AutomationScheduleStore::open(directory.path(), 1).unwrap();
        assert_eq!(
            reopened
                .get(schedule.id)
                .unwrap()
                .last_result
                .unwrap()
                .error
                .unwrap()
                .code,
            "schedule_interrupted"
        );
        assert!(reopened.claim_due(1).unwrap().is_empty());
    }

    #[test]
    fn second_store_cannot_open_the_same_file_until_owner_drops() {
        let directory = TestDirectory::new();
        let store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let error = AutomationScheduleStore::open(directory.path(), 0)
            .err()
            .unwrap();
        assert_eq!(error.code, "schedule_store_locked");
        drop(store);
        assert!(AutomationScheduleStore::open(directory.path(), 0).is_ok());
    }

    #[test]
    fn corrupt_or_unknown_version_never_replaces_original_bytes() {
        for (bytes, code) in [
            ("{broken", "corrupt_schedule_store"),
            (
                r#"{"formatVersion":2,"schedules":[]}"#,
                "unsupported_schedule_store_version",
            ),
            (r#"{"schedules":[]}"#, "unsupported_schedule_store_version"),
            (
                r#"{"formatVersion":1,"schedules":[],"unexpected":true}"#,
                "corrupt_schedule_store",
            ),
        ] {
            let directory = TestDirectory::new();
            fs::write(directory.path(), bytes).unwrap();
            let error = AutomationScheduleStore::open(directory.path(), 0)
                .err()
                .unwrap();
            assert_eq!(error.code, code);
            assert_eq!(fs::read_to_string(directory.path()).unwrap(), bytes);
        }
    }

    #[test]
    fn oversized_store_is_rejected_before_reading_or_replacing_it() {
        let directory = TestDirectory::new();
        let file = File::create(directory.path()).unwrap();
        file.set_len(MAX_STORE_BYTES as u64 + 1).unwrap();
        drop(file);
        assert_eq!(
            AutomationScheduleStore::open(directory.path(), 0)
                .err()
                .unwrap()
                .code,
            "corrupt_schedule_store"
        );
        assert_eq!(
            fs::metadata(directory.path()).unwrap().len(),
            MAX_STORE_BYTES as u64 + 1
        );
    }

    #[test]
    fn duplicate_schedule_ids_and_inconsistent_active_state_are_corrupt() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        store.create(definition(0, None), 0).unwrap();
        let mut data = store.data.clone();
        data.schedules.push(data.schedules[0].clone());
        drop(store);
        fs::write(directory.path(), serde_json::to_vec(&data).unwrap()).unwrap();
        assert_eq!(
            AutomationScheduleStore::open(directory.path(), 0)
                .err()
                .unwrap()
                .code,
            "corrupt_schedule_store"
        );
        data.schedules.pop();
        data.schedules[0].active_started_at_ms = Some(0);
        fs::write(directory.path(), serde_json::to_vec(&data).unwrap()).unwrap();
        assert_eq!(
            AutomationScheduleStore::open(directory.path(), 0)
                .err()
                .unwrap()
                .code,
            "corrupt_schedule_store"
        );
    }

    #[test]
    fn admission_bounds_reject_invalid_jobs_without_losing_existing_schedules() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        assert_eq!(
            store.create(definition(0, Some(999)), 0).unwrap_err().code,
            "invalid_schedule"
        );
        let mut empty = definition(0, None);
        empty.job.programs.clear();
        assert!(store.create(empty, 0).is_err());
        let mut excessive = definition(0, None);
        excessive.job.programs =
            vec![excessive.job.programs[0].clone(); MAX_AUTOMATION_JOB_PROGRAMS + 1];
        assert!(store.create(excessive, 0).is_err());
        let mut huge = definition(0, None);
        let AutomationProgram::Scenario { scenario } = &mut huge.job.programs[0] else {
            unreachable!()
        };
        scenario.steps[0].operation = AutomationStepOperation::Call {
            call: AutomationCall::Type {
                text: "x".repeat(MAX_AUTOMATION_JOB_BYTES),
                target: None,
            },
        };
        assert!(store.create(huge, 0).is_err());
        for _ in 0..MAX_AUTOMATION_SCHEDULES {
            store.create(definition(0, None), 0).unwrap();
        }
        let before = fs::read(directory.path()).unwrap();
        assert_eq!(
            store.create(definition(0, None), 0).unwrap_err().code,
            "schedule_limit"
        );
        assert_eq!(store.list().len(), MAX_AUTOMATION_SCHEDULES);
        assert_eq!(fs::read(directory.path()).unwrap(), before);
    }

    #[test]
    fn replay_parameters_are_validated_before_a_schedule_is_persisted() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let mut definition = definition(0, None);
        definition.job.programs = vec![serde_json::from_value(serde_json::json!({
            "type":"replay", "parameters":{},
            "replay": {
                "formatVersion":1, "name":"replay",
                "steps":[{"name":"type", "type":"call", "call":{"type":"type", "text":"", "target":null},
                    "bindings":[{"pointer":"/call/text", "source":{"type":"parameter", "name":"message"}}]}]
            }
        })).unwrap()];
        assert_eq!(
            store.create(definition, 0).unwrap_err().code,
            "missing_parameter"
        );
        assert!(store.list().is_empty());
        assert!(!directory.path().exists());
    }

    #[test]
    fn durable_error_is_compact_utf8_and_strips_failure_observations() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(0, None), 0).unwrap();
        let claim = store.claim_due(0).unwrap().remove(0);
        let mut failed = result(&claim);
        failed.status = AutomationStatus::Failed;
        failed.error = Some(AutomationError::new(
            "bad".repeat(1000),
            "🦀".repeat(100_000),
        ));
        let done = store.finish(schedule.id, claim.run_id, failed, 1).unwrap();
        let error = done.last_result.as_ref().unwrap().error.as_ref().unwrap();
        assert!(error.code.len() <= 128);
        assert!(error.message.len() <= 2048);
        assert!(error.message.ends_with('…'));
        assert!(
            serde_json::to_vec(&done.last_result).unwrap().len()
                <= MAX_AUTOMATION_SCHEDULE_RESULT_BYTES
        );
    }

    #[test]
    fn clock_overflow_exhausts_cadence_instead_of_creating_a_hot_loop() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store
            .create(definition(u64::MAX - 1000, Some(1000)), 0)
            .unwrap();
        let first = store.claim_due(u64::MAX - 1000).unwrap().remove(0);
        store
            .finish(schedule.id, first.run_id, result(&first), u64::MAX - 999)
            .unwrap();
        let last = store.claim_due(u64::MAX).unwrap().remove(0);
        store
            .finish(schedule.id, last.run_id, result(&last), u64::MAX)
            .unwrap();
        assert!(store.get(schedule.id).unwrap().next_run_at_ms.is_none());
        assert!(!store.get(schedule.id).unwrap().enabled);
        assert!(store.claim_due(u64::MAX).unwrap().is_empty());
    }

    #[test]
    fn backward_clock_does_not_move_pending_slot_back_or_invert_result_times() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let schedule = store.create(definition(1000, Some(1000)), 0).unwrap();
        let claim = store.claim_due(5000).unwrap().remove(0);
        let done = store
            .finish(schedule.id, claim.run_id, result(&claim), 4000)
            .unwrap();
        assert_eq!(done.next_run_at_ms, Some(6000));
        assert_eq!(done.last_result.unwrap().finished_at_ms, 5000);
        assert!(store.claim_due(4000).unwrap().is_empty());
    }

    #[test]
    fn cancelling_or_removing_unknown_schedule_is_explicit() {
        let directory = TestDirectory::new();
        let mut store = AutomationScheduleStore::open(directory.path(), 0).unwrap();
        let id = Uuid::new_v4();
        assert_eq!(store.get(id).unwrap_err().code, "unknown_schedule");
        assert_eq!(store.cancel(id, 0).unwrap_err().code, "unknown_schedule");
        assert_eq!(store.remove(id).unwrap_err().code, "unknown_schedule");
    }
}
