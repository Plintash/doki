//! Which development instance a window belongs to.
//!
//! Several worktrees can run a debug build at once, and each owns its own
//! `<worktree>/temp/app.db`. A window that names its checkout and its database
//! can be told apart at a glance - and while testing a feature that depends on
//! stored tasks, the badge answers which history is on screen. Release builds
//! have one app and no label.
//!
//! The identity is resolved once, on first use, so a frame only reads it.

use std::path::Path;
use std::sync::OnceLock;

struct Instance {
    /// `waku-streaming · temp/app.db`, or `None` in a release build.
    label: Option<String>,
    database: String,
}

fn instance() -> &'static Instance {
    static INSTANCE: OnceLock<Instance> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let database = crate::persistence::StateStore::default_path();
        Instance {
            label: cfg!(debug_assertions)
                .then(|| label_for(&database))
                .flatten(),
            database: database.display().to_string(),
        }
    })
}

/// `waku-streaming · temp/app.db` for a `<checkout>/temp/app.db` path.
fn label_for(database: &Path) -> Option<String> {
    let file = database.file_name()?.to_string_lossy();
    let parent = database.parent()?;
    let directory = parent.file_name()?.to_string_lossy();
    let checkout = parent.parent()?.file_name()?.to_string_lossy();
    Some(format!("{checkout} · {directory}/{file}"))
}

/// The badge text for this process, or `None` in a release build.
pub fn debug_label() -> Option<&'static str> {
    instance().label.as_deref()
}

/// The database this window reads, for the badge's tooltip.
pub fn database_path() -> &'static str {
    &instance().database
}

/// The window title: the app name, plus the instance identity in a debug build.
pub fn window_title() -> String {
    match debug_label() {
        Some(label) => format!("{} — {label}", crate::identity::APP_NAME),
        None => crate::identity::APP_NAME.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_names_the_checkout_and_its_database() {
        assert_eq!(
            label_for(Path::new("/Users/x/proj/waku-streaming/temp/app.db")),
            Some("waku-streaming · temp/app.db".to_owned())
        );
        assert_eq!(label_for(Path::new("app.db")), None);
        assert_eq!(label_for(Path::new("temp/app.db")), None);
    }
}
