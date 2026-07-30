# Release Notes

Dated entries, newest first. One entry per merged pull request.

## 2026-07-30 — Multi-node sync: `memories` and the knowledge graph (#57)

### Added
- **Sync client**: `sync::push_outbox` drains this node's `sync_outbox` to a
  configured hub, paged (`BATCH_SIZE=200`), marking rows sent per-remote via
  `sync_sends` — either the exact ids a modern hub reports processed, or
  (a count-only legacy response) the whole page. `sync::pull_remote` pages
  a hub's changes back (`PULL_PAGE_SIZE=500`, capped at `MAX_PULL_PAGES=100`
  per cycle), applying each via the conflict-resolution engine below and
  persisting a keyset `(updated_at, id)` cursor per remote in `sync_log`.
- **Conflict resolution** (`sync::upsert_record`): last-write-wins on
  `updated_at` — a *tied* timestamp means the incoming side loses, not a
  no-op and not a win — with `tags` union-merged (dedup, order-preserving)
  and `metadata` shallow-merged per key (the LWW winner's value wins on a
  collision, never recursively), **both regardless of which side wins**.
  A winning update never touches `created_at` (insert-only). Applying an
  incoming record echo-suppresses only the outbox row that very write just
  created, leaving a genuinely concurrent local edit to the same memory
  untouched.
- **This node's own peer server** (`sync::PeerServer`/`SyncPeer`): serves
  `GET /health`, `POST /sync/push`, `GET /sync/pull` — the exact same
  protocol this node's own client speaks to a hub, since hub and peer are
  the same protocol against different endpoints, matching the reference.
  Bearer-authenticated via `REMIND_ME_SYNC_SECRET`; off unless that secret
  is configured, independent of whether this node is also configured to
  sync outward.
- **A background `SyncWorker`** runs push-then-pull-then-prune every
  `REMIND_ME_SYNC_INTERVAL` seconds (default 60) against the configured
  hub. Enabled only when `REMIND_ME_NODE_ID`, `REMIND_ME_HUB_URL`, and
  `REMIND_ME_SYNC_SECRET` are all set — matching the reference's
  `SYNC_ENABLED` exactly — and started once from the CLI's real server
  entry point, not from `McpServer::new` (which the test suite also uses
  to build a server per test).
- `add_memory` now stamps `node_id`/`client` from
  `REMIND_ME_NODE_ID`/`REMIND_ME_CLIENT` on every write, unconditionally —
  not gated on sync being enabled, matching the reference exactly, so a
  node that turns sync on later already knows which of its existing
  memories were its own. `remind_me_update` does not re-stamp them, also
  matching the reference.
- `delete_memory` now tombstones (`deleted_at` + `updated_at` set) instead
  of hard-deleting when sync is configured — a hard `DELETE` produces no
  outbox row at all (the sync triggers only fire on INSERT/UPDATE), so it
  would otherwise silently resurrect on the next pull elsewhere. A node
  with sync disabled deletes immediately, exactly as before.
- `remind_me_server_status` gained `sync_peer` and `sync` fields, merged in
  by the MCP layer the same way `webhook`'s already is.
- `docs/adr/0004-sync-protocol-and-conflict-resolution.md`.

- **Knowledge-graph sync** — `entities`, `entity_relations`, and
  `memory_entities` mention links now sync alongside `memories`, over three
  new endpoints (`/sync/pull_entities`, `/sync/pull_links`,
  `/sync/pull_entity_relations`), each with its own namespaced `sync_log`
  cursor (`"{remote_id}#entities"` etc.). `sync::graph::ensure_schema`
  installs this crate's own outbox triggers for these three tables — there
  is no generated-schema equivalent, only `memories` ships one.
- Entities get their own sync-specific conflict resolution
  (`sync::upsert_entity_record`): LWW on `name`/`kind`/`node_id`, with
  `aliases` always union-merging regardless of the winner — a distinct
  function from the interactive `upsert_entity` used by direct tool calls,
  which has its own different "existing kind wins" merge rule. Relations
  and links are immutable insert-or-ignore, with no foreign key by design:
  a link or relation may reference a memory/entity that hasn't arrived on
  this node yet, and the row simply waits rather than erroring.
- A push batch is naturally heterogeneous — every graph-table trigger
  funnels into the same `sync_outbox` a memory row already uses, tagged
  with a `record_type` key (absent means `"memory"`, for backward
  compatibility). `sync::apply_incoming_record` dispatches each record in a
  batch to the right conflict-resolution function.
