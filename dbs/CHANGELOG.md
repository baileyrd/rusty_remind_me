# Changelog

All notable changes to this repo are documented here.
Format: Added / Changed / Deprecated / Removed / Fixed / Security, newest first.

This is a high-level, human-facing summary. For the detailed narrative of
individual PRs (rationale, alternatives considered, exact diffs), see
[RELEASE_NOTES.md](./RELEASE_NOTES.md). No versioned release has been cut
yet, so everything so far is under Unreleased.

## [Unreleased]

### Added
- **Core engine** (`dbs-core`): data model, error hierarchy, `Connector`
  contract/capabilities, cursor/checkpoint-safe engine with idempotent
  upsert + content-hash change classification, revision history,
  soft-delete sweep with a configurable safety fraction, crash-recovery
  reaper, least-privilege secrets scoping, a managed HTTP client
  (backoff/`Retry-After`/rate limiting), cooperative cancellation, and a
  `BackupService` façade shared by the CLI and web UI.
- **Dynamic plugin registry**: connectors run as separate
  `dbs-connector-<type>` subprocess binaries speaking a line-delimited
  JSON-IPC protocol (handshake → run → stream), not compiled into the
  host — see [ADR-0001](./docs/adr/0001-dynamic-plugin-registry.md).
  Per-source `[sources.NAME]` config crosses the same wire via
  `Connector::configure` — see
  [ADR-0002](./docs/adr/0002-per-source-connector-config.md).
- **14 connectors**, each a real subprocess binary with its own
  `configure()`: Raindrop, GitHub, Pinboard, Readwise, Mastodon, Bluesky,
  Spotify, Pocket Casts, generic Podcast (RSS/Atom + OPML), Vimeo, Udemy,
  Reddit, Skool, and YouTube. Reddit/Skool drive a Playwright-based
  browser-session helper for login capture and page scraping; Vimeo/Udemy
  optionally shell out to `yt-dlp` for video downloads.
- **SQLite storage**: schema/migrations, upsert/classify/revisions,
  browse/query with FTS5 full-text search, metrics aggregation, and
  maintenance (`VACUUM`, WAL checkpoint, `PRAGMA optimize`, snapshot).
- **Export formats**: JSON, NDJSON, CSV, Markdown, a zip archive bundle,
  Obsidian vault, and a cross-linked wiki — plus incremental per-item
  Markdown notes, passphrase-encrypted export/decrypt (scrypt +
  AES-256-GCM), and export-profile overrides per source.
- **`dbs` CLI**: `init`, `backup` (single source or `--all`, with
  `--parallel`, `--only-due`, `--force-full`, `--reconcile`, `--dry-run`),
  `status`, `history`, `items`, `stats`, `export`/`export-notes`/
  `export-profiles`/`export-wiki`, `verify`, `restore`, `decrypt`,
  `doctor`, `update-ytdlp`, `maintain`, `schedule`, `serve`, `capture`,
  `sources` (`list`/`add`/`check`), `connectors` (`list`/`describe`), and
  `research` (`youtube`/`youtube-backup`).
- **`dbs serve` web UI** (`dbs-web`, axum): a vanilla-JS SPA dashboard,
  a full `/api` surface (status, items/media browsing, sources,
  connectors, secrets management, backup trigger + SSE progress stream,
  export, in-UI setup and browser-login capture, research), a background
  scheduler (`--schedule`), and bearer-token auth for non-localhost binds.
- **`dbs research`** (`dbs-research`): ad-hoc NotebookLM pipelines over
  fresh YouTube searches or already-backed-up videos, rendering a
  Markdown research report.
- **Webhook notifications**: `[dbs] notify_url`/`notify_on` POSTs a
  Slack/Discord-compatible summary after each backup batch.

### Changed
- `[dbs] http_timeout`/`http_rate_limit_per_min` now reach the connector
  subprocess's real HTTP client instead of being parsed and unused.
- `[dbs] batch_max` now drives the engine's actual commit-flush cadence,
  replacing a previously hardcoded constant.

### Fixed
- Several rounds of stale doc-comments describing already-landed wiring
  as still-open work, caught and corrected as later features shipped.
- Dead/orphaned helper code removed once its real caller landed instead
  of left unreachable.

### Security
