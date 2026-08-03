//! Scanning: walk the trees, recognise projects, size what's reclaimable.
//!
//! Three rules keep this fast enough to point at a whole drive:
//!
//!   1. **Projects are recognised from directory entries, never from `stat`.**
//!      `read_dir` has already returned every child name by the time
//!      `process_read_dir` runs, so matching markers is a few hash lookups
//!      against [`MarkerIndex`] and no syscalls at all. Asking the filesystem
//!      "does `package.json` exist here?" once per marker per detector was a
//!      dozen `stat` calls for every directory on the disk.
//!
//!   2. **A project's reclaimable directories are pruned from discovery.**
//!      `node_modules`, `target`, `.venv` and friends hold the overwhelming
//!      majority of files, and there is nothing to *find* inside them — they get
//!      walked exactly once, later, by `dir_size`. Everything else under a
//!      project keeps being walked, so `apps/`, `packages/` and `services/`
//!      still yield the projects nested inside them.
//!
//!   3. **Sizing happens off the walker threads.** Recognised roots go down a
//!      channel; a single consumer resolves git state and measures. Git state in
//!      particular is a subprocess and a cache, and neither belongs on eight
//!      threads at once.
//!
//! Results are streamed as [`Event`]s rather than returned in one lump, because
//! a scan of a whole disk takes long enough that a caller needs to show progress
//! and partial results. [`scan`] is a thin collecting wrapper for callers that
//! genuinely want everything at the end.

use crate::detector::Detector;
use crate::git::{self, GitState};
use crate::links;
use crossbeam_channel::{unbounded, Sender};
use jwalk::WalkDir;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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

/// Marker filename to detector index, plus the directory names each detector
/// wants pruned once it has matched.
///
/// Built once per scan so the walk never has to ask the filesystem whether a
/// marker exists — it already has the names.
struct MarkerIndex {
    /// e.g. "package.json" -> 0. On a directory carrying several markers the
    /// lowest index wins, which preserves the first-detector-wins ordering the
    /// list in `detector.rs` implies.
    by_marker: HashMap<String, usize>,
    /// Per detector, the first path component of each reclaimable target:
    /// `target/debug` and `target/release` both contribute `target`.
    prune: Vec<HashSet<String>>,
}

impl MarkerIndex {
    fn build(detectors: &[Detector]) -> Self {
        let mut by_marker: HashMap<String, usize> = HashMap::new();
        let mut prune = Vec::with_capacity(detectors.len());

        for (i, d) in detectors.iter().enumerate() {
            for m in &d.markers {
                // Keep the lowest index so ordering is stable and matches the
                // old `detectors.iter().find(...)`.
                by_marker.entry(m.clone()).or_insert(i);
            }
            prune.push(
                d.reclaimable
                    .iter()
                    .filter_map(|r| {
                        Path::new(&r.path)
                            .components()
                            .next()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    })
                    .collect(),
            );
        }
        Self { by_marker, prune }
    }

    /// Which detector, if any, claims a directory containing these names?
    ///
    /// The walk does this inline over borrowed names to avoid allocating; this
    /// is the same rule, spelled out for the tests.
    #[cfg(test)]
    fn detector_for<'n>(&self, names: impl Iterator<Item = &'n str>) -> Option<usize> {
        names.filter_map(|n| self.by_marker.get(n)).min().copied()
    }
}

/// Counts of what the walk has been through, for progress reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub dirs: u64,
    pub files: u64,
}

/// Streamed as the scan proceeds.
#[derive(Debug)]
pub enum Event {
    /// A project root was recognised. Nothing has been measured yet, so this
    /// arrives long before the sizes do — it is what lets a caller show
    /// something immediately.
    Found {
        root: PathBuf,
        ecosystem: String,
    },
    /// Measured and ready to report. Only emitted for projects that turned out
    /// to have something reclaimable in them.
    Project(Project),
    Progress(Progress),
}

/// Drop roots that sit inside another root, so overlapping arguments cannot
/// report the same project twice.
fn independent_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
        .collect();
    canonical.sort();
    canonical.dedup();

    let mut kept: Vec<PathBuf> = Vec::new();
    for r in canonical {
        // Sorted order puts a parent before its children, so checking against
        // what we have already kept is enough.
        if kept.iter().any(|k| r.starts_with(k)) {
            continue;
        }
        kept.push(r);
    }
    kept
}

