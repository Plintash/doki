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

use super::protocol::{self, Error, Reply, Request};

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
    pub call: AuthenticatedCall,
    pub reply: std::sync::mpsc::Sender<Reply>,
}

/// Invalid authenticated requests still visit the UI failure boundary, so a
/// malformed call gets the same evidence as an action. No state is collected
/// for a caller that has not proved it owns the instance token.
pub struct AuthenticatedCall {
    pub id: u64,
    pub request: Result<Request, Error>,
}

static SERVICE: OnceLock<Discovery> = OnceLock::new();
static IDENTITY: OnceLock<Discovery> = OnceLock::new();
static GENERATION: OnceLock<uuid::Uuid> = OnceLock::new();

/// A process generation is separate from the source/build identity.
pub fn generation() -> uuid::Uuid {
    *GENERATION.get_or_init(|| {
        debug_generation(
            cfg!(debug_assertions),
            std::env::var("WAKU_RPA_INSTANCE_ID").ok().as_deref(),
        )
        .unwrap_or_else(uuid::Uuid::new_v4)
    })
}
fn debug_generation(debug: bool, value: Option<&str>) -> Option<uuid::Uuid> {
    debug
        .then(|| value.and_then(|value| uuid::Uuid::parse_str(value).ok()))
        .flatten()
}

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
    if !legacy_enabled(
        cfg!(debug_assertions),
        std::env::var("WAKU_RPA_LEGACY_TCP").ok().as_deref(),
    ) {
        return None;
    }
    match launch() {
        Ok(discovery) => {
            let _ = SERVICE.set(discovery);
            SERVICE.get()
        }
        // The bridge is development tooling; a port or filesystem problem
        // must not take the app down with it.
        Err(error) => {
            eprintln!("waku-rpa: could not start the endpoint: {error:#}");
            None
        }
    }
}

/// The live publication. The pump only runs after [`start`] has published it.
pub fn discovery() -> &'static Discovery {
    SERVICE
        .get()
        .expect("the RPA pump runs only after the endpoint is published")
}

/// Public instance identity for snapshots and failure evidence. The bearer
/// token stays only in the local discovery file used to authenticate clients;
/// it must never be copied into logs, test output or returned evidence.
pub fn instance_identity() -> serde_json::Value {
    let discovery = IDENTITY.get_or_init(|| {
        let directory = data_directory().expect("the app database has a parent directory");
        publication(&directory, "127.0.0.1:0".parse().unwrap(), String::new())
    });
    let mut identity = public_identity(discovery);
    identity.as_object_mut().unwrap().remove("address");
    identity["generation"] = serde_json::json!(generation());
    identity
}

fn legacy_enabled(debug: bool, flag: Option<&str>) -> bool {
    debug && flag == Some("1")
}

fn public_identity(discovery: &Discovery) -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": discovery.protocol_version,
        "address": discovery.address,
        "pid": discovery.pid,
        "checkout": discovery.checkout,
        "database": discovery.database,
        "app": discovery.app,
        "build": discovery.build,
        "startedAtMs": discovery.started_at_ms,
    })
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

fn publication(_directory: &Path, address: SocketAddr, token: String) -> Discovery {
    let database = crate::persistence::StateStore::default_path();
    let checkout = env!("CARGO_MANIFEST_DIR").to_owned();
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
        build: env!("WAKU_BUILD_ID").to_owned(),
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
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("waku-rpa: could not accept a connection: {error}");
                continue;
            }
        };
        let sender = sender.clone();
        let token = token.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("waku-rpa-conn".to_owned())
            .spawn(move || serve_connection(stream, sender, &token))
        {
            eprintln!("waku-rpa: could not serve a connection: {error}");
        }
    }
}

fn serve_connection(stream: TcpStream, sender: smol::channel::Sender<Inbound>, token: &str) {
    let _attached = AttachGuard::new();
    let reader_stream = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("waku-rpa: could not read a connection: {error}");
            return;
        }
    };
    let mut writer = stream;
    let mut lines = BufReader::new(reader_stream).lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match authenticate_call(&line, token) {
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
            Err(reply) => reply,
        };
        let Ok(encoded) = serde_json::to_string(&reply) else {
            break;
        };
        if writeln!(writer, "{encoded}").is_err() {
            break;
        }
    }
}

