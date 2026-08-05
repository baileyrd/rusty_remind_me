# Release Notes

Dated entries, newest first. One entry per merged pull request.

## 2026-08-05 — Thread-stack dumps for the slow-call watchdog

### Added
- **`stack-dumps`, an optional Linux-only feature** that makes the slow-call
  watchdog dump *every thread's stack* when a call runs past the threshold —
  the reference's `faulthandler.dump_traceback_later` guarantee, including for
  a thread wedged in synchronous CPU-bound code.
- `watchdog::install_stack_dump_hook()`, which a binary must call first thing
  in `main`, and `watchdog::stack_dumps_available()`, which reports whether all
  three preconditions (feature, platform, hook) actually hold.
- `StuckCall::stacks`, carrying the dump when there is one.

### Changed
- Nothing, with the feature off — which is the default, and every non-Linux
  build. The watchdog still reports the stuck call's identity and duration.

### Notes
- **This costs a system library**, `libunwind-ptrace` (`libunwind-dev`), plus
  permission to `ptrace`. That is the first exception to this crate's
  "no system binary" rule for optional features, which is why it is off by
  default and why `docs/adr/0014` exists.
- **Out-of-process by design.** Capture spawns a short-lived child that
  `ptrace`s this process. The cheaper-looking alternative — unwinding the stuck
  thread from a signal handler — can deadlock, because capturing a backtrace is
  not async-signal-safe, and it would deadlock exactly when the diagnostic is
  needed. The reference's rule that the watchdog must never be why a call fails
  ruled it out.
- **The hook is a safety interlock, not a formality.** Capture re-executes the
  binary; without the hook that child would run the *program*, starting a
  second server. Nothing is spawned unless the hook has announced itself, so
  forgetting it degrades to the feature-off behaviour rather than misfiring.
- **A refused `ptrace` is survivable.** Hardened hosts (`ptrace_scope` 2/3,
  seccomp, no `CAP_SYS_PTRACE`) log once and still get the identity-and-duration
  report.
- Tested by actually wedging a thread in CPU-bound code and asserting the dump
  names that frame with a source location — not by compiling the feature and
  hoping. CI runs those tests.

## 2026-08-05 — Windows Job object for sidecars (#186)

### Added
- **Sidecars are assigned to a Windows Job object** created with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, matching the reference. When the
  server dies abnormally on Windows — `TerminateProcess`, a hard crash, a
  power cut — the OS now reaps the SSH tunnel and the dashboard UI instead of
  leaving them orphaned holding their ports. No exit hook is involved, which
  is what makes it hold where `Drop` cannot.
- `windows-sys` as the workspace's **first and only FFI dependency**, declared
  under `[target.'cfg(windows)'.dependencies]` so it is absent from the
  dependency graph on Linux and macOS entirely.

### Changed
- `sidecars`' teardown table now reads **all four cells matching** the
  reference, where it previously documented one divergent cell. The Unix
  abnormal-exit row still orphans — in both implementations.
