# ADR-0007: Regenerate the schema dump; gate every outbox trigger on `sync_flags`

Status: Accepted
Date: 2026-07-30

## Context

Filed as `#76` while implementing `#57`'s memories-only slice (PR #75): the
installed `schema_triggers.sql` was dumped from a `remind_me` snapshot that
predates the trigger shape the reference actually ships today. Two things
were confirmed by directly re-running the reference's real schema code
(`remind_me_mcp/db.py`'s `_ensure_schema`/`_migrate_schema`, imported
standalone against a fresh in-memory SQLite connection, with only its
optional `embeddings`/`ann_index` dependencies stubbed out — not a
hand-transcription, the actual generated DDL) against a fresh dump:

1. **`memories_outbox_ai`/`_au` were missing a `WHEN` guard** and three
   payload columns (`doc_id`, `chunk_index`, `deleted_at`) the reference's
   `_migrate_v15_to_v16` added. Without `deleted_at` on the wire, a
   soft-deleted memory (`#57`'s `delete_memory` tombstone path) had no way to
   propagate its deletion to another node at all.
2. **`entities_outbox_ai`/`_au`, `entity_relations_outbox_ai`, and
   `memory_entities_outbox_ai` are themselves part of the reference's
   generated schema** (added with the entity graph in `_migrate_v9_to_v10`/
   `_migrate_v13_to_v14`), not something this crate needed to invent. An
   earlier pass of this port (`#57`'s graph-sync slice) installed hand-rolled
   equivalents because the schema dump it started from predated them.

Both gaps trace back to the same root cause: every one of these triggers is
gated in the reference on `COALESCE((SELECT value FROM sync_flags WHERE key
= 'sync_enabled'), '0') = '1'`, reconciled against the live configuration on
every startup by `_reconcile_sync_enabled_flag` (`db.py`). This crate's
outbox triggers had no gate at all, so they fired unconditionally on every
write regardless of whether sync was ever configured — the reason `#59`'s
`prune_outbox` had to exist in the first place, working around a growth
problem the reference's own gate prevents at the source.

## Decision

**Regenerated `schema_tables.sql`/`schema_indexes.sql`/`schema_triggers.sql`**
from the reference's real schema code rather than hand-editing the confirmed
columns in. Concretely: `remind_me_mcp.db`'s `_ensure_schema` was imported
directly (stubbing only `embeddings`, which needs `numpy` purely to import,
irrelevant to DDL) against an in-memory `sqlite3.Connection`, and the
resulting `sqlite_master` dump — `IF NOT EXISTS` re-added and FTS5's own
shadow tables/`sqlite_sequence` excluded, matching this repo's existing
convention exactly — replaced the stale trigger/index definitions. This
stays true to "generated verbatim, not hand-written" (`migrations.rs`'s own
documented contract) without needing a real `remind_me` server process
running (which `#76`'s own investigation found blocked on an unrelated MCP
SDK version mismatch in this environment).

Net schema diff: `idx_memories_source_capture_id` (a missing index),
`memories_outbox_ai`/`_au` gained the `WHEN` gate and three payload columns,
and the four graph-table outbox triggers moved from `sync::graph`'s
hand-rolled `ensure_schema` into the generated file itself (removing that
function and its `OWN_ADDITIONS` allowlist entry in `schema_test.rs`
entirely — they are no longer a second, diverging source of truth).

**Added `sync::reconcile_sync_enabled_flag`**, called on every
`initialize_schema` (matching the reference's every-startup call site),
implementing `_reconcile_sync_enabled_flag`'s exact stored/desired matrix:

- already matches: no-op.
- stored `"0"`, now enabled: backfill `sync_outbox` with an `insert` row for
  every current `memories`/`entities`/`memory_entities` row (**not**
  `entity_relations`, matching the reference's own omission exactly rather
  than silently covering more than a first sync from the reference would).
- now disabled (from any prior stored state): `sync_outbox`/`sync_sends` are
  cleared.
- unset (a fresh database) and now enabled: **no backfill** — reproducing the
  reference's exact stored/desired matrix even though its own stated
  rationale ("pre-gate triggers were unconditional, so the outbox is already
  complete") describes reference history this crate never had. One
  reconciliation function matching the matrix beats two diverging ones for
  "true fresh" vs. "upgraded from an older, once-ungated build."

**`SyncRecord` gained `deleted_at`**, applied through the exact same LWW path
as every other column (no special-casing) — this is what actually lets a
tombstone propagate. Deliberately did **not** add `doc_id`/`chunk_index`:
the reference's own receiving side (`sync.py`'s `_upsert_one`) sends them on
the wire for column-list parity but never reads them back off an incoming
record either, so there is nothing for this port to apply.

## Alternatives considered

**Hand-editing the three confirmed columns and the `WHEN` clause into the
existing `schema_triggers.sql` without a real re-dump.** Rejected: this file
is documented as generated verbatim, and the earlier hand-transcribed
migration ladder (referenced in `migrations.rs`'s own history) is the exact
class of mistake this crate moved away from. Running the reference's actual
schema code, even standalone, stays on the right side of that line.

**Skipping the `entities_outbox_*` moves and leaving `sync::graph`'s
hand-rolled triggers in place, just adding the gate to them separately.**
Rejected: once the fresh dump showed the reference ships these triggers
itself, keeping a hand-rolled copy would mean this crate maintains its own,
separately-evolving definition of something that already has a canonical
source — exactly the divergence risk the generated-schema convention exists
to prevent.

**Embedding `#76`'s tombstone-cleanup step (removing a remotely-tombstoned
memory's chunk vectors)** now. Deferred: that step depends on `#49`'s vector
storage, which isn't part of this branch. Tracked as a follow-up once `#49`
lands, not invented ahead of it.

## Consequences

- A node that has never configured sync now queues nothing in
  `sync_outbox` at all, for any table — the growth problem `#59`'s
  `prune_outbox` works around no longer occurs at the source for a node that
  never turns sync on, matching the reference exactly.
- A soft-deleted memory's tombstone now reaches every synced node, closing
  the concrete gap `#76` was filed over.
- Every test that asserted a local write reaches `sync_outbox` without
  configuring sync had to start configuring it first — a mechanical but
  wide-reaching test update (`outbox_test.rs`, `sync_test.rs`,
  `graph_sync_test.rs`, `schema_test.rs`), since the whole point of the gate
  is that an unconfigured node's writes must not reach the outbox at all.
- A periodic hard-delete compaction pass for old tombstones
  (`sync._compact_tombstones` in the reference) is not implemented here —
  noted as a real, separate gap for a follow-up issue, not silently rolled
  into this one.