/// Walk `roots`, streaming projects as they are recognised and measured.
///
/// Ranking is deliberately not done here — see `report::ranked`.
pub fn scan_with(
    roots: &[PathBuf],
    detectors: &[Detector],
    opts: Options,
    on: &mut dyn FnMut(Event),
) {
    let index = Arc::new(MarkerIndex::build(detectors));
    let dirs = Arc::new(AtomicU64::new(0));
    let files = Arc::new(AtomicU64::new(0));
    let mut repos = git::RepoCache::default();

    for root in independent_roots(roots) {
        // Discovery runs on jwalk's thread pool and only ever sends paths; the
        // expensive work happens on this thread as they arrive.
        let (tx, rx) = unbounded::<(PathBuf, usize)>();
        let walker = {
            let index = Arc::clone(&index);
            let dirs = Arc::clone(&dirs);
            let files = Arc::clone(&files);
            let tx: Sender<(PathBuf, usize)> = tx;
            WalkDir::new(&root)
                .skip_hidden(false)
                .process_read_dir(move |_, path, _, children| {
                    // One pass, no allocations: `to_string_lossy` borrows for
                    // the UTF-8 names that make up essentially every real path,
                    // and building a Vec<String> here meant one heap allocation
                    // per directory entry on the disk.
                    let (mut d, mut f) = (0u64, 0u64);
                    let mut matched: Option<usize> = None;
                    for c in children.iter().flatten() {
                        if c.file_type().is_dir() {
                            d += 1;
                        } else {
                            f += 1;
                        }
                        if let Some(&i) = index
                            .by_marker
                            .get(c.file_name().to_string_lossy().as_ref())
                        {
                            // Lowest index wins, preserving first-detector-wins.
                            matched = Some(matched.map_or(i, |m| m.min(i)));
                        }
                    }
                    dirs.fetch_add(d, Ordering::Relaxed);
                    files.fetch_add(f, Ordering::Relaxed);

                    if let Some(i) = matched {
                        // Ignore send errors: the receiver is only dropped when
                        // the walk is being torn down anyway.
                        let _ = tx.send((path.to_path_buf(), i));
                    }

                    // Prune what we will never need to look *inside*: the
                    // never-descend list always, plus this project's own
                    // reclaimable directories, which `dir_size` will walk later
                    // and which cannot contain projects worth reporting.
                    let prune = matched.map(|i| &index.prune[i]);
                    children.retain(|c| {
                        c.as_ref()
                            .map(|e| {
                                let name = e.file_name().to_string_lossy();
                                if NEVER_DESCEND.iter().any(|s| *s == name) {
                                    return false;
                                }
                                !prune.is_some_and(|p| p.contains(name.as_ref()))
                            })
                            .unwrap_or(false)
                    });
                })
        };

        // Drain the walk. `into_iter` drives it; the entries themselves are not
        // needed because recognition already happened in the closure above.
        let walk = std::thread::scope(|s| {
            let handle = s.spawn(|| walker.into_iter().for_each(drop));
            let mut found = Vec::new();
            for msg in rx.iter() {
                found.push(msg);
            }
            handle.join().ok();
            found
        });

        for (dir, detector_idx) in walk {
            let detector = &detectors[detector_idx];
            on(Event::Found {
                root: dir.clone(),
                ecosystem: detector.name.clone(),
            });

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

            on(Event::Progress(Progress {
                dirs: dirs.load(Ordering::Relaxed),
                files: files.load(Ordering::Relaxed),
            }));

            if targets.is_empty() {
                continue;
            }

            on(Event::Project(Project {
                root: dir,
                ecosystem: detector.name.clone(),
                git_state,
                targets,
            }));
        }
    }

    on(Event::Progress(Progress {
        dirs: dirs.load(Ordering::Relaxed),
        files: files.load(Ordering::Relaxed),
    }));
}

