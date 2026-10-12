//! The debug RPA endpoint: discovery publication and the local listener.
//!
//! The listener owns the socket and the protocol framing on a background
//! thread; every call is forwarded to the UI side through an async channel
//! and answered from there, so no socket work ever reaches a frame. Started
//! once per process by [`start`], only in debug builds.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::protocol::{self, Call, Reply};

/// The name of the discovery publication under the instance data directory.
pub const CONFIG_FILE: &str = "rpa.json";

/// What a client needs to reach and trust this instance. Serialized into the
/// discovery file and returned by `describe`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub protocol_version: u32,
    pub address: String,
    pub token: String,
    pub pid: u32,
    pub checkout: String,
    pub database: String,
    pub app: String,
    pub build: String,
    pub started_at_ms: u64,
}

/// One call handed to the UI side, with the channel its answer goes back on.
pub struct Inbound {
    pub call: Call,
    pub reply: std::sync::mpsc::Sender<Reply>,
}

struct Service {
    discovery: Discovery,
    receiver: Mutex<Option<smol::channel::Receiver<Inbound>>>,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

/// The one channel between the socket threads and the UI pump. Created on
/// first use so [`start`] can hand the sender to the listener thread and the
/// receiver to the pump without a second channel.
static CHANNEL: OnceLock<(
    smol::channel::Sender<Inbound>,
    smol::channel::Receiver<Inbound>,
)> = OnceLock::new();

fn channel() -> &'static (
    smol::channel::Sender<Inbound>,
    smol::channel::Receiver<Inbound>,
) {
    CHANNEL.get_or_init(smol::channel::unbounded)
}

/// Start the endpoint and publish discovery, or answer `None` when this build
/// or environment does not expose the bridge.
///
/// Idempotent: a second call returns the first publication.
pub fn start() -> Option<&'static Discovery> {
    if !enabled() {
        return None;
    }
    if let Some(service) = SERVICE.get() {
        return Some(&service.discovery);
    }
    match launch() {
        Ok(discovery) => {
            let service = Service {
                discovery,
                receiver: Mutex::new(Some(channel().1.clone())),
            };
            let _ = SERVICE.set(service);
            SERVICE.get().map(|service| &service.discovery)
        }
        Err(error) => {
            // The bridge is development tooling; a port or filesystem
            // problem must not take the app down with it.
            eprintln!("waku-rpa: could not start the endpoint: {error:#}");
            None
        }
    }
}

/// Whether this process should expose the bridge at all. Debug builds only;
/// `WAKU_RPA=0` (or `false`/`off`) opts a debug run out.
pub fn enabled() -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }
    !matches!(
        std::env::var("WAKU_RPA").ok().as_deref(),
        Some("0" | "false" | "off")
    )
}

/// The live publication, once [`start`] has run.
pub fn discovery() -> Option<&'static Discovery> {
    SERVICE.get().map(|service| &service.discovery)
}

/// Take the call receiver. The UI pump calls this exactly once.
pub fn take_inbound() -> Option<smol::channel::Receiver<Inbound>> {
    SERVICE
        .get()
        .and_then(|service| service.receiver.lock().take())
}

/// The discovery path readers use: an explicit override first, then the
/// instance data directory publication.
#[allow(dead_code)] // the REPL bindings reach for it next; the CLI reads the file directly
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WAKU_RPA_CONFIG").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    data_directory().map(|directory| directory.join(CONFIG_FILE))
}

#[allow(dead_code)] // used by the REPL bindings and tests landing with the endpoint follow-ups
pub fn read_discovery_at(path: &Path) -> anyhow::Result<Discovery> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("no RPA publication at {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("invalid RPA publication at {}", path.display()))
}

/// The instance data directory: the checkout's `temp/` in debug builds.
fn data_directory() -> Option<PathBuf> {
    crate::persistence::StateStore::default_path()
        .parent()
        .map(Path::to_path_buf)
}

fn launch() -> anyhow::Result<Discovery> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("could not bind the RPA endpoint")?;
    let address: SocketAddr = listener.local_addr()?;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );

    let data_directory = data_directory().ok_or_else(|| anyhow!("no instance data directory"))?;
    let (discovery, config_path) = publication(&data_directory, address, token)?;

    let sender = channel().0.clone();
    let token = discovery.token.clone();
    std::thread::Builder::new()
        .name("waku-rpa".to_owned())
        .spawn(move || accept_loop(listener, sender, token))
        .context("could not start the RPA listener thread")?;

    write_discovery(&config_path, &discovery).context("could not publish the RPA endpoint")?;
    Ok(discovery)
}

