//! Detectors describe *one ecosystem* — how to recognise a project and what is
//! safe to reclaim inside it.
//!
//! Detectors are plain TOML data, never Rust code. Adding support for a new
//! ecosystem means adding one file to `detectors/` and one test case. No Rust
//! knowledge required. This is deliberate: it is the main way people contribute.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Component, Path};

/// A single reclaimable target inside a recognised project.
#[derive(Debug, Clone, Deserialize)]
pub struct Reclaimable {
    /// Path relative to the project root, e.g. "node_modules" or ".next/cache".
    pub path: String,

    /// The command a user would run to regenerate this, e.g. "npm ci".
    /// Shown to the user; never executed by ForceFree.
    pub restore_command: String,

    /// Rough wall-clock seconds to regenerate on a typical machine with a warm
    /// network. Used to rank targets by "cheapest to get back", so the user
    /// deletes 4 GB of `node_modules` before 400 MB of Rust `target/`.
    ///
    /// Precision is not the point. Order of magnitude is.
    pub rebuild_seconds: u32,

    /// If true, this target is skipped unless the user passes `--aggressive`.
    /// Use for anything with real risk of being expensive or surprising to lose.
    #[serde(default)]
    pub aggressive_only: bool,
}

/// One ecosystem: Node, Rust, Python, Gradle, ...
#[derive(Debug, Clone, Deserialize)]
pub struct Detector {
    /// Stable machine id, matches the filename stem. e.g. "node".
    pub id: String,

    /// Human label shown in output. e.g. "Node.js".
    pub name: String,

    /// A directory is a project of this kind if ANY marker exists at its root.
    pub markers: Vec<String>,

    /// What can be deleted, and what it costs to restore.
    pub reclaimable: Vec<Reclaimable>,
}

impl Detector {
    /// Does this directory look like a project of this ecosystem?
    ///
    /// The scan no longer calls this: asking the filesystem once per marker per
    /// detector cost a dozen `stat` calls for every directory on the disk, and
    /// `scan::MarkerIndex` answers the same question from the names `read_dir`
    /// already returned. Kept as the obvious-and-slow reference the fast path is
    /// tested against.
    #[cfg(test)]
    pub fn matches(&self, dir: &Path) -> bool {
        self.markers.iter().any(|m| dir.join(m).exists())
    }
}

/// Detectors bundled into the binary at compile time.
///
/// Contributors: add your `detectors/<id>.toml` file, then add one line here.
/// That is the whole change.
const BUILTIN: &[(&str, &str)] = &[
    ("node", include_str!("../detectors/node.toml")),
    ("rust", include_str!("../detectors/rust.toml")),
    ("python", include_str!("../detectors/python.toml")),
    ("gradle", include_str!("../detectors/gradle.toml")),
    ("go", include_str!("../detectors/go.toml")),
    ("flutter", include_str!("../detectors/flutter.toml")),
];

/// Parse every built-in detector. Fails loudly on a malformed file so a bad
/// contribution breaks CI rather than shipping.
pub fn load_builtin() -> Result<Vec<Detector>> {
    BUILTIN
        .iter()
        .map(|(id, raw)| {
            let d: Detector = toml::from_str(raw)
                .with_context(|| format!("detector '{id}' is not valid TOML"))?;
            anyhow::ensure!(
                d.id == *id,
                "detector '{id}': `id` field is '{}', must match the filename",
                d.id
            );
            anyhow::ensure!(
                !d.markers.is_empty(),
                "detector '{id}': needs at least one marker"
            );
            anyhow::ensure!(
                !d.reclaimable.is_empty(),
                "detector '{id}': needs at least one reclaimable target"
            );
            validate_paths(&d)?;
            Ok(d)
        })
        .collect()
}

