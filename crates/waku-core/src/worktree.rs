//! Daemon-owned isolated Git worktrees for tasks.
//!
//! A draft records only the user's choice. The first submission creates the
//! worktree beneath `~/.waku/worktrees`, then every workspace consumer uses
//! the returned project-relative path for the lifetime of the task.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context as _, anyhow, bail};
use uuid::Uuid;

const DEFAULT_SLUG: &str = "new-worktree";
const MAX_SLUG_BYTES: usize = 48;
const MAX_CANDIDATES: usize = 100;

pub use waku_protocol::git::CreatedWorktree;

pub fn create(
    project_path: &Path,
    project_id: Uuid,
    session_id: Uuid,
    prompt: &str,
    base_branch: Option<&str>,
) -> anyhow::Result<CreatedWorktree> {
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not locate the home directory for ~/.waku/worktrees"))?
        .join(".waku/worktrees");
    create_in(
        project_path,
        &root,
        project_id,
        session_id,
        prompt,
        base_branch,
    )
}

fn create_in(
    project_path: &Path,
    worktree_root: &Path,
    project_id: Uuid,
    session_id: Uuid,
    prompt: &str,
    requested_base: Option<&str>,
) -> anyhow::Result<CreatedWorktree> {
    let project_path = fs::canonicalize(project_path)
        .with_context(|| format!("could not open project {}", project_path.display()))?;
    let repository = git_stdout(&project_path, &["rev-parse", "--show-toplevel"])
        .context("new worktrees require a Git repository")?;
    let repository = fs::canonicalize(PathBuf::from(repository.trim()))
        .context("could not resolve the Git repository root")?;
    let project_relative = project_path
        .strip_prefix(&repository)
        .context("the project is outside its Git repository root")?
        .to_owned();
    // Validate an exact branch ref and its commit before allocating anything.
    // Keep the canonical ref as Git's start point so branch.autoSetupMerge
    // still controls upstream tracking exactly as it does for worktree add.
    let base_ref = resolve_base(&repository, requested_base)?;
    let project_worktrees = worktree_root.join(project_id.to_string());
    fs::create_dir_all(&project_worktrees).with_context(|| {
        format!(
            "could not create the worktree directory {}",
            project_worktrees.display()
        )
    })?;

    let slug = worktree_slug(prompt);
    for index in 0..MAX_CANDIDATES {
        let name = candidate_name(&slug, index);
        let branch = format!("waku/{name}");
        let path = project_worktrees.join(&name);
        if path.exists() || local_branch_exists(&repository, &branch)? {
            continue;
        }

        let output = crate::command_env::plain_command("git")
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(&path)
            .arg(&base_ref)
            .current_dir(&repository)
            .output()
            .context("failed to execute git worktree add")?;
        if !output.status.success() {
            bail!("{}", command_error(&output));
        }

        let project_path = path.join(&project_relative);
        if !project_path.is_dir() {
            bail!(
                "Git created the worktree, but its project directory is missing: {}",
                project_path.display()
            );
        }
        return Ok(CreatedWorktree {
            path: project_path,
            branch,
        });
    }

    // A UUID fallback makes the final attempt independent of human-readable
    // name collisions while keeping the common path and branch pleasant.
    let fallback = format!("{slug}-{}", &session_id.simple().to_string()[..8]);
    let branch = format!("waku/{fallback}");
    let path = project_worktrees.join(&fallback);
    if path.exists() || local_branch_exists(&repository, &branch)? {
        bail!("could not allocate a unique Git worktree name");
    }
    let output = crate::command_env::plain_command("git")
        .args(["worktree", "add", "-b"])
        .arg(&branch)
        .arg(&path)
        .arg(&base_ref)
        .current_dir(&repository)
        .output()
        .context("failed to execute git worktree add")?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    Ok(CreatedWorktree {
        path: path.join(project_relative),
        branch,
    })
}

/// Resolve and validate the canonical ref shared by display and creation.
/// An explicit unavailable choice is an error, never an automatic fallback.
pub(crate) fn resolve_base(
    repository: &Path,
    requested_base: Option<&str>,
) -> anyhow::Result<String> {
    let reference = match requested_base {
        Some(branch) => explicit_base_ref(repository, branch)?,
        None => default_base_ref(repository)?,
    };
    base_commit(repository, &reference)
        .with_context(|| format!("base branch `{reference}` is unavailable"))?;
    Ok(reference)
}

