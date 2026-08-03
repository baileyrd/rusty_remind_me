# Gap Analysis — `rusty_remind_me` vs. `remind_me`

**Run date:** 2026-08-03 (re-verification of the 2026-08-02 run)
**Target:** `baileyrd/rusty_remind_me` @ `a2cce8b`, 46,374 LOC Rust, 5 crates
**Reference (pinned):** `baileyrd/remind_me` @ `935eb98` — **v1.54.0**, 79,893 LOC Python
**Previous run:** 2026-07-29 against target `de891ed` / reference v1.19.0. That
analysis is superseded in full — every one of its four blockers is resolved and
its entire gap table has been worked.

### Re-verification, 2026-08-03

This run re-derived the headline numbers independently rather than trusting the
2026-08-02 table, and re-confirmed the highest-value claim (S5) against both
codebases. Every figure below reproduced exactly:

| Check | Method | Result |
| --- | --- | --- |
| Reference tools = 61 | AST-ish scan of `@mcp.tool(name=…)` across `remind_me_mcp/` | confirmed |
| Target tools = 44 | `"name": "remind_me_*"` in `crates/remind_me_mcp/src/lib.rs` | confirmed — 43 shared, `remind_me_wiki_import` target-only, 18 missing |
| Reference routes = 25 / peer 7 | `api.py` `routes = [...]`, `peer_server.py` path dispatch | confirmed |
| Target routes = 21 / peer 6 | `crates/remind_me_api/src/` | confirmed — 5 missing, `/api` target-only, peer missing `/count` |
| Schema 27 vs. 19 | `db.py:462` vs. `db/migrations.rs:46` | confirmed |
| **S5 is a live defect** | `schema_triggers.sql:60` has only the `sync_enabled` guard, no `NEW.updated_at IS NOT OLD.updated_at`; `vitality.rs:534` does `UPDATE memories SET accessed_at = …` on read | **confirmed — every read enqueues an outbox row** |

