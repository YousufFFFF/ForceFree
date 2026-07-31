//! Scanning: walk the tree, recognise projects, size what's reclaimable.
//!
//! Two rules keep this fast:
//!   1. Once a directory is recognised as a project root, we do not descend into
//!      it looking for more project roots. A monorepo's inner packages are found
//!      via the outer project's own reclaimable paths, not by re-walking.
//!   2. Sizing a target is a separate parallel walk, so a 200k-file
//!      `node_modules` never blocks discovery of the next project.

use crate::detector::Detector;
use crate::git::{self, GitState};
use jwalk::WalkDir;
use std::path::{Path, PathBuf};

/// One reclaimable directory that actually exists on disk, with its real size.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    pub bytes: u64,
    /// Entries we could not stat or descend into while sizing. Non-zero means
    /// `bytes` is a lower bound, and the report has to say so.
    pub unreadable: u32,
    pub restore_command: String,
    pub rebuild_seconds: u32,
}

impl Target {
    /// Bytes reclaimed per second of rebuild time. The core ranking metric:
    /// high value means "lots of space back, cheap to restore".
    pub fn efficiency(&self) -> f64 {
        self.bytes as f64 / self.rebuild_seconds.max(1) as f64
    }
}

/// A recognised project and everything reclaimable inside it.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub ecosystem: String,
    pub git_state: GitState,
    pub targets: Vec<Target>,
}

impl Project {
    pub fn total_bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.bytes).sum()
    }

    pub fn unreadable(&self) -> u32 {
        self.targets.iter().map(|t| t.unreadable).sum()
    }
}

/// Directory names we never walk into. Cheap win: avoids sizing the same bytes
/// twice and keeps us out of places we'd never act on.
const NEVER_DESCEND: &[&str] = &[".git", "$RECYCLE.BIN", "System Volume Information"];

/// Result of measuring a directory.
///
/// `unreadable` exists so a partial measurement is never mistaken for a
/// complete one. Discarding walk errors silently is how a 420 MB `node_modules`
/// on a OneDrive-backed folder came back as zero bytes and was dropped from the
/// report entirely — the output looked identical to the directory not existing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measured {
    pub bytes: u64,
    pub unreadable: u32,
}

fn dir_size(path: &Path) -> Measured {
    let mut m = Measured::default();
    for entry in WalkDir::new(path).skip_hidden(false) {
        match entry {
            Ok(e) => match e.metadata() {
                Ok(meta) if meta.is_file() => m.bytes += meta.len(),
                Ok(_) => {}
                Err(_) => m.unreadable += 1,
            },
            Err(_) => m.unreadable += 1,
        }
    }
    m
}

/// Is this measurement worth reporting?
///
/// An empty directory is genuinely nothing and gets dropped. One we *failed to
/// read* is reported, because "we could not measure this" and "this does not
/// exist" must not look the same to the user.
fn is_worth_reporting(m: Measured) -> bool {
    m.bytes > 0 || m.unreadable > 0
}

