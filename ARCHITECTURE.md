# Architecture

This file covers only the merged repository. Each half's own architecture
document is unchanged and remains authoritative for it:

- [`remind_me/ARCHITECTURE.md`](./remind_me/ARCHITECTURE.md) — crate
  boundaries, the ACT-R vitality model, the RRF scoring model, and the SQLite
  schema of the memory engine.
- [`dbs/ARCHITECTURE.md`](./dbs/ARCHITECTURE.md) — ports and adapters, the
  connector subprocess protocol, and the backup data flow.

## Shape

Two products, one Cargo workspace, two directory trees:

```
rusty_recall/
├── Cargo.toml          the only [workspace]; 25 members, 17 binaries
├── Cargo.lock          one resolution for both halves
├── remind_me/          6 crates → rusty-remind-me, rusty-remind-me-hub
└── dbs/                19 crates → dbs + 14 dbs-connector-<type>
```

There is no top-level crate and no crate that depends on both halves. The
merge unified the *build*, not the code: nothing in `remind_me/` links
anything in `dbs/`, and vice versa.

## The one place the halves meet

`remind_me` reads `dbs`'s output, in two directions that both avoid a Cargo
dependency:

| Path | Mechanism |
| ---- | --------- |
| [`remind_me_core::dbs_import`](./remind_me/crates/remind_me_core/src/dbs_import.rs) | Opens a `dbs` archive `SQLITE_OPEN_READ_ONLY` and reads its `items`/`sources` tables with hand-written SQL. Each live item becomes a memory; its source and tags become graph entities. |
| `remind_me wiki-import` | Ingests the markdown `dbs export-wiki` writes. |

Both are deliberately dependency-free couplings, and the read-only flag is a
safety property rather than a style choice: the file on the other side is
someone's backup archive, and this workspace should not be able to damage it
even by accident. Merging the repositories did not change that, and should
not be taken as licence to change it — a `dbs-core = { path = "../..." }` in
`remind_me_core` would hand it a writable storage API over that same file.

What the merge *did* change is that this coupling is now checkable. The
schema on one side and the reader on the other can move in one commit, and
one `cargo test --workspace` run exercises both.

## Two databases

Unchanged by the merge, and load-bearing:

- `remind_me` keeps `~/.remind-me/memory.db` at the schema version upstream
  `remind-me` reports. That parity is the point — a version mismatch makes
  the database silently unreadable to the reference implementation, which is
  what `remind_me/scripts/check_schema_drift.sh` guards in CI.
- `dbs` keeps its own archive under its own `items`/`sources` schema,
  append-heavy and large.

Folding them into one file would break the first constraint to serve nothing
the import path does not already serve.

## Two async stacks

Also unchanged. `remind_me` is deliberately synchronous — plain OS threads,
`parking_lot::Mutex<rusqlite::Connection>` — with `remind_me_remote` the one
crate on `tokio`/`axum`/`rmcp`. `dbs-web` is `axum`/`tokio`. They are on
different `axum` majors (0.8 and 0.7 respectively) and are compiled
separately; see the known-follow-ups note in [README.md](./README.md).

## Build-level constraints the merge introduced

- **One `[workspace]`.** Adding a `[workspace]` table to a crate inside
  `remind_me/` or `dbs/` splits the build back into two, silently.
- **`links` crates must be unique.** `rusqlite`'s `bundled` feature pulls in
  `libsqlite3-sys`, which declares `links = "sqlite3"`. Both halves reach it,
  so both are pinned to one version in `[workspace.dependencies]`. This is
  the constraint that forced the only dependency bump the merge made
  (`remind_me` 0.31 → 0.32). Ordinary crates — `axum`, `reqwest` — carry no
  such rule and coexist at two majors.
- **One `Cargo.lock`.** It was built as the union of the two originals rather
  than re-resolved from scratch, so the merge is not also a dependency
  upgrade. That distinction had teeth: a from-scratch resolution moved 88
  crates, and one of them (`rmcp` 3.0.1 → 3.1.2) broke six of
  `remind_me_remote`'s stateless-MCP tests.
