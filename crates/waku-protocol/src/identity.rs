//! Shared application identity used by the daemon and desktop client.

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Doki Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Doki";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "sh.doki.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "sh.doki";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "Doki Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Doki";
