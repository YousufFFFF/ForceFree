//! Git safety. This is the trust layer.
//!
//! A cleanup tool people actually run on their dev drive is one that visibly
//! refuses to touch work that isn't backed up. ForceFree never deletes inside a
//! repo that has uncommitted changes or unpushed commits — no flag overrides it.
//!
//! Two rules govern everything here:
//!
//!   1. **The gate is the enclosing worktree, not the project directory.** A
//!      project detected at `repo/services/api` is inside `repo`, and `repo`'s
//!      state is what decides. Asking only whether `services/api/.git` exists
//!      answers "not a repo" and waves the deletion through.
//!   2. **Unknown is never safe.** If we cannot determine the state — git is
//!      missing, the repository is corrupt, another process holds `index.lock` —
//!      the answer is `Unknown` and nothing is reclaimed. A trust layer that
//!      fails open is not a trust layer.
//!
//! Shells out to `git` rather than linking libgit2: fewer build deps, and it
//! respects the user's own git config. Swap to the `git2` crate if the process
//! spawn ever shows up in profiles.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitState {
    /// Not a git repo at all. We have no safety signal, so treat with care.
    NotARepo,
    /// Tracked files modified, or untracked files present.
    Dirty,
    /// Clean tree, but commits exist locally that no remote has.
    Unpushed,
    /// Clean and fully pushed. Everything here is recoverable with `git clone`.
    Clean,
    /// There is a repository here but we could not read its state. Refused.
    Unknown,
}

impl GitState {
    /// Whether ForceFree is willing to delete reclaimable targets here.
    pub fn is_safe_to_reclaim(self) -> bool {
        matches!(self, GitState::Clean | GitState::NotARepo)
    }

    pub fn label(self) -> &'static str {
        match self {
            GitState::NotARepo => "not a repo",
            GitState::Dirty => "uncommitted changes",
            GitState::Unpushed => "unpushed commits",
            GitState::Clean => "clean + pushed",
            GitState::Unknown => "git state unknown",
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// The root of the worktree containing `dir`, if git can tell us.
///
/// `rev-parse --show-toplevel` does the upward walk itself and handles the
/// awkward cases — worktrees and submodules, where `.git` is a file rather than
/// a directory. On Windows it comes back with forward slashes (`C:/Users/...`);
/// that is fine here because the value is only ever handed back to `git -C` or
/// used as a map key, but it will surprise anyone who tries to compare it
/// against a `Path` built the normal way.
fn worktree_root(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(s.trim()))
}

/// Does `start`, or any ancestor of it, hold a `.git` entry — file or directory?
/// No process spawn. This is what lets us tell "git says this is not a
/// repository" apart from "we could not ask git at all".
fn has_git_dir_above(start: &Path) -> bool {
    start.ancestors().any(|dir| dir.join(".git").exists())
}

/// We have no worktree root. Either this genuinely is not a repository, or git
/// is unusable here. Only the first of those is safe to reclaim in, so anything
/// with a `.git` above it is refused.
fn unresolved_state(dir: &Path) -> GitState {
    if has_git_dir_above(dir) {
        GitState::Unknown
    } else {
        GitState::NotARepo
    }
}

fn state_of_worktree(toplevel: &Path) -> GitState {
    // Any modified tracked file or untracked file makes this dirty. Run from the
    // worktree root so the answer covers the whole repository, not just the
    // subdirectory a project happened to be detected in.
    match git(toplevel, &["status", "--porcelain"]) {
        Some(s) if !s.trim().is_empty() => return GitState::Dirty,
        Some(_) => {}
        None => return GitState::Unknown,
    }

    // Commits on any local branch that no remote branch contains.
    // Empty output = everything is on a remote somewhere.
    match git(
        toplevel,
        &["log", "--branches", "--not", "--remotes", "--oneline"],
    ) {
        Some(s) if !s.trim().is_empty() => GitState::Unpushed,
        Some(_) => GitState::Clean,
        None => GitState::Unknown,
    }
}

/// Repository state keyed by worktree root.
///
/// A monorepo can hold many detected projects and they all share one answer.
/// Without this, a repo with twenty projects would run `git status` twenty
/// times; with it the cost is one `rev-parse` per project plus two calls per
/// distinct repository.
#[derive(Default)]
pub struct RepoCache {
    by_toplevel: HashMap<PathBuf, GitState>,
}

impl RepoCache {
    pub fn state_for(&mut self, project_root: &Path) -> GitState {
        let Some(toplevel) = worktree_root(project_root) else {
            return unresolved_state(project_root);
        };
        if let Some(&cached) = self.by_toplevel.get(&toplevel) {
            return cached;
        }
        let state = state_of_worktree(&toplevel);
        self.by_toplevel.insert(toplevel, state);
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A one-shot inspection: a fresh cache has nothing memoised, so this is
    /// exactly "ask git about this directory".
    fn inspect(dir: &Path) -> GitState {
        RepoCache::default().state_for(dir)
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git must be installed to run these tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Identity is set per-repo so the tests neither depend on nor disturb the
    /// developer's global git config.
    fn init_repo(dir: &Path) {
        run(dir, &["init", "-q", "-b", "main", "."]);
        run(dir, &["config", "user.email", "test@forcefree.invalid"]);
        run(dir, &["config", "user.name", "ForceFree Test"]);
    }

    fn commit_all(dir: &Path, msg: &str) {
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", msg]);
    }

    /// A project detected below the repository root must be gated on the
    /// repository, not on whether it has its own `.git`. This is the case that
    /// silently offered up a `.venv` inside a repo holding uncommitted work.
    #[test]
    fn nested_project_in_dirty_repo_is_blocked() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);

        let nested = repo.join("services").join("api");
        fs::create_dir_all(nested.join(".venv")).unwrap();
        fs::write(nested.join("requirements.txt"), "flask\n").unwrap();
        commit_all(repo, "init");

        fs::write(repo.join("thesis.txt"), "work that is not backed up").unwrap();

        let state = inspect(&nested);
        assert_eq!(state, GitState::Dirty);
        assert!(!state.is_safe_to_reclaim());
    }

