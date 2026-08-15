# Release Notes

No PR workflow yet on this repo's first commit — this pushes directly to the
`claude/repo-config-danror` branch to establish the default branch and initial
scaffold. Once there's a real default branch and a second change lands through a
PR, switch to one entry per merged PR (reverse chronological), same convention as
[AISF's RELEASE_NOTES.md](https://github.com/baileyrd/AISF/blob/main/RELEASE_NOTES.md).

---

## `dbs-connector-pocketcasts`: wire `configure()` for real per-source scoping (closes #215)
**2026-08-15**

`PocketCastsConnector` had no `Connector::configure()` override —
`PocketCastsConfig`'s `include_subscriptions`/`include_starred`/`include_history`
(all consumed in `fetch()`) could never be set from a real `[sources.NAME]`
config block, same bug shape as the already-fixed #200 (skool). Added
`configure()`. 3 new tests: happy path, no-matching-keys no-op, and one
rejection case.

## `dbs-connector-spotify`: wire `configure()` for real per-source scoping (closes #214)
**2026-08-15**

`SpotifyConnector` had no `Connector::configure()` override — `SpotifyConfig`'s
`include_liked_tracks`/`include_playlists`/`page_size` (all consumed in
`fetch()`) could never be set from a real `[sources.NAME]` config block,
same bug shape as the already-fixed #200 (skool). Added `configure()`,
validating `page_size` as a 1-50 integer (mirrors the reference's
`Field(50, ge=1, le=50)`). 4 new tests: happy path, no-matching-keys
no-op, and two rejection cases.

## `dbs-connector-readwise`: wire `configure()` for real per-source scoping (closes #213)
**2026-08-15**

`ReadwiseConnector` had no `Connector::configure()` override — `ReadwiseConfig`'s
`include_books`/`include_highlights`/`page_size` (all consumed in `fetch()`)
could never be set from a real `[sources.NAME]` config block, same bug
shape as the already-fixed #200 (skool). Added `configure()`, validating
`page_size` as a 1-1000 integer (mirrors the reference's
`Field(1000, ge=1, le=1000)`). 4 new tests: happy path, no-matching-keys
no-op, and two rejection cases.

## `dbs-connector-raindrop`: wire `configure()` for real per-source scoping (closes #212)
**2026-08-15**

`RaindropConnector` had no `Connector::configure()` override — `RaindropConfig`'s
`collection_id`/`nested`/`include_types`/`page_size`/`overlap_seconds`/`poll_trash`
(all consumed in `fetch()`) could never be set from a real `[sources.NAME]`
config block, same bug shape as the already-fixed #200 (skool). Added
`configure()`, validating `page_size` (1-50, mirrors the reference's
`Field(ge=1, le=50)`) and `overlap_seconds` (>= 0). 6 new tests: happy
path (all six fields), no-matching-keys no-op, and four rejection cases.

## `dbs-connector-vimeo`: wire `configure()` for real per-source scoping (closes #217)
**2026-08-15**

`VimeoConnector` had no `Connector::configure()` override — `VimeoConfig`'s
`page_size`/`download_videos`/`downloads_dir`/`video_quality`/
`video_stall_timeout` (all consumed in `fetch()`) could never be set from
a real `[sources.NAME]` config block, same bug shape as the already-fixed
#200 (skool). Notably, `download_videos` could previously never actually
be turned on from a real config — only the connector's own hardcoded
default (`false`) ever applied. Added `configure()`, validating
`page_size` (1-100). 5 new tests: happy path (all five fields),
no-matching-keys no-op, and three rejection cases.

## `dbs-connector-github`: wire `configure()` for real per-source scoping (closes #211)
**2026-08-15**

`GitHubConnector` had no `Connector::configure()` override — `GitHubConfig`'s
`include_stars`/`include_gists`/`page_size` (all consumed in `fetch()`)
could never be set from a real `[sources.NAME]` config block, same bug
shape as the already-fixed #200 (skool). Added `configure()`, reading
`include_stars`/`include_gists` as bools and `page_size` as a 1-100
integer (mirrors the reference's `Field(100, ge=1, le=100)`). 5 new tests:
happy path, no-matching-keys no-op, and three rejection cases (non-bool,
out-of-range, non-integer).

## Wire `batch_max` from config instead of the hardcoded `BATCH_MAX` constant (closes #210)
**2026-08-15**

`[dbs] batch_max` was parsed and documented as the flush-batch size, but
`dbs-core::run_stream` defined its own `const BATCH_MAX: usize = 500;` and
that hardcoded constant — not `Config.batch_max` — was what actually bounded
host-side memory during a run. Setting `batch_max` in `dbs.toml` previously
had zero effect.

Added a `batch_max: usize` parameter to `run_connector_subprocess`
(mirroring the existing `sweep_safety_fraction` pattern), passed from
`SubprocessRunner::run_connector` as `self.config.batch_max`; removed the
constant. Test: a `dbs-core` end-to-end test with `batch_max = 1` (forcing a
flush after every single item) confirming the final committed state is
still exactly correct.

## Wire `http_timeout`/`http_rate_limit_per_min` into the connector's real HTTP client (closes #209)
**2026-08-15**

`[dbs] http_timeout`/`http_rate_limit_per_min` were parsed but never reached
the HTTP client every connector subprocess actually uses — the wire protocol
between host and connector (`WireRunContext`) had no fields to carry them,
and `dbs-connector-support::subprocess_main` built a bare
`reqwest::blocking::Client::new()` with no timeout and no
`ManagedHttpClient::rate_limit_per_min`. Setting either in `dbs.toml`
previously had zero effect.

Added `http_timeout`/`http_rate_limit_per_min` to `WireRunContext`
(`#[serde(default)]`, so an older host/wire line still deserializes),
populated from `Config` in `SubprocessRunner::run_connector`, and applied in
`subprocess_main::build_run_context` when building the connector's
`ManagedHttpClient`. `0.0`/`0` (the `#[serde(default)]` fallback) leaves
`reqwest`'s own untimed, unthrottled defaults in place, matching pre-#209
behavior. Tests: a `dbs-core` end-to-end passthrough test (real `dbs.toml` →
spawned fixture connector subprocess → echoed wire context), plus two
`dbs-connector-support` unit tests covering the configured and unset cases.

## Wire `notify_url`/`notify_on` webhook notification after backup runs (closes #208)
**2026-08-15**

`Config.notify_url`/`notify_on` were parsed from `[dbs]` TOML and even
documented in the `dbs init` scaffold template, but nothing ever read a
configured value and acted on it — a genuinely unwired capability, not
previously logged in `gap-analysis.md`. Added `BackupService::notify_results`
(mirrors the reference's method of the same name): POSTs a Slack/Discord-
compatible JSON summary (`text`/`content` plus the full per-run `results`)
to `notify_url` when `notify_on` (`failure`/`warning`/`always`) matches the
batch outcome. Best-effort by contract — a webhook failure is swallowed,
never propagated, so alerting can't break a backup.

Wired into every real call site that produces a `Vec<RunResult>`/`RunResult`:
`dbs-cli`'s `backup`/`--all`, `dbs-web`'s `POST /api/backup` (both `all` and
single-source), and `dbs serve --schedule`'s scheduler tick. Tests: 6 new
`dbs-core` tests using `mockito` to assert the webhook fires/doesn't fire per
`notify_on` gating, and that a non-2xx response is treated as a no-op rather
than an error.

## Fix more stale doc-comments claiming already-landed wiring is still open, round 3 (closes #207)
**2026-08-15**

`dbs-cli/src/main.rs`'s crate-level doc-comment still described every
subcommand but `dbs init` as an unimplemented stub, listing #64/#68/#69/
#70/#71/#72 as pending follow-ups — all of those are merged, and every
`cmd_*` handler is fully implemented. `cmd_backup`'s doc-comment likewise
still claimed connector discovery didn't exist and that none of the 14
`dbs-connector-*` crates were real subprocess binaries — both landed
(#160/#170, #164). Rewrote both to describe current behavior, following
the same pattern as #196/#202.

## Add `/api/export/profiles` and wire `wiki_grouping` into `/api/export` (closes #199)
**2026-08-14**

- **`GET /api/export/profiles`** — a route the reference has but this port's
  `/api` layer never got — now bridges `BackupService::export_profiles()`
  (each source's resolved export rules, and which fields its config
  overrode). Was already wired into `dbs-cli`'s `dbs export-profiles`;
  only the web route was missing.
- **`GET /api/export`** now reads `wiki_grouping` from the query string
  instead of always defaulting to `"topic"` — a web export to `wiki`
  format could never use a non-default grouping the way `dbs export
  --wiki-grouping` already could from the CLI. No new validation needed:
  `WikiExporter` already rejects an unknown grouping with a
  `DbsError::Config`, which `ApiError::from` already maps to 400.
- Tests: 2 new `/api/export/profiles` tests (empty config, a source with
  an override), 2 new `/api/export` tests (both valid `wiki_grouping`
  values succeed, an invalid one 400s).

## `dbs-connector-skool`: wire `configure()` for real per-source scoping (closes #200)
**2026-08-14**

`SkoolConnector` had no `Connector::configure()` override — every other
connector with per-source targeting config (bluesky's `identifier`,
mastodon's `instance`, podcast's `feeds`) implements it per ADR-0002, but
skool silently fell through to the trait's no-op default. Concretely,
`SkoolConfig`'s `communities`/`courses`/`no_download_communities` fields
could never be set from a real `[sources.NAME]` config block — the
already-tested `course_selected()` selector-matching logic had no way to
ever receive a real, non-empty selector list in production, and
auto-discover-everything always won.

Added `configure()`, reading all three fields as string arrays (mirrors
`dbs-connector-podcast`'s `feeds` parsing — the closest existing precedent
for an array-valued option, including its exact validation-error shape). 6
new tests: one per field's happy path, a no-matching-keys no-op case, and
two rejection cases (non-array value, non-string array entry).

## Remove the dead `reap_once` helper in dbs-core (closes #201)
**2026-08-14**

`reap_once` (a `storage.reap_interrupted_runs()` wrapper guarded by a shared
`already_reaped` flag) was fully implemented and unit-tested but had zero
real callers — `backup_source`/`backup_all` hand-roll the same "reap exactly
once per top-level call" guarantee inline instead, and neither one's control
flow actually needs a shared mutable guard flag (`backup_all` reaps once
unconditionally up front, then calls `backup_source` per source with
`reap: false`). Removed the function, its dedicated `CountingStorage` test
double, and its three tests; replaced with a single integration-level test
(`backup_all_reaps_exactly_once_across_multiple_sources`) asserting the same
guarantee against the real call graph via the existing `FakeStorage`. The
module doc-comment's description of the guarantee now points at the code
that actually provides it.

## Fix more stale doc-comments claiming already-landed wiring is still open (closes #202)
**2026-08-14**

Documentation-only, no code changes. A follow-up audit past #196 found more of
the same pattern:

- 11 connector `lib.rs` module doc-comments (raindrop, github, pinboard,
  readwise, mastodon, bluesky, spotify, pocketcasts, podcast, vimeo, udemy)
  claimed "this struct isn't reachable from a real `dbs backup` run yet; the
  plugin registry's run/stream bridge doesn't exist." False since #157/#164 —
  every one has had a real subprocess binary reachable from `dbs backup` for
  a while.
- `dbs-research/src/lib.rs` claimed `dbs research` subcommands "still report
  their own 'not yet implemented' stub." False since #189.
- `dbs-core/src/service.rs`'s `ConnectorRunner` scope note described
  `UnimplementedRunner` as "the production stand-in" and the run/stream wire
  protocol as unbuilt follow-up work. Both landed as `run_stream.rs`'s real
  `SubprocessRunner`, in production use everywhere.
- `dbs-web/src/setup.rs`'s module doc-comment and both capture-job error
  strings blamed the still-genuinely-blocked capture jobs on "issue #99
  hasn't built yet." #99 (the generic Playwright-subprocess launcher) is
  done and has real callers (reddit/skool's own acquisition scripts); the
  capture jobs are blocked on a dedicated login-capture script that simply
  hasn't been written, a different and more specific gap. Updated the
  doc-comment, both error strings, and the tests asserting on their exact
  wording.

## Wire `dbs verify`/`dbs restore`/`dbs maintain` CLI subcommands (closes #195)
**2026-08-14**

- **`dbs verify [source] [--archive PATH]`** now really calls `BackupService::verify` (DB integrity + per-source cursor/orphan-run checks) or, with `--archive`, `dbs_core::verify_archive` (an exported bundle's per-entry checksums) — both orchestrators have existed since #59/#60 but had no CLI caller. Exits 3 when either check finds real issues.
- **`dbs restore <path> [--dry-run] [--json]`** now really calls `BackupService::restore`, replaying an archive/ndjson export back into the database.
- **`dbs maintain [--vacuum] [--snapshot PATH] [--json]`** now really runs database housekeeping, via a new `BackupService::maintain` — `Storage::maintain`/`prune_revisions`/`vacuum_into` (#36) existed but had no orchestrator above the storage layer. Mirrors the reference: prunes each source's revisions past its configured `keep_revisions` first (so a `--vacuum` in the same pass reclaims the freed pages), then checkpoints/optimizes/vacuums, then optionally snapshots via `VACUUM INTO`. Populates the `MaintenanceReport` struct, which existed but was never constructed anywhere in the codebase until now.
- **`/api/verify`** now bridges `BackupService::verify` for real too, instead of an honest 501 — same `tokio::task::spawn_blocking` bridging pattern every other `/api` route uses.
- All three CLI subcommands were previously zero-field clap variants that fell through to a generic "not yet implemented" stub — that stub (`cmd_stub`/`command_name`) is now fully unused and removed, since every subcommand does real work.
- Tests: 5 new `BackupService::maintain` unit tests (revision-pruning selectivity, vacuum threading, snapshot success/failure), 11 new CLI integration tests covering all three subcommands' success/error/`--json` paths, an `/api/verify` route test replacing the old 501 test.

## Fix stale doc-comments claiming already-landed wiring is still an open gap (closes #196)
**2026-08-14**

Documentation-only, no code changes. Four doc-comments described real gaps
that later issues already closed, and had become actively misleading:

- `dbs-connector-bluesky`/`-mastodon`/`-podcast`'s `main.rs` files each
  claimed per-source config passthrough (`identifier`/`instance`/`feeds`)
  was still an open gap. It isn't — `configure()` in all three has read
  that value from `sources.<name>.*` since #166/ADR-0002, called by
  `dbs_connector_support::run_connector_main` on every real run.
- `dbs-core/src/run_stream.rs`'s module doc-comment claimed none of the
  14 connector crates had a real subprocess `main.rs` yet, and that
  every `dbs backup` call site still passed an always-empty
  `ConnectorRegistry`. Both closed since #164 and #160 respectively.

## `dbs-connector-skool`: wire fetch() to real (catalog-only) Playwright acquisition (closes #188)
**2026-08-14**

- **`SkoolConnector::fetch()`** now really walks communities → selected
  courses → lesson trees, instead of returning a canned "blocked on
  #99" error — the same `dbs_connector_support::python_launch` split
  `dbs-connector-reddit` (#187) established. Two calls to a new
  `scripts/acquire.py` (embedded via `include_str!`, staged to a temp
  file at run time): one navigates each community's classroom page
  (auto-discovering joined communities from the home page when none
  are configured), the other navigates every selected course's
  classroom page. Both hand back raw, undecoded `__NEXT_DATA__` blobs;
  Rust does 100% of the parsing (`parse_courses`, `course_selected`,
  `parse_lessons`, the community/course/lesson → `BackupItem` mapping)
  with the exact same pure functions its fixture-data tests already
  exercised before this issue existed.
- **Deliberately catalog-only**: no per-lesson page visits, no
  resource/video downloads, no `.meta.json` sidecar/resume, no
  GitHub-zip archiving of linked repos. `videoLink`/`videoId`/
  `resources` only populate when the course-tree payload itself
  happens to carry them — every community effectively backs up the
  way the reference's `no_download_communities` mode already works.
  Per-lesson enrichment and the download pipeline are a follow-up.
- Ported faithfully from the reference: auto-discovery finding zero
  communities is a hard `ConnectorAuthError` (a degraded-but-not-
  logged-out session should never silently "succeed" with 0 items); a
  community whose classroom page fails to load is simply skipped, not
  fatal; a `courses` filter or any course-page load failure withholds
  that community's `ReconcileMarker` (scoped `tag:<group>`, one per
  community) so deletion detection never runs against a partial walk.
- Tested against a fake acquisition-script stub for both call modes
  (mirrors `dbs-connector-reddit`'s identical convention — no real
  Playwright/network access needed in CI), plus direct tests of the
  per-record bookkeeping (`emit_and_track`) fetch() uses to build
  `live_by_group`/checkpoints.

## `dbs-connector-reddit`: wire fetch() to a real Playwright acquisition script (closes #187)
**2026-08-14**

- **`RedditConnector::fetch()`** now really acquires the saved feed
  instead of returning a canned "blocked on #99" error. It shells out
  (via #99's `dbs_connector_support::python_launch`) to a new
  `scripts/acquire.py`, embedded into the binary at compile time and
  staged to a temp file at run time. The script's only job is browser
  automation: launch the captured session as a scrubbed-UA Chromium
  context (ported from the reference's `_playwright.launch_scrubbed_context`),
  establish the reddit.com origin, verify login via `/api/me.json`,
  and page the cookie-authenticated `saved.json` feed via a same-origin
  in-page `fetch` — Reddit's edge 403s a separate HTTP client even with
  valid cookies, so the fetch has to run inside a real page. It hands
  back the raw, undecoded listing `children`; Rust's existing
  `record_from_child`/`to_item` (already written and tested against
  fixture data) do the actual record mapping, so that logic lives in
  exactly one place.
- Script result contract: one line of JSON on stdout —
  `{"ok": true, "account": ..., "children": [...]}` on success, or
  `{"ok": false, "kind": "auth"|"config"|"transient"|"rate_limited",
  "message": ...}` on failure — mapped straight to the matching
  `ConnectorError` variant.
- `fetch()` still validates everything that doesn't need a browser
  first (config, secret, session directory) before ever spawning the
  script, unchanged from before.
- Tested against a fake acquisition-script stub (a shell script
  standing in for the Python interpreter, mirroring
  `dbs_connector_support::python_launch`'s own tests and
  `dbs-connector-youtube`'s fake-yt-dlp convention) — no real
  Playwright or network access needed in CI.
- A real end-to-end run still needs a captured login session; that
  capture flow (`dbs-web::setup::run_capture_job`) is #99's other
  remaining unwired caller, out of scope here.

## `dbs-cli`: wire `dbs research` to the real pipeline (closes #189)
**2026-08-14**

- **`dbs research youtube`** and **`dbs research youtube-backup`** now call
  the real `dbs_research::pipeline::run_pipeline`/`run_pipeline_for_videos`
  instead of printing a stub message — the CLI reaches the same real
  YouTube-search-or-selection → NotebookLM-synthesis → report pipeline
  `dbs-web`'s `/api/research` routes (#177) already drive.
- **`youtube-backup`** converts its matched `ItemRow`s to `VideoMeta` via
  a new `item_row_to_video_meta` in `dbs-cli` — an intentional duplicate
  of `dbs-web`'s identical helper (same precedent as `find_python`: two
  small independent binaries, not worth a shared crate for one function).
- Every real run still fails cleanly at the NotebookLM step —
  `notebooklm::UnimplementedClient` is the only concrete client until
  Decision 4's `nlm`/`notebooklm-mcp` adapter lands (#84).
- `--auth-state` is accepted and echoed but not yet consumed by the real
  client (nothing to authenticate against yet).
- Tests updated for the real pipeline: two stub-era tests replaced with
  environment-robust ones (assert non-zero exit + no report file written,
  without pinning which of yt-dlp-missing/no-network causes the failure
  first), plus a new test that seeds a real backed-up video and confirms
  `youtube-backup` selection succeeds and the pipeline actually runs
  before failing at NotebookLM.

## `dbs-web`: wire `dbs serve --schedule`'s scheduler (closes #190)
**2026-08-14**

- **`dbs serve --schedule`** now starts a real background loop
  (`dbs-web::scheduler`) instead of just noting the flag and doing
  nothing — mirrors the reference's `create_app(schedule_seconds=...)`:
  wakes every 60 seconds, checks which enabled sources are due via a
  new `BackupService::due_sources`, and if any are, starts the same
  `{all: true, only_due: true}` job the web UI's "Backup all" button
  would, on the *same* `JobManager` `/api/backup` uses — a scheduled
  run shows up in the UI's live progress and history like any other.
  `JobAlreadyRunning` is swallowed (a run already in flight just gets
  picked up again next tick); any other tick failure is logged to
  stderr and the loop keeps going.
- This port's `--schedule` is a bare on/off flag (the reference's is a
  float interval in seconds) — a fixed 60-second tick is this port's
  substitute rather than a new CLI knob.
- `BackupService` gained a public `due_sources(now)` method, extracted
  from `backup_all`'s existing (private) `only_due` filtering logic —
  used here so an idle tick never spawns an empty job.
- `cmd_serve`'s stderr message updated to describe the real behavior.
- 4 new tests in `dbs-web::scheduler` exercising the tick logic
  directly (due / not due / already-running-job / the `spawn` loop
  itself catching a source becoming due), no real 60-second wait
  needed since the tick interval is test-injectable.

## `dbs-web`: real `/api` research routes (closes #177, closes #169)
**2026-08-14**

The last of #169's eight-issue `/api` split — `dbs serve`'s shipped SPA now has a
fully wired `/api` surface end to end.

- **`GET /api/research/meta`** reports `ready`/`pip_requirements`/`missing`
  (yt-dlp, the pipeline's one real installable dependency — gained a new
  `dbs_research::youtube_search::yt_dlp_available()`), `auth.configured`
  (`dbs_research::notebooklm::resolve_auth_state`), `youtube_sources`
  (every configured `youtube`-type source), and `default_questions`.
- **`POST /api/research/install`** (`pip install yt-dlp`) and
  **`POST /api/research/login`** (NotebookLM login capture, fails cleanly
  pending issue #99) both stream through the *same* `/api/setup/:id/stream`
  mount #175's connector install/capture already uses — one shared
  `job_manager`, matching `app.js`'s own `streamSetup` call for all three.
- **`POST /api/research`** starts the YouTube-search-or-backup →
  NotebookLM-synthesis → report pipeline as a background job on a
  *separate* `research_job_manager` (kept apart from the shared one so
  `/api/research/current` can't report back an unrelated backup/install
  job). Search mode bridges `dbs_research::pipeline::run_pipeline`
  directly; backup mode selects already-backed-up videos via
  `BackupService::select_youtube_backup_videos` and converts each
  `ItemRow` into a `dbs_research::VideoMeta` (a small `dbs-web`-side
  bridge — the two crates deliberately don't depend on each other).
  Every real run fails cleanly at the NotebookLM step
  (`UnimplementedClient` — Decision 4's `nlm`/`notebooklm-mcp` adapter
  isn't built yet, same external-tool boundary as issue #99), but
  search/selection, progress events, and report rendering are all real
  up to that point.
- **`GET /api/research/:id/stream`** is a dedicated SSE handler, not a
  `crate::jobs::sse_router` mount — its `end` event needs `result`
  (singular) instead of every other job stream's `results` (plural),
  plus a `connector` field hoisted out of the job's `spec`. Built on
  the same buffered/live/terminal `Job::subscribe` primitive (now
  `pub(crate)`) every other stream uses.
- **`GET /api/research/:id/report`** serves the finished job's already-
  rendered Markdown report as a real download; **`GET /api/research/current`**
  mirrors `/api/backup/current`'s reattach-after-reload role.
- 12 new router-level tests, including a full backup-mode pipeline run
  against a seeded YouTube video (real selection, real progress events,
  clean NotebookLM-boundary failure) — kept hermetic by testing the
  backup-mode path (no yt-dlp/network dependency) rather than search
  mode for the full-pipeline exercise.

## `dbs-web`: real `/api` export routes (closes #176)
**2026-08-14**

- **`GET /api/export`** is a real file download, not JSON — the
  shipped export form (`app.js`) submits by navigating the browser
  straight to this URL, so the response carries its own
  `Content-Type`/`Content-Disposition`. `dbs_core::Exporter::media_type`/
  `file_ext` already existed for exactly this (their own doc-comments
  call out "the seam a future web layer would use"), so there's no
  format→extension table to invent — `format`/`source`(repeatable)/
  `type`(repeatable)/`since`/`until`/`include_deleted`/
  `include_revisions`/`no_raw` all map straight onto `ExportQuery`,
  same as `/api/items`. No `encrypt`/passphrase support: the shipped
  frontend's export form has no such field, so
  `BackupService::export`'s `encrypt_passphrase` parameter is simply
  never used from this route.
- **`POST /api/export-notes`** bridges `dbs_core::export_notes`
  directly (`full` inverted is `incremental`, mirroring
  `cmd_export_notes`'s own `!full` wiring) — writes one Markdown note
  per live item into a caller-given `out_dir` and returns
  `{item_count, path, since}`, same trust boundary as every other
  `dbs serve` mutation (the CLI's own `export-notes` command accepts
  an arbitrary directory from its caller too).
- 3 new router-level tests, including a real download round-trip
  (`GET /api/export?format=json` against a seeded two-item database,
  asserting the actual `Content-Type`/`Content-Disposition` headers
  and body content) and an unzipped notes-export writing real
  `.md` files to a temp directory.

## `dbs-web`: real in-UI setup & browser-auth capture routes (closes #175)
**2026-08-14**

- **`POST /api/connectors/:type/install`** derives and runs the
  connector's `pip install`/`playwright install chromium` steps
  (`dbs-web::setup::install_commands`/`run_install_job`, already fully
  real since #83) as a background job on the shared `JobManager`
  `/api/backup` (#174) uses — progress streams over the same
  `GET /api/setup/:id/stream` mount `jobs::sse_router` provides for
  free once nested at `/api/setup` in `lib.rs`.
- **`POST /api/connectors/:type/capture`** and
  **`POST /api/sources/:name/capture`** both resolve their target via
  `BackupService::resolve_capture_target` (accepts either a bare
  connector type or a configured source name) before starting a job
  that fails cleanly with the documented issue #99 blocker
  (`run_capture_job`) — an unresolvable target gets a specific error
  instead of the generic "needs Playwright" one.
- **`POST /api/connectors/:type/import`** and
  **`POST /api/sources/:name/import`** are the headless-server
  workaround: a `multipart/form-data` upload of a session artifact
  `dbs capture` produced on a machine with a display. Validated with
  the functions already built for this
  (`validate_netscape_cookies`/`validate_storage_state`/
  `extract_session_zip`), written to `AuthCapture::target_path` (or a
  default under `<base_dir>/captures/` — every real connector today
  leaves `target_path` unset), and registered as a secret via
  `dbs-web::envfile::set_var` keyed by `AuthCapture::secret_key`.
- **`allow_setup` (`dbs serve --no-setup` inverted) now actually gates
  these five routes**, not just their buttons in the UI — a
  `--no-setup` server refuses to install/capture/import even if a
  client calls the routes directly. (`lib.rs`'s own doc-comment on
  `allow_setup` named this issue as the one that would wire this up.)
- `GET /api/setup/:id/stream` inherits #174's `jobs::sse_router`
  terminal `end`-event fix for free (the shared primitive both routes
  nest, fixed once rather than per-route) — `streamSetup` (`app.js`)
  gets the real snapshot its own `end` listener expects, not just an
  unnamed event stream that silently closes.
- `/import`'s scope (originally under #172) moved here during #172's
  implementation — it shares this issue's `dbs-web::setup` validation
  machinery, not #172's connectors/sources listing routes.
- 9 new router-level tests, including a fixture connector handshake
  declaring `auth_capture` (extending #172/#173's fixture-connector
  technique) and a hand-built `multipart/form-data` body (no new
  dependency needed just for tests).

## `dbs-web`: real `/api` backup trigger + live progress routes (closes #174)
**2026-08-14**

- **`POST /api/backup`** starts a `BackupService::backup_source`/
  `backup_all` run as a background `crate::jobs::Job` and returns its
  snapshot immediately (`openProgress`, `app.js`). A `source` that
  isn't configured is rejected synchronously with a 400 (a cheap
  in-memory `Config::sources` lookup, no DB open needed); a second
  trigger while one is already running is refused with a 409. Whether
  the run itself succeeds surfaces later through the job's own
  `status`/`error`/`results`.
- **`GET /api/backup/:id/stream`** and **`GET /api/backup/current`**
  are `crate::jobs`' existing SSE/snapshot primitives (issue #80),
  nested under `/api/backup` — unchanged from that issue beyond the
  `end`-event fix below.
- **`POST /api/backup/:id/cancel`** requests the job's cooperative
  early stop (`stopBackup`, `app.js`) — 404s for an unknown job id,
  otherwise always 200.
- **`crate::jobs::Job::subscribe` gained a terminal `end` SSE event**
  carrying the job's final snapshot — every `/api/*/:id/stream`
  consumer `app.js` already ships (`streamSetup`, `openProgress`,
  `resumeResearchIfRunning`) listens for a named `end` event, but the
  #80 primitive only ever emitted plain, unnamed progress events and
  then silently closed the connection. Fixed at the shared primitive
  rather than duplicated per-route, so #175's setup/capture streams
  and #177's research stream inherit the fix for free. A late
  subscriber — attached only after the job already finished — now
  gets the terminal event immediately instead of an open stream that
  never closes.
- A small watcher-thread bridge (`CancelBridge`) translates
  `crate::jobs::Job`'s cooperative-cancel flag into a real
  `dbs_core::CancelToken` `BackupSourceOptions`/`BackupAllOptions` can
  poll — the two types are structurally identical (`Arc<AtomicBool>`
  wrappers) but live in different crates with no shared trait.
- The job's `results` list is populated from `backup_source`/
  `backup_all`'s actual return value, not from `ProgressEvent`'s
  `SourceDone.result` field alone — a disabled/VPN-skipped/locked/
  dry-run source's `RunResult` never reaches `on_progress` at all
  (`backup_source` returns before calling `sink.emit` on those
  early-exit paths), so relying on the progress stream alone would
  have silently dropped it from `snap.results`. Mirrors `dbs-cli`'s
  own `cmd_backup`, which prints from that same return value.
- 15 new tests: 2 in `crate::jobs` for the `end`-event fix, 13
  router-level in `dbs-web`'s crate root covering the full trigger →
  poll → cancel → stream lifecycle against a real (near-instant,
  disabled-source) backup run.

## `dbs-web`: real `/api` secrets management routes (closes #173)
**2026-08-14**

- **`GET /api/secrets`** returns `secrets` (one entry per secret key a
  *configured* source's connector actually needs — `name`/`set`/
  `in_env_file`/`in_process_env`/`sources`, matching `loadSecrets`'s
  (`app.js`) per-row rendering) and `allowed` (every secret key any
  *registered* connector declares, configured or not — the wider list
  its "Set another key" picker draws from), plus `env_file` (the `.env`
  path being read/written). Pure `envfile` (#82) plumbing — no CLI
  equivalent to mirror, unlike every other `/api` slice so far.
- **`POST /api/secrets`** writes one secret via `envfile::set_var`,
  after checking the name against the registered-connector allow-list
  (rejecting an arbitrary env var name). Returns
  `shadowed_by_process_env`, mirroring `resolve_passphrase`'s own
  precedence note that a process-env value of the same name wins over
  `.env` at runtime — `saveSecret` (`app.js`) surfaces this as an
  informational toast, not an error.
- **`DELETE /api/secrets/:name`** removes one secret via
  `envfile::unset_var` — unlike the POST route, not checked against the
  allow-list, so clearing a stray/no-longer-declared key still works.
- `Config` gained an `env_file_path()` method (`<config file's
  directory>/.env`) so `dbs-web` reads/writes the identical file a
  `dbs backup` invocation's `resolve_passphrase` would, rather than a
  second, drifting path convention — `dbs-cli`'s own private
  `load_env_secret_store` helper already computed the same thing
  ad hoc and was left as-is (three call sites, one of which doesn't
  otherwise load a `Config`, so unifying them wasn't worth the risk).
- 8 new router-level tests, including a real spawnable
  `dbs-connector-fixture` handshake declaring `requires_auth`/
  `secret_keys` (extending #172's fixture technique) and a
  temp-directory `Config::base_dir` override so secrets tests never
  touch a real `./.env` next to wherever `cargo test` happens to run.

## `dbs-web`: real `/api` sources & connectors routes (closes #172)
**2026-08-14**

- **`GET /api/connectors`** bridges `BackupService::list_connectors`
  directly — the flat `Vec<ConnectorInfo>` array `app.js`'s
  `refreshStatus`, `loadConnectorsPanel`, and `loadAddForm` all fetch.
  Closing this route surfaced two upstream gaps in `dbs-core` itself,
  fixed alongside it: `ConnectorInfo` had no `auth_capture` field at
  all (the web UI needs it to know whether/how to show a capture
  button), and `AuthCapture` had no `per_source` field (whether a
  capture targets one configured source vs. the connector type
  generally) — both now populated from each connector's handshake,
  and set `true` on the three connectors with a personal-login-session
  capture story (reddit, skool, youtube).
- **`GET /api/sources`** bridges `BackupService::list_sources` — every
  configured source's `name`/`type`/`enabled`/`schedule`/`backed_up`.
- **`POST /api/sources`** bridges `BackupService::add_source`, taking
  the exact body `app.js`'s add-source form submit sends
  (`name`/`type`/`options`/`store_media`/`max_media_mb`/
  `requires_vpn`) and returning `{"name", "type"}` on success, all the
  frontend's own success toast reads.
- `/api/sources/:name/import` and `/api/connectors/:type/import` were
  originally scoped to this issue too, but moved to #175 (in-UI setup
  & capture) since they share that slice's `dbs-web::setup` validation
  machinery rather than anything here.
- 6 new router-level tests, including a real spawnable
  `dbs-connector-fixture` shell-script handshake (same technique
  `dbs-core`'s own `registry.rs` tests use) to exercise a real
  directory-scan discovery + successful `POST /api/sources` end to
  end, not just the read-only listing routes.

## `dbs-web`: real `/api` item browse & media routes (closes #171)
**2026-08-14**

- **`GET /api/items`** mirrors `dbs items`' list branch (`cmd_items`,
  `dbs-cli/src/main.rs`): `source`/`type` (repeatable), `q` (search
  text), `since`/`until`, `include_deleted`, `limit`, `offset`. Since
  `source`/`type` are sent as repeated query keys (`app.js`'s
  `browseParams()`) and `axum::extract::Query` can't collect those into
  a `Vec`, this parses the raw query string directly via
  `url::form_urlencoded` (already a `dbs-web` dependency) rather than
  pulling in a new query-string crate. Response envelope
  (`{items, total, limit, offset}`) matches both the CLI's own
  `--json` output and what `loadBrowseCardsFlat`/`loadBrowseTable`
  expect.
- **`GET /api/items/:id`** bridges `BackupService::get_item` directly —
  its `media` array (`id`/`filename`/`mime`/`kind`/`byte_size`/
  `has_data` per entry) already matched `openItemDrawer`'s (`app.js`)
  expectations exactly, no gap to close beyond the bridge itself.
- **`GET /api/media/:id`** is a real binary response, not JSON — reads
  `Storage::get_media_blob` directly (no `BackupService` wrapper exists
  for it, and none was needed) and reconstructs the raw bytes from the
  `ItemRow`'s JSON-array-of-byte-numbers encoding (chosen storage-side
  to keep that method's return type uniform with every other
  `ItemRow`-returning one).
- **`GET /api/thumb/:id`** — `:id` here is an *item* id, not a media
  id (`app.js`'s `thumbUrl` calls it as `/api/thumb/${it.id}`): serves
  the item's own local image media if it has one, otherwise redirects
  (307) to YouTube's public thumbnail CDN for a YouTube item whose URL
  carries a `?v=` video id — YouTube connector items have no local
  image media rows at all, so proxying would mean fetching bytes this
  server never stored.
- 12 new router-level tests, including a real temp-file-backed
  `SqliteStorage` seed helper (`seed_db`/`seed_youtube_item`) — the
  `:memory:` database `test_config()` uses for #170's read-only tests
  doesn't work here since every `/api` handler opens its own fresh
  connection per request, and each `:memory:` connection is an
  independent, empty database.

## `dbs-web`: real `/api` dashboard status routes (closes #170)
**2026-08-13**

- **`dbs serve`'s shipped SPA can now actually load its dashboard.**
  `GET /api/meta`, `/api/status`, `/api/metrics`, `/api/history`,
  `/api/vpn`, and `/api/verify` are real routes now, bridging into
  `dbs-core`'s existing `BackupService`/`Storage` exactly the way each
  `dbs-cli` `cmd_status`/`cmd_history`/etc. already does — first slice
  of #169's umbrella `/api` tracking issue, establishing the pattern
  every remaining slice (#171-#177) reuses: an async Axum handler
  clones `AppState`'s `Arc<Config>`, opens a fresh `SqliteStorage` +
  `ConnectorRegistry`/`BackupService` inside `tokio::task::spawn_blocking`,
  and returns JSON — `dbs-core` stays fully synchronous throughout.
- **`dbs serve` loads a real `Config` at startup now**, same as every
  other CLI command — previously it never read `dbs.toml` at all.
  `dbs_web::serve`/`router` gained a `ServeOptions` parameter
  (`config`, `allow_setup`, `schedule`) carrying that through to
  `AppState`.
- **`dbs_core::build_registry`/`connector_search_dirs`** (generalized
  out of a `dbs-cli`-private helper from #160): `dbs-web` needed the
  exact same "scan `PATH`/`connectors_dir`, handshake, apply overrides"
  logic `dbs-cli` already had — moving it into `dbs-core` means both
  build the identical registry instead of two implementations quietly
  drifting apart. `dbs-cli`'s own `build_registry` is now a thin
  eprintln-warnings wrapper around it.
- **`SourceStatus` gained a `requires_vpn` field** — the shipped
  frontend's `sourceRow`/`refreshVpn` (`app.js`) already read
  `r.requires_vpn` off every `/api/status` row; `service.status()` now
  actually populates it from `SourceConfig`.
- `/api/vpn` returns a single aggregate object (`{relevant, up, detail}`)
  matching the frontend's exact expectation, computed from
  `in_named_netns`/`named_netns_exists` — fail-closed (`up: false`)
  whenever a through-VPN run would currently fail, whether the
  namespace isn't up at all or is up but this process isn't joined to
  it. `/api/verify` honestly reports 501 (`dbs verify` itself is still
  an unimplemented CLI stub — no behavior to bridge to yet).
- Fixed `dbs-cli/tests/serve.rs`'s integration tests, all of which
  previously ran `dbs serve` with no `dbs.toml` in scope — harmless
  before (the server never touched config), a hard failure now (config
  loading happens before the listener binds). Each spawns its own
  temp-dir config via `--config`.

## `dbs-core`: per-source connector config over the subprocess wire (closes #166, implements ADR-0002)
**2026-08-13**

- **`WireRunContext` gains a `config: HashMap<String, serde_json::Value>`
  field**, populated by `SubprocessRunner::run_connector` from
  `SourceConfig::options` — the same `[sources.NAME]` TOML map already
  serialized into `config_json` for `Storage::upsert_source`, now also
  reaching the spawned connector. `SourceConfig::options` is
  `HashMap<String, toml::Value>`; the conversion to JSON happens once,
  host-side, in `dbs-core` (already depending on both `toml` and
  `serde_json`) rather than pulling a `toml` dependency into all 14
  connector crates just to name the wire field's type.
- **`Connector` gains a default no-op `configure(&mut self, options:
  &HashMap<String, serde_json::Value>) -> Result<(), ConnectorError>`**
  method, mirroring `open`/`close`'s existing default-method shape.
  `dbs_connector_support::run_connector_main` calls it right after a
  real run's wire context arrives, before `open`/`fetch` — a failure
  short-circuits into `WireOutcome::Error` the same way a failing
  `open()` already does, no new wire vocabulary needed. 11 of the 14
  built-in connectors need no code change at all; they inherit the
  no-op default.
- **`dbs-connector-mastodon`, `dbs-connector-podcast`, and
  `dbs-connector-bluesky` implement `configure()`** to read `instance`,
  `feeds`, and `identifier` respectively out of a source's config —
  closing the specific gap #166 reported: with this, `mastodon` and
  `podcast` can genuinely run against a real Mastodon instance / real
  feed list from a real `dbs backup` invocation for the first time,
  not just against a `DBS_..._TEST_BASE_URL`-pointed mock server in
  their own test suites. `bluesky`'s `identifier` wasn't blocking a run
  before (nothing validated it), but now authenticates as the right
  account instead of an opaque empty string.
- Each of those three connectors' `subprocess_binary_integration.rs`
  gains a test proving the *production* path — wire `config`, not the
  test-only `DBS_..._TEST_BASE_URL`/`DBS_PODCAST_TEST_FEED_URL` env var
  overrides #164 introduced — actually reaches a real spawned binary
  and takes effect, end to end through a real subprocess boundary.
  `dbs-core`'s own `run_stream_integration.rs` adds a
  `SubprocessRunner`-level test parsing a real `dbs.toml` and
  confirming a `[sources.NAME]` TOML key reaches the connector as JSON.
  Every existing `WireRunContext { .. }` test construction (raindrop's
  plus all 13 from #164) picked up the new field — the mechanical
  migration cost ADR-0002 flagged up front.
- Two of the three-connector subprocess-integration test files needed
  a small unrelated fix while adding a second env-var-touching test to
  each: `DBS_MASTODON_TEST_BASE_URL`/`DBS_PODCAST_TEST_FEED_URL`/
  `DBS_BLUESKY_TEST_BASE_URL` are process-global, and Rust runs
  `#[test]` functions in parallel by default — a shared `static
  ENV_LOCK: Mutex<()>` now serializes the two tests in each file that
  touch the same variable.

## 13 connectors: real dbs-connector-* subprocess binaries (closes #164)
**2026-08-13**

- Every remaining connector crate — `dbs-connector-bluesky`, `-github`,
  `-mastodon`, `-pinboard`, `-pocketcasts`, `-podcast`, `-readwise`,
  `-reddit`, `-skool`, `-spotify`, `-udemy`, `-vimeo`, `-youtube` — gets
  a `src/main.rs` mirroring `dbs-connector-raindrop`'s (#161): construct
  the connector with its default config, apply a test-only env-var
  override so integration tests can redirect it at a local mock instead
  of the real service, then hand it to
  `dbs_connector_support::run_connector_main`. All 14 built-in
  connectors are now real, independently spawnable binaries, not just
  `raindrop`.
- **10 plain REST+HTTP connectors** (bluesky, github, mastodon,
  pinboard, pocketcasts, podcast, readwise, spotify, udemy, vimeo) each
  get a `tests/subprocess_binary_integration.rs` proving the full
  ADR-0001 protocol against a real compiled binary: a handshake-validity
  check, plus a real run against a `mockito`-served fixture (or, for
  `podcast`, a served RSS feed — it has no fixed API host, so its
  redirect target is the feed URL list itself) landing real items in a
  real `SqliteStorage`. `mastodon` and `bluesky` redirect via a plain
  `Config` field (`instance`/`service`) rather than a `with_base_url`
  builder; `spotify` needs two redirects (OAuth token exchange +
  REST API); `udemy`/`vimeo` also have an off-by-default
  `download_videos` → `yt-dlp` path already covered by their own unit
  tests, left unwired here since it's unreachable from default config.
- **`dbs-connector-youtube`** has no HTTP layer at all — every fetch
  shells out to `yt-dlp`. Its test redirects via `with_yt_dlp_bin` to a
  fake script instead of a mock server, same two-test shape otherwise.
- **`dbs-connector-reddit` and `dbs-connector-skool`** are Playwright-
  session connectors permanently blocked pending issue #99 — `fetch()`
  unconditionally returns a `ConnectorError::Config` today, by design,
  regardless of input. Their integration tests prove the *subprocess
  boundary* correctly relays that error end to end (a real, fully-valid
  session directory still produces the expected error), rather than
  asserting real items land in storage — there's nothing to fetch until
  #99 lands.
- Known follow-up gap surfaced by this work, not solved here: several
  connectors (`bluesky`'s `identifier`, `mastodon`'s `instance`,
  `podcast`'s `feeds`) have config fields that are genuine per-source
  business data, not test infrastructure — today nothing passes a
  source's real config into a spawned connector process beyond secrets,
  so a real `dbs backup` run of these needs that plumbing before it can
  do real work. `mastodon`/`podcast` even hard-fail immediately without
  it (empty `instance`/no `feeds`); `bluesky`'s empty `identifier`
  happens not to block a real Bluesky login. This is a distinct gap from
  #99's browser-session blocker and deserves its own future issue.

## `dbs-cli`: real connector-candidate discovery (closes #160)
**2026-08-13**

- **`dbs-cli` no longer always constructs an empty registry.** Every
  one of the 17 call sites that used to hardcode
  `ConnectorRegistry::from_resolved([])` now calls a new `build_registry`
  helper: `dbs_core::scan_connector_candidates` scans `PATH` (plus an
  optional `[dbs] connectors_dir` config override, new `Config` field)
  for `dbs-connector-*` binaries, `dbs_core::override_map_from_config`
  converts `Config.connectors`'s per-type overrides into the shape
  `ConnectorRegistry::discover` (#45) expects, and any candidate that
  fails to load is reported as a warning rather than silently dropped.
- **Deliberately does *not* default to scanning this binary's own
  directory** (`std::env::current_exe()`'s parent) even though that
  would make a `cargo build --workspace` dev setup "just work" — in a
  Cargo workspace that directory holds every other crate's binary too,
  which would make discovery depend on incidental build layout instead
  of a real, portable install convention. `PATH` (optionally plus an
  explicit `connectors_dir`) is that convention, and it has a welcome
  side effect: every existing `dbs-cli` integration test that relies on
  "connector not found" behavior (many of them use `raindrop` as a
  stand-in type name, now a real, discoverable connector as of #161)
  keeps working unchanged, since this sandboxed test environment's
  `PATH` never has any `dbs-connector-*` binaries on it.
- 4 new integration tests (`dbs-cli/tests/connector_discovery.rs`)
  exercise the "actually found" path for real: `dbs-connector-raindrop`
  becomes a dev-dependency of `dbs-cli` purely so its binary gets built
  alongside the test suite, then `connectors_dir` points at a copy of
  it and `dbs connectors describe`/`list`/`sources check` are asserted
  to actually find and handshake with it — plus one test confirming
  the pre-#160 "nothing configured, nothing found" behavior still holds.
- With #161 and this issue both done, `dbs backup <name>` can now
  genuinely discover *and* run a real connector end to end for the
  first time in this port — the last missing piece was always either
  "no candidate to discover" or "nothing to run once discovered," and
  both are now closed for at least one real connector (`raindrop`).
  Wiring up the remaining 13 built-in connectors as real binaries
  (#161's own follow-up) is what extends this to the rest.

## `dbs-connector-support`/`dbs-connector-raindrop`: real subprocess binary (closes #161)
**2026-08-13**

- **New `dbs_connector_support::subprocess_main` module** — the
  connector-side counterpart to `dbs-core::run_stream` (#157), which
  only implemented the host side. `run_connector_main(connector: &mut
  dyn Connector)` is meant to be a `dbs-connector-<type>` binary's
  entire `main.rs`: it writes the handshake line (built entirely from
  `Connector`'s own trait methods — no per-connector code needed),
  then blocks reading one line from stdin. A discovery-only spawn
  (`ConnectorRegistry::discover`, #45) never writes one and gets
  killed after its handshake is read, so this returns cleanly on EOF;
  a real run (`run_connector_subprocess`) writes a `WireRunContext`
  right after spawn, and once that arrives this reconstructs an
  in-process `RunContext` (building a `Secrets` accessor scoped to
  exactly the connector's declared keys, and a real `ManagedHttpClient`
  when `wants_managed_http()`) and drives `open`/`fetch`/`close`,
  streaming the result back.
- **Fixed a real protocol bug this surfaced:** `run_connector_subprocess`
  (#157) never expected the handshake line a real connector always
  writes first — its first read would try to parse the handshake JSON
  as a `WireLine` and fail as a contract violation. Fixed by having it
  read and discard exactly one line (the handshake — the caller already
  has it from an earlier discovery call) before entering the
  `WireLine` loop; `test_connector_fixture`'s `run` scenarios were
  updated to emit a handshake line first too, so they keep exercising
  the real shape.
- **`dbs-connector-raindrop` gets a real `src/main.rs`** — the first of
  the 14 built-in connectors to actually be a `dbs-connector-<type>`
  binary, proving the pattern end to end. `DBS_RAINDROP_TEST_BASE_URL`
  is a test-only env var override for pointing a real spawned binary at
  a mock HTTP server instead of the live API.
- 2 new integration tests (`dbs-connector-raindrop/tests/subprocess_binary_integration.rs`)
  spawning the actual compiled binary: its handshake discovers cleanly
  and matches the connector's real contract, and a full run against a
  mock Raindrop API commits real items through the complete subprocess
  boundary — discovery, run-context write, `FetchEvent` stream, and
  storage commit, with nothing faked.
- **Wiring up the remaining 13 connectors is intentionally out of
  scope here** — mechanical repetition of the same `main.rs` shape now
  that the pattern and shared helper exist, left as follow-up work.
  `dbs-cli` still passes an always-empty `ConnectorRegistry::from_resolved([])`
  (#160) — real candidate discovery isn't wired up yet either, so
  `dbs backup` still can't reach a real connector end to end even with
  this issue done.

## `dbs-core`: connector run-stream bridge (closes #157)
**2026-08-13**

- **New `dbs-core::run_stream` module** — the Rust, subprocess-shaped
  counterpart to the reference's in-process `Engine.run_source`
  (`src/dbs/core/engine.py`). `registry.rs` (#45) only implemented
  ADR-0001's handshake step; this implements the run/stream half (steps
  2-3): [`WireRunContext`] is written as one JSON line to a connector
  subprocess's stdin, [`WireLine`] (zero or more `FetchEvent`s, then
  exactly one terminal [`WireOutcome`]) is read back from its stdout.
  [`run_connector_subprocess`] drives that stream — buffering items,
  flushing them at a batch cap or on a `Checkpoint`, collecting
  `ReconcileMarker` scopes, and (on a clean finish) handing them to the
  existing `engine::sweep_deletions` (#20) — matching the reference's
  invariant that an exception mid-stream skips the trailing flush and
  the sweep entirely, so a truncated run never soft-deletes from a
  partial enumeration.
- **A deliberate improvement over the reference:** cancellation. Python
  can only stop *reading* a hung in-process generator; a connector
  subprocess is a real OS process, so a background thread watches the
  `CancelToken` and calls `Child::kill()` directly, actually
  terminating a stuck connector instead of just abandoning interest in
  it. (Checking cancellation inline between reads doesn't work — the
  read blocks — so this needed its own thread, verified by a test that
  cancels a fixture mid-hang and asserts the call returns promptly
  instead of waiting out its sleep.)
- **`SubprocessRunner`** is the production `ConnectorRunner`
  (`dbs-core::service`, #46's seam) this module exists to supply —
  replacing `UnimplementedRunner` at all 17 call sites in `dbs-cli`. It
  resolves a connector's declared `secret_keys` from the process
  environment (never more — "a subprocess literally cannot read a
  secret it wasn't handed," per ADR-0001) and a source's
  `store_media`/`max_media_mb`/download directory from `Config`.
- **`ConnectorRunner`'s trait signature grew** to match what a real
  implementation needs: `storage: &mut dyn Storage` (checkpoints commit
  *during* the run, not after), `source_name`, `limit`, and `cancel`.
  `BackupSourceOptions` gained a `cancel: Option<CancelToken>` field —
  `backup_all` deliberately leaves it `None` per source ("in-flight
  sources still finish and commit"), but plain `dbs backup NAME` now
  wires the CLI's own Ctrl+C handler through it, fixing a real gap
  where that handler was already installed but silently unused outside
  `--all`.
- **`Handshake` (#45) gained a `volatile_fields` field.** `engine::prepare`
  (#17) has taken `volatile_fields` as a parameter since it was written,
  but nothing populated it from a real handshake until this issue
  actually needed to call `prepare()` with live data.
- 7 new integration tests (`tests/run_stream_integration.rs`), spawning
  the `test_connector_fixture` binary's new `run` scenarios against a
  real in-memory `SqliteStorage`: a clean run's items/checkpoint/cursor
  land correctly, the connector receives exactly the `WireRunContext`
  fields the host sent (mode/limit/secrets/source name), a full
  enumeration's reconcile marker sweeps the right items, a
  connector-reported error is `Partial` only when something already
  committed, a malformed line and a missing terminal line are both
  contract violations, and cancellation kills a hung connector instead
  of waiting for it.
- **Two real, separate gaps surfaced while implementing this** (not
  silently left for later): every `dbs backup`/`dbs backup --all` call
  site in `dbs-cli` still constructs `ConnectorRegistry::from_resolved([])`
  — an always-empty registry, so there's still no real connector to run
  yet — and none of the 14 built-in `dbs-connector-*` crates are actual
  subprocess binaries that speak this protocol (they're plain libraries
  today, exercised only by their own in-process unit tests). Both need
  their own follow-up issues.

## `dbs-connector-support`: TipTap rich-text→Markdown helper (closes #100)
**2026-08-13**

- **New `tiptap` module in `dbs-connector-support`**, a node-for-node
  port of `connectors/_tiptap.py`'s `tiptap_markdown`: converts
  TipTap/ProseMirror rich-text JSON (`"[v2]{...}"`-prefixed, as Skool
  stores lesson descriptions) to GitHub-flavored markdown —
  paragraphs, headings (levels 1-6), code blocks (with language),
  blockquotes, bullet/ordered lists (including nesting), horizontal
  rules, images, hard breaks, and inline marks (bold/italic/code/
  strike/link, with `]` escaped in link text so it doesn't close the
  markdown link early). Unknown node types render their children
  rather than dropping content; anything that isn't a non-empty
  string, or that fails to decode as JSON, passes through unchanged —
  a lesson body must never fail a backup over a rendering quirk, and
  the verbatim payload always survives separately in the item's `raw`.
- **Wired into `skool`'s `lesson_item`:** a lesson's `body` now renders
  through `tiptap_markdown` instead of carrying the raw `desc` string
  unrendered, closing out the note `skool` (#97) left pointing at this
  issue. `reddit`/`skool` acquisition itself is still blocked on #99
  (no browser session to read `desc` from in the first place) — this
  only fixes how an already-fetched `desc` value renders.
- 7 new tests, ported directly from the reference's
  `tests/connectors/test_tiptap.py` fixtures: paragraphs/headings/
  marks, link-text bracket escaping, lists/code-blocks/quotes,
  images/hard-breaks/nested lists, unknown-node passthrough,
  passthrough-and-garbage (plain text, undecodable `[v2]` payload,
  bare JSON doc without the prefix, `None`/empty/non-dict-list
  inputs), and a bare block-array payload without the usual
  `{"type": "doc", ...}` wrapper.

## `dbs-connector-support`: shared Playwright launch helper (closes #99)
**2026-08-13**

- **New `python_launch` module in `dbs-connector-support`.** The
  reference's `connectors/_playwright.py` has one function,
  `launch_scrubbed_context(pw, session_dir, *, headless)`, that drives
  Playwright **in-process**: it takes an already-imported
  `playwright.sync_api` handle and calls
  `pw.chromium.launch_persistent_context(...)` directly, then probes
  and scrubs the `HeadlessChrome` UA token. That has no Rust
  equivalent — there's no Rust Playwright binding, and per
  gap-analysis.md's Connectors-cluster decision 3, there isn't going
  to be one. So this ports the module's *role* rather than its code:
  `reddit`/`skool`/`youtube`/`dbs capture` will shell out to a
  **separate** Python script that itself imports Playwright and does
  the real browser driving, and `python_launch` is the generic,
  Playwright-agnostic half of that split.
- **API surface:** `find_python()` resolves `python3` then `python`
  off `PATH` (same order `dbs-cli`'s `update-ytdlp` command already
  uses); `run_python_script(script, args, timeout)` runs
  `<python> <script> <args...>` to completion using that resolved
  interpreter; `run_python_script_using(interpreter, ...)` is the
  interpreter-injectable form the former delegates to, split out so
  callers (and tests) aren't forced to depend on what's actually
  installed. Both capture stdout/stderr verbatim and have no opinion
  on their format — a browser-automation script might emit a JSON
  result line, for instance, but that's the caller's concern.
- **Reuses `run_with_watchdog`** (the same primitive `vimeo` #94 and
  `udemy` #95 use for their `yt-dlp` downloads) for the stall/wall-
  clock timeout: a hung browser launch must not block a scheduled run
  forever. A zero timeout disables the watchdog and runs inline to
  completion, matching that primitive's existing convention
  elsewhere in the crate. On timeout the subprocess is abandoned, not
  force-killed — the same constraint `run_with_watchdog` already
  documents for its worker thread.
- 7 new tests, run through `/bin/sh` standing in for a Python
  interpreter (the launcher genuinely doesn't care what interpreter
  it invokes, so this avoids any dependency on Python being present
  in the test/CI environment): stdout capture on success, nonzero
  exit code + stderr capture, argument pass-through, a missing-
  interpreter spawn error, a stalled process abandoned past its
  timeout, a zero-timeout inline run, and a smoke test that
  `find_python`'s real `PATH` resolution doesn't panic.
- Not yet wired up: `reddit` (#96) and `skool` (#97) still return
  their own "blocked on #99" `ConnectorError::Config`, since actually
  calling into this helper needs the not-yet-written Python/Playwright
  scripts those connectors would shell out to — that's follow-on work
  for whichever issue re-opens those connectors' acquisition step, not
  this one.

## `youtube` connector (closes #98)
**2026-08-13**

- **New `dbs-connector-youtube` crate — fully implemented, not
  blocked.** Unlike `reddit` (#96) and `skool` (#97), YouTube's lists
  (Watch Later, Liked, watch history, owned playlists) need no
  Playwright browser at all: the reference itself only ever calls
  `yt_dlp.YoutubeDL(...).extract_info(...)`, so this port shells out
  to the `yt-dlp` binary the same way `vimeo` (#94) and `udemy` (#95)
  already do. `yt-dlp --dump-single-json --flat-playlist` mirrors
  `extract_info()`'s return shape exactly — one JSON object with a
  top-level `title` and an `entries` array — fast metadata only, no
  media download; the video URL rides along as a `MediaRef`.
- A full-enumeration source like `reddit`/`skool`: no server-side
  delta, so every run is full and yields one `ReconcileMarker` —
  unless a list failed to load, in which case the whole run's marker
  is withheld (the same "one bad group taints the sweep" shape as
  `skool`'s per-community partial enumeration). A video can live in
  several lists at once, so `external_id` is namespaced by list
  (`"<list>:<video_id>"`) — the same video in Watch Later and Liked
  stays two distinct items; the same video listed twice *within* one
  list keeps only its first occurrence.
- There's no per-item progress to stream during a flat extraction
  (unlike a real video download), so `run_with_watchdog` here is a
  plain wall-clock deadline on the whole extraction (no heartbeat),
  matching the reference's own timeout-only `run_with_watchdog` call.
- Auth is `YOUTUBE_COOKIES_FILE` (a Netscape cookies.txt) or
  `cookies_from_browser` in config to read cookies straight from a
  local browser profile instead — no secret needed.
- Not wired up: same registry run/stream boundary as `raindrop` (#85)
  through `skool` (#97) — but acquisition itself works end to end and
  is exercised as such.
- 14 new `dbs-connector-youtube` tests, most running the real
  acquisition path against a fake `yt-dlp` script on disk that
  branches on a URL substring in its own arguments (the same
  fake-executable pattern `dbs-research`'s YouTube search and
  `dbs-connector-vimeo` already use): missing-cookies/nonexistent-
  cookies-file errors, a full fetch across Watch Later and Liked with
  a reconcile marker, history off by default and included when
  enabled, a failed list withholding the marker while keeping the
  healthy list's items, playlist discovery failing without aborting
  the run, playlist discovery finding and dumping each playlist, a
  duplicate video within one list keeping only its first occurrence,
  and direct unit tests of `entry_record` and `to_item`.

## `skool` connector (closes #97)
**2026-08-13**

- **New `dbs-connector-skool` crate** — scaffolds the connector for
  your Skool communities/courses/lessons, with the same **honestly
  blocked** acquisition posture as `reddit` (#96): Skool has no public
  API, but every classroom page embeds a `__NEXT_DATA__` JSON blob;
  reading it (and visiting each lesson's own page to sniff its Mux
  video/resources) needs a live, logged-in Chromium page, which this
  port has no Playwright launch helper for yet (issue #99, same gap
  `dbs-connector-reddit` documents). `fetch()` validates everything it
  honestly can — `video_cookies_file_env` is declared correctly, the
  `SKOOL_SESSION_DIR` secret is set, the session directory exists, a
  downloads folder resolves — then returns a clear `Config` error
  naming #99.
- **Every genuinely pure function is ported and tested**, ready for
  #99's future acquisition step to call into: extracting a
  community's display name, BFS-searching a `__NEXT_DATA__` tree for
  a key or a specific lesson node, decoding a lesson's JSON-encoded
  `metadata` fields (video/resources/desc, including marking a
  link-only resource as external), matching a configured
  `communities`/`courses` selector (with `"community/course"`
  scoping), reconstructing a Mux HLS URL, matching a URL's host
  against an allow-list, classifying a permanent-vs-transient video
  error, parsing memberships/courses/lessons out of a `__NEXT_DATA__`
  blob (module vs. bare-lesson tree shape), and the
  community/course/lesson → `BackupItem` mapping (a downloaded video
  path winning over the watch link, an unavailable video's link
  suppressed entirely).
- **TipTap rich-text → Markdown rendering is deliberately deferred to
  issue #100** (its own listed unit of work) — a lesson's `body` here
  is the raw `desc` string, unrendered.
- **Deliberately not ported**, for the same reason as acquisition
  itself: resource-file downloads, the `yt-dlp`-driven video download
  once a URL is found, the `.meta.json` sidecar/resume pipeline,
  directory-naming and note-writing, and GitHub-zip archiving —
  pipeline mechanics with zero reachable callers until #99 exists.
- **Not wired up:** same boundary as `raindrop` (#85) through `reddit`
  (#96) for the registry run/stream bridge — and, separately, blocked
  on issue #99 for acquisition specifically.
- 25 new `dbs-connector-skool` tests: the four `fetch()` validation
  stages in order plus the final #99-blocked error, `group_name`'s
  fallback chain, `deep_find`'s BFS-first-match semantics, `json_field`
  parsing embedded JSON strings, `lesson_fields` extracting a video
  link and marking a link-only resource external, `find_lesson_node`
  matching by id under course props, `course_selected`'s scoped and
  unscoped selector matching, `mux_hls_url`'s exact-id-match
  reconstruction, `url_host_matches`'s subdomain handling and
  lookalike rejection, `classify_video_error`'s permanent-vs-transient
  split (never matching a bot-check message), `parse_memberships`
  deduping by slug, `parse_courses`'s tri-state `hasAccess` mapping,
  `parse_lessons` splitting modules from bare lessons,
  `slug_from_community`'s URL/bare-slug handling, all three item
  mappers, `to_item`'s dispatch, and connector metadata matching the
  reference.

## `reddit` connector (closes #96)
**2026-08-13**

- **New `dbs-connector-reddit` crate** — scaffolds the connector for
  your saved Reddit posts/comments, but its acquisition step is
  **honestly blocked**, not implemented: the reference walks
  `saved.json` via a same-origin `fetch` evaluated *inside a real
  Chromium page* on reddit.com, because Reddit's edge fingerprints
  plain HTTP clients and 403s them even with valid cookies — only an
  actual browser carries a genuine TLS/HTTP2 fingerprint. This port
  has no Playwright launch helper yet (`dbs-web`'s own
  `run_capture_job` already documents the identical gap, pointing at
  the same issue #99), so `fetch()` performs every check that
  *doesn't* need a browser — `session_dir_env` is declared correctly,
  the `REDDIT_SESSION_DIR` secret is set, the session directory
  actually exists on disk — and then returns a clear `Config` error
  naming #99, instead of a full run.
- **Everything that doesn't need a browser is implemented and tested
  as pure functions**, ready for #99's future acquisition step to call
  into directly: the raw Reddit listing → record mapping (`t3`
  post/`t1` comment, self-post outbound-URL suppression, non-URL
  thumbnail token filtering), the record → `BackupItem` mapping, and
  the opportunistic outbound-link fetch (`archive_outbound_link`,
  gated on `store_media` — a single plain HTTP hop with no session
  cookies needed, unlike the primary walk).
- Config, capabilities, `item_kinds`, `volatile_fields`
  (`extracted_at`/`score`/`num_comments`, since vote counts tick on
  every live thread), `export_profile` (group by subreddit/flair), and
  `auth_capture` (`browser_session`, matching the reference's capture
  metadata) are all ported in full.
- **Not wired up:** same boundary as `raindrop` (#85) through `udemy`
  (#95) for the registry run/stream bridge — and, separately, blocked
  on issue #99 for acquisition specifically, as described above.
- 17 new `dbs-connector-reddit` tests: the four `fetch()` validation
  stages in order (undeclared `session_dir_env`, missing secret,
  nonexistent session directory, and the final #99-blocked error with
  an otherwise-valid config), `record_from_child` mapping both a post
  and a comment, unknown listing kinds and missing fullnames being
  skipped, a self post's outbound URL/thumbnail token being
  suppressed, `abs_permalink`'s relative/absolute handling,
  `to_item`'s post/comment mapping and its id-required rejection, the
  outbound-link fetch firing only when `archive_outbound_link` and
  `store_media` are both on, and `ext_for_mime`'s fallback.

## `udemy` connector (closes #95)
**2026-08-13**

- **New `dbs-connector-udemy` crate** — backs up enrolled courses and
  their full curricula via Udemy's own web-app REST surface
  (`/api-2.0`; there's no official public learner API). Auth is the
  `access_token` cookie from a logged-in browser
  (`UDEMY_ACCESS_TOKEN`), sent both as a Bearer header and a cookie to
  match the web client, with a desktop Chrome User-Agent since
  Cloudflare fronts the API and blocks obviously non-browser clients.
  Mirrors `dbs.connectors.udemy`: two item layers — `course` (one per
  enrollment) and `lecture`/`quiz` (one per curriculum entry, walked
  per course via `subscriber-curriculum-items`, quizzes sharing the
  `lecture:` identity prefix exactly as the reference does). Article
  lectures keep their full HTML in `body`; downloadable supplementary
  assets become `file`-kind `MediaRef`s. A full-enumeration source
  like `vimeo` (#94): every run walks everything and yields one
  `ReconcileMarker` — but if any single course's curriculum fails to
  load, the run continues (logged, not fatal) while the marker is
  withheld entirely for that run, since a walk missing one course's
  lectures would otherwise falsely sweep them.
  `completion_ratio`/`last_accessed_time` are volatile so watch-
  progress ticks never spawn revisions.
- **Second connector to use `dbs-connector-support`** (after `vimeo`,
  #94) — `download_videos = true` pulls each video lecture via the
  `yt-dlp` binary (needs `UDEMY_COOKIES_FILE`, a full Netscape
  cookies.txt export, since yt-dlp needs the whole cookie jar, not
  just the one token), guarded by `run_with_watchdog` the same way.
  Downloads are idempotent (existing file wins) and best-effort — a
  failed or DRM-protected lecture is logged and the run moves on.
- **Not wired up:** same boundary as `raindrop` (#85) through `vimeo`
  (#94) — this struct isn't reachable from a real `dbs backup` run
  yet; the plugin registry's run/stream bridge doesn't exist.
- 16 new `dbs-connector-udemy` tests: a full fetch yielding course,
  lecture, and quiz items plus a reconcile marker, `course_filter`
  matching by id or slug, a failed curriculum withholding the marker
  while keeping the other courses' items, pagination across `next`
  links, an article lecture's HTML body, supplementary assets becoming
  file `MediaRef`s, `download_videos` off by default having no video
  media, a successful `yt-dlp` download via a fake script on disk, a
  missing/invalid cookies file being skipped without failing the run,
  an existing download being reused without invoking `yt-dlp` at all,
  both HTTP status classifications (401/403 vs. other), and a direct
  unit test of `safe_name`.

## `vimeo` connector (closes #94)
**2026-08-13**

- **New `dbs-connector-vimeo` crate** — backs up the catalog of videos
  you own via Vimeo's REST API v3.4 (`GET /me/videos`), personal
  access token auth (`VIMEO_TOKEN`). Mirrors `dbs.connectors.vimeo`:
  a full-enumeration source like `podcast`/`pocketcasts` —
  `supports_incremental = false`, every run re-reads `/me/videos` and
  yields one `ReconcileMarker` so removed videos get soft-deleted.
  `stats`/`metadata` (play counts, hypermedia links with short-lived
  tokens) are declared volatile so an unchanged video never spawns a
  spurious revision. By default only catalog metadata is stored (a
  thumbnail + the watch-link `MediaRef`); `download_videos = true`
  additionally pulls each video file via the `yt-dlp` *binary* into
  the source's download folder, downloaded video winning over the
  watch link once present.
- **First connector to use `dbs-connector-support`** — its
  `run_with_watchdog` guards the `yt-dlp` subprocess against hanging
  indefinitely, with a heartbeat fed by the subprocess's own stdout
  lines (`--newline` forces periodic progress output) rather than a
  per-event progress-hook callback, since a shelled-out CLI has no
  such hook. Per this port's round-1 decision (documented on
  `dbs-connector-support` itself) `yt-dlp` is a subprocess, not a
  library call, so there's no `impersonate_target()` capability probe
  to port — `--impersonate chrome` (Vimeo blocks yt-dlp's default TLS
  fingerprint on data-center/VPN IPs) is passed unconditionally, and a
  missing `curl_cffi` backend just surfaces as an ordinary logged,
  non-fatal download failure.
- **Not wired up:** same boundary as `raindrop` (#85) through
  `podcast` (#93) — this struct isn't reachable from a real
  `dbs backup` run yet; the plugin registry's run/stream bridge
  doesn't exist.
- 14 new `dbs-connector-vimeo` tests: a full fetch yielding videos and
  a reconcile marker, pagination following `paging.next`, a video with
  a non-numeric uri being skipped, `download_videos` off by default
  keeping the watch link, `download_videos` on downloading via a fake
  `yt-dlp` script on disk (the same fake-executable-on-disk pattern
  `dbs-research`'s YouTube search already uses) and preferring the
  local path, a missing download folder being a config error, an
  existing non-empty download being reused without invoking `yt-dlp`
  at all, both HTTP status classifications (401/403 vs. other), and
  direct unit tests of `video_id` and `safe_suffix`.

## `podcast` connector (closes #93)
**2026-08-13**

- **New `dbs-connector-podcast` crate** — backs up episodes from
  RSS/Atom feeds you list, no account or token needed. The first new
  XML-parsing dependency in the workspace (`roxmltree`, a read-only
  DOM parser — no other connector needed one). Mirrors
  `dbs.connectors.podcast`: the source of truth is a plain list of
  feed URLs from `feeds` and/or an OPML subscription export
  (`opml_path`), merged and deduplicated. Both RSS 2.0 (with the
  iTunes namespace) and Atom are supported via one pure `parse_feed`
  function, directly unit-tested. `download_audio = true` downloads
  each episode's enclosure into the source's download folder
  (idempotent — an existing non-empty file wins — and best-effort — a
  dead enclosure never fails the run); otherwise the enclosure is
  referenced by URL only via a `MediaRef`. One broken feed of many is
  logged and skipped so healthy feeds still make progress; only when
  *every* feed fails does the run fail.
- **Deletion detection is deliberately disabled** — a podcast feed is
  a rolling window over the newest N episodes, so an episode leaving
  the feed is ordinary aging, not a deletion. Hence
  `supports_full_enumeration = false` and no `ReconcileMarker`, ever;
  what this connector has stored, it keeps.
- **Not wired up:** same boundary as `raindrop` (#85) through
  `pocketcasts` (#92) — this struct isn't reachable from a real
  `dbs backup` run yet; the plugin registry's run/stream bridge
  doesn't exist.
- 12 new `dbs-connector-podcast` tests against a `mockito` fixture
  server plus direct unit tests of `parse_feed`: missing-managed-
  http/no-feeds-configured errors, an RSS feed and an Atom feed each
  parsed into episodes, OPML feeds merged and deduplicated against
  configured `feeds`, one broken feed of many being skipped while
  healthy feeds still yield items, every feed failing being a
  transient error, `download_audio` downloading an enclosure into the
  download dir, `max_episodes_per_feed` capping the count, a
  non-RSS/Atom root being rejected, and an episode with no
  guid/link/enclosure being skipped.

## `pocketcasts` connector (closes #92)
**2026-08-13**

- **New `dbs-connector-pocketcasts` crate** — backs up podcast
  subscriptions, starred episodes, and listening history via Pocket
  Casts' unofficial web-player API (no official public API exists;
  this speaks the same reverse-engineered endpoints the community
  python/nodejs `pocketcasts` libraries use). Mirrors
  `dbs.connectors.pocketcasts`: `POST /user/login` with
  `POCKETCASTS_EMAIL`/`POCKETCASTS_PASSWORD` and `scope: "webplayer"`
  returns a bearer token, then three POST endpoints
  (`/user/podcast/list`, `/user/starred`, `/user/history`) each list
  one kind, each its own small method so a shift in one endpoint's
  shape stays a one-method fix. The API has no trustworthy
  since-filter, so every run is a full walk of all three
  (`supports_incremental=false`); a complete walk yields one
  `ReconcileMarker`, so unsubscribed podcasts, unstarred episodes,
  *and* history entries that scroll off Pocket Casts' server-side
  history window all get soft-deleted — accepted, not a bug, since a
  soft delete keeps the row and every revision. `playedUpTo`/
  `playingStatus` are declared volatile so a listening-position
  micro-update never spawns a revision by itself. A deliberately-
  partial enumeration (any kind disabled) withholds the marker
  entirely.
- **Not wired up:** same boundary as `raindrop` (#85), `github` (#86),
  `pinboard` (#87), `readwise` (#88), `mastodon` (#89), `bluesky`
  (#90), and `spotify` (#91) — this struct isn't reachable from a real
  `dbs backup` run yet; the plugin registry's run/stream bridge
  doesn't exist.
- 10 new `dbs-connector-pocketcasts` tests against a `mockito` fixture
  server: missing-managed-http/missing-credentials errors, a rejected
  login classified as an auth error, a full fetch yielding all three
  item kinds and a reconcile marker, a disabled kind withholding the
  marker, a podcast with no uuid being skipped, an episode url falling
  back to the podcast page without a `shareUrl`, both HTTP status
  classifications (401/403 vs. other) on the API, and connector
  metadata matching the reference.

## `spotify` connector (closes #91)
**2026-08-13**

- **New `dbs-connector-spotify` crate** — backs up liked songs and
  playlist catalog metadata via the Web API. The one genuinely
  OAuth-shaped auth flow in the connector set: access tokens live ~1
  hour, so the durable secret is a refresh token
  (`SPOTIFY_CLIENT_ID`/`SPOTIFY_CLIENT_SECRET`/
  `SPOTIFY_REFRESH_TOKEN`), exchanged for a fresh access token via
  `POST /api/token` (HTTP Basic client auth, `reqwest`'s
  `basic_auth()` — no new `base64` dependency needed) at the start of
  every run. Mirrors `dbs.connectors.spotify`: `/v1/me/tracks` returns
  liked songs newest-first with an `added_at` per entry, so
  incremental mode early-stops below the stored watermark (with
  overlap) — `github`'s stars fast path (#86). Playlists are a small
  catalog listed fully every run via offset pagination. `track` /
  playlist `snapshot_id`/`images`/`tracks` are declared volatile
  (popularity scores, rotating CDN image URLs, count wrappers churn
  constantly) while the semantic projection (title/url/body) still
  hashes meaningful changes. A full/reconcile run yields one combined
  `ReconcileMarker` across both kinds, withheld if either is disabled.
- **Not wired up:** same boundary as `raindrop` (#85), `github` (#86),
  `pinboard` (#87), `readwise` (#88), `mastodon` (#89), and `bluesky`
  (#90) — this struct isn't reachable from a real `dbs backup` run
  yet; the plugin registry's run/stream bridge doesn't exist.
- 11 new `dbs-connector-spotify` tests against a `mockito` fixture
  server: missing-managed-http/missing-secrets errors, a rejected
  token refresh classified as an auth error, a full fetch yielding
  both item kinds and a combined reconcile marker, a reconcile run
  with one kind disabled withholding the marker, incremental tracks
  early-stopping past the watermark, a track with no catalog id
  (local file) being skipped, a playlist with no id being skipped,
  both HTTP status classifications (401 vs. other) on the API, and
  connector metadata matching the reference.

## `bluesky` connector (closes #90)
**2026-08-13**

- **New `dbs-connector-bluesky` crate** — backs up liked posts via the
  AT Protocol, app-password auth (`BLUESKY_APP_PASSWORD`, Settings →
  App Passwords — never the account password). Mirrors
  `dbs.connectors.bluesky`: likes are records in your own repo
  (collection `app.bsky.feed.like`), enumerable via
  `com.atproto.repo.listRecords` with plain cursor pagination — no
  scraping. Each run exchanges the app password for a session token
  via `com.atproto.server.createSession`; the resolved DID from that
  session (not the configured handle) is what gets enumerated, so a
  handle change never breaks the source. Records are tiny with no
  usable delta filter, so every run is a full enumeration
  (`supports_incremental=false`) followed by one `ReconcileMarker`.
  Identity is the record's `at://` URI (immutable); like records never
  mutate, so no `volatile_fields`. The subject post's web URL is
  derived (`https://bsky.app/profile/<did>/post/<rkey>`) from the
  record's `at://` subject reference.
- **Not wired up:** same boundary as `raindrop` (#85), `github` (#86),
  `pinboard` (#87), `readwise` (#88), and `mastodon` (#89) — this
  struct isn't reachable from a real `dbs backup` run yet; the plugin
  registry's run/stream bridge doesn't exist.
- 12 new `dbs-connector-bluesky` tests against a `mockito` fixture
  server plus direct unit tests of `derive_bsky_url`: missing-managed-
  http/missing-password errors, a rejected session classified as an
  auth error, a full fetch yielding a like and a reconcile marker,
  cursor pagination across pages, a record with no uri being skipped,
  a non-post subject producing no derived url, both HTTP status
  classifications (401/403 vs. other) on `listRecords`, and connector
  metadata matching the reference.

## `mastodon` connector (closes #89)
**2026-08-13**

- **New `dbs-connector-mastodon` crate** — backs up bookmarked and
  favourited posts via the Mastodon v1 API, token auth
  (`MASTODON_TOKEN`, `read:bookmarks read:favourites` scopes).
  Mirrors `dbs.connectors.mastodon`: bookmark/favourite listings
  paginate by internal marker ids exposed only through the `Link`
  response header (not status ids), with no usable `since` filter, so
  every run is a full enumeration (`supports_incremental=false`)
  followed by one combined `ReconcileMarker` — withheld if either kind
  is disabled, same rule as `github` (#86) and `readwise` (#88).
  Pagination follows the `Link` header's `rel="next"` URL verbatim
  (parsed by a small pure `next_link_from_header` helper, directly
  unit-tested) rather than reconstructing it. Engagement counters and
  the nested `account` object are declared volatile — both churn
  constantly without the saved content changing; the author handle is
  captured into `title` at map time so a real display-name/handle
  change still hashes. Config carries the `instance` base URL
  (multi-instance accounts = one source each), validated at fetch time
  to be an `http(s)://` URL.
- **Not wired up:** same boundary as `raindrop` (#85), `github` (#86),
  `pinboard` (#87), and `readwise` (#88) — this struct isn't reachable
  from a real `dbs backup` run yet; the plugin registry's run/stream
  bridge doesn't exist.
- 13 new `dbs-connector-mastodon` tests against a `mockito` fixture
  server plus direct unit tests of `next_link_from_header`: missing-
  managed-http/missing-token/invalid-instance errors, a full fetch
  yielding both item kinds and a combined reconcile marker, a run with
  one kind disabled withholding the marker, pagination following the
  `Link` header across pages, a status with no id being skipped, a
  status with no account having no title, both HTTP status
  classifications (401 vs. other), and connector metadata matching the
  reference.

## `readwise` connector (closes #88)
**2026-08-13**

- **New `dbs-connector-readwise` crate** — backs up books/articles and
  highlights via Readwise's v2 API, token auth (`READWISE_TOKEN`, sent
  as `Authorization: Token <token>`). Mirrors
  `dbs.connectors.readwise`: the cleanest delta of the connector set
  so far — both `/books/` and `/highlights/` accept a real
  `updated__gt=<ISO>` server-side filter, so incremental mode queries
  a per-kind watermark directly (minus an overlap the idempotent
  upsert dedups), unlike `github`'s stars (#86), which have no
  server-side filter and rely on a client-side early-stop. Pagination
  follows the server's own `next` URL from the standard
  `{"count", "next", "results"}` shape rather than reconstructing page
  numbers. A full/reconcile run enumerates both kinds and yields one
  combined `ReconcileMarker`, withheld entirely if either kind is
  disabled — same partial-enumeration-never-sweeps rule as `github`.
- **Not wired up:** same boundary as `raindrop` (#85), `github` (#86),
  and `pinboard` (#87) — this struct isn't reachable from a real
  `dbs backup` run yet; the plugin registry's run/stream bridge
  doesn't exist.
- 12 new `dbs-connector-readwise` tests against a `mockito` fixture
  server: missing-managed-http/missing-token errors, a full fetch
  yielding both item kinds and a combined reconcile marker, a
  reconcile run with one kind disabled withholding the marker, an
  incremental run sending `updated__gt` with the overlap applied,
  pagination following the server's `next` URL across pages, a
  title/url fallback when a book has no title, a highlight title
  truncating to 120 chars, a record with no id being skipped, both
  HTTP status classifications (401 vs. other), and connector metadata
  matching the reference.

## `pinboard` connector (closes #87)
**2026-08-13**

- **New `dbs-connector-pinboard` crate** — backs up bookmarks via
  Pinboard's API, token auth (`PINBOARD_TOKEN`, the `username:HEXTOKEN`
  value from Settings → Password). Mirrors `dbs.connectors.pinboard`'s
  cheapest-possible incremental strategy: Pinboard exposes a global
  change signal, `posts/update`, returning the account's last-modified
  timestamp. If it hasn't moved since the stored cursor, the run ends
  after that one request — no listing, no hashing. When it has moved,
  `posts/all?fromdt=<watermark minus overlap>` returns only the
  added/updated posts (the idempotent upsert dedups the overlap).
  Identity is Pinboard's own `hash` (an md5 of the URL, stable across
  title/tag edits), so there are no volatile fields. Deletion detection
  needs the full listing, so full/reconcile runs page `posts/all`
  (already a single response — Pinboard doesn't paginate it) and yield
  one `ReconcileMarker`.
- **Extracted `posts_all_params` as a pure function** — whether a run
  sends `fromdt` (and its value) is the one bit of real request-shaping
  logic in this connector; pulling it out of `fetch()` makes "a full
  run never sends `fromdt`, even with a stale watermark" a plain
  `assert_eq!` against the function's return value instead of an HTTP
  mock query-string match — `mockito`'s matcher uses the `regex` crate
  under the hood, which can't express the negative assertion that
  behavior needs.
- **Not wired up:** same boundary as `raindrop` (#85) and `github`
  (#86) — this struct isn't reachable from a real `dbs backup` run yet;
  the plugin registry's run/stream bridge doesn't exist.
- 12 new `dbs-connector-pinboard` tests against a `mockito` fixture
  server plus direct unit tests of `posts_all_params`: missing-managed-
  http/missing-token errors, an unchanged watermark short-circuiting to
  zero requests and zero events, an incremental run sending the correct
  `fromdt`, a full run ignoring a stale cursor and yielding a reconcile
  marker (with title falling back to `href` when `description` is
  empty), a hash-less post being skipped, both HTTP status
  classifications (401 vs. other), and connector metadata matching the
  reference.

## `github` connector (closes #86)
**2026-08-13**

- **New `dbs-connector-github` crate** — backs up starred repositories
  and gists via GitHub's REST API v3, token auth (`GITHUB_TOKEN`).
  Mirrors `dbs.connectors.github`: stars have no server-side `since`
  filter, so incremental mode pages `sort=created&direction=desc`
  (with the `application/vnd.github.star+json` media type, which adds
  `starred_at`) and early-stops past a stored watermark minus an
  overlap — `raindrop`'s exact fast path. Gists DO have a real delta
  filter (`GET /gists?since=ISO` against `updated_at`), so their
  incremental mode is a genuine server-side query. A full/reconcile run
  yields one combined `ReconcileMarker` across both kinds — withheld
  entirely if either kind is disabled in config, so a deliberately
  partial enumeration never offers the skipped kind's stored items up
  for deletion sweeping.
- **Extends `HttpError::Status` with the response's headers** —
  `raindrop`'s connector only ever needed the status code to
  reclassify a non-retryable response, but GitHub's 403 means two
  different things (rate-limit-exhausted vs. token-lacks-access) told
  apart only by the `X-RateLimit-Remaining` response header, which
  `ManagedHttpClient` was discarding before converting a bad status
  into an error. `Status` is now `{ error, headers }`; `raindrop`'s
  connector updated to match the new shape (no behavior change there).
- **Not wired up:** same boundary as `raindrop` (#85) — this struct
  isn't reachable from a real `dbs backup` run yet; the plugin
  registry's run/stream bridge doesn't exist.
- 10 new `dbs-connector-github` tests against a `mockito` fixture
  server: missing-managed-http/missing-token errors, a full fetch
  yielding both item kinds and a combined reconcile marker, a
  reconcile run with one kind disabled withholding the marker,
  incremental stars early-stopping past the watermark, incremental
  gists actually sending the stored watermark as `?since=`, all three
  HTTP status classifications (401, 403+rate-limit-exhausted,
  403+no-header), and connector metadata matching the reference.

## `raindrop` connector (closes #85)
**2026-08-13**

- **New `dbs-connector-raindrop` crate** — the first connector this
  port implements `dbs_core::Connector` for. Mirrors
  `dbs.connectors.raindrop` faithfully: token-based REST auth (bearer,
  `RAINDROP_TOKEN`), the three engine-selected modes (incremental —
  pages `-created`-sorted and early-stops past a stored
  `created_high_watermark` cursor minus a small overlap, optionally
  polling the Trash collection `-99` for same-day deletions; reconcile
  — full page walk + a `ReconcileMarker` of every live id; full — same
  as reconcile but ignores the existing cursor), and the exact
  `to_item` field mapping (title/url/body/tags/media/deleted).
- **Extends `RunContext` with `http`** (`Option<RefCell<ManagedHttpClient>>`)
  — issue #22's `ManagedHttpClient` existed but wasn't wired into
  `RunContext` yet; this is the first connector that actually needs
  it. `RefCell` because the client's retry/rate-limit bookkeeping needs
  `&mut self` per request while `fetch` only ever holds `&RunContext`.
  `RunContext` drops its `Debug`/`Clone` derives as a result (neither
  was ever actually used outside this module's own tests — `Managed
  HttpClient` holds a boxed closure that can't derive either).
- A connector's `fetch()` reclassifies a non-retryable HTTP status per
  its own domain knowledge (documented on `HttpError` itself): 401/403
  become `ConnectorError::Auth`, everything else non-retryable becomes
  `Transient`.
- **Not ported:** `archive_permanent_copy` — an opt-in, Pro-tier-only
  feature that opportunistically downloads Raindrop's cached snapshot
  of a bookmark via a redirect-following, deliberately unauthenticated
  second request. Off by default in the reference too, and orthogonal
  to what this issue asks for (token auth + delta/cursor fetch).
- **Not wired up:** `RaindropConnector` isn't reachable from a real
  `dbs backup` run yet. Per ADR-0001 a real connector is its own
  subprocess binary discovered through the plugin registry's handshake
  protocol (`dbs-core::registry`, already built) — but the *run/stream*
  half (writing a `RunContext`, reading a `FetchEvent` stream back over
  the wire) is a separate, not-yet-built bridge, same gap
  `registry.rs`'s own scope note already documented. This issue's
  acceptance criteria are scoped to the connector's own fetch/delta
  logic, tested directly against the `Connector` trait and fixture
  HTTP responses.
- 8 new `dbs-connector-raindrop` tests against a `mockito` fixture
  server: missing-managed-http and missing-token config/auth errors, a
  full fetch paging two items and yielding a reconcile marker, an
  incremental fetch early-stopping past the watermark, a trashed item
  yielded with `deleted: true`, `include_types` filtering, a 401
  response classified as an auth error, and connector metadata
  (type/secret_keys/capabilities) matching the reference.

## Research pipeline: YouTube search → NotebookLM synthesis → report (closes #84)
**2026-08-13**

- **New `dbs-research` crate** — kept separate from `dbs-core` (same
  reasoning as the reference: this pipeline has nothing to do with the
  `Connector`/`Storage`/engine machinery, it's a one-shot ad-hoc
  command, not a backup source).
- **Fully real:** `youtube_search` — shells out to the `yt-dlp` binary
  (`--dump-json` against a `ytsearchN:"query"` pseudo-URL, one JSON
  object per line) rather than the reference's in-process
  `yt_dlp.YoutubeDL` (Decision 3), with the same dedup-by-id,
  recency-filter, and engagement-ranking logic ported directly.
  `report::render_report` is a direct, fully real port (pure Markdown
  formatting). `pipeline::run_pipeline`/`run_pipeline_for_videos`
  orchestrate both, plus the NotebookLM indexing/Q&A loop, with the
  same per-video-failure-vs-fatal-auth-failure distinction as the
  reference.
- **The NotebookLM half sits behind a `NotebookLmClient` trait** —
  `create_notebook`/`add_source`/`ask`/`generate_infographic`, mirroring
  the reference's swappable `client_module` test seam exactly (which is
  how the pipeline's tests run against zero real network/auth: a
  scripted fake client). The concrete adapter that actually shells out
  to `nlm`/`notebooklm-mcp` (gap-analysis.md's Decision 4 — a
  *different* tool than the reference's in-process `notebooklm-py`,
  chosen specifically because Rust can't import a Python library)
  isn't implemented yet: writing it correctly needs that external
  tool's actual CLI/MCP surface confirmed against a real install,
  which this port can't verify in this environment. `UnimplementedClient`
  is the documented stand-in (same shape as
  `dbs_core::service::UnimplementedRunner`) until that's done.
- Async note: the reference's `notebooklm-py` client is async-only,
  making `pipeline.py` the repo's first `asyncio` use; this port's
  `NotebookLmClient` trait is plain synchronous method calls instead
  (a subprocess/MCP call is no more "async" in Rust than any other
  blocking I/O), so there's no async boundary to bridge here at all.
- **Risk called out in the issue, addressed:** the auth-vs-per-video-
  failure distinction is exercised by tests, not just the happy path —
  an `Auth` error from `add_source` aborts the whole run as a distinct
  `ResearchError::Auth` (not tracked as a per-video failure), and
  separately from `create_notebook`, against a scripted fake client.
- **Not yet wired up:** `dbs-cli`'s `dbs research` subcommands (#77)
  still report their own "not yet implemented" — wiring them to call
  into this crate is a natural follow-up, though it wouldn't change
  user-visible behavior yet (`UnimplementedClient` means every real
  run still fails at the NotebookLM step).
- 23 new `dbs-research` tests across all 5 modules: engagement/outcome
  math, yt-dlp NDJSON parsing + dedup across queries (via a fake
  `yt-dlp` script), recency filtering (drops old, keeps
  missing/unparseable dates), ranking/truncation, report rendering
  (default vs. custom question section titles, empty-video-list),
  auth-state resolution, `UnimplementedClient`'s uniform failure, and
  the pipeline's full lifecycle against a scripted fake NotebookLM
  client (success, per-video failure, all-videos-fail abort, auth
  failure from two different call sites, infographic opt-in).

## In-UI setup: dependency install + browser-auth capture jobs (closes #83)
**2026-08-13**

- **New `Handshake` fields:** `pip_requirements: Vec<String>` and
  `needs_playwright_browser: bool` (mirroring how `auth_capture` was
  added for #76) — what a connector declares it needs installed.
- **Implemented, fully real:** `dbs-web::setup`'s dependency-install
  path — `install_commands`/`playwright_install_commands` derive
  `pip install`/`playwright install chromium` steps (via whichever of
  `python3`/`python` is on `PATH`), and `run_commands` actually
  executes them, streaming each line. `run_install_job` wires that
  through the #80 job manager as a real, trackable background job.
- **Reuses `dbs-web::jobs::JobManager`** rather than porting the
  reference's second, structurally-identical `SetupManager` — a setup
  job's log line is just `{"line": ...}` on the same generic
  `Value`-typed event stream the backup job manager already has.
- **Pure helpers ported directly** (no gap): `validate_netscape_cookies`,
  `validate_storage_state`, `to_netscape_cookies`,
  `extract_session_zip` (zip-slip guarded, using the `zip` crate
  already in the workspace).
- **Deliberately scoped:** `run_capture_job` fails cleanly rather than
  driving a real browser — same gap `dbs capture` (#76) already
  documented: real browser automation needs the shared Playwright
  launch helper subprocess issue #99 hasn't built yet. No `/api`
  routes wire any of this into `dbs serve` yet either — there's no
  filed issue for the general `/api/status`/`/api/sources`/etc. read
  surface these actions would sit alongside in the real UI.
- 18 new `dbs-web` tests: `run_commands`' three outcomes (success,
  first-failure-stops-the-rest, launch failure), `install_commands`'
  derivation, both job kinds run end-to-end through a real
  `JobManager`, and the pure validation/formatting/zip-extraction
  helpers (including a zip-slip rejection test).

## envfile.py (scoped secrets writer) (closes #82)
**2026-08-13**

- **Implemented:** `dbs-web::envfile` — a minimal, dependency-free
  `.env` writer mirroring the reference's `envfile.py`: `set_var`
  upserts a single `KEY="value"` line while preserving the rest of the
  file (comments, ordering, unrelated keys, collapsing any duplicate
  existing assignment of the same key to one), `unset_var` removes
  every assignment of a key, and `read_keys` returns the set of keys
  currently assigned a non-empty value (reusing
  `dbs_core::parse_env_file` rather than re-parsing the format a
  second time — the one place `dbs-web` depends on `dbs-core` so far).
  `validate` rejects an invalid env var name or a value containing a
  newline/CR/double-quote, since every written value is double-quoted
  and an embedded one could inject extra lines/keys. A newly-created
  file is `chmod 0600` on Unix (best-effort, matching the reference).
- **Deliberately scoped:** the in-UI setup routes that will actually
  call this from the web UI (issue #83) don't exist yet — this issue
  is just the writer itself, same as #80's job manager landing ahead
  of the routes that use it.
- 14 new `dbs-web` tests: create/update/preserve-unrelated-lines,
  duplicate-assignment collapse, the `export` prefix, invalid key/value
  rejection, the Unix chmod, `unset_var`'s three outcomes (removed, no
  such file, key never present), and `read_keys` (non-empty-only,
  missing file).

## Auth / CSRF / Origin / Host protection (closes #81)
**2026-08-13**

- **New dependency:** `url` (narrowly-scoped, standard Rust URL
  parser — already in the workspace's dependency graph transitively via
  `reqwest`; added directly here for robust `Origin` header parsing on
  a security-sensitive path rather than hand-rolling it).
- **Implemented:** `dbs-web::auth::security_gate` — the reference's
  `_security_gate` middleware, mirrored check-for-check: (1) the `Host`
  header must name this machine (`localhost`/`127.0.0.1`/`::1`) unless
  a `--token` is configured — the DNS-rebinding defense; (2) a
  state-changing request (non-`GET`/`HEAD`/`OPTIONS`) carrying a
  cross-origin `Origin` header is rejected unless it's already
  token-authenticated — the CSRF defense (Origin-based, matching the
  reference exactly — there's no server-side session to bind an issued
  CSRF token to, since the bearer token already serves that role); (3)
  once `--token` is configured, every `/api` request needs it (bearer
  header or `?token=` query param, for `EventSource`/download links
  that can't set headers), timing-safe compared. The static SPA itself
  stays reachable without a token so it can load and prompt for one.
- Wired into `router()`/`serve()` (both now take the configured
  token) and, in `dbs-cli`, `cmd_serve` — which can now actually bind
  off-loopback for real once `--token` is given, closing the gap #79's
  `cmd_serve` left open ("validated but not yet served for real").
- 16 new `dbs-web` tests (Host/Origin parsing edge cases — ports,
  bracketed IPv6, scheme; each of the three checks' accept/reject
  paths over real HTTP; constant-time comparison; the gate is actually
  wired into `router()`) plus a `dbs-cli` `serve` integration test
  rewrite for the now-real off-loopback+token path, verifying the
  static frontend stays reachable while an unauthenticated `/api`
  request is rejected.

## Job manager (background jobs + SSE progress) (closes #80)
**2026-08-13**

- **New dependencies:** `futures-util`, `tokio-stream` (both narrowly-
  scoped async-ecosystem utility crates; `tokio-stream` for
  `UnboundedReceiverStream`, `futures-util` for `Stream`/`StreamExt`).
- **Implemented:** `dbs-web::jobs` — a domain-agnostic background-job
  manager mirroring `dbs.web.jobs.JobManager`'s shape: `start` runs a
  caller-supplied closure on a blocking thread (one job at a time;
  a second `start` while one's running is refused), tracks
  running/done/error transitions, buffers up to 1000 progress events
  per job for replay, evicts finished jobs beyond the newest 20, and
  supports cooperative cancellation via a `CancelToken`. `GET /:id`
  (JSON snapshot) and `GET /:id/stream` (its progress as Server-Sent
  Events, buffered events replayed first then live, ending when the
  job finishes) are exposed as a reusable `sse_router`.
- **Deliberately scoped:** not wired into `dbs serve` yet — a real
  `/api/backup` route needs the auth gate (#81) first, since starting a
  backup is a mutating action and this skeleton has no auth. This
  issue's own acceptance criteria anticipate that: its tests exercise
  the manager, including the SSE endpoint end-to-end over HTTP, with a
  *fake* job.
- **Not ported:** the reference's VPN-subprocess routing
  (`_run_vpn_source`/`_run_all_mixed`/`_finish_vpn_source` —
  `requires_vpn` sources re-executing themselves as
  `<vpn_exec> dbs backup <name>` subprocesses). `BackupService` already
  made a different, earlier, deliberate choice: it refuses a
  `requires_vpn` source run outside the right network namespace instead
  of relaunching itself through a wrapper. A second, contradictory
  subprocess-relaunch path that only the web tier had wasn't this
  issue's call to make.
- 16 new `dbs-web` tests: lifecycle transitions (running→done,
  running→error with its message, cancellation, `JobAlreadyRunning`,
  history eviction), plus HTTP-level tests of both routes against a
  fake job (snapshot JSON, SSE events streamed and the connection
  closing when the job finishes, both 404 on an unknown job id).

## Web app skeleton + static SPA serving (closes #79)
**2026-08-13**

- **New dependencies:** `axum` (0.7) + `tokio` (async runtime), per
  gap-analysis.md's Decisions section item 1/the issue's own framing —
  the workspace's first async code. New `dbs-web` crate.
- **Implemented:** an Axum router serving the reference's actual static
  SPA — `crates/dbs-web/static/{index.html,app.js,style.css}` are an
  unmodified copy of `dbs/web/static/*`. `GET /` renders `index.html`
  with its `{{v}}` cache-bust placeholder substituted (process-start
  timestamp, since these assets are compiled into the binary rather
  than read from disk — no mtime to read); `GET /static/<name>` serves
  `app.js`/`style.css` with the right content type. `dbs serve` now
  actually binds and runs this for a loopback host (default, or
  `--host localhost`), blocking until interrupted, instead of just
  validating flags and exiting.
- **Sync/async boundary, decided here (doc-commented in
  `dbs-web/src/lib.rs`):** `dbs-cli` stays synchronous everywhere except
  `cmd_serve`, which builds a dedicated Tokio runtime to drive the
  server. Nothing here calls into `dbs-core` yet (no `/api` routes
  exist until #80/#81/#83 land), but the decision for when that need
  arrives is made now: async handlers will cross into `dbs-core`'s
  synchronous `Storage` API via `tokio::task::spawn_blocking` at the
  call site, not by growing `dbs-core` an async-facing wrapper.
- **Deliberately scoped:** a non-loopback bind (`--host 0.0.0.0`, etc.)
  still validates `--token` as before but doesn't actually serve for
  real yet — there's no auth gate wired into the app skeleton (#81),
  and starting an unauthenticated listener on a non-loopback interface
  the moment `--token` is merely *present* would be the exact hole the
  original validation exists to close. It reports as much and exits 4;
  this restriction comes out once #81 lands.
- The SPA's `app.js` calls a `/api/*` surface that mostly doesn't exist
  in this port yet — every route it drives is a later web-tier issue
  (#80 job manager, #81 auth, #83 in-UI setup, plus the export/research
  API routes those depend on). Shipping the real frontend now,
  unmodified, means each of those issues lands against a frontend
  that's already real instead of needing its own follow-up port.
- 6 new `dbs-web` unit/integration tests (index renders with the
  placeholder substituted, `/static/app.js` and `/static/style.css`
  serve with the right content type, unknown static asset and unknown
  route both 404, a real ephemeral-port bind answers a real HTTP
  request) plus a rewrite of the `dbs-cli` `serve` integration tests:
  loopback binds now spawn the real binary, poll the port, and check a
  real HTTP response (with a kill-on-drop guard so a failing assertion
  never leaks the child process); the non-loopback and usage-error
  cases stay quick-exit.

## dbs version (closes #78)
**2026-08-13**

- **Implemented:** `dbs version`, matching the reference's output shape
  (`<tool> <version> (core API v<N>)`). Uses `rusty_dbs` as the
  self-identifying tool name — the same name every export manifest
  already writes to its `tool` field
  (`BackupService::export_manifest_row`) — and this crate's own
  `Cargo.toml` version rather than the reference's `dbs.__version__`
  (there's no equivalent Rust-side single-sourced-version mechanism to
  port).
- 1 new `dbs-cli` integration test asserting the exact output string.

## dbs research subcommands (closes #77)
**2026-08-13**

- **Implemented:** `dbs research youtube TOPIC` and `dbs research
  youtube-backup TOPIC` flag surfaces, matching the reference's
  option-for-option (`--query/-q`, `--per-query-count`, `--count`,
  `--months`, `--question`, `--infographic[-orientation]`, `--out/-o`,
  `--notebook-name`, `--auth-state` for `youtube`; `--source/-s`,
  `--list/-l`, plus the shared options for `youtube-backup`), and the
  reference's default output path (`./<slug>.md`, same slugify rules:
  lowercase, non-alphanumeric runs collapse to `-`, trimmed, falls back
  to `"research"`).
- **`youtube-backup`'s video selection is real, not a stub:** added
  `BackupService::select_youtube_backup_videos`, mirroring the
  reference's `research/from_backup.py::videos_from_rows` — queries
  already-backed-up items (kind `video`), keeps only rows from
  `youtube`-type sources with a parseable id, applies the `--list`
  filter against `raw.list_label`, dedups the same video saved under
  multiple lists, and truncates to `--count`. The CLI surfaces the
  reference's own "No backed-up YouTube videos matched (...)" error,
  including its source/list scope text, when nothing matches.
- **Deliberately scoped:** the actual NotebookLM synthesis — a live
  YouTube search (`youtube`) or feeding selected videos into a
  NotebookLM notebook (both commands) — depends on a research
  subsystem that doesn't exist in this port yet (gap-analysis.md's
  Research subsystem row isn't its own filed issue yet) and the
  NotebookLM integration strategy (gap-analysis.md's Decisions section
  item 4: shell out to `nlm`/`notebooklm-mcp`). So once flags parse
  (and, for `youtube-backup`, videos are selected), both commands
  report what they *would* do instead of pretending to run a real
  pipeline, and exit `4` — the same pattern as `capture`/`serve`.
- Removed the now-obsolete "nested stub subcommand" test in `cli.rs` —
  `sources`/`connectors` (#71) and now `research` (#77) are no longer
  pure stubs, so there's no remaining nested-subcommand enum left to
  cover with that test; `verify`'s bare-stub test still covers
  `cmd_stub` itself.
- 6 new `dbs-core` unit tests for `select_youtube_backup_videos`
  (youtube-type-only filtering, cross-list dedup, list-label filter,
  limit truncation, source-name restriction, no-match) plus 7 new
  `dbs-cli` integration tests (both commands' pipeline-stub reporting
  and default/custom out path, `youtube-backup`'s no-match error and
  its source/list scope text, missing-topic usage errors).

## dbs capture (closes #76)
**2026-08-13**

- **Implemented:** `dbs capture TARGET [--out/-o PATH]` target
  resolution, matching the reference's `capture` command: resolve
  `TARGET` as a connector type directly, falling back to a configured
  source name whose connector type is then looked up; error if neither
  resolves, or if the resolved connector declares no `auth_capture`
  spec (nothing to interactively capture, e.g. a connector
  authenticated purely by an API token). Added the new
  `BackupService::resolve_capture_target` method in `dbs-core` for
  this, plus the `auth_capture: Option<AuthCapture>` field it needed on
  `Handshake` (mirroring how `export_profile` was added earlier for
  that issue's needs) — a connector-handshake gap this issue surfaced
  along the way.
- Also implemented: the default output path per capture kind
  (`./<target>-session.zip` / `-cookies.txt` / `-storage_state.json`,
  matching the reference exactly), overridable with `--out`.
- **Deliberately scoped, per this repo's connector architecture:**
  connectors here are external subprocesses discovered over a
  spawn/handshake protocol that doesn't exist yet (ADR-0001 step 1,
  gap-analysis.md's Connectors cluster, #85-100) — there's no
  Playwright-Python helper to drive, and no display in this sandboxed
  environment to test one against even if there were. So once the
  target and capture kind resolve, `dbs capture` reports what it
  *would* capture (resolved connector, capture kind, destination path)
  instead of attempting real browser automation, and exits `4` (same
  code used for `sources check`/`doctor`/`serve`'s own "not available"
  states). The CLI's connector registry is always empty today, so this
  is observable end-to-end only via the resolution-failure paths —
  same limitation `dbs sources`/`dbs connectors` (#71) already
  documented.
- 6 new `dbs-core` unit tests for `resolve_capture_target` (connector
  type found directly, source-name fallback, neither resolves, source's
  connector type unregistered, connector with no `auth_capture`) plus 4
  new `dbs-cli` integration tests (unknown target, source-name fallback
  reporting its unregistered connector type, `--out` flag parsing,
  missing target as a usage error).

## dbs serve flag wiring (closes #75)
**2026-08-13**

- **Implemented:** `dbs serve --host --port --allow-setup/--no-setup
  --token --schedule/--no-schedule`, matching the reference's flag
  surface and its security-relevant validation (refusing to bind
  off-localhost without a `--token`, since the API would otherwise be
  unauthenticated and able to read backups/write secrets).
- **Deliberately scoped, per this issue's own framing:** the actual
  web server is out of scope here — see gap-analysis.md's Web tier
  rows (app skeleton, job manager, auth) — so once flags validate,
  `dbs serve` reports what it *would* do (address, scheduler-on,
  setup-actions-disabled, token-required) instead of pretending to
  listen for real, and exits `4` (same code the reference uses for its
  own "web extras not installed" case).
- `is_local_host` mirrors the reference's `is_local` check exactly,
  including treating an empty host string as local.
- 2 new unit tests for `is_local_host` plus 8 new `dbs-cli` integration
  tests: default host/port, a custom port, the off-localhost-without-
  token refusal and its accepted-with-token counterpart, `localhost`
  by name, `--schedule`/`--no-setup` reflected in the report, and
  `--allow-setup --no-setup` together as a usage error.

## dbs schedule (closes #74)
**2026-08-13**

- **Implemented:** `dbs schedule [--interval daily|hourly]`, printing
  a ready-to-paste unattended-run snippet. The Linux branch ports the
  reference's cron line + systemd service/timer unit text exactly
  (down to matching its own quirk: the systemd timer's `OnCalendar`
  isn't parameterized by `--interval` either, in the reference or
  here — not a bug this port introduced). The Windows branch is new
  (no reference equivalent): a `schtasks /Create` command, the
  standard CLI-scriptable equivalent of cron/systemd there — needed
  for this repo's cross-platform floor (gap-analysis.md), which
  covers Windows from round 1.
- `render_schedule(config_path, interval, windows)` takes the
  platform as a plain argument rather than checking
  `cfg!(target_os = ...)` internally, so both branches are exercisable
  from a single test run regardless of which OS actually built the
  binary; `cmd_schedule` passes the real `cfg!(target_os = "windows")`
  at its one call site. The printed config path is absolutized via
  `std::path::absolute` (lexical, unlike `canonicalize`, so it doesn't
  require the config file to already exist — `dbs schedule` is
  commonly run before `dbs init`).
- 6 new `render_schedule` unit tests (Linux daily/hourly content,
  cross-contamination checks in both directions) plus 2 new `dbs-cli`
  integration tests confirming the compiled binary wires the command
  up end to end.

## dbs update-ytdlp (closes #73)
**2026-08-13**

- **Implemented:** `dbs update-ytdlp [--dry-run]`, shelling out to
  `python3 -m pip install --upgrade "yt-dlp[default]"` (falling back
  to `python` if `python3` isn't on `PATH`) — matches the reference's
  own command exactly, which likewise just shells out to pip rather
  than doing anything more elaborate.
- **No before/after version reporting**, unlike this issue's filed
  acceptance checklist implied: the actual reference command (the
  source of truth over the issue text where they disagree) doesn't
  print yt-dlp's version before or after upgrading — it only echoes
  the command it's about to run and, on success, a one-line "restart
  `dbs serve`" reminder. `dbs doctor`'s `deps.yt-dlp` check (#72) is
  where a yt-dlp version would surface, and this Rust binary's own
  dependency graph doesn't include yt-dlp at all (only the yt-dlp-
  dependent connectors/`dbs capture` shell out to it, per
  gap-analysis.md's Decisions section item 3), so there's no "current
  version" to report from inside `dbs` itself either way.
- This binary has no `sys.executable` of its own (it isn't a Python
  process) — `find_python` looks for `python3` then `python` on
  `PATH` instead, erroring cleanly if neither is found.
- 4 new `dbs-cli` integration tests: `--dry-run` never executes
  anything, "no python on PATH" is a config error (tested via an
  empty-directory `PATH` override), and the successful/failing
  `pip install` branches — the latter two via a fake `python3` shell
  script placed first on `PATH` rather than a real `pip install`
  (slow, network-dependent, and would mutate the test runner's actual
  Python environment).

## dbs doctor (closes #72)
**2026-08-13**

- **Implemented:** `dbs doctor` — read-only environment/health
  diagnostics — wired to a new `BackupService::doctor` method.
- **Architecture note:** two of the reference's check categories have
  no equivalent here and are intentionally omitted (not silently
  dropped — documented on `doctor`'s doc-comment): per-source
  `config`/`deps` checks assume an in-process connector class with a
  Pydantic model and importable Python dependencies, but this port's
  connectors are external subprocesses whose only interface is the
  spawn/handshake protocol (ADR-0001 step 1) — same gap `check_sources`
  (#71) already has. `deps.yt-dlp` checks a Python package this Rust
  binary doesn't depend on at all.
- Checks implemented: `database.integrity` (`Storage::integrity_check`),
  `database.wal` (warns past 10MB, unfolded into the main file),
  `runs.interrupted` (recent-history scan), and per enabled source:
  connector resolvability, VPN-netns readiness (reusing the same
  `named_netns_exists`/`in_named_netns` logic the backup-time VPN
  guard uses), declared-secret presence (checked against a
  `.env`/environment map, the same convention `dbs export --encrypt`
  already reads passphrases from), and staleness (no successful run
  within 2x the schedule cadence — reusing `schedule_slack`).
- `dbs doctor [--json]`: `  [status] name: detail` per line (or the
  raw `DoctorCheck` list as JSON); exits `1` if any check is `fail`.
- 11 new `dbs-core` unit tests (using `ConnectorRegistry::from_resolved`
  for the checks that need a real registry entry — secrets ok/fail,
  VPN guard-off vs. netns-not-up, staleness warn vs. never-run) plus 5
  new `dbs-cli` integration tests covering what the CLI's always-empty
  registry can honestly produce: healthy-database-only state, the
  connector-unavailable failure and its exit code, a disabled source
  reporting ok, and `--json` in both the healthy and failing cases.

## dbs sources / dbs connectors (closes #71)
**2026-08-13**

- **Implemented:** `dbs sources list/add/check` and `dbs connectors
  list/describe`, wired to four new `BackupService` methods
  (`list_sources`, `list_connectors`, `check_sources`, `add_source`)
  that didn't exist before this issue.
- **Architecture note:** the reference's `check_sources`/`add_source`
  validate each source's options against the connector's in-process
  Pydantic model — there's no equivalent here, since this port's
  connectors are external subprocesses whose only interface is the
  spawn/handshake protocol (ADR-0001 step 1), which doesn't expose a
  config schema to validate against. Both methods instead validate
  connector *resolvability* (does the registry have this type at all)
  — the one thing answerable without spawning anything, and exactly
  what the acceptance checklist's "a connector `check` failure
  surfaced to CLI output" scenario tests.
- `dbs sources list [--json]`: name/type/enabled/backed-up per
  configured source, sorted by name; "No sources configured. Add one
  with: dbs sources add ..." when empty.
- `dbs sources add NAME --type TYPE [--set key=value]...`: appends a
  `[sources.NAME]` TOML block to the config file after checking the
  name isn't already taken and the connector type resolves — writes
  nothing on either failure. `--set` values are best-effort coerced to
  bool/int/list/string (`coerce_set_value`, mirroring the reference's
  `_coerce`) and serialized back to TOML literals (`toml_value`,
  mirroring `_toml_value`).
- `dbs sources check`: validates every configured source's connector
  type resolves; exit `4` if any don't.
- `dbs connectors list [--json] [--verbose]`: every registry entry,
  `--verbose` also showing load failures/shadowed collisions from
  `ConnectorRegistry::report()`.
- `dbs connectors describe TYPE`: display name, description, item
  kinds, required secrets, and capability flags from the connector's
  handshake — the reference's Pydantic `config_model.model_json_schema()`
  section renders as an honest empty `{}`, since the handshake
  protocol carries no schema to show.
- Updated `a_nested_stub_subcommand_reports_not_yet_implemented` to
  use `research youtube` instead of `sources list`, now that
  `sources`/`connectors` are no longer stubs.
- 6 new `dbs-core` unit tests (using `ConnectorRegistry::from_resolved`
  to build a real registry entry, the way `list_sources`/
  `list_connectors`'s "found" paths and `add_source`'s success path
  can only be exercised) plus 11 new `dbs-cli` integration tests
  covering what the CLI's always-empty registry can honestly produce:
  empty/populated config, JSON output, and every failure path
  (`check` surfacing "not found", `add` rejecting an unregistered type
  or a duplicate name without touching the file, a malformed `--set`
  pair, `describe` on an unknown type).

## dbs export* / dbs decrypt CLI wiring (closes #70)
**2026-08-13**

- **Implemented:** `dbs export`, `dbs export-notes`, `dbs
  export-profiles`, `dbs export-wiki`, and `dbs decrypt`, wired to the
  export/crypto machinery that already existed in `dbs-core` (the
  exporter issues #51-#58, `BackupService::export` pulled forward into
  #61, `notes_export::export_notes`/`export_wiki_dir` from #61, and
  the crypto module). `BackupService::export_profiles` — previously
  private, used only to build the export manifest — is now `pub` so
  the CLI can render it directly.
- **`BackupService::export` gained encryption:** a new
  `encrypt_passphrase: Option<&str>` parameter. When set, the exporter
  writes through an `EncryptingWriter` instead of straight to the file
  — still inside the existing tmp-file-then-rename span, so a crash
  mid-export still never leaves a half-written (or half-encrypted)
  file at the destination. Passphrase *resolution* (`--passphrase-env`
  / `.env` / the process environment, via
  `crypto::resolve_passphrase`) stays the CLI's job, mirroring how the
  CLI already owns `ExportQuery` construction.
- `dbs export --out PATH --format FMT [--source]... [--type]...
  [--since] [--until] [--since-updated] [--until-updated]
  [--include-deleted] [--include-revisions] [--no-raw]
  [--wiki-grouping] [--encrypt] [--passphrase-env]`: all 7 formats,
  filter flags mapping onto `ExportQuery` (reusing `parse_date_arg`
  from #69 for `--since`/`--until`/`--since-updated`/`--until-updated`).
- `dbs export-notes --out-dir DIR [--source]... [--type]... [--since]
  [--full]` and `dbs export-wiki --out-dir DIR [--grouping]
  [--source]... [--type]... [--since]`: unzipped directory variants of
  the `obsidian`/`wiki` formats for a folder-watching downstream tool.
- `dbs export-profiles [--json]`: each source's resolved export rules
  (item kinds, group-by, body-from, page-per), with a `*` marker on
  fields a `[sources.NAME.export]` block explicitly overrode.
- `dbs decrypt SRC [--out] [--passphrase-env]`: decrypts a `dbs export
  --encrypt` file back to plain form — refuses to overwrite an
  existing destination, and cleans up a partial destination file on a
  failed decrypt (wrong passphrase/corruption) rather than leaving one
  behind.
- Passphrases are resolved from a `<config dir>/.env` file (the same
  convention `dbs init` already writes `.env.example` into) or the
  process environment — never accepted as a CLI argument.
- Updated `a_stub_subcommand_reports_not_yet_implemented` to use
  `verify` instead of `export`, now that the `export*`/`decrypt`
  family is no longer stubbed.
- 2 new `dbs-core` unit tests (an encrypt-then-decrypt round trip
  producing byte-identical content to a plain export; a source-level
  `[sources.NAME.export]` override actually taking effect) plus 15 new
  `dbs-cli` integration tests seeding real item rows via `SqliteStorage`
  (no connector-candidate discovery exists yet, #85-100): each export
  format's CLI invocation, an unknown format, a full CLI-level
  encrypt/decrypt round trip, decrypt's overwrite/not-encrypted/
  missing-file/wrong-passphrase error paths, `export-notes`,
  `export-wiki`, and `export-profiles`' override marker (text and
  JSON).

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
