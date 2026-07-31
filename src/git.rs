//! Git safety. This is the trust layer.
//!
//! A cleanup tool people actually run on their dev drive is one that visibly
//! refuses to touch work that isn't backed up. ForceFree never deletes inside a
//! repo that has uncommitted changes or unpushed commits — no flag overrides it.
//!
//! Shells out to `git` rather than linking libgit2: fewer build deps, and it
//! respects the user's own git config. Swap to the `git2` crate if the process
//! spawn ever shows up in profiles.

use std::path::Path;
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

pub fn inspect(project_root: &Path) -> GitState {
    if !project_root.join(".git").exists() {
        return GitState::NotARepo;
    }

    // Any modified tracked file or untracked file makes this dirty.
    match git(project_root, &["status", "--porcelain"]) {
        Some(s) if !s.trim().is_empty() => return GitState::Dirty,
        None => return GitState::NotARepo,
        _ => {}
    }

    // Commits on HEAD that no remote branch contains.
    // Empty output = everything is on a remote somewhere.
    match git(
        project_root,
        &["log", "--branches", "--not", "--remotes", "--oneline"],
    ) {
        Some(s) if !s.trim().is_empty() => GitState::Unpushed,
        _ => GitState::Clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_directory_is_not_a_repo() {
        let dir = std::env::temp_dir().join("forcefree_git_test_plain");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(inspect(&dir), GitState::NotARepo);
    }

    #[test]
    fn dirty_and_unpushed_are_never_reclaimable() {
        assert!(!GitState::Dirty.is_safe_to_reclaim());
        assert!(!GitState::Unpushed.is_safe_to_reclaim());
        assert!(GitState::Clean.is_safe_to_reclaim());
    }
}
