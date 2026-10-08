//! Daemon-owned pseudoterminals for remote clients.
//!
//! The daemon owns the shell, cwd, and PTY. Clients only render the byte
//! stream and send input/resize controls, so a browser can operate against a
//! daemon on another machine without interpreting any daemon-side paths.

#[cfg(not(unix))]
use std::path::Path;

#[cfg(not(unix))]
use anyhow::bail;

#[cfg(not(unix))]
use crate::EventSink;

#[cfg(unix)]
mod platform {
    use std::collections::VecDeque;
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use alacritty_terminal::event::{OnResize as _, WindowSize};
    use alacritty_terminal::tty::{self, EventedPty as _, EventedReadWrite as _, Shell};
    use anyhow::{Context as _, bail};
    use base64::Engine as _;
    use parking_lot::Mutex;
    use serde_json::json;

    use crate::{EventSink, WireDriverEvent};

    const CELL_WIDTH: u16 = 8;
    const CELL_HEIGHT: u16 = 16;
    const MIN_COLUMNS: u16 = 2;
    const MIN_ROWS: u16 = 1;
    const SHELL_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);
    /// How long the reader waits before retrying an empty PTY. Also the
    /// granularity of the flush deadline below.
    const READ_RETRY_INTERVAL: Duration = Duration::from_millis(4);
    const READ_CHUNK_BYTES: usize = 32 * 1024;
    /// Output coalescing bounds: a flush happens once this much output has
    /// accumulated, or this long after the previous flush, whichever comes
    /// first. A shell writing byte by byte therefore cannot drive one event
    /// per write, and a flooding producer cannot exceed one event per
    /// threshold either. The interval matches the frame cadence, so an
    /// interactive echo waits at most one frame while a flood still drains.
    pub(crate) const FLUSH_BYTES: usize = 32 * 1024;
    pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

    /// A terminal is retained for the life of its shell, not only while a
    /// client is attached, so its output buffer must be capped: a producer
    /// that floods the terminal would otherwise grow daemon memory at the
    /// stream rate forever. One MiB holds the visible grid and ample
    /// scrollback for any terminal size while staying cheap per terminal.
    pub const RETAINED_OUTPUT_BYTES: usize = 1024 * 1024;

    /// Bounded retention of a terminal's most recent output bytes, oldest
    /// dropped first. Attached clients never depend on it for delivery; it
    /// exists so output produced while nobody was attached can be replayed.
    pub(crate) struct RetainedOutput {
        capacity: usize,
        bytes: VecDeque<u8>,
    }

    impl RetainedOutput {
        pub(crate) fn new(capacity: usize) -> Self {
            Self {
                capacity: capacity.max(1),
                bytes: VecDeque::new(),
            }
        }

        pub(crate) fn push(&mut self, bytes: &[u8]) {
            if bytes.len() >= self.capacity {
                // The push alone overflows the buffer, so nothing older can
                // survive it.
                self.bytes.clear();
                self.bytes
                    .extend(bytes[bytes.len() - self.capacity..].iter().copied());
                return;
            }
            let overflow = self
                .bytes
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(self.capacity);
            if overflow > 0 {
                self.bytes.drain(..overflow);
            }
            self.bytes.extend(bytes.iter().copied());
        }

        pub(crate) fn snapshot(&self) -> Vec<u8> {
            self.bytes.iter().copied().collect()
        }
    }

    pub struct DaemonTerminal {
        pty: Arc<Mutex<tty::Pty>>,
        stopped: Arc<AtomicBool>,
        retained: Arc<Mutex<RetainedOutput>>,
        reader: Option<JoinHandle<()>>,
    }

    impl DaemonTerminal {
        pub fn open(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
        ) -> anyhow::Result<Self> {
            let shell = crate::command_env::default_terminal_shell();
            let shell_args = crate::command_env::default_terminal_shell_args(&shell);
            Self::open_with_shell(
                cwd,
                cols,
                rows,
                events,
                Shell::new(shell.to_string_lossy().into_owned(), shell_args),
            )
        }

        pub(crate) fn open_with_shell(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
            shell: Shell,
        ) -> anyhow::Result<Self> {
            Self::open_retaining(cwd, cols, rows, events, shell, RETAINED_OUTPUT_BYTES)
        }

        /// Production terminals all share [`RETAINED_OUTPUT_BYTES`]; tests use a
        /// small window so a flood that overruns it stays cheap to produce.
        #[cfg(all(test, unix))]
        pub(crate) fn open_with_shell_and_retention(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
            shell: Shell,
            retained_bytes: usize,
        ) -> anyhow::Result<Self> {
            Self::open_retaining(cwd, cols, rows, events, shell, retained_bytes)
        }

        fn open_retaining(
            cwd: &std::path::Path,
            cols: u16,
            rows: u16,
            events: EventSink,
            shell: Shell,
            retained_bytes: usize,
        ) -> anyhow::Result<Self> {
            if !cwd.is_dir() {
                bail!(
                    "terminal working directory does not exist: {}",
                    cwd.display()
                );
            }

            let mut options = tty::Options {
                shell: Some(shell),
                working_directory: Some(cwd.to_owned()),
                drain_on_exit: false,
                ..Default::default()
            };
            for (name, value) in crate::command_env::shell_environment() {
                options.env.insert(
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                );
            }
            options.env.insert("TERM".into(), "xterm-256color".into());
            options.env.insert("COLORTERM".into(), "truecolor".into());

            let size = window_size(cols, rows);
            let pty = tty::new(&options, size, 0)
                .with_context(|| format!("spawn terminal in {}", cwd.display()))?;
            let mut output = pty.file().try_clone().context("clone terminal output")?;
            let pty = Arc::new(Mutex::new(pty));
            let stopped = Arc::new(AtomicBool::new(false));
            let retained = Arc::new(Mutex::new(RetainedOutput::new(retained_bytes)));
            let reader_pty = pty.clone();
            let reader_stopped = stopped.clone();
            let reader_retained = retained.clone();
            let reader = std::thread::Builder::new()
                .name("waku-daemon-terminal-output".into())
                .spawn(move || {
                    let mut output_events = TerminalOutput::new(events, reader_retained);
                    let mut buffer = [0_u8; READ_CHUNK_BYTES];
                    while !reader_stopped.load(Ordering::Acquire) {
                        match output.read(&mut buffer) {
                            Ok(0) => {
                                output_events.exited();
                                break;
                            }
                            Ok(read) => output_events.push(&buffer[..read]),
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                std::thread::sleep(READ_RETRY_INTERVAL);
                                output_events.flush_if_due();
                            }
                            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                                // A PTY master may report EIO briefly before the
                                // freshly spawned child has attached its slave.
                                // Only treat it as EOF after Alacritty's SIGCHLD
                                // channel confirms the child actually exited.
                                if reader_pty.lock().next_child_event().is_some() {
                                    output_events.exited();
                                    break;
                                }
                                std::thread::sleep(READ_RETRY_INTERVAL);
                                output_events.flush_if_due();
                            }
                            Err(error) => {
                                output_events.failed(error);
                                break;
                            }
                        }
                    }
                })
                .context("start terminal output thread")?;

            Ok(Self {
                pty,
                stopped,
                retained,
                reader: Some(reader),
            })
        }

        /// The most recent output bytes this terminal produced, oldest dropped
        /// first, capped at [`RETAINED_OUTPUT_BYTES`].
        pub fn retained_output(&self) -> Vec<u8> {
            self.retained.lock().snapshot()
        }

        pub fn write(&self, data: Vec<u8>) -> anyhow::Result<()> {
            if data.is_empty() {
                return Ok(());
            }
            let mut pty = self.pty.lock();
            pty.writer()
                .write_all(&data)
                .context("write terminal input")?;
            pty.writer().flush().context("flush terminal input")
        }

        pub fn resize(&self, cols: u16, rows: u16) {
            self.pty.lock().on_resize(window_size(cols, rows));
        }
    }

    impl Drop for DaemonTerminal {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            // Alacritty's PTY destructor sends SIGHUP and then waits for the
            // child without a timeout. A shell can ignore or defer SIGHUP,
            // so bound its grace period before either join or PTY drop.
            // Hold the lock so the reader cannot reap the child between our
            // exit check and signal delivery.
            terminate_shell(&self.pty.lock());
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn terminate_shell(pty: &tty::Pty) {
        let child_pid = pty.child().id() as libc::pid_t;
        if child_has_exited(child_pid) {
            return;
        }
        unsafe {
            libc::kill(child_pid, libc::SIGHUP);
        }
        let deadline = Instant::now() + SHELL_SHUTDOWN_GRACE;
        while !child_has_exited(child_pid) {
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(child_pid, libc::SIGKILL);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn child_has_exited(child_pid: libc::pid_t) -> bool {
        // WNOWAIT leaves reaping to Alacritty's Child, keeping the PID owned
        // until its destructor has finished sending signals and waiting.
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child_pid as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            unsafe { status.si_pid() != 0 }
        } else {
            // The output reader may already have reaped a naturally exited
            // shell via next_child_event before shutdown began.
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
        }
    }

    /// Retains and delivers one terminal's output. Reads accumulate into a
    /// bounded batch, and each batch is appended to the retained window and
    /// emitted as a single `terminalOutput` event, so a shell writing byte by
    /// byte cannot drive one event per write.
    struct TerminalOutput {
        events: EventSink,
        retained: Arc<Mutex<RetainedOutput>>,
        pending: Vec<u8>,
        last_flush: Option<Instant>,
    }

    impl TerminalOutput {
        fn new(events: EventSink, retained: Arc<Mutex<RetainedOutput>>) -> Self {
            Self {
                events,
                retained,
                pending: Vec::with_capacity(READ_CHUNK_BYTES),
                last_flush: None,
            }
        }

        fn push(&mut self, bytes: &[u8]) {
            self.pending.extend_from_slice(bytes);
            if self.pending.len() >= FLUSH_BYTES || self.flush_is_due() {
                self.flush();
            }
        }

        /// Delivers the batch once `FLUSH_INTERVAL` has passed since the last
        /// one, so an interactive echo waits at most one frame.
        fn flush_if_due(&mut self) {
            if self.flush_is_due() {
                self.flush();
            }
        }

        fn flush(&mut self) {
            if self.pending.is_empty() {
                return;
            }
            self.retained.lock().push(&self.pending);
            let data = base64::engine::general_purpose::STANDARD.encode(&self.pending);
            self.pending.clear();
            self.last_flush = Some(Instant::now());
            let _ = self.events.send_ephemeral(WireDriverEvent::new(
                "terminalOutput",
                json!({ "data": data }),
            ));
        }

        fn flush_is_due(&self) -> bool {
            self.last_flush
                .is_none_or(|last| last.elapsed() >= FLUSH_INTERVAL)
        }

        fn exited(&mut self) {
            self.flush();
            let _ = self.events.send_ephemeral(WireDriverEvent::new(
                "terminalExited",
                serde_json::Value::Null,
            ));
        }

        fn failed(&mut self, error: std::io::Error) {
            self.flush();
            let _ = self.events.send_ephemeral(WireDriverEvent::new(
                "terminalError",
                serde_json::Value::String(error.to_string()),
            ));
        }
    }

    fn window_size(cols: u16, rows: u16) -> WindowSize {
        WindowSize {
            num_lines: rows.max(MIN_ROWS),
            num_cols: cols.max(MIN_COLUMNS),
            cell_width: CELL_WIDTH,
            cell_height: CELL_HEIGHT,
        }
    }
}

#[cfg(unix)]
pub use platform::DaemonTerminal;

/// Delivery cadence of terminal output, exposed so tests can assert the bound
/// it puts on the number of delivered events.
#[cfg(all(test, unix))]
pub(crate) use platform::{FLUSH_BYTES, FLUSH_INTERVAL};

#[cfg(not(unix))]
pub struct DaemonTerminal;

#[cfg(not(unix))]
impl DaemonTerminal {
    pub fn open(_cwd: &Path, _cols: u16, _rows: u16, _events: EventSink) -> anyhow::Result<Self> {
        bail!("daemon terminals are not supported on this platform")
    }

    pub fn write(&self, _data: Vec<u8>) -> anyhow::Result<()> {
        bail!("daemon terminals are not supported on this platform")
    }

    pub fn resize(&self, _cols: u16, _rows: u16) {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::platform::RetainedOutput;

    #[test]
    fn retained_output_keeps_all_bytes_below_capacity() {
        let mut retained = RetainedOutput::new(8);
        retained.push(b"abcd");
        retained.push(b"ef");

        assert_eq!(retained.snapshot(), b"abcdef");
    }

    #[test]
    fn retained_output_drops_only_the_oldest_bytes_beyond_capacity() {
        let mut retained = RetainedOutput::new(8);
        retained.push(b"abcdef");
        retained.push(b"ghij");
        // One push may be larger than the whole buffer; only its tail survives.
        retained.push(b"klmnopqrs");

        assert_eq!(retained.snapshot(), b"lmnopqrs");
    }
}
