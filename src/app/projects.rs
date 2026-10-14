//! Project registration and non-destructive picker choices.
//!
//! Project IDs own durable issue/session bindings. Never remove a duplicate
//! record or rewrite those bindings merely to make a picker less repetitive.

use super::*;

/// A remote daemon's paths must never be resolved on the desktop host.
#[derive(Clone, Copy)]
pub(super) enum ProjectPathHost {
    Local,
    Remote,
}

impl Waku {
    pub(super) fn register_project(
        &mut self,
        path: PathBuf,
        preferred: Option<Uuid>,
        host: ProjectPathHost,
    ) -> Uuid {
        let previous_count = self.state.projects.len();
        let id = register_project(&mut self.state.projects, path, preferred, host);
        if self.state.projects.len() > previous_count {
            self.pending_project_registrations.insert(id);
        }
        id
    }
}

/// A catalog requested before a registration's save must not erase it and
/// allow the next Add action to allocate another ID. Retire this protection
/// once the daemon echoes the ID; all other catalog omissions stay authoritative.
/// The sync worker requests and delivers catalogs serially for this daemon.
pub(super) fn merge_catalog(
    local: &mut Vec<Project>,
    mut remote: Vec<Project>,
    pending: &mut HashSet<Uuid>,
) {
    let acknowledged = remote
        .iter()
        .map(|project| project.id)
        .collect::<HashSet<_>>();
    pending.retain(|id| !acknowledged.contains(id));
    remote.extend(
        local
            .drain(..)
            .filter(|project| pending.contains(&project.id)),
    );
    *local = remote;
}

/// One-shot registration, outside rendering. Resolve aliases only when both
/// local directories can be inspected; inaccessible paths retain exact-path
/// identity. A Git worktree is its own directory, not its repository's root.
pub(super) fn register_project(
    projects: &mut Vec<Project>,
    path: PathBuf,
    preferred: Option<Uuid>,
    host: ProjectPathHost,
) -> Uuid {
    let canonical = match host {
        ProjectPathHost::Local => path.canonicalize().ok(),
        ProjectPathHost::Remote => None,
    };
    let existing = projects
        .iter()
        .filter(|project| Some(project.id) == preferred)
        .chain(
            projects
                .iter()
                .filter(|project| Some(project.id) != preferred),
        )
        .find(|project| {
            project.path == path
                || canonical.as_ref().is_some_and(|canonical| {
                    project.path.canonicalize().ok().as_ref() == Some(canonical)
                })
        });
    if let Some(project) = existing {
        return project.id;
    }
    let project = Project::from_path(path);
    let id = project.id;
    projects.push(project);
    id
}

/// Pure in-memory projection. Old exact-path registrations share one choice;
/// the selected ID wins so merely opening/reselecting a menu cannot replace
/// an issue's or conversation's binding with another registration's ID.
pub(super) fn picker_projects(projects: &[Project], selected: Option<Uuid>) -> Vec<&Project> {
    let mut seen = HashSet::new();
    projects
        .iter()
        .filter(|project| Some(project.id) == selected)
        .chain(
            projects
                .iter()
                .filter(|project| Some(project.id) != selected),
        )
        .filter(|project| !project.is_projectless())
        .filter(|project| seen.insert(&project.path))
        .collect()
}

