//! A private OpenCode 2 service for the tests that need a live one.
//!
//! The API routes are a moving target, and a canned fixture cannot notice a
//! rename — the identity route alone moved four times in a fortnight, and every
//! live test was `#[ignore]`d and pointed at whatever the developer happened to
//! have running, so nobody ran them and the drift went unnoticed.
//!
//! This starts a FOREGROUND server on an ephemeral port with every XDG
//! directory pointed into a temp tree, so it never reads or writes the user's
//! service, database, config, or registration — the rule that an adopted
//! service is never signalled protects the user's daemon, and this is not it.
//! The registration is written into that temp tree, which is where the
//! production discovery path is pointed, so `shared()`, `attached()`, and the
//! session and driver tests exercise the real path rather than a shortcut.
//!
//! One server serves the whole test binary: `OnceLock` never drops, so the
//! child is killed from an `atexit` handler instead. Without it every `cargo
//! test` would leave a server behind.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::http_wire::Endpoint;
use crate::model::ProviderKind;
use crate::opencode2_service::ServiceRegistration;

/// How long the server may take to answer its identity route.
const START_BUDGET: Duration = Duration::from_secs(30);
/// How long its model catalogue may take to arrive. It is fetched from
/// models.dev on a cold data directory, which is the slow part of a first run.
const CATALOG_BUDGET: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(200);

/// The Basic username the service demands.
const SERVICE_USER: &str = "opencode";
const PASSWORD: &str = "waku-live-service";

/// The skill this harness plants in the workspace.
///
/// A catalogue Waku wrote itself is the only way to assert the skill route's
/// shape without depending on whatever the developer has configured.
pub(crate) const PROBE_SKILL: &str = "waku-live-probe";

pub(crate) struct LiveService {
    /// Kept so the child handle outlives the tests; dropping a `Child` neither
    /// kills nor reaps it, so cleanup is the `atexit` handler below.
    _child: Child,
    pub(crate) endpoint: Endpoint,
    pub(crate) registration: ServiceRegistration,
    /// A workspace the service may create sessions in, with [`PROBE_SKILL`]
    /// already planted under `.opencode/skills`.
    pub(crate) workspace: PathBuf,
}

impl LiveService {
    /// Waits until the service publishes a model catalogue.
    ///
    /// Called by the tests that assert on the catalogue rather than from
    /// [`service`], so a machine without network only fails the tests that
    /// actually need models.dev.
    pub(crate) fn wait_for_models(&self) {
        self.wait_for("model catalogue", || {
            self.catalogue(&self.endpoint, |endpoint, directory| {
                crate::opencode2_api::list_models(endpoint, Some(directory))
            })
        });
    }

    /// Waits until the location publishes its agent catalogue.
    ///
    /// A cold location publishes its registries in stages — empty, then
    /// built-ins — so a catalogue read immediately after the first session is
    /// created can be empty and mean "not yet" rather than "gone".
    pub(crate) fn wait_for_agents(&self) {
        self.wait_for("agent catalogue", || {
            self.catalogue(&self.endpoint, |endpoint, directory| {
                crate::opencode2_api::list_agents(endpoint, Some(directory))
            })
        });
    }

    fn catalogue<T>(
        &self,
        endpoint: &Endpoint,
        read: impl Fn(&Endpoint, &str) -> crate::opencode2_api::Result<Vec<T>>,
    ) -> bool {
        let directory = self.workspace.to_string_lossy().into_owned();
        read(endpoint, &directory).is_ok_and(|items| !items.is_empty())
    }

    fn wait_for(&self, what: &str, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + CATALOG_BUDGET;
        loop {
            if ready() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the live service published no {what} within {CATALOG_BUDGET:?}. The model \
                 catalogue is fetched from models.dev at startup, so that one needs network"
            );
            thread::sleep(POLL);
        }
    }
}

/// The CLI these tests drive, or a failure that says what to install.
///
/// A missing CLI is not a reason to skip quietly: the whole point of these
/// tests is that the provider's assumptions get checked against a real service.
pub(crate) fn binary() -> PathBuf {
    crate::model::provider_binary(ProviderKind::OpenCode2).expect(
        "the live OpenCode 2 tests need the v2 CLI installed: either `opencode2`, or `opencode` \
         2.x. Skip them with `cargo test -p waku-core -- --skip opencode2`",
    )
}

pub(crate) fn service() -> &'static LiveService {
    static SERVICE: OnceLock<LiveService> = OnceLock::new();
    SERVICE.get_or_init(start)
}

