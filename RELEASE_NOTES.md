# Release Notes

Dated entries, newest first. One entry per merged pull request.

## 2026-07-29 — Entity annotation, and three entity-layer bugs (#7)

### Added
- `remind_me_annotate` — applies subject/predicate/object triples and entity
  mentions to existing memories, in batches of 1–100. Only the SPO fields
  supplied are written; omitted ones keep their value.
- Per-item error handling rather than all-or-nothing, matching the reference:
  one unknown `memory_id` is reported in `errors` and the rest of the batch
  still applies. An extraction pass carrying one stale id should not lose 99
  good annotations.
- `entity::apply_entity_mentions`, shared by annotate and `add_memory`.

### Fixed
- **`add_memory` silently discarded its `entities` field.** `MemoryAddInput` has
  always accepted entity mentions; they were parsed and dropped, so callers
  supplying them got a no-op with no error. They are now applied through the
  same path as annotate.
- **`upsert_entity` never merged aliases.** Its `ON CONFLICT(name) DO UPDATE`
  clause updated `kind` and `updated_at` but not `aliases`, so aliases could
  only ever be set on first insert. They now union-merge — existing first, new
  appended, de-duplicated.
- **`upsert_entity` crashed on a casing variant.** It looked up by the
  case-sensitive `name` column while deriving `id` from the case-folded name, so
  `"tasmania"` after `"Tasmania"` missed the lookup, attempted an insert, and
  hit the `entities.id` unique constraint. Lookups now key on the derived id,
  which is what carries the identity. `get_entity_by_name` resolves the same
  way and is now case- and whitespace-insensitive.
- `kind` precedence now matches the reference: an existing kind is never
  overwritten by a later mention, and a missing one is filled in. Previously
  `COALESCE(excluded.kind, ...)` let a later guess clobber a deliberate earlier
  value.

### Notes
The `tools/list` JSON literal outgrew `serde_json::json!`'s macro recursion
limit. The annotate schema is built in its own function and interpolated, which
costs no expansion depth; further deeply-nested schemas should do the same.

## 2026-07-29 — Vault vitality report (#6)

### Added
- `remind_me_vitality_report` — active/dormant counts, average vitality, a
  vault health percentage, distribution buckets, and a per-category breakdown.
  Defaults to JSON, unlike most tools, matching the reference.
- `vitality::effective_vitality` — a memory's vitality *now*, with real
  elapsed-days decay applied. The stored `vitality` column is a write-time
  snapshot and never decays on its own.
- `vitality::is_dormant`, and the `DI-04` **open-ended top bucket**: an accessed
  memory scores above 1.0 (one access gives `sqrt(2) ≈ 1.41`), so a closed top
  bucket would drop rows and the counts would not sum to the total. There is a
  test asserting that sum.
- 14 tests, including bridge protection, the floor boundary, and decay actually
  moving a year-old memory into dormancy.

### Notes
**`base_weight` has no column in this crate**, where the reference has one.
`effective_vitality` therefore reads the stored `vitality` as the base weight.
That is exact today because nothing ever updates `vitality`, `access_count`, or
`last_accessed_at` after insert — there is no access tracking — so the column
still holds precisely the seeded value.

Whoever adds access tracking must add a real `base_weight` column at the same
time. Once `vitality` is rewritten to include the frequency boost, feeding it
back in would apply that boost twice. The invariant is documented on the
function.

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