/// Enforce, in code, that a detector can only name things inside a project.
///
/// Invariant 3 says ForceFree never deletes anything a detector did not name by
/// exact path. Until this existed that was enforced by code review: `scan.rs`
/// joins `path` onto the project root and `reclaim.rs` hands the result to
/// `remove_dir_all`, so `path = "../.."` in a contributed TOML would have
/// escaped the project entirely. Detectors are the surface strangers contribute
/// through, which makes review the wrong place for the check.
fn validate_paths(d: &Detector) -> Result<()> {
    let id = &d.id;
    for r in &d.reclaimable {
        let p = &r.path;
        anyhow::ensure!(
            !p.trim().is_empty(),
            "detector '{id}': empty reclaimable path"
        );
        // Backslashes before anything else, because what a path *means* is
        // otherwise platform-dependent and detectors are shared across all
        // three. `C:\Windows` is an absolute path on Windows and a single
        // oddly-named file on Linux; rejecting the separator outright makes
        // every check below give the same answer everywhere. Rust accepts `/`
        // on Windows, so nothing legitimate needs a backslash.
        anyhow::ensure!(
            !p.contains('\\'),
            "detector '{id}': path '{p}' uses backslashes; separate with '/' on every platform"
        );
        anyhow::ensure!(
            !Path::new(p).is_absolute(),
            "detector '{id}': path '{p}' is absolute; paths are relative to the project root"
        );
        // Component-wise rather than a substring search for "..": this also
        // rejects roots and Windows prefixes like `C:` and `\\server\share`,
        // which a string check would wave through.
        for c in Path::new(p).components() {
            anyhow::ensure!(
                matches!(c, Component::Normal(_)),
                "detector '{id}': path '{p}' must stay inside the project — \
                 no '..', no root, no drive prefix"
            );
        }
        anyhow::ensure!(
            !r.restore_command.trim().is_empty(),
            "detector '{id}': path '{p}' has no restore_command; if it cannot be \
             regenerated by a stated command it is not reclaimable"
        );
    }

    // Overlapping targets double-count their bytes, and deleting the outer one
    // makes the inner one fail. Compared component-wise so `build` catches
    // `build/cache` but not `buildkit`.
    for (i, a) in d.reclaimable.iter().enumerate() {
        for b in &d.reclaimable[i + 1..] {
            let (pa, pb) = (Path::new(&a.path), Path::new(&b.path));
            anyhow::ensure!(
                !pa.starts_with(pb) && !pb.starts_with(pa),
                "detector '{id}': paths '{}' and '{}' overlap; one contains the other",
                a.path,
                b.path
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_detector_parses() {
        let detectors = load_builtin().expect("all built-in detectors must parse");
        assert_eq!(detectors.len(), BUILTIN.len());
    }

    #[test]
    fn detector_ids_are_unique() {
        let detectors = load_builtin().unwrap();
        let mut ids: Vec<_> = detectors.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate detector id");
    }

    /// Hostile TOML goes through exactly the path a contributed file would.
    fn parse(toml_src: &str) -> Result<Detector> {
        let d: Detector = toml::from_str(toml_src)?;
        validate_paths(&d)?;
        Ok(d)
    }

    fn detector_with(path: &str, restore: &str) -> String {
        format!(
            "id = \"x\"\nname = \"X\"\nmarkers = [\"m\"]\n\n\
             [[reclaimable]]\npath = \"{path}\"\n\
             restore_command = \"{restore}\"\nrebuild_seconds = 10\n"
        )
    }

    /// Invariant 3, enforced rather than reviewed. Each of these would have
    /// handed `remove_dir_all` a path outside the project.
    #[test]
    fn paths_cannot_escape_the_project_root() {
        for bad in ["../..", "../../etc", "a/../../b", "/etc", "C:\\\\Windows"] {
            let err = parse(&detector_with(bad, "cmd")).unwrap_err().to_string();
            assert!(
                err.contains("absolute")
                    || err.contains("stay inside")
                    || err.contains("backslash"),
                "wrong error for {bad:?}: {err}"
            );
        }
    }

    /// The same TOML has to mean the same thing on every platform. `C:\Windows`
    /// is an absolute path on Windows and a legal filename on Linux, so the
    /// separator is rejected rather than interpreted — otherwise this test
    /// passes on one CI runner and fails on the others, which is how it was
    /// found.
    #[test]
    fn backslash_separators_are_rejected_identically_everywhere() {
        let err = parse(&detector_with("target\\\\debug", "cargo build"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("backslash"), "{err}");
        // The portable spelling is fine.
        assert!(parse(&detector_with("target/debug", "cargo build")).is_ok());
    }

    #[test]
    fn a_path_must_actually_be_something() {
        assert!(parse(&detector_with("", "cmd")).is_err());
        assert!(parse(&detector_with("   ", "cmd")).is_err());
    }

    /// Invariant 5: if it cannot be regenerated by a stated command, it is not
    /// reclaimable.
    #[test]
    fn a_target_without_a_restore_command_is_rejected() {
        let err = parse(&detector_with("build", ""))
            .expect_err("empty restore_command must be rejected")
            .to_string();
        assert!(err.contains("restore_command"), "{err}");
    }

    /// `build` and `build/cache` double-count their bytes, and removing the
    /// outer one makes the inner removal fail.
    #[test]
    fn overlapping_targets_are_rejected() {
        let src = "id = \"x\"\nname = \"X\"\nmarkers = [\"m\"]\n\n\
                   [[reclaimable]]\npath = \"build\"\nrestore_command = \"c\"\nrebuild_seconds = 10\n\n\
                   [[reclaimable]]\npath = \"build/cache\"\nrestore_command = \"c\"\nrebuild_seconds = 10\n";
        let err = parse(src)
            .expect_err("overlap must be rejected")
            .to_string();
        assert!(err.contains("overlap"), "{err}");
    }

    /// A shared prefix is not containment: `build` must not reject `buildkit`.
    #[test]
    fn merely_similar_names_are_allowed() {
        let src = "id = \"x\"\nname = \"X\"\nmarkers = [\"m\"]\n\n\
                   [[reclaimable]]\npath = \"build\"\nrestore_command = \"c\"\nrebuild_seconds = 10\n\n\
                   [[reclaimable]]\npath = \"buildkit\"\nrestore_command = \"c\"\nrebuild_seconds = 10\n";
        assert!(parse(src).is_ok());
    }

    #[test]
    fn ordinary_nested_paths_are_still_fine() {
        assert!(parse(&detector_with(".next/cache", "npm run build")).is_ok());
        assert!(parse(&detector_with("target/debug", "cargo build")).is_ok());
    }

    #[test]
    fn rebuild_estimates_are_plausible() {
        // Guards against a contributor pasting 0 or a wild value, which would
        // wreck the cost ranking for everyone.
        for d in load_builtin().unwrap() {
            for r in &d.reclaimable {
                assert!(
                    (1..=7200).contains(&r.rebuild_seconds),
                    "{}/{}: rebuild_seconds={} is out of range",
                    d.id,
                    r.path,
                    r.rebuild_seconds
                );
            }
        }
    }
}
