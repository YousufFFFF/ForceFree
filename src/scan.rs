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
use crate::links;
use jwalk::WalkDir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One reclaimable directory that actually exists on disk, with its real size.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    /// Apparent size — what `du` would tell you.
    pub bytes: u64,
    /// Of `bytes`, how much would survive deletion — either because something
    /// outside also links to it, or because it is the same physical file
    /// counted twice inside. See [`crate::links`].
    pub shared_bytes: u64,
    /// Entries we could not stat or descend into while sizing. Non-zero means
    /// `bytes` is a lower bound, and the report has to say so.
    pub unreadable: u32,
    pub restore_command: String,
    pub rebuild_seconds: u32,
}

impl Target {
    /// What deleting this would actually return to the filesystem. Hard linked
    /// bytes are still referenced elsewhere, so removing this copy frees nothing.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.bytes.saturating_sub(self.shared_bytes)
    }

    /// Bytes reclaimed per second of rebuild time. The core ranking metric:
    /// high value means "lots of space back, cheap to restore".
    ///
    /// Deliberately over `reclaimable_bytes`, not `bytes` — ranking on apparent
    /// size is what made a pnpm `node_modules` look like the best win in a scan
    /// when almost none of it was really there.
    pub fn efficiency(&self) -> f64 {
        self.reclaimable_bytes() as f64 / self.rebuild_seconds.max(1) as f64
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
    pub fn reclaimable_bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.reclaimable_bytes()).sum()
    }

    pub fn shared_bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.shared_bytes).sum()
    }

    pub fn rebuild_seconds(&self) -> u64 {
        self.targets.iter().map(|t| t.rebuild_seconds as u64).sum()
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
    /// Apparent size: every entry counted, as `du` without `-l` would.
    pub bytes: u64,
    /// Of `bytes`, how much would survive deletion because something outside
    /// this directory also links to it.
    pub shared_bytes: u64,
    pub unreadable: u32,
}

/// One physical file seen inside a target, however many paths point at it.
struct Occupant {
    /// Total links to this data anywhere on the volume.
    links: u32,
    /// How many paths inside *this* target resolved to it.
    seen_inside: u32,
    size: u64,
}