fn authenticate_call(line: &str, token: &str) -> Result<AuthenticatedCall, Reply> {
    #[derive(Deserialize)]
    struct Envelope {
        id: u64,
        version: u32,
        token: String,
    }
    let envelope = serde_json::from_str::<Envelope>(line).map_err(|error| {
        Reply::error(
            0,
            Error::new(
                "invalid_call",
                format!("the call has no valid envelope: {error}"),
            ),
        )
    })?;
    if envelope.token != token {
        return Err(Reply::error(
            envelope.id,
            Error::new(
                "unauthorized",
                "the call did not present this instance's token",
            ),
        ));
    }
    let request = if envelope.version != protocol::PROTOCOL_VERSION {
        Err(Error::new(
            "version_mismatch",
            format!(
                "client protocol {} does not match this app's {}",
                envelope.version,
                protocol::PROTOCOL_VERSION
            ),
        ))
    } else {
        serde_json::from_str::<Request>(line).map_err(|error| {
            Error::new(
                "invalid_call",
                format!("the call is not a valid RPA request: {error}"),
            )
        })
    };
    Ok(AuthenticatedCall {
        id: envelope.id,
        request,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_generation_override_is_debug_only_and_validated() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(debug_generation(true, Some(&id.to_string())), Some(id));
        assert_eq!(debug_generation(false, Some(&id.to_string())), None);
        assert_eq!(debug_generation(true, Some("invalid")), None);
    }

    #[test]
    fn legacy_listener_requires_explicit_debug_opt_in() {
        assert!(!legacy_enabled(true, None));
        assert!(!legacy_enabled(true, Some("true")));
        assert!(legacy_enabled(true, Some("1")));
        assert!(!legacy_enabled(false, Some("1")));
    }

    #[test]
    fn daemon_identity_exists_without_starting_the_legacy_listener() {
        let identity = instance_identity();
        assert_eq!(identity["build"], env!("WAKU_BUILD_ID"));
        assert_ne!(identity["build"], env!("CARGO_PKG_VERSION"));
        assert_eq!(identity["generation"], generation().to_string());
        assert!(identity.get("token").is_none());
        assert!(identity.get("address").is_none());
    }

    #[test]
    fn authenticated_invalid_calls_reach_the_evidence_boundary() {
        for (version, kind, expected) in [
            (protocol::PROTOCOL_VERSION, "unknown", "invalid_call"),
            (2, "snapshot", "version_mismatch"),
            (0, "snapshot", "version_mismatch"),
        ] {
            let line =
                serde_json::json!({"id": 17, "version": version, "token": "secret", "type": kind})
                    .to_string();
            let call = authenticate_call(&line, "secret").unwrap();
            assert_eq!(call.id, 17);
            assert_eq!(call.request.unwrap_err().code, expected);
        }
    }

    #[test]
    fn diagnostic_filters_require_session_identity_on_the_legacy_transport() {
        let session_id = uuid::Uuid::from_u128(8);
        for kind in ["logs", "timeline"] {
            let request = |query| {
                serde_json::json!({
                    "id": 17, "version": protocol::PROTOCOL_VERSION, "token": "secret",
                    "type": kind, "query": query,
                })
                .to_string()
            };
            let current = request(serde_json::json!({"sessionId": session_id}));
            assert!(
                authenticate_call(&current, "secret")
                    .unwrap()
                    .request
                    .is_ok()
            );
            let legacy = request(serde_json::json!({"taskId": session_id}));
            assert_eq!(
                authenticate_call(&legacy, "secret")
                    .unwrap()
                    .request
                    .unwrap_err()
                    .code,
                "invalid_call"
            );
        }
    }

    #[test]
    fn unauthenticated_calls_never_collect_state_or_diagnostics() {
        let line = serde_json::json!({"id": 17, "version": 0, "token": "wrong", "type": "unknown"})
            .to_string();
        let reply = authenticate_call(&line, "secret").err().unwrap();
        let error = reply.error.unwrap();
        assert_eq!(error.code, "unauthorized");
        assert!(error.bundle.is_none());
        assert!(error.state.is_none());
        assert!(authenticate_call("not json", "secret").is_err());
    }

    #[test]
    fn snapshots_publish_identity_without_the_authentication_token() {
        let discovery = publication(
            Path::new("/tmp/rpa-identity-test"),
            "127.0.0.1:41234".parse().unwrap(),
            "private-test-bearer-token".into(),
        );
        let identity = public_identity(&discovery);
        assert_eq!(identity["protocolVersion"], protocol::PROTOCOL_VERSION);
        assert_eq!(identity["pid"], std::process::id());
        assert_eq!(identity["address"], "127.0.0.1:41234");
        assert!(identity.get("token").is_none());
        assert!(!identity.to_string().contains(&discovery.token));
        assert!(
            serde_json::to_value(&discovery)
                .unwrap()
                .get("token")
                .is_some(),
            "the local connection file must retain its authentication token"
        );
    }

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
