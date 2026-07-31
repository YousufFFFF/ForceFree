//! Detectors describe *one ecosystem* — how to recognise a project and what is
//! safe to reclaim inside it.
//!
//! Detectors are plain TOML data, never Rust code. Adding support for a new
//! ecosystem means adding one file to `detectors/` and one test case. No Rust
//! knowledge required. This is deliberate: it is the main way people contribute.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

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
            Ok(d)
        })
        .collect()
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