fn dir_size(path: &Path, check_links: bool) -> Measured {
    let mut m = Measured::default();
    let mut occupants: HashMap<links::FileId, Occupant> = HashMap::new();
    // Bytes belonging to files whose links we actually resolved. Anything we
    // could not probe is excluded from the hard link arithmetic entirely.
    let mut accounted = 0u64;

    for entry in WalkDir::new(path).skip_hidden(false) {
        match entry {
            Ok(e) => {
                // jwalk does not yield an Err for a directory it could not read
                // into — it hands back the entry with the failure parked in
                // `read_children_error`. That is the usual way sizing goes wrong
                // (permissions, a cloud-sync placeholder that won't hydrate), so
                // missing it here would leave the whole count silently short.
                if e.read_children_error.is_some() {
                    m.unreadable += 1;
                }
                match e.metadata() {
                    Ok(meta) if meta.is_file() => {
                        m.bytes += meta.len();
                        if !check_links {
                            continue;
                        }
                        // Unix hands identity and link count over with the stat.
                        // Windows needs a handle, so only pay for it there.
                        let facts = match links::from_metadata(&meta) {
                            Some(f) => Some(f),
                            None => links::probe(&e.path()),
                        };
                        match facts {
                            Some(f) => {
                                accounted += meta.len();
                                let slot = occupants.entry(f.id).or_insert(Occupant {
                                    links: f.links,
                                    seen_inside: 0,
                                    size: meta.len(),
                                });
                                slot.seen_inside += 1;
                            }
                            // Could not tell. Folded into the same uncertainty
                            // the report already knows how to show, rather than
                            // being quietly assumed reclaimable.
                            None => m.unreadable += 1,
                        }
                    }
                    Ok(_) => {}
                    Err(_) => m.unreadable += 1,
                }
            }
            Err(_) => m.unreadable += 1,
        }
    }

    if check_links {
        // Sum each physical file once, and only when every link to it is inside
        // this directory. Counting per *path* would double a file that is hard
        // linked twice within the target — which is exactly what cargo does
        // between `target/debug/deps/` and `target/debug/`.
        let freed: u64 = occupants
            .values()
            .filter(|o| links::frees_bytes(o.links, o.seen_inside))
            .map(|o| o.size)
            .sum();

        // The gap between apparent size and what deletion returns. Files we
        // could not probe stay out of it and are optimistically left in the
        // reclaimable figure — they already raised `unreadable`, so the report
        // marks the number as approximate.
        m.shared_bytes = accounted.saturating_sub(freed);
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

/// How the caller wants the scan performed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Include targets marked `aggressive_only`.
    pub aggressive: bool,
    /// Skip hard link accounting. Faster, at the cost of reporting apparent
    /// size as though all of it were reclaimable — see [`crate::links`].
    pub skip_link_check: bool,
}

/// Walk `root`, returning every project found, in discovery order. Ranking is
/// deliberately not done here — see `report::render`.
pub fn scan(root: &Path, detectors: &[Detector], opts: Options) -> Vec<Project> {
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
            .filter(|r| opts.aggressive || !r.aggressive_only)
            .filter_map(|r| {
                let p = dir.join(&r.path);
                if !p.is_dir() {
                    return None;
                }
                let m = dir_size(&p, !opts.skip_link_check);
                if !is_worth_reporting(m) {
                    return None;
                }
                Some(Target {
                    path: p,
                    bytes: m.bytes,
                    shared_bytes: m.shared_bytes,
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

        let m = dir_size(&dir, false);
        assert_eq!(m.bytes, 3512);
        assert_eq!(m.unreadable, 0);
    }

    #[test]
    fn empty_target_is_dropped_but_unmeasurable_one_is_kept() {
        assert!(!is_worth_reporting(Measured::default()));
        assert!(is_worth_reporting(Measured {
            unreadable: 3,
            ..Default::default()
        }));
        assert!(is_worth_reporting(Measured {
            bytes: 10,
            ..Default::default()
        }));
    }

    fn target(bytes: u64, shared_bytes: u64, rebuild_seconds: u32) -> Target {
        Target {
            path: "node_modules".into(),
            bytes,
            shared_bytes,
            unreadable: 0,
            restore_command: "pnpm install".into(),
            rebuild_seconds,
        }
    }

    /// The pnpm case: a directory that looks enormous but is almost entirely
    /// hard links into a shared store, so deleting it returns almost nothing.
    #[test]
    fn hardlinked_bytes_are_not_reclaimable() {
        let t = target(1000, 900, 10);
        assert_eq!(t.reclaimable_bytes(), 100);
    }

    #[test]
    fn reclaimable_bytes_never_underflows() {
        // A sampled estimate could in principle overshoot the apparent size.
        assert_eq!(target(100, 500, 10).reclaimable_bytes(), 0);
    }

    /// Ranking on apparent size puts the pnpm tree first; ranking on what would
    /// actually be freed puts it last. This is the whole product thesis.
    #[test]
    fn efficiency_ranks_on_reclaimable_not_apparent() {
        let pnpm = target(600_000_000, 540_000_000, 45); // 600 MB, 90% shared
        let venv = target(400_000_000, 0, 60); // 400 MB, all real

        assert!(
            pnpm.bytes > venv.bytes,
            "the pnpm tree does look bigger, which is the trap"
        );
        assert!(
            venv.efficiency() > pnpm.efficiency(),
            "but the venv is the better deal and must rank first"
        );
    }

    /// pnpm's shape: the second link lives in a store outside the target, so
    /// deleting the target returns nothing for that file.
    ///
    /// `hard_link` works on Windows and Unix alike, so unlike the permission
    /// test this exercises the real detection path on every platform.
    #[test]
    fn a_link_from_outside_makes_bytes_unreclaimable() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store");
        let dir = tmp.path().join("node_modules");

        write(&store.join("shared.js"), 4000);
        fs::create_dir_all(&dir).unwrap();
        fs::hard_link(store.join("shared.js"), dir.join("shared.js")).unwrap();
        write(&dir.join("own.js"), 1000);

        let m = dir_size(&dir, true);

        assert_eq!(m.bytes, 5000, "apparent size counts both");
        assert_eq!(
            m.shared_bytes, 4000,
            "the externally linked file frees nothing"
        );
        assert_eq!(m.unreadable, 0);
    }

    /// Cargo's shape: a file hard linked twice *within* the target. Both links
    /// disappear when the directory does, so every byte is reclaimable — but
    /// apparent size counted it twice, and that duplication has to be netted out.
    #[test]
    fn links_wholly_inside_the_target_are_still_reclaimable() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("target");
        fs::create_dir_all(dir.join("deps")).unwrap();
        write(&dir.join("deps").join("app-abc123.exe"), 3000);
        fs::hard_link(dir.join("deps").join("app-abc123.exe"), dir.join("app.exe")).unwrap();

        let m = dir_size(&dir, true);

        assert_eq!(m.bytes, 6000, "apparent size sees the file twice");
        assert_eq!(
            m.shared_bytes, 3000,
            "the duplicate counting is removed, but nothing is lost to an outside link"
        );
        assert_eq!(
            m.bytes - m.shared_bytes,
            3000,
            "deleting the directory really does return 3000 bytes"
        );
    }

    #[test]
    fn plain_tree_reports_nothing_shared() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("node_modules");
        write(&dir.join("a.js"), 1000);
        write(&dir.join("b.js"), 2000);

        let m = dir_size(&dir, true);
        assert_eq!(m.bytes, 3000);
        assert_eq!(m.shared_bytes, 0);
    }

    #[test]
    fn skipping_the_link_check_reports_apparent_size_as_reclaimable() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store");
        let dir = tmp.path().join("node_modules");
        write(&store.join("shared.js"), 4000);
        fs::create_dir_all(&dir).unwrap();
        fs::hard_link(store.join("shared.js"), dir.join("shared.js")).unwrap();

        let m = dir_size(&dir, false);
        assert_eq!(m.bytes, 4000);
        assert_eq!(m.shared_bytes, 0, "opted out, so no accounting was done");
    }

    #[test]
    fn finds_project_and_names_its_targets() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        write(&proj.join("node_modules").join("dep.js"), 4096);

        let projects = scan(tmp.path(), &[node_detector()], Options::default());

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

        assert!(scan(tmp.path(), &[node_detector()], Options::default()).is_empty());
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

        let off = Options::default();
        let on = Options {
            aggressive: true,
            ..Default::default()
        };
        assert!(scan(tmp.path(), std::slice::from_ref(&detector), off).is_empty());
        assert_eq!(scan(tmp.path(), &[detector], on).len(), 1);
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

        let m = dir_size(&dir, false);

        // Restore before the assertions so TempDir can always clean up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(m.bytes, 500, "readable part still counted");
        assert!(m.unreadable > 0, "failure must be recorded, not discarded");
        assert!(is_worth_reporting(m));
    }
}
