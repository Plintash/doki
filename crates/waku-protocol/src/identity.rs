//! Shared application identity used by the daemon and desktop client.
//!
//! A debug build can be given its own name, bundle identity and data
//! directory through the environment, so two development worktrees can run
//! side by side without one watcher's `pkill`/`open` touching the other's
//! app, or sharing a database. Release builds ignore the overrides.

use std::sync::OnceLock;

fn override_or(key: &str, default: &str) -> String {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var(key) {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_owned();
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = key;
    default.to_owned()
}

#[cfg(debug_assertions)]
const DEFAULT_APP_NAME: &str = "Doki Debug";
#[cfg(not(debug_assertions))]
const DEFAULT_APP_NAME: &str = "Doki";

#[cfg(debug_assertions)]
const DEFAULT_APP_ID: &str = "sh.doki.dev";
#[cfg(not(debug_assertions))]
const DEFAULT_APP_ID: &str = "sh.doki";

#[cfg(debug_assertions)]
const DEFAULT_DATA_DIRECTORY_NAME: &str = "Doki Debug";
#[cfg(not(debug_assertions))]
const DEFAULT_DATA_DIRECTORY_NAME: &str = "Doki";

pub fn app_name() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE
        .get_or_init(|| override_or("WAKU_APP_NAME", DEFAULT_APP_NAME))
        .as_str()
}

pub fn app_id() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE
        .get_or_init(|| override_or("WAKU_APP_ID", DEFAULT_APP_ID))
        .as_str()
}

pub fn data_directory_name() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE
        .get_or_init(|| override_or("WAKU_DATA_DIR", DEFAULT_DATA_DIRECTORY_NAME))
        .as_str()
}
