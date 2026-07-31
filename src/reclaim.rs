//! Deletion. Deliberately boring and deliberately hard to trigger by accident.
//!
//! Split into a pure [`plan`] and an effectful [`execute`] so the decision about
//! *what* gets deleted can be tested without a filesystem, and the deletion
//! itself can be tested against scratch directories. This module is the only one
//! that can destroy data; it should be the best covered, not the worst.
//!
//! Two gates stand between a scanned target and `remove_dir_all`:
//!
//!   1. [`plan`] drops anything in a repository with work that isn't backed up.
//!   2. [`execute`] re-resolves every path and refuses anything that does not
//!      land inside its own project root. Load-time validation in
//!      [`crate::detector`] stops a detector *naming* an escaping path; this
//!      catches a legitimate-looking path that resolves elsewhere at runtime
//!      through a symlink or junction.

use crate::report::{self, bytes, duration, lower_bound_warning, shared_warning};
use crate::scan::{Project, Target};
use anyhow::Result;
use std::io::{self, Write};
use std::path::Path;

/// One target selected for deletion, kept with the project that owns it so the
/// containment check has something to check against.
#[derive(Debug, Clone, Copy)]
pub struct Doomed<'a> {
    pub project: &'a Project,
    pub target: &'a Target,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Bytes the filesystem actually gave back.
    pub freed: u64,
    pub removed: usize,
    /// Removal was attempted and failed — in use, permissions.
    pub failures: usize,
    /// Removal was not attempted: the path did not resolve inside its project.
    pub refused: usize,
}

/// Decide what to delete. Pure — no filesystem, no output.
///
/// Defaults to what the chart recommended: everything above the break-even line
/// and nothing below it. Drawing a line that says three of six targets are not
/// worth reclaiming and then reclaiming all six makes the advice decorative, and
/// the safer default for a destructive flag is the one that deletes less.
/// `all` takes everything eligible.
///
/// Uses [`crate::report::ranked`] rather than its own filter so the two views
/// cannot drift apart.
pub fn plan(projects: &[Project], worth_rate: f64, all: bool) -> Vec<Doomed<'_>> {
    let (above, below) = report::ranked(projects, worth_rate);
    let chosen = above.into_iter().chain(if all {
        below.into_iter()
    } else {
        Vec::new().into_iter()
    });
    chosen
        .map(|(project, target)| Doomed { project, target })
        .collect()
}

/// Eligible targets the plan deliberately left alone, so the user can be told.
pub fn skipped_below_line(projects: &[Project], worth_rate: f64, all: bool) -> usize {
    if all {
        0
    } else {
        report::ranked(projects, worth_rate).1.len()
    }
}

/// Does `target` really sit inside `project_root`?
///
/// Both sides are canonicalised, which resolves symlinks and junctions — the
/// point is to catch a path that *looks* contained but isn't. Fails closed: if
/// either side cannot be resolved we cannot prove containment, so the answer is
/// no. The root itself never counts as inside itself; deleting a project root is
/// not something any detector should be able to ask for.
fn is_inside_project(project_root: &Path, target: &Path) -> bool {
    match (project_root.canonicalize(), target.canonicalize()) {
        (Ok(root), Ok(t)) => t != root && t.starts_with(&root),
        _ => false,
    }
}