- `docs/adr/0005-graph-sync.md`, including a real bug this work caught
  before it shipped: the outbox-push "did the peer accept this" check
  originally matched on `sync_outbox.memory_id`, which for a link row
  holds only the memory half of its identity, not its wire id
  (`memory_id|entity_id`) — every link would have been silently retried
  forever. Fixed by matching on the record's own `payload["id"]` instead,
  for all four record types.

- **Peer discovery** — `sync::discover_peers` combines a static peer list
  (`REMIND_ME_STATIC_PEERS`, a JSON array of `{"node_id", "url"}` objects)
  with Tailscale's local API (`GET /localapi/v0/status` over a Unix
  socket, `REMIND_ME_TAILSCALE_SOCKET` to override the platform-default
  path). Every `Online` Tailscale peer with an address is a candidate,
  addressed at `http://{ip}:{PEER_PORT}`; whether it's actually a
  `remind_me` instance is decided by probing `/health` right before
  syncing (`sync::probe_peer`), the same check the hub already gets — not
  a tag or hostname filter at discovery time. Static peers are processed
  first, so one wins a URL collision with a Tailscale-sourced duplicate.
  The background `SyncWorker` now syncs (push + all four pulls) with the
  hub, then every discovered peer in turn, skipping any that name this
  node's own `node_id` or fail the health probe.
- `docs/adr/0006-peer-discovery.md`, including one deliberate divergence:
  a malformed `REMIND_ME_STATIC_PEERS` *value* degrades to an empty peer
  list instead of crashing the process at startup the way the reference's
  own unguarded `json.loads` does — matching this crate's consistent
  graceful-degradation posture for every other optional feature, since the
  reference's behavior here reads as an oversight rather than a
  considered design choice. A malformed individual *entry* within an
  otherwise-valid array is still skipped, matching the reference exactly.

### Scope — matches the epic's own suggested split, three slices in
Per the issue's explicit instruction to split this epic: **`memories`, the
knowledge-graph tables, and peer discovery (static list + Tailscale)**.
Still no OAuth and no `remind_me_revoke_clients` — its own follow-up
issue, the same way `#59` was already split out of this epic for the
outbox-growth defect.

### Fixed: tombstone propagation, and every outbox trigger's `sync_flags` gate (#76)
The generated `schema_*.sql` files were dumped from an earlier `remind_me`
snapshot whose `memories_outbox_ai`/`_au` had no `WHEN` guard and a
23-column payload ending at `superseded_by` — missing `doc_id`,
`chunk_index`, and (critically) `deleted_at`, so a `delete_memory`
tombstone had no way to reach another node at all. Re-running the
reference's actual schema code (`db.py`'s `_ensure_schema`, imported
standalone rather than hand-transcribed) against a fresh SQLite connection
confirmed the current shape: both triggers gated on
`sync_flags.sync_enabled = '1'`, a 26-column payload, and — a second,
independent finding — the four graph-table outbox triggers
(`entities_outbox_ai`/`_au`, `entity_relations_outbox_ai`,
`memory_entities_outbox_ai`) are themselves part of the reference's
generated schema, not something this crate needed to hand-roll. All three
`schema_*.sql` files were regenerated from that dump; `sync::graph`'s
hand-rolled trigger installer was removed entirely now that its
triggers live in the generated file like every other one.

Every outbox trigger firing at all is now conditional on
`sync_flags.sync_enabled`, reconciled against the live configuration on
every open by the new `sync::reconcile_sync_enabled_flag` — matching the
reference's own `_reconcile_sync_enabled_flag` exactly, including its
backfill-on-first-enable behavior for `memories`/`entities`/
`memory_entities` (deliberately not `entity_relations`, matching a real
omission in the reference rather than "fixing" it into a difference).
A node that never configures sync now queues nothing in `sync_outbox` at
all — closing the growth problem `#59`'s `prune_outbox` was a downstream
workaround for, at the actual source, matching the reference.

`SyncRecord` gained `deleted_at`, applied through the same last-write-wins
path as every other column — this is what actually lets a tombstone
propagate. `doc_id`/`chunk_index` remain unapplied on receipt, matching the
reference's own receiving side exactly (it sends them for wire column-list
parity but never reads them back either). See ADR-0007.

