# Architecture

## Overview

A Rust reimplementation of
[baileyrd/Daily-Backup-System](https://github.com/baileyrd/Daily-Backup-System)
(pinned `@6cc6491`): a host process (`dbs-core`, driven by `dbs-cli` or
`dbs-web`) incrementally pulls items from configured sources through a
plugin protocol, classifies/upserts them into SQLite, and exposes the
result through a CLI, an optional local web UI, and export formats.

## Boundaries

Domain logic (`dbs-core`) is kept free of I/O specifics behind a small set
of ports; each has one production adapter today.

| Port | Adapter(s) | Notes |
| ---- | ---------- | ----- |
| `Connector` (`dbs-core::connector`) | 14 `dbs-connector-<type>` crates, each compiled to its own subprocess binary | The plugin contract every backup source implements: `open`/`fetch`/`close`/`configure`/`capabilities`. Never called in-process by the host — see [ADR-0001](./docs/adr/0001-dynamic-plugin-registry.md). |
| `ConnectorRunner` (`dbs-core::service`) | `SubprocessRunner` (`dbs-core::run_stream`) | Spawns a connector binary, writes its `WireRunContext`, streams `FetchEvent`s back. `UnimplementedRunner` (test-only) is the other implementation. |
| `Storage` (`dbs-core::storage`) | `SqliteStorage` | Upsert/classify, revision history, soft-delete sweep, FTS5 search, browse/query, metrics, maintenance (VACUUM/snapshot). |
| `Exporter` / `ExportSource` (`dbs-core::export`) | `JsonExporter`, `NdjsonExporter`, `CsvExporter`, `MarkdownExporter`, `ArchiveExporter`, `ObsidianExporter`, `WikiExporter` | One item stream in, one output format out — `dbs export --format <name>` and the `/api/export` route both drive the same trait. |
| `ProgressSink` (`dbs-core::service`) | CLI stderr progress line, web UI SSE stream (`dbs-web::jobs`) | Same `BackupService` run, two presentation layers. |

## Structure

A Cargo workspace, modular-monolith style: one shared domain crate, thin
presentation crates on top, and one crate per connector so each ships as
its own subprocess binary.

```
crates/
  dbs-core/                domain: models, engine, storage, config, export,
                            secrets, http, service (BackupService façade),
                            run_stream (subprocess protocol host side)
  dbs-connector-support/   shared subprocess-binary scaffolding every
                            connector's main.rs uses (handshake, wire I/O,
                            Playwright/yt-dlp process helpers)
  dbs-cli/                 `dbs` binary — argv parsing + rendering only,
                            all behavior delegates to dbs-core
  dbs-web/                 `dbs serve` — axum app: SPA host, /api routes,
                            background-job manager, scheduler
  dbs-research/            `dbs research` — ad-hoc NotebookLM pipelines,
                            not part of the backup path
  dbs-connector-*/         14 crates (raindrop, github, pinboard, readwise,
                            mastodon, bluesky, spotify, pocketcasts,
                            podcast, vimeo, udemy, reddit, skool, youtube),
                            each a standalone `dbs-connector-<type>` binary
```

A component gets split into its own service only for a concrete forcing
function (independent scaling, a team/language boundary, hard fault
isolation). The connectors are the one place that line was already
crossed at the domain level — see ADR-0001 for why that's a subprocess
boundary rather than an in-process trait object.

## Data flow

1. `dbs backup <source>` (or `--all`) resolves the source's config, then
   `BackupService::backup_source` calls the `ConnectorRunner` port.
2. `SubprocessRunner` spawns `dbs-connector-<type>` (discovered via
   `ConnectorRegistry`, `PATH`/a configured connectors directory), writes
   a `WireRunContext` line (secrets scoped to exactly this connector's
   declared `secret_keys`, cursor/since-watermark, run mode, and this
   source's `[sources.NAME]` config map — see
   [ADR-0002](./docs/adr/0002-per-source-connector-config.md)), then reads
   one JSON line per `FetchEvent` (`Item`/`Checkpoint`/`ReconcileMarker`)
   until a terminal `WireOutcome`.
3. `dbs-core::engine` classifies each item (new/changed/unchanged via
   content hash), upserts it, writes revision history, and — on a
   reconcile run — soft-deletes anything not seen, guarded by
   `sweep_safety_fraction`.
4. Results land in SQLite (`SqliteStorage`), queryable via `dbs items`/
   `dbs stats`/the web UI's `/api/items`, exportable via `dbs export` /
   `/api/export`, and optionally summarized to a webhook (`notify_url`).
5. `dbs serve` wraps the same `BackupService` behind axum: `/api/backup`
   triggers a run, `/api/backup/:id/stream` (SSE) carries the same
   `ProgressEvent`s the CLI's progress line renders, and `--schedule`
   drives the same call on a timer (`dbs-web::scheduler`).

## Key decisions

See [docs/adr/](./docs/adr/) for the record of individual decisions and
their tradeoffs — notably the subprocess/JSON-IPC plugin boundary
(ADR-0001) and how per-source config crosses it (ADR-0002).

## Non-goals

- **No stable ABI / `cdylib` plugins.** Rejected in ADR-0001 in favor of
  the subprocess boundary — revisit only if per-item IPC overhead becomes
  a measured bottleneck, which is unlikely for a personal-backup tool's
  throughput.
- **No YAML config.** The reference's optional `pyyaml`-gated path isn't
  ported; TOML only.
- **No compiled-in static connector registry.** Explicitly rejected by the
  round-1 scope decision in favor of true dynamic subprocess discovery.
