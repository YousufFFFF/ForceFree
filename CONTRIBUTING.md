# Contributing to ForceFree

The most useful thing you can contribute is **a detector for an ecosystem we don't support yet**. It takes about ten minutes and requires **no Rust knowledge**.

## Adding a detector (the common case)

Say you want ForceFree to understand Flutter projects.

**1. Create `detectors/flutter.toml`**

```toml
id = "flutter"
name = "Flutter"

# A directory is a Flutter project if ANY of these files exist in it.
markers = ["pubspec.yaml"]

# Each [[reclaimable]] is one directory that can be safely deleted.
[[reclaimable]]
path = "build"
restore_command = "flutter build"
rebuild_seconds = 180

[[reclaimable]]
path = ".dart_tool"
restore_command = "flutter pub get"
rebuild_seconds = 45
```

**2. Register it in `src/detector.rs`** — add one line to the `BUILTIN` list:

```rust
("flutter", include_str!("../detectors/flutter.toml")),
```

**3. Run `cargo test`.** That's it. Open the PR.

### Getting the fields right

**`markers`** — files that identify the project, at its root. Prefer the manifest
(`pubspec.yaml`, `go.mod`, `Cargo.toml`) over the artifact directory. If markers
are too loose you'll match directories that aren't projects.

**`path`** — relative to the project root. Must be a directory. Nested paths like
`.next/cache` are fine.

**`rebuild_seconds`** — a rough estimate of wall-clock time to regenerate on a
normal machine with a normal connection. **This is the field people get wrong,
and it matters more than you'd think.** ForceFree ranks targets by bytes
reclaimed per second of rebuild time, so a bad estimate tells everyone to delete
the wrong thing first. Order of magnitude is what counts:

| Feels like | Use roughly |
|---|---|
| Regenerated automatically, no user action | `2` |
| A quick dependency fetch | `30–60` |
| A full build of a small project | `120–300` |
| A cold Rust/C++/Gradle build | `600+` |

Time it if you can. Guessing conservatively (higher) is safer than guessing low.

**`aggressive_only = true`** — set this when a path *might* contain something the
user cares about, or when losing it is genuinely annoying. These are skipped
unless the user passes `--aggressive`. When you're unsure, set it.

### What not to add

- Anything outside the project root. Global caches (`~/.npm`, `~/.cargo/registry`)
  and Docker are handled separately — see the open issues, they need real code.
- Source directories, config files, lockfiles, `.env` files. Ever.
- Paths whose contents aren't reproducible from a command. If it can't be
  restored, it isn't reclaimable.

## Working on the Rust

The codebase is small and each file has one job:

| File | Responsibility |
|---|---|
| `src/detector.rs` | Detector model, loading, validation |
| `src/scan.rs` | Parallel walk, project recognition, sizing |
| `src/git.rs` | Safety checks — is this work backed up? |
| `src/report.rs` | Output formatting and cost ranking |
| `src/reclaim.rs` | Deletion, confirmation |

```bash
cargo build
cargo test
cargo run -- ~/some/dev/folder      # dry run, always safe
```

**MSRV is 1.75**, declared in `Cargo.toml` and enforced by a CI job, and the
committed `Cargo.lock` is resolved to match. It is kept at lockfile format v3 on
purpose — Cargo 1.75 cannot read v4.

A plain `cargo update` on a modern toolchain will pull in crates that need a much
newer compiler and quietly break the MSRV job. Use:

```bash
cargo update --config "resolver.incompatible-rust-versions='fallback'"
```

Cargo 1.75's own resolver is not MSRV-aware, so `cargo generate-lockfile` under
1.75 does not work either — it happily selects crates that need edition 2024.

## Non-negotiables

These are the reasons people are willing to run a deletion tool on their dev
drive. Please don't file PRs that weaken them:

1. **Dry run is the default.** Deleting requires `--reclaim`.
2. **Never delete inside a repo with uncommitted or unpushed work.** There is no
   flag to override this and there will not be one.
3. **Never delete anything a detector didn't explicitly name.** No globs, no
   heuristics, no "this looks like a cache".
4. **Always show what restoring costs**, not just what's freed.

## Before you open a PR

- `cargo test` passes
- `cargo fmt` and `cargo clippy` are clean
- If you added a detector, you actually ran ForceFree against a real project of
  that kind and it found the right directories

Small PRs get reviewed fast. A detector PR should be merged within a day or two.
