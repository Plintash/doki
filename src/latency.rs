//! Startup and reopen latency milestones and the budgets they must meet.
//!
//! The desktop records one [`TraceRun`] per launch or window rebuild while
//! `WAKU_STARTUP_TRACE` is set (see `crate::startup_trace`), and the latency
//! harness reads those lines back, parses them with [`ParsedTrace::parse`],
//! and compares each run against [`BUDGETS`]. This module is deliberately free
//! of GPUI so the collection and comparison stay unit-testable.
//!
//! A run's milestones are reported in milliseconds since the process started,
//! and `start_ms` marks the activation that produced the run: zero for a cold
//! launch, the moment the reopen was requested for a rebuild. A run's latency
//! is therefore `interactive - start`, which is what the budgets bound.

use std::collections::BTreeMap;

/// One observable step of a launch or a rebuild, in the order the app reaches
/// them. `ProcessStart` is shared by every run in the process; the rest are
/// per run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Milestone {
    /// The process began running our code. Reported at zero for every run.
    ProcessStart,
    /// A window was opened (not focused) for this run.
    WindowOpen,
    /// The window painted its first frame.
    FirstFrame,
    /// The application's daemon connection answered.
    DaemonReady,
    /// The workspace finished loading task state from the daemon.
    TasksHydrated,
    /// The first frame showing the hydrated workspace was painted.
    Interactive,
}

impl Milestone {
    /// Every milestone a completed run reports, in order.
    pub const ALL: [Milestone; 6] = [
        Milestone::ProcessStart,
        Milestone::WindowOpen,
        Milestone::FirstFrame,
        Milestone::DaemonReady,
        Milestone::TasksHydrated,
        Milestone::Interactive,
    ];

    /// The `key=value` name used in a trace line, without the `_ms` suffix.
    pub fn key(self) -> &'static str {
        match self {
            Milestone::ProcessStart => "process_start",
            Milestone::WindowOpen => "window_open",
            Milestone::FirstFrame => "first_frame",
            Milestone::DaemonReady => "daemon_ready",
            Milestone::TasksHydrated => "tasks_hydrated",
            Milestone::Interactive => "interactive",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Milestone::ALL.into_iter().find(|it| it.key() == key)
    }
}

/// Which activation produced a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    /// The first window of a process.
    Cold,
    /// A window rebuilt after the previous one closed.
    Reopen,
}

impl RunKind {
    pub fn label(self) -> &'static str {
        match self {
            RunKind::Cold => "cold",
            RunKind::Reopen => "reopen",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label {
            "cold" => Some(RunKind::Cold),
            "reopen" => Some(RunKind::Reopen),
            _ => None,
        }
    }
}

/// The milestones one launch or rebuild reached, in process-relative
/// milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceRun {
    /// Runs are numbered from zero in the order they complete.
    pub index: u32,
    pub kind: RunKind,
    /// Milliseconds since process start at which this run's activation
    /// happened: zero for a cold launch.
    pub start_ms: f64,
    milestones: BTreeMap<Milestone, f64>,
}

impl TraceRun {
    /// A run whose process began `start_ms` after the process started.
    ///
    /// `ProcessStart` is recorded immediately: it belongs to every run, and a
    /// rebuild reports it even though the rebuild happened long after.
    pub fn new(index: u32, kind: RunKind, start_ms: f64) -> Self {
        let mut milestones = BTreeMap::new();
        milestones.insert(Milestone::ProcessStart, 0.0);
        Self {
            index,
            kind,
            start_ms,
            milestones,
        }
    }

    /// Record a milestone. The first observation wins, so a repeated frame
    /// cannot move a milestone that has already happened.
    pub fn record(&mut self, milestone: Milestone, at_ms: f64) {
        self.milestones.entry(milestone).or_insert(at_ms);
    }

    /// When this milestone was observed, or `None` if the run never reached
    /// it.
    pub fn reached(&self, milestone: Milestone) -> Option<f64> {
        self.milestones.get(&milestone).copied()
    }

    /// Milestones this run has not reached yet, in order.
    pub fn missing(&self) -> Vec<Milestone> {
        Milestone::ALL
            .into_iter()
            .filter(|milestone| !self.milestones.contains_key(milestone))
            .collect()
    }

    /// How long the run took to become interactive, or `None` if it never did.
    pub fn latency_ms(&self) -> Option<f64> {
        self.reached(Milestone::Interactive)
            .map(|interactive| interactive - self.start_ms)
    }

    /// The one-line record the harness parses.
    pub fn line(&self, pid: u32) -> String {
        let mut line = format!(
            "startup-trace pid={pid} run={} kind={} start_ms={:.3}",
            self.index,
            self.kind.label(),
            self.start_ms
        );
        for milestone in Milestone::ALL {
            if let Some(at) = self.reached(milestone) {
                line.push_str(&format!(" {}_ms={:.3}", milestone.key(), at));
            }
        }
        line
    }
}

