# ForceFree

**Find, rank and reclaim the disk space your dev projects are quietly eating.**

Every other cleanup tool tells you how much space you can free. ForceFree tells
you what getting it back will *cost* you — and sorts by that, so you delete the
4 GB that takes 45 seconds to restore before the 400 MB that takes ten minutes.

It also refuses to touch a repo that has uncommitted or unpushed work. No flag
overrides that.

```
  ~/dev/dashboard        [Node.js]  4.2 GB  (clean + pushed)
      node_modules                   3.8 GB   restore: npm ci (~45s)
      .next/cache                  412.0 MB   restore: npm run build (~1m 30s)

! ~/dev/thesis-model     [Python]   2.1 GB  (uncommitted changes)
      .venv                          2.1 GB   restore: python -m venv .venv && ... (~1m)

  ~/dev/parser           [Rust]     6.7 GB  (clean + pushed)
      target/debug                   6.7 GB   restore: cargo build (~3m)

─────────────────────────────────────────────
Reclaimable now : 10.9 GB
Cost to restore : ~5m 15s
Held back       : 2.1 GB in repos with uncommitted or unpushed work

Run with --reclaim to delete. Nothing has been touched.
```

## Install

```bash
cargo install forcefree
```

## Use

```bash
forcefree                      # dry run on the current directory
forcefree ~/dev                # dry run somewhere specific
forcefree ~/dev --reclaim      # actually delete, after confirmation
forcefree ~/dev --aggressive   # also consider build outputs (dist/, bin/)
forcefree --list-detectors     # what ecosystems are supported
```

Dry run is the default. You have to ask for deletion, and then confirm it.

## How it decides what's safe

Before touching any project, ForceFree checks its git state:

| State | Behaviour |
|---|---|
| Uncommitted changes | **Skipped.** Nothing here is backed up. |
| Unpushed commits | **Skipped.** A remote doesn't have this yet. |
| Clean and pushed | Eligible — everything is recoverable with `git clone`. |
| Not a repo | Eligible, but only paths a detector explicitly named. |

It only ever deletes directories that a detector named by exact path. No globs,
no guessing.

## Supported ecosystems

Node.js, Rust, Python, Gradle/Android, Go — and growing.

**Yours missing?** Adding one is a ten-minute PR against a single TOML file and
needs no Rust. See [CONTRIBUTING.md](CONTRIBUTING.md).

## How this differs from the alternatives

- **npkill** finds `node_modules`. Excellent at that, and only that.
- **kondo** covers many ecosystems, but is explicitly scoped to project
  directories — it won't touch system or package-manager caches.
- **WizTree / WinDirStat / dust** show you where bytes are. They don't know what
  a `node_modules` *is*, so they can't tell you what's safe or cheap to remove.

ForceFree is aiming at the part none of them cover: restore cost as a
first-class number, git-aware safety, and the caches that live outside your
project folders.

## Roadmap

The things worth building next, roughly in order:

- [ ] Global package caches — `~/.npm`, pnpm store, `~/.cargo/registry`, `~/.gradle`, `~/.m2`, pip, Go modules
- [ ] Docker reclamation — dangling images, stopped containers, build cache, orphaned volumes
- [ ] **WSL2 `ext4.vhdx` compaction** — the virtual disk grows and never shrinks; reclaiming it today means manual `diskpart` + `compact vdisk`
- [ ] Measured rebuild times instead of estimates
- [ ] Whole-repo archival for clean repos untouched for months
- [ ] JSON output for scripting
- [ ] GUI (Tauri) once the CLI is solid

## Building from source

```bash
git clone https://github.com/YousufFFFF/forcefree
cd forcefree
cargo build --release
cargo test
```

MSRV 1.75.

## License

MIT
