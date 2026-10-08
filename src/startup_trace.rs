//! Opt-in collection of startup and reopen milestones.
//!
//! Setting `WAKU_STARTUP_TRACE` turns the trace on: `1` or `stderr` writes the
//! milestone lines to stderr, and any other value is treated as a file path to
//! append them to. The app writes one line per completed launch or rebuild —
//! see [`crate::latency`] for the format and the budgets — so a harness can
//! read a cold launch and a reopen out of the same process. Tracing is off
//! everywhere else, and a disabled trace costs one branch per hook.
//!
//! The collector is an application global because a window is disposable: a
//! rebuilt window has to join the run the process already started, and the
//! run has to survive the window that opened it. Milestones are stamped
//! relative to [`mark_process_start`], which `run` calls before it touches
//! GPUI, so "process start" means the same thing for a launch and a rebuild.
//!
//! The line is handed to a writer thread rather than written from the frame
//! that reached `Interactive`: tracing may not put file I/O on the UI thread.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};
use std::time::Instant;

use gpui::{App, Global};

use crate::latency::{Milestone, RunKind, TraceRun};

/// Enables the trace. `1` or `stderr` selects stderr; anything else is a file
/// path to append to.
pub const TRACE_ENV: &str = "WAKU_STARTUP_TRACE";

/// Development aid for the latency harness: when set, the app closes its
/// window once a cold launch has been traced, so the harness can drive a
/// rebuild with `open -g` and measure it. Without it a launch leaves its
/// window open, which is what a user wants.
pub const CLOSE_AFTER_LAUNCH_ENV: &str = "WAKU_STARTUP_TRACE_CLOSE_AFTER_LAUNCH";

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Stamp the moment the process began running our code. `run` calls this
/// before it builds GPUI so a cold launch is measured from the earliest point
/// available to the app.
pub fn mark_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}

fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// Where a completed run's line goes.
enum Sink {
    Stderr,
    File(PathBuf),
}

/// The application's startup trace.
pub struct StartupTrace {
    process_start: Instant,
    enabled: bool,
    close_after_launch: bool,
    state: RefCell<TraceState>,
    writer: Option<Sender<String>>,
}

#[derive(Default)]
struct TraceState {
    /// The run currently being observed, if any.
    current: Option<TraceRun>,
    /// Runs that reached `Interactive`, in completion order.
    completed: Vec<TraceRun>,
}

impl Global for StartupTrace {}

impl StartupTrace {
    /// The trace the application runs with, on unless `WAKU_STARTUP_TRACE` is
    /// set.
    pub fn from_env() -> Self {
        let value = std::env::var(TRACE_ENV).ok();
        let sink = match value.as_deref().map(str::trim) {
            None | Some("") => None,
            Some("1") | Some("stderr") => Some(Sink::Stderr),
            Some(path) => Some(Sink::File(PathBuf::from(path))),
        };
        let close_after_launch =
            std::env::var_os(CLOSE_AFTER_LAUNCH_ENV).is_some_and(|value| !value.is_empty());
        Self::new(process_start(), sink, close_after_launch)
    }

    /// A trace that collects runs without writing them anywhere. Only tests
    /// need this: the application either traces to a sink or not at all.
    #[cfg(test)]
    pub fn collecting(process_start: Instant) -> Self {
        let mut trace = Self::new(process_start, None, false);
        trace.enabled = true;
        trace
    }

    fn new(process_start: Instant, sink: Option<Sink>, close_after_launch: bool) -> Self {
        let writer = sink.map(spawn_writer);
        let mut state = TraceState::default();
        state.begin(RunKind::Cold, 0.0);
        Self {
            process_start,
            enabled: writer.is_some(),
            close_after_launch,
            state: RefCell::new(state),
            writer,
        }
    }

    /// Whether tracing is on. A disabled trace records nothing.
    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Note that a window is opening. The first window continues the cold run
    /// the process started with; a window opened after that begins a reopen
    /// run measured from this moment.
    pub fn note_window_open(&self) {
        self.note_window_open_at(Instant::now());
    }

    fn note_window_open_at(&self, at: Instant) {
        if !self.enabled {
            return;
        }
        let start_ms = self.elapsed_ms(at);
        let mut state = self.state.borrow_mut();
        let continues_cold_run = state.current.as_ref().is_some_and(|run| {
            run.kind == RunKind::Cold && run.reached(Milestone::WindowOpen).is_none()
        });
        if !continues_cold_run {
            state.begin(RunKind::Reopen, start_ms);
        }
        if let Some(run) = state.current.as_mut() {
            run.record(Milestone::WindowOpen, start_ms);
        }
    }

    /// Record a milestone for the current run. Returns the kind of the run
    /// that just completed, if this call was its `Interactive`.
    pub fn record(&self, milestone: Milestone) -> Option<RunKind> {
        self.record_at(milestone, Instant::now())
    }

    fn record_at(&self, milestone: Milestone, at: Instant) -> Option<RunKind> {
        if !self.enabled {
            return None;
        }
        let mut state = self.state.borrow_mut();
        let run = state.current.as_mut()?;
        run.record(milestone, self.elapsed_ms(at));
        if milestone != Milestone::Interactive {
            return None;
        }
        let run = state
            .current
            .take()
            .expect("the current run was just borrowed");
        let kind = run.kind;
        if let Some(writer) = &self.writer {
            let _ = writer.send(run.line(std::process::id()));
        }
        state.completed.push(run);
        Some(kind)
    }

    /// Runs that have reached `Interactive`, in completion order.
    #[cfg(test)]
    pub fn completed_runs(&self) -> Vec<TraceRun> {
        self.state.borrow().completed.clone()
    }

