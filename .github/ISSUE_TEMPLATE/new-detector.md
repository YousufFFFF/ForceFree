---
name: Add support for an ecosystem
about: Request or claim a new detector. Good first issue.
title: 'Detector: <ecosystem name>'
labels: 'good first issue, detector'
---

**Ecosystem:** <!-- e.g. Flutter, Unity, Xcode, Composer, Bundler, Deno, Nix -->

**Marker files** — what identifies a project of this kind?
<!-- e.g. pubspec.yaml -->

**Reclaimable directories** — for each one: the path, the command that restores
it, and roughly how long that takes.

| Path | Restore command | Rebuild time |
|---|---|---|
|  |  |  |

**Anything risky?** Any of these that might hold something a user cares about?

---

Adding a detector is one TOML file plus one line of Rust. No Rust knowledge
needed — see [CONTRIBUTING.md](../../CONTRIBUTING.md). Comment to claim this.
