# Gap Analysis — `rusty_remind_me` vs. `remind_me`

**Run date:** 2026-07-29
**Target:** `baileyrd/rusty_remind_me` @ `de891ed` ("Initial commit of Rust migration"), 1,436 LOC Rust
**Reference (pinned):** `baileyrd/remind_me` @ `remind-me-mcp` **v1.19.0**, 21,261 LOC Python
**Assessment path:** `spec` — the two codebases share no structurally diffable
surface (Python package vs. Cargo workspace), so `cargo public-api` does not
apply. Candidates were extracted by reading the reference's declared contracts
directly: `@mcp.tool(name=...)` registrations, Starlette route table, and the
`db.py` schema ladder.

**Scope definition:** `rusty_remind_me` has no `ROADMAP.md`. The closest thing
to a hand-curated scope doc is `ARCHITECTURE.md` §1, Tenet 3:

> **Data Parity with `remind-me`**: Identical SQLite Version 19 schema and JSON
> tool signatures for drop-in interoperability.

That tenet is treated as the definition of parity for this run. `remind_me`'s
`BACKLOG.md` is the *reference's own* improvement backlog, not a roadmap for the
port, and is deliberately **not** used as the scope source.

---

## Blockers (read before step 2)

These are not parity gaps. They are conditions that prevent the parity-loop's
implement→PR→green-CI→merge cycle from running at all.

| # | Blocker | Evidence | Effect on the loop |
| --- | --- | --- | --- |
| **B1** | **The workspace does not build.** Every crate depends on `../Rusty_Mill/*` path crates that do not exist here and are not published to crates.io. | `cargo check --workspace` → `failed to read /home/user/Rusty_Mill/rusty_db/rusty_db/Cargo.toml`. No `source =` line for any `rusty_*` entry in `Cargo.lock`. | Step 3.6's local gate (`cargo build && test && clippy && fmt`) cannot run. No change can be verified before push. |
| **B2** | **The Rusty Mill crates are separate repos, not a monorepo.** `Cargo.toml` expects `../Rusty_Mill/rusty_db/rusty_db`; upstream is 40+ standalone repos (`baileyrd/rusty_db`, `baileyrd/rusty_json`, …). | `list_repos` — no `Rusty_Mill` monorepo exists; `Rusty-Mill/.github` is an org profile repo only. | Cloning siblings will not satisfy the declared paths without a manifest change. |
| **B3** | **No CI.** No `.github/` directory; zero Actions workflows. | `actions_list` → `total_count: 0`. | "Merge on green CI" has nothing to gate on. `watch_and_merge.sh` would merge unconditionally. |
| **B4** | **No `RELEASE_NOTES.md`, no issue/PR templates, no labels.** | Repo root listing; `list_issues` → 0 issues. | Steps 2 and 3.7 have no conventions to follow. |

**B1–B3 together mean the autonomous half of this skill cannot run as
specified.** The assessment below is complete and actionable regardless.

---

## Headline numbers

| Surface | `remind_me` v1.19.0 | `rusty_remind_me` | Covered |
| --- | --- | --- | --- |
| MCP tools | 43 | 7 | **16%** |
| HTTP API routes | 21 | 4 | **19%** |
| SQLite tables | 22 | 7 | **32%** |
| Schema migration ladder | 19 versioned steps | none (stamps `user_version = 19` directly) | **0%** |

---

## Gap table

### Correctness — schema parity claim

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `PRAGMA user_version = 19` stamp | fn (existing) | spec | both | `db.py:278` `_SCHEMA_VERSION = 19`, `db.py:316` migration ladder | **yes** | M | **Highest severity.** `schema.rs:104` stamps `user_version = 19` after creating only 7 of the reference's 22 tables. A DB created by `rusty_remind_me` therefore *claims* to be fully migrated; if `remind_me` opens it, `current_version < _SCHEMA_VERSION` is false and every migration is skipped, leaving 15 tables absent. This actively breaks the "drop-in interoperability" tenet rather than merely falling short of it. Fix is either an honest lower version stamp or a real migration ladder — both change existing behavior. |