    /// The run still being observed, if any.
    #[cfg(test)]
    pub fn current_run(&self) -> Option<TraceRun> {
        self.state.borrow().current.clone()
    }

    /// Whether a cold launch should close its window so a harness can measure
    /// the rebuild.
    pub fn close_after_launch(&self) -> bool {
        self.close_after_launch
    }

    fn elapsed_ms(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.process_start)
            .as_secs_f64()
            * 1000.0
    }
}

impl TraceState {
    fn begin(&mut self, kind: RunKind, start_ms: f64) {
        let index = self.completed.len() as u32 + self.current.iter().count() as u32;
        self.current = Some(TraceRun::new(index, kind, start_ms));
    }
}

/// Record a milestone for the application's trace, if it has one.
pub fn record(cx: &App, milestone: Milestone) -> Option<RunKind> {
    cx.try_global::<StartupTrace>()
        .and_then(|trace| trace.record(milestone))
}

/// Note that a window is opening for the application's trace.
pub fn note_window_open(cx: &App) {
    if let Some(trace) = cx.try_global::<StartupTrace>() {
        trace.note_window_open();
    }
}

/// Whether the trace asked the app to close its window after a cold launch.
pub fn close_after_launch(cx: &App) -> bool {
    cx.try_global::<StartupTrace>()
        .is_some_and(StartupTrace::close_after_launch)
}

/// Write trace lines off the UI thread, one file handle for the process.
fn spawn_writer(sink: Sink) -> Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    let spawned = std::thread::Builder::new()
        .name("waku-startup-trace".into())
        .spawn(move || match sink {
            Sink::Stderr => {
                for line in rx {
                    eprintln!("{line}");
                }
            }
            Sink::File(path) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(mut file) => {
                    for line in rx {
                        if writeln!(file, "{line}").is_err() {
                            break;
                        }
                    }
                }
                Err(error) => eprintln!(
                    "waku startup trace: could not open {}: {error}",
                    path.display()
                ),
            },
        });
    if let Err(error) = spawned {
        eprintln!("waku startup trace: could not start its writer thread: {error}");
    }
    tx
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{StartupTrace, process_start};
    use crate::latency::{Milestone, RunKind};

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn a_cold_launch_collects_every_milestone_once() {
        let base = Instant::now();
        let trace = StartupTrace::collecting(base);

        trace.note_window_open_at(base + ms(12));
        trace.record_at(Milestone::FirstFrame, base + ms(30));
        trace.record_at(Milestone::DaemonReady, base + ms(210));
        trace.record_at(Milestone::TasksHydrated, base + ms(240));
        // Renders repeat; only the first observation may land.
        trace.record_at(Milestone::FirstFrame, base + ms(45));
        let completed = trace.record_at(Milestone::Interactive, base + ms(260));

        assert_eq!(completed, Some(RunKind::Cold));
        let runs = trace.completed_runs();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.index, 0);
        assert_eq!(run.kind, RunKind::Cold);
        assert_eq!(run.start_ms, 0.0);
        assert_eq!(run.missing(), Vec::new());
        assert_eq!(run.reached(Milestone::WindowOpen), Some(12.0));
        assert_eq!(run.reached(Milestone::FirstFrame), Some(30.0));
        assert_eq!(run.reached(Milestone::DaemonReady), Some(210.0));
        assert_eq!(run.reached(Milestone::TasksHydrated), Some(240.0));
        assert_eq!(run.reached(Milestone::Interactive), Some(260.0));
        assert_eq!(run.latency_ms(), Some(260.0));
    }

    #[test]
    fn a_rebuild_starts_a_new_run_measured_from_its_activation() {
        let base = Instant::now();
        let trace = StartupTrace::collecting(base);

        trace.note_window_open_at(base + ms(1));
        trace.record_at(Milestone::Interactive, base + ms(500));

        // The window closed and Dock activation rebuilt it.
        trace.note_window_open_at(base + ms(30_000));
        trace.record_at(Milestone::DaemonReady, base + ms(30_003));
        trace.record_at(Milestone::FirstFrame, base + ms(30_010));
        trace.record_at(Milestone::TasksHydrated, base + ms(30_040));
        let completed = trace.record_at(Milestone::Interactive, base + ms(30_050));

        assert_eq!(completed, Some(RunKind::Reopen));
        let runs = trace.completed_runs();
        assert_eq!(runs.len(), 2);
        let reopen = &runs[1];
        assert_eq!(reopen.index, 1);
        assert_eq!(reopen.kind, RunKind::Reopen);
        assert_eq!(reopen.start_ms, 30_000.0);
        assert_eq!(reopen.reached(Milestone::ProcessStart), Some(0.0));
        assert_eq!(reopen.reached(Milestone::WindowOpen), Some(30_000.0));
        assert_eq!(reopen.missing(), Vec::new());
        assert_eq!(reopen.latency_ms(), Some(50.0));
    }

    #[test]
    fn a_disabled_trace_records_nothing() {
        let trace = StartupTrace::new(Instant::now(), None, false);
        assert!(!trace.enabled());

        trace.note_window_open();
        assert_eq!(trace.record(Milestone::Interactive), None);
        assert_eq!(trace.completed_runs(), Vec::new());
    }

    #[test]
    fn the_global_process_start_is_stable() {
        mark_process_start_once();

        assert_eq!(process_start(), process_start());
    }

    /// `mark_process_start` is idempotent, which is what lets `run` and
    /// `from_env` both call it without moving the origin.
    fn mark_process_start_once() {
        super::mark_process_start();
    }
}
