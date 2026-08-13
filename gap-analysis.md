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
| Core data model (`Item`, `MediaRef`, `Checkpoint`, `RunResult`, etc.) | type | spec | both | `src/dbs/core/models.py` | — | no | M | Foundation everything else depends on; do first |
| Error hierarchy (`ConnectorConfigError`/`ConnectorAuthError`/`TransientFetchError`/`RateLimitedError`/`ConnectorContractError`) | type | spec | both | `src/dbs/core/errors.py` | — | no | S | |
| Connector contract + `Capabilities` | type | spec | both | `src/dbs/core/connector.py`, `capabilities.py` | — | no | M | The trait/interface every connector implements |
| Plugin registry / discovery | fn | spec | both | `src/dbs/core/registry.py` | — | no | L | Python uses entry-point discovery; Rust has no equivalent — this is an early **architecture decision** (compiled-in registry vs. dynamic loading), not a straight port. Flag for explicit design discussion before implementing. |
| Engine — cursor/checkpoint transaction safety | fn | spec | both | `src/dbs/core/engine.py` | rusty_db (unverified) | no | M | Split from engine below — "cursor never gets ahead of data" invariant |
| Engine — idempotent upsert + content-hash classification | fn | spec | both | `src/dbs/core/engine.py`, `hashing.py` | rusty_db (unverified) | no | M | |
| Engine — revision history writing | fn | spec | both | `src/dbs/core/engine.py` | — | no | S | |
| Engine — soft-delete sweep + safety-fraction guard | fn | spec | both | `src/dbs/core/engine.py` | — | no | M | Data-safety critical — the 50%-mass-delete guard |
| Engine — crash-recovery reaper | fn | spec | both | `src/dbs/core/engine.py` | — | no | S | |
| Engine — least-privilege secrets scoping | fn | spec | both | `src/dbs/core/secrets.py` | — | no | S | |
| Managed HTTP client (backoff, `Retry-After`, rate limit) | fn | spec | both | `src/dbs/core/http.py` | rusty_http / rusty_request (unverified) | no | M | |
| Timeutil helpers | fn | spec | both | `src/dbs/core/timeutil.py` | — | no | S | |
| `CORE_API_VERSION` gating | fn | spec | both | `src/dbs/core/versioning.py` | — | no | S | |
| Cooperative cancellation (Ctrl+C → finish in-flight, no new starts) | fn | spec | both | `src/dbs/core/cancel.py` | — | no | S | |
| `netns` helper | fn | spec | linux | `src/dbs/core/netns.py` | — | no | S | Confirm Linux-only scope when picked up — name suggests network-namespace, may not need a Windows counterpart |
| `BackupService` (UI-agnostic façade: `backup_source`/`backup_all`, connector instantiation via the registry, VPN guard checks, status/history rendering, once-per-call crash-recovery reap threading) | type+fn | spec | both | `src/dbs/core/service.py` | — | no | L | Done (#21 reap-once slice, #46 the rest) |
| Engine — `run_source` orchestrator / connector run-stream bridge (drives one connector's actual fetch: writes a `RunContext`, reads the `FetchEvent` stream back over ADR-0001's subprocess protocol — steps 2-3, not yet implemented; #45 only did step 1/4 handshake+discovery) | fn | spec | both | `src/dbs/core/engine.py` (`Engine.run_source`) | — | no | M | **Discovered while implementing #46** — `BackupService.backup_source` calls `self.engine.run_source(rc, ctx, ...)` in the reference, and nothing in this port does that job yet. #46 introduced a `ConnectorRunner` trait as the seam (`UnimplementedRunner` is the production stand-in today) specifically so this row didn't have to block #46's own scope. Whoever picks this up implements a real `ConnectorRunner`: write the `RunContext` JSON line, read `FetchEvent` lines back, drive `engine::{prepare, commit_checkpoint, sweep_deletions}` per event, same as the reference's fetch loop. |

## Storage

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `Storage` trait (ABC) | type | spec | both | `src/dbs/storage/base.py` | rusty_db (unverified) | no | S | |
| SQLite storage — schema + migrations | fn | spec | both | `src/dbs/storage/{sqlite,migrations}.py` | rusty_db (unverified) | no | M | New dependency: a SQLite crate (`rusqlite` or similar) — flagged below |
| SQLite storage — upsert/classify/revisions | fn | spec | both | `src/dbs/storage/sqlite.py` | rusty_db (unverified) | no | M | Done (#36) |
| SQLite storage — browse/query + FTS5 search | fn | spec | both | `src/dbs/storage/sqlite.py` | rusty_db (unverified) | no | M | Done (#36, #47, #48) |
| SQLite storage — metrics aggregation | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | S | Done (#36) |
| SQLite storage — maintenance (VACUUM, WAL checkpoint, `PRAGMA optimize`, snapshot) | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | M | Done (#36) |
| SQLite storage — FTS5 full-text index (`_ensure_fts`: virtual table + triggers + backfill, `browse_items`' MATCH-then-LIKE fallback) | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | S | Done (#47) |
| SQLite storage — `browse_items` video-link thumbnail fallback (YouTube/Loom/Vimeo URL detection when an item has no image media) | fn | spec | both | `src/dbs/storage/sqlite.py` | — | no | S | Done (#48) |

## Config & secrets

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Config loading (`dbs.toml` + `.env`, per-source blocks, inline-secret rejection) | fn | spec | both | `src/dbs/config.py` | — | no | M | New dependency: TOML parser — check rusty_json's scope first (name suggests JSON only) |
| Export profile (`ExportProfile`/`ExportProfileOverride` — per-source export rules: which item kinds export, wiki grouping) | type | spec | both | `src/dbs/core/export_profile.py` | — | no | S | Done (#49) |

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
| Restore (`dbs restore`, dry-run, manifest/schema-version validation) | fn | spec | both | `src/dbs/restore.py` | — | no | M | Done (#59) — orchestrator landed as `BackupService::restore` (`src/dbs/core/service.py`'s split, not `restore.py`'s own scope) |
| Verify (DB integrity + archive checksum check) | fn | spec | both | `cli.py` (`dbs verify`) | — | no | S | Done (#60) — orchestrator landed as `BackupService::verify`; archive checksum half already done in #59's `restore::verify_archive` |
| `notes_export.py` (incremental per-item markdown, collision map, state file) | fn | spec | both | `src/dbs/notes_export.py` | — | no | M | Done (#61) — pulled forward `BackupService::export` (an `ExportQuery`-to-file orchestrator) from #70, since this issue cannot exist without it |
| `templates.py` (`dbs init` scaffolding writer) | fn | spec | both | `src/dbs/templates.py` | — | no | S | Done (#62) — writer landed as a library function (`write_scaffolding`), same "no CLI crate yet" pattern as `BackupService::export`/`verify`/`restore` |

## CLI (`dbs.cli`, ~22 subcommands)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| CLI skeleton + `dbs init` | fn | spec | both | `cli.py` | — | no | M | Done (#63) — new `dbs-cli` binary crate; added the `clap` dependency named in this row |
| `dbs backup` — single-source run | fn | spec | both | `cli.py` | — | no | M | |
| `dbs backup --all --only-due` scheduling gate | fn | spec | both | `cli.py` | — | no | S | |
| `dbs backup --parallel N` worker pool | fn | spec | both | `cli.py` | — | no | L | Needs an async-runtime/threading decision — split further at filing |
| `dbs backup` progress line + Ctrl+C handling | fn | spec | both | `cli.py` | — | no | S | |
| `dbs status` / `dbs history` | fn | spec | both | `cli.py` | — | no | S | |
| `dbs items` / `dbs stats` (browse + FTS5 CLI) | fn | spec | both | `cli.py` | — | no | M | |
| `dbs export*` / `dbs decrypt` CLI wiring | fn | spec | both | `cli.py` | — | no | M | |
| `dbs sources` / `dbs connectors` (list/add/check/describe) | fn | spec | both | `cli.py` | — | no | M | |
| `dbs doctor` | fn | spec | both | `cli.py` | — | no | M | |
| `dbs update-ytdlp` | fn | spec | both | `cli.py` | — | no | S | Only meaningful once/if a yt-dlp-equivalent connector strategy is decided (see Connectors section) |
| `dbs schedule` (cron/systemd snippets) | fn | spec | both | `cli.py` | — | no | S | Windows needs a Task Scheduler snippet instead of systemd — cross-platform floor makes this two branches, not one |
| `dbs serve` flag wiring | fn | spec | both | `cli.py` | — | no | S | Thin; real work is the Web tier row below |
| `dbs capture` (headless login capture) | fn | spec | both | `cli.py`, `web/setup.py` | — | no | L | Depends on the browser-automation strategy decision (see Connectors) |
| `dbs research` subcommands | fn | spec | both | `cli.py`, `research/*` | — | no | L | Depends on NotebookLM client existing in Rust — see Research row |
| `dbs version` | fn | spec | both | `cli.py` | — | no | S | |

## Web tier (`[web]` extra)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Web app skeleton + static SPA serving | fn | spec | both | `src/dbs/web/app.py` | rusty_http (unverified — likely client-only) | no | L | New dependency: async web framework (`axum` or similar) + async runtime — flagged below |
| Job manager (background jobs + SSE progress) | fn | spec | both | `src/dbs/web/jobs.py` | — | no | M | |
| Auth / CSRF / Origin / Host protection | fn | spec | both | `src/dbs/web/app.py` (security gate) | — | no | M | Security-sensitive — implement carefully, don't skip the DNS-rebinding defense |
| `envfile.py` (scoped secrets writer) | fn | spec | both | `src/dbs/web/envfile.py` | — | no | S | |
| In-UI setup (dependency install + browser-auth capture jobs) | fn | spec | both | `src/dbs/web/setup.py` | — | no | L | Depends on browser-automation strategy — see Connectors |

## Research subsystem (NotebookLM-dependent, sits outside the connector model)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Research pipeline (YouTube search → NotebookLM synthesis → report) | fn | spec | both | `src/dbs/research/*` | — | no | L | Needs an authenticated NotebookLM client in Rust, which doesn't exist anywhere in the platform directory. Highest-risk row in this table — likely needs its own scoping conversation before issues are filed for it. |

## Connectors (14 — each a natural one-issue unit; "template" per README's own A/B split)

| Symbol | Category | Source | Platforms | Reference | Existing RustyMill impl | Breaking? | Est. size | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `raindrop` (template A: REST+token, real delta endpoint) | fn | spec | both | `connectors/raindrop.py` | — | no | M | Simplest connector — good first real connector after core lands |
| `github` (template A: stars/gists) | fn | spec | both | `connectors/github.py` | — | no | M | |
| `pinboard` (template A: `posts/update` delta) | fn | spec | both | `connectors/pinboard.py` | — | no | M | |
| `readwise` (template A: `updatedAfter` cursor) | fn | spec | both | `connectors/readwise.py` | — | no | M | |
| `mastodon` (template A) | fn | spec | both | `connectors/mastodon.py` | — | no | M | |
| `bluesky` (template A) | fn | spec | both | `connectors/bluesky.py` | — | no | M | |
| `spotify` (template A, catalog-only) | fn | spec | both | `connectors/spotify.py` | — | no | M | |
| `pocketcasts` | fn | spec | both | `connectors/pocketcasts.py` | — | no | M | |
| `podcast` (generic RSS-shaped) | fn | spec | both | `connectors/podcast.py` | — | no | M | |
| `vimeo` | fn | spec | both | `connectors/vimeo.py` | — | no | M | |
| `udemy` (template B-ish) | fn | spec | both | `connectors/udemy.py` | — | no | M | |
| `reddit` (template B: browser-session, full enumeration) | fn | spec | both | `connectors/reddit.py` | — | no | L | Needs the browser-automation strategy decision below |
| `skool` (template B + native HLS video download via yt-dlp/ffmpeg) | fn | spec | both | `connectors/skool.py` | — | no | L | Heaviest connector in the reference. No clean Rust equivalent for yt-dlp/Playwright — needs its own scoping decision, likely the last connector implemented, not the first |
| `youtube` (yt-dlp-based) | fn | spec | both | `connectors/youtube.py` | — | no | L | Same yt-dlp dependency question as `skool` |
| Shared Playwright launch helper | fn | spec | both | `connectors/_playwright.py` | — | no | M | Only needed if browser-automation connectors are in scope |
| Tiptap rich-text→Markdown helper | fn | spec | both | `connectors/_tiptap.py` | — | no | S | |
| Shared watchdog/timeout helper | fn | spec | both | `connectors/_util.py` | — | no | S | Reusable — do early, several connectors depend on it |

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
