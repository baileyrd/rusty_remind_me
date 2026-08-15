# Gap analysis: rusty_dbs vs baileyrd/Daily-Backup-System

Reference pinned at `daily-backup-system@6cc6491` (default branch tip at
assessment time). Target (`rusty_dbs`) has zero code — no `Cargo.toml`, no
`src/` — so this is the "no comparable surface to diff, no target roadmap"
path: capabilities extracted by reading the reference directly (README,
`docs/architecture.md`, `docs/ROADMAP.md`, `docs/BACKLOG.md`, and the
`src/dbs` module tree), not a mechanical `cargo public-api` diff.

**Scope for this run** (settled 2026-08-12, user-confirmed): full feature
parity — every module below is in scope, including the 20 already-shipped
`ROADMAP.md` items (they're part of the reference's *current* shipped
surface, not future plans to skip) and all 14 connectors. Platform floor:
cross-platform from round 1 — libc floor on Linux, windows-sys floor on
Windows, per the rustils-style RFC v2 convention.

**RustyMill sibling check — partial.** This session is locked to the
`baileyrd` owner tier (already has `rusty_dbs`/`skill_pack`/
`daily-backup-system` attached) and cross-owner attach of `Rusty-Mill/*`
repos is refused ("cross-tier adds are not supported"). Anonymous
unauthenticated clone also failed for the `Rusty-Mill` org. So the
"Existing RustyMill impl" column below is **purpose-name-only**, from
`references/platform-directory.md`, not source-verified — flagged
per-row as `(unverified)`. Real verification (read the actual source, not
just the name) needs to happen in step 3 when each issue is picked up,
from a session that can reach `Rusty-Mill/*` — e.g. a fresh session seeded
from that org, not this one.

Granularity here is module/feature-level, not symbol-level (this is an
application, not a library with a diffable public API) — **L-sized rows
get split into multiple issues at filing time**, per the skill's own rule,
not left as one oversized issue.

