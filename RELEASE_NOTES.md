# Release Notes

Dated entries, newest first. One entry per merged pull request.

## 2026-07-29 — Deeper remind_me_stats, one shared implementation (#5)

### Added
- `remind_me_stats` now reports per-category and per-source counts, the import
  ledger total, the five most recent memories with 80-character previews, and
  the database path and size — matching the reference's payload field for field.
  `total_memories` keeps its meaning, so existing consumers are unaffected.

### Fixed
- **Statistics were computed in four places**, not the three the issue listed:
  the MCP tool, the `memory://stats` MCP resource, the HTTP `GET /stats` route,
  and the CLI `stats` subcommand. All four now call one `stats::collect` and
  cannot drift.
- All four swallowed database errors with `.unwrap_or(0)`, reporting an empty
  store when the database was unreadable. Errors now propagate.

### Notes
- Database size comes from SQLite's own page accounting rather than a
  filesystem `stat`, so it is correct for an in-memory database — where the
  reference has no file to measure and reports 0.
- No vitality distribution here. The issue called for it, but the reference
  keeps vitality reporting in `remind_me_vitality_report` (#6); `memory_stats`
  has none. Putting buckets here would have invented a divergence.
- `categories` and `sources` serialize alphabetically where the reference emits
  them count-descending. JSON objects are unordered by specification, and
  matching the reference's order would have required a new dependency for
  insertion-ordered maps, so consumers that care should sort by value.

## 2026-07-29 — Wiki list and delete (#4)

### Added
- `remind_me_wiki_list` — every page, most recently updated first. The core
  `list_wiki_pages` already existed; it had simply never been exposed as a tool.
- `remind_me_wiki_delete` — deletes by **title or slug**. Both work because the
  input is run through the existing `wiki_import::slugify`, which is idempotent
  on a string that is already a slug: `"VLAN Setup!"` and `"vlan-setup"` both
  resolve to `vlan-setup`. This is how the reference accepts either form.
- Reserved system pages (`index`, `log`, `schema`) are refused rather than
  deleted, matching `wiki.RESERVED_SLUGS`. None of them exist yet — this crate
  has no on-disk wiki — so the guard is there to keep behavior stable once
  `wiki_load` / `wiki_compile` start generating them.
- 12 tests, including casing and punctuation drift in titles, delete
  idempotence, and reserved pages addressed by title rather than slug.

### Notes
`get_wiki_page` and `list_wiki_pages` each carried their own copy of the
row-to-struct mapping; both now share one helper alongside the new delete.

## 2026-07-29 — Complete memory CRUD (#3)

### Added
- `remind_me_list` — filter by category, source, and tags (ALL-of), newest
  first, with `limit` clamped to 1..=100 and a `total` that counts every match
  rather than just the returned page.
- `remind_me_update` — partial update of content, category, tags, and metadata.
- `remind_me_delete` — removes a memory by id.
- 17 tests covering pagination tiling, ALL-of tag matching, FTS consistency
  across update and delete, entity-link cascade, and JSON-RPC round trips.

### Notes
Two behaviors differ from what issue #3 originally specified, after reading the
reference more closely:

- **Delete is a hard delete, not a soft delete.** `remind_me` tombstones via
  `deleted_at` only when sync is configured, so the deletion can propagate to
  other nodes; with sync off its path is a plain `DELETE`. This crate has no
  sync layer, so a hard delete is the reference-matching behavior. The
  `deleted_at` column and its read filters remain for when sync lands.
- **Update does not recompute vitality.** The reference seeds `base_weight` from
  `source` alone — it is not category-derived — and its update leaves the value
  alone. `decay_rate` *is* recomputed here on a category change, because in this
  crate it is a pure function of category and would otherwise go stale.

Tag filtering runs over `json_each(memories.tags)` rather than a normalized tag
table, keeping the predicate in SQL so `COUNT`/`LIMIT`/`OFFSET` stay correct.
It becomes a plain join once `memory_tags` lands (#10), with no caller changes.

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
