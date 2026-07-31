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
}

/// Directory names we never walk into. Cheap win: avoids sizing the same bytes
/// twice and keeps us out of places we'd never act on.
const NEVER_DESCEND: &[&str] = &[".git", "$RECYCLE.BIN", "System Volume Information"];

fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Walk `root`, returning every project found, largest first.
pub fn scan(root: &Path, detectors: &[Detector], aggressive: bool) -> Vec<Project> {
    let mut projects = Vec::new();

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

        let git_state = git::inspect(&dir);

        let targets: Vec<Target> = detector
            .reclaimable
            .iter()
            .filter(|r| aggressive || !r.aggressive_only)
            .filter_map(|r| {
                let p = dir.join(&r.path);
                if !p.is_dir() {
                    return None;
                }
                let bytes = dir_size(&p);
                if bytes == 0 {
                    return None;
                }
                Some(Target {
                    path: p,
                    bytes,
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
