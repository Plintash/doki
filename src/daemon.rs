//! Desktop ownership of the Waku daemon process.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
use gpui::{App, Global};

/// The application's daemon connection.
///
/// The window opens before the daemon answers, so the connection is asked for
/// once, at application scope, and its answer is kept here. Windows borrow the
/// daemon and never own it, so closing the only window leaves the daemon
/// running: a rebuilt window reattaches to the same process instead of
/// starting and reaping a second one. The request itself is kept here too, so
/// the window that made it closing mid-flight cannot drop the supervisor.
#[derive(Clone)]
pub enum DaemonState {
    /// No window has asked for a daemon yet.
    Idle,
    /// A connection request is in flight; windows paint skeleton content.
    Connecting,
    /// The daemon answered.
    Ready(waku_client::DaemonSupervisor),
    /// The last request could not reach a daemon.
    Failed(String),
}

impl Global for DaemonState {}

/// How a window reaches its daemon. Production starts the supervised daemon,
/// or connects to one managed elsewhere; tests substitute a prepared answer so
/// the startup path can run without a daemon. Runs off the UI thread.
pub(crate) type DaemonConnector =
    Arc<dyn Fn() -> anyhow::Result<waku_client::DaemonSupervisor> + Send + Sync>;

/// The application's current daemon connection.
///
/// An application that has not asked yet reads as [`DaemonState::Idle`], so
/// this is answerable before the first window builds.
pub fn state(cx: &App) -> DaemonState {
    cx.try_global::<DaemonState>()
        .cloned()
        .unwrap_or(DaemonState::Idle)
}

/// The connected daemon, once it has answered.
pub fn connected(cx: &App) -> Option<waku_client::DaemonSupervisor> {
    match cx.try_global::<DaemonState>() {
        Some(DaemonState::Ready(daemon)) => Some(daemon.clone()),
        _ => None,
    }
}

/// Ask for the application's daemon, unless one is already connected or being
/// connected.
///
/// The request belongs to the application rather than to the window that made
/// it: its answer is published in [`DaemonState`] and handed to whichever main
/// windows exist when it lands. A window that closes while the daemon is
/// answering therefore cannot drop the supervisor — which would reap the
/// daemon it just started — and a window opened while the request is still in
/// flight joins it instead of starting a second daemon. A failed attempt may
/// be retried, which is what reopening after a failure does.
pub fn request(cx: &mut App, connector: DaemonConnector) {
    match cx.try_global::<DaemonState>() {
        Some(DaemonState::Ready(_) | DaemonState::Connecting) => return,
        _ => {}
    }
    cx.set_global(DaemonState::Connecting);
    cx.spawn(async move |cx| {
        let answer = cx
            .background_executor()
            .spawn(async move { connector() })
            .await;
        cx.update(|cx| {
            cx.set_global(match answer {
                Ok(daemon) => DaemonState::Ready(daemon),
                Err(error) => DaemonState::Failed(error.to_string()),
            });
            crate::main_window::attach_main_windows(cx);
        });
    })
    .detach();
}

/// Connect to a daemon managed elsewhere, or start and supervise the local one.
///
/// Blocking: callers run this off the UI thread, and the main window paints
/// skeleton content until it answers.
pub fn connect() -> anyhow::Result<waku_client::DaemonSupervisor> {
    let address = std::env::var(waku_client::DAEMON_ADDRESS_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let token = std::env::var(waku_client::DAEMON_TOKEN_ENV)
        .ok()
        .filter(|value| !value.is_empty());
    match (address, token) {
        (Some(address), Some(token)) => {
            return waku_client::DaemonSupervisor::connect(address.trim(), token);
        }
        (Some(_), None) => bail!(
            "{} is set but {} is missing",
            waku_client::DAEMON_ADDRESS_ENV,
            waku_client::DAEMON_TOKEN_ENV
        ),
        (None, Some(_)) => bail!(
            "{} is set but {} is missing",
            waku_client::DAEMON_TOKEN_ENV,
            waku_client::DAEMON_ADDRESS_ENV
        ),
        (None, None) => {}
    }
    let app_settings = waku_client::persistence::load_or_create_app_settings()
        .context("could not load desktop daemon settings")?;
    waku_client::DaemonSupervisor::spawn_configured(
        &daemon_executable_path()?,
        cfg!(debug_assertions),
        app_settings.daemon_exposure,
    )
}

/// Resolve the local host name once during app construction. Settings can
/// then show a useful LAN URL without touching the OS from a render frame.
pub fn local_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buffer = [0_u8; 256];
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let length = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            let hostname = String::from_utf8_lossy(&buffer[..length]).trim().to_owned();
            if !hostname.is_empty() {
                return Some(hostname);
            }
        }
    }
    // `COMPUTERNAME` is the Windows equivalent and is always set; `HOSTNAME`
    // covers the shells that export it.
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|hostname| hostname.trim().to_owned())
        .find(|hostname| !hostname.is_empty())
}

fn daemon_executable_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("WAKU_DAEMON_PATH").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    let executable = format!("waku-daemon{}", std::env::consts::EXE_SUFFIX);
    let current = std::env::current_exe().context("could not locate the Waku executable")?;

    // Development keeps the daemon beside Cargo's debug artifacts rather than
    // inside Waku Debug.app. The supervisor watches this file and swaps only
    // the daemon when the development watcher relinks it.
    #[cfg(debug_assertions)]
    if let Some(debug_directory) = current
        .ancestors()
        .find(|candidate| candidate.file_name().is_some_and(|name| name == "debug"))
    {
        let external = debug_directory.join(&executable);
        if external.is_file() {
            return Ok(external);
        }
    }

    let sibling = current
        .parent()
        .map(|directory| directory.join(&executable))
        .ok_or_else(|| anyhow!("Waku executable has no parent directory"))?;
    if sibling.is_file() {
        return Ok(sibling);
    }
    #[cfg(debug_assertions)]
    bail!(
        "Waku daemon was not found in Cargo's debug directory or next to the app executable: {}",
        sibling.display(),
    );
    #[cfg(not(debug_assertions))]
    bail!(
        "Waku daemon is missing next to the app executable: {}",
        sibling.display(),
    )
}