### Schema — missing tables

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `memory_tags` | table | spec | both | `db.py` | no | S | Normalized tag index; target stores tags only as a JSON blob column. |
| `memory_feedback` | table | spec | both | `db.py` | no | S | Backs `remind_me_feedback`. |
| `memory_associations` | table | spec | both | `db.py` | no | S | Co-retrieval graph; backs neighbor expansion in search. |
| `memories_vec`, `vec_chunks`, `embedding_meta` | table | spec | both | `db.py` | no | M | Vector-search storage (`sqlite-vec`). Blocked on the semantic stack below. |
| `wiki_links`, `wiki_fts`, `wiki_meta` | table | spec | both | `db.py` | no | M | Wikilink graph, wiki FTS index, compile watermark. Backs `wiki_search` / `wiki_compile`. |
| `sync_outbox`, `sync_sends`, `sync_log`, `sync_flags` | table | spec | both | `db.py` | no | L | Multi-machine sync. Includes the outbox trigger set (`HY-03`). Split before filing. |
| `dbs_imports`, `mempalace_imports` | table | spec | both | `db.py` | no | S | Import dedup ledgers (`chat_imports` already exists in target). |

### MCP tools — 36 missing of 43

Grouped for issue sizing; each group is one issue unless noted.

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `remind_me_list`, `remind_me_update`, `remind_me_delete` | tool | spec | both | `tools/crud.py` | no | M | **Start here.** Completes basic CRUD; target has only add/get. Needs soft-delete via `deleted_at` and FTS trigger consistency. Pure addition. |
| `remind_me_vitality_report` | tool | spec | both | `tools/lifecycle.py` | no | S | Target already has `vitality.rs`; this is the reporting surface over it. Mind `DI-04` (open-ended top bucket) — the reference already fixed that bug, so port the fixed behavior. |
| `remind_me_reclassify`, `remind_me_reclassify_batch` | tool | spec | both | `tools/lifecycle.py` | no | M | Category reassignment; recomputes decay rate + vitality on write. |
| `remind_me_feedback` | tool | spec | both | `tools/search.py` | no | S | Depends on `memory_feedback` table. |
| `remind_me_entity_traverse` | tool | spec | both | `tools/entity.py` | no | M | 1-hop relation walk. `entities` / `entity_relations` / `memory_entities` tables already exist in target. |
| `remind_me_wiki_list`, `remind_me_wiki_delete` | tool | spec | both | `tools/wiki.py` | no | S | Trivial over the existing `wiki_pages` table. |
| `remind_me_wiki_search` | tool | spec | both | `tools/wiki.py` | no | M | Depends on `wiki_fts`. |
| `remind_me_wiki_load`, `remind_me_wiki_compile` | tool | spec | both | `tools/wiki.py`, `wiki.py` | no | L | The `FT-08` synthesis layer: files-on-disk are source of truth, DB is a reconcile-from-files cache, two-phase compile with a watermark. Largest single feature here — split into load/reconcile and compile/watermark. |
| `remind_me_stats` (depth) | tool (existing) | spec | both | `tools/admin.py:322` | no | S | Target returns `{total_memories}` only; reference returns per-category counts, vitality distribution, DB size, embedding coverage. Additive to the response body. |
| `remind_me_normalize_batch`, `remind_me_normalize_apply` | tool | spec | both | `tools/normalize.py` | no | M | Two-phase propose/apply normalization. |
| `remind_me_consolidate` | tool | spec | both | `tools/lifecycle.py` | no | L | Near-duplicate merging. Reference infers embedding dim from blob length (`DI-06`) — depends on the semantic stack. |
| `remind_me_annotate` | tool | spec | both | `tools/capture.py` | no | S | Metadata annotation on an existing memory. |
| `remind_me_auto_capture`, `remind_me_get_capture` | tool | spec | both | `tools/capture.py` | no | M | Capture-session grouping via `capture_id` (column already present in target schema). |
| `remind_me_decompose`, `remind_me_decompose_batch`, `remind_me_extract_batch` | tool | spec | both | `tools/capture.py` | no | L | Subject/predicate/object extraction into structured triples. Split into three issues — these are independent handlers despite the shared module. |
| `remind_me_import_chat`, `remind_me_import_directory` | tool | spec | both | `tools/admin.py`, `importer.py` | no | L | Chat-export and generic document ingestion (`FT-02`). Must honor `IMPORT_ROOTS` path confinement (`SE-02`) — this is a **security-relevant** port, not a mechanical one. |
| `remind_me_import_mempalace`, `remind_me_import_dbs` | tool | spec | both | `mempalace_import.py`, `dbs_import.py` | no | L | ChromaDB and foreign-SQLite importers. Reasonable candidates for explicit **out-of-scope**. |
| `remind_me_export_memories` | tool | spec | both | `exporter.py` | no | M | `FT-01` / `FT-06`: JSON/JSONL dump, importer-compatible, including the entity graph. |
| `remind_me_backup` | tool | spec | both | `backup.py` | no | S | SQLite online-backup snapshot. |
| `remind_me_reindex` | tool | spec | both | `tools/admin.py:402` | no | M | FTS rebuild; must prune orphaned `vec_chunks` (`DI-01`). |
| `remind_me_server_status` | tool | spec | both | `tools/admin.py`, `pid.py` | no | M | Requires a PID/lifecycle layer the target has no equivalent of. |
| `remind_me_check_update`, `remind_me_self_update` | tool | spec | both | `updater.py` | no | M | Git-fetch-based self-update. Needs the `SE-06` opt-out env var. Reasonable **out-of-scope** candidate for a Rust binary. |
| `remind_me_watch_status` | tool | spec | both | `watcher.py` | no | L | Folder-watch auto-ingest (`FT-03`). |
| `remind_me_webhook_status` | tool | spec | both | `webhook_server.py` | no | L | Webhook receiver. |
| `remind_me_list_connectors` | tool | spec | both | `tools/admin.py:227` | no | S | Enumerates configured connectors. |
| `remind_me_revoke_clients` | tool | spec | both | `oauth.py` | no | L | OAuth client revocation (`FT-07`). Depends on the whole OAuth stack. |