## Core / engine (foundational — nothing else can be built without this)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Core data model (`Item`, `MediaRef`, `Checkpoint`, `RunResult`, etc.) | type | spec | both | `src/dbs/core/models.py` | — | no | M | Done (#2) — foundation everything else depends on |
| Error hierarchy (`ConnectorConfigError`/`ConnectorAuthError`/`TransientFetchError`/`RateLimitedError`/`ConnectorContractError`) | type | spec | both | `src/dbs/core/errors.py` | — | no | S | Done (#3) |
| Connector contract + `Capabilities` | type | spec | both | `src/dbs/core/connector.py`, `capabilities.py` | — | no | M | Done (#4) — the trait/interface every connector implements |
| Plugin registry / discovery | fn | spec | both | `src/dbs/core/registry.py` | — | no | L | Done (#5 ADR, #45 implementation) — subprocess + line-delimited JSON-IPC per ADR-0001; #45 covers handshake/contract-validation/version-gating/collision-resolution only (protocol steps 1 and 4) — see the `run_source` row below for steps 2-3 |
| Engine — cursor/checkpoint transaction safety | fn | spec | both | `src/dbs/core/engine.py` | rusty_db (unverified) | no | M | Done (#16) — "cursor never gets ahead of data" invariant |
| Engine — idempotent upsert + content-hash classification | fn | spec | both | `src/dbs/core/engine.py`, `hashing.py` | rusty_db (unverified) | no | M | Done (#7, #17) |
| Engine — revision history writing | fn | spec | both | `src/dbs/core/engine.py` | — | no | S | Done (#19) |
| Engine — soft-delete sweep + safety-fraction guard | fn | spec | both | `src/dbs/core/engine.py` | — | no | M | Done (#20) — data-safety critical, the 50%-mass-delete guard |
| Engine — crash-recovery reaper | fn | spec | both | `src/dbs/core/engine.py` | — | no | S | Done (#21) |
| Engine — least-privilege secrets scoping | fn | spec | both | `src/dbs/core/secrets.py` | — | no | S | Done (#6) |
| Managed HTTP client (backoff, `Retry-After`, rate limit) | fn | spec | both | `src/dbs/core/http.py` | rusty_http / rusty_request (unverified) | no | M | Done (#22) — `[dbs] http_timeout`/`http_rate_limit_per_min` reach the connector's actual client (#209), threaded through `WireRunContext` since the host never makes the connector's own HTTP calls |
| Timeutil helpers | fn | spec | both | `src/dbs/core/timeutil.py` | — | no | S | Done (#8) |
| `CORE_API_VERSION` gating | fn | spec | both | `src/dbs/core/versioning.py` | — | no | S | Done (#9) |
| Cooperative cancellation (Ctrl+C → finish in-flight, no new starts) | fn | spec | both | `src/dbs/core/cancel.py` | — | no | S | Done (#10, #67) — `CancelToken` primitive landed in #10; `backup_all`/CLI wiring landed in #67 |
| `netns` helper | fn | spec | linux | `src/dbs/core/netns.py` | — | no | S | Done (#24) — Linux-only, degrades to a safe `false` off-Linux |
| `BackupService` (UI-agnostic façade: `backup_source`/`backup_all`, connector instantiation via the registry, VPN guard checks, status/history rendering, once-per-call crash-recovery reap threading) | type+fn | spec | both | `src/dbs/core/service.py` | — | no | L | Done (#21 reap-once slice, #46 the rest) |
| Engine — `run_source` orchestrator / connector run-stream bridge (drives one connector's actual fetch: writes a `RunContext`, reads the `FetchEvent` stream back over ADR-0001's subprocess protocol — steps 2-3) | fn | spec | both | `src/dbs/core/engine.py` (`Engine.run_source`) | — | no | M | Done (#157) — new `dbs-core::run_stream` module (`WireRunContext`/`WireLine`/`WireOutcome` + `run_connector_subprocess`) and `SubprocessRunner`, the real `ConnectorRunner` now wired into all 17 `dbs-cli` call sites, replacing `UnimplementedRunner`. Cancellation actually kills the connector subprocess (`Child::kill`), a real improvement over the reference's abandon-only in-process generator. Surfaced two further gaps, both since closed: `dbs-cli` passing an always-empty `ConnectorRegistry::from_resolved([])` (fixed by real discovery, #160/#170) and none of the 14 `dbs-connector-*` crates being real subprocess binaries (fixed, #164). `[dbs] batch_max` reaches `run_connector_subprocess`'s actual flush cadence now too (#210), replacing a hardcoded `BATCH_MAX` constant |

## Storage

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `Storage` trait (ABC) | type | spec | both | `src/dbs/storage/base.py` | rusty_db (unverified) | no | S | Done (#11) |
| SQLite storage — schema + migrations | fn | spec | both | `src/dbs/storage/{sqlite,migrations}.py` | rusty_db (unverified) | no | M | Done (#12) — `rusqlite` dependency added |
| SQLite storage — upsert/classify/revisions | fn | spec | both | `src/dbs/storage/sqlite.py` | rusty_db (unverified) | no | M | Done (#36) |
| SQLite storage — browse/query + FTS5 search | fn | spec | both | `src/dbs/storage/sqlite.py` | rusty_db (unverified) | no | M | Done (#36, #47, #48) |
| SQLite storage — metrics aggregation | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | S | Done (#36) |
| SQLite storage — maintenance (VACUUM, WAL checkpoint, `PRAGMA optimize`, snapshot) | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | M | Done (#36, orchestrated #195) — `Storage::maintain`/`prune_revisions`/`vacuum_into` (#36) had no caller above the storage layer until #195 added `BackupService::maintain` (revision pruning per source's `keep_revisions`, then `maintain`, then an optional `vacuum_into` snapshot — populating the previously-unused `MaintenanceReport`) and wired `dbs maintain [--vacuum] [--snapshot PATH] [--json]` to call it, replacing the earlier generic CLI stub |
| SQLite storage — FTS5 full-text index (`_ensure_fts`: virtual table + triggers + backfill, `browse_items`' MATCH-then-LIKE fallback) | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | S | Done (#47) |
| SQLite storage — `browse_items` video-link thumbnail fallback (YouTube/Loom/Vimeo URL detection when an item has no image media) | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | S | Done (#48) |

## Config & secrets

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Config loading (`dbs.toml` + `.env`, per-source blocks, inline-secret rejection) | fn | spec | both | `src/dbs/config.py` | — | no | M | Done (#13) — `toml` dependency added |
| Export profile (`ExportProfile`/`ExportProfileOverride` — per-source export rules: which item kinds export, wiki grouping) | type | spec | both | `src/dbs/core/export_profile.py` | — | no | S | Done (#49) |
| Webhook notification (`notify_url`/`notify_on` — POST a batch summary after `dbs backup`/`--all`, Slack/Discord-compatible payload) | fn | spec | both | `src/dbs/core/service.py::notify_results` | — | no | S | Done (#208) — `BackupService::notify_results`, called from the CLI `backup` path and both web-triggered (`/api/backup`) and scheduled (`dbs serve --schedule`) batch jobs |

## Crypto

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Encryption at rest / encrypted exports (scrypt + AES-256-GCM) | fn | spec | both | `src/dbs/crypto.py` | rusty_tls (unverified — TLS-focused, may not cover AEAD/KDF) | no | M | Done (#52) — `rusty_tls` still unreachable (cross-tier `add_repo` refused), so it stays unverified; added `aes-gcm`/`scrypt`/`rand` dependencies |

## Export (7 formats)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Export base + filter query | fn | spec | both | `src/dbs/export/base.py` | — | no | S | Done (#50) — `ExportResult`/`Exporter`/`ExportSource` (the exporter trait/summary types, as opposed to the query type) remain unported, picked up by the individual exporter issues |
| JSON exporter | fn | spec | both | `src/dbs/export/json.py` | rusty_json (unverified) | no | S | Done (#51) — also lands the shared `Exporter`/`ExportSource`/`ExportResult`/`get_exporter` base from `export/base.py`, deferred from #50 |
| NDJSON exporter | fn | spec | both | `src/dbs/export/ndjson.py` | rusty_json (unverified) | no | S | Done (#53) |
| CSV exporter | fn | spec | both | `src/dbs/export/csv.py` | — | no | S | Done (#54) |
| Markdown exporter | fn | spec | both | `src/dbs/export/markdown.py` | — | no | S | Done (#55) |
| Obsidian vault exporter | fn | spec | both | `src/dbs/export/obsidian.py` | — | no | M | Done (#56) — added the `zip` crate dependency (pulled forward from this row's own note below) |
| Wiki exporter (topic/item grouping, wikilinks) | fn | spec | both | `src/dbs/export/wiki.py` | — | no | M | Done (#57) — added `ExportSource::profiles()`, a new default-empty trait method the per-source `ExportProfile` grouping/`page_per` overrides read through |
| Self-describing archive exporter (checksummed manifest zip) | fn | spec | both | `src/dbs/export/archive.py` | — | no | M | Done (#58) — reused the `zip` crate dependency already added in #56 (Obsidian exporter), no new dependency needed |

## Restore, verify, maintain, notes/wiki helpers

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Restore (`dbs restore`, dry-run, manifest/schema-version validation) | fn | spec | both | `src/dbs/restore.py` | — | no | M | Done (#59, CLI wired #195) — orchestrator landed as `BackupService::restore` (`src/dbs/core/service.py`'s split, not `restore.py`'s own scope); `dbs restore <path> [--dry-run] [--json]` now really calls it, replacing the earlier generic CLI stub |
| Verify (DB integrity + archive checksum check) | fn | spec | both | `cli.py` (`dbs verify`) | — | no | S | Done (#60, CLI wired #195) — orchestrator landed as `BackupService::verify`; archive checksum half already done in #59's `restore::verify_archive`; `dbs verify [source] [--archive PATH]` now really calls both, and `/api/verify` bridges `BackupService::verify` for real instead of 501ing |
| `notes_export.py` (incremental per-item markdown, collision map, state file) | fn | spec | both | `src/dbs/notes_export.py` | — | no | M | Done (#61) — pulled forward `BackupService::export` (an `ExportQuery`-to-file orchestrator) from #70, since this issue cannot exist without it |
| `templates.py` (`dbs init` scaffolding writer) | fn | spec | both | `src/dbs/templates.py` | — | no | S | Done (#62) — writer landed as a library function (`write_scaffolding`), same "no CLI crate yet" pattern as `BackupService::export`/`verify`/`restore` |

## CLI (`dbs.cli`, ~22 subcommands)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| CLI skeleton + `dbs init` | fn | spec | both | `cli.py` | — | no | M | Done (#63) — new `dbs-cli` binary crate; added the `clap` dependency named in this row |
| `dbs backup` — single-source run | fn | spec | both | `cli.py` | — | no | M | Done (#64) — every source currently reports "connector not found" since no on-disk connector-candidate discovery exists yet (implicit connectors-cluster prerequisite) |
| `dbs backup --all --only-due` scheduling gate | fn | spec | both | `cli.py` | — | no | S | Done (#65) |
| `dbs backup --parallel N` worker pool | fn | spec | both | `cli.py` | — | no | L | Done (#66) — sync `std::thread::scope` pool, consistent with #22's `reqwest::blocking` choice |
| `dbs backup` progress line + Ctrl+C handling | fn | spec | both | `cli.py` | — | no | S | Done (#67) — only `SourceStart`/`SourceDone` are emitted (no run/stream protocol yet for per-item progress) |
| `dbs status` / `dbs history` | fn | spec | both | `cli.py` | — | no | S | Done (#68) |
| `dbs items` / `dbs stats` (browse + FTS5 CLI) | fn | spec | both | `cli.py` | — | no | M | Done (#69) — full FTS5 search already available (landed with `Storage::browse_items`, not gated on this issue) |
| `dbs export*` / `dbs decrypt` CLI wiring | fn | spec | both | `cli.py` | — | no | M | Done (#70) |
| `dbs sources` / `dbs connectors` (list/add/check/describe) | fn | spec | both | `cli.py` | — | no | M | Done (#71) — `check`/`add`/`describe` validate only connector *resolvability*, since a subprocess connector has no in-process config schema to validate against |
| `dbs doctor` | fn | spec | both | `cli.py` | — | no | M | Done (#72) — Pydantic option/dependency validation and yt-dlp version checks have no equivalent in this port's architecture |
| `dbs update-ytdlp` | fn | spec | both | `cli.py` | — | no | S | Done (#73) — shells out to `python3 -m pip install --upgrade yt-dlp[default]`, per the Decisions section's subprocess strategy |
| `dbs schedule` (cron/systemd snippets) | fn | spec | both | `cli.py` | — | no | S | Done (#74) — Linux branch ports the reference's cron+systemd snippets exactly; Windows branch generates a `schtasks` command, no reference equivalent |
| `dbs serve` flag wiring | fn | spec | both | `cli.py` | — | no | S | Done (#75) — flags parse and validate for real; the actual server is still the Web tier row below |
| `dbs capture` (headless login capture) | fn | spec | both | `cli.py`, `web/setup.py` | — | no | L | Done (#76) — target resolution + default out-path per capture kind implemented; the actual browser-automation subprocess still depends on the strategy decision (see Connectors) |
| `dbs research` subcommands | fn | spec | both | `cli.py`, `research/*` | — | no | L | Done (#77, real pipeline wiring #189) — both `youtube` and `youtube-backup` call the real `dbs_research::pipeline::run_pipeline`/`run_pipeline_for_videos` (the latter converting `ItemRow`→`VideoMeta` for its matched videos, same conversion `dbs-web`'s `/api/research` uses); the NotebookLM synthesis step itself still depends on the NotebookLM client — see Research row |
| `dbs version` | fn | spec | both | `cli.py` | — | no | S | Done (#78) — prints `rusty_dbs <crate version> (core API v<N>)` |

## Web tier (`[web]` extra)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Web app skeleton + static SPA serving | fn | spec | both | `src/dbs/web/app.py` | rusty_http (unverified — likely client-only) | no | L | Done (#79) — new `dbs-web` crate (axum + tokio); serves the reference's real static SPA unmodified; `dbs serve` actually binds on loopback now. `/api` route layer landing incrementally — see #169's umbrella issue and the dashboard-status row below |
| Job manager (background jobs + SSE progress) | fn | spec | both | `src/dbs/web/jobs.py` | — | no | M | Done (#80) — `dbs-web::jobs`: generic submit/track/cancel + SSE, tested with a fake job; no `/api/backup` wiring yet (#174) and no VPN-subprocess routing (`BackupService` already refuses `requires_vpn` outside the namespace instead) |
| Auth / CSRF / Origin / Host protection | fn | spec | both | `src/dbs/web/app.py` (security gate) | — | no | M | Done (#81) — `dbs-web::auth::security_gate`, wired into `router()`/`dbs serve`; DNS-rebinding Host check, Origin-based CSRF defense, opt-in bearer token gating `/api`. `dbs serve` now binds off-loopback for real once `--token` is given |
| `envfile.py` (scoped secrets writer) | fn | spec | both | `src/dbs/web/envfile.py` | — | no | S | Done (#82) — `dbs-web::envfile`; the in-UI setup routes that call it are still #175 |
| In-UI setup (dependency install + browser-auth capture jobs) | fn | spec | both | `src/dbs/web/setup.py` | — | no | L | Done (#83) — `dbs-web::setup`: install job is fully real (derives + runs pip/playwright steps via the #80 job manager); capture job fails cleanly pending #99's Playwright helper. Route wiring is #175 |
| `/api` dashboard status routes (meta/status/metrics/history/vpn/verify) | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | M | Done (#170, first slice of #169's `/api` umbrella) — `dbs-web::api`; bridges into `BackupService`/`Storage` via `tokio::task::spawn_blocking`, the pattern every remaining `/api` slice (#171-#177) reuses. `dbs serve` now loads a real `Config` at startup (previously never touched `dbs.toml`). `dbs_core::build_registry`/`connector_search_dirs` generalized out of a `dbs-cli`-private helper so both crates build the identical registry. `SourceStatus` gained `requires_vpn` (the shipped frontend already reads it). `/api/verify` bridges `BackupService::verify` for real since #195 (originally 501'd pending `dbs verify` itself, which #195 also wired) |
| `/api` item browse & media routes (items/media/thumbnails) | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | M | Done (#171) — `dbs-web::api`; `GET /api/items` (multi-value `source`/`type` filters parsed by hand via `url::form_urlencoded`, `axum::extract::Query` can't collect repeated keys), `/api/items/:id` (bridges `get_item` directly, no gap), `/api/media/:id` (real binary response via `Storage::get_media_blob`), `/api/thumb/:id` (item-scoped: local image media, or a 307 to YouTube's thumbnail CDN for a derivable video) |
| `/api` sources & connectors management routes | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | M | Done (#172) — `dbs-web::api`; `GET /api/connectors` (bridges `list_connectors`, which gained a `ConnectorInfo::auth_capture`/`AuthCapture::per_source` gap-fill along the way), `GET /api/sources` (bridges `list_sources`), `POST /api/sources` (bridges `add_source`). `/import` routes moved to #175 (shared `dbs-web::setup` machinery) |
| `/api` secrets management routes | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | S | Done (#173) — `dbs-web::api`; `GET /api/secrets` (needed-vs-allowed key lists), `POST /api/secrets` (bridges `envfile::set_var`, allow-list checked), `DELETE /api/secrets/:name` (bridges `envfile::unset_var`, not allow-list checked). `Config` gained `env_file_path()` for the shared `<config dir>/.env` convention. Pure `envfile` (#82) plumbing, no CLI equivalent to mirror |
| `/api` backup trigger + live progress routes | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | M | Done (#174) — `dbs-web::api`; `POST /api/backup` (starts a `crate::jobs::Job` running `backup_source`/`backup_all`), `GET /api/backup/:id/stream` + `/current` (the #80 job primitive, nested), `POST /api/backup/:id/cancel`. Fixed a real gap in the #80 primitive along the way: `Job::subscribe` now emits a terminal `end` SSE event with the final snapshot, which every stream consumer (`app.js`) already expected but nothing produced — benefits #175/#177's streams too |
| `/api` in-UI setup & capture routes | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | M | Done (#175) — `dbs-web::api`; `POST /api/connectors/:type/install` (real, via #83's `setup::run_install_job`), `POST /api/connectors/:type/capture`/`POST /api/sources/:name/capture` (resolve for real, then fail cleanly pending #99), `POST /api/connectors/:type/import`/`POST /api/sources/:name/import` (multipart upload → validate → write → register secret, moved here from #172). `allow_setup` (`--no-setup`) now actually gates these server-side |
| `/api` export routes | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | S | Done (#176, `wiki_grouping` + `/profiles` wired #199) — `dbs-web::api`; `GET /api/export` (a real file download via `BackupService::export` to a temp file, `Content-Type`/`Content-Disposition` from `Exporter::media_type`/`file_ext`; now reads `wiki_grouping` from the query string instead of always defaulting to `"topic"`, validated by the existing `WikiExporter`), `GET /api/export/profiles` (bridges `BackupService::export_profiles()`, previously missing entirely), `POST /api/export-notes` (bridges `dbs_core::export_notes` directly) |
| `/api` research routes (YouTube pipeline + NotebookLM) | fn | spec | both | `src/dbs/web/app.py` (API routes) | — | no | M | Done (#177, closes the #169 umbrella) — `dbs-web::api`; `/api/research/meta`/`install`/`login`/`current`/`:id/stream`/`:id/report` plus `POST /api/research` (bridges `dbs_research::pipeline::run_pipeline`/`run_pipeline_for_videos` for real, converting `ItemRow`→`VideoMeta` for backup mode). Every real run still fails cleanly at the NotebookLM step — Decision 4's adapter remains deferred per #84 |
| `dbs serve --schedule` background scheduler | fn | spec | both | `src/dbs/web/app.py` (`create_app`'s scheduler lifespan) | — | no | S | Done (#190) — new `dbs-web::scheduler`: a background loop wakes every 60s (this port's `--schedule` is a bare flag, not the reference's float-seconds interval, so the tick cadence is fixed rather than a new CLI knob), checks `BackupService::due_sources` (new public method, extracted from `backup_all`'s existing `only_due` filter), and starts an `{all: true, only_due: true}` job on the same `JobManager` `/api/backup` uses — a scheduled run shows up in the UI's live progress/history like any other |

## Research subsystem (NotebookLM-dependent, sits outside the connector model)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Research pipeline (YouTube search → NotebookLM synthesis → report) | fn | spec | both | `src/dbs/research/*` | — | no | L | Done (#84) — new `dbs-research` crate: YouTube search (real, yt-dlp subprocess) + report rendering are fully real; NotebookLM sits behind a `NotebookLmClient` trait whose concrete `nlm`/`notebooklm-mcp` adapter (Decision 4) is deferred pending that tool's confirmed CLI surface. Wired into `dbs-cli`'s `dbs research` (#77, #189) and `dbs-web`'s `/api/research` (#177) — every real run reaches this step and fails cleanly at `NotebookLmClient` |

## Connectors (14 — each a natural one-issue unit; "template" per README's own A/B split)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `raindrop` (template A: REST+token, real delta endpoint) | fn | spec | both | `connectors/raindrop.py` | — | no | M | Done (#85, subprocess binary #161, discovery #160) — new `dbs-connector-raindrop` crate; full incremental/reconcile/full fetch + trash polling against fixture HTTP. A real `dbs-connector-raindrop` executable speaking ADR-0001's full protocol (handshake + run/stream) since #161, the first of the 14 to be one. With #160's real `PATH`/`connectors_dir` candidate discovery also landed, `dbs backup <name>` can genuinely discover and run this connector end to end — the first connector in this port reachable from a real `dbs backup` invocation, not just its own crate's tests. `archive_permanent_copy` (opt-in, Pro-only) not ported. `configure()` (#212) reads `collection_id`/`nested`/`include_types`/`page_size`/`overlap_seconds`/`poll_trash` from a real source's config, per ADR-0002 |
| `github` (template A: stars/gists) | fn | spec | both | `connectors/github.py` | — | no | M | Done (#86) — new `dbs-connector-github` crate; stars (watermark+overlap early-stop) and gists (real `since=` delta) fetch, combined reconcile marker withheld if either kind is disabled. Real `dbs-connector-github` binary since #164 (`src/main.rs` + subprocess integration test), reachable from a real `dbs backup` run. `configure()` (#211) reads `include_stars`/`include_gists`/`page_size` from a real source's config, per ADR-0002 |
| `pinboard` (template A: `posts/update` delta) | fn | spec | both | `connectors/pinboard.py` | — | no | M | Done (#87) — new `dbs-connector-pinboard` crate; `posts/update` global change signal short-circuits a no-op run to one request, `posts/all?fromdt=` delta on change, full/reconcile yields a `ReconcileMarker`. Real `dbs-connector-pinboard` binary since #164, reachable from a real `dbs backup` run |
| `readwise` (template A: `updatedAfter` cursor) | fn | spec | both | `connectors/readwise.py` | — | no | M | Done (#88) — new `dbs-connector-readwise` crate; real `updated__gt` server-side delta per kind (books/highlights), server-`next`-URL pagination, combined `ReconcileMarker` withheld if either kind disabled. Real `dbs-connector-readwise` binary since #164, reachable from a real `dbs backup` run. `configure()` (#213) reads `include_books`/`include_highlights`/`page_size` from a real source's config, per ADR-0002 |
| `mastodon` (template A) | fn | spec | both | `connectors/mastodon.py` | — | no | M | Done (#89) — new `dbs-connector-mastodon` crate; full enumeration every run (no usable delta filter), `Link`-header pagination, combined `ReconcileMarker` withheld if either kind disabled. Real `dbs-connector-mastodon` binary since #164, and since #166/ADR-0002 its `configure()` reads `instance` from a source's real `[sources.NAME]` config over the subprocess wire — a real `dbs backup` run against a real Mastodon instance works end to end now, no test-only env var needed |
| `bluesky` (template A) | fn | spec | both | `connectors/bluesky.py` | — | no | M | Done (#90) — new `dbs-connector-bluesky` crate; app-password session exchange, `listRecords` cursor pagination, full enumeration every run (no usable delta filter) followed by a `ReconcileMarker`. Real `dbs-connector-bluesky` binary since #164, reachable from a real `dbs backup` run. `identifier` never blocked a run (nothing validates it's non-empty before the HTTP layer), but since #166/ADR-0002 its `configure()` also reads the real `identifier` from a source's config, so a real run now authenticates as the right account instead of an opaque empty string |
| `spotify` (template A, catalog-only) | fn | spec | both | `connectors/spotify.py` | — | no | M | Done (#91) — new `dbs-connector-spotify` crate; refresh-token OAuth exchange each run, `github`-style stars-pattern early-stop on liked tracks, offset-paginated playlist catalog, combined `ReconcileMarker` withheld if either kind disabled. Real `dbs-connector-spotify` binary since #164, reachable from a real `dbs backup` run. `configure()` (#214) reads `include_liked_tracks`/`include_playlists`/`page_size` from a real source's config, per ADR-0002 |
| `pocketcasts` | fn | spec | both | `connectors/pocketcasts.py` | — | no | M | Done (#92) — new `dbs-connector-pocketcasts` crate; unofficial web-player API login, full walk of subscriptions/starred/history every run (no delta filter), combined `ReconcileMarker` (soft-deletes aging-out history too, by design) withheld if any kind disabled. Real `dbs-connector-pocketcasts` binary since #164, reachable from a real `dbs backup` run. `configure()` (#215) reads `include_subscriptions`/`include_starred`/`include_history` from a real source's config, per ADR-0002 |
| `podcast` (generic RSS-shaped) | fn | spec | both | `connectors/podcast.py` | — | no | M | Done (#93) — new `dbs-connector-podcast` crate; RSS 2.0/Atom parsing via `roxmltree` (first XML dep in the workspace), OPML merge+dedup, optional enclosure download, deletion detection deliberately disabled (rolling-window feeds, no `ReconcileMarker` ever). Real `dbs-connector-podcast` binary since #164, and since #166/ADR-0002 its `configure()` reads `feeds` (its only "target" — no fixed API host exists to configure otherwise) from a source's real config over the subprocess wire — a real `dbs backup` run now has something to fetch |
| `vimeo` | fn | spec | both | `connectors/vimeo.py` | — | no | M | Done (#94) — new `dbs-connector-vimeo` crate; full-enumeration `/me/videos` catalog + `ReconcileMarker`, opt-in `download_videos` via the `yt-dlp` binary (first connector using `dbs-connector-support`'s `run_with_watchdog`, stdout-line heartbeat, unconditional `--impersonate chrome`). Real `dbs-connector-vimeo` binary since #164, reachable from a real `dbs backup` run. `configure()` (#217) reads `page_size`/`download_videos`/`downloads_dir`/`video_quality`/`video_stall_timeout` from a real source's config, per ADR-0002 — `download_videos` can now actually be turned on from a real config, not just the connector's own default |
| `udemy` (template B-ish) | fn | spec | both | `connectors/udemy.py` | — | no | M | Done (#95) — new `dbs-connector-udemy` crate; course+lecture/quiz curriculum walk, full enumeration with per-course partial-failure marker withholding, opt-in `download_videos` via `yt-dlp` + `dbs-connector-support`'s `run_with_watchdog` (second user after `vimeo`, #94). Real `dbs-connector-udemy` binary since #164, reachable from a real `dbs backup` run. `configure()` (#216) reads `page_size`/`course_filter`/`download_videos`/`video_format`/`download_timeout` from a real source's config, per ADR-0002 — `download_videos` can now actually be turned on from a real config, not just the connector's own default |
| `reddit` (template B: browser-session, full enumeration) | fn | spec | both | `connectors/reddit.py` | — | no | L | Done (#96, acquisition wired #187) — new `dbs-connector-reddit` crate; config/capabilities/`auth_capture`/`export_profile` ported, every pure mapping function (listing → record, record → `BackupItem`, opportunistic outbound-link fetch) implemented and tested, and `fetch()`'s acquisition step now shells out for real to a Playwright-driven Python script (`scripts/acquire.py`, embedded via `include_str!`) through #99's `run_python_script` — the script's only job is browser automation/pagination; it hands back raw listing JSON and Rust does the actual record mapping. Tested against a fake acquisition-script stub (no real Playwright/network in CI). Real `dbs-connector-reddit` binary since #164, reachable from a real `dbs backup` run — a real login capture (still pending `dbs-web::setup::run_capture_job`, #99's other remaining caller) is the only thing standing between this and end-to-end real data |
| `skool` (template B + native HLS video download via yt-dlp/ffmpeg) | fn | spec | both | `connectors/skool.py` | — | no | L | Done (#97, catalog acquisition wired #188) — new `dbs-connector-skool` crate; config/capabilities/`auth_capture` ported, every genuinely pure function (`__NEXT_DATA__` BFS search, lesson-field decoding, course-selector matching, Mux HLS URL reconstruction, membership/course/lesson parsing, community/course/lesson → `BackupItem` mapping) implemented and tested, and `fetch()` now really walks communities → selected courses → lesson trees via two calls to a Playwright-driven `scripts/acquire.py` (embedded via `include_str!`) through #99's `run_python_script` — the script only navigates and hands back raw `__NEXT_DATA__` blobs; Rust does 100% of the parsing with the same functions its fixture-data tests already exercised. Deliberately **catalog-only**: no per-lesson page visits (so `videoLink`/`videoId`/`resources` only populate when the course-tree payload itself carries them), no resource/video downloads, no `.meta.json` resume, no GitHub-zip archiving — every community backs up the way the reference's `no_download_communities` mode already works. Per-lesson enrichment + the download pipeline are a follow-up. Real `dbs-connector-skool` binary since #164, reachable from a real `dbs backup` run. `configure()` (#200) reads `communities`/`courses`/`no_download_communities` from a real source's config, per ADR-0002 — the already-tested `course_selected()` scoping is now actually reachable, not just auto-discover-everything |
| `youtube` (yt-dlp-based) | fn | spec | both | `connectors/youtube.py` | — | no | L | Done (#98) — new `dbs-connector-youtube` crate; unlike `reddit`/`skool`, needs no Playwright browser (the reference itself is yt-dlp-only), so `yt-dlp --dump-single-json --flat-playlist` shells out directly and `fetch()` is fully implemented and tested end to end against a fake `yt-dlp` script, not blocked on #99. Real `dbs-connector-youtube` binary since #164 (redirects via `with_yt_dlp_bin` to a fake script instead of a mock HTTP server, since this connector has no HTTP layer), reachable from a real `dbs backup` run. `configure()` (#218) reads `watch_later`/`liked`/`history`/`playlists`/`max_history`/`extract_timeout`/`cookies_from_browser` from a real source's config, per ADR-0002 |
| Shared Playwright launch helper | fn | spec | both | `connectors/_playwright.py` | — | no | M | Done (#99) — new `dbs-connector-support::python_launch` module. The reference's `launch_scrubbed_context` drives Playwright in-process (no Rust equivalent), so this ports the module's *role*, not its code: a generic, Playwright-agnostic subprocess launcher (`find_python`/`run_python_script`/`run_python_script_using`) that a connector shells a separate Python/Playwright script out to, reusing `run_with_watchdog` for the stall timeout. `reddit` (#187) and `skool` (#188) are its real callers — `launch_scrubbed_context` itself is inlined into each connector's own `scripts/acquire.py` (a private implementation detail per-script, same as the reference keeps it private per-connector). `dbs capture`/in-UI browser-session capture remains unwired |
| Tiptap rich-text→Markdown helper | fn | spec | both | `connectors/_tiptap.py` | — | no | S | Done (#100) — new `dbs-connector-support::tiptap` module, a node-for-node port of the reference (paragraphs, headings, code blocks, blockquotes, bullet/ordered lists incl. nesting, horizontal rules, images, hard breaks, and text marks bold/italic/code/strike/link, with `]` escaped in link text). Wired into `skool`'s lesson `body` (previously the raw unrendered `desc` string, per #97's note) |
| Shared watchdog/timeout helper | fn | spec | both | `connectors/_util.py` | — | no | S | Done (#14) — `dbs-connector-support::watchdog`'s `run_with_watchdog`, reused by `vimeo` (#94), `udemy` (#95), and `python_launch` (#99) |

---

## Decisions (resolved 2026-08-12, user-confirmed)

1. **Foundational dependencies → standard external crates.** `rusqlite`
   (SQLite), `serde` + `toml`/`serde_json` (config/JSON), `clap` (CLI),
   `reqwest` (HTTP client), `tokio` (async runtime), `zip` (archive
   export), `aes-gcm` + `scrypt` (encryption). Each gets named explicitly
   in the PR that first adds it, per the repo's dependency-justification
   convention — this decision pre-approves *that* a crate is used, not
   every specific crate choice made along the way.
2. **Plugin registry → true dynamic plugin loading.** Connectors are
   separate dynamically-loaded libraries (`cdylib` + a stable ABI, or a
   subprocess/IPC boundary), closer to Python's entry-point model than a
   compiled-in registry. This is the highest-engineering-cost decision in
   the table (ABI stability, versioning, sandboxing) — its own issue,
   sized L, needs a design doc/ADR before implementation starts, not a
   straight port.
3. **Browser-automation connectors (`reddit`, `skool`, `youtube`,
   `dbs capture`, in-UI setup) → shell out to Python/yt-dlp.** rusty_dbs
   invokes the existing yt-dlp (and a small Playwright-Python helper where
   needed) as a subprocess rather than reimplementing browser automation
   in Rust. Pulls in a Python-runtime dependency, scoped to exactly these
   surfaces.
4. **Research subsystem → in scope, same subprocess pattern.** Uses
   [jacob-bd/gemini-notebook-mcp-cli](https://github.com/jacob-bd/gemini-notebook-mcp-cli)
   (Python; wraps NotebookLM via browser-automation cookie extraction
   against undocumented internal APIs; ships both a CLI (`nlm`) and an MCP
   server (`notebooklm-mcp`), MIT-licensed) — rusty_dbs shells out to the
   `nlm` CLI or runs `notebooklm-mcp` as a subprocess/MCP client, the same
   integration shape as decision 3. Any port would need to replicate its
   cookie-extraction + CSRF-refresh handling, not just its HTTP calls.
5. **Minor standard crates → auto-approved, named in the PR (decided
   2026-08-12, mid-implementation).** Small, narrowly-scoped, widely-used
   crates (`sha2`, `csv`, `uuid`, `urlencoding`/`percent-encoding`, and
   similar single-purpose RustCrypto/rust-lang-adjacent tier crates) no
   longer need an individual stop-and-ask — the earlier foundational-
   dependency decision only enumerated the big, architecturally-visible
   ones (SQLite, TOML/JSON, CLI parsing, HTTP, async runtime, zip,
   AES-GCM/scrypt), and dozens of small equivalents were always going to
   surface across 66 rows. Still stop and ask for anything with real
   weight: a new architectural choice, broad-scope crates (a web
   framework, a database engine), or anything replacing an
   already-decided piece.
