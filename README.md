# ForceFree

**Find, rank and reclaim the disk space your dev projects are quietly eating.**

Every other cleanup tool tells you how much space you can free. ForceFree tells
you what getting it back will *cost* you — and sorts by that, so you delete the
4 GB that takes 45 seconds to restore before the 400 MB that takes ten minutes.

It also refuses to touch a repo that has uncommitted or unpushed work. No flag
overrides that.

Every reclaimable directory is one row on a balance. Space you'd get back grows
left of the spine; time you'd spend rebuilding grows right. **You read the lean,
not the numbers** — and the line through the middle is where getting space back
stops being worth the wait.

```
  13.5 GB reclaimable across 6 targets · 21m 15s to rebuild it all

       space freed │ time to rebuild
      ████████████│██                dashboard/node_modules      3.7 GB     45s
                                     npm ci
  ████████████████│█████             parser/target/debug         6.5 GB      3m
                                     cargo build
         █████████│███               thesis/.venv                2.1 GB      1m
                                     python -m venv .venv && pip install -r r…
  ────────────────┼────────────────  break-even · 3 MB per second
                ██│███               dashboard/.next/cache     120.0 MB  1m 30s
                                     npm run build
            ██████│████████          parser/target/release     800.0 MB     10m
                                     cargo build --release
               ███│██████            mobile/.gradle            300.0 MB      5m
                                     ./gradlew build

  Held back — 2.1 GB in repos with work that isn't backed up
    thesis-model               Python           uncommitted changes

  Run with --reclaim to delete. Nothing has been touched.
```

The three rows above the line give back 12.3 GB for about five minutes. The three
below give back 1.2 GB for another sixteen. That asymmetry is the whole product.

## Install

Not on crates.io yet — build it from source. Rust 1.75 or newer:

```bash
git clone https://github.com/YousufFFFF/ForceFree
cd ForceFree
cargo build --release
```

The binary lands in `target/release/forcefree`. A `cargo install forcefree`
release will follow once the reclaim path has the test coverage a deletion tool
ought to have.

## Use

```bash
forcefree                      # dry run on the current directory
forcefree ~/dev                # dry run somewhere specific
forcefree ~/dev --reclaim      # actually delete, after confirmation
forcefree ~/dev --aggressive   # also consider build outputs (dist/, bin/)
forcefree ~/dev --all          # every target, not just those above the line
forcefree ~/dev --worth 10     # move the break-even line (default 3 MB/s)
forcefree --list-detectors     # what ecosystems are supported
```

Dry run is the default. You have to ask for deletion, and then confirm it — and
the report is printed before the prompt, so you always see the list you're
agreeing to.

`--worth` sets what counts as a fair trade, in megabytes returned per second of
rebuild. Rows better than that lean left; rows worse lean right. The default of
3 MB/s is calibrated so that a `node_modules` you can restore in under a minute
comes out ahead and a ten-minute release build does not.

### Sizes are what you'd actually get back

pnpm and friends keep one copy of a package in a global store and hard link it
into every project. Deleting `node_modules` then frees almost nothing — the
bytes stay alive in the store. ForceFree counts links, and reports what deletion
would really return rather than what `du` would say. Where they differ, it says
so. `--no-link-check` skips the accounting if you'd rather have the speed.

## How it decides what's safe

Before touching any project, ForceFree checks the state of the repository that
*contains* it — not just the project directory. A project at
`repo/services/api` is gated on `repo`, so uncommitted work anywhere in the
repository protects everything inside it.

| State | Behaviour |
|---|---|
| Uncommitted changes | **Skipped.** Nothing here is backed up. |
| Unpushed commits | **Skipped.** A remote doesn't have this yet. |
| Repository unreadable | **Skipped.** Git is missing, or the repo is corrupt, or another process holds a lock. If we can't tell, we don't touch it. |
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
git clone https://github.com/YousufFFFF/ForceFree
cd ForceFree
cargo build --release
cargo test
```

MSRV 1.75.

## License

MIT