/// Walk `root`, returning every project found, largest first.
pub fn scan(root: &Path, detectors: &[Detector], aggressive: bool) -> Vec<Project> {
    let mut projects = Vec::new();
    let mut repos = git::RepoCache::default();

    let walker = WalkDir::new(root)
        .skip_hidden(false)
        .process_read_dir(|_, _, _, children| {
            children.retain(|c| {
                c.as_ref()
                    .map(|e| {
                        let name = e.file_name().to_string_lossy();
                        !NEVER_DESCEND.iter().any(|s| *s == name)
                    })
                    .unwrap_or(false)
            });
        });

    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_dir() {
            continue;
        }
        let dir = entry.path();

        // Skip anything already inside a project we've recorded.
        if projects
            .iter()
            .any(|p: &Project| dir.starts_with(&p.root) && dir != p.root)
        {
            continue;
        }

        let Some(detector) = detectors.iter().find(|d| d.matches(&dir)) else {
            continue;
        };

        // Gated on the enclosing worktree, which may be well above `dir`.
        let git_state = repos.state_for(&dir);

        let targets: Vec<Target> = detector
            .reclaimable
            .iter()
            .filter(|r| aggressive || !r.aggressive_only)
            .filter_map(|r| {
                let p = dir.join(&r.path);
                if !p.is_dir() {
                    return None;
                }
                let m = dir_size(&p);
                if !is_worth_reporting(m) {
                    return None;
                }
                Some(Target {
                    path: p,
                    bytes: m.bytes,
                    unreadable: m.unreadable,
                    restore_command: r.restore_command.clone(),
                    rebuild_seconds: r.rebuild_seconds,
                })
            })
            .collect();

        if targets.is_empty() {
            continue;
        }

        projects.push(Project {
            root: dir,
            ecosystem: detector.name.clone(),
            git_state,
            targets,
        });
    }

    projects.sort_by_key(|p| std::cmp::Reverse(p.total_bytes()));
    projects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::Reclaimable;
    use std::fs;
    use tempfile::TempDir;

    fn node_detector() -> Detector {
        Detector {
            id: "node".into(),
            name: "Node.js".into(),
            markers: vec!["package.json".into()],
            reclaimable: vec![Reclaimable {
                path: "node_modules".into(),
                restore_command: "npm ci".into(),
                rebuild_seconds: 45,
                aggressive_only: false,
            }],
        }
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn sizes_a_known_tree_exactly() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("node_modules");
        write(&dir.join("a.js"), 1000);
        write(&dir.join("pkg").join("b.js"), 2500);
        write(&dir.join("pkg").join("nested").join("c.js"), 12);

        let m = dir_size(&dir);
        assert_eq!(m.bytes, 3512);
        assert_eq!(m.unreadable, 0);
    }

    #[test]
    fn empty_target_is_dropped_but_unmeasurable_one_is_kept() {
        assert!(!is_worth_reporting(Measured::default()));
        assert!(is_worth_reporting(Measured {
            bytes: 0,
            unreadable: 3
        }));
        assert!(is_worth_reporting(Measured {
            bytes: 10,
            unreadable: 0
        }));
    }

    #[test]
    fn finds_project_and_names_its_targets() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        write(&proj.join("node_modules").join("dep.js"), 4096);

        let projects = scan(tmp.path(), &[node_detector()], false);

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].ecosystem, "Node.js");
        assert_eq!(projects[0].targets.len(), 1);
        assert_eq!(projects[0].targets[0].bytes, 4096);
        assert!(projects[0].targets[0].path.ends_with("node_modules"));
    }

    #[test]
    fn project_without_any_reclaimable_directory_is_not_reported() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();

        assert!(scan(tmp.path(), &[node_detector()], false).is_empty());
    }

    #[test]
    fn aggressive_only_targets_need_the_flag() {
        let mut detector = node_detector();
        detector.reclaimable[0].aggressive_only = true;

        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        write(&proj.join("node_modules").join("dep.js"), 1024);

        assert!(scan(tmp.path(), std::slice::from_ref(&detector), false).is_empty());
        assert_eq!(scan(tmp.path(), &[detector], true).len(), 1);
    }

    /// Producing a genuine read failure needs POSIX permissions; the Windows
    /// equivalent is ACL surgery. The counting logic itself is covered
    /// cross-platform by `empty_target_is_dropped_but_unmeasurable_one_is_kept`.
    #[cfg(unix)]
    #[test]
    fn unreadable_entries_are_counted_not_silently_dropped() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("node_modules");
        write(&dir.join("visible.js"), 500);
        let locked = dir.join("locked");
        fs::create_dir_all(&locked).unwrap();
        write(&locked.join("hidden.js"), 9999);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let m = dir_size(&dir);

        // Restore before the assertions so TempDir can always clean up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(m.bytes, 500, "readable part still counted");
        assert!(m.unreadable > 0, "failure must be recorded, not discarded");
        assert!(is_worth_reporting(m));
    }
}