fn start() -> LiveService {
    let binary = binary();
    let root = std::env::temp_dir().join(format!("waku-live-opencode2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let data = root.join("data");
    let state = root.join("state");
    let config = root.join("config");
    let workspace = root.join("work");
    for directory in [&data, &state, &config, &workspace] {
        std::fs::create_dir_all(directory).expect("the live service needs its own directories");
    }
    // Planted before the server starts so its first scan sees it: skills are
    // rescanned from a file watcher, and a test that races the watcher would be
    // testing its own timing.
    let skill = workspace.join(format!(".opencode/skills/{PROBE_SKILL}"));
    std::fs::create_dir_all(&skill).expect("the probe skill needs a directory");
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {PROBE_SKILL}\ndescription: Planted by the live-service harness\n---\nSay OK.\n"),
    )
    .expect("the probe skill needs a body");

    let port = free_port();
    let mut command = crate::command_env::command(&binary);
    command
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .env("XDG_DATA_HOME", &data)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", &config)
        .env("OPENCODE_PASSWORD", PASSWORD)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("could not start {}: {error}", binary.display()));
    arm_cleanup(child.id());

    let url = format!("http://127.0.0.1:{port}");
    let endpoint = Endpoint::basic(&url, SERVICE_USER, PASSWORD).expect("a valid loopback URL");
    let identity = wait_for_identity(&endpoint, &mut child);
    let registration = ServiceRegistration {
        id: "waku-live-service".to_owned(),
        version: Some(identity.version),
        url,
        pid: identity.pid,
        password: PASSWORD.to_owned(),
    };
    // Point the production discovery path at this service, then publish the
    // descriptor exactly as a real one is published.
    crate::opencode2_service::use_test_state_root(state);
    let registration_path = crate::opencode2_service::state_directory().join("service.json");
    std::fs::write(
        &registration_path,
        serde_json::json!({
            "id": registration.id,
            "version": registration.version,
            "url": registration.url,
            "pid": registration.pid,
            "password": registration.password,
        })
        .to_string(),
    )
    .expect("the live service must register itself");

    LiveService {
        _child: child,
        endpoint,
        registration,
        // Canonicalized once, here: the service compares location directories
        // by exact string equality, and on macOS `temp_dir()` is the symlinked
        // `/var/...` spelling of `/private/var/...`, so a location resolved one
        // way and queried the other is a DIFFERENT, cold location.
        workspace: workspace
            .canonicalize()
            .expect("the live workspace must resolve"),
    }
}

fn wait_for_identity(
    endpoint: &Endpoint,
    child: &mut Child,
) -> crate::opencode2_api::ServiceIdentity {
    let deadline = Instant::now() + START_BUDGET;
    loop {
        match crate::opencode2_api::identify(endpoint) {
            Ok(identity) => return identity,
            Err(error) => {
                if let Some(status) = child.try_wait().ok().flatten() {
                    panic!("the live service exited before identifying itself ({status}): {error}");
                }
                assert!(
                    Instant::now() < deadline,
                    "the live service did not answer its identity route within {START_BUDGET:?}: \
                     {error}"
                );
                thread::sleep(POLL);
            }
        }
    }
}

/// A port nothing is listening on, so the service can be told which to take.
///
/// `serve` refuses `--port 0`, so this is the only way to avoid a fixed port
/// that a second test run — or the user's own service — could already hold.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("a loopback port must be bindable")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

#[cfg(unix)]
static SERVICE_PID: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn kill_service_at_exit() {
    let pid = SERVICE_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: `kill(2)` on a pid this process spawned and still owns. The
        // service is our child, not the user's daemon.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Kills the service when the test binary exits.
///
/// `OnceLock` never drops, so a `Drop` impl cannot be the cleanup, and a test
/// run that leaves a server behind is exactly the kind of quiet mess this
/// harness exists to avoid.
#[cfg(unix)]
fn arm_cleanup(pid: u32) {
    SERVICE_PID.store(pid as i32, Ordering::SeqCst);
    // SAFETY: `atexit` takes a plain function pointer and the handler only
    // reads a static and calls `kill(2)`.
    unsafe {
        libc::atexit(kill_service_at_exit);
    }
}

/// Windows has no `atexit` in the standard library, so the child is left to the
/// next `opencode service stop` there.
#[cfg(not(unix))]
fn arm_cleanup(_pid: u32) {}