/// Delete. Every target is re-checked immediately before removal.
pub fn execute(doomed: &[Doomed<'_>]) -> Outcome {
    let mut out = Outcome::default();
    for d in doomed {
        let path = &d.target.path;
        if !is_inside_project(&d.project.root, path) {
            out.refused += 1;
            eprintln!(
                "  REFUSED {} — does not resolve inside {}",
                report::display_path(path),
                report::display_path(&d.project.root)
            );
            continue;
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => {
                out.removed += 1;
                // Reclaimable, not apparent: this has to agree with the figure
                // the report showed before the prompt.
                out.freed += d.target.reclaimable_bytes();
                println!(
                    "  removed {} ({})",
                    report::display_path(path),
                    bytes(d.target.reclaimable_bytes())
                );
            }
            Err(e) => {
                out.failures += 1;
                eprintln!("  FAILED {}: {e}", report::display_path(path));
            }
        }
    }
    out
}

pub fn run(projects: &[Project], skip_prompt: bool, worth_rate: f64, all: bool) -> Result<()> {
    let doomed = plan(projects, worth_rate, all);
    let below = skipped_below_line(projects, worth_rate, all);
    let held_back = projects
        .iter()
        .filter(|p| !p.git_state.is_safe_to_reclaim())
        .count();

    if doomed.is_empty() {
        println!("\nNothing eligible to reclaim.");
        if held_back > 0 {
            println!("{held_back} project(s) skipped: uncommitted or unpushed work.");
        }
        return Ok(());
    }

    let total: u64 = doomed.iter().map(|d| d.target.reclaimable_bytes()).sum();
    let secs: u64 = doomed.iter().map(|d| d.target.rebuild_seconds as u64).sum();
    let unreadable: u32 = doomed.iter().map(|d| d.target.unreadable).sum();
    let shared: u64 = doomed.iter().map(|d| d.target.shared_bytes).sum();

    println!(
        "\nAbout to free {}{} across {} target(s).",
        if unreadable > 0 { "at least " } else { "" },
        bytes(total),
        doomed.len()
    );
    println!("Restoring all of it would cost roughly {}.", duration(secs));
    if let Some(w) = shared_warning(shared) {
        println!("{w}");
    }
    if let Some(w) = lower_bound_warning(unreadable) {
        println!("{w}");
    }
    if below > 0 {
        println!(
            "{below} target(s) below the break-even line will be left alone. \
             --all to include them."
        );
    }
    if held_back > 0 {
        println!("{held_back} project(s) will be skipped: uncommitted or unpushed work.");
    }

    if !skip_prompt {
        print!("\nType 'yes' to continue: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            println!("Aborted. Nothing was deleted.");
            return Ok(());
        }
    }

    let out = execute(&doomed);

    println!("\nFreed {}.", bytes(out.freed));
    if out.failures > 0 {
        eprintln!(
            "{} target(s) could not be removed (in use, or permissions).",
            out.failures
        );
    }
    if out.refused > 0 {
        anyhow::bail!(
            "{} target(s) were refused because they did not resolve inside their \
             project. This is a detector bug — please report it.",
            out.refused
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart;
    use crate::git::GitState;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn target(path: PathBuf, bytes: u64, shared: u64) -> Target {
        Target {
            path,
            bytes,
            shared_bytes: shared,
            unreadable: 0,
            restore_command: "npm ci".into(),
            rebuild_seconds: 45,
        }
    }

    fn project(root: PathBuf, state: GitState, targets: Vec<Target>) -> Project {
        Project {
            root,
            ecosystem: "Node.js".into(),
            git_state: state,
            targets,
        }
    }

    /// Every eligible target, break-even ignored. The execute tests are about
    /// what deletion does, not about which rows were selected.
    fn everything(p: &Project) -> Vec<Doomed<'_>> {
        plan(std::slice::from_ref(p), chart::DEFAULT_WORTH_RATE, true)
    }

    /// Builds `<tmp>/app` with a `node_modules` holding one file of `bytes`.
    fn app_with_target(tmp: &TempDir, bytes: usize) -> (PathBuf, PathBuf) {
        let root = tmp.path().join("app");
        let target = root.join("node_modules");
        fs::create_dir_all(&target).unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::write(target.join("dep.js"), vec![b'x'; bytes]).unwrap();
        (root, target)
    }

    /// The trust gate has to hold at the deletion boundary too, not only in the
    /// report. Anything else means one refactor away from deleting a dirty repo.
    #[test]
    fn plan_excludes_git_blocked_projects() {
        let projects = vec![
            project(
                "/clean".into(),
                GitState::Clean,
                vec![target("/clean/node_modules".into(), 10, 0)],
            ),
            project(
                "/dirty".into(),
                GitState::Dirty,
                vec![target("/dirty/node_modules".into(), 10, 0)],
            ),
            project(
                "/unknown".into(),
                GitState::Unknown,
                vec![target("/unknown/node_modules".into(), 10, 0)],
            ),
            project(
                "/unpushed".into(),
                GitState::Unpushed,
                vec![target("/unpushed/node_modules".into(), 10, 0)],
            ),
        ];
        let doomed = plan(&projects, chart::DEFAULT_WORTH_RATE, true);
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].project.root, PathBuf::from("/clean"));
    }

    const MB: u64 = 1_048_576;

    /// A good deal and a bad one in the same project. The default must take the
    /// good one and leave the other, matching the line the chart drew.
    #[test]
    fn plan_stops_at_the_break_even_line() {
        // 900 MB for 45s = 20 MB/s, well above the 3 MB/s reference.
        let good = Target {
            rebuild_seconds: 45,
            ..target("/p/node_modules".into(), 900 * MB, 0)
        };
        // 100 MB for ten minutes = 0.17 MB/s, well below.
        let bad = Target {
            rebuild_seconds: 600,
            ..target("/p/target/release".into(), 100 * MB, 0)
        };
        let projects = vec![project(
            "/p".into(),
            GitState::Clean,
            vec![good.clone(), bad.clone()],
        )];

        let default = plan(&projects, chart::DEFAULT_WORTH_RATE, false);
        assert_eq!(default.len(), 1, "only the good deal by default");
        assert_eq!(default[0].target.path, good.path);
        assert_eq!(
            skipped_below_line(&projects, chart::DEFAULT_WORTH_RATE, false),
            1
        );

        let everything = plan(&projects, chart::DEFAULT_WORTH_RATE, true);
        assert_eq!(everything.len(), 2, "--all takes both");
        assert_eq!(
            skipped_below_line(&projects, chart::DEFAULT_WORTH_RATE, true),
            0
        );
    }

    /// What --reclaim deletes and what the chart drew above the line have to be
    /// the same set, or the tool advises one thing and does another.
    #[test]
    fn plan_matches_what_the_report_ranked_above_the_line() {
        let projects = vec![project(
            "/p".into(),
            GitState::Clean,
            vec![
                Target {
                    rebuild_seconds: 45,
                    ..target("/p/a".into(), 900 * MB, 0)
                },
                Target {
                    rebuild_seconds: 600,
                    ..target("/p/b".into(), 100 * MB, 0)
                },
                Target {
                    rebuild_seconds: 60,
                    ..target("/p/c".into(), 500 * MB, 0)
                },
            ],
        )];
        let (above, _) = report::ranked(&projects, chart::DEFAULT_WORTH_RATE);
        let doomed = plan(&projects, chart::DEFAULT_WORTH_RATE, false);

        let ranked_paths: Vec<_> = above.iter().map(|(_, t)| &t.path).collect();
        let doomed_paths: Vec<_> = doomed.iter().map(|d| &d.target.path).collect();
        assert_eq!(ranked_paths, doomed_paths);
    }

    #[test]
    fn execute_removes_only_the_named_directory() {
        let tmp = TempDir::new().unwrap();
        let (root, nm) = app_with_target(&tmp, 100);
        let p = project(
            root.clone(),
            GitState::Clean,
            vec![target(nm.clone(), 100, 0)],
        );

        let out = execute(&everything(&p));

        assert_eq!(out.removed, 1);
        assert_eq!(out.failures, 0);
        assert_eq!(out.refused, 0);
        assert!(!nm.exists(), "target should be gone");
        assert!(root.join("package.json").exists(), "sibling must survive");
        assert!(root.exists(), "project root must survive");
    }

    /// The report says one number before the prompt; the deletion must not
    /// announce a different one afterwards.
    #[test]
    fn execute_reports_reclaimable_not_apparent_bytes() {
        let tmp = TempDir::new().unwrap();
        let (root, nm) = app_with_target(&tmp, 1000);
        // Apparent 1000, but 900 of it is hard linked elsewhere.
        let p = project(root, GitState::Clean, vec![target(nm, 1000, 900)]);

        let out = execute(&everything(&p));
        assert_eq!(
            out.freed, 100,
            "must report what deletion actually returned"
        );
    }

    /// The shape a malicious or broken detector produces: a target that is not
    /// under the project it claims to belong to.
    #[test]
    fn a_target_outside_its_project_root_is_refused() {
        let tmp = TempDir::new().unwrap();
        let (root, _) = app_with_target(&tmp, 10);
        let elsewhere = tmp.path().join("not_the_project");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("important.txt"), "keep me").unwrap();

        let p = project(
            root,
            GitState::Clean,
            vec![target(elsewhere.clone(), 10, 0)],
        );
        let out = execute(&everything(&p));

        assert_eq!(out.refused, 1);
        assert_eq!(out.removed, 0);
        assert!(
            elsewhere.join("important.txt").exists(),
            "must be untouched"
        );
    }

    #[test]
    fn the_project_root_itself_is_never_deleted() {
        let tmp = TempDir::new().unwrap();
        let (root, _) = app_with_target(&tmp, 10);
        let p = project(
            root.clone(),
            GitState::Clean,
            vec![target(root.clone(), 10, 0)],
        );

        let out = execute(&everything(&p));
        assert_eq!(out.refused, 1);
        assert!(root.exists());
    }

    /// A path we cannot resolve is a path we cannot prove is safe.
    #[test]
    fn an_unresolvable_path_is_refused_rather_than_attempted() {
        let tmp = TempDir::new().unwrap();
        let (root, _) = app_with_target(&tmp, 10);
        let ghost = root.join("does_not_exist");
        let p = project(root, GitState::Clean, vec![target(ghost, 10, 0)]);

        let out = execute(&everything(&p));
        assert_eq!(out.refused, 1);
        assert_eq!(out.removed, 0);
    }

    /// One target failing must not strand the rest half-done.
    #[test]
    fn a_failed_removal_does_not_stop_the_others() {
        let tmp = TempDir::new().unwrap();
        let (root, nm) = app_with_target(&tmp, 100);
        let ghost = root.join("never_existed");
        let p = project(
            root,
            GitState::Clean,
            vec![target(ghost, 50, 0), target(nm.clone(), 100, 0)],
        );

        let out = execute(&everything(&p));

        assert_eq!(out.refused, 1, "the missing one is refused");
        assert_eq!(out.removed, 1, "the real one still goes");
        assert_eq!(out.freed, 100);
        assert!(!nm.exists());
    }

    /// `plan` must be inert. Producing the list is not permission to act on it.
    #[test]
    fn planning_alone_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let (root, nm) = app_with_target(&tmp, 100);
        let p = project(root, GitState::Clean, vec![target(nm.clone(), 100, 0)]);

        let doomed = everything(&p);
        assert_eq!(doomed.len(), 1);
        assert!(nm.exists(), "plan must not touch the filesystem");
    }
}
