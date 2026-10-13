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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow};
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

static SERVICE: OnceLock<Discovery> = OnceLock::new();
static CHANNEL: OnceLock<(
    smol::channel::Sender<Inbound>,
    smol::channel::Receiver<Inbound>,
)> = OnceLock::new();

/// How many clients are connected right now. The UI keeps its accessibility
/// tree built only while this is non-zero, so a debug session that is not
/// being driven pays no accessibility cost.
static ATTACHED: AtomicUsize = AtomicUsize::new(0);

fn channel() -> &'static (
    smol::channel::Sender<Inbound>,
    smol::channel::Receiver<Inbound>,
) {
    CHANNEL.get_or_init(smol::channel::unbounded)
}

/// Start the endpoint and publish discovery, or answer `None` when this build
/// does not expose the bridge. Called once from `run`.
pub fn start() -> Option<&'static Discovery> {
    if !cfg!(debug_assertions) {
        return None;
    }
    if SERVICE.get().is_none() {
        match launch() {
            Ok(discovery) => {
                let _ = SERVICE.set(discovery);
            }
            // The bridge is development tooling; a port or filesystem problem
            // must not take the app down with it.
            Err(error) => eprintln!("waku-rpa: could not start the endpoint: {error:#}"),
        }
    }
    SERVICE.get()
}

/// The live publication, once [`start`] has run.
pub fn discovery() -> Option<&'static Discovery> {
    SERVICE.get()
}

/// The calls the UI pump consumes.
pub fn inbound() -> &'static smol::channel::Receiver<Inbound> {
    &channel().1
}

pub fn attached() -> usize {
    ATTACHED.load(Ordering::SeqCst)
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

    let directory = data_directory().ok_or_else(|| anyhow!("no instance data directory"))?;
    let discovery = publication(&directory, address, token);
    let sender = channel().0.clone();
    let connection_token = discovery.token.clone();
    std::thread::Builder::new()
        .name("waku-rpa".to_owned())
        .spawn(move || accept_loop(listener, sender, connection_token))
        .context("could not start the RPA listener thread")?;

    write_discovery(&directory.join(CONFIG_FILE), &discovery)?;
    Ok(discovery)
}

fn publication(directory: &Path, address: SocketAddr, token: String) -> Discovery {
    let database = crate::persistence::StateStore::default_path();
    let checkout = directory
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let started_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
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
    }
}

fn write_discovery(path: &Path, discovery: &Discovery) -> anyhow::Result<()> {
    let contents = serde_json::to_vec_pretty(discovery)?;
    std::fs::write(path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;
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
    fn discovery_file_carries_the_connection() {
        let directory = std::env::temp_dir().join(format!("waku-rpa-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CONFIG_FILE);
        let discovery = publication(
            &directory,
            "127.0.0.1:41234".parse().unwrap(),
            "token".into(),
        );
        write_discovery(&path, &discovery).unwrap();

        let read: Discovery =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.address, "127.0.0.1:41234");
        assert_eq!(read.token, "token");
        assert_eq!(read.pid, std::process::id());
        std::fs::remove_dir_all(&directory).ok();
    }
}
