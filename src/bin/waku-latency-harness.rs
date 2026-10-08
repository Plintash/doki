//! Repeatable cold-launch and reopen latency harness.
//!
//! Launches the debug app in the background, reads the milestone lines it
//! writes with `WAKU_STARTUP_TRACE` (see `waku::latency`), and fails when
//! either the cold launch or the rebuild exceeds the recorded budgets.
//!
//! The app is opened with `open -g`, so it never takes the user's focus, and
//! the harness closes it — and the daemon it started — when it is done. A
//! traced cold launch
//! (`WAKU_STARTUP_TRACE_CLOSE_AFTER_LAUNCH`) closes its own window once the
//! launch has been recorded and rebuilds it through the same opener Dock
//! activation uses, which is what produces the reopen run. Driving AppKit's
//! own reopen from outside the app needs accessibility control, which a
//! repeatable harness cannot rely on.
//!
//! Build the app first:
//!
//! ```text
//! cargo build --package waku-daemon --bin waku-daemon
//! scripts/bundle.sh debug
//! cargo run --bin waku-latency-harness
//! ```
//!
//! `--cold-budget-ms` and `--reopen-budget-ms` override the recorded budgets
//! so the gate itself can be checked without rebuilding: passing `1` makes the
//! harness fail, which is how a deliberately slowed build is expected to
//! behave.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use waku::latency::{BUDGETS, Budgets, Milestone, ParsedTrace, RunKind};
use waku::{CLOSE_AFTER_LAUNCH_ENV, TRACE_ENV};

const DEFAULT_APP: &str = "target/debug/Doki Debug.app";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(10);

struct Args {
    app: PathBuf,
    trace: PathBuf,
    budgets: Budgets,
    timeout: Duration,
}

impl Args {
    fn parse(arguments: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut args = Self {
            app: PathBuf::from(DEFAULT_APP),
            trace: std::env::temp_dir().join(format!("waku-latency-{}.log", std::process::id())),
            budgets: BUDGETS,
            timeout: DEFAULT_TIMEOUT,
        };
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .with_context(|| format!("{argument} needs a value"))
            };
            match argument.as_str() {
                "--app" => args.app = PathBuf::from(value()?),
                "--trace" => args.trace = PathBuf::from(value()?),
                "--cold-budget-ms" => args.budgets.cold_launch_ms = parse_ms(&value()?)?,
                "--reopen-budget-ms" => args.budgets.reopen_ms = parse_ms(&value()?)?,
                "--timeout-secs" => {
                    args.timeout = Duration::from_secs(
                        value()?.parse().context("--timeout-secs is not a number")?,
                    )
                }
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => bail!("unknown argument {other}"),
            }
        }
        Ok(args)
    }
}

fn parse_ms(value: &str) -> anyhow::Result<f64> {
    value
        .parse()
        .with_context(|| format!("{value} is not a millisecond count"))
}

fn print_usage() {
    println!(
        "usage: waku-latency-harness [--app PATH] [--trace PATH] [--cold-budget-ms MS] \
         [--reopen-budget-ms MS] [--timeout-secs S]"
    );
}

fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("waku-latency-harness: {error}");
            print_usage();
            std::process::exit(2);
        }
    };
    match run(&args) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("waku-latency-harness: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run(args: &Args) -> anyhow::Result<()> {
    let app = args.app.clone();
    if !app.exists() {
        bail!(
            "no debug app at {}; build it with `scripts/bundle.sh debug`",
            app.display()
        );
    }
    // The app finds its daemon inside a release bundle or beside itself in a
    // debug build directory; without one the launch never reaches the
    // workspace and the harness would only report a timeout.
    let bundled_daemon = app.join("Contents/MacOS/waku-daemon");
    let sibling_daemon = app.parent().map(|directory| directory.join("waku-daemon"));
    if !bundled_daemon.exists() && !sibling_daemon.is_some_and(|daemon| daemon.exists()) {
        bail!(
            "no daemon beside {}; build it with `cargo build --package waku-daemon --bin \
             waku-daemon`",
            app.display()
        );
    }
    let _ = fs::remove_file(&args.trace);
    println!("waku-latency-harness: app {}", app.display());
    println!("waku-latency-harness: trace {}", args.trace.display());

    let launched = launch(&app, &args.trace)?;
    // Whatever happens next, the app this harness started does not outlive it.
    let mut guard = AppGuard::new(launched);

    let cold = wait_for_run(&args.trace, RunKind::Cold, args.timeout)?;
    guard.confirm(cold.pid);
    println!(
        "waku-latency-harness: cold launch traced (pid {})",
        cold.pid
    );

    // The traced cold launch closes its window and rebuilds it through the
    // app's own opener, which is the reopen this measures.
    let reopen = wait_for_run(&args.trace, RunKind::Reopen, args.timeout)?;
    guard.confirm(reopen.pid);

    guard.terminate_and_disarm();

    report(&cold);
    report(&reopen);
    let cold_check = args.budgets.check(&cold.run);
    let reopen_check = args.budgets.check(&reopen.run);
    println!("waku-latency-harness: {}", cold_check.summary());
    println!("waku-latency-harness: {}", reopen_check.summary());
    if cold_check.is_failure() || reopen_check.is_failure() {
        bail!("latency budgets not met");
    }
    println!("waku-latency-harness: OK");
    Ok(())
}

