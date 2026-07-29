# Release Notes

Dated entries, newest first. One entry per merged pull request.

## 2026-07-29 — Wave 0: buildable workspace and CI

### Fixed
- **The workspace now builds.** Every crate declared `rusty_*` dependencies
  pointing at `../Rusty_Mill/...` paths that do not exist, so `cargo check`
  failed at manifest load before compiling anything. No source file referenced
  any of those crates, so the declarations were removed rather than repointed.
  Re-adopting a Rusty Mill crate should add it as a git dependency at the point
  it gains a real call site.
- `cargo clippy -- -D warnings` is clean: replaced two hand-written `Default`
  impls in `models.rs` with `#[derive(Default)]` + `#[default]`, and one
  `map_or(false, ...)` in the CLI with `is_some_and`.
- `cargo fmt --all --check` is clean; the workspace had never been formatted.

### Added
- CI workflow (`.github/workflows/ci.yml`) running fmt, build, test, and
  clippy on pushes and pull requests against `main`.
- `.gitignore` covering `target/`, local SQLite databases, and editor files.
- `gap-analysis.md` — parity assessment against `remind_me` v1.19.0.

### Removed
- Untracked 6,260 build artifacts under `target/` and the scratch
  `remind_me.db` that had been committed to the repository. Both remain on
  disk; they are now ignored rather than versioned.
