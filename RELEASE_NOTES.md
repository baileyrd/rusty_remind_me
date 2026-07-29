# Release Notes

Dated entries, newest first. One entry per merged pull request.

## 2026-07-29 — Wiki search, and an FTS sanitizer both searches now use (#15)

### Added
- `remind_me_wiki_search` — BM25 full-text search over wiki page titles and
  content, with an FTS5 `snippet()` excerpt. `limit` clamps to 1..=50.
- `fts::sanitize_fts_query`, shared by wiki and memory search.

### Fixed
- **Memory search choked on ordinary punctuation.** `search_memories` passed the
  raw query straight to `MATCH`, where `?`, `'`, `,`, `.` and `-` are operator
  syntax — so `what's the plan, exactly?` was a SQLite *syntax error*, not a
  search returning nothing. Both searches now tokenise, quote each token (which
  also stops `and` / `or` / `near` being parsed as operators), and join with
  `OR`; BM25 still ranks by term importance.
- A query with no searchable tokens short-circuits to no results. `MATCH` on an
  empty expression is itself an error.

### Notes
This is a visible change to memory search: queries that previously errored now
return results. Nothing that worked before behaves differently — the sanitizer
is a no-op on a query that was already a bare word list.

## 2026-07-29 — Reset the schema to remind_me's current one (#29)

### Changed
- **The schema is now generated, not written.** `schema_tables.sql`,
  `schema_indexes.sql` and `schema_triggers.sql` are dumped verbatim from a
  `remind_me` database's `sqlite_master` and compiled in with `include_str!`.
  They are not hand-edited; they are regenerated.
- The hand-transcribed 19-step ladder is gone. This crate no longer replays
  `remind_me`'s version history — it creates the current schema and reconciles
  anything that differs, then stamps the version.

### Fixed
- Four tables diverged from the reference in columns *and* constraints:
  `wiki_pages` (missing `summary`/`mtime`, carrying target-only
  `topic`/`created_at` that were `NOT NULL` with no default — which would have
  made `remind_me`'s inserts fail outright), `entities` (missing `node_id`,
  carrying a `UNIQUE` the reference lacks), `memory_entities` (missing
  `created_at`, carrying `ON DELETE CASCADE` foreign keys the reference
  deliberately omits), and `entity_relations` (entirely different column names).
- Parity is now **exact and verified**: 21 tables, 29 indexes, 11 triggers, DDL
  identical after normalisation, checked against a database built by replaying
  `remind_me`'s own migrations.

### Removed
- `wiki_pages.topic`. `remind_me_wiki_write` takes `summary` instead, which is
  the column the reference actually has. `wiki_import` still parses `topic:`
  front matter and reports it, but no longer persists it.
- `memory_entities`' cascade. `delete_memory` now cleans up `memory_entities`,
  `memory_feedback` and `memory_associations` explicitly, matching the
  reference — which omits the foreign keys because sync can deliver a mention
  link before the memory it points at, and a cascade would reject that.

### Notes
A legacy database is reconciled rather than abandoned: tables whose DDL differs
are rebuilt carrying the intersection of old and new columns,
`last_accessed_at` is renamed (not replaced) so access times survive, and
`memory_tags` and both FTS indexes are backfilled for rows that predate the
triggers maintaining them.

The parity test now compares **every** table, index and trigger by normalised
DDL. The previous one compared table names and `memories` columns, which is
exactly the shape of the gap it failed to catch.

## 2026-07-29 — Tag filtering uses the normalized index (#10)

### Changed
- `list_memories` filters tags against `memory_tags` rather than scanning
  `json_each(memories.tags)` per row. `idx_memory_tags_tag` now serves the
  lookup instead of parsing JSON for every candidate.

### Notes
Behaviour-preserving by construction: every existing ALL-of tag test passes
untouched. Correctness now rests on the `memories_tags_ai` / `_au` / `_ad`
triggers keeping the index in step with the JSON column, so there is a new test
covering the case where they could drift — editing a memory's tags and checking
the removed tag stops matching while the added one starts.

## 2026-07-29 — Real migration ladder; schema now matches the reference (#2)

### Fixed
- **The `user_version` stamp is no longer a lie.** The schema previously created
  7 tables and then stamped 19. `remind_me` reads that number on open and skips
  migrating anything already at 19, so a database written here was permanently
  missing 14 tables — the stamp defeated the interoperability it exists for.
  Version is now written step by step, as each migration completes.
- **Databases already carrying the false stamp are detected and repaired.** They
  cannot be identified by version alone, so the schema itself is inspected; if
  the stamp does not match reality the ladder is replayed from zero. Every step
  is idempotent, so replaying only fills gaps.

### Added
- 19 ordered migrations mirroring the reference's, producing **exact parity**:
  all 21 tables, all 26 `memories` columns *in the reference's order*, and all
  11 triggers.
- 9 columns the schema lacked: `node_id`, `client`, `base_weight`, `status`,
  `memory_type`, `source_capture_id`, `doc_id`, `chunk_index`, and `accessed_at`.
- `memory_tags` with its three sync triggers, plus a backfill from the existing
  JSON `tags` column.
- The `sync_outbox` triggers. This crate has no sync layer, but `remind_me` reads
  that table to decide what to propagate and will not re-add the triggers to a
  database already stamped 19 — without them, records written here would look
  migrated while silently never syncing.
- 10 tests, including reference-parity assertions and a repair test that builds
  the old 7-table schema, stamps it 19, and checks it heals.

### Changed
- **`memories.last_accessed_at` is now `accessed_at`**, matching the reference.
  Existing databases are *renamed*, not given a second column — a rename keeps
  the values, where adding one would silently reset every memory's access time.
- **`base_weight` is a real column.** `effective_vitality` reads it directly
  instead of treating stored `vitality` as a stand-in, which retires the hazard
  documented in #6: that substitution was exact only while nothing wrote to
  `vitality` after insert, and would have double-counted the frequency boost the
  moment access tracking landed.
- `search_memories` now derives its `SELECT` list from `MEMORY_COLUMNS` rather
  than spelling it out, after the hand-written list silently omitted
  `base_weight`.

### Notes
`memories_vec` is not created. The reference makes it only when the `sqlite-vec`
extension loads, so it is not part of the base schema — the earlier gap analysis
listing it as a plain missing table was wrong.

`status` and `memory_type` exist as columns but nothing reads them yet; the
decay priors here still key off `category`. Wiring them is #17's business.

## 2026-07-29 — Database backup (#8)

### Added
- `remind_me_backup` — a WAL-safe online backup written to `backups/` beside the
  database file, with the oldest pruned beyond a retention count of 10.
- Uses SQLite's online backup API via `rusqlite::backup`, not a file copy. The
  database runs in WAL mode, so copying the `.db` alone would miss anything
  still in the `-wal` and could capture a torn page mid-write.
- 10 tests, including a point-in-time snapshot check, retention pruning the
  oldest, and successive backups not colliding on filename.

### Notes
- The tool takes **no parameters**. The issue called for confining a
  caller-supplied destination path; the reference has no such input, so there is
  nothing to confine — the arbitrary-write concern does not arise. The internal
  `label` is still slugged to filename-safe characters, with a test, so that
  stays true if a label is ever plumbed through.
- Backing up an in-memory database is refused with an explanation rather than a
  raw SQLite error, since there is no on-disk location to write beside.
- `rusqlite` gains its `backup` feature. Not a new dependency — a feature flag
  on one already in the workspace.

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
