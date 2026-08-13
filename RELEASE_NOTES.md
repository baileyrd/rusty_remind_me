# Release Notes

No PR workflow yet on this repo's first commit — this pushes directly to the
`claude/repo-config-danror` branch to establish the default branch and initial
scaffold. Once there's a real default branch and a second change lands through a
PR, switch to one entry per merged PR (reverse chronological), same convention as
[AISF's RELEASE_NOTES.md](https://github.com/baileyrd/AISF/blob/main/RELEASE_NOTES.md).

---

## dbs items / dbs stats (closes #69)
**2026-08-13**

- **Implemented:** `dbs items` (browse/search/filter, or one item's
  full detail by id) and `dbs stats` (aggregate item/media/revision
  metrics), wired to three new thin `BackupService` delegations —
  `browse_items`/`get_item`/`metrics` — over the existing
  `Storage::browse_items`/`get_item`/`metrics`.
- **Not gated on FTS5**, unlike this issue's original framing: the
  filed gap assumed FTS5 search was still pending its own storage
  issue, but `Storage::browse_items` already tries an FTS5 MATCH query
  first (all-words, prefix match on the last token) and only falls
  back to a plain `LIKE` scan when the SQLite build lacks the FTS5
  module — so `dbs items --search` gets full search parity today, no
  follow-up needed.
- `dbs items [ID] [--source NAME]... [--type KIND]... [--search Q]
  [--since DATE] [--until DATE] [--include-deleted] [-n LIMIT]
  [--offset N] [--json]`: newest-first listing with a `1-50 of N (next
  page: --offset 50)` footer, or (with `ID`) the reference's full item
  detail view — source/kind/tags/timestamps, a truncated body preview,
  the media list, and the raw payload. `--since`/`--until` accept
  either a bare `YYYY-MM-DD` or full ISO-8601 (ported as
  `parse_date_arg`, a CLI-local complement to `dbs_core::parse_iso`
  for the date-only case the core parser doesn't handle).
- `dbs stats [--json]`: total live/deleted item counts, revision
  count, archived media count + size (`human_bytes`, e.g. `"3.4
  KiB"`), then a per-source/per-kind breakdown table.
- Updated `a_stub_subcommand_reports_not_yet_implemented` to use
  `export` instead of `items`, now that `items`/`stats` are no longer
  stubs.
- 12 new `dbs-cli` integration tests using a real `SqliteStorage` to
  seed genuine item rows via `upsert_items` (no connector-candidate
  discovery exists yet, #85-100, so a real `dbs backup` run can't
  produce this data itself): empty DB, populated DB with source/text
  filters, pagination, the JSON envelope, item detail by id (found and
  not-found), an invalid `--since` date, and `stats`'s empty vs.
  populated output.

## dbs status / dbs history (closes #68)
**2026-08-13**

- **Implemented:** `dbs status` (per-source item counts, last run,
  cursor watermark, schedule/due state) and `dbs history` (recent
  runs, newest first), wired to `BackupService::status`/`history` —
  both already existed in `dbs-core` from an earlier models-parity
  pass; this issue is the CLI rendering on top of them.
- `dbs status [SOURCE]`: one line per source (`name type on/off
  items=… (deleted …) runs=… last=…`), plus a `! has interrupted
  runs` line where applicable. An unknown `SOURCE` name is a
  placeholder row (`type=?`, `enabled=off`), not an error — matches
  the reference. No sources configured prints "No sources configured."
  instead of an empty table.
- `dbs history [SOURCE] [-n/--limit N]` (default 20): one line per run
  (`started_at  source  status [mode] +created ~updated xdeleted
  (fetched-failed)  duration`), plus an indented error/warning line
  underneath where present.
- Both support `--json` (the raw `SourceStatus`/run-row data via
  `serde_json`, no reshaping) instead of the rendered table.
- 10 new `dbs-cli` integration tests using a real `SqliteStorage` to
  seed run rows directly (no connector-candidate discovery exists yet,
  #85-100, so a real `dbs backup` run can't produce this history
  itself): empty state, multiple sources, a seeded run reflected in
  both commands, the unknown-source placeholder, `--json` shape,
  newest-first ordering, `--limit`, and source-name filtering.

## dbs backup progress line + Ctrl+C handling (closes #67)
**2026-08-13**

- **Implemented:** a live progress line during `dbs backup` runs, and
  a Ctrl+C handler that lets in-flight work finish rather than aborting
  mid-write. `crate::cancel::CancelToken` already existed (#10) — this
  issue wires it up: `BackupService::backup_source`/`backup_all` gained
  `on_progress`/`cancel` (the latter on `backup_all` only) options,
  `dbs-cli` gained a `ProgressRenderer` and a `ctrlc`-based SIGINT
  handler.
- **Scope note, not a bug:** `backup_source` currently hands off to
  `ConnectorRunner` as a single blocking call — there's no run/stream
  protocol yet (ADR-0001 steps 2-3) to report *per-item* progress
  from. Only `ProgressPhase::SourceStart`/`SourceDone` are emitted
  today; `Item`/`Checkpoint`/`Sweep` stay reserved on the enum (already
  present from an earlier models-parity pass) for that follow-up issue
  to start emitting through this same seam without an API break. The
  CLI's progress line reflects this honestly: a static `[i/N] source
  [mode] running…` rather than the reference's animated spinner + live
  item counter, since nothing changes between a source's start and its
  end without per-item events to redraw against.
- `backup_all`'s `cancel` token is checked between sources (sequential
  path) and before each dequeue (parallel path, `--parallel N`): a
  cancelled token stops new sources from starting while any already in
  flight finish and commit normally — every started run still reaches
  `finish_run`, so storage is never left half-committed.
- The CLI installs a Ctrl+C handler once per `dbs backup` invocation:
  the first press cancels the token and prints a "Stopping…" notice;
  a second press aborts the whole process immediately via `exit(130)`
  — a single in-flight connector call can't be interrupted mid-fetch
  without the run/stream protocol either, so this matches the
  reference's own documented behavior rather than pretending to do
  more.
- New CLI flags: `--progress`/`--no-progress` (default: auto, on for a
  TTY).
- `Storage` and `ConnectorRunner`'s trait bounds already required
  `Send`/`Send + Sync` (from #66); this issue adds a `ProgressSink`
  trait (`Sync`, implemented for any `Fn(&ProgressEvent) + Sync`) as
  the callback seam, plus private `FramedProgress` (source_index/total)
  and `LockedProgress` (serializes delivery across `--parallel`
  workers) wrapper sinks.
- 8 new `dbs-core` unit tests (`SourceStart`→`SourceDone` emission
  shape, silence for a disabled source, cross-source framing,
  sequential and parallel cancellation — the parallel one against a
  real `SqliteStorage`, confirming the stopped-early run still reached
  a terminal status) plus 4 `dbs-cli` unit tests for the renderer's
  dirty-line state machine and 3 CLI integration tests for the new
  flags.

## dbs backup --all --parallel N: worker pool (closes #66)
**2026-08-13**

- **Implemented:** `dbs backup --all --parallel N`, running the
  work-list on a bounded thread pool instead of one source at a time.
  Added `BackupAllOptions.parallel: Option<u32>` (`None` falls back to
  the config's `parallel` key, itself defaulting to `1`) and a private
  `BackupService::backup_all_parallel`.
- **Sync-threadpool-vs-tokio decision (required by this issue):** this
  crate already chose `reqwest::blocking` over `tokio` for the HTTP
  client (#22) — the worker pool stays consistent with that and uses
  plain `std::thread::scope`, no new dependency. Each worker gets its
  own `Storage::spawn()` connection (SQLite's WAL mode + `busy_timeout`
  arbitrate the single writer slot; the existing per-source lock table
  still prevents double-running a source), pulling work off a shared
  queue so a fast source's worker doesn't sit idle behind a slow one.
  `Storage` gained a `Send` supertrait (a spawned connection is owned
  by exactly one thread) and `ConnectorRunner` gained `Send + Sync`
  (one runner is shared read-only across workers) to make this
  possible; the `ScriptedRunner` test double moved from `RefCell` to
  `Mutex` to satisfy the new bound.
- When the storage backend can't provide `N` independent connections
  (an in-memory database), `backup_all` falls back to the existing
  sequential path rather than failing — same fallback contract as the
  reference's `_backup_all_parallel` returning `None`.
- A dry run, a single-source work-list, or `--parallel 1` all skip the
  thread pool entirely and take the plain sequential path — nothing to
  parallelize in the first two cases, and no behavioral difference in
  the third.
- 4 new `dbs-core` unit tests (2 with `FakeStorage` covering the
  fallback and `N=1` cases, 2 with a real file-backed `SqliteStorage`
  actually exercising concurrent workers — one all-succeed, one
  isolating a failing source among successful ones under
  `continue_on_error`) plus 3 new `dbs-cli` integration tests
  confirming `--parallel` is wired through the CLI end to end.
- The progress line + Ctrl+C handling this issue's acceptance
  checklist references (`on_progress`/`CancelToken` in the reference)
  remain out of scope — filed separately as #67.

## dbs backup --all --only-due: scheduling gate (closes #65)
**2026-08-13**

- **Implemented:** `dbs backup --all --only-due`, filtering the
  `--all` source list down to sources that are actually due, mirroring
  `cli.py`'s `_is_due` check. Added `BackupAllOptions.only_due: bool`
  (default `false`, i.e. `--all` alone still runs every enabled
  source) and a new `BackupService::source_is_due` method: looks up
  the source's most recent run via `Storage::recent_runs`, falls back
  to "never run" (always due) when there is none, and compares against
  the source's configured `schedule` (default `"daily"`) via the
  existing `is_due` free function.
- `--only-due` is wired as a new CLI flag on `dbs backup`; `cmd_backup`
  now shares its config/storage/service setup between the
  single-source and `--all` paths.
- 2 new service-level unit tests (never-run sources are always due;
  an empty/all-disabled source list returns no results) plus 5 new
  CLI integration tests in `crates/dbs-cli/tests/backup_all.rs`, using
  a real `SqliteStorage` seeded with genuine `Utc::now()`-based run
  timestamps to exercise the "recently run source is skipped" case —
  `is_due` compares against the real wall clock, so a fake/fixed
  timestamp can't cover that scenario.
- `--parallel N` worker-pool batching (#66) and the progress line/
  Ctrl+C handling (#67) remain out of scope for this issue.

## dbs backup: single-source run (closes #64)
**2026-08-13**

- **Implemented:** `dbs backup <source>`, wired to
  `BackupService::backup_source`. Prints "Backup results:" followed by
  one line per run (`_print_run`'s format, colors dropped — no color
  dependency added for this issue): status/mode/created/updated/
  unchanged/deleted/undeleted/failed counts, fetched total, and a
  compact duration (`0.8s`/`2m45s`), plus any error/warning lines
  underneath. Exit codes match the reference: `0` success/partial-free,
  `2` source locked, `3` (via the shared `_exit_code` port) any
  `failed` result, `5` unknown source, `4` everything else
  (config/database/connector errors, or no `SOURCE`/`--all` given).
- `--all` is explicitly **out of scope** here (gap-analysis.md splits
  it into #65 `--only-due`, #66 `--parallel`, #67 the progress line/
  Ctrl+C handling) — it still stubs out ("not yet implemented") rather
  than half-working.
- **Known limitation, not a bug:** no connector-candidate discovery
  mechanism exists yet (scanning for installed connector subprocesses
  on disk — an implicit prerequisite of the connectors cluster,
  #85-100), so the registry `dbs backup` constructs is always empty
  today. Every configured source's connector type is therefore
  reported "not found" until that lands — this is what the acceptance
  checklist's "connector error surfaced to CLI output" scenario tests.
- 5 new integration tests: an unknown source name (exit 5), an
  unregistered connector type surfacing as a config error (exit 4), a
  disabled source completing successfully end-to-end (exit 0, printed
  result) — the honest stand-in for "a successful run" given no real
  connector exists to succeed against yet, since `backup_source`
  returns `Ok(RunResult)` for a disabled source before any registry
  lookup happens — no `SOURCE`/`--all` (exit 4, usage message), and
  `--all` still reporting as a stub.

## CLI skeleton + dbs init (closes #63)
**2026-08-12**

- **Added:** a new `dbs-cli` binary crate producing the `dbs` binary —
  the first CLI issue in the batch, porting the argument-parsing
  skeleton of `src/dbs/cli.py`'s entry point in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`). `--config`/`-c`
  (env `DBS_CONFIG`, default `dbs.toml`, global), `--help`, and
  `--version` are wired for real; every subcommand name and the
  `sources`/`connectors`/`research` sub-app nesting match the
  reference's surface exactly, so every follow-up CLI issue only adds
  flags and behavior to an existing stub, never new dispatch.
- **`dbs init` fully implemented**, wired to #62's
  `templates::write_scaffolding` plus a real database initialization
  (opens the configured SQLite path and runs migrations) — matches
  the reference's `init` command output and idempotency (an existing
  config isn't clobbered without `--force`; `.env.example` never is).
- **Added dependency:** `clap = "4"` (`dbs-cli`, `derive`+`env`
  features) — the CLI-parsing crate named in this issue's own
  acceptance criterion.
- Every other subcommand is a stub: prints "not yet implemented" to
  stderr and exits `1` (not one of the reference's real exit codes,
  since a stub represents no real outcome). Exit codes otherwise
  match the reference's documented cron-friendly convention (`0`
  success, `2` partial, `3` failed, `4` config error, `5` no such
  source) — `dbs init` already uses `4` for a config/database error.
- 8 new integration tests (spawning the real compiled binary, same
  pattern as `dbs-core`'s connector-fixture tests): `--help` lists
  every subcommand, an unknown subcommand errors non-zero, `--version`
  prints a version, `dbs init` on a fresh directory writes the
  config/`.env.example`/database, a re-run doesn't clobber without
  `--force`, `--force` does overwrite, and both a top-level and a
  nested stub subcommand report "not yet implemented".

## templates: dbs init scaffolding writer (closes #62)
**2026-08-12**

- **Added:** `dbs-core::templates`, porting `src/dbs/templates.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — `CONFIG_TEMPLATE`/
  `ENV_TEMPLATE` (embedded verbatim via `include_str!` from sibling
  `.template` files, byte-for-byte the reference's two constants), and
  `write_scaffolding` (the writer half, which the reference itself
  keeps in `cli.py`'s `init` command rather than `templates.py` — no
  CLI crate exists yet to host it, so it lands here, same rationale as
  `BackupService::export`/`verify`/`restore` landing as library methods
  ahead of the CLI cluster).
- Idempotent by design, matching the reference: an existing config is
  left alone unless `force` is set (never silently clobbered), and
  `.env.example` is never overwritten at all, `force` or not — only
  the config template gets a force override.
- Deliberately doesn't create `export_dir`/`download_root` directories
  at init time, matching the reference: neither directory is `mkdir`'d
  by `dbs init` in `cli.py` either, only referenced in the written
  config for later, lazy creation when actually used.
- 6 new tests: a fresh directory writes both files, an
  already-initialized directory clobbers neither without `--force`,
  `--force` overwrites the config but still never the `.env.example`,
  both templates contain their expected content, and the written
  config round-trips through `toml::from_str` (byte-for-byte
  TOML-syntax fidelity, not just "some text got written").

## notes_export: incremental per-item Markdown export (closes #61)
**2026-08-12**

- **Added:** `BackupService::export`, porting the reference's
  `BackupService.export` — runs an `ExportQuery` through any landed
  `Exporter` and atomically writes the result to a path (write to a
  sibling `.tmp` file, then rename, so a crash mid-export never leaves
  a half-written file). **Pulled forward from #70** (the CLI-facing
  `dbs export*` wiring): `export_notes`/`export_wiki_dir` cannot exist
  without *some* way to turn a query into a written file, and every
  exporter issue already lands `Exporter`/`ExportQuery` — this is the
  missing link between them and `Storage`, not CLI argument parsing
  (still #70's own scope). Backed by a new in-memory `ExportSource`
  adapter that eagerly collects `Storage::iter_items`/`iter_revisions`/
  `iter_media_blobs` (so a storage error surfaces before a single byte
  is written, since the `Exporter` trait's `items()` etc. are
  infallible) and a manifest/per-source-`ExportProfile` builder
  mirroring the reference's `_manifest`/`_export_profiles` (minus
  `tool_version`/`git_sha` — no build-metadata/VCS-introspection
  equivalent wired up yet).
- **Added:** `dbs-core::notes_export`, porting `src/dbs/notes_export.py`
  — `export_notes` (one Markdown file per live item, unzipped, into a
  directory; incremental via a `.dbs_export_state.json` state file
  recording the previous run's *start* time as the next cutoff, applied
  as created-*or*-updated since `ExportQuery` only ANDs filters
  together; a persistent `(source, external_id) -> filename` map so
  the same item always lands in the same file across runs, surviving
  title edits and disambiguating a genuine new collision the same way
  the obsidian exporter itself would) and `export_wiki_dir` (the
  `wiki` export's pages loose into a directory, deliberately **not**
  incremental — a hub page is an aggregate of everything a source has,
  not "what's new since the cutoff").
- 8 new tests: identity parsing (including backslash/quote
  unescaping) and filename resolution as isolated unit tests, plus
  `SqliteStorage`-backed integration tests (a real in-memory database,
  not a hand-rolled fake) for a first full-write run, an incremental
  re-run that only writes items created after the prior cutoff, a
  same-titled item colliding across two separate runs and getting
  disambiguated by `external_id`, and `export_wiki_dir` extracting
  pages and the index but leaving `manifest.json` behind.

## Verify (closes #60)
**2026-08-12**

- **Added:** `BackupService::verify`, porting the reference's
  `BackupService.verify` (`src/dbs/core/service.py`) — database
  integrity via the already-existing `Storage::integrity_check`
  (#36), plus per-source checks (defaulting to every configured
  source when no name is given): an unparseable cursor, and any run
  still `"running"` in the last 50 (an orphan left behind by a crash
  the reaper hasn't caught yet). Returns a `VerifyReport` (`ok` plus a
  `VerifyIssue` per finding — `source`/`kind`/`detail`), types that
  already existed in `models.rs` from an earlier pass.
- Archive-bundle checksum verification (this issue's other acceptance
  criterion) was already landed in #59 as `restore::verify_archive` —
  the reference's own CLI calls it directly rather than through
  `BackupService`, so this port follows the same split rather than
  adding a redundant wrapper.
- `FakeStorage` (test-only, in `service.rs`) gained `integrity`/
  `unparseable_cursor` fields so verify's failure paths are actually
  exercisable, and `recent_runs` now includes each run's `id` (needed
  for the orphan-run detail message).
- 5 new tests: a clean database with no sources reports `ok`, a
  corrupted database surfaces one `integrity` issue, an unparseable
  cursor surfaces one `cursor` issue, a run stuck `"running"` surfaces
  one `orphan_run` issue, and a name that matches no configured source
  is silently skipped rather than erroring (matching the reference).

## Restore (closes #59)
**2026-08-12**

- **Added:** `dbs-core::restore`, porting `src/dbs/restore.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — `read_manifest`
  (a zip without `manifest.json` is refused as "not a dbs archive"),
  `iter_export_rows` (items from an archive's `items/*.ndjson` files
  or a bare ndjson export), `prepared_item_from_row` (maps one export
  row back to a `PreparedItem`, requiring `external_id`/`content_hash`/
  a `raw` payload — a `--no-raw` export is refused as "not
  restore-grade"), `verify_archive` (per-entry sha256 checksum
  verification, flagging missing/mismatched/unlisted-extra entries),
  and `skipped_extras` (revision/media counts present but not
  restored).
- **Added:** `BackupService::restore`, porting the reference's
  `BackupService.restore` (`src/dbs/core/service.py`) — replays an
  export into storage through the same classified `upsert_items` path
  a live backup uses (so the stored `content_hash` carries over
  verbatim and a re-restore of the same bundle is a no-op), with
  encrypted-bundle auto-decryption (via #52's `crypto` module, to a
  private temp file, passphrase from the environment/secret store,
  never argv), pre-write integrity verification and schema-version
  rejection (`db_schema_version` newer than this build's own), and
  dry-run support (validates and reports without touching storage).
  Latest item state only (v1): revision history and media blobs in a
  bundle are counted and reported as *skipped*, matching the
  reference's own documented scope — replaying revisions verbatim
  would bypass the engine's one-revision-per-change invariant, and
  media rows need their items' DB ids.
- **Documented divergence:** the reference's `iter_export_rows` is a
  generator streaming one line at a time; this port collects into a
  `Vec` instead, since a lazy Rust iterator borrowing a `zip::ZipFile`
  across yields adds real complexity this issue's own acceptance
  criteria don't exercise (no test needs memory-boundedness) — flagged
  in the module doc-comment rather than silently narrowed.
- `FakeStorage`'s `upsert_items` (test-only, in `service.rs`) is now a
  real (not `unimplemented!()`) classifier — created/updated/unchanged
  by `(source_id, external_id) -> content_hash` — since the restore
  tests are the first in this crate to actually exercise batch upsert
  classification through `BackupService`.
- 18 new tests: 12 on the pure `restore` functions (manifest
  present/absent/malformed, ndjson/archive row iteration, every
  `prepared_item_from_row` validation error, a clean checksum
  verification, a tampered-entry checksum failure, skipped-extras
  counting) and 6 on `BackupService::restore` (happy path, dry-run,
  a newer-schema-version rejection, a corrupt-checksum-archive
  rejection, an encrypted-bundle round trip, and a missing-file error).

## Encryption at rest / encrypted exports (closes #52)
**2026-08-12**

- **Added:** `dbs-core::crypto`, porting `src/dbs/crypto.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — passphrase
  encryption for export bundles: `EncryptingWriter<W>` (a
  `std::io::Write` adapter that chunks plaintext into 1 MiB
  AES-256-GCM frames as it streams, so multi-GB archives never buffer
  in memory), `decrypt_stream`/`decrypt_file` (frame-by-frame decrypt
  with wrong-passphrase/tamper/reorder/truncation all surfacing as
  `DbsError::Config`, never partial silence), `is_encrypted` (magic-header
  sniff), and `resolve_passphrase` (secret store, then
  `DBS_EXPORT_PASSPHRASE`, then error — never silently unencrypted).
  Format is byte-for-byte the reference's: `DBSENC01` magic, 16-byte
  salt, 8-byte nonce prefix, then `len(u32 BE) || ciphertext` frames
  with a counter nonce and `b"dbs"`/`b"dbs-final"` AAD distinguishing
  the terminator frame (what makes truncation detectable).
- **Added dependencies:** `aes-gcm = "0.10"`, `scrypt = "0.11"`,
  `rand = "0.8"` (`dbs-core`) — `rusty_tls` was checked first per this
  issue's own acceptance criterion (`add_repo` on `Rusty-Mill/rusty_tls`)
  and is unreachable from this session (cross-tier repo access
  refused: "session already has repos from owner(s) [baileyrd]"), so
  there was nothing there to verify against; fell back to the
  pre-approved standard RustCrypto crates named in the issue itself.
  Key derivation matches the reference's scrypt parameters exactly
  (`n=2^14, r=8, p=1`, 32-byte key).
- **Deliberate divergence:** the reference's `EncryptingWriter.close()`
  (called implicitly via Python's context-manager protocol) becomes
  `EncryptingWriter::finish(self) -> Result<W, DbsError>`, an explicit
  consuming method — Rust's `Drop` can't propagate the I/O error the
  final frame's write can raise, so there's no implicit-close
  equivalent; a writer that's never `finish()`-ed simply never
  produces a file at all (no half-written output), the same safety
  property the reference gets from "never implicitly close."
- 12 new tests: small-plaintext round trip, a round trip crossing the
  1 MiB chunk boundary, empty-plaintext round trip, wrong passphrase,
  tampered ciphertext (AEAD auth failure), a truncated file missing
  its final frame, a bad magic header, `is_encrypted` on encrypted/
  plaintext/missing files, a real-file `decrypt_file` round trip, and
  `resolve_passphrase`'s store-then-env-then-error precedence.

## Archive exporter (closes #58)
**2026-08-12**

- **Added:** `ArchiveExporter`, porting `src/dbs/export/archive.py` in
  baileyrd/Daily-Backup-System (pinned `@6cc6491`) — a self-describing,
  self-*verifying* zip bundle: `items/<source>.ndjson` and (with
  `include_revisions`) `revisions/<source>.ndjson`, one file per
  source, streamed straight to the open zip entry a line at a time;
  archived media blobs under `media/<source>/<external_id>/`; and a
  `manifest.json` (schema versions from `ExportSource::manifest()`,
  the query, counts, `checksum_algorithm: "sha256"`, and a `checksums`
  map — a running sha256 computed per entry while streaming, not
  after the fact). This is the format `dbs restore`/`dbs verify`
  (filed separately) will validate a bundle's bytes against before
  ingesting anything. Registered as `"archive"` in `get_exporter`/
  `available_formats` — the last of the seven export formats.
- Reuses the `zip = "2"` dependency added in #56 (Obsidian exporter);
  no new dependency needed, as anticipated in that PR's gap-analysis
  note.
- 6 new tests: empty result set (manifest only, no per-source entries),
  a single item with its NDJSON entry's checksum independently
  recomputed and compared against the manifest, revisions gated by
  `include_revisions`, multiple sources producing separate entries and
  `by_source` counts, a media blob's checksum round-tripped the same
  way, and a metadata sanity check.

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