/// Every project under `roots`, in discovery order. Collecting wrapper over
/// [`scan_with`] for callers that want the whole answer at once.
pub fn scan(roots: &[PathBuf], detectors: &[Detector], opts: Options) -> Vec<Project> {
    let mut out = Vec::new();
    scan_with(roots, detectors, opts, &mut |e| {
        if let Event::Project(p) = e {
            out.push(p);
        }
    });
    out
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

    fn python_detector() -> Detector {
        Detector {
            id: "python".into(),
            name: "Python".into(),
            markers: vec!["requirements.txt".into()],
            reclaimable: vec![Reclaimable {
                path: ".venv".into(),
                restore_command: "python -m venv .venv".into(),
                rebuild_seconds: 60,
                aggressive_only: false,
            }],
        }
    }

    /// Roots are canonicalised by the scan, which on Windows means discovered
    /// paths carry the `\\?\` extended-length prefix. Expected paths have to go
    /// through the same normalisation to be comparable.
    fn canon(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    /// Collect every event, not just the projects, so the walk itself can be
    /// asserted on.
    fn events(roots: &[PathBuf], detectors: &[Detector]) -> Vec<Event> {
        let mut out = Vec::new();
        scan_with(roots, detectors, Options::default(), &mut |e| out.push(e));
        out
    }

    fn last_progress(events: &[Event]) -> Progress {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Progress(p) => Some(*p),
                _ => None,
            })
            .next_back()
            .expect("a scan always reports progress")
    }

    /// The regression guard for the double walk. Discovery used to descend
    /// through every file of every `node_modules` and then `dir_size` walked
    /// them all again. Sizing still visits them; the *discovery* walk must not.
    #[test]
    fn discovery_does_not_descend_into_reclaimable_directories() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        for i in 0..200 {
            write(&proj.join("node_modules").join(format!("dep{i}.js")), 10);
        }
        // A handful of files the walk *should* see.
        write(&proj.join("src").join("index.js"), 10);

        let seen = last_progress(&events(&[tmp.path().to_path_buf()], &[node_detector()]));
        assert!(
            seen.files < 20,
            "discovery walked {} files; it should have pruned node_modules",
            seen.files
        );
    }

    /// A repo with a Python service under a Node root used to report only the
    /// Node root, because everything below a found project was skipped.
    #[test]
    fn a_nested_project_is_found() {
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path().join("repo");
        fs::create_dir_all(&outer).unwrap();
        fs::write(outer.join("package.json"), "{}").unwrap();
        write(&outer.join("node_modules").join("dep.js"), 100);

        let inner = outer.join("services").join("api");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("requirements.txt"), "flask").unwrap();
        write(&inner.join(".venv").join("lib.py"), 500);

        let found = scan(
            &[tmp.path().to_path_buf()],
            &[node_detector(), python_detector()],
            Options::default(),
        );

        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found.iter().any(|p| p.root == canon(&outer)));
        assert!(found.iter().any(|p| p.root == canon(&inner)));
    }

    /// Packages inside `node_modules` carry a `package.json` each. They are not
    /// the user's projects and must never be offered as such.
    #[test]
    fn a_package_inside_node_modules_is_not_a_project() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        let dep = proj.join("node_modules").join("left-pad");
        fs::create_dir_all(&dep).unwrap();
        fs::write(dep.join("package.json"), "{}").unwrap();
        write(&dep.join("index.js"), 100);

        let found = scan(
            &[tmp.path().to_path_buf()],
            &[node_detector()],
            Options::default(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].root, canon(&proj));
    }

    /// The fast path and the obvious-but-slow one must classify identically.
    #[test]
    fn dirent_detection_agrees_with_marker_matching() {
        let detectors = [node_detector(), python_detector()];
        let index = MarkerIndex::build(&detectors);
        let tmp = TempDir::new().unwrap();

        let cases: [(&str, &[&str], Option<usize>); 4] = [
            ("node", &["package.json", "src"], Some(0)),
            ("py", &["requirements.txt"], Some(1)),
            ("both", &["package.json", "requirements.txt"], Some(0)),
            ("neither", &["README.md"], None),
        ];

        for (dir, files, expected) in cases {
            let d = tmp.path().join(dir);
            fs::create_dir_all(&d).unwrap();
            for f in files {
                fs::write(d.join(f), "").unwrap();
            }
            assert_eq!(index.detector_for(files.iter().copied()), expected, "{dir}");

            // And the reference implementation must agree about *whether* it matched.
            let by_stat = detectors.iter().position(|det| det.matches(&d));
            assert_eq!(by_stat, expected, "{dir}: stat-based disagreed");
        }
    }

    #[test]
    fn multiple_roots_are_all_scanned() {
        let tmp = TempDir::new().unwrap();
        for name in ["one", "two"] {
            let p = tmp.path().join(name);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("package.json"), "{}").unwrap();
            write(&p.join("node_modules").join("dep.js"), 100);
        }
        let found = scan(
            &[tmp.path().join("one"), tmp.path().join("two")],
            &[node_detector()],
            Options::default(),
        );
        assert_eq!(found.len(), 2);
    }

    /// Passing a directory and something inside it must not report the inner
    /// project twice.
    #[test]
    fn overlapping_roots_do_not_double_report() {
        let tmp = TempDir::new().unwrap();
        let inner = tmp.path().join("app");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("package.json"), "{}").unwrap();
        write(&inner.join("node_modules").join("dep.js"), 100);

        let found = scan(
            &[tmp.path().to_path_buf(), inner.clone()],
            &[node_detector()],
            Options::default(),
        );
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn every_project_is_emitted_exactly_once() {
        let tmp = TempDir::new().unwrap();
        for i in 0..12 {
            let p = tmp.path().join(format!("p{i}"));
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("package.json"), "{}").unwrap();
            write(&p.join("node_modules").join("dep.js"), 100);
        }
        let found = scan(
            &[tmp.path().to_path_buf()],
            &[node_detector()],
            Options::default(),
        );
        let mut roots: Vec<_> = found.iter().map(|p| p.root.clone()).collect();
        roots.sort();
        let before = roots.len();
        roots.dedup();
        assert_eq!(before, 12);
        assert_eq!(roots.len(), 12, "a project was emitted more than once");
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

        let projects = scan(
            &[tmp.path().to_path_buf()],
            &[node_detector()],
            Options::default(),
        );

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

        assert!(scan(
            &[tmp.path().to_path_buf()],
            &[node_detector()],
            Options::default()
        )
        .is_empty());
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
        assert!(scan(
            &[tmp.path().to_path_buf()],
            std::slice::from_ref(&detector),
            off
        )
        .is_empty());
        assert_eq!(scan(&[tmp.path().to_path_buf()], &[detector], on).len(), 1);
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