- All three Win32 calls report `GetLastError()` on failure, matching the
  reference (whose issue #138 was precisely about these having been silent).
  None is fatal: a failure costs the abnormal-exit guarantee, not the sidecar.

### Notes
- **The Windows path is type-checked, not runtime-tested.** CI is
  `ubuntu-latest` only. Verification is `cargo check --target
  x86_64-pc-windows-gnu -p remind_me_core` plus a line-by-line read against the
  reference's `ctypes` calls. Runtime-testing it needs a Windows runner.
- **This reverses the "no FFI" half of ADR-0013**, which is amended in place
  rather than quietly contradicted. ADR-0012's refusal of `libc` stands: that
  probe has a pure-`std` alternative, and `CreateJobObjectW` does not.
- One deliberate divergence: when `SetInformationJobObject` fails, the
  reference keeps the now-useless handle and still assigns children to it;
  this closes it and skips assignment. Identical sidecar behaviour, minus a
  leaked handle.

## 2026-08-05 — `remind_me_history` defaults `limit` to 10 (#183)

### Changed
- **`limit` now defaults to 10**, matching the reference's
  `RevisionHistoryInput.limit` (`models.py:504`). It defaulted to 20 here.

### Notes
- **A caller who omits `limit` now gets 10 revisions instead of 20.** Callers
  that pass it explicitly are unaffected.
- The bounds already matched (`1..=100`); only the default had drifted, so
  this is the narrowest possible fix.
- Unlike the other signature gaps found this run, nothing here was rejected,
  created, or lost — the extra revisions were real. What it broke was the
  "same call, same answer" property: a session summarising "your last N edits"
  got a different N depending on which implementation it was talking to.
- A test pins both the struct default and the declared schema, so the next
  drift is caught rather than re-derived.

## 2026-08-05 — `response_format` on `remind_me_history` and `remind_me_stats` (#176)

### Added
- **`response_format` on both tools**, defaulting to **markdown** — the
  reference's default for `RevisionHistoryInput` (`models.py:510`) and
  `MemoryStatsInput` (`models.py:754`).
- `history::render_revisions_markdown` and `stats::render_markdown`, kept
  byte-compatible with the reference's `_fmt_revisions` and the markdown
  branch of its `remind_me_stats` — including the empty-history sentence
  (`_No revision history for memory ..._`) and the 200-character revision
  preview.

### Notes
- **This changes what both tools return by default.** They emitted JSON
  unconditionally; they now emit markdown unless asked for JSON. Deliberate:
  the point of the field is that the same call gets the same answer from
  either implementation, and defaulting to JSON here would have kept the
  divergence while merely adding a knob.
- **`remind_me_history`'s JSON branch is now an envelope**, matching the
  reference: `{memory_id, count, revisions}` rather than the bare array it
  returned before. `count` saves a caller re-deriving what the producer
  already knew.
- **This is not the case the decision log already covers.** It records
  refusing `response_format` twice — for `remind_me_recalibrate_candidates`
  (#102) and the four saved-search tools (#117) — both times because the
  *reference's* models lack the field, so adding it would diverge in the
  direction that breaks drop-in interoperability quietly. These two are the
  opposite: the reference has it, and its absence here was the divergence.
- The revision preview truncates by characters, not bytes — slicing a
  multi-byte codepoint would panic on arbitrary user content.
- **Neither tool had any MCP-level test coverage**, which is why changing
  their default output broke nothing. Four tests now cover both formats for
  both tools, the empty-history sentence, and the declared defaults.
- Found while here and filed separately rather than folded in: `limit`
  defaults to 20 on `remind_me_history` where the reference uses 10 (#183).

## 2026-08-05 — `remind_me_entity` is read-only, matching the reference (#177)

### Changed
- **`remind_me_entity` no longer writes.** It was an upsert taking
  `{name, kind}`; the reference's is a lookup taking `{name, limit}` with
  `readOnlyHint: True`. Same tool name, opposite effect — a mistyped name
  returned `found=false` from `remind_me` while silently *creating* a junk
  entity here. It now returns `{found: true, ...profile}` or
  `{found: false, query, message}`.

### Added
- **`remind_me_entity_upsert`** — target-only, and where the old write
  behaviour lives. The capability is kept; it is just no longer reachable by a
  call that meant to read.

### Notes
- **Breaking for callers that relied on `remind_me_entity` to create.** They
  move to `remind_me_entity_upsert`. This was chosen deliberately over keeping
  the superset: a silent write on a read-shaped tool is the kind of divergence
  that shows up as data drift rather than an error.
- The lookup reuses `entity::entity_profile`, already shared with
  `GET /api/entity`, so a dashboard and an LLM client see identical data.
- `found` is spread alongside the profile's fields rather than wrapping them,
  matching the reference's `{"found": True, **profile}`.
- A miss is **not** `isError` — an unknown name is a valid answer, and
  flagging it would make clients retry a question already answered.
- `remind_me_entity_upsert` is in the `core` tool profile. `remind_me_entity`
  was already there and could write, so omitting it would have cost a trimmed
  profile the ability to create an entity at all.
- The CLI's `entity` subcommand still upserts — it calls
  `entity::upsert_entity` directly, and the reference's CLI has no `entity`
  subcommand, so nothing pulls either way. Left as-is deliberately.
- The old tool had **no test coverage at all**, which is why nothing broke
  when its behaviour was replaced. Five tests now cover both halves.

## 2026-08-05 — Export no longer resurrects deleted and superseded memories (#175)

### Fixed
- **`remind_me_export_memories` was exporting soft-deleted and superseded
  memories, with no way to exclude them.** `export::filters` had neither a
  `deleted_at IS NULL` nor a `superseded_by IS NULL` condition, so it behaved
  as a permanent `include_deleted=true`. Since every exported record is
  stamped `role: "assistant"` so the importer reads it as live content, an
  export → import round-trip **brought back everything the user had deleted or
  superseded, as fresh live memories.**

### Added
- **`ExportInput.include_deleted`**, defaulting to `false` and gating both
  conditions together, matching `exporter.py:163`. The escape hatch for a
  genuine full-backup or audit export.
- The same flag as an `include_deleted` query parameter on `GET /api/export`,
  because the reference exposes it over HTTP too (`api.py:1383`) — unlike
  `clear_superseded` (#174), which is MCP-only upstream and stayed that way.

### Notes
- **This changes what an export returns by default.** Anyone relying on the
  current output gets a smaller file. That is deliberate: defaulting to `true`
  to preserve today's behaviour would keep both the interoperability gap and
  the data-resurrection path open, and the reference is unambiguous about
  which default is correct.
- **Superseded memories leaked regardless of sync; tombstones only with sync
  on.** `delete_memory` hard-deletes when sync is disabled and tombstones when
  it is enabled, so a synced vault — the deployment this crate is built for —
  accumulated tombstones the export then carried.
- The tool description previously claimed "a complete logical backup"; it now
  says what the default actually does and points at the flag for the rest.
- `Request::query_bool_default_false` is deliberately not the negation of
  `query_bool_default_true`: a bare `?include_deleted` with no value reads as
  *on*, which is how a query-string flag is normally written.
## 2026-08-05 — `clear_superseded` on `remind_me_update` (#174)

### Added
- **`MemoryUpdateInput.clear_superseded`** — clears a memory's `superseded_by`
  pointer, un-hiding it from search, entity and subject/predicate lookups. The
  recovery path for a false-positive contradiction-supersession, matching
  `models.py:391` and `crud.py:410`.

### Notes
- **The two sides disagreed about a caller who sent this field, and not
  symmetrically.** `remind_me`'s models are `extra="forbid"`, so it *rejects*
  an unknown field loudly; serde here *ignores* one. A client sending
  `clear_superseded: true` was getting a success response and no un-hiding,
  with nothing saying why. A silently-ignored flag is a worse failure than a
  rejected one because it is invisible from the caller's side.
- **A plain `bool`, not `Option<bool>`** — unlike `sensitive`, there is no
  "set it back on" direction to express. Re-superseding is something
  `remind_me_add` does on detecting a contradiction, never something an update
  asserts directly. The reference types it the same way.
- **Deliberately not exposed on the HTTP `PATCH` route.** The reference's own
  `api_update` (`api.py:1047`) handles content/category/source/tags/metadata/
  sensitive and nothing else, so this is an MCP-tool affordance upstream, not
  an HTTP one. Adding it to the route would be a surface this crate has and
  `remind_me` does not.
- Clearing does **not** cascade to the memory that did the superseding, which
  the reference states explicitly and a test asserts.
- **How this was found.** The 2026-08-05 assessment compared MCP tools by
  *name* and reported 61/61. A follow-up pass comparing every reference
  `*Input` model's fields against this crate's structs found this and three
  siblings (#175, #176, #177). Name-matching alone is not a parity check.
## 2026-08-05 — Sidecar processes (#169)

### Added
- **`sidecars`** — child processes kept alive alongside the server: the hub
  SSH tunnel, and optionally the dashboard UI. `Sidecars::ensure` is
  idempotent and runs at the top of each sync cycle, so a sidecar lost to a
  sibling server's exit returns within one interval.
- **`REMIND_ME_TUNNEL`** — full command line for the tunnel. Unset = no
  sidecar management at all.
- **`REMIND_ME_SIDECAR_UI`** — `1`/`true`/`yes` to also keep the dashboard
  alive, on `REMIND_ME_MCP_UI_PORT` (default `5199`, the reference's).

### Notes
- **Teardown is guaranteed on graceful exit, not on abnormal exit — and that
  is one cell short of the reference, not four.** Children are killed on
  `Drop`, and unwinding runs `Drop`, so a normal return *and* a panic both
  tear them down. What is missing is the reference's Windows Job object with
  `KILL_ON_JOB_CLOSE`, which survives `TerminateProcess`. Its `_job()` returns
  `None` immediately on non-Windows, so on Linux and macOS the reference
  orphans its sidecars on abnormal exit exactly as this does. See
  `docs/adr/0013` and the table in the module docs.
- **No new dependency.** Matching the Windows cell needs `windows-sys`; this
  workspace has deliberately had no FFI dependency at all (`docs/adr/0012`
  took the same decision against `libc`). ADR-0013 records the trade and how
  to revisit it, rather than leaving a silent gap.
- **Backslash handling in the tunnel command is platform-dependent, and has
  to be.** On Windows `\` is a path separator, so `"C:\Program Files\ssh.exe"`
  must survive intact; on Unix it escapes. The reference makes the same split
  via `shlex.split(..., posix=sys.platform != "win32")`. Treating `\` as an
  escape everywhere would mangle every Windows tunnel command into an
  unrunnable path — there is a test for exactly that.
- **The port, not the process handle, decides whether to start.** Several
  servers share one database, so a tunnel started by a sibling is a perfectly
  good tunnel; keying off `self.procs` would start a second one.
- **A dead sidecar is reaped before respawning.** Without that, a
  persistently-failing sidecar (bad key, unreachable host) leaks one zombie
  per tick for as long as the misconfiguration lasts — the reference hit
  exactly this as its issue #139.
- Sidecar stdio is detached. A child writing to the MCP server's stdout would
  corrupt the JSON-RPC stream, which is the one unrecoverable failure here.
## 2026-08-05 — Fix a ~50% flake in the recalibration boundary test (#180)

### Fixed
- **`recalibrate_test::the_boundary_day_qualifies` was a coin flip**, failing
  about half of all CI runs on unrelated changes. It planted a memory at
  *exactly* the stale threshold using a sub-second timestamp, then compared it
  against `julianday('now')` — which SQLite evaluates at coarser precision, so
  the difference landed just *below* the threshold roughly half the time.
  Measured over 2000 samples of the same arithmetic: `diff < 90` in 993, with a
  typical shortfall of `-1.16e-08` days.

### Notes
- **The exact-boundary form could not be made to work, only made to look like
  it worked.** Its stated purpose was to distinguish the predicate's `>=` from
  a `>`, which needs a difference of exactly `90.0` — unreachable against a
  wall clock read by `julianday('now')`. It caught nothing reliably; a `>=` →
  `>` regression would have moved it from "fails half the time" to "fails half
  the time".
- Replaced with `the_stale_window_is_bracketed_on_both_sides`, which pins the
  threshold from both directions with a margin: `+2s` past the window must
  qualify, `-1h` inside it must not. Two seconds far exceeds any plausible
  clock or precision skew and is far below the day granularity the window is
  expressed in. 25 consecutive runs, zero failures.
- The reasoning is recorded in the test itself, so the exact-boundary version
  is not "restored" later as an apparent improvement.

## 2026-08-05 — A stuck-call watchdog (#168)

### Added
- **`watchdog`** — a call running past a threshold announces itself. Previously
  the only symptom of a hung tool call, from outside the process, was the
  client reporting a timeout; nothing said *which* call or for how long.
- **`REMIND_ME_SLOW_CALL_SECONDS`** — threshold in seconds, default `30`,
  `0` disables. Both match the reference.
- **`watchdog` on `remind_me_server_status`** — `enabled`,
  `threshold_seconds`, `calls_in_flight`, the same three fields the
  reference's `watchdog.status()` reports.

### Notes
- **This is a weaker signal than the reference's, deliberately.** The reference
  arms `faulthandler.dump_traceback_later`, which dumps every thread's stack
  including one blocked in synchronous CPU-bound code. Rust has no stdlib
  equivalent, and pulling in a stack-unwinding crate for a diagnostic would be
  a new third-party dependency. So this reports *identity and duration* — which
  call, how long — rather than a stack. The property that mattered survives: a
  stuck call names itself without a debugger attached. The module docs say this
  outright rather than leaving it to be discovered.
- **Reference-counting is `CallGuard`'s job, not a counter's.** Calls overlap,
  so a watchdog tied to one would disarm while others still run. Dropping the
  guard is what disarms — which means an early return, a `?`, or a panic cannot
  leak a permanently-armed watchdog the way an unbalanced manual `disarm()`
  can. A test asserts the panic path specifically.
- **One report per stuck call, not one per monitor wake-up.** The reference
  passes `repeat=True` because a *later stack dump* can show something the first
  did not. Here the payload is "call X has been running Ys", which does not
  change qualitatively, so repeating it would be noise.
- **A malformed threshold falls back to the default rather than disabling.**
  `0` is the off switch; a typo in a tuning knob should not silently turn the
  diagnostic off.
- **The monitor thread starts lazily, on the first armed call.** The CLI's
  one-shot subcommands and every test that only builds a server never pay for a
  thread they will not use.
- The report sink is injectable, so the firing behaviour is tested against a
  120 ms threshold and a channel rather than by capturing stderr and waiting 30
  seconds.
- `server_status` counts the status call itself in `calls_in_flight`; the
  reference does the same, since its own `arm()` is active during the call.
## 2026-08-05 — The `list` CLI subcommand (#167)

### Added
- **`rusty-remind-me list [--limit N] [--category CATEGORY] [--json]`** — browse
  memories by filter, the counterpart to `search`. The reference has exposed
  `add`/`search`/`list` since its issue #189; this crate had only the first two,
  so the one subcommand for enumerating a known slice was missing.

### Notes
- **The flag set is the reference's, not this crate's tool input's.**
  `MemoryListInput` also carries `tags`, `source`, `offset` and
  `include_sensitive`, but `remind_me_mcp/cli.py`'s `list_p` exposes only
  `--limit`, `--category` and `--json`. Accepting flags the reference rejects
  would be the same drop-in divergence as missing ones, just in the other
  direction, so the extra fields stay at their defaults.
- **An out-of-range `--limit` is refused rather than clamped.** `list_memories`
  clamps into `LIST_LIMIT_MIN..=LIST_LIMIT_MAX` silently, which would hand
  someone who asked for 500 a page of 100 with no indication why. The reference
  bounds `limit` on its pydantic model and so rejects it; this matches that at
  the CLI boundary instead of inheriting the clamp.
- **Markdown by default, JSON on `--json`,** matching the reference's
  `ResponseFormat` default. The renderer is the existing
  `reminders::render_memories_markdown`, already kept byte-compatible with the
  reference's `_fmt_memory_md` — a second renderer would be a second thing to
  keep in step.
- The JSON branch serializes `MemoryListResult` whole. The reference's payload
  is `count`/`memories`/`total`; this additionally carries the pagination
  cursor rather than reshaping into a strictly smaller object.
- Parsing is factored into `parse_list_args` returning `Result` rather than
  exiting inline, so it is testable without a process boundary. Six end-to-end
  tests still drive the real binary, since a parser wired to nothing would pass
  the unit tests.

## 2026-08-05 — Cross-encoder reranking (#155, part 2 of 2)

### Added
- **Optional `rerank` feature** (`rten`/`tokenizers`) adding a cross-encoder
  stage that re-scores the head of the RRF-ranked list. RRF fuses *independent*
  rank lists and so never reads the query and a candidate together; a
  cross-encoder does, which is far more precise at ordering the few candidates
  that matter.
- `MemorySearchResult.rerank_score`, the logit each rescored pair got. It is a
  diagnostic, **not** a component of `score` — reranking permutes the head
  rather than contributing to the fused total.

### Notes
- **Reranking may never break search.** A missing feature, an unconfigured
  model, an unreadable one, a tokenizer mismatch, an inference failure, a
  scorer returning the wrong number of scores — every one returns the incoming
  order untouched. None is an error. Search already worked without this.
- **The head is reordered; the tail is preserved.** Only the first
  `REMIND_ME_RERANK_TOP_K` (default 20) are rescored, and the rest keep their
  places. Reranking never drops a candidate and never changes how many results
  come back — it permutes a prefix. Ties keep their RRF order.
- **The pool is wider than the response limit, and truncation happens after.**
  Rescoring only what was already going to be returned would discard the most
  useful thing a cross-encoder does: promote a candidate from just past the
  cutoff. Feedback adjustment still runs first, so it perturbs the order
  feeding the cross-encoder and the cross-encoder keeps the final say.
- **On by default, but "on" never means "downloads a model".** `REMIND_ME_RERANK`
  defaults to enabled, matching the reference. What differs is the cost: the
  reference fetches a cross-encoder from HuggingFace on first use, whereas
  `REMIND_ME_RERANK_MODEL_PATH` and `REMIND_ME_RERANK_TOKENIZER_PATH` must
  point at files that already exist. Unconfigured, reranking is a no-op — which
  is the default state of every build.
- **`rten` again rather than a second runtime.** It is already the optional
  dependency behind `ocr`, is pure Rust, and takes explicit model paths.
- A malformed `REMIND_ME_RERANK_TOP_K` falls back to the default rather than
  disabling reranking: `REMIND_ME_RERANK` is the switch, and a typo in a tuning
  knob should not silently turn a feature off.
- **Unlike `ocr`/`audio`, CI runs this feature's tests.** The whole ordering
  contract is testable with an injected scorer — no model, no runtime — and the
  feature-on tests additionally prove that compiling the reranker in does not
  make a search download anything. What is *not* verified is the quality of a
  real cross-encoder's ordering, which needs real weights.

## 2026-08-05 — Image (OCR) and audio (transcription) import (#156)

### Added
- **Optional `ocr` feature** (`ocrs`/`rten`) importing `.png`, `.jpg` and
  `.jpeg` by recognising the text in them. One memory per image, matching the
  reference's deliberate choice not to make one per detected text region.
- **Optional `audio` feature** (`whisper-rs`/`symphonia`) importing `.mp3`,
  `.m4a`, `.wav` and `.ogg` by transcribing them. One chunk per Whisper
  segment, each carrying `{"start", "end"}` in seconds — the positional anchor
  that lets a search hit be found again in the recording, exactly as a PDF
  chunk carries its page.
- `image` and `audio` import kinds, picked automatically from the extension.

### Fixed
- **PDF imports recorded `source = "document_import"`; they now record
  `"pdf_import"`, as the reference does.** These are not interchangeable:
  `normalize` selects on `source IN ('document_import', 'chat_import')`, so
  extracted PDF text was silently enrolled in a rewriting pass the reference
  deliberately keeps it out of.

### Notes
- **Neither feature downloads a model, ever.** `REMIND_ME_OCR_DET_MODEL_PATH`
  and `REMIND_ME_OCR_REC_MODEL_PATH` point at the two `ocrs` models;
  `REMIND_ME_AUDIO_MODEL_PATH` points at a Whisper GGML file. Unset, an import
  fails with a message naming the variables and saying where the files come
  from. This is stricter than the reference, which fetches from HuggingFace on
  first use — importing a voice memo should not quietly pull several hundred
  megabytes.
- **This is the one place the port needs configuration the reference does
  not.** RapidOCR's models ship inside its Python wheel, so the reference needs
  none at all; the Rust models are separate files.
- **A textless image and a silent recording are refused, not imported as zero
  memories.** A successful import of nothing is indistinguishable from an empty
  file — the failure #147 fixed for JSONL transcripts and #153 for scanned
  PDFs.
- **`ocrs`/`rten` rather than an ONNX Runtime binding.** The reference picked
  RapidOCR because it already had ONNX Runtime present for its embedder; that
  reasoning does not transfer, because the Rust ONNX binding does not carry a
  runtime — it downloads one at run time. A runtime whose install strategy is
  an implicit download is the wrong shape for a feature required not to
  download anything. `ocrs` is pure Rust and takes explicit model paths.
- **No system `ffmpeg`.** whisper.cpp decodes nothing, so `symphonia` handles
  all four containers in-process — the same "no system binary" rule that made
  the reference reject `pywhispercpp` in favour of faster-whisper.
- **Resampling to 16 kHz low-passes first.** Dropping samples would fold
  everything above 8 kHz down into the speech band as a tone that was never in
  the recording, degrading transcription in a way that looks like a bad model
  rather than a bad resampler.
- **A real recognition or transcription is not verified at all yet**, and
  cannot be in CI without a model. CI compiles both features; the feature-off
  refusals, the model-configuration errors, and the audio decode/resample
  arithmetic are tested unconditionally. That an actual image OCRs to the right
  text, or an actual recording transcribes to it, has **not** been run against
  a real model — do that by hand before relying on the output.
- Images and recordings are now *supported* formats, so a directory sweep or
  watcher picks them up instead of passing over them. Feature-off, each is
  reported as a failed file rather than skipped — the same as the reference.

## 2026-08-04 — ANN vector index (#155, part 1 of 2)

### Added
- **Optional `ann` feature** (`usearch`) putting an approximate-nearest-
  neighbour index in front of vector recall, so it stops being a full scan of
  every chunk. `ann_index::build` creates and persists it; search uses it
  automatically when it is present and current.

### Notes
- **The index narrows candidates; it never scores them.** It over-fetches, and
  the caller then computes the **exact** dot product over that much smaller
  set. Scores are therefore identical to brute force, so nothing downstream —
  RRF fusion in particular — has to know which path ran, and a category filter
  can still be applied during exact scoring, which the index cannot express.
- **Every failure resolves to brute force.** No index, a stale one, an
  unreadable one, a dimension mismatch, an empty candidate set: all fall back.
  A search must never fail, or change its answers, because an optimisation was
  unavailable.
- **A short list after filtering falls back rather than being returned.** A
  category filter can remove most of what the index proposed, and silently
  returning fewer results than a full scan would is a retrieval regression
  nobody would notice.
- **Staleness is detected, not assumed away.** The vector count and dimension
  are recorded at build time and checked at search time. A stale index quietly
  returning deleted memories is worse than no index, because the results still
  look plausible.
- **The position → rowid mapping is persisted alongside the index**, not held
  in a process-local map. In memory it would make the index work only in the
  process that built it and fall back silently everywhere else — a feature that
  looks enabled and never runs.
- **Building is always explicit.** A search must not silently pay for an index
  build, which would turn one slow query into a pathological one at exactly the
  moment someone is waiting.
- CI **runs the tests** for this feature rather than only clippy: the
  equivalence check — that narrowing never drops a row brute force would rank —
  is the reason the feature is safe to enable, so it has to actually run.
- The reranker half of #155 is **not** included. It needs a model downloaded at
  runtime and cannot be verified in CI, so it is left as its own change rather
  than blocking the half that can be.

## 2026-08-04 — Cloud backup upload (#154)

### Added
- **Optional `cloud-backup` feature** (`aws-sdk-s3`) uploading a finished local
  backup to S3 or any S3-compatible endpoint. Reported on `BackupOutcome` as
  `upload`.

### Notes
- **The plaintext gate is the point of this module, and my issue missed it
  entirely.** When `REMIND_ME_DB_ENCRYPTION_KEY` is unset the backup file is
  **plaintext personal data** — every memory the vault holds, in the clear.
  Uploading it requires the explicit
  `REMIND_ME_BACKUP_S3_ALLOW_PLAINTEXT_UPLOAD` opt-in, checked **before any
  client is built or any byte leaves the machine**. Without it, "enable cloud
  backup" would silently mean "ship an unencrypted copy of everything to a
  third party" — a materially different decision from the one being made.
  Uploading plaintext personal data to third-party storage needs explicit
  consent, not silent default behaviour.
- **No bespoke credential environment variables**, and my issue had this
  backwards too. The AWS SDK's own credential chain is used; a parallel
  secret-storage convention would be one more thing to get right with none of
  the existing hardening. Only bucket, prefix, endpoint and region are read
  here. A test pins that none of those names looks like a credential, so a
  future `..._SECRET_KEY` reads as the regression it would be.
- **The local backup always wins.** Upload runs strictly after the local file
  is finalised, and a refused, failed or unconfigured upload is reported
  *alongside* the backup rather than instead of it. `upload_backup` cannot
  return an error into the backup path.
- **The gate is a pure function of the environment**, so it is tested without
  a bucket, a network, or the optional feature compiled in — gating its
  coverage behind a feature flag would mean the default build never checks the
  control protecting the default build. Only a truthy opt-in counts; `0`,
  `false`, `off` and blank all still refuse.
- Prefix slashes are normalised, so `/host/backups/` and `host/backups` are the
  same key and a blank prefix uploads at the root.
- CI clippy-checks the feature-on build. It is a check rather than a second
  full test run because the SDK is a 233-crate tree and the decision that
  matters is already covered unconditionally.

## 2026-08-04 — PDF import (#153)

### Added
- **`pdf` import kind**, behind an **optional `pdf` Cargo feature**
  (`pdf-extract`). Per-page text extraction feeding the existing chunker, each
  chunk tagged `{"page": N}`.
- `.pdf` joins the supported extensions and is picked automatically by `auto`
  — unlike `readwise`, a `.pdf` is unambiguous, so there is nothing to
  misroute it as.
- **CI now builds and tests the feature-on configuration**, so an optional
  feature cannot rot untested and break only for whoever enabled it.

### Notes
- **The dependency was verified to build here before any code was written.**
  `pdf-extract` is pure Rust, 71 transitive crates, ~19s. Optional because most
  builds do not want a PDF parser, mirroring the reference's lazily-imported
  extra.
- **Feature-off is a clear refusal naming the flag to rebuild with** — not a
  crash, and not a silent success. "unsupported format" would send someone
  looking at their file instead of their build.
- **A PDF with no extractable text is refused, not imported as nothing.** A
  pure scan parses fine and yields empty text on every page; recorded as a
  successful import of zero memories, that is indistinguishable from importing
  an empty file — the silent failure #147 fixed for JSONL. It says what
  happened and points at OCR.
- **Per page, not per document.** A page is the only positional anchor a PDF
  reliably has, and it is what lets a search hit be found again in the
  original.
- `raw_bytes` is now threaded through `import_content`. A PDF is binary, and
  the lossy UTF-8 decode every text connector receives has already destroyed
  it by then.
- Extraction is wrapped in `catch_unwind`: the parser can panic on some
  malformed files, and a corrupt attachment must not take down the process
  that was merely asked to read it.
- `raw_entries` counts **pages that carried text**, not chunks produced — the
  honest answer to "how much of this document was readable".

## 2026-08-04 — Automation event stream (#152)

### Added
- **`events::emit`** — POSTs `created` / `updated` / `deleted` to
  `REMIND_ME_EVENT_WEBHOOK_URL` as memories are mutated locally, for a relay,
  a second indexer, or an audit log.

### Notes
- **The payload is metadata only** — `event`, `memory_id`, `category`,
  `timestamp` — and carries **no memory content, tags or metadata**. This is an
  event-notification stream, not a content-sync mechanism; a consumer that
  wants the memory calls back with the id, where the ordinary sensitive-memory
  rules apply. Content on the wire here would silently turn every configured
  webhook into an egress path for the whole vault. My issue said the opposite
  ("carry enough to act on without a follow-up read") and was corrected first.
  A test asserts the payload has exactly four keys, because a fifth added
  carelessly is how content leaks in.
- **Sync-applied writes emit nothing.** Only local mutations do. Emitting on a
  record arriving from a peer is how two synced nodes would echo each other's
  mutations forever.
- **A separate config and delivery path from notifications, not a second use
  of it.** Notifications are human-facing and deliberately throttled; this has
  **no throttling at all**, because suppressing a "repeat" would silently drop
  a real mutation. Same transport, opposite requirements.
- **Unconfigured is a true no-op** — no thread started, not one that discovers
  it has nowhere to go. This runs on every write. A blank URL counts as unset,
  which is how it arrives from many process managers.
- **A dead endpoint cannot fail the write.** The POST runs on a detached
  thread whose handle is held until it finishes: a write must not wait on a
  webhook, but a handle dropped immediately can also lose the POST mid-flight.
- Tests run against a real listening socket rather than only inspecting the
  payload builder — the no-content guarantee is a property of what is actually
  sent, and a builder-only test would keep passing if the emit path started
  sending the whole memory.

## 2026-08-04 — Maintenance nudges and capture health (#151)

### Added
- **`maintenance::pending_counts`** — depth of every maintenance queue:
  undecomposed captures, unannotated memories, unnormalized imports,
  unclassified memories, contradiction candidates, recalibration candidates.
- **`maintenance::capture_health`** — whether conversation capture is actually
  happening, counted by distinct `capture_id` so the dialog/summary pair one
  capture writes counts once rather than twice.
- **A throttled nudge** naming the three deepest backlogs and the prompt that
  drains each.

### Notes
- **The throttle slot is claimed before the counts run, not after.** The
  obvious ordering — count, then decide whether to emit — pays six `COUNT(*)`s
  on every search even when nothing is pending. Claiming first bounds the
  *work*, not just the output, so a quiet vault costs the same as a busy one.
  There is a test asserting the slot is claimed even when no notice is
  produced, because that is the case the natural implementation gets wrong.
- **Timers are keyed, not global.** Two independent advisories with different
  cadences competing for one slot means whichever fires first silences the
  other.
- **A queue whose query fails reports 0 rather than propagating.** These are
  status helpers; on a partially-migrated database, letting one make an
  advisory the thing that breaks a search is an absurd trade.
- Contradiction and recalibration counts go through
  `candidate_count`, which reuses each tool's own predicate — so the nudge
  cannot claim a backlog that draining it does not find.
- **`ever_captured` exists to make "never configured" visible.** A client where
  auto-capture was never set up is indistinguishable from one where it was but
  nothing was worth capturing — both are silent.
- Only the three deepest backlogs are named; equal counts break ties by key so
  a nudge does not reshuffle between calls and read as new information.

## 2026-08-04 — Readwise highlight import (#150)

### Added
- **`readwise` import kind** — turns a saved Readwise export into one memory
  per highlight. Accepts both the documented `{"count", "nextPageCursor",
  "results"}` object and a bare top-level array of the same entries.
- Both new import kinds are now advertised in the tool schema's `kind` enum
  (`auto`, `chat`, `document`, `obsidian`, `readwise`), with a test asserting
  every advertised string actually deserializes — the advertised-but-
  unreachable failure the tool cross-check exists to catch.

### Notes
- **This is a file import, not an API client.** My issue described an endpoint,
  auth header, pagination and `updated__gt`; the reference makes **no live call
  against Readwise at all**. The user exports once and hands over the file,
  which keeps an access token out of this crate entirely. Corrected on the
  issue before implementing.
- **One memory per highlight, not per book.** A highlight is Readwise's own
  atomic unit of meaning. Grouped, every search hit for one highlight would
  compete for ranking and embedding budget against every other highlight in the
  same book — diluting exactly the retrieval precision the store exists for.
  The book is not lost, only demoted: title, author, category, source URL and
  id ride as metadata on every highlight it produced.
- **A note is appended to content**, as `"{text}\n\nNote: {note}"`, never
  metadata-only. It is often *why* the highlight was made, and FTS indexes
  `content`, not `metadata`.
- **Deliberately excluded from `auto`.** A Readwise export and a chat export
  are both an unadorned `.json`. Sniffing for a `highlights` key would misroute
  a chat export that merely discusses Readwise — silently corrupting working
  chat-import behaviour, which is strictly worse than requiring one keyword.
- **Malformed parts are skipped; a malformed whole is refused.** A bad entry or
  highlight is dropped, matching the tolerance the chat connector shows a bad
  JSONL line. A file that is not a Readwise export fails the import outright
  rather than succeeding with zero memories — the silent-success mode #147
  fixed.
- Metadata is sparse: only keys actually present, so a reader can tell
  "Readwise did not have this" from "this is empty".

## 2026-08-04 — Obsidian vault import (#149)

### Added
- **`obsidian` import kind** — a vault note's own conventions, which a generic
  document import treats as opaque prose:
  - **YAML frontmatter**, parsed into fields, with `tags` folded into the
    memory's tags exactly like an inline `#tag`.
  - **`[[Wikilinks]]`**, each resolving to an **entity** for the linked note's
    title, with the mentioning memory linked to it — so the vault's own link
    graph becomes traversable through `remind_me_entity` /
    `remind_me_entity_traverse` instead of being flattened into text.
  - **Inline `#tag`**, deduplicated case-insensitively against the frontmatter
    ones.
- Its own `source` and `category`, so a search can filter specifically on
  notes ingested from a vault.

### Notes
- **This is the entity graph, not `wiki_links`.** The issue's original
  criteria said otherwise; `wiki_links` belongs to the separate LLM Wiki layer
  and is untouched. Corrected on the issue before implementing.
- **Chunking is not reimplemented** — the connector strips frontmatter and
  hands the body to the same per-section Markdown chunker the document kind
  uses. Only the per-note extraction is new.
- **Frontmatter parsing is hand-rolled and deliberately partial**: flat
  `key: value`, `key: [a, b]`, and `key:` + indented `- item`. Real vault
  frontmatter is overwhelmingly that shape, and a YAML crate is a large
  dependency for this bounded a need — the same call the reference made.
  Anything unrecognised degrades the whole block to "no fields" rather than
  erroring, and the delimiters are stripped either way, so the prose always
  imports.
- **Mentions attach per chunk**, not per note — only the wikilinks whose
  literal `[[…]]` text landed in a chunk. Smeared across the file, every
  section would claim every mention and the graph would say each section is
  about everything the note touches.
- **A note's tags merge with the caller's rather than replacing them.** The
  caller asked for these memories to be tagged a certain way; the note's own
  tags are additional information, not a correction.
- Four `#` shapes are deliberately **not** tags: `# Heading` (the space),
  `[[Note#Heading]]` (wikilink spans are masked), a `#` in fenced or inline
  code, and a purely numeric `#123`. `#2024/review` **is** a tag — only wholly
  numeric ones are dropped, or the commonest dated-note scheme would vanish.
- **v1 limitation, stated rather than hidden**: a heading or block anchor is
  stripped, so `[[Note#Heading]]` resolves to `Note` as a whole rather than
  forking a second entity.

## 2026-08-04 — Two defects found by reference movement (#147, #148)

Both come from `remind_me` commits landed past the pinned `935eb98`, and both
were present here as well.

### Fixed
- **Importing a Claude Code `.jsonl` session transcript created zero
  memories** (#147). Claude Code wraps each line in an envelope
  (`{"type": …, "message": {"role": …, "content": […]}}`), which matched none
  of `extract_messages`' branches, so extraction returned nothing. The import
  was still recorded as a **success** with zero memories created — nothing
  errored and nothing warned, so a whole session import looked exactly like
  importing an empty file.
- **Content blocks were flattened type-blind.** `text_of` took `block["text"]`
  from every object block regardless of `type`, so a `tool_use` block carrying
  a `text` field inside its input was imported as conversation, and blocks
  without a `text` key contributed blank lines. Now only `text` blocks are
  kept — plus blocks with **no `type` but a `text` key**, which older exports
  produce and which would otherwise be silently dropped.
- **`remind_me_server_status` kept reporting a sync error after sync had
  recovered** (#148). `SyncWorkerStatus.last_error` is one process's memory
  while the `sync_log` watermarks are shared, and the normal deployment runs
  one MCP server process per connected client against the same database. A
  process that failed while the hub was unreachable reported that error
  indefinitely, even as the same report showed the watermarks advancing.

### Notes
- The error is now compared against the watermarks, which only ever advance on
  success. A superseded error moves to `superseded_error` rather than being
  discarded — the evidence of what happened is worth keeping, the same reason
  `sync_repair` resets only the cursors.
- **Every** remote must have succeeded since the failed cycle, not just one.
  The error is cycle-level, so the remote that produced it is not identifiable
  from the message; requiring all of them is the only reading that cannot
  quietly declare a still-stuck remote healthy.
- Reporting fails **closed** in every ambiguous case — unparseable timestamp,
  missing timestamp, no remotes, or a remote still at the epoch default.
  Failing open would hide a real outage behind a formatting problem.

## 2026-08-04 — Tool profiles (#122)

### Added
- **`REMIND_ME_TOOL_PROFILE`** — `full` (default), `standard`, or `core`,
  narrowing which tools the MCP server advertises.
- `full` changes nothing, so upgrading is a no-op for existing installs.
  `standard` drops the admin/ops tier. `core` keeps only the conversational
  surface and hides the maintenance prompts with it.

### Notes
- **This buys context, nothing else.** It is not a fix for tool-selection
  accuracy: the tools that genuinely compete — `search`, `list`, `get`,
  `entity`, all of which read as "find things" — are every one of them in
  `core`, so no profile separates them.
- **Hidden means gone**: absent from `tools/list` *and* refused on
  `tools/call`, with the refusal naming the env var that restores it.
- **An unlisted tool defaults to admin/ops**, so a tool added later starts
  hidden under a narrowed profile rather than smuggling itself onto a trimmed
  surface.
- An unknown profile falls back to `full` — refusing to start over a
  misspelled optimisation would be worse than the misspelling.
- `remind_me_server_status` stays visible in every profile: it is what reports
  which profile is active.

## 2026-08-04 — Rate limiting on the webhook and remote MCP endpoints (#121)

### Added
- **A fixed-window, in-memory request limiter** (`remind_me_core::rate_limit`)
  applied to the webhook ingest endpoint and the remote MCP connector — the two
  surfaces exposed to the internet when tunnelled.
- Defaults match the reference: **60 requests per 60 seconds**, on by default,
  `REMIND_ME_RATE_LIMIT_ENABLED=""` to opt out. Over-limit returns **429** with
  a whole-second `Retry-After`.
- No new dependency: a map behind a `Mutex`.

### Notes
- **The limiter runs before authentication on both surfaces.** One that only
  engaged after a valid credential would leave an unauthenticated flood
  unbounded, which is the flood that matters here.
- `resolve_key` compares the secret in **constant time** even though it only
  picks a bucket — a fast-path `==` would make the limiter the timing oracle
  the auth check avoids being.
- **A rejected call does not extend the window it was rejected by**, so a
  client that backs off meets a clean window rather than one its own retries
  primed.
- One shared limiter across both endpoints: the limit is a property of the
  caller, not of the route.
- A correct secret shares one `auth:known` bucket; everyone else is bucketed by
  address, and an unknown address shares `ip:unknown` rather than bypassing the
  limit.
- **Single-process only** — counters are per process with no shared store, so
  two nodes behind one tunnel each enforce separately.
- Expired buckets are pruned lazily during ordinary calls, so a long-running
  server seeing many addresses does not leak.

## 2026-08-04 — Named, scope-limited API keys (#120)

### Added
- **`remind_me_api_key`** — issue, list and revoke named dashboard API keys
  with a `read` or `read-write` scope.
- Scope enforcement in the HTTP auth gate: a `read` key is refused **403** on
  every mutating method.
- `remind_me_core::api_keys` — a `0600` JSON store written by atomic replace,
  holding SHA-256 hashes only.

### Notes
- **Tool coverage: 60 → 61 of 61.** Every tool the reference exposes is now
  covered, plus the target-only `remind_me_wiki_import` (62 advertised).
- **The enforcement is the feature.** A read-scoped key that could still write
  would be worse than no feature at all, because it gets handed out on the
  understanding that it is safe — so the test checks every mutating route
  rather than one representative.
- **Not multi-tenancy.** Every key reads and writes the same vault; only the
  permitted methods differ.
- The plaintext is shown **exactly once**, at issuance. Only its hash is
  stored, so a key can be revoked and replaced but never recovered.
- **Unknown scopes fail closed** — a hand-edited or future-versioned file is
  treated as read-only, because the alternative grants write access on a typo.
- **A corrupt store authorises nothing** rather than everything.
- The store is **re-read on every request**, so a key revoked in another
  process stops working on the next call rather than at the next restart.
- Written by atomic replace with permissions tightened *before* the rename —
  between a world-readable create and a later chmod, the hashes are exposed.
- **401 and 403 stay distinct**: 401 means "I do not know this credential",
  403 means "I know it and it may not do that". Collapsing them sends a
  read-key holder hunting an auth problem they do not have.
- The flat `REMIND_ME_API_KEY` stays implicitly read-write, so adding scopes
  cannot retroactively restrict a deployment already relying on it.

## 2026-08-04 — Prometheus metrics and the PWA manifest (#119)

### Added
- **`GET /metrics`** — Prometheus text exposition, gated on
  `REMIND_ME_METRICS_ENABLED` and a plain 404 while off.
- **`GET /manifest.json`** — the dashboard's Web App Manifest.
- `remind_me_core::metrics` — counters for tool calls, their durations, search
  tiers and rate-limit rejections, plus `remind_me_build_info` and ad-hoc
  gauges computed per scrape.
- Tool-call timing wired into the MCP dispatch, so the tool families carry
  real samples rather than headers with nothing under them.

### Notes
- **404 while disabled, not 403 or an empty 200.** "Off" must be
  indistinguishable from "this build does not have it", so a scrape pointed at
  a metrics-disabled server fails loudly instead of silently recording nothing.
- **Unauthenticated, gated on the enable flag** — same posture as `/health`,
  and the reference's. Prometheus scrape configs typically send no headers.
  While enabled this reveals usage *patterns* (tool call counts, search volume,
  memory and outbox totals) to anyone who can reach the port; it exposes no
  memory content.
- **Gauges are computed per scrape, never shadowed as counters**, so
  `remind_me_memories_total` and `remind_me_sync_outbox_pending` cannot drift
  from the tables they describe.
- `remind_me_build_info` is emitted unconditionally, including on a server that
  has served nothing — an absent series reads as "scrape target down", not
  "idle".
- Search tiers are zero-filled, so a dashboard query renders a flat line at
  zero rather than a gap.
- HTTP routes: 23 → **25 of 25**.

## 2026-08-04 — Reminders calendar feed (#118)

### Added
- **`GET /api/reminders/{token}.ics`** — a subscribable RFC 5545 calendar feed
  carrying the same `all` window `remind_me_list_reminders` returns.
- **`remind_me_reminders_ics_url`** — hands back the subscribe URL, or the feed
  path plus how to start the dashboard when there is no HTTP surface yet.
- `remind_me_core::ics` — dependency-free ICS generation with RFC 5545 line
  folding (§3.1) and TEXT escaping (§3.3.11), plus the feed token's
  resolution, `0600` persistence, and rotation.

### Notes
- **The token in the path is the whole credential.** The route is exempt from
  the `Authorization` gate every other `/api/*` route sits behind, because a
  calendar app polls this URL from its provider's servers with no way to
  attach a header. Nothing else is exempt, and the near-misses are tested.
- A wrong token gets a **bare 404**, not a 401 — a 401 confirms the route
  exists and a token was checked. Compared with `constant_time_eq`. Never
  logged.
- **No "disabled" opt-out**, unlike the API key: the token *is* the path, so
  falling open would publish every reminder. An unwritable token file yields
  an ephemeral per-process token rather than none.
- **UIDs are deterministic** (`{memory_id}-{remind_at}@remind-me`). A
  subscribing calendar re-fetches on its own schedule, and a random UID would
  create a duplicate event on every poll. Rescheduling mints a new UID,
  because that is a different occurrence.
- Folding splits on UTF-8 character boundaries, so non-ASCII reminders survive
  the round trip. Escaping matters more than it looks: an unescaped comma
  corrupts every VEVENT *after* the one containing it.
- **Tool coverage: 59 → 60 of 61.** HTTP routes 22 → 23 of 25.
  (An earlier version of this entry said 61 of 61. That counted the
  target-only `remind_me_wiki_import` toward the reference's 61;
  `remind_me_api_key` was still missing and lands in #120.)

### Known limitations
- **Rotation, not revocation** — matching the reference. Deleting the token
  file mints a fresh token and invalidates every existing subscription at
  once; there is no way to revoke one calendar's access alone.
- **No all-day events.** The reference emits only timed `DTSTART` values in
  UTC and `remind_at` is a timestamp, so an all-day reminder is not
  representable upstream either.

## 2026-08-04 — Reminder delivery scheduler and webhook notifications (#117)

### Added
- **Reminder scheduler** (`remind_me_core::scheduler`) — a background loop that
  delivers every due, not-yet-delivered reminder exactly once, every
  `REMIND_ME_REMINDER_POLL_INTERVAL` seconds (default 60). Started with
  `start_scheduler(db_path)`, stopped with a handle that joins.
- **Webhook notification channel** (`remind_me_core::notifications`) — POSTs
  `{"subject", "body", "source": "remind-me"}` to
  `REMIND_ME_NOTIFY_WEBHOOK_URL`, timeout `REMIND_ME_NOTIFY_WEBHOOK_TIMEOUT`
  (default 5s). The URL being set *is* the opt-in; there is no separate switch.
- `sync::http` gained a default port 80 and unauthenticated POST, both
  additive — `post_json`/`get` still send the bearer token unchanged.

### Notes
- **Exactly-once comes from `reminder_deliveries`, not from a past-timestamp
  check.** A bare "`remind_at` is in the past" test cannot remember that it
  already fired, leaving only two behaviours, both wrong: deliver forever, or
  skip anything already past and lose it. The delivery table is what makes the
  third possible — a reminder that came due while nothing was running fires
  once on the next pass, then never again. Confirmed against the reference,
  whose due query has no lower bound.
- **A failed channel does not hold the reminder back.** The delivery row is
  written whether or not a webhook accepted it, matching the reference. The log
  line is the channel that cannot fail; retrying would re-log the same reminder
  every 60 seconds for as long as the webhook stayed down. The issue's
  acceptance criteria said "retried per the reference" — the reference does not
  retry.
- **With no channel configured a reminder is still logged and still recorded.**
  Opt-in governs whether a *notification* is attempted, not whether the
  reminder is handled.
- The delivery row is written **after** the hook, so a hook that panics leaves
  the reminder pending. `INSERT OR IGNORE` keeps the unique index as the real
  guarantee, so racing pollers produce one delivery rather than an error that
  strands the rest of the batch.
- The scheduler reads the same overdue window `remind_me_list_reminders` shows,
  so a reminder cannot sit visibly overdue while the loop considers it handled.

### Known limitations
- **The webhook is `http://` only.** This crate's HTTP client has no TLS, the
  same choice `sync::http` and `embedder` already made. That is a sharper
  constraint here: the endpoints people actually configure (Slack, Discord,
  ntfy) are public HTTPS, so this channel needs a local relay in front to be
  useful. Reaching them directly needs a TLS-capable client.
- **No SMTP channel.** The reference gets it free from `smtplib`; Rust needs
  `lettre` plus a TLS backend. Both this and TLS are dependency decisions left
  for their own issue.
- The loop carries reminders only. The reference also piggybacks scheduled
  digests, watched-search polling, revision compaction and analytics capture on
  the same thread — a follow-up.

## 2026-08-03 — Reminders: set, clear and list (#116)

### Added
- **`remind_me_set_reminder`** — sets `memories.remind_at` on an existing
  memory, or clears it. Omitted, null and blank all mean "clear".
- **`remind_me_list_reminders`** — memories with a reminder, soonest first, in
  one of three windows: `upcoming` (still in the future), `overdue` (came due
  with no matching `reminder_deliveries` row), `all`. Both `markdown` and
  `json` response formats.
- `Memory` now carries `remind_at` and `sensitive`, so every memory-returning
  tool reports both. The markdown renderer mirrors the reference's
  `_fmt_memory_md`, lock marker and 2000-char truncation included.
- The digest's **Reminders** and **Sync health** sections are now populated.
  Both were omitted while their subsystems did not exist; #114 and #116 supply
  them, and each reads from the exact function its own tool calls, so the
  digest cannot disagree with `remind_me_list_reminders` or
  `remind_me_sync_status`.

### Notes
- **`remind_at` is applied on the receiving side of sync — the reference drops
  it.** `sync.py`'s `_upsert_one` writes neither `remind_at` nor `sensitive`
  even though its own outbox trigger emits both, so the reference ships a
  payload carrying two fields nobody reads back. A reminder is a property of
  the memory, not of the machine holding it. Deliberate divergence, same call
  as `sensitive` under #105, tested end-to-end in both directions.
- `reminder_deliveries` stays local, so a reminder delivered on one node is
  still pending on another and fires there too. Being told twice on two
  machines beats being told on neither because the machine that fired it was
  the one you weren't at.
- **A past `remind_at` is rejected, not stored.** Stored, it would land in the
  overdue bucket — the one meaning "nothing was running when this came due" —
  making a typo indistinguishable from a genuine missed delivery.
- Delivery is keyed on `(memory_id, remind_at)`, so rescheduling a delivered
  reminder makes it pending again rather than staying suppressed forever.
- Setting a reminder writes **no revision**: the revision log exists to recover
  a value a human replaced, not to record scheduling.
- Naive timestamps are read as UTC and stored canonicalized, because
  `remind_at` is string-compared against a UTC `now` in every window query.
- Tool coverage: 58 → **60 of 61**.

## 2026-08-03 — remind_me_sync_reconcile and _peer (#115)

### Added
- **`remind_me_sync_reconcile`** and **`remind_me_sync_reconcile_peer`** — diff
  this node's record counts against the hub's or a discovered peer's, and
  classify the drift. Both read the `/count` endpoint #113 added.
- Four verdicts: `in-sync`; `pull-lag` (remote ahead, pull recent — the
  ordinary state between cycles); **`node-ahead`** (this node holds records the
  remote does not, so pushes are not landing — the only direction that means
  data is at risk); and `fault` (remote ahead but the pull is stale or never
  ran). Each carries hints explaining what to check.

### Notes
- **One classifier serves both remote kinds.** "Local greater than remote means
  pushes are not landing" does not depend on which machine is on the other end.
- `node-ahead` is checked first and unconditionally. Mixed drift is where
  reading numbers by eye goes wrong: a remote ahead by 38 somewhere is loud,
  and the 3 records at risk are quiet.
- Lag is judged from `last_pull_at` — the wall clock of the last successful
  pull, not the content cursor — for the same reason #114 reports liveness that
  way: a quiet-but-healthy remote advances it every cycle.
- An unreachable remote returns `unavailable` with the reason, not a verdict. A
  verdict computed against counts that could not be fetched would be a guess.
- Tool coverage: 56 → **58 of 61**. **Wave 5 complete.**

## 2026-08-03 — remind_me_sync_status and remind_me_sync_repair (#114)

### Added
- **`remind_me_sync_status`** — outbox depth with a drain verdict
  (`draining`/`stalled`/`growing`/`idle`/`unknown`), tombstone counts split by
  what is compactable now, and per-remote contact state. When sync is off it
  names the specific missing environment variables rather than just saying so.
- **`remind_me_sync_repair`** — resets a remote's pull cursors so the next sync
  re-pulls history.

### Notes
- **Liveness is read from `last_attempt_at`/`last_push_at`/`last_pull_at`,
  never from the `last_pull`/`last_push` cursors.** This is what the v20
  columns exist for: a quiet-but-healthy remote advances its contact clocks
  every cycle while its cursors stand still, so reading liveness off the
  cursors reports that remote as stalled — and reports a genuinely wedged one
  as fine the moment anything happens to move.
- A never-contacted remote sits at the epoch default rather than NULL, so
  "never tried" is recognised by value and stays distinguishable from "tried
  and failing".
- **Repair touches only the cursors.** The contact timestamps record what
  actually happened; rewriting them to force a re-pull would destroy the
  evidence you were reading when you decided to repair.
- The drain verdict reports direction from the delta alone and a per-minute
  rate only when measurable time has passed — gating the verdict on elapsed
  time made back-to-back calls report `unknown` for a backlog that had
  visibly not moved.
- Tool coverage: 54 → **56 of 61**.

## 2026-08-03 — GET /count on the peer server (#113)

### Added
- **`GET /count`** on the sync peer server — record counts, field-for-field
  the shape the hub returns, so one client-side comparator serves both
  remotes. This is the pre-check that makes `remind_me_sync_reconcile` cheap:
  diff counts before pulling anything.
- **`version` on the peer server's `/health`**, which is where a reconcile
  reads the other side's build from.

### Notes
- **The issue's `approx=1`, `since=` and `by=origin_node` parameters belong to
  the hub's `/count`, not the peer server's.** The reference's peer endpoint
  takes no query parameters and always reports `"approximate": false` — a peer
  has no planner estimates to offer, since `?approx=1` is a Postgres
  `reltuples` read and the hub is a Postgres service (E1, out of scope). The
  field is still present rather than omitted, so a caller need not know which
  kind of remote it is talking to.
- **Counts do not filter `deleted_at`.** Both ends of a reconcile must count
  identically; the hub counts every row and reports tombstones separately, so
  filtering here would make a healthy peer look permanently behind by its own
  tombstone count.
- Peer-server route coverage: 6 → **7 of 7**.

## 2026-08-03 — Analytics snapshots and GET /api/analytics/trend (#112)

### Added
- **Daily analytics snapshots** — total memories, vitality buckets and
  category counts, recorded at most once per calendar day. `remind_me_stats`
  and the vitality report answer "what does the vault look like now"; nothing
  could answer "is it getting better or worse", because nothing was recorded to
  compare against.
- **`GET /api/analytics/trend`** — the full series, oldest first, which is what
  the re-copied dashboard (#107) plots. That fetch had been 404ing since #107;
  it now returns data.

### Notes
- **The route captures on read.** The reference captures from its scheduler;
  this crate has no always-on loop yet, so a scheduler-only capture would leave
  the chart permanently empty on exactly the installs most likely to open it.
  Safe only because capture is idempotent per calendar day — there is a route
  test hitting the endpoint three times and asserting one data point.
- Comparison is by **date**, not timestamp: a server restarted three times in a
  day would otherwise show three points and read as a spike that never
  happened.
- A malformed stored value degrades to an empty map rather than failing the
  read — one bad row should not blank the chart.
- HTTP route coverage: 21 → **22 of 25**. **Wave 4 complete.**

## 2026-08-03 — remind_me_digest (#111)

### Added
- **`remind_me_digest`** — memories added in a window plus current vitality,
  in Markdown or JSON. The listing is capped at 20 but the true count is
  carried separately, so a busy week reads as "20 of 340" rather than silently
  looking like a quiet one.

### Notes
- **No `include_sensitive`, deliberately.** Unlike search and list, a digest is
  the ambient, often-scheduled surface the flag exists to protect, and it has
  no per-call caller intent to opt back in against. The exclusion covers the
  count as well as the list.
- **Two of the reference's five sections are omitted rather than stubbed.**
  Reminders and sync status read from subsystems that do not exist yet (#116,
  #114). A "Reminders: none" line would read as "you have nothing due" when the
  truth is "nothing here can tell", so the sections are absent from both the
  Markdown and the JSON until their subsystems land.
- An empty window says so explicitly — "nothing new this week" is information;
  a blank section reads as a bug.
- Tool coverage: 53 → **54 of 61**.

## 2026-08-03 — remind_me_contradiction_candidates (#110)

### Added
- **`remind_me_contradiction_candidates`** — surfaces pairs of memories that
  might assert incompatible things but were never caught by exact-triple
  supersession: two pieces of prose that conflict without either carrying a
  formal subject/predicate/object. Read-only, with no apply half — a confirmed
  contradiction is fixed with the existing `remind_me_update`,
  `remind_me_delete`, or an `remind_me_add` carrying an explicit triple.

### Notes
- **The entity fan-out cap is included, not deferred.** Entities mentioned by
  more than 20 memories are excluded from the pairing join on both sides. The
  join is quadratic in an entity's mention count, and on the reference author's
  vault a single 745-mention project entity produced 277,140 of 372,750
  candidates — 74% of the queue — before the cap existed. This is invisible on
  a small vault and decisive on a real one, which is why it ships with the tool
  rather than after it.
- The exclusion of pairs the triple mechanism already covers is subtler than it
  looks: a pair sharing a subject and predicate but differing in object cannot
  be observed here at all, because the write path would have superseded the
  first the moment the second landed. So the exclusion filters out same-object
  verbatim restatements, not genuine conflicts.
- Tool coverage: 52 → **53 of 61**.

## 2026-08-03 — Edit history and revert (#109)

### Added
- **`remind_me_history`** — a memory's revisions, newest first. Each is a
  snapshot of content, category, tags, metadata and the sensitive flag as they
  were *before* an edit replaced them.
- **`remind_me_revert`** — restores all five together. Reverting is itself an
  edit: it records a revision of the state just before the revert, so a
  mistaken revert is recoverable.

### Notes
- **Revisions are written from the update path alone.** The issue warned that
  "a revision row has to be written by every existing mutation path" and listed
  seven, asking for the list to be audited against the reference. The audit
  answer is one: the reference records from `_apply_memory_field_update` only.
  Reclassify, normalize, annotate, consolidate and decompose record nothing,
  and that is followed here — a revision exists to recover a value a *human*
  replaced, and recording the derived-data paths would bury those under
  machine-generated noise. Pinned by tests in both directions.
- Access tracking never produces a revision: it writes `accessed_at` and no
  tracked column. Without that, a vault would accumulate a revision per read —
  the same shape of bug #100 fixed in the sync outbox.
- A same-value update records nothing, mirroring the outbox trigger's "only on
  genuine change" discipline.
- Reverting to the state a memory already holds reports `no_change` rather than
  writing a no-op revision and an outbox row that says nothing changed.
- Tool coverage: 50 → **52 of 61**.

## 2026-08-03 — Saved and watched searches (#108)

### Added
- **Four tools** — `remind_me_save_search`, `remind_me_list_saved_searches`,
  `remind_me_run_saved_search`, `remind_me_delete_saved_search`. A saved search
  stores a query plus its filters under a unique name; re-saving the same name
  updates it in place, matching the `UNIQUE` constraint and
  `remind_me_wiki_write`'s "one name is one logical thing" convention.
- **Watch polling** — `saved_searches::poll_saved_search` records current
  matches and reports the ones not seen before. The first poll **seeds**:
  turning on `watch` for a search that already matches a hundred memories must
  not read as a hundred new matches.
- Deleting a saved search removes its `saved_search_seen_memories` rows, which
  are unreachable dead weight the moment the parent row goes.

### Notes
- **Running a saved search returns all its matches, watched or not.** Only
  polling diffs. This matches the reference and contradicts the issue's own
  wording — a caller asking for a saved search's results must not get a partial
  list because something polled it earlier.
- `poll_saved_search` returns the new matches rather than dispatching
  notifications; the transport is the scheduler's half (#117). This keeps the
  diff logic complete and testable rather than shipping `watch` inert.
- **`MemorySearchInput` now has a hand-written `Default`.** A derived one gives
  `limit: 0`/`token_budget: 0` — a search that structurally cannot return
  anything. The hand-written impl matches what serde supplies for absent keys.
- Tool coverage: 46 → **50 of 61**.

## 2026-08-03 — GET /api/versions, and the build on /health (#107)

### Added
- **`GET /api/versions`** — reports this node's build, its `node_id`,
  whether sync is configured, and the hub's build when reachable. Auth-gated
  by the `/api/` prefix: this node's build is its own to publish, the hub's is
  another machine's.
- **`version` on `GET /health`**, which is unauthenticated. Not in the issue's
  criteria, but the dashboard header cannot work without it — the reference
  reads the *node's* version from `/health` and only the *hub's* from
  `/api/versions`, because `/health` still answers when the API key is wrong or
  missing, and that is exactly when you want to know which build you are
  talking to.
- **`sync::probe_hub_version`** — a cached, best-effort probe of the hub's
  `/health`. An unreachable hub yields `null` rather than an error, so the
  dashboard omits a line instead of rendering a failure into its own chrome.

### Changed
- **`dashboard/App.jsx` re-copied verbatim from the reference** (it was 114
  lines behind), which is what renders the version in the header. The file is
  vendored under the same convention as the generated `schema_*.sql` —
  regenerate, never patch. The newer copy also calls `/api/analytics/trend`,
  which does not exist here until #112; that fetch is individually caught, so
  the trend panel renders empty rather than breaking the page.

### Fixed
- **Gap A6 was a false positive and is withdrawn.** There is no target-only
  `/api` route: the row came from an extraction that grepped `"/api"` string
  literals and matched a doc comment. `ROUTES` has 20 patterns, all of which
  the reference serves too. The gap analysis's headline row now reads 20
  shared rather than "21 (20 shared + 1 target-only)".

### Notes
- HTTP route coverage: 20 → **21 of 25**.

## 2026-08-03 — Per-call retrieval strategy (#106)

### Added
- **`MemorySearchInput.strategy`**, defaulting to `auto`, with the reference's
  four values (`auto`, `balanced`, `keyword_favored`, `semantic_favored`) and a
  matching MCP tool-schema property. Previously the RRF weight profile was
  reachable only through environment variables, with `search_memories`
  hardcoding `Auto`.

### Notes
- **Nearly all of this already existed.** The enum, the three multiplier
  presets, the `Auto` router and its shape heuristics were all in
  `retrieval.rs` — only the field and one line threading it through were
  missing. The rest of the work was coverage: none of that logic was reachable
  from the test suite, and now 12 tests pin it, including the four `Auto`
  branches and the keyword-wins-over-semantic precedence that an `if`/`else if`
  reordering would silently flip.
- **Precedence against the environment:** `REMIND_ME_RRF_W_*` sets the
  baseline, the strategy scales it. `Balanced` is the identity multiplier, so
  it reproduces the configured baseline exactly rather than resetting to
  built-in defaults — which is what lets an operator zero a signal and have it
  stay zeroed under every strategy.

## 2026-08-03 — The sensitive-memory flag (#105)

### Added
- **`MemoryAddInput.sensitive`**, **`MemorySearchInput.include_sensitive`** and
  **`MemoryListInput.include_sensitive`**, all defaulting to `false`, plus the
  matching MCP tool-schema properties and dashboard `?include_sensitive=1`
  support. A sensitive memory stays out of ordinary search and list results
  unless asked for.
- **`MemoryUpdateInput.sensitive: Option<bool>`** — set or clear the flag after
  creation. Not named by the issue; added because the reference has it
  (`models.py:382`) and because without it a memory marked at creation could
  never be unmarked. `Option` rather than `bool` so an update that does not
  mention the flag cannot silently clear it.
- **`SyncRecord.sensitive`**, so the flag survives a sync. #101 put it into the
  outbox payload (the sending half); without this the receiving peer stored the
  memory unmarked and it surfaced in that node's ordinary search — the flag
  defeated by the first sync rather than by anything visible locally. Same
  reasoning ADR-0007 records for `deleted_at`. `#[serde(default)]`, so a record
  from a pre-v27 peer parses as not-sensitive instead of failing the pull, and
  a custom deserializer that accepts SQLite's integer booleans: the outbox
  trigger's `json_object` emits `0`/`1`, and serde will not read an integer
  into a `bool`. Without it **every memory record in a push batch failed to
  deserialise** — the receiver counted them as failures, `push_outbox` reported
  `pushed: 0`, and nothing looked wrong on the sending node. Sync stopped
  working entirely.

### Notes
- **This is not access control.** It is a single-user store: anyone who can read
  the database file reads every memory in it, marked or not. The flag is a
  "don't surface by default" convenience and nothing more. Said plainly in the
  tool schema, the model doc comments, and the test module docs.
- Filtering is applied in SQL, not after the query, so `COUNT`, `LIMIT` and
  `OFFSET` agree — a total that counted hidden rows would make pagination skip
  a page.
- The search filter covers **both** halves of the RRF fusion. Filtering only the
  keyword SQL would leave a sensitive memory able to arrive through the semantic
  half and rank into the output.
- Adding three fields to widely-constructed input structs touched 80 struct
  literals across 34 files. That part of the diff is mechanical.

## 2026-08-03 — The serving build is reported in server status (#104)

### Added
- **`remind_me_server_status` now reports `version`**, the build actually
  serving the request (`updater::INSTALLED_VERSION`, i.e. `CARGO_PKG_VERSION`).
  A stale install after a failed self-update explains more odd behaviour than
  anything else that report covers, and it is the one fact a calling session
  has no other way to observe. Purely additive — no existing field changes.

### Notes
- **Two of the issue's three acceptance criteria did not match the reference**
  and were not implemented here. Recorded on the issue rather than resolved
  silently:
  - *"Installed version present in **both** tools' output"* — the reference
    reports it in `remind_me_server_status` only (via `pid.py`'s
    `get_server_status`). `remind_me_stats` (`admin.py:408`) contains no
    version field at all, so adding one would be a divergence, not parity.
    The gap analysis's own T10 row said "both"; that was inaccurate and is
    corrected there too.
  - *"Peer version captured during the existing `/health` probe"* — in the
    reference this surfaces through `remind_me_sync_status`, which this crate
    does not have yet. It belongs to #114, which builds that tool.
- Tool coverage unchanged at 46 of 61 — this deepens an existing tool rather
  than adding one.

## 2026-08-03 — remind_me_undo_import (#103)

### Added
- **`remind_me_undo_import`** — rolls back a previous import, removing its
  memories and its tracking rows. Imports are the one bulk write this crate
  makes, and `remind_me_delete` takes a single id, which is unusable at import
  scale. Handles all three ledgers: `chat` (scoped by `chat_imports.import_id`,
  resolved through `memories.doc_id`), `dbs` (by `dbs_source` prefix), and
  `mempalace` (by `drawer_id` prefix).
- **Dry run by default.** Bulk deletion that propagates over sync is opt-in,
  not opt-out. Resumable via `limit` — call until `remaining` reaches 0.
- On a sync-enabled node this soft-deletes, so the removal propagates instead
  of resurrecting on the next pull; the reported `mode` says which happened and
  that disk is not reclaimed until compaction.

### Notes
- Deletion routes through the existing `db::queries::delete_memory` rather than
  a bulk `DELETE`, so chunk vectors, entity links, feedback and associations are
  cleaned up rather than orphaned. Orphaned `vec_chunks` rows are the dangerous
  case: SQLite reuses freed rowids, so a later memory could silently inherit
  another's vectors.
- Tracking rows must go too — every import path treats a tracked id as already
  done, so leaving them would make the same content permanently un-importable.
  `chat_imports` rows are per-file, so one is dropped only once none of its
  chunks survive; a partially-drained import keeps its row and cannot be
  duplicated by a re-import.
- The mempalace resolver unions the tracking table with the `source`/
  `metadata.mempalace_drawer_id` signals, so an undo covers content that
  predates the ledger instead of silently leaving half the batch behind.
- Tool coverage: 45 → 46 of 61.

## 2026-08-03 — remind_me_recalibrate_candidates (#102)

### Added
- **`remind_me_recalibrate_candidates`** — surfaces memories whose importance
  classification may have gone stale: they look important (`base_weight >=
  1.15`, or a durable `memory_type` of `decision`/`fact`), have gone 90+ days
  without access, and have never received feedback. `vitality.rs` seeds
  `base_weight` at write time and adjusts it from explicit feedback, but
  nothing re-examined whether the original classification still held — a
  "decision" later reversed, or a "fact" superseded in spirit but never through
  the formal triple path, kept the importance it was born with.
- Read-only, and deliberately with **no apply half**: `remind_me_reclassify`/
  `_batch` already change `memory_type` (and the `decay_rate` that follows),
  and `remind_me_feedback` already nudges `base_weight` alone. A third writer
  would duplicate both. Matches the reference's own reasoning.

### Notes
- The issue's third acceptance criterion asked for `markdown` and `json`
  `response_format` variants. The reference's `RecalibrateCandidatesInput`
  carries only `limit` and always returns JSON, so adding the field would have
  created exactly the signature divergence this port exists to remove. Built to
  match the reference; noted on the issue rather than silently either way.
- Tool coverage: 44 → 45 of 61.

## 2026-08-03 — Schema regenerated at remind_me v27 (#101)

### Changed
- **`SCHEMA_VERSION` 19 → 27**, with `schema_tables.sql`, `schema_indexes.sql`
  and `schema_triggers.sql` regenerated from `remind_me` v1.54.0. **This is a
  breaking change**: it alters what schema a fresh database is created with.
  Existing databases reconcile on open with rows preserved, and #95's
  pre-migration snapshot fires for the transition (now asserted by a test).
- Previously a v27 database opened by this crate hit the reconciler with 5
  tables and 2 `memories` columns it did not know about. Interop now works in
  both directions rather than only one.

### Added
- **5 tables** — `analytics_snapshots`, `memory_revisions`,
  `reminder_deliveries`, `saved_searches`, `saved_search_seen_memories`.
- **6 indexes** — including `idx_memories_remind_at` and
  `idx_memories_normalized_from`.
- **5 columns** — `memories.remind_at` (v23), `memories.sensitive` (v26),
  `sync_log.last_pull_at`/`.last_push_at`/`.last_attempt_at` (v20). The last
  three split the sync cursor from the liveness clock, so a stalled peer is
  finally distinguishable from an idle one.
- **`remind_at` and `sensitive` in the `memories_outbox_ai`/`_au` payloads.**
  A synced peer rebuilds a memory from the payload alone, so a column missing
  from it is dropped in transit — a failure that is invisible locally and only
  shows up as data loss on the other node.
- **`scripts/regenerate_schema.py`** — ADR-0007's generation method, which was
  previously prose performed by hand, as a repeatable script. It refuses to
  write anything if the generated `user_version` disagrees with the
  reference's `_SCHEMA_VERSION`, so a partial migration ladder cannot produce
  a mislabelled dump.

### Fixed
- **`ARCHITECTURE.md` §5 no longer reproduces the schema DDL.** The inline copy
  had gone stale — it still showed `last_accessed_at`, an `entities` table with
  no `node_id`, cascading foreign keys on `memory_entities`, and a
  `wiki_pages.topic` column, four shapes the schema tests assert are wrong. It
  now points at the generated files and the tests that police them. Tenet 3 and
  the §5 heading both read Version 27.

### Notes
- Nothing was removed by the regeneration. The four graph outbox triggers show
  as changed but differ only in line wrapping.
- Issue #100's hand-added `memories_outbox_au` guard came back identical in the
  v27 dump, so that annotated exception erased itself as predicted. A test now
  pins it rather than trusting the reasoning.
- This unblocks the downstream schema-dependent work: saved searches (#108),
  history/revert (#109), analytics (#112), reminders (#116–#118), and the
  `sensitive` tool fields (#105).

## 2026-08-03 — The sync outbox no longer records every read (#100)

### Fixed
- **`memories_outbox_au` fired on every `UPDATE`**, and this crate records
  access on read (`accessed_at`/`access_count`, PR #42) — so on a node with
  sync configured, a 20-result search enqueued 20 full-payload `sync_outbox`
  rows and pushed 20 no-op updates to every peer. The trigger now carries the
  reference's own second condition, `AND NEW.updated_at IS NOT OLD.updated_at`
  (`db.py`'s `_migrate_v21_to_v22`), so only genuine content changes enter the
  outbox. Nothing that previously synced stops syncing: LWW on the receiving
  side compares `updated_at`, so a row whose `updated_at` did not advance was
  already being discarded on arrival — this removes the round trip, not the
  effect.
- **A changed trigger body never reached an existing database.** Every
  statement in `schema_triggers.sql` is `CREATE TRIGGER IF NOT EXISTS`, and
  reconciliation compared tables only, so a vault created before this change
  would have kept the old trigger forever and gone on flooding its own outbox.
  `db::migrations::reconcile_triggers` now diffs each trigger's stored DDL
  against the generated one and drops the ones that differ, immediately before
  the create pass rebuilds them. This is also the mechanism the schema-v27
  work (#101) will need to get its outbox payload changes onto existing
  databases.

### Notes
- `schema_triggers.sql` is generated verbatim from a `remind_me` dump and
  normally not hand-edited. This guard is one deliberate, annotated exception:
  it is forward-ported from the reference's v22 rather than waiting for the v27
  regeneration (#101), which is a breaking change gated on a separate decision.
  Regenerating at v27 reinstates the identical line, so the exception erases
  itself rather than needing to be reapplied. See ADR-0007's 2026-08-03
  addendum for why this does not reopen the hand-transcription problem that
  ADR rejected.

## 2026-08-01 — Embedding-model versioning and auto-clear on mismatch (#96)

### Added
- **`embedding_meta` is now read and written.** The table already existed in
  the generated schema (schema-parity boilerplate, unused until now) but
  nothing in this crate ever touched it. `crate::vectors::embed_and_store`
  now records the backend/model/dimension that produced a memory's vectors
  (`Embedder::identity()`, a new trait method) after every successful write,
  matching the reference's own "recorded after a successful (re-)embed, not
  merely inferred from config" behavior.
- **A changed `REMIND_ME_EMBEDDING_BACKEND`/`REMIND_ME_OLLAMA_EMBED_MODEL`/
  `REMIND_ME_EMBEDDING_DIM` is detected at startup** (`db::schema::initialize_schema`,
  the same "check at every open" timing the reference uses) and clears the
  now-invalid `vec_embeddings`/`vec_chunks` rows automatically, so a
  forgotten `remind_me_reindex` after switching models can no longer leave
  semantic search silently scoring vectors from a different embedding space.
  A first-ever run with no prior `embedding_meta` state is deliberately a
  no-op — nothing recorded yet means nothing to have changed away from.
- See `docs/adr/0002-embeddings-ollama-and-brute-force-vectors.md`'s new
  addendum for how the reference's `memories_vec`-recreation-on-mismatch and
  ANN-index-invalidation steps were adapted to this crate's own
  `vec_embeddings` table and its lack of an ANN index.
## 2026-08-01 — Pre-migration snapshot guard (#95)

### Added
- **Schema reconciliation now snapshots the database before it changes
  anything**, closing the gap where a bad migration against the single
  SQLite file holding someone's memory store had no way back. Matches the
  reference's `_maybe_snapshot_before_migration`: triggered only when
  reconciliation actually has pending work to do (the on-disk version stamp
  is behind `SCHEMA_VERSION`, or a table already present differs from the
  generated schema — a rename or added column included, since either changes
  the table's stored DDL), skipped for a brand-new database with no rows in
  `memories` yet, and non-fatal on failure: a snapshot that can't be created
  (e.g. the `backups/` directory can't be written) is swallowed rather than
  blocking the migration it exists to protect against. Reuses
  `crates/remind_me_core/src/backup.rs`'s existing `create_backup` — no new
  backup logic. New logic lives in
  `crates/remind_me_core/src/db/migrations.rs`
  (`migration_pending`/`has_existing_data`/`snapshot_before_migration`),
  called from `apply()` before any table is touched.
- Tests: `crates/remind_me_core/tests/migration_snapshot_test.rs` — a
  brand-new database takes no snapshot, an up-to-date database with data
  takes no snapshot on reopen, a legacy-shaped database with data is
  snapshotted (and one with no rows is not), an old version stamp is
  snapshotted under a label reflecting that version, and a snapshot failure
  does not block the migration.

## 2026-08-01 — Query-contextual feedback is applied at search time (#94)

### Fixed
- **`remind_me_feedback`'s query-contextual mode was write-only.**
  `record_feedback` already stored `memory_feedback` rows with the query
  that prompted them, but nothing ever read them back — a down-voted
  result for a specific query came back unchanged on a repeat of that same
  query. Global `base_weight` demotion (the other feedback mode) was
  unaffected and worked correctly the whole time.

### Added
- **`crates/remind_me_core/src/vitality.rs`**: `contextual_feedback_adjustment`
  and `apply_feedback_adjustment`, porting the reference's
  `vitality.py` functions of the same name. Per candidate, Jaccard
  similarity between the current query and each stored feedback query is
  computed; matches below `FEEDBACK_SIMILARITY_THRESHOLD` (0.3) are
  ignored, matches at or above it contribute `±magnitude * similarity`
  (helpful/unhelpful), summed and clamped to `±FEEDBACK_ADJUSTMENT_CAP`
  (0.4).
- **`crates/remind_me_core/src/db/queries.rs::search_memories`** now calls
  `apply_feedback_adjustment` right after RRF fusion and before truncating
  to `limit`, applying the adjustment multiplicatively to `score` and
  re-sorting — the same pipeline position the reference uses.
- **`MemorySearchResult` gained `feedback_adjustment: Option<f64>`**,
  `None` unless an adjustment was actually applied, so callers can see when
  and how much a result's ranking was nudged.
- Tests: 21 new cases in `crates/remind_me_core/tests/feedback_test.rs`
  covering the similarity/threshold/cap arithmetic directly, ranking
  reorder (a lower-ranked result promoted above a higher one), and an
  end-to-end case through `search_memories` confirming a real query's score
  moves after feedback is recorded.

## 2026-07-30 — Live `dashboard`/`embeddings` status, dashboard PID-file liveness (#90)

### Fixed
- **`remind_me_server_status` no longer hardcodes `dashboard` and
  `embeddings` as permanently missing.** Both subsystems are real elsewhere
  in this workspace (`remind_me_api`'s dashboard, `remind_me_core`'s Ollama
  embedding backend) but the status tool had never been wired up to see
  them, so it told operators semantic search and the dashboard didn't exist
  even when both were actively running.

### Added
- **`embeddings` status reflects real config and reachability.**
  `crates/remind_me_core/src/status.rs`'s `server_status` now reports
  `Active`/`NotImplemented` from `embedder::resolve_embedder()` (config
  only, no network call — this module's own stated contract). The MCP
  dispatch layer (`remind_me_mcp/src/lib.rs`'s `remind_me_server_status`
  arm) overrides that with the new `embedder::embedding_status()`, which
  adds the live, cached "and reachable" probe via `available_embedder()` —
  matching the reference's own `_get_embedder()` check.
- **`crates/remind_me_core/src/pid.rs`**: a PID-file liveness mechanism for
  the dashboard, porting the reference's `remind_me_mcp/pid.py`. A JSON PID
  file (`server.pid`, beside the database file) is written by
  `rusty-remind-me api` on start and read cross-process by the MCP server;
  liveness is proven by parsing the file *and* a `GET {url}/health` probe
  (2s timeout) succeeding — a PID file whose health check fails is treated
  as stale and removed, same outcome as the reference's `os.kill(pid, 0)`
  staleness check without needing a new `libc` dependency (see the module's
  doc comment for why that trade-off is safe).
- **`rusty-remind-me api` refuses a double start** for the same database:
  it checks `pid::dashboard_status` before binding and exits with an error
  naming the already-running instance's URL and PID instead of silently
  colliding on the port.
- **`dashboard` status** in `remind_me_server_status` is now
  `pid::dashboard_status` merged in by the MCP dispatch layer, the same
  override pattern `sync`/`webhook`/`remote` already use — `running: true`
  with the URL/PID/start time when a live dashboard is found, `running:
  false` otherwise. An in-memory database (no on-disk location for a PID
  file) reports `not_implemented` rather than a false "not running".
- Tests: `crates/remind_me_core/tests/pid_test.rs` (PID-file write/read,
  live-health-check "running", stale-file cleanup, malformed-file cleanup),
  new cases in `status_test.rs` for `embeddings` configured/unconfigured,
  and new `remind_me_mcp` dispatch tests covering both subsystems'
  live-and-not-live cases end to end.

## 2026-07-30 — Single-user OAuth 2.1 authorization server for the remote connector (#86)

### Added
- **OAuth 2.1 authorization server (FT-07)**, mounted alongside `#85`'s
  FT-05 secret-path connector when `REMIND_ME_REMOTE_ISSUER` is set:
  RFC 8414 AS metadata, RFC 9728 protected-resource metadata, RFC 7591
  dynamic client registration, a mandatory PKCE (S256) authorization-code
  flow with refresh-token rotation, and RFC 7009 revocation. New
  `crates/remind_me_remote/src/oauth/` module (`issuer`, `pkce`, `types`,
  `provider`, `routes`) ports `remind_me_mcp/oauth.py` and `remote.py`'s
  OAuth-mode branch — hand-rolled from the reference's actual behavior and
  the RFCs it cites, since `rmcp` (this workspace's Rust MCP SDK) has no
  server-side auth framework equivalent to the Python MCP SDK's
  `mcp.server.auth` (its own `auth` feature is *client*-side OAuth only,
  confirmed by reading its source directly). See
  `docs/adr/0011-oauth-hand-rolled-no-server-side-sdk.md`.
- **Single-user `/consent` (GET+POST)**: no accounts, no sessions — the
  owner pastes the existing connector token to approve a requesting client.
  A wrong credential and an explicit deny produce the identical
  `access_denied` redirect (the form never leaks which part failed),
  matching the reference's `hmac`-style constant-time comparison via this
  crate's existing `constant_time_eq`.
- **`remind_me_revoke_clients`** registered in `remind_me_mcp` (alongside
  `remind_me_self_update`/`remind_me_check_update`): empty `client_id`
  **lists** every registered client with live access/refresh token counts;
  a non-empty `client_id` revokes exactly that one client's registration
  and every token it holds; there is no "revoke all" operation. Verified
  against the reference's `tools/admin.py`, not assumed from the parameter
  name (the issue's own explicit warning) — see the ADR and
  `tests/oauth_test.rs`'s `revoke_clients_semantics_*` test.
- **Issuer validation** (`oauth::validate_issuer`): must be an https origin
  (http allowed only for `localhost`/`127.0.0.1`, matching the installed
  MCP SDK's own local-testing carve-out), no path beyond `/`, no query, no
  fragment — never derived from the inbound `Host` header. DNS-rebinding
  protection stays disabled in OAuth mode for the same reason `#85`
  disabled it for the plain connector: the credential is the issuer-bound
  access token or the secret-path/bearer token, not `Host`.
- **Legacy coexistence**: the FT-05 secret-path URL and
  `Authorization: Bearer <connector-token>` both keep working when OAuth is
  active — `auth::secret_gate` gained a `GateConfig` (`oauth_mode`,
  extra allow-paths/prefixes for the OAuth routes and `/.well-known/`
  metadata) so one middleware serves both modes, rewriting the secret-path
  form into a bearer request that `oauth::require_bearer` (layered only
  onto `/mcp`) authenticates the same way it authenticates an issued OAuth
  token.
- `remind_me_core::remote` gained `OAuthStateStore` (JSON-file client/token
  persistence, `0600`, re-read on every operation so `remind_me_revoke_clients`
  running in the stdio process revokes a client on the *live* remote server
  immediately) and `RemoteStatus`'s `oauth_enabled`/`issuer`/
  `oauth_state_file`/`oauth_clients` fields, surfaced through
  `remind_me_server_status` the same way `#85`'s plain-mode fields already
  are. `OAuthStateStore` lives in `remind_me_core` (not `remind_me_remote`)
  so the synchronous `remind_me_revoke_clients` tool can use it without
  pulling in tokio/axum — the same sync/async split `RemoteConfig`/
  `RemoteStatus` already established.

### Testing
- 47 new unit tests: `oauth::issuer` (issuer validation), `oauth::pkce`
  (S256, including the RFC 7636 Appendix B test vector), `oauth::types`
  (redirect_uri pinning, scope validation), `oauth::provider` (the full
  authorize → consent → code → exchange → refresh → revoke flow, denial
  paths, expiry), `oauth::routes` (redirect-URI construction), plus new
  `remind_me_core::remote` tests for `OAuthStateStore` and the extended
  `RemoteStatus`/`RemoteConfig`.
- 15 new HTTP integration tests (`crates/remind_me_remote/tests/oauth_test.rs`,
  same style as `#85`'s `http_test.rs` — the real axum server on an
  ephemeral loopback port, no mocking): the full PKCE flow end to end
  (register → authorize → consent → code → token → authenticated `/mcp`
  round-trip → refresh rotation → revocation), dynamic client registration
  and its rejection paths, wrong-owner-credential/explicit-deny parity,
  a PKCE mismatch that does *not* consume the code, single-use code
  replay, `remind_me_revoke_clients`' list-vs-revoke-one semantics, a
  malformed issuer rejected at `build_router` build time, and the legacy
  secret-path/bearer token still authenticating in OAuth mode.
- A real, environment-specific flakiness bug was found and fixed while
  building this out: `OAuthStateStore`'s writes now use an explicit
  `File`+`sync_all` instead of plain `fs::write`, and both `read`/`write`
  retry with backoff — a same-process read was intermittently not seeing
  its own immediately-preceding write under this sandbox's parallel test
  execution (0 failures across 90+ full-suite runs after the fix; see the
  ADR for the full root-cause writeup).
- **Not yet validated: a real claude.ai custom connector's OAuth discovery,
  consent, and token flow.** Same outstanding item `#85` recorded for the
  transport half — this sandboxed environment has no network path to
  exercise it. Must be performed by a human before this is considered fully
  done.

### Docs
- `docs/adr/0011-oauth-hand-rolled-no-server-side-sdk.md`: the
  server-side-SDK investigation (`rmcp`'s `auth` feature is client-side
  only), the hand-roll decision, the `client_id=""` semantics verification,
  and the filesystem-flakiness fix.

## 2026-07-30 — Remote MCP connector over Streamable HTTP (#85)

### Added
- **`remind_me_remote`**: a new workspace crate serving the MCP server as a
  remote connector over real MCP Streamable HTTP transport — session-managed
  SSE via the official `rmcp` (3.0.1) Rust MCP SDK, gated by the reference's
  FT-05 secret-path/bearer auth (`remind_me_mcp/remote.py`'s
  `SecretPathMiddleware` / `build_remote_app`'s no-issuer branch). Two ways
  in, matching the reference: `GET`/`POST /mcp/<token>` (path segment,
  constant-time compared, rewritten to `/mcp` before reaching the transport)
  and `/mcp` directly with `Authorization: Bearer <token>`. `GET /health`
  stays unauthenticated (SE-04 parity). Wrong token or wrong path both 404
  identically — neither leaks whether the real endpoint exists. OAuth
  (FT-07) and `remind_me_revoke_clients` are the separate, blocked-on-this
  `#86`, out of scope here.
- `tokio`/`axum`/`rmcp` are dependencies of `remind_me_remote` only —
  `remind_me_core`, `remind_me_api`, `remind_me_mcp`, and `remind_me_cli`
  stay synchronous and architecturally untouched, per the decision recorded
  on `#57`. `remind_me_cli` gained one `remote` subcommand that calls into
  `remind_me_remote::run_blocking` (which owns its own tokio runtime)
  instead of gaining any async code of its own.
- **A thin `rmcp::ServerHandler` adapter** (`remind_me_remote::handler::RemindMeHandler`),
  not a reimplementation of tool/resource/prompt dispatch: investigation of
  `rmcp` 3.0.1's actual API (its source, vendored and read directly — see
  `docs/adr/0010-remote-mcp-transport-rmcp-typed-adapter.md`) found no raw
  JSON-RPC passthrough mode, only a typed trait, so each handler method
  builds the same JSON-RPC envelope the stdio transport sends and calls
  `McpServer::handle_request` — the crate's one existing, already-tested
  dispatch entry point — on `tokio::task::spawn_blocking`.
- Default bind is loopback-only (`127.0.0.1:8768`,
  `REMIND_ME_REMOTE_HOST`/`REMIND_ME_REMOTE_PORT`); binding wider without a
  tunnel in front logs a startup warning (`remind_me_remote::warn_if_widened`,
  backed by the pure, directly-tested `is_loopback_host`) — this app never
  terminates TLS, matching the reference's own posture exactly.
- **Token resolution** (`remind_me_core::remote::resolve_connector_token`):
  `REMIND_ME_REMOTE_TOKEN` env var, else a token persisted at
  `~/.remind_me/connector_token` (0600 on unix), generated on first use from
  two concatenated `uuid` v4s (~244 bits of entropy) rather than a new `rand`
  dependency for one call site. `remind_me_server_status` now merges in a
  `remote` field (`enabled`, `host`, `port`, `token_file`, `token_configured`)
  the same way it already merges in `webhook`/`sync_peer`/`sync` — matching
  where the reference itself surfaces `get_remote_status()` (folded into its
  own `remind_me_server_status`, not a separate tool).

### Testing
- 6 new unit tests in `remind_me_core::remote` (token/config resolution,
  status reporting), 6 in `remind_me_remote` (auth path-rewriting, bind-host
  classification, handler `Send`/`Sync`), and 7 HTTP integration tests
  (`crates/remind_me_remote/tests/http_test.rs`) driving the real
  axum/rmcp server end to end: health unauthenticated, missing/wrong bearer
  401, wrong secret-path token and an unrelated path both 404 with an
  identical body, a real `initialize` negotiating a session over actual SSE
  framing (asserted directly, not assumed), and a real `remind_me_add` tool
  call round-tripping through `McpServer::handle_request` over the bearer
  path while reusing the session the secret-path form opened.
- **Not yet validated: a real claude.ai custom connector.** This sandboxed
  environment has no network path to reach one, so everything above proves
  protocol shape and this crate's own auth/routing correctness against
  `rmcp`'s real transport — not interop with an actual MCP client in the
  wild. That validation is still outstanding and must be performed by a
  human (add the tunneled `/mcp/<token>` URL as a claude.ai custom connector
  and confirm tool calls succeed) before `#85` is considered fully done.

### Docs
- `docs/adr/0010-remote-mcp-transport-rmcp-typed-adapter.md`: the
  `rmcp`-vs-hand-rolled decision, what its API investigation actually found,
  and the `ServerHandler` adapter design.

## 2026-07-30 — Consolidate near-duplicate memories (#50)

### Added
- **`remind_me_consolidate`**: finds clusters of near-duplicate memories by
  embedding similarity and merges them into one canonical representative,
  reusing the `superseded_by` supersession mechanism already in this crate
  (the same one `supersede_contradicting_facts` uses for contradictions)
  rather than a new schema concept. Unblocked by `#49` landing on `main`
  (`vec_chunks`/cosine similarity).
- **Pure clustering/merge layer** (`remind_me_core::consolidation`), mirroring
  the reference's split between `consolidation.py` (no DB access) and
  `tools/lifecycle.py` (the DB-touching handler): `find_clusters` groups
  memories via Union-Find over pairwise cosine similarity — transitive, so
  A~B and B~C cluster all three even when cos(A, C) itself falls short —
  `pick_canonical` selects the highest-vitality member (tie-broken by most
  recent `accessed_at`, keeping the *first* tie like Python's `max()` rather
  than Rust's `Iterator::max_by`, which keeps the last), and `merge_cluster`
  combines content and sums access counts across the merged memories.
- **Two-phase, safe by default**: `dry_run` (the default `true`) reports
  clusters — canonical, members, and each member's similarity to the
  canonical — without touching the store. Actually merging a cluster when
  `dry_run: false` requires a `summaries: {canonical_id: summary}` entry for
  it (an LLM-authored distillation, produced client-side exactly like
  `remind_me_decompose`/`remind_me_normalize_apply` already are); a cluster
  found but missing a summary is reported in `skipped_no_summary`, never
  silently merged with a raw line union — the reference's own `#55` fix,
  confirmed from its current source rather than an older description of it.
  A merge updates the canonical's content/access_count/tags/vitality,
  supersedes every other member, and best-effort re-embeds the canonical
  with the merged content.
- `similarity_threshold` (default 0.85, clamped 0.5..=1.0), `limit` (default
  500, clamped 10..=5000, capping the SQL fetch) and a second, independent
  `CONSOLIDATE_MAX_CANDIDATES` (1500) ceiling on the clustering step's own
  O(n^2) comparison cost — both matching the reference. Bounds are clamped
  rather than rejected, this port's established convention elsewhere (e.g.
  `EntityTraverseInput::hops`) even though the reference's Pydantic model
  itself rejects out-of-range input.
- 22 new tests: 13 pure unit tests inline in `consolidation.rs` (no
  clustering below threshold, a transitive three-member chain, `max_candidates`
  truncation, canonical selection and its tie-break including the
  first-vs-last-max distinction, line de-duplication, access-count summing,
  tag merging) and 9 integration tests in `consolidate_test.rs` (category
  scoping, `limit` capping the candidate pool before clustering ever runs,
  dry-run leaving the store untouched even with a summary supplied, a
  missing-summary cluster being skipped not merged, and a superseded member
  dropping out of a later consolidation pass).

### Notes
The reference's re-embed of the merged canonical is a fire-and-forget async
background task; this crate has no async runtime, so it re-embeds
synchronously, best-effort, in the same place every other content-mutating
write in this crate already does it (`add_memory`, `apply_normalizations`).

## 2026-07-30 — Fix an ADR numbering collision

### Fixed
- `docs/adr/0002-otel-tracing-hand-rolled-otlp-http-export.md` and
  `docs/adr/0002-embeddings-ollama-and-brute-force-vectors.md` landed
  independently via separate merges, each unaware the number was already
  taken (a third, `docs/adr/0002-dashboard-vendored-jsx-and-cors.md`, was
  caught and renumbered to `0008` while resolving a merge conflict).
  Renumbered the OpenTelemetry ADR to `0009` — its own internal `# ADR-0002:`
  header updated to match, and the one other reference to its old filename
  (in this file's own OpenTelemetry entry, below) updated too.

## 2026-07-30 — Serve the dashboard, and CORS to match (#78)

### Added
- **`GET /` serves the dashboard**: `remind_me_mcp/dashboard/App.jsx`
  vendored verbatim (a backend-agnostic client-side React component that
  only ever calls `window.location.origin + "/api"`, so it needed no
  adaptation) into `crates/remind_me_api/src/dashboard/App.jsx`, wrapped in
  the reference's own `_build_dashboard_html()` HTML exactly — same pinned
  CDN React/ReactDOM/Babel builds, same Subresource Integrity hashes.
  Registered in the same `ROUTES` table as every `/api/*` route, and
  unauthenticated even when `REMIND_ME_API_KEY` is set, matching the
  reference.
- **CORS**, matching the reference's `CORSMiddleware` exactly
  (`allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?"`,
  confirmed from source; every method and header allowed): a hand-rolled
  origin matcher (`http::cors_allowed_origin`), no new `regex` dependency,
  applied to **every** response this server sends via a new
  `write_response_cors` — not just `/api/*` — the same way Starlette's
  middleware wraps the whole app. A non-matching or absent `Origin` gets no
  CORS headers at all, which is what makes the browser refuse a
  cross-origin response rather than this crate silently allowing everything.
- `OPTIONS` (a CORS preflight) is answered uniformly before routing or auth
  — 200 with CORS headers if the origin matches, none if it doesn't —
  matching the reference's own preflight handling.

### Notes
`docs/adr/0008-dashboard-vendored-jsx-and-cors.md` records the decision and
one deliberate scope cut: `sidecars.py` (Windows Job Object process
supervision for an SSH tunnel and, optionally, a separate dashboard-UI
process) is out of scope here — it's driven from the sync loop in the
reference, so `#57` is its more natural home if it's ported at all.

The dashboard still requires network access to `unpkg.com` on first load,
exactly like the reference — an inherited limitation, not a regression.
Verified live: `GET /` serves correct HTML end to end and the page reaches
`#root` with zero errors when there's direct network access to the CDN;
only the three CDN `<script>` fetches fail without it, matching the
reference's own stated offline limitation.

## 2026-07-30 — Optional OpenTelemetry tracing (#77)

### Added
- **`telemetry::maybe_span(name)`**, matching `remind_me_mcp/telemetry.py`'s
  `maybe_span()` exactly in shape: a guard that is a genuine no-op whenever
  tracing is disabled, unconfigured, or has permanently failed — a call
  site never has to branch on whether tracing is on. `Span::mark_error()`
  records that the operation the span timed failed (`status.code = ERROR`
  on export, `OK` otherwise).
- Instrumented at all four of the reference's boundaries — every MCP tool
  call (`tool.{name}`), each folder-watcher scan pass (`watcher.scan`), each
  webhook ingest request (`webhook.ingest`), and (now that `#57`'s sync
  worker exists) each sync cycle (`sync.cycle`), marked as an error span when
  the cycle records a failure against any remote.
- `REMIND_ME_OTEL_ENABLED` (off unless `true`/`1`/`yes`), `REMIND_ME_OTEL_ENDPOINT`
  (defaults to the real OTLP exporter default, `http://localhost:4318/v1/traces`,
  confirmed against the spec rather than the bare host:port the env var's own
  doc-comment paraphrases), `REMIND_ME_OTEL_SERVICE_NAME` (defaults to
  `remind-me-mcp`) — matching the reference's three config vars.
- A hand-rolled OTLP/HTTP JSON exporter (`resourceSpans`/`scopeSpans`/`spans`,
  hex-encoded ids, nanosecond timestamps), not the real OTEL SDK — full
  reasoning in `docs/adr/0009-otel-tracing-hand-rolled-otlp-http-export.md`.
  Spans export from a dedicated background thread over a bounded channel
  (the same shape `SyncWorker` already uses) so a slow or unreachable
  collector never blocks the tool call, watcher pass, webhook request, or
  sync cycle the span is timing; a full channel just drops the span.
- Any export failure permanently disables tracing for the rest of the run,
  matching the reference's `_get_tracer()` exactly, reported via a queryable
  `telemetry::last_error()` rather than a logging framework this crate
  doesn't otherwise depend on.

### Notes
`telemetry::is_enabled()` is not yet wired into `remind_me_server_status` —
the reference's own `ServerStatus` has only `Active`/`NotImplemented`
variants, neither of which fits "built here, but off by configuration"
correctly; extending that enum is a separate, small follow-up rather than an
unrequested change bundled into this one.

## 2026-07-30 — Fix a flaky graph-sync test (#81)

### Fixed
- `graph_sync_test.rs::pull_entities_applies_the_hubs_entities_and_persists_a_namespaced_cursor`
  failed intermittently under the default concurrent test harness (root
  cause: it wrote to the hub-side database without holding the file's
  `ENV_LOCK`, so a concurrently-running test's `enable_sync("local-node")`
  could transiently stamp the inserted entity's `node_id` from the
  process-wide `NODE_ID_ENV`, causing the pull's own `exclude_node`
  filter to silently exclude it). Now holds `ENV_LOCK` and explicitly
  disables sync env vars for the duration of the write, matching every
  other test in the file that touches process-global sync configuration.
  Applied the same guard to the neighboring `pull_links_and_pull_entity_relations`
  test as a preventative measure against the same class of race.

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

## 2026-07-29 — `remind_me_check_update` and `remind_me_self_update` (#58)

### Added
- **`remind_me_check_update`** — read-only, no inputs. Fetches from
  `origin` and compares `HEAD` against `origin/main`, reporting whether an
  update is available, how many commits behind, and (up to 10) their
  one-line messages. Ports the reference's `check_for_update()` directly —
  nothing about this step depends on how the running binary was built or
  installed.
- **`remind_me_self_update(force: bool = false)`** — `git pull --ff-only`
  (refusing a working tree with uncommitted changes unless `force` is set),
  followed by `cargo build --release --workspace` in place of the
  reference's `pip install -e .`. Always reports `restart_required: true`
  on success. `force` only bypasses the dirty-tree guard, never the
  fast-forward-only pull — a diverged local history is refused either way,
  verified directly against the reference's `perform_update()` rather than
  assumed. A build failure after a successful pull is rolled back
  automatically (`git reset --hard` to the pre-pull commit); if the
  rollback itself fails, the error names the exact manual recovery command.
- A background startup check (`start_background_check`, called once from
  the CLI's actual `server`/`mcp` entry point — deliberately not from
  `McpServer::new`, which the test suite also uses to build a server per
  test) that surfaces a one-shot notice on whatever tool call happens to
  come first afterward, then clears. `REMIND_ME_AUTO_UPDATE_CHECK=false`
  skips it; the two manual tools are unaffected either way.
- `docs/adr/0003-self-update-strategy.md` — records the decision the issue
  required before implementing: the reference's own mechanism (`git pull`
  + `pip install -e .`, coherent because an editable install's source tree
  *is* the running installation) has no equivalent for a compiled binary,
  so self-update here means "pull and rebuild, then require a restart,"
  not "fetch a prebuilt release binary and swap it" (no release pipeline
  exists to fetch from) and not "check-only" (the issue names that as the
  fallback if a real update path isn't worth porting — rebuilding is a
  small addition on top of the git plumbing the read-only check already
  needs). Also records why repository discovery walks up from the
  process's current working directory rather than the executable's own
  path, unlike the reference.

### Notes
Repository discovery requires running `remind_me_check_update`/
`remind_me_self_update` from inside the repository (or a subdirectory of
it) — a real, stated divergence from the reference, not a silent
approximation. A `cargo install`ed binary has no fixed relationship to the
source tree that produced it the way an editable pip install's package
files always live inside the repo they came from, so there is no
executable-path-based discovery to fall back to; self-update is only a
coherent operation at all when it's clear which checkout it means.

Tested against real, disposable git repositories under a temp directory
(a local filesystem "origin" plus a clone of it — git treats a path remote
exactly like any other, no network needed): up to date, behind by N
commits with correct messages, an unreachable origin, a dirty tree refused
without `force`, `force` still refusing a diverged (non-fast-forward)
history, a successful pull-and-build, and a build failure's automatic
rollback. The MCP-level test suite deliberately does not invoke either
tool end to end — unlike every other tool tested there, both discover
their repository from the test binary's own working directory, which
inside this workspace's test suite is this very checkout; running
`remind_me_self_update` for real there would rebuild the actual repository
under test. Registration and input schemas are asserted instead, with the
behavioral coverage living entirely in `updater.rs`'s own unit tests
against disposable repos.

## 2026-07-29 — Embeddings and semantic search, plus `remind_me_reindex` (#49)

### Added
- **Semantic search**, wired into the existing RRF fusion in `retrieval.rs`:
  `rank_rrf` now takes both a keyword-ranked list and a semantic-ranked
  list and fuses them, so `MemorySearchResult.vec_score` is finally real
  instead of always `None`. A memory found by only one list is not
  penalized to zero on the other — it gets the same past-the-end penalty
  rank the vitality term already used for a dormant memory. When semantic
  search never ran at all (no embedder configured or reachable), `vec_score`
  reports `None`, not a misleading constant score that would look like it
  carried information.
- **`remind_me_reindex`** — a new MCP tool, no inputs, matching the
  reference exactly: embeds every memory that has no vector yet, leaving
  existing embeddings untouched. Safe to run repeatedly, and reports
  `degraded: true` (rather than silently doing nothing) when no embedder is
  configured or reachable.
- An embedding backend behind a new `Embedder` trait
  (`crates/remind_me_core/src/embedder.rs`), implemented for a local
  **Ollama** daemon's `POST /api/embed` — a hand-rolled HTTP client over
  `std::net::TcpStream`, the same pattern already established for the
  webhook endpoint (#56) and the HTTP API (#48). Off unless
  `REMIND_ME_EMBEDDING_BACKEND=ollama` is set, matching the folder watcher
  and webhook convention that the heavier optional feature stays off until
  asked for. `add`/`update` embed inline when an embedder is available;
  every other write path (bulk imports, capture, decompose, normalize)
  relies on `remind_me_reindex` as its backstop.
- `docs/adr/0002-embeddings-ollama-and-brute-force-vectors.md` — records the
  backend decision the issue required before writing any of this: Ollama
  over an in-process ONNX Runtime engine (verified feasible, not chosen, for
  scope reasons), and a new `vec_embeddings` table over `sqlite-vec`'s
  `vec0` virtual table (no mature Rust binding exists, and bundling a
  second native shared library per platform contradicts this port's
  established dependency-avoidance pattern).

### Notes
**The issue's acceptance criteria says vectors are stored in `vec_chunks`
itself; that isn't what the reference actually does.** Reading `db.py`
showed the reference's real vector store is `memories_vec`, a separate
`sqlite-vec` `vec0` virtual table — `vec_chunks` is only ever the rowid map
back to `memory_rowid`/`chunk_ix`, in the reference as much as in this
crate's own already-generated schema. This port introduces its own
`vec_embeddings(vec_rowid, embedding)` table instead of attempting to load
`sqlite-vec` as a runtime extension, created by this crate's own code (not
added to `schema_tables.sql`, which is generated verbatim from the
reference's `sqlite_master` and is not this crate's file to hand-edit). A
database shared with `remind_me` is unaffected either way: neither side
reads or writes the other's vector table.

Vectors are stored as raw float32 bytes with dimension inferred from
`len(bytes) / 4`, matching the reference's own convention exactly, so a
384/768/1024-dimensional model all round-trip without a schema change.
Semantic search itself is a brute-force cosine-similarity scan in Rust, not
a SQL `MATCH` — identical retrieval quality to the reference's own exact
scan below its `ANN_MIN_CHUNKS` threshold, just a different mechanism for
getting there.

ANN (explicitly optional in the issue, and only consulted above a chunk-count
threshold most vaults will never reach), reranking, and query expansion are
out of scope for this change — see ADR-0002's "Alternatives considered" and
"Consequences" for why, and what would have to be revisited to add any of
them later.

Deleting a memory now also deletes its `vec_chunks`/`vec_embeddings` rows —
needed because SQLite reuses freed rowids, so without this cleanup a new
memory landing on a deleted one's old rowid would silently inherit its
stale vectors.

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