Re-embedding a synced memory is still deliberately left to
`remind_me_reindex` (`#49`) rather than done inline here — `upsert_record`
never calls the embedder, so a synced memory has no vector until the next
reindex, exactly like any other bulk-arrived memory (`dbs`, MemPalace,
chat/document import). Hard-deleting old tombstones once every reachable
peer has almost certainly observed them (the reference's
`sync._compact_tombstones`) is not implemented here — a real, separate gap
for its own follow-up, not folded into this fix.

## 2026-07-29 — Import a MemPalace ChromaDB store (#53)

### Added
- **`remind_me_import_mempalace`** — bulk-imports MemPalace drawers by
  reading its persistent ChromaDB store's metadata segment directly,
  read-only. A drawer carrying `remind_me`'s own memory frontmatter has its
  `category`/`tags`/`created` restored; everything else becomes one opaque
  memory per drawer, tagged with its wing and room.
- `docs/adr/0001-mempalace-chroma-sqlite-read.md` — the first ADR in this
  repo, recording the investigation the issue asked for before writing any
  code: Chroma's local persistence keeps documents and metadata in an
  ordinary SQLite file (`collections`/`segments`/`embeddings`/
  `embedding_metadata`, with a document's text under the reserved key
  `chroma:document`), verified stable from `chromadb` 0.5.0 (the reference's
  own minimum pin) through 1.5.9. The vector segment — the actual HNSW
  index — is never opened.
- A `mempalace` entry in the connector registry, `file_import_kind: false`,
  matching `dbs`'s precedent from #52: listed for discovery, not dispatch.

### Notes
This was the one import source in the backlog with a genuinely hard
dependency — Rust has no ChromaDB client — and the issue was explicit that
declining the tool was a legitimate outcome if direct reading wasn't
feasible. It turned out to be the wrong question: the reference's own read
(`collection.get(..., include=["documents", "metadatas"])`) never asks Chroma
for a vector in the first place, so "read a vector database" was never
actually required. Once that was verified rather than assumed, this became
comparable in shape to `#52`'s `dbs` importer — read a foreign SQLite schema,
directly, read-only.

**The issue's acceptance criteria says frontmatter-bearing drawers restore
their original `id`; the reference does not do this.** `pull_mempalace`
parses a drawer's `id:` frontmatter field into a dict and never reads it
again — the memory id is always freshly minted, matching every other memory
this crate creates. This is matched deliberately, not "fixed": these gap
issues track the reference's actual behaviour, and a test
(`a_native_drawers_frontmatter_id_is_not_restored`) asserts the real behaviour
directly so a future change does not quietly turn this into a divergence.
`category`/`tags`/`created` genuinely are restored, and `source` is restored
too but always prefixed `mempalace:` — also matched exactly, not idealized.

Unlike `dbs_import`, there is no edit-detection: dedup is keyed on
`drawer_id` alone, so an edited drawer that keeps its id is silently skipped
on a rerun. This matches the reference exactly — it has no content-hash
column to notice the edit with — and is asserted directly for the same
reason as the `id` point above.

`REMIND_ME_MEMPALACE_PATH` (default `~/.mempalace/palace`) is operator
configuration, not a per-call argument — `MempalaceImportInput` has no path
field, matching the reference's own `MempalaceImportInput`. This is why it
does not go through the import-roots containment check (`SE-02`) a
caller-supplied path would: nothing about this path is caller-supplied.

The wing/room filter and pagination are applied in Rust, over every drawer in
the collection, rather than pushed into Chroma's own query planner the way
the reference does (`collection.get(where=..., limit=, offset=)`) — reading a
flat key/value table makes that just as simple locally, and it avoids
depending on whatever internal query-planning API a given Chroma version
exposes.

## 2026-07-29 — The HTTP API grows from 2 routes to 19 (#48)

### Added
- **17 new HTTP routes**, taking `remind_me_api` from `/health` and `/stats`
  to near-parity with the reference's REST surface: memory CRUD and search
  (`/api/memories`, `/api/memories/{id}`, `/api/memories/search`), three
  bulk operations with no MCP-tool equivalent (`/api/memories/bulk/delete`,
  `/bulk/tag`, `/bulk/reclassify`), the entity graph
  (`/api/entity`, `/api/entities`, `/api/entity/traverse`), import/export
  (`/api/import`, `/api/export`), vitality (`/api/vitality`), and the
  read-only wiki surface (`/api/wiki`, `/wiki/search`, `/wiki/load`,
  `/wiki/status`, `/wiki/{slug}`).