/// A collapsed Board filter must include issues bound to every old ID for
/// that path. This changes the view only, never the stored issue binding.
pub(super) fn project_path_ids(projects: &[Project], selected: Uuid) -> HashSet<Uuid> {
    let selected_path = projects
        .iter()
        .find(|project| project.id == selected)
        .map(|project| &project.path);
    projects
        .iter()
        .filter(|project| project.id == selected || selected_path == Some(&project.path))
        .map(|project| project.id)
        .chain(std::iter::once(selected))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("waku-project-identity-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn directory(&self, path: &str) -> PathBuf {
            let path = self.0.join(path);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn registration_reuses_directory_and_resolvable_parent_alias() {
        let fixture = Fixture::new();
        let path = fixture.directory("repo");
        fixture.directory("repo/child");
        let mut projects = Vec::new();
        let first = register_project(&mut projects, path.clone(), None, ProjectPathHost::Local);
        assert_eq!(
            register_project(&mut projects, path.clone(), None, ProjectPathHost::Local),
            first
        );
        assert_eq!(
            register_project(
                &mut projects,
                path.join("child/.."),
                None,
                ProjectPathHost::Local
            ),
            first
        );
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, path);
    }

    #[cfg(unix)]
    #[test]
    fn registration_resolves_local_symlinks_but_never_remote_paths() {
        let fixture = Fixture::new();
        let path = fixture.directory("repo");
        let alias = fixture.0.join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let mut projects = vec![Project::from_path(alias.clone())];
        let first = projects[0].id;
        assert_eq!(
            register_project(&mut projects, path.clone(), None, ProjectPathHost::Local),
            first
        );
        assert_eq!(
            projects[0].path, alias,
            "existing execution path is untouched"
        );
        let remote = register_project(&mut projects, path.clone(), None, ProjectPathHost::Remote);
        assert_ne!(remote, first);
        assert_eq!(
            register_project(&mut projects, path, None, ProjectPathHost::Remote),
            remote
        );
    }

    #[test]
    fn distinct_directories_names_worktrees_and_unresolved_parents_stay_distinct() {
        let fixture = Fixture::new();
        let paths = [
            fixture.directory("one/repo"),
            fixture.directory("two/repo"),
            fixture.directory("worktrees/branch"),
            fixture.directory("other"),
            fixture.0.join("missing/../one/repo"),
        ];
        let mut projects = Vec::new();
        let ids = paths
            .into_iter()
            .map(|path| register_project(&mut projects, path, None, ProjectPathHost::Local))
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), 5);
        assert_eq!(picker_projects(&projects, None).len(), 5);
    }

    #[test]
    fn legacy_duplicates_keep_selected_identity_and_all_existing_bindings() {
        let mut state = PersistedState::fresh(PathBuf::from("/unavailable/one/repo"));
        let original = state.projects[0].id;
        let duplicate = Project::from_path(state.projects[0].path.clone());
        let selected = duplicate.id;
        state.projects.push(duplicate);
        state
            .projects
            .push(Project::from_path(PathBuf::from("/unavailable/two/repo")));
        let mut task = Task::new("Existing issue", TaskState::Doing, 0);
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id: selected,
            path: PathBuf::from("/worktrees/issue"),
            branch: "issue-branch".to_owned(),
        });
        state.sessions[0].task_id = Some(task.id);
        state.sessions[0].project_id = selected;
        state.sessions[0].workspace = SessionWorkspace::Worktree {
            path: PathBuf::from("/worktrees/conversation"),
            branch: "conversation-branch".to_owned(),
        };
        state.tasks.push(task);
        let mut original_task = Task::new("Issue bound to the other ID", TaskState::Backlog, 1);
        original_task.workspace = Some(TaskWorkspace::Local {
            project_id: original,
        });
        let mut original_session = AgentSession::new(original, ProviderKind::Pi);
        original_session.task_id = Some(original_task.id);
        state.tasks.push(original_task);
        state.push_session(original_session);
        let before = serde_json::to_value(&state).unwrap();
        let options = picker_projects(&state.projects, Some(selected));
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id, selected);
        assert!(!options.iter().any(|project| project.id == original));
        let group = project_path_ids(&state.projects, original);
        assert!(group.contains(&original));
        assert!(group.contains(&selected));
        assert!(!group.contains(&options[1].id));
        assert_eq!(
            register_project(
                &mut state.projects,
                PathBuf::from("/unavailable/one/repo"),
                Some(selected),
                ProjectPathHost::Local
            ),
            selected
        );
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
    }

    #[test]
    fn stale_catalog_cannot_erase_a_pending_registration_and_create_a_second_id() {
        let path = PathBuf::from("/unavailable/pending-project");
        let mut projects = Vec::new();
        let id = register_project(&mut projects, path.clone(), None, ProjectPathHost::Local);
        let mut pending = HashSet::from([id]);

        merge_catalog(&mut projects, Vec::new(), &mut pending);
        assert_eq!(projects.len(), 1);
        assert_eq!(
            register_project(&mut projects, path, None, ProjectPathHost::Local),
            id
        );
        assert_eq!(pending, HashSet::from([id]));

        let mut acknowledged = projects[0].clone();
        acknowledged.name = "Daemon-owned name".to_owned();
        merge_catalog(&mut projects, vec![acknowledged], &mut pending);
        assert!(pending.is_empty());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "Daemon-owned name");

        // Protection is bounded to the pending registration, not a permanent
        // union of catalogs that could resurrect deleted/replaced records.
        merge_catalog(&mut projects, Vec::new(), &mut pending);
        assert!(projects.is_empty());
    }

    #[test]
    fn catalog_omissions_only_preserve_registrations_pending_on_this_client() {
        let ordinary = Project::from_path(PathBuf::from("/unavailable/existing"));
        let pending_project = Project::from_path(PathBuf::from("/unavailable/pending"));
        let pending_id = pending_project.id;
        let mut projects = vec![ordinary, pending_project];
        let mut pending = HashSet::from([pending_id]);
        merge_catalog(&mut projects, Vec::new(), &mut pending);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, pending_id);
    }
}