/// A trace line together with the process that wrote it.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedTrace {
    pub pid: u32,
    pub run: TraceRun,
}

impl ParsedTrace {
    /// Read back a line written by [`TraceRun::line`].
    pub fn parse(line: &str) -> Option<ParsedTrace> {
        let mut fields = line.split_whitespace();
        if fields.next()? != "startup-trace" {
            return None;
        }
        let mut pid = None;
        let mut index = None;
        let mut kind = None;
        let mut start_ms = None;
        let mut milestones = BTreeMap::new();
        for field in fields {
            let (key, value) = field.split_once('=')?;
            match key {
                "pid" => pid = Some(value.parse().ok()?),
                "run" => index = Some(value.parse().ok()?),
                "kind" => kind = Some(RunKind::from_label(value)?),
                "start_ms" => start_ms = Some(value.parse().ok()?),
                key => {
                    let milestone = Milestone::from_key(key.strip_suffix("_ms")?)?;
                    milestones.insert(milestone, value.parse().ok()?);
                }
            }
        }
        let mut run = TraceRun::new(index?, kind?, start_ms?);
        run.milestones = milestones;
        Some(ParsedTrace { pid: pid?, run })
    }
}

/// The latency budgets, in milliseconds, derived from the recorded baselines in
/// `docs/performance.md`. Cold launch pays for process startup, the window, the
/// daemon connection, and the first state load; a rebuild reuses the running
/// daemon and pays only for the window and the reload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budgets {
    pub cold_launch_ms: f64,
    pub reopen_ms: f64,
}

/// The budgets the latency harness enforces. Keep in step with the baselines
/// and budget table in `docs/performance.md`.
///
/// Debug-build baselines on the reference machine (2026-10-08): cold launch
/// 271-933 ms (the first launch after a build is the slow one), reopen
/// 74-111 ms. The budgets are roughly three to five times the worst observed
/// run, which is loose enough for a cold page cache and tight enough that
/// reintroducing a blocking daemon spawn or state load on the reopen path
/// fails.
pub const BUDGETS: Budgets = Budgets {
    cold_launch_ms: 3000.0,
    reopen_ms: 500.0,
};

impl Budgets {
    /// The budget a run of `kind` must meet.
    pub fn limit_for(&self, kind: RunKind) -> f64 {
        match kind {
            RunKind::Cold => self.cold_launch_ms,
            RunKind::Reopen => self.reopen_ms,
        }
    }

    /// Compare a completed run against its budget.
    pub fn check(&self, run: &TraceRun) -> BudgetCheck {
        let budget_ms = self.limit_for(run.kind);
        match run.latency_ms() {
            None => BudgetCheck::Incomplete {
                kind: run.kind,
                missing: run.missing(),
            },
            Some(measured_ms) if measured_ms > budget_ms => BudgetCheck::Overrun {
                kind: run.kind,
                measured_ms,
                budget_ms,
            },
            Some(measured_ms) => BudgetCheck::Within {
                kind: run.kind,
                measured_ms,
                budget_ms,
            },
        }
    }
}

/// What a budget comparison found.
#[derive(Clone, Debug, PartialEq)]
pub enum BudgetCheck {
    /// The run reached interactive inside its budget.
    Within {
        kind: RunKind,
        measured_ms: f64,
        budget_ms: f64,
    },
    /// The run took longer than its budget.
    Overrun {
        kind: RunKind,
        measured_ms: f64,
        budget_ms: f64,
    },
    /// The run never reached interactive, so it cannot be judged.
    Incomplete {
        kind: RunKind,
        missing: Vec<Milestone>,
    },
}

impl BudgetCheck {
    /// Whether this result should fail a latency run.
    pub fn is_failure(&self) -> bool {
        !matches!(self, BudgetCheck::Within { .. })
    }