/// Prefer the local branch named by `origin/HEAD`, so a worktree starts from
/// the default branch without fetching or mutating the user's ordinary
/// checkout. Repositories without that metadata fall back to their current
/// branch, then detached `HEAD`. Keep this policy shared with branch discovery
/// so the UI describes the same automatic base that creation will use.
fn default_base_ref(repository: &Path) -> anyhow::Result<String> {
    if let Some(remote_default) = git_optional_stdout(
        repository,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )? {
        if let Some(local_default) = remote_default.strip_prefix("refs/remotes/origin/")
            && local_branch_exists(repository, local_default)?
        {
            return Ok(format!("refs/heads/{local_default}"));
        }
        return Ok(remote_default);
    }
    if let Some(branch) = git_optional_stdout(repository, &["symbolic-ref", "--quiet", "HEAD"])? {
        return Ok(branch);
    }
    Ok("HEAD".to_owned())
}

/// Persisted choices use full refs to distinguish local and remote branches.
/// Historical per-create overrides used short *local* branch names; qualify
/// those before asking Git, rather than allowing ambiguous revision syntax.
fn explicit_base_ref(repository: &Path, requested_base: &str) -> anyhow::Result<String> {
    let requested_base = requested_base.trim();
    if requested_base.is_empty() {
        bail!("the worktree base branch must not be empty");
    }
    let base_ref = if requested_base.starts_with("refs/") {
        if !requested_base.starts_with("refs/heads/")
            && !requested_base.starts_with("refs/remotes/")
        {
            bail!("the worktree base must be a local or remote branch ref");
        }
        requested_base.to_owned()
    } else {
        format!("refs/heads/{requested_base}")
    };
    git_stdout(repository, &["check-ref-format", &base_ref])
        .with_context(|| format!("invalid worktree base branch `{requested_base}`"))?;
    Ok(base_ref)
}

fn base_commit(repository: &Path, base_ref: &str) -> anyhow::Result<String> {
    let revision = if base_ref == "HEAD" {
        "HEAD".to_owned()
    } else {
        // `show-ref --verify` requires an exact ref, even if a tag or a local
        // branch has a name that would shadow it under revision DWIM rules.
        git_stdout(repository, &["show-ref", "--verify", "--hash", base_ref])?
    };
    git_stdout(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )
}

fn local_branch_exists(repository: &Path, branch: &str) -> anyhow::Result<bool> {
    let output = crate::command_env::plain_command("git")
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(repository)
        .output()
        .context("failed to inspect Git branches")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("{}", command_error(&output)),
    }
}

fn git_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = crate::command_env::plain_command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to execute git")?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_optional_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let output = crate::command_env::plain_command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to execute git")?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    bail!("{}", command_error(&output))
}

fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr
    }
}

fn candidate_name(slug: &str, index: usize) -> String {
    if index == 0 {
        slug.to_owned()
    } else {
        format!("{slug}-{}", index + 1)
    }
}