fn publication(
    data_directory: &Path,
    address: SocketAddr,
    token: String,
) -> anyhow::Result<(Discovery, PathBuf)> {
    let database = crate::persistence::StateStore::default_path();
    let checkout = data_directory
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let started_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    Ok((
        Discovery {
            protocol_version: protocol::PROTOCOL_VERSION,
            address: address.to_string(),
            token,
            pid: std::process::id(),
            checkout,
            database: database.display().to_string(),
            app: crate::identity::app_name().to_owned(),
            build: env!("CARGO_PKG_VERSION").to_owned(),
            started_at_ms,
        },
        data_directory.join(CONFIG_FILE),
    ))
}

fn write_discovery(path: &Path, discovery: &Discovery) -> anyhow::Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(discovery)?)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

fn accept_loop(listener: TcpListener, sender: smol::channel::Sender<Inbound>, token: String) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let sender = sender.clone();
        let token = token.clone();
        std::thread::Builder::new()
            .name("waku-rpa-conn".to_owned())
            .spawn(move || serve_connection(stream, sender, &token))
            .ok();
    }
}

/// How many clients are connected right now. The UI keeps its accessibility
/// tree built only while this is non-zero, so a debug session that is not
/// being driven pays no accessibility cost.
static ATTACHED: AtomicUsize = AtomicUsize::new(0);

pub fn attached() -> usize {
    ATTACHED.load(Ordering::SeqCst)
}

/// When the last call arrived, so automation can linger long enough for a
/// scenario's calls to share one attached window instead of rebuilding the
/// tree for every one-second gap between commands.
static LAST_CALL_MS: AtomicU64 = AtomicU64::new(0);

pub fn mark_call() {
    LAST_CALL_MS.store(now_ms(), Ordering::SeqCst);
}

pub fn idle_ms() -> u64 {
    now_ms().saturating_sub(LAST_CALL_MS.load(Ordering::SeqCst))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

struct AttachGuard;

impl AttachGuard {
    fn new() -> Self {
        ATTACHED.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        ATTACHED.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve_connection(stream: TcpStream, sender: smol::channel::Sender<Inbound>, token: &str) {
    let _attached = AttachGuard::new();
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut writer = stream;
    let mut lines = BufReader::new(reader_stream).lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Call>(&line) {
            Ok(call) if call.version != protocol::PROTOCOL_VERSION => Reply::error(
                call.id,
                super::protocol::Error::new(
                    "version_mismatch",
                    format!(
                        "client protocol {} does not match this app's {}",
                        call.version,
                        protocol::PROTOCOL_VERSION
                    ),
                ),
            ),
            Ok(call) if call.token != token => Reply::error(
                call.id,
                super::protocol::Error::new(
                    "unauthorized",
                    "the call did not present this instance's token",
                ),
            ),
            Ok(call) => {
                let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
                if sender
                    .send_blocking(Inbound {
                        call,
                        reply: reply_sender,
                    })
                    .is_err()
                {
                    break;
                }
                match reply_receiver.recv() {
                    Ok(reply) => reply,
                    Err(_) => break,
                }
            }
            Err(error) => Reply::error(
                0,
                super::protocol::Error::new(
                    "invalid_call",
                    format!("the call is not a valid RPA request: {error}"),
                ),
            ),
        };
        let Ok(encoded) = serde_json::to_string(&reply) else {
            break;
        };
        if writeln!(writer, "{encoded}").is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_round_trips_through_its_file() {
        let directory = std::env::temp_dir().join(format!("waku-rpa-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CONFIG_FILE);
        let (discovery, _) = publication(
            &directory,
            "127.0.0.1:41234".parse().unwrap(),
            "token".to_owned(),
        )
        .unwrap();
        write_discovery(&path, &discovery).unwrap();
        let read = read_discovery_at(&path).unwrap();
        assert_eq!(read.address, "127.0.0.1:41234");
        assert_eq!(read.token, "token");
        assert_eq!(read.pid, std::process::id());
        std::fs::remove_dir_all(&directory).ok();
    }
}
