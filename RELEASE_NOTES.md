# Release Notes

No PR workflow yet on this repo's first commit — this pushes directly to the
`claude/repo-config-danror` branch to establish the default branch and initial
scaffold. Once there's a real default branch and a second change lands through a
PR, switch to one entry per merged PR (reverse chronological), same convention as
[AISF's RELEASE_NOTES.md](https://github.com/baileyrd/AISF/blob/main/RELEASE_NOTES.md).

---

## Wiki exporter (closes #57)
**2026-08-12**

- **Added:** `WikiExporter`, porting `src/dbs/export/wiki.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — a zipped,
  synthesis-layer wiki (as opposed to `obsidian`'s one-note-per-item
  mirror): `pages/<slug>.md` per item or per source/axis hub, a
  generated `index.md` grouping pages by topic with `[[wikilinks]]`,
  and the same `manifest.json` shape as `obsidian`'s, plus
  `wiki_grouping`. `ExportQuery::wiki_grouping` (`"topic"`, the
  default, or `"item"`) selects the export-wide shape; a per-source
  `ExportProfile` (`page_per`/`group_by`/`body_from`) can override it,
  naming a connector's own grouping axes (e.g. Reddit's `subreddit`)
  instead of collapsing onto the generic `Tag:` namespace.
  Registered as `"wiki"` in `get_exporter`/`available_formats`.
- **Added `ExportSource::profiles()`**, a new default-empty trait
  method (`HashMap<String, ExportProfile>` keyed by source name) — the
  seam the wiki exporter reads per-source rendering rules through;
  wiring a real source's resolved profiles is the separate CLI-facing
  issue #70, same deferral as `ExportSource` itself.
- Unknown `wiki_grouping` values error via `DbsError::Config` rather
  than panicking, matching the reference's `ValueError`.
- 7 new tests: empty result set (index + manifest only), topic
  grouping creating a source hub and a tag hub cross-linked to each
  other, item grouping creating one page per item with no hub pages,
  multi-topic grouping creating a distinct hub per tag, a filtered
  subset, an unknown-grouping config error, and a metadata sanity
  check.

## Obsidian vault exporter (closes #56)
**2026-08-12**

- **Added:** `ObsidianExporter`, porting `src/dbs/export/obsidian.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — a zipped Obsidian
  vault: one `.md` note per item under `notes/` with url2obs-compatible
  YAML frontmatter (`category`/`author`/`title`/`description`/`source`/
  `clipped`/`published`/`tags`, plus `dbs_`-namespaced provenance
  fields), archived media blobs under `media/<source>/<external_id>/`
  (embedded via `![[...]]` when the mime type looks image-like, linked
  otherwise), and a `manifest.json` combining `ExportSource::manifest()`
  with the query and per-source/media counts. Note-name collisions are
  disambiguated by `external_id`, then by source, matching the
  reference. Registered as `"obsidian"` in `get_exporter`/
  `available_formats`.
- **Added dependency:** `zip = "2"` (`dbs-core`, `deflate` feature only)
  — the first exporter needing an actual zip archive; the (still
  upcoming) archive exporter (#58) will share it. Pulled forward from
  #58's gap-analysis note since Obsidian's `media_type` is already
  `application/zip` in the reference.
- 7 new tests: empty result set (manifest-only), a fully-populated item
  (frontmatter, clipped-date truncation, tags), a deleted-item flag,
  note-name collision disambiguation, a media blob written and linked
  from its note, a media blob with no bytes skipped, and a metadata
  sanity check.

## Markdown exporter (closes #55)
**2026-08-12**

- **Added:** `MarkdownExporter`, porting `src/dbs/export/markdown.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — a human-readable
  document grouped by source, with a `##` heading per source, a `###`
  heading per item (title, falling back to `url` then `external_id`),
  a metadata line (`kind`/`created`/`**deleted**`), a bare `<url>`
  autolink, backtick-wrapped tags, and the body. Lossy by design.
  Registered as `"markdown"` in `get_exporter`/`available_formats`.
- Ports the reference's Python-truthiness field checks (`null`/`false`/
  `0`/empty string/collection all falsy) and title-flattening/escaping
  (collapses whitespace, escapes `[`/`]`) faithfully.
- 8 new tests: empty result set, a fully-populated item, multi-source
  grouping (one heading per source, not per item), a deleted-item flag,
  title fallback chain, title flattening/escaping, missing-source
  fallback to `(unknown)`, and a metadata sanity check.

## CSV exporter (closes #54)
**2026-08-12**

- **Added:** `CsvExporter`, porting `src/dbs/export/csv.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — a flattened,
  explicitly lossy view over the fixed base columns (`source`, `type`,
  `external_id`, `item_kind`, `title`, `url`, `body`, `tags`,
  `created_at`, `updated_at`, `revision`, `deleted`, `deleted_at`,
  `content_hash`, plus `raw` when `ExportQuery::include_raw` is set).
  The first physical line is a `# NOTE:` comment warning that CSV is not
  restore-grade; `tags` is comma-joined, `deleted` is emitted as `"0"`/
  `"1"`, and `raw` is JSON-encoded. Registered as `"csv"` in
  `get_exporter`/`available_formats`.
- **Added dependency:** `csv = "1"` (`dbs-core`), for RFC 4180 quoting/
  escaping matching the reference's stdlib `csv` module behavior.
- 7 new tests: empty result set (comment + header only), single item,
  values needing escaping (commas/quotes/newlines) round-trip through the
  `csv` crate's own reader, `tags`/`deleted` special-casing, `raw`
  included/omitted by `include_raw`, and a metadata sanity check.

## NDJSON exporter (closes #53)
**2026-08-12**

- **Added:** `NdjsonExporter`, porting `src/dbs/export/ndjson.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — the canonical,
  lossless, streaming export format: one JSON object per line, no
  wrapping array. Registered as `"ndjson"` in `get_exporter`/
  `available_formats` alongside the JSON exporter from #51.
- 5 new tests: empty result set writes nothing, single item is one line,
  multiple items are one line each in declaration order, and a metadata
  sanity check (`format`/`media_type`/`file_ext`).

## JSON exporter (closes #51)
**2026-08-12**

- **Added:** `dbs-core::export`, the shared exporter plumbing from
  `src/dbs/export/base.py`/`__init__.py` in baileyrd/Daily-Backup-System
  (pinned `@6cc6491`) — `ExportResult` (summary of a completed export),
  `ExportSource` (a streaming data source trait: `items`/`revisions`/
  `media_blobs`/`manifest`, implemented by the service over storage +
  an `ExportQuery` in a later CLI-facing issue, #70), `Exporter` (the
  per-format contract: `format`/`media_type`/`file_ext`/`write`), and
  `get_exporter`/`available_formats` (the reference's `EXPORTERS` dict
  equivalent).
- **Landed here, not split into its own issue:** #50 (the real
  `ExportQuery`) deliberately left these base types unported, noting
  they'd be "picked up by the individual exporter issues." This is the
  first such issue, so the shared plumbing lands alongside the format it
  introduces — every subsequent exporter issue (ndjson #53, csv #54,
  markdown #55, obsidian #56, wiki #57, archive #58) adds its own module
  plus one more `get_exporter` arm.
- **Added:** `JsonExporter`, porting `src/dbs/export/json.py` — one JSON
  array of item objects, brackets/commas streamed directly to the
  output writer (never buffering the whole export as one string), pretty
  printed the same as the reference's `indent=2`.
- 7 new tests: `get_exporter` success/unknown-format, `available_formats`,
  and the JSON exporter's empty/single/multi-item cases (parsing the
  written bytes back to verify structure and count rather than asserting
  exact text, since `ItemRow`'s `HashMap` backing has no defined key
  order) plus a metadata sanity check (`format`/`media_type`/`file_ext`).

## Export base: real ExportQuery (closes #50)
**2026-08-12**

- **Replaced the placeholder `ExportQuery`** in
  `crates/dbs-core/src/storage/mod.rs` with the reference's real type
  from `src/dbs/export/base.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`): `sources: Option<Vec<String>>` (by source *name*, not the
  placeholder's single internal `source_id`), `item_types:
  Option<Vec<String>>` (was a single `item_kind`), `since`/`until`
  (against `item_created_at`), the new `since_updated`/`until_updated`
  pair (against `item_updated_at` — the connector-reported upstream edit
  time), `include_deleted`, `include_raw`, `include_revisions`, and
  `wiki_grouping`. Every field ANDs together, matching the reference's
  explicit no-OR-semantics documentation.
- **`Storage`'s SQLite implementation updated to match:**
  `build_filter` now filters by `s.name IN (...)` (a join against
  `sources`, not a bare `source_id` equality) and `i.item_kind IN
  (...)` for multi-value filters, plus new `item_updated_at`
  range clauses for `since_updated`/`until_updated`. `iter_items`/
  `iter_revisions` now honor `include_raw` for real — previously always
  included the raw payload regardless of the query (noted as a known
  simplification in #11's original module doc-comment); `row_to_item`/
  `row_to_revision` became `fn(bool) -> impl Fn(&Row) -> ...` so the
  toggle threads through per-call rather than being baked into the row
  mapper.
- **Slightly exceeds this issue's own acceptance checklist, on purpose:**
  the checklist named `sources`/`item_types`/the four date fields/
  `include_raw`/`include_deleted`; `include_revisions` and
  `wiki_grouping` are also part of the reference's real `ExportQuery`
  dataclass and cost nothing extra to include now versus as a future
  correction PR, so they're ported too (unused by storage — read only by
  the archive/wiki exporters, not filed yet).
- 5 new tests: multi-source-name filtering (including the "empty list
  means every source" falsy-check parity with the reference), item-type
  filtering, `since_updated`/`until_updated` range filtering, and
  `include_raw` toggling for both `iter_items` and `iter_revisions`.

## Export profile: ExportProfile/ExportProfileOverride types (closes #49)
**2026-08-12**

- **Added:** `dbs-core::export_profile`, porting
  `src/dbs/core/export_profile.py` in baileyrd/Daily-Backup-System
  (pinned `@6cc6491`) — `ExportProfile` (per-source selection/rendering
  rules: `enabled`, `item_kinds`, `group_by`, `body_from`, `page_per`),
  `ExportProfileOverride` (the `[sources.NAME.export]` config block, every
  field optional so an unset field keeps the connector's default),
  `resolve_export_profile` (field-by-field merge + `page_per` validation),
  `raw_value`/`group_values` (dotted-path resolution against an export
  row's `raw` payload, falling back to its normalized columns), and
  `axis_label` (wiki hub-page title casing).
- **This was missed entirely in the original gap-analysis pass** — the
  same failure class as `BackupService`/`service.py`: referenced by both
  `connector.py`'s `export_profile` class attribute and `config.py`'s
  `SourceConfig.export`, but had no row of its own. `connector.rs` and
  `config.rs` both previously had no way to declare or override an
  export profile at all.
- **Wired in:** `Connector::export_profile()` (new default-`None` trait
  method, same declarative-default pattern as `capabilities()`/
  `item_kinds()`); `registry::Handshake::export_profile` (a connector
  subprocess can now declare its default profile over the ADR-0001
  JSON-IPC handshake); `SourceConfig::export`, parsed from a
  `[sources.NAME.export]` TOML block (already reserved in
  `RESERVED_SOURCE_KEYS` since #13, just never parsed into a typed field
  until now).
- 21 new tests: `accepts_kind` selection logic, `resolve_export_profile`
  field-by-field merge (including the "override wins only where it's
  actually set" case) and `page_per` validation, `raw_value`'s
  raw-then-normalized-columns fallback and dotted-path resolution,
  `group_values`' scalar/list/boolean/non-scalar handling, `axis_label`'s
  title-casing and empty-path fallback, and three `load_config`
  integration tests (no `export` block, a fully-populated one, and
  confirming `export` doesn't leak into connector `options`).

## SQLite storage: browse_items video-link thumbnail fallback (closes #48)
**2026-08-12**

- **Added:** `browse_items`'s `thumb_url` now matches the reference's
  `src/dbs/storage/sqlite.py` (pinned `@6cc6491`) exactly —
  `COALESCE(first image media URL, CASE WHEN raw_json->>videoLink
  matches YouTube/Loom/Vimeo THEN that videoLink END)`. Previously this
  port only ever returned the first image media's URL; items shaped by
  connectors that store a `videoLink` field instead of image media (e.g.
  skool lessons) got no thumbnail at all. Note the reference doesn't
  derive an actual thumbnail *image* URL here — it passes the raw
  `videoLink` through, on the assumption a web tier resolves it
  client-side (YouTube's thumbnail convention, or oEmbed for
  Loom/Vimeo); this port matches that division of responsibility
  exactly, not a raw image URL.
- 5 new tests: YouTube/Loom/Vimeo `videoLink` fallback (one test per
  host), no thumbnail when neither image media nor a recognized
  `videoLink` is present, and image media taking priority over a
  `videoLink` when both exist.

## SQLite storage: FTS5 full-text index (closes #47)
**2026-08-12**

- **Added:** `SqliteStorage::ensure_fts`, porting the reference's
  `_ensure_fts` from `src/dbs/storage/sqlite.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — an `items_fts` FTS5
  virtual table over `items(title, body)`, kept in sync via
  `AFTER INSERT`/`AFTER DELETE`/`AFTER UPDATE OF title, body` triggers,
  with a one-time `INSERT INTO items_fts(items_fts) VALUES('rebuild')`
  backfill the first time it's created on a pre-existing database.
  `browse_items` now tries an FTS5 `MATCH` query first (all search
  tokens quoted and ANDed, final token prefix-matched, matching the
  reference's `_fts_match_query`), falling back to the `LIKE` path from
  #36 when FTS5 is unavailable or a pathological query trips `MATCH`'s
  parser — same attempt-list structure as the reference.
- **Deliberately not a numbered migration:** matches the reference's own
  reasoning — a SQLite build without the FTS5 module would fail a
  migration permanently, whereas `ensure_fts` just returns `false` and
  `browse_items` degrades to `LIKE`-only. Called from `SqliteStorage::
  open()` and from the `Storage::migrate()` trait method, both already
  idempotent call sites.
- **No new dependency, despite first appearances:** `rusqlite` doesn't
  gate FTS5 behind a Cargo feature at all (verified by reading
  `libsqlite3-sys`'s `build.rs` directly, not assumed) — its `bundled`
  build unconditionally compiles with `-DSQLITE_ENABLE_FTS5`, so the
  existing `rusqlite = { features = ["bundled"] }` dependency already
  covers this.
- 5 new tests: FTS5 enabled on a fresh database, `_fts_match_query`'s
  quoting/prefix-matching pure logic (including embedded-quote
  escaping), case-insensitive + prefix search, all-tokens-required
  (AND) semantics, and the sync trigger actually re-indexing after a
  title change (old term stops matching, new term starts).

## BackupService: connector instantiation, VPN guard, run-mode selection, backup_all batching (closes #46)
**2026-08-12**

- **Added:** `dbs-core::service::BackupService`, mirroring
  `src/dbs/core/service.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`)'s `backup_source`/`backup_all`/`status`/`history` — the
  rest of `BackupService` beyond #21's narrow reap-once slice. Covers:
  connector instantiation via the plugin registry (#45), VPN guard checks
  (`vpn_guard_skip`, using the existing `VpnGuard` enum and
  `netns::in_named_netns`), run-mode selection (`choose_mode`, matching
  the reference's force-full/force-reconcile/first-run/explicit-mode/
  auto-reconcile-every-N-runs rules exactly), due-date scheduling helpers
  (`next_due_at`/`is_due`, same slack-window table as the reference:
  hourly→50min, daily→20h, weekly→6d), `backup_source`'s full run
  bookkeeping (source registration, cursor/run-count load, lock
  acquire/release, `begin_run`/`finish_run`), sequential `backup_all`
  batching with `continue_on_error`, and `status`/`history` rendering
  from storage.
- **New seam, not in the reference — the `ConnectorRunner` trait.** The
  reference's `backup_source` hands off to `self.engine.run_source(rc,
  ctx, ...)`, which drives the connector's actual fetch loop. That
  bridge (ADR-0001 steps 2-3: writing a `RunContext`, reading a
  `FetchEvent` stream back) has no issue yet — #45 only implemented the
  handshake half. Rather than block this issue's real scope (connector
  instantiation, VPN guard, batching) on that follow-up landing first,
  `ConnectorRunner` is the injected seam the reference's
  constructor-injected `engine` plays: `BackupService` does every
  preflight step for real and calls out to a `&dyn ConnectorRunner` for
  the actual fetch. `UnimplementedRunner` is the production stand-in
  until a real one exists (fails clearly, not silently); tests use a
  scripted fake (`ScriptedRunner`).
- **A deliberate improvement over the reference, stated plainly:** an
  uncaught exception from the reference's `engine.run_source` skips
  `finish_run` entirely, leaving that row `running` until the next reap
  — a latent rough edge. `backup_source` here always calls `finish_run`
  exactly once, translating a `ConnectorRunner` error into a `Failed`
  result instead.
- `ConnectorRegistry::from_resolved` (new, in `registry.rs`): builds a
  registry directly from already-resolved connectors, bypassing spawn/
  handshake — used by `BackupService`'s tests, and generally useful for
  any caller that already has `RegisteredConnector` values from
  elsewhere.
- `RunResult::skipped`/`RunResult::failed` (new, in `models.rs`):
  small constructors for the early-exit paths (disabled source, VPN
  skip, dry-run) and `backup_all`'s `continue_on_error` isolation.
  `RunStatus` now derives `Default` (`Failed`, via the repo's usual
  `#[default]`-attribute pattern) so `ConnectorRunOutcome` can too.
- 35 new tests: pure-function coverage for `choose_mode` (every branch:
  force-full, incremental-incapable, force-reconcile-needs-enumeration,
  first-run, explicit-mode-with-reconcile-downgrade, auto-every-N-runs
  including the "0 means unset" rule) and `vpn_guard_skip`/`next_due_at`/
  `is_due`; integration tests against a fuller in-memory `Storage` double
  (`FakeStorage`) covering unknown source, disabled source, VPN skip,
  unregistered connector type, dry-run, the happy path (source
  registered, run finished, history recorded), a `ConnectorRunner` error
  becoming a `Failed` result, source-locked detection, `backup_all`
  batching/`continue_on_error` (both directions), and `status`/`history`
  rendering before and after a run.

## Plugin registry: subprocess discovery, contract validation, collision resolution (closes #45)
**2026-08-12**

- **Added:** `dbs-core::registry::ConnectorRegistry`, implementing
  ADR-0001's subprocess + line-delimited JSON-IPC design
  (`docs/adr/0001-dynamic-plugin-registry.md`, issue #5), matching the
  *behavior* of `src/dbs/core/registry.py` in baileyrd/Daily-Backup-System
  (pinned `@6cc6491`) — entry-point discovery with isolation, contract
  validation, `CORE_API_VERSION` gating, and collision precedence
  (explicit override > built-in shadow protection > deterministic
  third-party sort) — over a different mechanism (spawn + handshake line,
  not Python class introspection).
- `ConnectorRegistry::discover` spawns each `ConnectorCandidate`, waits up
  to a caller-supplied timeout for one JSON handshake line on its stdout
  (via a worker thread + `mpsc::recv_timeout` — a blocking `read_line` has
  no deadline of its own), validates the contract (type-name format,
  `Capabilities::assert_coherent`, non-empty `item_kinds`, `secret_keys`
  required when `requires_auth`, `core_api_version` compatibility via
  `crate::versioning::is_api_compatible`), then resolves same-type
  collisions. A candidate that fails to spawn, hangs past the timeout,
  writes malformed JSON, or fails validation is recorded in the report's
  `failures` and never crashes discovery of the others.
- **Scoped intentionally:** this issue implements discovery given an
  already-resolved list of candidate connector commands — enumerating
  those candidates from a directory scan of `dbs-connector-*` binaries or
  a `connectors.toml` manifest (the ADR's "replaces entry-point metadata"
  step) is deferred to the CLI issue that needs it (`dbs sources`/`dbs
  connectors`, #71), which already has to resolve a connectors
  directory/PATH from config. Likewise, only the handshake half of the
  protocol (ADR steps 1 and 4) is implemented here; the run/stream half
  (writing a `RunContext`, reading `FetchEvent` lines back) is separate
  follow-up work bridging a `RegisteredConnector` to the `Connector`
  trait's `fetch` signature.
- **New test-only subprocess fixture:** `src/bin/test_connector_fixture.rs`
  (a `[[bin]]` target auto-discovered by Cargo, not part of the public
  product) — a controllable fake connector process for exercising the
  real handshake protocol end-to-end (valid, malformed JSON, incompatible
  version, invalid type, no output, and a hang past the deadline), spawned
  from a new integration test file (`tests/registry_integration.rs`) via
  `env!("CARGO_BIN_EXE_test_connector_fixture")`.
- 17 new tests: 10 unit tests for the pure validation/collision-resolution
  logic, 7 integration tests spawning the real fixture binary (including
  a genuine timeout-past-deadline case, asserting it completes well under
  5s rather than actually blocking).

## SQLite Storage: export, browse, stats, maintenance (closes #36)
**2026-08-12**

- **Added:** `iter_items`, `iter_revisions`, `iter_media_blobs`,
  `item_counts`, `browse_items`, `get_item`, `get_media_blob`, `metrics`,
  `maintain`, `prune_revisions`, and `vacuum_into` on `SqliteStorage` —
  the third and final trait-section PR against #36, mirroring
  `src/dbs/storage/sqlite.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`)'s export/browse/stats/maintenance methods. #36 is now
  closed: every `Storage` method has a real SQLite-backed implementation.
- **Scope corrections found while implementing this, both filed as new
  gap-analysis rows rather than silently absorbed:**
  - **FTS5 full-text search is not ported.** The reference's
    `browse_items` tries an FTS5 `MATCH` query first (built by
    `_ensure_fts`, called from `migrate()`), falling back to `LIKE` only
    when FTS5 is unavailable or the query trips `MATCH`'s parser. This
    PR always uses the `LIKE` path — FTS5's index/triggers/backfill need
    their own `migrate()`-adjacent hook this port doesn't have.
  - **The video-link thumbnail fallback (YouTube/Loom/Vimeo URL
    detection) is not ported.** `browse_items` here only returns the
    first image media's URL as `thumbnail`; the reference derives a
    thumbnail from a `videoLink` field when an item has no image media —
    UI polish specific to one connector's item shape.
- **`iter_items`/`iter_revisions`/`iter_media_blobs` collect eagerly**
  into a `Vec` rather than streaming a live cursor — a `Box<dyn Iterator
  + 'a>` borrowing a `rusqlite::Statement`/`Rows` across the call is a
  self-referential-lifetime problem this port doesn't take on. Export
  result sets are bounded by what one backup run holds.
- **Binary blobs have no dedicated wire representation:** `ItemRow`
  (`HashMap<String, Value>`) has no binary variant, so
  `get_media_blob`/`iter_media_blobs` encode blob bytes as a JSON array
  of byte values (`serde_json`'s default `Vec<u8>` encoding) rather than
  the reference's raw Python `bytes` — no `base64` dependency added for
  a row type already documented as "kept loose on purpose" (#11).
- 24 new tests against a real in-memory SQLite connection covering every
  new method, including a full `upsert → browse → get_item` round trip,
  media-blob archiving/retrieval, `prune_revisions`' keep-newest-N
  behavior, and `vacuum_into`'s existing-target refusal.

## SQLite Storage: items/batch commit, media archiving (part of #36)
**2026-08-12**

- **Added:** `upsert_items`, `soft_delete_missing`, and `live_external_ids`
  on `SqliteStorage`, mirroring `SqliteStorage.upsert_items`/
  `soft_delete_missing`/`live_external_ids` in
  `src/dbs/storage/sqlite.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`) — this is the largest, most correctness-sensitive part of
  #36. Covers: batch classification against a pre-fetched existing-row
  index (created/updated/unchanged/deleted/undeleted), revision writing
  on every content change (`item_revisions`), the still-deleted-item stays
  deleted rule (a native-deletes source re-emitting a mutated trash item
  must never resurrect it), tag-scoped reconcile sweeps via a temp table
  anti-join against `live_ids` (`_sweep_live`, matching the reference's
  memory-bounded approach rather than loading every live id into
  memory), and inline media archiving (local-file bytes for
  `MediaRef::url`, or connector-prefetched bytes via `MediaRef::data`),
  each capped at `max_media_bytes` with the reference's "record path +
  size but drop the bytes" over-cap behavior.
- **Typed media, not a raw dict:** `PreparedItem::media` holds each entry
  as a `serde_json::Value` round-tripped from `MediaRef` (#4/#17);
  `upsert_items` deserializes back into `MediaRef` rather than pulling
  `url`/`data`/etc. out of the `Value` by hand.
- Whole-batch atomicity: `upsert_items` and `soft_delete_missing` each
  run inside one `rusqlite` transaction (no reference `transaction()`
  combinator per #11 — see the module doc-comment).
- 15 new tests against a real in-memory SQLite connection: empty-batch
  no-op, insert-as-created, unchanged-on-same-hash, updated-on-hash-
  change, native-delete-then-undelete, local-file media archiving
  (writes real bytes to a temp file and reads them back), media bytes
  skipped without `store_media`, supplied-media byte cap enforcement,
  sweep removal + idempotent second pass, and tag-scoped
  `live_external_ids`/`soft_delete_missing` isolation.
- **Still stubbed, pending a follow-up PR:** export/browse/stats/
  maintenance (`iter_items`/`iter_revisions`/`iter_media_blobs`/
  `browse_items`/`get_item`/`get_media_blob`/`metrics`/`maintain`/
  `prune_revisions`/`vacuum_into`), including FTS5 search. #36 stays
  open until that lands.

## SQLite Storage: sources, runs, cursor/state, locking (part of #36)
**2026-08-12**

- **Added:** `dbs-core::storage::sqlite_storage::SqliteStorage`, a concrete
  `Storage` implementation over the connection/schema from #12, mirroring
  `SqliteStorage` in `src/dbs/storage/sqlite.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). This PR covers **schema
  lifecycle, sources, runs (`begin_run`/`finish_run`/
  `reap_interrupted_runs`/`recent_runs`), cursor/state
  (`save_cursor`/`load_cursor`/run-count), and locking**
  (`acquire_lock`/`release_lock`), plus `spawn`/`close`/`integrity_check`.
- **Scoped intentionally, not an oversight:** issue #36 itself calls for
  splitting the ~1100-line reference module across multiple PRs by trait
  section. This PR's `SqliteStorage` implements every `Storage` trait
  method (required for the type to compile as a concrete `impl Storage`),
  but the items/upsert section
  (`upsert_items`/`soft_delete_missing`/`live_external_ids`) and the
  export/browse/stats/maintenance section return
  `Err(DbsError::Storage("... not yet implemented (see issue #36)"))` for
  now — the largest and most correctness-sensitive parts of the reference
  (batch classification, revision writing, media archiving, FTS5 search)
  land in follow-up PRs against the same issue.
- **Differences from the reference, all deliberate:** no `transaction()`
  context-manager combinator (per #11 — each method opens its own
  `rusqlite` transaction where needed); `migrate()` here is a
  redundant-but-idempotent second call since `open_connection` (#12)
  already migrates on open; `close()` best-effort runs `PRAGMA optimize`
  and otherwise relies on `Drop` rather than truly invalidating the value;
  the `Storage` trait's `finish_run` doesn't expose `items_failed` (see
  #11), so that column keeps its schema default until the items/upsert PR
  wires it through.
- 19 new tests against a real in-memory SQLite connection, covering
  source upsert/get/list/delete, run begin/finish/recent/reap-interrupted
  (including lock clearing on reap), cursor save/load (including the
  watermark-only-advances rule), run-count increment, lock
  acquire/release/contention, integrity_check, and `spawn`'s
  memory-vs-file-backed behavior.

## Managed HTTP client (closes #22)
**2026-08-12**

- **Added:** `dbs-core::http::ManagedHttpClient`, mirroring
  `src/dbs/core/http.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`): retry with exponential backoff + jitter on transient
  failures (network errors, 5xx, 429), `Retry-After` handling for both
  the delta-seconds and HTTP-date forms (capped at `max_retry_after`),
  immediate return on a non-429 4xx, and optional pre-emptive rate
  limiting. Jitter/backoff use the same deterministic LCG as the
  reference (no global RNG), so behavior is reproducible in tests.
- **Design choice, stated explicitly:** blocking (`reqwest::blocking`),
  not async. The rest of this crate is synchronous by design
  (`Connector::fetch` returns a plain `Iterator`); threading async
  through the whole connector trait would be a far bigger change than
  this issue warrants, and `reqwest::blocking` runs its own internal
  runtime without requiring `tokio` as an explicit dependency here.
  `tokio` stays a *future* dependency for whichever issue actually needs
  it (most plausibly the web tier).
- **New dependencies:** `reqwest` (`blocking` + `rustls-tls` features —
  rustls over the platform TLS stack for portability, matching the
  cross-platform floor decision) and, dev-only, `mockito` for real
  HTTP-level retry/status tests rather than only testing the pure
  helper functions.
- 16 new unit tests: 6 pure (`Retry-After` delta/negative/HTTP-date-
  future/HTTP-date-past/garbage/missing), 2 jitter (determinism, stays
  in `[0,1)`), 2 backoff (`Retry-After` capped at `max_retry_after`,
  exponential growth capped at `max_backoff`), 2 throttle (sleeps once
  the per-minute limit is hit, no-op without a configured limit), and 4
  against a real local mock server (success, immediate non-retryable
  4xx, retries exhausted on a persistent 5xx reporting `Transient`,
  retries exhausted on persistent 429s reporting `RateLimited`) —
  135/135 total passing across the workspace.

## Engine — soft-delete sweep safety decision (closes #20, subsumes #19)
**2026-08-12**

- **Added:** `dbs-core::engine::sweep_deletions`, mirroring the deletion-
  sweep block in `Engine.run_source` (`core/engine.py`,
  baileyrd/Daily-Backup-System, pinned `@6cc6491`): per reconcile scope
  (`"source"` or `"tag:<value>"`), compares a full enumeration against
  what storage still has live and refuses to sweep — recording a warning
  instead — when the enumeration is empty while live items exist, or
  when the fraction that would be deleted exceeds
  `sweep_safety_fraction`. Both are the signature of a truncated
  upstream listing, not genuine mass deletion; an unrecognized scope
  shape is refused rather than silently widened into a source-wide
  sweep.
- **#19 (revision history writing) is subsumed here, not implemented
  separately:** same discovery as #17's classification logic — revision
  writing (`_insert_revision`) is backend-specific SQL in the
  reference's `SqliteStorage`, with no independent engine-side content.
  It's tracked as part of #36.
- 8 new unit tests (unrecognized scope skipped with a warning, a safe
  source-scope sweep deletes the missing items, a tag scope passes the
  tag through to storage, an empty enumeration against existing live
  items is refused, a fraction over the safety threshold is refused
  with the percentage in the warning text, a fraction exactly at the
  threshold is allowed — strict `>`, not `>=`, matching the reference —
  zero existing live items is never unsafe, multiple scopes evaluated
  independently), 119/119 total passing across the workspace.

## Service — crash-recovery reap-once guarantee (closes #21)
**2026-08-12**

- **Added:** `dbs-core::service::reap_once` — calls
  `storage.reap_interrupted_runs()` at most once per shared flag, so
  repeated calls across a batch collapse to a single reap.
- **Scope correction (third one found this session, recorded plainly):**
  crash-recovery reaping turned out to be `BackupService`-level
  orchestration (`core/service.py`), not `Engine`-level — the reference's
  `Engine.run_source` has no reap call anywhere; `reap_interrupted_runs()`
  is called from `BackupService.backup_source`/`backup_all`. Worse:
  `core/service.py` (`BackupService`) was never given its own
  `gap-analysis.md` row at all, same failure class as `export_profile.py`.
  Added it now, sized L, split into "this issue's narrow reap-once slice"
  plus a much larger follow-up (connector instantiation via the registry,
  VPN guard checks, `backup_source`/`backup_all` batching, status/history
  rendering) not yet filed.
- The actual invariant: reap must run *exactly once* per top-level call —
  once before a standalone `backup_source`, once before an entire
  `backup_all` batch, never once per source touched within that batch. A
  mid-batch reap under `--parallel N` could flip a sibling's genuinely-
  still-running row.
- 3 new unit tests (first call reaps, repeated calls sharing one flag
  reap only once, independent flags each reap once — simulating two
  unrelated standalone `backup_source` calls), 111/111 total passing
  across the workspace.

## Engine — prepare + content-hash computation (closes #17)
**2026-08-12**

- **Added:** `dbs-core::engine::{prepare, compute_hash}`, mirroring
  `Engine._prepare`/`Engine._compute_hash` in `core/engine.py`
  (baileyrd/Daily-Backup-System, pinned `@6cc6491`): validates
  `item_kind` against the connector's declared kinds
  (`ConnectorError::Contract` if not declared), computes the content
  hash (a `revision_token` shortcut when the connector supplies one,
  otherwise a normalized projection with volatile fields stripped and
  tags sorted), gates `deleted`/media on the connector's capabilities,
  and formats timestamps via `iso_z`.
- **Scope correction, stated plainly rather than left implicit:** #17's
  title ("idempotent upsert classification") undersold what's actually
  engine-side in the reference. The created/updated/unchanged/deleted/
  undeleted *classification* — comparing the computed hash against
  what's already stored — is backend-specific SQL in the reference's
  `SqliteStorage._update_item`, not engine code. That belongs to #36
  (`SqliteStorage`). This issue is genuinely just `prepare`/
  `compute_hash`, the same functions `core/engine.py` actually has.
- 10 new unit tests (undeclared-kind rejection, declared-kind
  acceptance, `deleted` gated on `supports_native_deletes`, media gated
  on `produces_media`, `revision_token` short-circuiting the projection
  hash for both matching and differing tokens, volatile-field stripping,
  tag-order independence, the `deleted` flag participating in the hash,
  timestamp formatting), 108/108 total passing across the workspace.

## Engine — cursor/checkpoint transaction safety (closes #16)
**2026-08-12**

- **Added:** `dbs-core::engine::commit_checkpoint` — persists buffered
  items before saving the new cursor, never the reverse, mirroring
  invariant #1 in `docs/architecture.md`'s "Anatomy of a backup run"
  (`src/dbs/core/engine.py`, baileyrd/Daily-Backup-System, pinned
  `@6cc6491`): "the cursor never gets ahead of data." A crash between the
  two calls leaves the cursor lagging committed data (safe — the next
  run re-fetches the overlap and idempotent upsert, #17, dedups it) and
  never advances the cursor past data that was never durably written.
- **Design note:** kept as its own `engine` module rather than folded
  into the `Storage` trait, matching the reference's own
  `dbs.core.engine`/`dbs.storage.base` boundary — `Storage`'s trait
  surface (#11) is unchanged. The reference wraps both calls in one DB
  transaction for atomicity *within* the upsert batch itself; this
  round's `Storage` trait deliberately has no `transaction()` combinator
  (#11's scope note), so that stronger guarantee is left to the concrete
  `SqliteStorage` (new issue #36, filed this session) if it proves
  necessary — the ordering invariant holds either way.
- **Found and filed a sequencing gap:** #12 was scoped to schema +
  connection setup only, not an actual `Storage` implementation — there's
  no real backend to persist through yet. Filed #36 to close it. The
  engine issues don't block on it; they're tested against a `Storage`
  test double instead, same pattern as #11's `InMemoryStorage`.
- 4 new unit tests (items-then-cursor ordering, a simulated crash between
  the two calls leaving the cursor lagging not ahead, a recovered run
  after that crash succeeding normally, watermark derivation from the
  committed batch's `max_updated_at`), 98/98 total passing across the
  workspace.

## netns — VPN network-namespace membership check (closes #24)
**2026-08-12**

- **Added:** `dbs-core::netns` — `named_netns_exists`/`in_named_netns`,
  mirroring `src/dbs/core/netns.py` in baileyrd/Daily-Backup-System
  (pinned `@6cc6491`). Guards `requires_vpn` sources against backing up
  outside the VPN wrapper's network namespace (which would leak traffic
  via the host's real IP) by comparing `(device, inode)` of
  `/proc/self/ns/net` against the named-netns bind mount — the same check
  `ip netns identify` does.
- Genuinely Linux-only, confirmed by reading the reference directly (not
  assumed from the module name) back when this was filed. The reference
  reaches its non-Linux degradation implicitly (the `/proc`/`/run` paths
  simply don't exist off Linux, so `os.stat` raises and gets caught);
  this port makes that explicit via `#[cfg(target_os = "linux")]` instead
  of relying on path-not-found as the portability strategy.
- 4 new unit tests (empty name disables both checks, a nonexistent
  namespace doesn't exist, not-in-a-nonexistent-namespace, a
  platform-specific test confirming the Linux path does a real
  `stat`-based comparison rather than short-circuiting), 94/94 total
  passing across the workspace.

## Shared connector watchdog/timeout helper (closes #14)
**2026-08-12**

- **Added:** a new workspace crate, `dbs-connector-support`, and its
  first module, `watchdog` (`run_with_watchdog`/`WatchdogTimeout`/
  `WatchdogError`), mirroring `src/dbs/connectors/_util.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). Real crate boundary,
  not just a doc convention — the reference's own docstring separates
  "core public contract" from "connector implementation detail," and
  ADR-0001's subprocess connectors will link against this, not
  `dbs-core`. First connector-side shared code.
- Abandons a stalled worker thread past its deadline the same way the
  reference does (Rust threads can't be force-killed either), but
  distinguishes the call's own error (`WatchdogError::Inner`) from a
  timeout (`WatchdogError::Timeout`) and a worker panic
  (`WatchdogError::WorkerPanicked`) as three separate cases — cleaner
  than the reference's box-and-reraise pattern, which Rust's type system
  doesn't really have an equivalent for anyway.
- **Deliberately not ported:** `impersonate_target` (yt-dlp/`curl_cffi`
  TLS-fingerprint tuning) — round-1's browser-automation decision has
  `rusty_dbs` shell out to the yt-dlp *binary*, not call its Python
  library API, so this Python-library-specific helper has no Rust
  equivalent to write. `ext_for_mime` is deferred to whichever media/
  export issue actually needs it.
- 7 new unit tests (zero-timeout inline execution, fast completion,
  inner-error propagation, wall-clock timeout without a heartbeat, an
  active heartbeat preventing timeout during healthy long-running work,
  worker-panic handling, timeout message wording), 90/90 total passing
  across the workspace.

## Config loading (closes #13)
**2026-08-12**

- **Added:** `dbs-core::config` — `Config`, `SourceConfig`,
  `ConnectorOverride`, `VpnGuard`, `NotifyOn`, `load_config`,
  `parse_env_file`, mirroring `src/dbs/config.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). Parsing pipeline
  matches the reference's order deliberately: reject inline secrets
  *before* `${ENV}` expansion, then expand, then extract into the typed
  structs.
- **New dependency:** `toml`.
- **Scoped narrower than the reference:** TOML only, no YAML path (the
  reference's is `pyyaml`-gated and optional there too).
  `SourceConfig.export` (`ExportProfileOverride`) isn't ported.
- **Found and recorded a gap-analysis miss:** `core/export_profile.py`
  was never given its own row — referenced by both `connector.py` and
  `config.py` but not captured when the original 66-row table was built.
  Added to `gap-analysis.md`'s Config & secrets section; both
  `connector.rs` and `config.rs` currently omit the field pending it.
- 12 new unit tests (defaults, missing file, inline-secret rejection,
  `*_env` key allowed, missing `type`, env expansion (set + unset),
  invalid `vpn_guard`, registry override translation, download-dir
  joining, `.env` parsing with comments/export/quotes, missing `.env`),
  83/83 total passing.

## SQLite storage — schema + migrations (closes #12)
**2026-08-12**

- **Added:** `dbs-core::storage::migrations` — the 6 ordered migrations
  (`schema_migrations`, `sources`, `sync_runs`, `items`, `item_revisions`,
  `media`, `sync_state`, `source_locks` tables and their indexes) and a
  `migrate()` runner, ported verbatim from `src/dbs/storage/migrations.py`
  in baileyrd/Daily-Backup-System (pinned `@6cc6491`). Each migration
  commits atomically with its `schema_migrations` bookkeeping row via
  `BEGIN IMMEDIATE`, re-checking applied versions after acquiring the
  write lock — same race-safety the reference calls out for concurrent
  callers opening the same not-yet-migrated database.
- **Added:** `dbs-core::storage::sqlite::open_connection` — connection
  setup matching `SqliteStorage._configure`'s pragmas exactly
  (`journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`,
  `busy_timeout=30000`), plus parent-directory creation and minimal `~`
  expansion for file paths.
- **New dependency:** `rusqlite` (`bundled` feature — compiles SQLite
  from source rather than depending on a system library, so CI and a
  future Windows build don't need to locate one).
- **Scoped narrower than the reference:** this is schema + connection
  setup only, not a full `Storage` implementation — the upsert/query
  methods land with the engine issues (#16/#17/#19/#20) building on top.
  In-memory path handling is also simplified: URI query params like
  `?cache=shared` aren't honored, unlike the reference's plain
  `sqlite3.connect(path)`.
- 11 new unit tests (all migrations apply to a fresh DB, idempotent
  re-run, every expected table exists, migration-0002 columns are
  actually usable via real inserts, schema version, statement splitting,
  pragmas set correctly, memory-path variants, parent-directory creation,
  `~` expansion), 70/70 total passing.

## Storage trait (closes #11)
**2026-08-12**

- **Added:** `dbs-core::storage` — the `Storage` trait plus
  `PreparedItem`, `BatchResult`, `ItemRow`, `SourceRecord`, mirroring
  `src/dbs/storage/base.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`). Trait-only, no concrete backend — that's #12, which is
  also where `rusqlite` gets added; this issue needed no new dependency.
- **Scoped deliberately narrower than the reference in two ways,
  documented in the module doc-comment:**
  - `iter_items`/`iter_revisions`/`browse_items` take an `ExportQuery`
    defined locally as a minimal placeholder, not the reference's real
    `export/base.py::ExportQuery` (its own gap-analysis row) — superseded
    once that lands.
  - The reference's `transaction()` context-manager method isn't ported;
    Rust has no direct trait-object-safe equivalent for an RAII guard
    over an unknown concrete connection type without more design work
    than this issue covers. Atomicity for a batch write is instead each
    such method's own responsibility in the concrete (#12)
    implementation.
- **Changed:** `DbsError` gains a `Storage(String)` variant — the
  reference lets backend exceptions propagate unchecked, but `Storage`'s
  methods need a concrete error type for their `Result` signatures.
- 6 new unit tests (`BatchResult::merge` counting and `max_updated_at`
  precedence, an `InMemoryStorage` test double proving the trait is
  object-safe and round-trips a source, default `maintain`/
  `prune_revisions`/`vacuum_into`/`spawn` behavior matching the
  reference's defaults), 59/59 total passing.

## Cooperative cancellation + RunContext catch-up (closes #10)
**2026-08-12**

- **Added:** `dbs-core::cancel::CancelToken` — a thread-safe, one-way
  cooperative cancellation signal, mirroring `src/dbs/core/cancel.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). Backed by
  `Arc<AtomicBool>` rather than the reference's `threading.Event` —
  cloning a token shares the same underlying flag, so one token is safe
  to hand to every `--parallel` worker thread, same guarantee as the
  reference.
- **Changed:** `RunContext` gains `secrets: Secrets` and
  `cancel: Option<CancelToken>`. `secrets` should have landed when #6
  merged but was missed — caught up here rather than left drifting.
  `http` (#22) and a logger equivalent are still the only pieces left
  before `RunContext` fully matches the reference.
- 7 new unit tests (uncancelled start, cancel sets/idempotent, clones
  share state, independent tokens don't, cross-thread visibility,
  `RunContext.cancel` reflects a shared token), 53/53 total passing.

## CORE_API_VERSION gating (closes #9)
**2026-08-12**

- **Added:** `dbs-core::versioning` — `CORE_API_VERSION`,
  `CURRENT_API_VERSION` (alias, matching the reference's two-name split),
  `is_api_compatible`, mirroring `src/dbs/core/versioning.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`).
- **Changed:** `connector::CORE_API_VERSION` (a placeholder defined
  directly in #4, before this module existed) is now a re-export from
  `versioning` instead of its own definition — one source of truth, same
  public path (`dbs_core::CORE_API_VERSION` still works).
- 3 new unit tests (same-version compatible, different-version
  incompatible, alias equality), 46/46 total passing.

## Timeutil helpers (closes #8)
**2026-08-12**

- **Added:** `dbs-core::timeutil` — `iso_z`/`parse_iso`, mirroring
  `src/dbs/core/timeutil.py` in baileyrd/Daily-Backup-System (pinned
  `@6cc6491`). Smaller than the reference: `chrono::DateTime<Utc>` is
  always UTC and always timezone-aware in the type system, so there's no
  naive-vs-aware branch needed on the formatting side — only on parsing
  untrusted input text, where a naive (no-offset) string is still treated
  as UTC, same as the reference.
- 8 new unit tests (zero/nonzero fractional seconds, `None`/empty/
  whitespace input, `Z` suffix, explicit offset conversion, naive-as-UTC,
  garbage rejection, round-trip), 43/43 total passing.

## Content hashing for change classification (closes #7)
**2026-08-12**

- **Added:** `dbs-core::hashing` — `canonical_json`/`content_hash`,
  mirroring `src/dbs/core/hashing.py` in baileyrd/Daily-Backup-System
  (pinned `@6cc6491`). `serde_json::Value`'s default `Map` is a
  `BTreeMap` (this workspace doesn't enable `preserve_order`), so
  `canonical_json` gets sorted-key determinism for free from
  `serde_json::to_string` — no manual sorting needed, unlike the
  reference's explicit `sort_keys=True`.
- **New dependency:** `sha2` (SHA-256). First use of the new "small,
  narrowly-scoped standard crates are auto-approved" policy decided
  mid-implementation — see `gap-analysis.md`'s Decisions section, item 5.
- 6 new unit tests (key sorting at every nesting level, compact
  separators, non-ASCII kept unescaped, order-independence, real-change
  detection, digest shape), 35/35 total passing.

## Least-privilege secrets accessor (closes #6)
**2026-08-12**

- **Added:** `dbs-core::secrets::Secrets` — a read-only, allow-listed view
  over a secret store, mirroring `src/dbs/core/secrets.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`): `get`/`get_optional`
  reject an undeclared key with `ConnectorError::Contract`, a declared but
  missing/empty key with `ConnectorError::Auth`; `require_all` pre-flights
  every declared key at once. 8 new unit tests, 29/29 total passing.

## ADR-0001: dynamic plugin registry via subprocess + JSON IPC (closes #5)
**2026-08-12**

- **Added:** `docs/adr/0001-dynamic-plugin-registry.md`, replacing the ADR
  seed template with the first real decision. Proposes subprocess + line-
  delimited JSON IPC for connector loading (each connector a separate
  `dbs-connector-<type>` executable, a handshake self-describing its
  contract, a manifest-based registry) instead of a `cdylib` + stable-ABI
  approach — Rust's lack of a stable ABI makes the `cdylib` path a much
  higher-risk lockstep-versioning/UB problem than a subprocess boundary,
  which only needs a stable *wire* protocol.
- **Known limitation:** this is a proposal (`Status: Proposed`), not yet
  accepted or implemented. The registry implementation itself is a
  follow-up issue once this ADR is reviewed.

## Connector plugin contract + partial RunContext (closes #4)
**2026-08-12**

- **Added:** `dbs-core::connector` — the `Connector` trait and a first-pass
  `RunContext`, mirroring `src/dbs/core/connector.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). `fetch` returns
  `Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>>>` rather than
  an associated type, keeping `Connector` object-safe (`Box<dyn
  Connector>`) — a deliberate head start on issue #5's dynamic-plugin-
  loading design, which needs trait objects across a `cdylib` boundary.
- **Known limitation, scoped deliberately:** `RunContext` omits
  `secrets`/`http`/`cancel`/`logger` — those depend on #6, #22, #10, none
  of which exist yet. It carries only `source_id`/`source_name`/`cursor`/
  `since`/`run_id`/`mode`/`limit`/media options/`items_failed` for now;
  grows to match the reference once those land.
- 5 new unit tests (default-method values, a `FakeConnector` exercising
  `fetch`, object-safety, `report_failed` accumulation, `ReconcileMarker`
  round-trip through `FetchEvent`) — 21/21 total passing.

## Core error hierarchy (closes #3)
**2026-08-12**

- **Added:** `dbs-core::errors` — `DbsError`, `ConnectorLoadError`,
  `ConnectorError`, `BackupRunError`, mirroring `src/dbs/core/errors.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). The reference uses an
  exception *class* hierarchy (`RateLimitedError` subclasses
  `TransientFetchError` so one `except` catches both); Rust has no
  subclassing, so that relationship is a classification method,
  `ConnectorError::is_retryable()`, instead of nested variant matching —
  same semantics as the reference, idiomatic shape for the language. 5 new
  unit tests, all green.

## Cargo workspace scaffold + core data model (closes #2)
**2026-08-12**

- **Added:** the first Rust code in this repo — a Cargo workspace with a
  `dbs-core` crate, mirroring `dbs.core.models`/`dbs.core.capabilities` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`): `BackupItem`, `MediaRef`,
  `Cursor`, `Checkpoint`, `ReconcileMarker`, `FetchEvent`, `RunResult`,
  `RunStatus`, `ProgressEvent`/`ProgressPhase`, `SourceStatus`,
  `ConnectorInfo`, `VerifyIssue`/`VerifyReport`, `DoctorCheck`,
  `MaintenanceReport`, `RestoreReport`, `Capabilities`, `ItemKind`,
  `AuthCapture`. 11 unit tests, all green; `cargo clippy -D warnings` and
  `cargo fmt --check` clean.
- **Added:** `.github/workflows/ci-rust.yml` (fmt --check, clippy -D
  warnings, test) now that a manifest exists — repo-config's audit had
  correctly skipped it until now.
- **Deliberately deferred:** `RunContext` (the reference's per-run injected
  context) isn't implemented yet — it depends on `Secrets`/
  `ManagedHTTPClient`/`CancelToken`, which don't exist yet (separate
  issues). It belongs with the connector trait (#4), not the plain data
  model.
- New dependencies: `serde` + `serde_json` + `chrono` (with `serde`
  feature) — all pre-approved in `gap-analysis.md`'s foundational-dependency
  decision.

## Add parity-loop gap analysis against Daily-Backup-System
**2026-08-12**

- **Added:** `gap-analysis.md` — a full-feature-parity assessment against
  [baileyrd/Daily-Backup-System](https://github.com/baileyrd/Daily-Backup-System)
  (pinned `@6cc6491`), produced by the `parity-loop` skill. 66 rows across
  core/engine, storage, config, crypto, exports, restore/maintain, CLI, web
  tier, research, and 14 connectors, since the reference has no comparable
  Rust surface to diff and this repo has no roadmap doc of its own yet.
- **Decided (user-confirmed):** full feature parity is the round-1 scope;
  cross-platform floor (Linux + Windows) from round 1; foundational
  dependencies (SQLite, TOML/JSON, CLI parsing, HTTP, async runtime, zip,
  crypto) via standard external crates; the connector plugin registry via
  true dynamic loading (its own ADR-first issue, not a straight port);
  browser-automation connectors (reddit/skool/youtube) and the research
  subsystem both shell out to existing Python tooling (yt-dlp/Playwright,
  and [gemini-notebook-mcp-cli](https://github.com/jacob-bd/gemini-notebook-mcp-cli)
  for NotebookLM) rather than reimplementing browser automation in Rust.
- **Known limitation:** the RustyMill sibling check (`rusty_db`,
  `rusty_json`, `rusty_http`, etc.) is name/purpose-only, not
  source-verified — this session can't attach `Rusty-Mill/*` repos
  (cross-owner restriction, already holds `baileyrd`-owned repos). Real
  verification needs a session that can reach that org, done per-issue in
  step 3, not assumed from the table.

## Replace hand-reconstructed PR/issue templates with the real source
**2026-08-12**

- **Fixed:** swapped the four hand-reconstructed PR templates and two
  hand-reconstructed issue templates for the actual files from
  `baileyrd/skill_pack` (`my_loops/repo-config/assets/templates/.github/`,
  commit `ae532fb`), now that this session has that repo cloned. The
  reconstructions from the previous entry turned out to differ meaningfully
  from source, not just cosmetically — most notably the issue templates are
  GitHub issue-form YAML (`bug_report.yml`, `feature_request.yml`) with
  structured fields, not the plain Markdown-with-frontmatter files guessed
  from the changelog description. `config.yml` also gained a
  `Security vulnerability` contact link (pointing at this repo's GitHub
  Security Advisories) and `blank_issues_enabled: false`, both present in
  source and absent from the reconstruction.
- **Reported upstream:** filed
  [baileyrd/skill_pack#1](https://github.com/baileyrd/skill_pack/issues/1)
  documenting this as (at least) a third occurrence of the sync-gap pattern
  already logged twice in that repo's own `RELEASE_NOTES.md` — the local
  `synced/repo-config` copy was missing `assets/templates/.github/` entirely
  and had lost the executable bit on both scripts, while the source repo
  itself is confirmed correct on both counts.
- CI workflow still correctly absent — no `Cargo.toml` yet to run it against.

## Apply repo-config governance scaffold
**2026-08-12**

- **Added:** initial governance file set via the `repo-config` skill — README,
  CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, CHANGELOG, RELEASE_NOTES (this file),
  ARCHITECTURE, an ADR seed, four PR templates, and two issue templates + config.
- **Context:** repo was fully empty (no commits, no manifest, no branches) except
  for a configured `git remote origin` — so `{{OWNER_REPO}}` (`baileyrd/rusty_dbs`)
  and `{{SECURITY_CONTACT}}` (`baileyrd`, the repo owner) resolved for real rather
  than staying placeholders, per the skill's default-to-owner rule. Project intent
  (a Rust reimplementation of
  [baileyrd/Daily-Backup-System](https://github.com/baileyrd/Daily-Backup-System))
  came from the user, since nothing existed yet to infer it from.
- **Known limitation, stated rather than hidden:** the `.github/PULL_REQUEST_TEMPLATE/`,
  `.github/ISSUE_TEMPLATE/`, and CI-workflow assets were missing from this session's
  locally synced copy of the `repo-config` skill — a documented recurring sync gap
  (see the skill's own `RELEASE_NOTES.md`, "Record a sync-gap finding"). Pulling the
  canonical versions from the skill's source repo (`baileyrd/skill_pack`) was blocked
  by this session's repo-access scope, so the PR and issue templates here were
  hand-reconstructed from that same source file's description of their contents
  rather than copied verbatim — worth a diff against `skill_pack` once this session
  has access, to confirm they match. CI workflow was correctly skipped (no manifest
  yet to run against), so that particular gap didn't matter this time.
- No Rust code has landed yet — `ARCHITECTURE.md`'s boundary table and README's
  Getting Started section are left as scaffolding on purpose; there's nothing real
  to put in them until the first slice of the reimplementation exists.
