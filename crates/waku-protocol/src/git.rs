use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::ProviderKind;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct BranchEntry {
    pub name: String,
    pub checked_out_elsewhere: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct BranchSnapshot {
    #[ts(type = "string")]
    pub repository: PathBuf,
    pub current: Option<String>,
    pub detached_head: Option<String>,
    pub default_branch: Option<String>,
    /// Automatic worktree base, independent of the local review default.
    #[serde(default)]
    pub worktree_base_ref: Option<String>,
    /// Canonical local and remote branch refs; remote symbolic HEAD is omitted.
    #[serde(default)]
    pub worktree_base_refs: Vec<String>,
    pub branches: Vec<BranchEntry>,
    pub additions: u64,
    pub deletions: u64,
}

impl BranchSnapshot {
    pub fn display_branch(&self) -> Option<&str> {
        self.current.as_deref().or(self.detached_head.as_deref())
    }

    /// Match a saved/per-create choice without falling back for a missing ref.
    /// Old short choices refer only to local branches, never remote tracking
    /// branches with a coincidentally matching short name.
    pub fn worktree_base_choice(&self, requested: Option<&str>) -> Option<&str> {
        match requested {
            None => self.worktree_base_ref.as_deref(),
            Some(requested) => {
                let requested = requested.trim();
                self.worktree_base_refs
                    .iter()
                    .find(|reference| {
                        if requested.starts_with("refs/") {
                            reference.as_str() == requested
                        } else {
                            reference.strip_prefix("refs/heads/") == Some(requested)
                        }
                    })
                    .map(String::as_str)
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct CommitSnapshot {
    pub branch: String,
    pub additions: u64,
    pub deletions: u64,
    pub staged_additions: u64,
    pub staged_deletions: u64,
    pub has_staged: bool,
    pub has_unstaged: bool,
    pub can_push: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct AgentInvocation {
    pub provider: ProviderKind,
    #[ts(type = "string")]
    pub binary: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct CreatedWorktree {
    #[ts(type = "string")]
    pub path: PathBuf,
    pub branch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_snapshot() -> BranchSnapshot {
        serde_json::from_value(serde_json::json!({
            "repository": "/repository",
            "current": "feature",
            "detached_head": null,
            "default_branch": "main",
            "branches": [],
            "additions": 0,
            "deletions": 0
        }))
        .unwrap()
    }

    #[test]
    fn old_branch_snapshot_payload_defaults_worktree_metadata() {
        let snapshot = legacy_snapshot();
        assert_eq!(snapshot.worktree_base_ref, None);
        assert!(snapshot.worktree_base_refs.is_empty());
        // Older daemon metadata must not make the UI promise that creation
        // starts from current or from review's different default.
        assert_eq!(snapshot.worktree_base_choice(None), None);
    }

    #[test]
    fn worktree_choices_preserve_canonical_refs_and_legacy_local_names() {
        let mut snapshot = legacy_snapshot();
        snapshot.worktree_base_ref = Some("refs/remotes/origin/main".to_owned());
        snapshot.worktree_base_refs = vec![
            "refs/heads/main".to_owned(),
            "refs/remotes/origin/main".to_owned(),
        ];
        assert_eq!(
            snapshot.worktree_base_choice(None),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(
            snapshot.worktree_base_choice(Some("main")),
            Some("refs/heads/main")
        );
        assert_eq!(
            snapshot.worktree_base_choice(Some(" refs/heads/main ")),
            Some("refs/heads/main")
        );
        assert_eq!(
            snapshot.worktree_base_choice(Some("refs/remotes/origin/main")),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(snapshot.worktree_base_choice(Some("origin/main")), None);
        assert_eq!(snapshot.worktree_base_choice(Some("missing")), None);
        assert_eq!(snapshot.worktree_base_choice(Some("")), None);
        snapshot
            .worktree_base_refs
            .push("refs/heads/origin/main".to_owned());
        assert_eq!(
            snapshot.worktree_base_choice(Some("origin/main")),
            Some("refs/heads/origin/main")
        );
    }
}