fn worktree_slug(prompt: &str) -> String {
    let mut slug = prompt
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    slug.truncate(MAX_SLUG_BYTES);
    if slug.is_empty() {
        DEFAULT_SLUG.to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = crate::command_env::plain_command("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", command_error(&output));
    }

    struct RepositoryFixture {
        root: PathBuf,
        repository: PathBuf,
        worktrees: PathBuf,
    }

    impl RepositoryFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("waku-base-test-{}", Uuid::new_v4()));
            let repository = root.join("repository");
            fs::create_dir_all(&repository).unwrap();
            run_git(&repository, &["init", "-b", "main"]);
            run_git(&repository, &["config", "core.autocrlf", "false"]);
            run_git(&repository, &["config", "user.name", "Waku Tests"]);
            run_git(&repository, &["config", "user.email", "waku@example.com"]);
            fs::write(repository.join("README.md"), "main\n").unwrap();
            run_git(&repository, &["add", "."]);
            run_git(&repository, &["commit", "-m", "initial"]);
            Self {
                worktrees: root.join("worktrees"),
                root,
                repository,
            }
        }

        fn commit(&self, content: &str) -> String {
            fs::write(self.repository.join("README.md"), content).unwrap();
            run_git(&self.repository, &["add", "."]);
            run_git(&self.repository, &["commit", "-m", content.trim()]);
            git_stdout(&self.repository, &["rev-parse", "HEAD"]).unwrap()
        }

        fn create(&self, prompt: &str, base: Option<&str>) -> anyhow::Result<CreatedWorktree> {
            create_in(
                &self.repository,
                &self.worktrees,
                Uuid::new_v4(),
                Uuid::new_v4(),
                prompt,
                base,
            )
        }

        fn origin_head(&self, branch: &str, commit: &str) {
            let reference = format!("refs/remotes/origin/{branch}");
            run_git(&self.repository, &["update-ref", &reference, commit]);
            run_git(
                &self.repository,
                &["symbolic-ref", "refs/remotes/origin/HEAD", &reference],
            );
        }
    }

    impl Drop for RepositoryFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    #[test]
    fn creates_isolated_worktrees_from_the_default_branch() {
        let root = std::env::temp_dir().join(format!("waku-worktree-test-{}", Uuid::new_v4()));
        let repository = root.join("repository");
        let project = repository.join("packages/app");
        fs::create_dir_all(&project).unwrap();
        run_git(&repository, &["init", "-b", "main"]);
        run_git(&repository, &["config", "core.autocrlf", "false"]);
        fs::write(project.join("README.md"), "main\n").unwrap();
        run_git(&repository, &["add", "."]);
        run_git(
            &repository,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "initial",
            ],
        );
        run_git(
            &repository,
            &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
        );
        run_git(
            &repository,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        run_git(&repository, &["checkout", "-b", "feature"]);
        fs::write(project.join("README.md"), "feature\n").unwrap();
        run_git(&repository, &["add", "."]);
        run_git(
            &repository,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "feature",
            ],
        );

        let worktree_root = root.join("worktrees");
        let project_id = Uuid::new_v4();
        let first = create_in(
            &project,
            &worktree_root,
            project_id,
            Uuid::new_v4(),
            "Build a project selector",
            None,
        )
        .unwrap();
        assert_eq!(first.branch, "waku/build-a-project-selector");
        assert_eq!(
            fs::read_to_string(first.path.join("README.md")).unwrap(),
            "main\n"
        );
        fs::write(first.path.join("README.md"), "worktree\n").unwrap();
        assert_eq!(
            fs::read_to_string(project.join("README.md")).unwrap(),
            "feature\n"
        );

        let second = create_in(
            &project,
            &worktree_root,
            project_id,
            Uuid::new_v4(),
            "Build a project selector",
            None,
        )
        .unwrap();
        assert_eq!(second.branch, "waku/build-a-project-selector-2");

        let from_feature = create_in(
            &project,
            &worktree_root,
            project_id,
            Uuid::new_v4(),
            "Use selected base",
            Some("feature"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(from_feature.path.join("README.md")).unwrap(),
            "feature\n"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn canonical_refs_distinguish_local_remote_and_legacy_local_overrides() {
        let fixture = RepositoryFixture::new();
        let repository = &fixture.repository;
        let remote_commit = git_stdout(repository, &["rev-parse", "HEAD"]).unwrap();
        fixture.origin_head("main", &remote_commit);
        let local_commit = fixture.commit("local main\n");
        // A tag named main and a local branch named origin/main must not
        // influence canonical branch selection through Git's DWIM rules.
        run_git(repository, &["tag", "main", &remote_commit]);
        run_git(repository, &["branch", "origin/main", &local_commit]);

        for (index, (base, expected)) in [
            ("refs/heads/main", local_commit.as_str()),
            ("main", local_commit.as_str()),
            ("refs/remotes/origin/main", remote_commit.as_str()),
            ("origin/main", local_commit.as_str()),
        ]
        .into_iter()
        .enumerate()
        {
            let created = fixture
                .create(&format!("explicit base {index}"), Some(base))
                .unwrap();
            assert_eq!(
                git_stdout(&created.path, &["rev-parse", "HEAD"]).unwrap(),
                expected,
                "wrong commit for {base}"
            );
        }

        let automatic = fixture.create("automatic local", None).unwrap();
        assert_eq!(
            git_stdout(&automatic.path, &["rev-parse", "HEAD"]).unwrap(),
            local_commit
        );
        assert_eq!(resolve_base(repository, None).unwrap(), "refs/heads/main");
    }

    #[test]
    fn pathological_full_ref_ambiguity_is_an_error_not_a_different_commit() {
        let fixture = RepositoryFixture::new();
        let repository = &fixture.repository;
        let original_commit = git_stdout(repository, &["rev-parse", "HEAD"]).unwrap();
        fixture.commit("local main\n");
        run_git(repository, &["tag", "refs/heads/main", &original_commit]);
        let refs_before = git_stdout(repository, &["show-ref"]).unwrap();
        let worktrees_before =
            git_stdout(repository, &["worktree", "list", "--porcelain"]).unwrap();

        // Git refuses an ambiguous fully spelled start point. Preserve that
        // error, rather than falling back to a tag, current branch, or OID
        // that would drop configured upstream tracking semantics.
        assert!(
            fixture
                .create("ambiguous full ref", Some("refs/heads/main"))
                .is_err()
        );
        assert_eq!(git_stdout(repository, &["show-ref"]).unwrap(), refs_before);
        assert_eq!(
            git_stdout(repository, &["worktree", "list", "--porcelain"]).unwrap(),
            worktrees_before
        );
    }

    #[test]
    fn worktree_upstream_tracking_remains_controlled_by_git_configuration() {
        let fixture = RepositoryFixture::new();
        let repository = &fixture.repository;
        run_git(repository, &["remote", "add", "origin", "."]);
        fixture.origin_head("trunk", "HEAD");
        run_git(repository, &["config", "branch.autoSetupMerge", "true"]);

        let tracked = fixture.create("tracked remote default", None).unwrap();
        assert_eq!(
            git_stdout(
                repository,
                &[
                    "config",
                    "--get",
                    &format!("branch.{}.remote", tracked.branch)
                ]
            )
            .unwrap(),
            "origin"
        );
        assert_eq!(
            git_stdout(
                repository,
                &[
                    "config",
                    "--get",
                    &format!("branch.{}.merge", tracked.branch)
                ]
            )
            .unwrap(),
            "refs/heads/trunk"
        );

        run_git(repository, &["config", "branch.autoSetupMerge", "false"]);
        let untracked = fixture
            .create("untracked remote choice", Some("refs/remotes/origin/trunk"))
            .unwrap();
        assert_eq!(
            git_optional_stdout(
                repository,
                &[
                    "config",
                    "--get",
                    &format!("branch.{}.remote", untracked.branch)
                ],
            )
            .unwrap(),
            None
        );

        run_git(repository, &["config", "branch.autoSetupMerge", "always"]);
        let local = fixture
            .create("tracked local choice", Some("main"))
            .unwrap();
        assert_eq!(
            git_stdout(
                repository,
                &[
                    "config",
                    "--get",
                    &format!("branch.{}.remote", local.branch)
                ]
            )
            .unwrap(),
            "."
        );
        assert_eq!(
            git_stdout(
                repository,
                &["config", "--get", &format!("branch.{}.merge", local.branch)]
            )
            .unwrap(),
            "refs/heads/main"
        );
    }

    #[test]
    fn invalid_or_missing_explicit_refs_never_fall_back_or_create_anything() {
        let fixture = RepositoryFixture::new();
        fixture.origin_head("main", "HEAD");
        let blob = git_stdout(&fixture.repository, &["hash-object", "-w", "README.md"]).unwrap();
        run_git(
            &fixture.repository,
            &["update-ref", "refs/remotes/origin/not-a-commit", &blob],
        );
        let before = git_stdout(&fixture.repository, &["show-ref"]).unwrap();
        for requested in [
            "",
            "   ",
            "missing",
            "origin/main",
            "refs/heads/missing",
            "refs/remotes/origin/missing",
            "refs/remotes/origin/not-a-commit",
            "refs/tags/main",
            "main~1",
            "refs/heads/main^{commit}",
            "--help",
            "HEAD",
        ] {
            assert!(
                fixture.create("must not create", Some(requested)).is_err(),
                "unexpectedly accepted {requested:?}"
            );
            assert!(!fixture.worktrees.exists());
            assert_eq!(
                git_stdout(&fixture.repository, &["show-ref"]).unwrap(),
                before
            );
        }
    }

    #[test]
    fn automatic_remote_default_is_used_instead_of_the_current_branch() {
        let fixture = RepositoryFixture::new();
        fixture.origin_head("trunk", "HEAD");
        fixture.commit("current branch has diverged\n");
        let created = fixture.create("automatic remote", None).unwrap();
        assert_eq!(
            fs::read_to_string(created.path.join("README.md")).unwrap(),
            "main\n"
        );
        assert_eq!(
            resolve_base(&fixture.repository, None).unwrap(),
            "refs/remotes/origin/trunk"
        );
        let snapshot = crate::git_branch::inspect(&fixture.repository)
            .unwrap()
            .unwrap();
        // Review historically falls back to current, but the worktree UI
        // must show the same remote commit source that create actually uses.
        assert_eq!(snapshot.default_branch.as_deref(), Some("main"));
        assert_eq!(
            snapshot.worktree_base_ref.as_deref(),
            Some("refs/remotes/origin/trunk")
        );
    }

    #[test]
    fn automatic_falls_back_to_current_then_detached_head() {
        let fixture = RepositoryFixture::new();
        run_git(&fixture.repository, &["checkout", "-b", "feature"]);
        let feature_commit = fixture.commit("feature\n");
        let from_current = fixture.create("from current", None).unwrap();
        assert_eq!(
            git_stdout(&from_current.path, &["rev-parse", "HEAD"]).unwrap(),
            feature_commit
        );
        assert_eq!(
            resolve_base(&fixture.repository, None).unwrap(),
            "refs/heads/feature"
        );

        run_git(&fixture.repository, &["checkout", "--detach", "HEAD"]);
        let from_detached = fixture.create("from detached", None).unwrap();
        assert_eq!(
            git_stdout(&from_detached.path, &["rev-parse", "HEAD"]).unwrap(),
            feature_commit
        );
        let snapshot = crate::git_branch::inspect(&fixture.repository)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.current, None);
        assert_eq!(snapshot.worktree_base_ref.as_deref(), Some("HEAD"));
    }

    #[test]
    fn broken_automatic_default_errors_and_is_not_advertised_as_current() {
        let fixture = RepositoryFixture::new();
        run_git(
            &fixture.repository,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/missing",
            ],
        );
        assert!(fixture.create("broken automatic", None).is_err());
        assert!(!fixture.worktrees.exists());
        let snapshot = crate::git_branch::inspect(&fixture.repository)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.default_branch.as_deref(), Some("main"));
        assert_eq!(snapshot.worktree_base_ref, None);
    }

    #[test]
    fn creation_preserves_source_dirty_state_and_does_not_fetch() {
        let fixture = RepositoryFixture::new();
        let repository = &fixture.repository;
        let original_commit = git_stdout(repository, &["rev-parse", "HEAD"]).unwrap();
        fixture.origin_head("main", &original_commit);
        run_git(repository, &["checkout", "-b", "feature"]);
        let feature_commit = fixture.commit("feature committed\n");

        // Point origin at a local repository whose main is ahead of our
        // tracking ref. An accidental fetch would change origin/main.
        let origin = fixture.root.join("origin.git");
        run_git(
            &fixture.root,
            &[
                "clone",
                "--bare",
                repository.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        run_git(&origin, &["update-ref", "refs/heads/main", &feature_commit]);
        run_git(
            repository,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let fetch_head = repository.join(".git/FETCH_HEAD");
        fs::write(&fetch_head, "existing fetch metadata\n").unwrap();

        fs::write(repository.join("README.md"), "staged edit\n").unwrap();
        run_git(repository, &["add", "README.md"]);
        fs::write(repository.join("README.md"), "unstaged edit\n").unwrap();
        fs::write(repository.join("untracked.txt"), "untracked edit\n").unwrap();
        let staged = git_stdout(repository, &["diff", "--cached", "--binary"]).unwrap();
        let unstaged = git_stdout(repository, &["diff", "--binary"]).unwrap();
        let index = fs::read(repository.join(".git/index")).unwrap();
        let head = fs::read(repository.join(".git/HEAD")).unwrap();

        let created = fixture.create("preserve dirty source", None).unwrap();
        assert_eq!(
            fs::read_to_string(created.path.join("README.md")).unwrap(),
            "main\n"
        );
        assert!(!created.path.join("untracked.txt").exists());
        assert_eq!(fs::read(repository.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read(repository.join(".git/HEAD")).unwrap(), head);
        assert_eq!(
            fs::read_to_string(repository.join("README.md")).unwrap(),
            "unstaged edit\n"
        );
        assert_eq!(
            fs::read_to_string(repository.join("untracked.txt")).unwrap(),
            "untracked edit\n"
        );
        assert_eq!(
            git_stdout(repository, &["diff", "--cached", "--binary"]).unwrap(),
            staged
        );
        assert_eq!(
            git_stdout(repository, &["diff", "--binary"]).unwrap(),
            unstaged
        );
        assert_eq!(
            git_stdout(repository, &["rev-parse", "refs/remotes/origin/main"]).unwrap(),
            original_commit
        );
        assert_eq!(
            fs::read_to_string(fetch_head).unwrap(),
            "existing fetch metadata\n"
        );
    }

    #[test]
    fn slug_is_short_and_branch_safe() {
        assert_eq!(worktree_slug("你好 👋"), DEFAULT_SLUG);
        assert_eq!(
            worktree_slug("Fix Project/Worktree Picker!"),
            "fix-project-worktree-picker"
        );
        assert!(worktree_slug(&"a".repeat(100)).len() <= MAX_SLUG_BYTES);
    }
}