    #[test]
    fn nested_project_in_clean_pushed_repo_is_clean() {
        let tmp = TempDir::new().unwrap();

        // A bare repo on disk stands in for a remote; no network involved.
        let origin = tmp.path().join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "-q", "--bare", "-b", "main", "."]);

        let repo = tmp.path().join("work");
        let nested = repo.join("services").join("api");
        fs::create_dir_all(&nested).unwrap();
        init_repo(&repo);
        fs::write(nested.join("requirements.txt"), "flask\n").unwrap();
        commit_all(&repo, "init");
        run(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        run(&repo, &["push", "-q", "origin", "main"]);

        let state = inspect(&nested);
        assert_eq!(state, GitState::Clean);
        assert!(state.is_safe_to_reclaim());
    }

    #[test]
    fn repo_root_dirty_is_blocked() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        fs::write(repo.join("main.py"), "print(1)\n").unwrap();
        commit_all(repo, "init");
        fs::write(repo.join("main.py"), "print(2)\n").unwrap();

        assert_eq!(inspect(repo), GitState::Dirty);
    }

    /// A `.git` we cannot read is the case that used to fail open: git errors,
    /// the old code returned NotARepo, and NotARepo is reclaimable.
    #[test]
    fn corrupt_git_dir_is_unknown_not_safe() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("broken");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git").join("HEAD"), "not a valid ref\n").unwrap();

        let state = inspect(&dir);
        assert_eq!(state, GitState::Unknown);
        assert!(!state.is_safe_to_reclaim());
    }

    #[test]
    fn plain_directory_is_not_a_repo() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("plain");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(inspect(&dir), GitState::NotARepo);
    }

    #[test]
    fn cache_agrees_with_uncached_inspection() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(repo, "init");
        fs::write(repo.join("b.txt"), "uncommitted\n").unwrap();

        let nested = repo.join("services").join("api");
        fs::create_dir_all(&nested).unwrap();

        let mut cache = RepoCache::default();
        // Asked twice, so the second answer comes from the map rather than git.
        assert_eq!(cache.state_for(&nested), inspect(&nested));
        assert_eq!(cache.state_for(&nested), GitState::Dirty);
    }

    #[test]
    fn unresolvable_states_are_never_reclaimable() {
        assert!(!GitState::Dirty.is_safe_to_reclaim());
        assert!(!GitState::Unpushed.is_safe_to_reclaim());
        assert!(!GitState::Unknown.is_safe_to_reclaim());
        assert!(GitState::Clean.is_safe_to_reclaim());
    }
}