**Target movement since the last run: none.** `main` advanced only by the two
documentation commits that recorded that analysis. All 23 filed issues
(#100–#122) are still open and unworked, so the gap table stands unchanged
apart from the delta below.

**Reference movement since the last run: one commit.** `9ca9844` → `935eb98`,
*"maintenance: cap contradiction-candidate pairing at 20 mentions/entity."*
It touches only `maintenance.py`, and only the SQL that backs gap **T6**
(issue [#110](https://github.com/baileyrd/rusty_remind_me/issues/110)) — see
that row's notes. No new tool, route, table, or schema step; the version stays
v1.54.0 and the schema stays v27.

**Assessment path:** `spec`. The two codebases share no structurally diffable
surface (Python package vs. Cargo workspace), so `cargo public-api` does not
apply. Candidates were extracted from the reference's declared contracts:
`@mcp.tool(name=…)` registrations, the Starlette route table, the pydantic input
models, and — for the schema — by *executing* both schema definitions into real
SQLite databases and diffing `sqlite_master` object-by-object rather than
comparing table names by eye.

**Scope definition:** `rusty_remind_me` still has no `ROADMAP.md`.
`ARCHITECTURE.md` §1 Tenet 3 remains the hand-curated scope statement and is
treated as the definition of parity for this run:

> **Data Parity with `remind-me`**: Identical SQLite Version 19 schema and JSON
> tool signatures for drop-in interoperability.

Note that the tenet's own version number is now stale — the reference is at
schema **v27**. Updating that sentence is part of gap **S1** below.

`remind_me`'s `BACKLOG.md` is the reference's own improvement backlog, not a
roadmap for the port, and is again deliberately **not** used as the scope source.

---

## Status of the previous run's blockers

All four are cleared; the autonomous half of the loop can run this time.

| # | Previous blocker | Now |
| --- | --- | --- |
| B1 | Workspace did not build (missing `../Rusty_Mill/*` path crates) | **Resolved.** `cargo check --workspace` is clean. The Rusty Mill path dependencies were removed from the manifest. |
| B2 | Rusty Mill expected as a monorepo | **Resolved** by the same manifest change. |
| B3 | No CI | **Resolved.** `.github/workflows/ci.yml` runs build / test / clippy / fmt with `RUSTFLAGS: -D warnings` on push and PR to `main`. |
| B4 | No `RELEASE_NOTES.md`, templates, labels | **Resolved.** `RELEASE_NOTES.md` (91 KB), `CONTRIBUTING.md`, `.github/`, and 12 ADRs under `docs/adr/` are all present. |

Open `parity-gap` issues at the start of this run: **0**. The previous gap list
was worked to completion (PRs #28 → #99).

---

## Headline numbers

| Surface | `remind_me` v1.54.0 | `rusty_remind_me` | Covered |
| --- | --- | --- | --- |
| MCP tools | 61 | 44 (43 shared + 1 target-only) | **70%** |
| HTTP API routes | 25 | 20 (all shared) | **80%** |
| Peer-server routes | 7 | 6 | **86%** |
| SQLite tables | 30 | 25 | **83%** |
| SQLite indexes | 36 | 30 | **83%** |
| SQLite triggers | 15 | 15 | **100%** (one body differs — see S4) |
| Schema version | 27 | 19 | **8 steps behind** |
| Import formats | 13 extensions | 5 | **38%** |

The port went from 16% to 70% tool coverage in the last run. What remains is
almost entirely *new* reference work landed since v1.19.0 — reminders,
saved searches, edit history, analytics, and the hub — rather than leftovers.

---

## Gap table

### Correctness — the schema-parity claim, again

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **S1** | `SCHEMA_VERSION = 19` | const (existing) | spec | both | `db.py:462` `_SCHEMA_VERSION = 27` | **yes** | M | The stamp is *honest* now — the generated schema really is v19 — so this is no longer the "actively breaks interop" bug the last run flagged. But it is 8 steps stale. A DB created by `rusty_remind_me` opens in `remind_me` and gets migrated 19→27 correctly; the reverse direction is the problem — a v27 DB opened by this crate hits the reconciler with 5 tables and 2 `memories` columns it does not know about. Fix is to regenerate `schema_*.sql` from a v27 reference dump and bump the constant, which changes existing DB-creation behavior. Also update `ARCHITECTURE.md` Tenet 3's "Version 19" text. |

### Schema — missing objects (exact, from an executed diff)

Both schemas were materialized into SQLite and diffed via `sqlite_master`.
These are the complete differences; nothing else diverges.

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **S2** | `memories.remind_at`, `idx_memories_remind_at`, `reminder_deliveries`, `idx_reminder_deliveries_memory_remind_at` | column + table + index | spec | both | `db.py` `_migrate_v22_to_v23` (issue #179) | no | S | Backs the whole reminders subsystem (T1). |
| **S3** | `memories.sensitive` | column | spec | both | `db.py` `_migrate_v25_to_v26` (issue #195) | no | S | A "don't surface by default" flag, explicitly *not* access control. Also adds `include_sensitive` / `sensitive` to three tool signatures — see T9. |
| **S4** | `sync_log.last_pull_at`, `.last_push_at`, `.last_attempt_at` | columns | spec | both | `db.py` `_migrate_v19_to_v20` (SY-18) | no | S | Splits the sync cursor from the liveness clock. The target currently overloads `last_pull`/`last_push` for both, so a stalled peer is indistinguishable from an idle one. Prerequisite for `remind_me_sync_status` (T5). |
| **S5** | `memories_outbox_au` trigger body | trigger (existing) | spec | both | `db.py` `_migrate_v21_to_v22` (issue #147) | no | S | The reference guards the trigger with `AND NEW.updated_at IS NOT OLD.updated_at`; the target's fires on every update. Because the target *does* record access (`accessed_at`/`access_count` writes, PR #42), every memory read currently enqueues a sync-outbox row. This is a live defect, not just a missing feature: it inflates the outbox and pushes no-op updates to peers. Smallest high-value fix in this list. |
| **S6** | `idx_memories_normalized_from` | index | spec | both | `db.py` `_migrate_v20_to_v21` | no | S | Indexes the `normalized_from` JSON pointer. Pure performance; the target's normalize tools already write the pointer. |
| **S7** | `memory_revisions`, `idx_memory_revisions_memory_edited` | table + index | spec | both | `db.py` `_migrate_v23_to_v24` (issue #187) | no | S | Backs `remind_me_history` / `remind_me_revert` (T4). |
| **S8** | `analytics_snapshots`, `idx_analytics_snapshots_captured_at` | table + index | spec | both | `db.py` `_migrate_v24_to_v25` (issue #186) | no | S | Backs the analytics trend route (T7 / A1). |
| **S9** | `saved_searches`, `saved_search_seen_memories`, `idx_saved_search_seen_memories_search_memory` | tables + index | spec | both | `db.py` `_migrate_v26_to_v27` (issue #194) | no | S | Backs the four saved-search tools (T3). |
| **S10** | outbox payload fields `remind_at`, `sensitive` | trigger (existing) | spec | both | `db.py` `_outbox_payload_sql` | no | S | Follows S2/S3 — the `memories_outbox_ai`/`au` payloads must carry the new columns or synced peers silently drop them. Do this in the same change as S2/S3, not separately. |

### MCP tools — 18 missing of 61

Grouped for issue sizing. Each group is one issue unless the Notes say otherwise.

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **T1** | `remind_me_set_reminder`, `remind_me_list_reminders`, `remind_me_reminders_ics_url` | tool | spec | both | `tools/reminders.py`, `reminders.py`, `scheduler.py`, `notifications.py`, `ics_export.py` (issues #179, #180) | no | L | Time-based reminders: `remind_at` on a memory, a delivery-tracking scheduler loop, optional outbound notification channels, and an iCalendar feed. **Split into three issues** — (a) storage + `set_reminder`/`list_reminders` over S2, (b) the scheduler/delivery loop, (c) ICS export + its route (A2). The notification channels are a likely dependency question. |
| **T2** | `remind_me_sync_status`, `remind_me_sync_repair`, `remind_me_sync_reconcile`, `remind_me_sync_reconcile_peer` | tool | spec | both | `tools/admin.py:1194–1360` (SY-12, SY-14, issues #215, #216) | no | M | Sync observability and repair over the sync stack the target already has. `sync_status` needs S4. `reconcile`/`reconcile_peer` need the `/count` endpoint (A5). **Split into two issues**: status+repair, then the two reconcile tools. |
| **T3** | `remind_me_save_search`, `remind_me_list_saved_searches`, `remind_me_run_saved_search`, `remind_me_delete_saved_search` | tool | spec | both | `saved_searches.py`, `tools/saved_searches.py` (issue #194) | no | M | Saved and watched searches; `saved_search_seen_memories` is what makes "watch" report only new hits. Depends on S9. |
| **T4** | `remind_me_history`, `remind_me_revert` | tool | spec | both | `tools/history.py` (issue #187) | no | M | Per-memory edit history and rollback. Depends on S7. Note the revision row must be written by the *existing* update/reclassify/normalize paths, so this touches more than a new handler. |
| **T5** | `remind_me_digest` | tool | spec | both | `digest.py` (issue #188) | no | M | Vault digest synthesis over a time window. 372 LOC in the reference. |
| **T6** | `remind_me_contradiction_candidates` | tool | spec | both | `tools/contradictions.py`, `maintenance.py:207` | no | M | Surfaces same-subject/same-predicate/different-object triples. The target's `entity.rs:836` already documents this exact contradiction rule for supersession — the detection logic is largely present, the reporting surface is not. **Updated 2026-08-03:** the reference's pairing SQL now excludes entities mentioned by more than `CONTRADICTION_CANDIDATE_MAX_ENTITY_FANOUT` (20) memories, on both sides of the self-join. Without the cap the join is quadratic in an entity's mention count — on the reference author's vault a single 745-mention project entity produced 277,140 of 372,750 pairs (74%), nearly all of them "these two both mention the same project" rather than genuine contradictions. Port the cap *with* the tool, not as a follow-up; a naive port ships the pathological queue this commit exists to remove. |
| **T7** | `remind_me_recalibrate_candidates` | tool | spec | both | `tools/recalibrate.py` | no | S | Proposes vitality/decay corrections. Smallest of the tool gaps at 126 LOC. |
| **T8** | `remind_me_undo_import` | tool | spec | both | `tools/admin.py:1040` | no | M | Rolls back an import by `import_id`, removing its memories and tracking rows. The target already has all four import ledgers (`chat_imports`, `dbs_imports`, `mempalace_imports`), so this is additive. |
| **T9** | `remind_me_api_key` | tool | spec | both | `api_keys.py` (issue #185) | no | M | Named, scope-limited (read vs. read-write) dashboard API keys, stored as SHA-256 hashes in a 0600 JSON file under `MEMORY_DIR`. The target's `remote.rs` already implements the same hash-at-rest discipline for connector tokens, so the conventions exist. **Security-relevant** — port the scope enforcement, not just the issuance. |
| **T10** | `remind_me_server_status` (depth) | tool (existing) | spec | both | `pid.py` `get_server_status`, `version.py` (issue #207) | no | S | **Corrected 2026-08-03 while working #104.** This row previously said the reference reports the installed version in *both* `remind_me_stats` and `remind_me_server_status`. It does not: `pid.py:176` puts `version` in `get_server_status`'s payload and `admin.py:622` prints it, while `remind_me_stats` (`admin.py:408`) has no version field anywhere. Only the status half is a real gap. The peer-version half of the original row is real but surfaces through `remind_me_sync_status`, so it belongs to T2a / #114, not here. |

### Tool signature parity — 4 fields

The last run compared tool *names*. This run diffed the pydantic input models
against the Rust structs field-by-field; these are the only divergences.

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **T11** | `MemorySearchInput.include_sensitive`, `MemoryListInput.include_sensitive`, `MemoryAddInput.sensitive` | field | spec | both | `models.py` | no | S | Follows S3. Additive with a `false` default, so no existing caller changes behavior. |
| **T12** | `MemorySearchInput.strategy` | field | spec | both | `models.py:34` `RetrievalStrategy` | no | S | The target *has* the `RetrievalStrategy` enum (`models.rs:15`) and made the RRF weights configurable in PR #91 — but only via environment variables. The reference exposes it as a per-call parameter with an `AUTO` heuristic router. Wiring the existing enum into the existing input struct is most of the work. |

### HTTP API — 5 missing of 25, plus one target-only route

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **A1** | `/api/analytics/trend` | route | spec | both | `api.py`, `analytics.py` (issue #186) | no | M | Depends on S8. |
| **A2** | `/api/reminders/{token}.ics` | route | spec | both | `api.py`, `ics_export.py` | no | M | Token-authenticated iCalendar feed. Part of T1(c). |
| **A3** | `/api/versions` | route | spec | both | `api.py` (issues #207, #211, #221) | no | S | Reports the serving build; the dashboard header reads it. Recent reference work — landed in v1.53.0/v1.54.0. |
| **A4** | `/metrics`, `/manifest.json` | route | spec | both | `metrics.py` (issue #197), `api.py` | no | M | Prometheus-format exposition plus the PWA manifest. `/metrics` also exists on the hub. |
| **A5** | `/count` on the peer server | route | spec | both | `peer_server.py` (issues #214, #216, #217) | no | M | With `?approx=1` for O(1) polling, `?since=` and `?by=origin_node` filters. This is the pre-check that makes `remind_me_sync_reconcile*` (T2) cheap — file it before T2. |
| **A6** | ~~`/api` (target-only)~~ | — | spec | both | — | no | — | **Withdrawn 2026-08-03 while working #107: this was a false positive.** There is no `/api` route. The extraction that produced this row grepped `"/api"` string literals across `crates/remind_me_api/src/`, and matched a doc comment — `routes.rs`'s note that the vendored dashboard talks to `window.location.origin + "/api"`. The registered route table (`ROUTES` in `routes.rs`) contains 20 patterns, every one of which the reference also serves. The target therefore has **no** target-only routes, and the headline table's "21 (20 shared + 1 target-only)" should read **20 shared, 5 missing**. |

### Import-format depth — 8 extensions missing of 13

`remind_me_import_directory` exists on both sides; the reference dispatches to
five format handlers the target does not have. These are *not* separate MCP
tools — they are wired into the existing importer, so each is additive depth on
a tool that already ships.

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **I1** | `.pdf` | fn | spec | both | `pdf_import.py` | no | M | 113 LOC. Almost certainly a **new third-party dependency** (PDF text extraction) → stop-and-ask. |
| **I2** | `.png`, `.jpg`, `.jpeg` | fn | spec | both | `image_import.py` | no | M | OCR/vision path. **New dependency** → stop-and-ask. |
| **I3** | `.mp3`, `.m4a`, `.wav`, `.ogg` | fn | spec | both | `audio_import.py` | no | M | Transcription path. **New dependency** → stop-and-ask. |
| **I4** | Obsidian vault import | fn | spec | both | `obsidian_import.py` (FT-31) | no | L | 425 LOC. Wikilink-aware vault ingestion — the target's `wiki_links` table and `wiki_fs.rs` make this a better fit here than the other four. No new dependency expected. |
| **I5** | Readwise connector | fn | spec | both | `readwise_import.py` | no | M | Highlights import over the Readwise API. Network connector. |

### Subsystems with no target equivalent

Each is a multi-issue epic, not a single gap. Listed so the omission is visible.

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **E1** | Sync hub (Postgres) | subsystem | spec | both | `hub/` — 1,341 LOC + Containerfile, Quadlet units, compose/fly/railway deploy | no | L | A separate deployable FastAPI+Postgres service the nodes sync through, with `/count`, `/metrics`, `/admin/compact_tombstones`, versioned images, and deploy verification. The target's sync client already speaks the node-side protocol, so the *client* side is largely covered — the hub itself is a new artifact and a scope decision, not obviously in a Rust port's remit. |
| **E2** | Rate limiting | subsystem | spec | both | `rate_limit.py` (issue #183) | no | M | In-memory, dependency-free limiter protecting the webhook ingest and remote MCP endpoints. The target exposes both surfaces with no limiter. **Security-relevant** given both can be tunneled publicly. |
| **E3** | Tool profiles | subsystem | spec | both | `tool_profiles.py` | no | M | `full` / `standard` / `core` profiles that prune the advertised tool surface. With 44 tools the target has the same context-cost problem the reference wrote this for. |
| **E4** | Maintenance nudges | subsystem | spec | both | `maintenance.py` | no | M | Pending-work counts, nudges, capture health. 475 LOC. |
| **E5** | Automation event stream | subsystem | spec | both | `events.py` (issue #198) | no | M | Raw event stream for memory mutations. |
| **E6** | Cloud backup | subsystem | spec | both | `cloud_backup.py` (issue #196) | no | M | Optional cloud upload of local backups. The target has `backup.rs`; this is the upload leg. Likely a new dependency. |
| **E7** | Sidecar process management | subsystem | spec | both | `sidecars.py` | no | M | Keeps the hub SSH tunnel and dashboard alive in a Windows Job object. Windows-specific and tied to E1 — a strong **out-of-scope** candidate. |
| **E8** | ANN index / reranker | subsystem | spec | both | `ann_index.py`, `reranker.py` | no | L | The target has embeddings and brute-force vector search (ADR 0002) but no ANN index and no reranking stage. Both are quality/scale improvements over a path that already works. New dependencies likely. |

---

## Filed issues

Scope approved 2026-08-03: **waves 1–7**. Waves 8 (import formats) and the
remaining epics are deliberately deferred — see "Deferred by decision" below.

**Progress.** Waves 1 and 2 are merged: #100 (S5) as PR #125, and #101 — which
carried S1, S2, S3, S4, S6, S7, S8, S9 and S10 together — as PR #126. Every row
in the "Correctness" and "Schema — missing objects" tables above is therefore
closed, and the downstream schema-dependent issues are unblocked.

Wave 3 is in progress: #102 (T7) merged as PR #127, #103 (T8) as PR #128,
#104 (T10) as PR #129, #105 (T11) as PR #130, #106 (T12) as PR #131;
#107 (A3, A6) is in flight and completes the wave.

| Wave | Gap IDs | Issue |
| --- | --- | --- |
| 1 | S5 | [#100](https://github.com/baileyrd/rusty_remind_me/issues/100) outbox trigger fires on every read |
| 2 | S1 + S2, S3, S4, S6, S7, S8, S9, S10 | [#101](https://github.com/baileyrd/rusty_remind_me/issues/101) regenerate schema at v27 **(breaking)** |
| 3 | T7 | [#102](https://github.com/baileyrd/rusty_remind_me/issues/102) `recalibrate_candidates` |
| 3 | T8 | [#103](https://github.com/baileyrd/rusty_remind_me/issues/103) `undo_import` |
| 3 | T10 | [#104](https://github.com/baileyrd/rusty_remind_me/issues/104) version in stats/status |
| 3 | T11 | [#105](https://github.com/baileyrd/rusty_remind_me/issues/105) sensitive-memory fields |
| 3 | T12 | [#106](https://github.com/baileyrd/rusty_remind_me/issues/106) `strategy` search parameter |
| 3 | A3, A6 | [#107](https://github.com/baileyrd/rusty_remind_me/issues/107) `/api/versions` |
| 4 | T3 | [#108](https://github.com/baileyrd/rusty_remind_me/issues/108) saved/watched searches |
| 4 | T4 | [#109](https://github.com/baileyrd/rusty_remind_me/issues/109) `history` / `revert` |
| 4 | T6 | [#110](https://github.com/baileyrd/rusty_remind_me/issues/110) `contradiction_candidates` |
| 4 | T5 | [#111](https://github.com/baileyrd/rusty_remind_me/issues/111) `digest` |
| 4 | A1 | [#112](https://github.com/baileyrd/rusty_remind_me/issues/112) analytics snapshots + trend route |
| 5 | A5 | [#113](https://github.com/baileyrd/rusty_remind_me/issues/113) `/count` on the peer server |
| 5 | T2a | [#114](https://github.com/baileyrd/rusty_remind_me/issues/114) `sync_status` / `sync_repair` |
| 5 | T2b | [#115](https://github.com/baileyrd/rusty_remind_me/issues/115) `sync_reconcile` / `_peer` |
| 6 | T1a | [#116](https://github.com/baileyrd/rusty_remind_me/issues/116) reminders 1/3 — storage + tools |
| 6 | T1b | [#117](https://github.com/baileyrd/rusty_remind_me/issues/117) reminders 2/3 — scheduler |
| 6 | T1c, A2 | [#118](https://github.com/baileyrd/rusty_remind_me/issues/118) reminders 3/3 — ICS feed |
| 7 | A4 | [#119](https://github.com/baileyrd/rusty_remind_me/issues/119) `/metrics` + `/manifest.json` |
| 7 | T9 | [#120](https://github.com/baileyrd/rusty_remind_me/issues/120) scoped API keys |
| 7 | E2 | [#121](https://github.com/baileyrd/rusty_remind_me/issues/121) rate limiting |
| 7 | E3 | [#122](https://github.com/baileyrd/rusty_remind_me/issues/122) tool profiles |

Closing all 23 takes MCP tool coverage from 44/61 to **61/61** and HTTP routes
from 20/25 to **25/25**.

## Deferred by decision

Not filed. Recorded here so the omission stays visible rather than becoming an
accident.

- **E1, the Postgres sync hub** — out of scope. It is a separate deployable in
  another language and runtime, not an addition to the Rust workspace, and the
  existing Python hub serves Rust nodes unchanged. The client-side half *is*
  in scope and is covered by #113 and #115.
- **I1, I2, I3, I5** — PDF, image/OCR, audio, and Readwise import. Each needs a
  new third-party crate and its own dependency decision.
- **I4, Obsidian vault import** — the best fit of the five (no new dependency
  expected, and `wiki_links`/`wiki_fs.rs` already exist), but deferred with the
  rest of the importer work.
- **E4–E8** — maintenance nudges, automation event stream, cloud backup,
  sidecar process management, ANN index and reranker. E7 in particular is
  Windows-specific and tied to the hub, making it a strong permanent exclusion.

## Recommended sequencing

Ordered so each wave depends only on what precedes it.

- **Wave 1 — the live defect.** S5 alone. The outbox trigger is enqueueing a row
  on every memory read. One-line guard, immediate benefit, no dependencies.
- **Wave 2 — schema to v27.** S1 (regenerate `schema_*.sql` from a v27 dump,
  bump the constant, fix the Tenet 3 text) carrying S2, S3, S4, S6, S7, S8, S9,
  S10 with it. Best done as *one* regeneration plus per-feature follow-ups
  rather than eight hand-written steps — the module docstring in
  `db/migrations.rs` explains at length why hand-transcription was abandoned.
  S1 is breaking → **stop-and-ask before starting this wave**.
- **Wave 3 — cheap tool wins over the new schema.** T7, T8, T10, T11, T12, A3,
  A6. All small, all additive, none blocked.
- **Wave 4 — features over the new tables.** T3 (needs S9), T4 (needs S7),
  T6, A1 (needs S8).
- **Wave 5 — sync observability.** A5 (`/count`) → T2. In that order.
- **Wave 6 — reminders.** T1 split three ways, plus A2. Largest coherent
  feature; the notification-channel leg needs a dependency decision.
- **Wave 7 — ops surface.** A4 (`/metrics`, `/manifest.json`), T9 (API keys),
  E2 (rate limiting), E3 (tool profiles). T9 and E2 are security-relevant.
- **Wave 8 — importers and epics.** I4 (no new dependency, best fit) first;
  I1/I2/I3/I5 and E1/E4–E8 each need their own scoping and dependency decision.

## Stop-and-ask items (never auto-implemented)

Under the skill's rules these pause the loop rather than proceeding:

1. **S1** — bumping `SCHEMA_VERSION` and regenerating the schema changes
   existing DB-creation behavior.
2. **I1, I2, I3** — PDF, image/OCR, and audio import each need a new
   third-party crate.
3. **E6, E8** — cloud backup and the ANN/reranker stack likewise.
4. **E1** — the hub is a new deployable artifact in another language and
   runtime, not an addition to the Rust workspace. Scope decision first.
5. **T1(b)** — outbound notification channels may need a new dependency
   depending on which channels are in scope.
6. Any port of reference behavior still marked `todo` in `remind_me`'s
   `BACKLOG.md`.

## Deliberately excluded from the candidate list

Not filed as issues; recorded so the omission is visible rather than silent:

- `remind_me`'s `BACKLOG.md` items themselves — the reference's own improvement
  backlog, not port scope. Backlog IDs cited above (`SY-18`, `SE-01`, …) point
  at *already-shipped* behavior worth porting, not at open backlog work.
- `remind_me`'s `benchmarks/` harness (LongMemEval, synthetic corpora) — a
  reference-side evaluation tool, not part of the served surface.
- Python-specific packaging (`pyproject.toml`, `uv.lock`, hatchling).
- `storage_interfaces.py` — interface *documentation*, no runtime behavior.
- Test-suite parity — the reference's coverage gate is a CI concern, not a
  per-symbol gap. The target's CI already gates build/test/clippy/fmt.
- `formatting.py` — the target already implements `ResponseFormat` with both
  `markdown` and `json` variants; only the sensitive-flag fields differ (T11).