- New, tested `remind_me_core` functions backing routes with no MCP-tool
  equivalent: `queries::{bulk_delete, bulk_tag, search_paginated}`,
  `entity::{entity_profile, list_entities}`, `wiki_fs::pending_compile_count`,
  `fts::extract_entity_token`, `import_paths::validate_import_database`.
- `remind_me_api` is now a synchronous `std::net` server, one connection at a
  time — the same shape `webhook.rs` (#56) established, and for the same
  reason: every handler takes the database lock, so a thread per connection
  would not finish requests faster. The `tokio` dependency is gone from both
  `remind_me_api` and the CLI.

### Auth posture — stated explicitly, not inherited silently
The reference always runs authenticated (an auto-generated, persisted key).
This crate does not reproduce that — auto-generating a secret needs a vetted
random source and a place to persist it, and improvising one for a security
token is worse than not having the feature yet. Instead:

- **`REMIND_ME_API_KEY` unset**: `GET` routes stay open (this crate's
  existing pre-#48 behaviour for `/stats`, carried forward explicitly).
  Every mutating route is refused with 401 — adding write routes is the part
  that actually changes this surface's risk profile, so it does not default
  open.
- **Set**: every `/api/*` request, read or write, requires
  `Authorization: Bearer <key>`, compared via the same
  `remind_me_core::webhook::constant_time_eq` the webhook endpoint uses —
  one bearer-auth implementation, not two to drift apart.
- **`GET /health`** is always public, matching the reference's own
  rationale: a liveness probe has to work whether or not auth is configured.
- Every mutating request additionally needs a JSON `Content-Type` (415
  otherwise) — the reference's CSRF hardening, kept regardless of the auth
  posture above.

CORS is not implemented: nothing in this crate serves the dashboard HTML the
reference's CORS policy exists to protect a browser tab talking to.

### Notes
`GET /api/memories/search` extracts an `entity:NAME` token from `q`
(`FT-04`), narrowing results to memories linked to that entity or matching it
structurally — matching the reference's own HTTP-side syntax. The MCP
`remind_me_search` tool does not yet support this (a separate, still-open
gap); the extraction itself is a shared, tested function specifically so
whatever fixes that reuses it instead of writing a second parser.

Superseded memories are excluded unconditionally from paginated search, in
both its FTS and entity-scoped branches. The reference only excludes them on
the entity-scoped path — reproducing that would mean two search entry points
in this crate disagreeing about whether a stale, superseded chunk is a
result, which the #24/#41 fixes elsewhere exist specifically to prevent.

`GET /api/memories` and the paginated search route share this crate's
existing `LIST_LIMIT_MAX` (100) rather than the reference's 200 — one core
function (`list_memories`), one bound, across MCP and HTTP.

`/api/import` and `/api/export` share containment (`SE-02`) with the MCP
import/export tools rather than duplicating it: the HTTP handler resolves a
path only to decide file-vs-directory or read-vs-write, and the actual,
authoritative check runs inside `importer::import_chat`/`import_directory`
and `export::export_memories`, the same functions the MCP tools call.

## 2026-07-29 — Import a dbs archive (#52)

### Added
- **`remind_me_import_dbs`** — bulk-imports a
  [daily-backup-system](https://github.com/baileyrd/daily-backup-system)
  archive: the SQLite database `dbs` collects a person's Reddit, YouTube,
  Raindrop and GitHub-stars data into, under a uniform `items`/`sources`
  schema. Each live item becomes a memory.
- `import_paths::validate_import_database`, which applies the same
  containment-then-existence rule to a database path as to any other import
  source, minus the extension check — a `.sqlite3` is read with SQL, so its
  suffix carries no meaning.
- A `dbs` entry in the connector registry, listed for discovery with
  `file_import_kind: false` — it is not something you can pass as `kind` to
  `remind_me_import_chat`.
- `dbs_imports` finally has a writer. It arrived with the generated schema and
  had been unused since.

### Notes
**The point of this over the export route is structure.** `dbs export-notes`
plus the folder watcher (#55) already handles the *content* — what it cannot
carry is the shape: an item's source and tags arrive as prose in a note, and
prose is not a graph. Here they become first-class entities (`FT-04`) linked to
the memory, so "everything from raindrop" and "everything tagged rust" are
traversals rather than searches. An implementation of this that did not write
entities would have no reason to exist, so that is asserted directly rather
than left implied.

`item_kind` is deliberately **not** an entity. It becomes the memory's category
and lands in metadata. There is no established "kind" entity type in this graph
to reuse, and inventing one would put a second, incompatible taxonomy beside
the existing ones.

**The archive is opened read-only, and the test asserts that at the SQLite
layer** rather than trusting this module never to attempt a write. It is
someone's backup.

**Dedup is on `(dbs_source, external_id)` — `dbs`'s own item identity —
plus a content hash.** An item whose hash moved gets a *fresh* memory, with the
previous one marked `superseded_by` it: history accumulates rather than being
overwritten, mirroring the watcher's changed-file behaviour. Comparing hashes
rather than timestamps is the point — `dbs` does not always move a timestamp on
an edit, so an importer keying on `item_created_at` would miss it entirely.

The memory id is **derived**, not minted: `sha256("dbs:source:external_id:hash")`
truncated to 12 characters. Everything else in this crate mints `mem_<uuid>` so
that storing the same content twice gives two memories; here the opposite is
wanted. Two concurrent or retried imports of the same item version compute the
same id, so `INSERT OR IGNORE` collapses them. Without that, both would read
"not yet imported", both would insert under different ids, and `dbs_imports` —
one row per `(source, external_id)` — would track only whichever wrote last,
orphaning the other memory permanently.

A memory's `created_at` is the item's own creation time, not the import's.
Vitality decay reads that column, so importing a decade of archive must not
make all of it look like it happened today.

The whole page is one transaction. A half-applied import would leave
`dbs_imports` claiming items no memory backs, and the next rerun would skip
them — the archive would look imported when it was not.

## 2026-07-29 — Push ingestion over HTTP (#56)

### Added
- **`POST /ingest`** — an HTTP endpoint that turns a pushed payload into
  memories through the same importer a file goes through: same parsing, same
  dedup, same chunking. The folder watcher covers senders that can drop a file
  where this process sees it; a CI job or an automation on another machine
  usually cannot.
- **`remind_me_webhook_status`** — whether a secret is configured, whether it
  is listening, where, and how many pushes were ingested, skipped or refused.
- `importer::import_bytes`, the filesystem-free import entry point, and
  `importer::validate_kind_and_suffix`, which now decides the accepted formats
  in one place for file, directory and pushed imports alike.

### Notes
**It does not exist unless `REMIND_ME_WEBHOOK_SECRET` is set.** That is the
behaviour, not a deployment recommendation — an endpoint that writes arbitrary
content into memory with no authentication is worse than no endpoint — and
there is no configuration that asks for one: `WebhookConfig` cannot be
constructed with an empty secret. The bind address defaults to localhost for
the same reason.

Three properties are load-bearing rather than decorative:

- **The token comparison is constant-time.** A `==` returns faster on a wrong
  first byte than a wrong last one, which recovers a secret one byte at a time
  over enough requests.
- **The body is capped at 10 MiB and the header block at 8 KiB**, both checked
  before anything is buffered. A declared `Content-Length` is never trusted as
  an allocation size.
- **The listener stops before the database connections close** (`SE-07`).
  `WebhookServer` joins its thread in `Drop`, and `McpServer` declares the
  webhook field *before* the database — Rust drops fields in declaration order,
  so the ordering holds by construction rather than by remembering to call
  things in sequence. A test asserts it directly: once `stop()` returns, the
  database `Arc` has one holder left.

Authentication is checked **before** routing, so an unauthenticated caller
cannot map the endpoint by comparing a 404 against a 405.

A pushed `filename` names nothing on disk — it supplies the extension that
picks the parser and the display name stored in metadata. It is held to exactly
the same format rule a real file is, so the extension is not a way to reach a
parser the file importer will not reach.

**`source` stays `chat_import`/`document_import`, not `webhook`.** #56 asked
for the latter, and `webhook` does carry a 0.9 prior in the vitality source
table — but the reference stores pushed content under the file-import values
too, and `source` feeds dedup, the `normalize_batch` selection and that same
prior. Diverging would make identical content score differently depending on
which implementation happened to receive the push, on a database the two are
meant to share. The arrival channel is recorded as `metadata.ingest` instead,
where it costs nothing.

Connections are served one at a time. Every request takes the database lock to
import, so a thread per connection would not make any of them finish sooner —
it would only move the queue out of the kernel's accept backlog and into
unbounded threads inside this process. Reads and writes carry a 10-second
deadline so a stalled client releases the loop.

## 2026-07-29 — Bound the sync outbox (#59)

### Fixed
- **`sync_outbox` grew on every write and nothing drained it.** The outbox
  triggers arrived with the generated schema and fire correctly, but this crate
  has no sync engine, so every row leaked — each carrying a full JSON snapshot
  of the memory. Worse, `memories_outbox_au` fires on `UPDATE`, and since #41
  every search that returns results updates `memories` to record access, so the
  table grew on **reads** as well as writes.

### Notes
The fix applies the reference's own retention rule rather than a new one: rows
already marked sent are echo-suppressed and never pushed, so they go
immediately; the rest are kept for `REMIND_ME_OUTBOX_RETENTION_DAYS` (default
30) so an intermittently-reachable remote can still catch up, then dropped along
with their per-remote send markers.

Copying the policy matters because a database can be shared with `remind_me`,
which opens the same file and prunes on the same rule — so anything this deletes
is something the reference would have deleted anyway. A tighter rule would
silently drop changes the reference still intended to push.

The reference prunes on each sync cycle. This crate has no sync cycle, so it
prunes on open. That bounds a long-lived database but **not** a single process
that stays up past the retention window; whatever implements sync should call
`sync::prune_outbox` per cycle, which is the arrangement the reference already
has.

## 2026-07-29 — Access recording (#41)

### Fixed
- **Nothing recorded retrieval, so two thirds of the vitality model were
  inert.** `access_count` and `accessed_at` were read everywhere and written
  nowhere, so every memory was permanently frozen at its insert-time values.
  Concretely:
  - the `sqrt(access_count + 1)` frequency boost was always 1.0 — a memory
    retrieved a thousand times ranked exactly like one never retrieved;
  - bridge protection keys on ten accesses, so it could never fire. #24's test
    for it passed only because it set the column by hand;
  - dormancy aged a memory from when it was *written*. A memory in daily use
    decayed exactly as though abandoned on day one, and eventually dropped out
    of default search.

  That last one is the sharp edge: #24 made dormancy filtering real but left it
  measuring the wrong clock. This is the other half.

### Changed
- Search now increments `access_count`, stamps `accessed_at`, and refreshes the
  stored `vitality` and `status`. **`status` had no writer at all before this.**
- Frequently retrieved memories now survive dormancy filtering where they
  previously did not, so an active vault will return *more* by default than it
  did after #24.

### Notes
Only direct hits are recorded. #35's expansion sections are a discovery aid
surfaced by adjacency, not answers to the query, and recording them would
inflate the vitality of every neighbour on every expanded search. Recording also
runs after the expansions are built, so an expansion describes the store as the
caller found it.

`search_memories` stays a pure read; the write lives in `search_with_expansions`
alongside co-retrieval reinforcement. `remind_me_get` does not record an access
— matching the reference, where search is the only caller.

One `SELECT` and one prepared `UPDATE` reused across rows, rather than a round
trip per result.

## 2026-07-29 — Search expansions (#35)

### Added
- `expand_co_retrieval` on `MemorySearchInput` — it was missing entirely.
- All three expansions now do something. They are returned in their own
  sections, each capped at 5 items with 300-character snippets:
  - `expand_entities` → other memories mentioning the same entities, with the
    linking entity names;
  - `include_neighbors` → sibling chunks of the same source document, within one
    chunk position;
  - `expand_co_retrieval` → memories retrieved alongside these before, strongest
    association first.
- `Memory` gained `doc_id` and `chunk_index`, which the schema had but the model
  did not read.
- Co-retrieval is now recorded. Every search returning two or more results
  reinforces the association between each pair.

### Fixed
- **`expand_entities` and `include_neighbors` were inert.** Both were declared
  on `MemorySearchInput` and read nowhere, so setting either changed nothing.
- **`memory_associations` had no writer**, so the co-retrieval graph was empty
  by construction.

### Notes
Expansions sit **outside** the ranked results and never merge into them, so
they do not consume `limit` — their own caps and snippet length bound their
cost instead.

For co-retrieval the one-way flow is the point: search results → recorded
associations → surfaced as suggestions, never as a ranking input. Letting a
weight reach the ranking would build a loop where whatever came back together
once comes back together forever; keeping it out means no decay maths is needed
to counteract one. There is a test asserting a maxed-out association leaves
result order untouched.

Pairs are sorted before insert, so `(a,b)` and `(b,a)` are one row — the
question #12 deferred. Without that every weight would split across two rows and
read back at half strength. Weight accumulates as a count clamped at 50, and
only the first 10 results of a search participate in pairing, which bounds one
search to 45 writes.

**Searching now writes.** `search_with_expansions` records associations;
`search_memories` remains a pure read for callers that only want results.

`include_neighbors` returns nothing until importers land, since only an importer
writes `doc_id` — except `normalize_apply`, which inherits it.

## 2026-07-29 — Import normalization (#18)

### Added
- `remind_me_normalize_batch` — returns raw `document_import` / `chat_import`
  memories that have not been normalized yet, with 1000-character snippets and
  `total_unnormalized` so a caller can tell whether another round is worth it.
  Batch size 1–100.
- `remind_me_normalize_apply` — writes distillations back, 1–50 at a time. Each
  entry is `{memory_id, question, summary, resolution?, refs?, entities?}`.

### Notes
The write is **non-destructive**. A distillation becomes a *new* memory
(category `normalized`, source `normalization`) and the raw import is untouched,
staying searchable in its own right. The link is metadata-only, via
`normalized_from`, which is also how the backlog shrinks — there is no
"normalized" flag column, so a raw row drops out once anything points back at
it.

The new memory inherits the raw row's `tags`, `doc_id` and `chunk_index`, the
last two so neighbour-aware retrieval still associates it with the rest of the
document, and links any entities the entry names — a raw import is never
entity-linked automatically, so without that the distillation would be invisible
to `remind_me_entity` and `remind_me_entity_traverse`.

Distillations are given the same write-time vitality treatment as memories
written through `remind_me_add`, rather than being left at the column defaults
to rank unlike everything else.

The asymmetric bounds — 100 to read, 50 to write — are the reference's, not a
transcription slip.

**The batch is empty until importers land.** Nothing in this crate writes
`document_import` or `chat_import` memories yet, so on a store built only
through `remind_me_add` there is never anything to normalize. That is correct
behaviour and is pinned by a test.

## 2026-07-29 — Entity relation traversal (#16)

### Added
- `remind_me_entity_traverse` — walks the typed entity-relation graph outward
  from a starting entity, in **both directions**, so a traversal from "Bailey"
  surfaces relations Bailey is the subject of and relations naming Bailey as the
  object. Takes `name`, `hops` (1–3), an exact `relation` filter, and `cap`
  (1–100, default 20). The graph could be built before this and never queried.
- Start-node resolution by canonical name **or alias**, ignoring casing and
  spacing: a deterministic-id lookup first, then a scan that prefers a canonical
  name match over an alias match.

### Changed
- README claimed "1-hop relation linking". The reference allows three, and so
  does this.

### Notes
This is a different thing from search's `expand_entities`, which is 1-hop
co-mention — two memories happening to name the same entity. Traversal follows
typed subject/relation/object triples, which is what lets a chained question
("who introduced me to the person who recommended this") resolve at all.

Termination on cyclic graphs falls out of the breadth-first shape rather than
needing a guard: each hop queries only the entities newly discovered by the
previous one, so a cycle produces an empty next frontier and stops.

`hops` and `cap` are clamped rather than rejected — a caller that ignores the
schema gets a bounded walk instead of an error — and an unresolvable start node
is `found: false` with a message, since "no such entity" is an ordinary answer.

Nothing in this crate writes `entity_relations` yet; the tools that will
(`decompose`, the relation half of `annotate`) are still to come, so on a store
built only through `remind_me_add` every traversal returns no edges.

## 2026-07-29 — Entity ids now match remind_me's (#36)

### Fixed
- **Entity ids could never converge with `remind_me`'s.** An entity's id is a
  content hash precisely so that two machines recording the same entity land on
  the same row. Ours diverged from the reference on three counts at once — we
  prefixed `ent_`, kept all 64 hex characters instead of the first 12, and
  normalised the name by trimming only. The reference collapses internal
  whitespace too, so `"Bailey  Robertson"` and `"Bailey Robertson"` were two
  entities here and one there. An entity created in either system was invisible
  to the other.

### Changed
- **Existing entity ids are rewritten on open.** `entities.id` and every
  reference to it — `memory_entities.entity_id`, and both
  `entity_relations` endpoint columns — are repointed. Nothing cascades here
  (the reference declares no foreign key, so sync can deliver rows out of
  order), so the rewrite is explicit; a link left behind would dangle silently.
- Rows that now normalise to the same id are **merged** rather than colliding:
  aliases union, the earliest `created_at` wins, an already-set `kind` is kept,
  and duplicate `(memory_id, entity_id)` links collapse to one.

### Notes
Twelve hex characters is 48 bits of id space. That is narrower than what we had
and is inherited from the reference rather than chosen — widening it would put
us straight back to two populations of entities that never meet, which is the
whole thing this fixes.

`normalize_entity_name` and `entity_id` are now the single source for entity
identity, so no write path can normalise differently.

## 2026-07-29 — Dormancy filtering actually filters (#24)

### Fixed
- **`remind_me_search` filtered on the stored `vitality` column, which never
  decays.** `add_memory` computes that value once, with `access_count = 0` and
  zero elapsed days, and nothing rewrites it afterwards. So `include_dormant:
  false` — the default — filtered nothing, and `min_vitality` compared against a
  number unrelated to the memory's current standing. Both predicates now use
  vitality recomputed from real elapsed time, including the bridge rule that
  halves decay for memories accessed at least 10 times.

### Changed
- **Default search results are smaller.** Memories that have decayed below the
  0.05 floor no longer come back unless `include_dormant: true` is passed. That
  is the documented behaviour, but it is the first release in which it has any
  effect, so an existing vault will visibly return fewer rows.

### Notes
The filter runs inside the query, before `LIMIT`, so a page of results is not
truncated and then thinned — the under-filling shape the reference tracks as
`DI-03`. Expressing the ACT-R formula in SQL would have achieved that too, but
the bundled SQLite is compiled without `SQLITE_ENABLE_MATH_FUNCTIONS`, so `exp`
and `sqrt` do not exist. `calculate_vitality` is registered as a scalar SQL
function instead, which keeps the predicate in the query and leaves exactly one
implementation of the maths.

`accessed_at` is nullable — `remind_me` leaves it unset until a memory is first
retrieved — so it falls back to `created_at`. Without that a synced row would
compute a NULL vitality and vanish from every search.

## 2026-07-29 — Memory classification, and one owner for decay_rate (#17)

### Added
- `remind_me_reclassify` — applies `memory_type` classifications in batches of
  1–100, setting each memory's `decay_rate` to match its new type. Unknown ids
  are reported in `not_found` rather than failing the batch.
- `remind_me_reclassify_batch` — returns still-unclassified memories with
  500-character snippets, plus `total_unclassified` so a caller can tell whether
  another round is worth requesting.
- `memory_type` and the `unclassified` default are now actually used. The schema
  reset added the column; this is the code that reads and writes it.

### Fixed
- **`decay_rate` had two writers with different sources of truth.** #3 made
  `update_memory` recompute it from `category`; `reclassify` derives it from
  `memory_type`. With both, classifying a memory as `decision` (slow decay) and
  then editing its category to `action_item` (fast decay) silently overrode the
  classification. `update_memory` no longer touches `decay_rate` — matching the
  reference, whose update path does not mention the column at all.

### Notes
Issue #17 asked for `decay_rate` *and* vitality to be recomputed, and for the
recompute to share a helper with #3's category path. Both were wrong:
classification does not touch vitality or `base_weight` — it says what a memory
*is*, not how much it has been used — and there is no shared helper because
`category` was never the right source for decay.

## 2026-07-29 — Retrieval feedback (#14)

### Added
- `remind_me_feedback` — mark a memory helpful or unhelpful, in two modes that
  the reference selects by whether a `query` is supplied:
  - **without `query`** — a global judgement. `base_weight` scales by ±15%,
    clamped to 0.1..=3.0, and vitality is recomputed.
  - **with `query`** — contextual. The event is logged to `memory_feedback` with
    normalised query tokens, and `base_weight` is left alone.
- `vitality::tokenize_query`, the coarse lowercase/alphanumeric tokenisation the
  contextual mode clusters queries by.
- 14 tests, including the clamps at both ends, append-only logging, and delete
  cleanup.

### Notes
The two modes are the point, not an embellishment. A memory can be a poor answer
to one question and exactly right for another; demoting it globally on the
first's feedback would punish the second. Issue #14 said feedback should not
mutate vitality at all — that is true only of the contextual mode.

`access_count` is untouched in both modes: it feeds `sqrt(access_count + 1)`,
where a "negative access" has no meaning.

Cleanup on delete is `delete_memory`'s job, not the database's — `memory_feedback`
has no foreign key, because the reference omits it so sync can deliver rows out
of order.

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