### HTTP API — 17 missing of 21, plus a path-prefix mismatch

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `/api/v1/*` prefix | route (existing) | spec | both | `api.py` | **yes** | S | Target serves `/api/v1/memories` and `/api/v1/search`; reference serves `/api/memories` and `/api/memories/search`. Any client written against one 404s on the other. Straight contradiction of the drop-in-interop tenet — changing it breaks the target's existing published contract. |
| `/api/memories/{id}` (GET/PATCH/DELETE) | route | spec | both | `api.py` | no | M | Per-record REST. |
| `/api/memories/bulk/{delete,tag,reclassify}` | route | spec | both | `api.py` | no | M | Bulk mutations. |
| `/api/entities`, `/api/entity`, `/api/entity/traverse` | route | spec | both | `api.py` | no | M | Entity graph over HTTP. |
| `/api/wiki`, `/api/wiki/{slug}`, `/api/wiki/load`, `/api/wiki/search`, `/api/wiki/status` | route | spec | both | `api.py` | no | L | Wiki over HTTP; follows the wiki tool work. |
| `/api/export`, `/api/import` | route | spec | both | `api.py` | no | M | Mirrors the export/import tools. Must enforce `IMPORT_ROOTS`. |
| `/api/vitality` | route | spec | both | `api.py` | no | S | Mirrors `remind_me_vitality_report`. |
| API auth (bearer + CSRF) | fn | spec | both | `api.py`, `SE-01` | **yes** | M | Reference requires/generates an API key by default and rejects non-JSON `Content-Type` on mutating routes. Target's HTTP server is **fully unauthenticated**. Adding auth changes existing endpoint behavior → breaking. Security-relevant. |