    /// A one-line human summary.
    pub fn summary(&self) -> String {
        match self {
            BudgetCheck::Within {
                kind,
                measured_ms,
                budget_ms,
            } => format!(
                "{} within budget: {measured_ms:.0} ms <= {budget_ms:.0} ms",
                kind.label()
            ),
            BudgetCheck::Overrun {
                kind,
                measured_ms,
                budget_ms,
            } => format!(
                "{} over budget: {measured_ms:.0} ms > {budget_ms:.0} ms",
                kind.label()
            ),
            BudgetCheck::Incomplete { kind, missing } => {
                let names = missing
                    .iter()
                    .map(|milestone| milestone.key())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} incomplete: never reached {names}", kind.label())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BudgetCheck, Budgets, Milestone, ParsedTrace, RunKind, TraceRun};

    fn cold_run() -> TraceRun {
        let mut run = TraceRun::new(0, RunKind::Cold, 0.0);
        run.record(Milestone::WindowOpen, 12.0);
        run.record(Milestone::FirstFrame, 30.0);
        run.record(Milestone::DaemonReady, 210.0);
        run.record(Milestone::TasksHydrated, 240.0);
        run.record(Milestone::Interactive, 260.0);
        run
    }

    #[test]
    fn a_cold_launch_reports_every_milestone_and_its_latency() {
        let run = cold_run();

        assert_eq!(run.missing(), Vec::new());
        assert_eq!(run.reached(Milestone::ProcessStart), Some(0.0));
        assert_eq!(run.reached(Milestone::WindowOpen), Some(12.0));
        assert_eq!(run.reached(Milestone::FirstFrame), Some(30.0));
        assert_eq!(run.reached(Milestone::DaemonReady), Some(210.0));
        assert_eq!(run.reached(Milestone::TasksHydrated), Some(240.0));
        assert_eq!(run.reached(Milestone::Interactive), Some(260.0));
        assert_eq!(run.latency_ms(), Some(260.0));
    }

    #[test]
    fn a_milestone_is_recorded_once_so_a_later_frame_cannot_move_it() {
        let mut run = TraceRun::new(0, RunKind::Cold, 0.0);
        run.record(Milestone::FirstFrame, 30.0);
        run.record(Milestone::FirstFrame, 90.0);

        assert_eq!(run.reached(Milestone::FirstFrame), Some(30.0));
    }

    #[test]
    fn a_reopen_run_keeps_the_process_start_and_measures_from_its_own_start() {
        let mut run = TraceRun::new(1, RunKind::Reopen, 30_000.0);
        run.record(Milestone::WindowOpen, 30_004.0);
        run.record(Milestone::FirstFrame, 30_011.0);
        run.record(Milestone::DaemonReady, 30_005.0);
        run.record(Milestone::TasksHydrated, 30_040.0);
        run.record(Milestone::Interactive, 30_050.0);

        assert_eq!(run.missing(), Vec::new());
        assert_eq!(run.reached(Milestone::ProcessStart), Some(0.0));
        assert_eq!(run.latency_ms(), Some(50.0));
    }

    #[test]
    fn a_trace_line_round_trips_through_its_parser() {
        let run = cold_run();
        let line = run.line(4242);

        let parsed = ParsedTrace::parse(&line).expect("a line written by the app parses back");
        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.run, run);
        assert_eq!(
            line,
            "startup-trace pid=4242 run=0 kind=cold start_ms=0.000 \
             process_start_ms=0.000 window_open_ms=12.000 first_frame_ms=30.000 \
             daemon_ready_ms=210.000 tasks_hydrated_ms=240.000 interactive_ms=260.000"
        );
    }

    #[test]
    fn an_incomplete_run_reports_the_milestones_it_never_reached() {
        let mut run = TraceRun::new(0, RunKind::Cold, 0.0);
        run.record(Milestone::WindowOpen, 12.0);

        assert_eq!(
            run.missing(),
            vec![
                Milestone::FirstFrame,
                Milestone::DaemonReady,
                Milestone::TasksHydrated,
                Milestone::Interactive,
            ]
        );
        assert_eq!(run.latency_ms(), None);
    }

    #[test]
    fn unrelated_lines_do_not_parse() {
        assert_eq!(ParsedTrace::parse("daemon ready"), None);
        assert_eq!(ParsedTrace::parse(""), None);
        assert_eq!(
            ParsedTrace::parse("startup-trace pid=1 run=0 kind=cold"),
            None,
            "a line without a start or milestones is not a run"
        );
    }

    #[test]
    fn a_budget_under_run_passes_and_an_over_run_fails() {
        let budgets = Budgets {
            cold_launch_ms: 300.0,
            reopen_ms: 100.0,
        };

        let cold = cold_run();
        assert_eq!(
            budgets.check(&cold),
            BudgetCheck::Within {
                kind: RunKind::Cold,
                measured_ms: 260.0,
                budget_ms: 300.0,
            }
        );

        let mut slow_reopen = TraceRun::new(1, RunKind::Reopen, 10_000.0);
        slow_reopen.record(Milestone::Interactive, 10_150.0);
        let check = budgets.check(&slow_reopen);
        assert_eq!(
            check,
            BudgetCheck::Overrun {
                kind: RunKind::Reopen,
                measured_ms: 150.0,
                budget_ms: 100.0,
            }
        );
        assert!(check.is_failure());
    }

    #[test]
    fn an_incomplete_run_fails_rather_than_passing_silently() {
        let budgets = Budgets {
            cold_launch_ms: 300.0,
            reopen_ms: 100.0,
        };
        let mut run = TraceRun::new(0, RunKind::Cold, 0.0);
        run.record(Milestone::WindowOpen, 12.0);

        let check = budgets.check(&run);
        assert!(check.is_failure());
        assert!(matches!(check, BudgetCheck::Incomplete { .. }));
    }
}