/// Open a fresh instance in the background with tracing on, and return its pid.
///
/// `open` does not report the pid, so the instance is found as the app process
/// that appeared across the launch; more than one would be ambiguous and is
/// refused rather than guessed at.
fn launch(app: &Path, trace: &Path) -> anyhow::Result<u32> {
    let before = app_pids(app);
    let status = Command::new("open")
        .arg("-g")
        .arg("-n")
        .arg("--env")
        .arg(format!("{TRACE_ENV}={}", trace.display()))
        .arg("--env")
        .arg(format!("{CLOSE_AFTER_LAUNCH_ENV}=1"))
        .arg(app)
        .status()
        .context("could not run `open`")?;
    if !status.success() {
        bail!("`open` refused to launch {}", app.display());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut appeared = app_pids(app)
            .into_iter()
            .filter(|pid| !before.contains(pid));
        match (appeared.next(), appeared.next()) {
            (Some(pid), None) => return Ok(pid),
            (Some(first), Some(second)) => bail!(
                "two app processes appeared ({first}, {second}); refusing to guess which one to \
                 terminate"
            ),
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!("the app did not appear within 15s of `open`");
        }
        sleep(POLL_INTERVAL);
    }
}

/// Poll the trace file for a completed run of `kind`.
fn wait_for_run(trace: &Path, kind: RunKind, timeout: Duration) -> anyhow::Result<ParsedTrace> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(trace)
            && let Some(found) = contents
                .lines()
                .filter_map(ParsedTrace::parse)
                .find(|parsed| {
                    parsed.run.kind == kind && parsed.run.reached(Milestone::Interactive).is_some()
                })
        {
            return Ok(found);
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {}s waiting for the {} run in {}",
                timeout.as_secs(),
                kind.label(),
                trace.display()
            );
        }
        sleep(POLL_INTERVAL);
    }
}

fn report(parsed: &ParsedTrace) {
    let run = &parsed.run;
    let milestones = Milestone::ALL
        .into_iter()
        .map(|milestone| {
            let at = run
                .reached(milestone)
                .map(|at| format!("{at:.0}"))
                .unwrap_or_else(|| "-".into());
            format!("{}={at}", milestone.key())
        })
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "waku-latency-harness: {} pid={} run={} start={:.0}ms {milestones} latency={}ms",
        run.kind.label(),
        parsed.pid,
        run.index,
        run.start_ms,
        run.latency_ms()
            .map(|ms| format!("{ms:.0}"))
            .unwrap_or_default(),
    );
}

/// The running app processes for this bundle, whatever checkout built them.
///
/// The bundle path is matched by name rather than by path: `open` resolves a
/// checkout's `target` symlink, so the launched process reports the shared
/// build-cache path, not the path it was opened with.
fn app_pids(app: &Path) -> Vec<u32> {
    let bundle = app
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let executable = bundle.strip_suffix(".app").unwrap_or(bundle);
    pids_matching(&format!("{bundle}/Contents/MacOS/{executable}"))
}

fn pids_matching(pattern: &str) -> Vec<u32> {
    let Ok(output) = Command::new("pgrep").arg("-f").arg(pattern).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn child_pids(parent: u32) -> Vec<u32> {
    let Ok(output) = Command::new("pgrep")
        .arg("-P")
        .arg(parent.to_string())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status();
}

/// Owns the app this harness launched: every exit path terminates it, and the
/// daemon it started with it.
struct AppGuard {
    pid: u32,
    armed: bool,
}

impl AppGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    /// Cross-check the pid the trace reported against the one `open` produced.
    fn confirm(&mut self, traced_pid: u32) {
        if self.pid != traced_pid {
            eprintln!(
                "waku-latency-harness: trace came from pid {traced_pid}, but pid {} was launched",
                self.pid
            );
        }
    }

    fn terminate_and_disarm(&mut self) {
        self.terminate();
        self.armed = false;
    }

    fn terminate(&self) {
        let children = child_pids(self.pid);
        signal(self.pid, "-TERM");
        if !wait_for_exit(self.pid, TERMINATE_TIMEOUT) {
            signal(self.pid, "-KILL");
            wait_for_exit(self.pid, TERMINATE_TIMEOUT);
        }
        // The daemon notices its parent is gone, but do not leave one behind
        // if it is slow to.
        for child in children {
            if !wait_for_exit(child, TERMINATE_TIMEOUT) {
                signal(child, "-KILL");
            }
        }
    }
}

impl Drop for AppGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if self.pid != 0 && alive(self.pid) {
            self.terminate();
        }
    }
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        sleep(POLL_INTERVAL);
    }
    !alive(pid)
}