### Subsystems with no target equivalent at all

Listed for completeness; each is a multi-issue epic, not a single gap.

| Symbol | Category | Source | Platforms | Reference | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Semantic / vector search | subsystem | spec | both | `embeddings.py`, `ann_index.py`, `reranker.py`, `query_expansion.py` | no | L | Embeddings, sqlite-vec, HyDE expansion, reranking. Target's `retrieval.rs` declares `w_semantic` and `vec_score` fields that are **never populated** — the scaffolding exists, the implementation does not. Would need an ONNX/embedding stack; **new third-party dependency → stop-and-ask** under the skill's rules. |
| Multi-machine sync | subsystem | spec | both | `sync.py`, `peer_server.py`, `hub/` | no | L | Outbox/keyset-pagination/echo-suppression. Note reference item `SY-10` (tombstone deletes) is still `todo` upstream — do not port a known-incomplete design without deciding on it first. |
| OAuth 2.1 + remote MCP | subsystem | spec | both | `oauth.py`, `remote.py` | no | L | `FT-05` / `FT-07`: Streamable HTTP transport, DCR, PKCE. |
| Dashboard UI | subsystem | spec | both | `dashboard/` | no | L | Likely **out of scope** for a Rust port. |
| Telemetry (OTel) | subsystem | spec | both | `telemetry.py` | no | M | Optional extra upstream. |
| PID / process lifecycle | subsystem | spec | both | `pid.py` | no | M | Prerequisite for `remind_me_server_status`. |

---

## Recommended sequencing

Ordered so each wave only depends on what precedes it, and so the early waves
are implementable without touching the blocked subsystems.

- **Wave 0 — unblock (not parity work).** Resolve B1/B2 (make the workspace
  build), add CI (B3), add `RELEASE_NOTES.md` + labels + templates (B4).
  Nothing below can be verified until this lands.
- **Wave 1 — honesty.** The `user_version = 19` stamp. Breaking, and it is the
  one item where the current state is worse than simply incomplete.
- **Wave 2 — CRUD + cheap wins.** `list`/`update`/`delete`, `wiki_list`/
  `wiki_delete`, `annotate`, `backup`, `list_connectors`, `stats` depth,
  `vitality_report`. All pure additions over tables that already exist.
- **Wave 3 — schema fill + tools that need it.** `memory_tags`,
  `memory_feedback`, `memory_associations`, `wiki_fts`, `wiki_links`,
  `wiki_meta` → then `feedback`, `wiki_search`, `entity_traverse`,
  `reclassify`, `normalize`.
- **Wave 4 — API surface.** Per-record and bulk routes, entity routes, wiki
  routes, export/import. Prefix mismatch and auth are stop-and-ask.
- **Wave 5 — epics.** Semantic stack, sync, OAuth, importers, watcher,
  webhooks. Each needs its own scoping pass and at least one dependency
  decision.

## Stop-and-ask items (never auto-implemented)

Under the skill's rules these pause the loop rather than proceeding:

1. `user_version` stamp — changes existing DB-creation behavior.
2. `/api/v1/*` → `/api/*` — changes the target's existing published contract.
3. HTTP API authentication — changes existing endpoint behavior.
4. Semantic/vector search — requires new third-party dependencies.
5. Any port of reference behavior still marked `todo` in `remind_me`'s
   `BACKLOG.md` (`SY-10` tombstone deletes, `SY-11` embed batching).

## Deliberately excluded from the candidate list

Not filed as issues; recorded so the omission is visible rather than silent:

- `remind_me`'s `BACKLOG.md` items themselves — that is the reference's own
  improvement backlog, not port scope. Where a backlog ID is cited above
  (`DI-01`, `SE-02`, …) it is a pointer to the *already-fixed* behavior worth
  porting, not an instruction to port the backlog.
- Python-specific packaging (`pyproject.toml` extras, `uv.lock`, hatchling).
- Test-suite parity — the reference's 80% coverage gate is a CI concern for
  Wave 0, not a per-symbol gap.
