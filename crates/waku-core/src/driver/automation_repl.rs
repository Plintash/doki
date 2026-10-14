//! Debug-only, process-local daemon capability for the native REPL. Providers
//! receive a nonsecret file path; only the Rust REPL reads its credentials.
use std::path::PathBuf;
use std::sync::Arc;

pub(super) const ENV_FILE: &str = "WAKU_RPA_REPL_ENV_FILE";

#[cfg(debug_assertions)]
#[derive(Clone)]
struct Endpoint {
    generation: uuid::Uuid,
    address: String,
    token: String,
}

#[cfg(debug_assertions)]
static ENDPOINTS: std::sync::Mutex<Vec<Endpoint>> = std::sync::Mutex::new(Vec::new());

#[cfg(debug_assertions)]
pub(crate) struct EndpointGuard<'a> {
    endpoints: &'a std::sync::Mutex<Vec<Endpoint>>,
    generation: uuid::Uuid,
}

#[cfg(debug_assertions)]
impl Drop for EndpointGuard<'_> {
    fn drop(&mut self) {
        self.endpoints
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|endpoint| endpoint.generation != self.generation);
    }
}

#[cfg(debug_assertions)]
fn register_endpoint(
    endpoints: &std::sync::Mutex<Vec<Endpoint>>,
    address: String,
    token: String,
) -> EndpointGuard<'_> {
    let generation = uuid::Uuid::new_v4();
    endpoints
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(Endpoint {
            generation,
            address,
            token,
        });
    EndpointGuard {
        endpoints,
        generation,
    }
}

#[cfg(debug_assertions)]
fn sole_endpoint(endpoints: &std::sync::Mutex<Vec<Endpoint>>) -> Option<Endpoint> {
    let endpoints = endpoints.lock().unwrap_or_else(|error| error.into_inner());
    match endpoints.as_slice() {
        [endpoint] => Some(endpoint.clone()),
        _ => None,
    }
}

#[cfg(debug_assertions)]
pub(crate) fn set_endpoint(address: String, token: String) -> EndpointGuard<'static> {
    register_endpoint(&ENDPOINTS, address, token)
}

pub(super) struct AutomationReplConfig {
    pub(super) repl_path: PathBuf,
    pub(super) env_file: PathBuf,
}

impl AutomationReplConfig {
    pub(super) fn load() -> anyhow::Result<Option<Arc<Self>>> {
        #[cfg(debug_assertions)]
        if let Some(endpoint) = sole_endpoint(&ENDPOINTS) {
            return Ok(Self::optional(
                endpoint,
                crate::computer_use::js_repl_server_path(),
            ));
        }
        Ok(None)
    }

    #[cfg(debug_assertions)]
    fn optional(endpoint: Endpoint, repl_path: anyhow::Result<PathBuf>) -> Option<Arc<Self>> {
        match repl_path.and_then(|path| Self::create(endpoint, path)) {
            Ok(config) => Some(config),
            Err(error) => {
                // Debug instrumentation must not prevent an ordinary provider
                // from starting in an unbundled development/test executable.
                // The path is nonsecret; never include endpoint contents here.
                eprintln!("waku-daemon: debug RPA REPL is unavailable: {error}");
                None
            }
        }
    }

    #[cfg(debug_assertions)]
    fn create(endpoint: Endpoint, repl_path: PathBuf) -> anyhow::Result<Arc<Self>> {
        use std::io::Write as _;
        let directory =
            std::env::temp_dir().join(format!("waku-rpa-repl-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        let config = Arc::new(Self {
            repl_path,
            env_file: directory.join("endpoint.json"),
        });
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&config.env_file)?;
        file.write_all(&serde_json::to_vec(&serde_json::json!({
            "address": endpoint.address, "token": endpoint.token
        }))?)?;
        Ok(config)
    }
}

impl Drop for AutomationReplConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.env_file);
        if let Some(directory) = self.env_file.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn automation_repl_overlapping_endpoints_fail_closed_and_guards_remove_only_their_owner() {
        let endpoints = std::sync::Mutex::new(Vec::new());
        let first = register_endpoint(&endpoints, "first".into(), "fixture-a".into());
        assert_eq!(sole_endpoint(&endpoints).unwrap().address, "first");
        let second = register_endpoint(&endpoints, "second".into(), "fixture-b".into());
        assert!(sole_endpoint(&endpoints).is_none());
        drop(first);
        assert_eq!(sole_endpoint(&endpoints).unwrap().address, "second");
        let third = register_endpoint(&endpoints, "third".into(), "fixture-c".into());
        assert!(sole_endpoint(&endpoints).is_none());
        drop(third);
        assert_eq!(sole_endpoint(&endpoints).unwrap().address, "second");
        drop(second);
        assert!(sole_endpoint(&endpoints).is_none());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn automation_repl_missing_optional_package_does_not_block_provider_start() {
        let config = AutomationReplConfig::optional(
            Endpoint {
                generation: uuid::Uuid::new_v4(),
                address: "127.0.0.1:1".into(),
                token: "fixture-secret".into(),
            },
            Err(anyhow::anyhow!("fixture REPL package is absent")),
        );
        assert!(config.is_none());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn automation_repl_capability_is_private_and_lifetime_owned() {
        let config = AutomationReplConfig::create(
            Endpoint {
                generation: uuid::Uuid::new_v4(),
                address: "127.0.0.1:1".into(),
                token: "fixture-secret".into(),
            },
            "/fixture/repl".into(),
        )
        .unwrap();
        let path = config.env_file.clone();
        let directory = path.parent().unwrap().to_owned();
        assert!(!path.to_string_lossy().contains("fixture-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let retained = config.clone();
        drop(config);
        assert!(path.is_file());
        drop(retained);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_cannot_register_automation_repl() {
        assert!(AutomationReplConfig::load().unwrap().is_none());
    }
}
