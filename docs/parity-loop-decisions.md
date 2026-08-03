# Parity-loop decision log

Decisions taken **autonomously** while working the `parity-gap` issues from
`gap-analysis.md`, recorded here rather than raised as questions.

Each entry says what was decided, why, and what would have to be true to
revisit it. Anything a reviewer would want to have been consulted about belongs
here — if it is not written down, it did not happen deliberately.

Entries are newest last, grouped by issue.

---

## #102 — `remind_me_recalibrate_candidates`

**The issue asked for `markdown` and `json` `response_format` variants. Not
implemented; matched the reference instead.**

The reference's `RecalibrateCandidatesInput` (`models.py:1350`) carries only
`limit` and always returns JSON. Its models are `extra="forbid"`, so a caller
sending `response_format` would be *rejected* by `remind_me` while being
accepted here — divergence in the direction that breaks drop-in
interoperability quietly rather than loudly.

Revisit if: the reference adds the field, or drop-in interoperability stops
being the point of this port.

---

## #104 — version in server status

**The issue asked for the version in *both* `remind_me_stats` and
`remind_me_server_status`. Implemented in `server_status` only.**

`pid.py:176` puts `version` into `get_server_status`'s payload; `admin.py:622`
prints it. `remind_me_stats` (`admin.py:408`) contains no version field at all
— zero occurrences of the word in the whole tool body.

**The issue's second criterion (peer versions from the `/health` probe) was
deferred to #114.** It surfaces through `remind_me_sync_status`, which does not
exist here yet; building half of it would leave a field nothing populates.

**The gap analysis's T10 row was corrected in place**, because it asserted the
"both tools" claim and is where the next run would re-derive it.

---

## #105 — the sensitive flag

**Implemented two fields the issue did not name.**

`MemoryUpdateInput.sensitive` (`models.py:382`) — without it a memory marked at
creation can never be unmarked, so the feature is half-usable. `Option<bool>`
rather than `bool` so an update that does not mention the flag cannot silently
clear it.

`SyncRecord.sensitive` — required to make the issue's *own* "round-trips
through the sync outbox payload" criterion true in both directions. #101
delivered the sending half; without the receiving half a peer stores the memory
unmarked and surfaces it in ordinary search.

**Deliberately did not expose `sensitive` on the public `Memory` read struct.**
The reference's memory payloads do carry it, so this is a real follow-up — but
it means adding a field to a widely-constructed public struct, and no
acceptance criterion needed it. Filed as a note here rather than done quietly.

**The semantic half of search is filtered in Rust, not by threading a parameter
through `semantic_search_scored`.** Threading it would change an existing public
signature, which this loop does not do unattended. The cost is one extra query
per search that excludes sensitive memories.

---

## #106 — per-call retrieval strategy

**Most of the issue's acceptance criteria were already satisfied before any
code was written, and were covered with characterisation tests rather than
reimplemented.**

The `RetrievalStrategy` enum, the three multiplier presets, the `Auto` router
and all three shape heuristics (`looks_keyword_shaped`, `looks_semantic_shaped`,
`looks_temporal_shaped`) already existed in `retrieval.rs`. `search_memories`
simply hardcoded `RetrievalStrategy::Auto` instead of reading the caller's
choice. The only missing piece was the field and one line threading it through.

Rewriting working, tuned retrieval logic to "implement" criteria it already met
would have risked changing search behaviour for no gain. What was missing was
*coverage*: none of it was reachable from the test suite. It is now.

**Weights are asserted as ratios against `Balanced`, never as absolute
numbers.** The presets are multipliers composed on top of
`RrfWeights::from_env`, deliberately, so a preset cannot resurrect a signal an
operator zeroed. Asserting absolutes would bake the default config into the
tests and fail for anyone who tuned theirs.

**Precedence between the env vars and the per-call value is documented on the
field** (the issue asked for it to be documented, not changed): the environment
sets the baseline, the strategy scales it, and `Balanced` is the identity
multiplier rather than a reset to built-in defaults.

---

## #107 — `/api/versions`

**Gap A6 was withdrawn: there is no target-only `/api` route.** The row came
from an extraction that grepped `"/api"` string literals and matched a doc
comment — `routes.rs`'s note that the vendored dashboard talks to
`window.location.origin + "/api"`. `ROUTES` contains 20 patterns, all of which
the reference also serves. The headline table's "21 (20 shared + 1
target-only)" is corrected to 20 shared, and the gap row is struck through
rather than deleted so the next run does not re-derive it.

**Added `version` to `/health` as well as `/api/versions`.** Not in the issue's
criteria, but the dashboard header cannot be satisfied without it: the
reference reads the *node's* version from `/health` and only the *hub's* from
`/api/versions`. Its stated reason is worth keeping — `/health` is
unauthenticated, so it still answers when the API key is wrong or missing,
which is exactly when you most want to know which build you are talking to.

**Re-copied `dashboard/App.jsx` verbatim from the reference rather than
patching in a version header.** The file is vendored under the same convention
as the generated `schema_*.sql` — regenerate, never patch. The copy was 114
lines behind.

The cost: the newer dashboard also calls `/api/analytics/trend`, which does not
exist here until #112. Accepted because every fetch in that component is
individually caught and a failure leaves the trend state at its empty default,
so the chart renders empty rather than the page breaking. Revisit if #112 slips
far enough that an empty panel becomes confusing.

**`probe_hub_version` was added to `sync` rather than making `sync::http`
public.** The API crate has no business making raw HTTP requests, and this
keeps the auth header, the timeout and the cache in one place. Probed live
rather than read from sync state, following the reference's reasoning: a
dashboard started standalone never runs a sync cycle, so anything populated by
that cycle would report `null` forever while looking like a working feature.

---

## #108 — saved and watched searches

**The issue's criterion "a watched search returns only new hits on the second
run" describes behaviour the reference does not have.** Its
`remind_me_run_saved_search` calls the search core and returns whatever comes
back, watched or not (`tools/saved_searches.py`); the unseen-only diff lives in
the background poller, which notifies rather than filtering a tool's output.

Implemented to match: running returns everything, polling diffs. Asking for a
saved search's results and silently getting a partial list because something
polled it earlier would be surprising and unfixable from the caller's side.

**The criterion "both `markdown` and `json` `response_format` variants" is the
same overstatement as #102's.** None of the four reference tools take a
`response_format`. Not added.

**`poll_saved_search` returns the new matches instead of dispatching
notifications.** The transport is the scheduler's half and lands with #117.
Returning them keeps the diff logic — which is the part the issue rightly
flags as most likely to be dropped — complete and testable before any transport
exists, rather than shipping `watch` as an inert stored flag.

**`MemorySearchInput::Default` is hand-written, not derived.** Deriving it gave
`limit: 0` and `token_budget: 0` — not a neutral starting point but a search
that structurally cannot return anything, which is exactly how it failed first
time. The hand-written impl matches what serde supplies for absent keys, so a
programmatically-built input and a minimal JSON one behave identically.

**Polling searches with `include_dormant: true`.** A watch that stopped
reporting a memory because it decayed below the vitality floor would look like
the memory had been deleted.

---

## #109 — edit history and revert

**The issue's scope warning listed seven mutation paths and asked for them to
be audited rather than assumed. The audit answer is one.** The reference writes
`memory_revisions` rows from its update path alone (`tools/crud.py`'s
`_apply_memory_field_update`) — reclassify, normalize, annotate, consolidate
and decompose record nothing.

Followed rather than "corrected", and the reasoning holds: a revision exists to
recover a value a human replaced. The other paths either add derived data
alongside the original or change recomputable classification metadata, and
recording them would bury the edits worth reverting under machine-generated
noise. Both halves are pinned by tests — updates record, reclassify does not —
because "we forgot to wire it up" and "the reference deliberately does not" are
indistinguishable from the outside.

**Reverting to a memory's current state reports `NoChange` rather than writing
a no-op revision.** The alternative is an outbox row that says nothing changed
and a history entry recording a non-edit.

**`TrackedChanges` compares stored representations, not parsed values.** Tags
and metadata are compared as their JSON strings, so a metadata
re-serialisation with reordered keys is not mistaken for an edit.

---

## #110 — contradiction candidates

**The fan-out cap is implemented as part of the tool, not deferred.** It landed
in the reference on 2026-08-02 (`935eb98`), after the gap analysis was first
written, and the T6 row was amended to carry it. Porting the pre-`935eb98` SQL
would have shipped a queue where a single broadly-mentioned entity contributes
74% of the rows — invisible on a small test vault, decisive on a real one.

**No apply tool**, matching the reference: a confirmed contradiction is fixed
with the existing `remind_me_update`, `remind_me_delete`, or an
`remind_me_add` carrying an explicit triple.

---

## #111 — vault digest

**Two of the reference's five digest sections are omitted, not stubbed.**
Upcoming/overdue reminders and sync status read from subsystems this crate
does not have yet (#116 and #114). They are modelled as `Option` and left out
of both the Markdown and the JSON.

A "Reminders: none" line on a build with no reminders subsystem says something
false — it reads as "you have nothing due" when the truth is "nothing here can
tell". Omitting is the honest shape, and filling them in is a one-line change
for whichever issue lands first. `status.rs`'s module docs make the same
argument about hollow stubs.

**The digest has no `include_sensitive`, matching the reference.** It is the
ambient, often-scheduled surface the flag exists to protect, with no per-call
caller intent to opt back in against. The exclusion covers the count as well
as the list — a total that included a sensitive memory would leak that
something is there.

---

## #112 — analytics snapshots and the trend route

**The trend route captures a snapshot on read.** The reference captures from
its scheduler's poll loop; this crate has no always-on loop yet, so a
scheduler-only capture would leave the chart permanently empty on exactly the
installs most likely to open it.

Safe only because capture is idempotent per calendar day — asserted by a route
test that hits the endpoint three times and checks the series still has one
point. Without that, the chart would measure page loads rather than the vault.
When a scheduler lands (#117), it can call the same function; the day check
makes both callers safe together.

**Comparison is by date, not timestamp.** A server restarted three times in a
day would otherwise produce three data points and the trend would read as a
spike that never happened.

**A malformed stored value degrades to an empty map rather than failing the
read.** One bad row should not blank a whole chart.

---

## #113 — `GET /count` on the peer server

**The issue's `approx=1`, `since=` and `by=origin_node` parameters belong to
the *hub's* `/count`, not the peer server's.** The reference's
`peer_server.py` takes no query parameters at all and always reports
`"approximate": false`, with a comment explaining why: a peer has no planner
estimates to offer. `?approx=1` is a Postgres `reltuples` read, and the hub is
a Postgres service — E1, deliberately out of scope.

Implemented the peer's endpoint faithfully: no parameters, `approximate`
present-and-false. The three parameters would arrive with the hub if the hub
were ever in scope.

**Counts deliberately do not filter `deleted_at`.** Both ends of a reconcile
have to count identically. The hub counts every row and reports tombstones
separately, so filtering here would make a healthy peer look permanently
behind by its own tombstone count.

**`version` was added to the peer server's `/health` too.** A reconcile
reports which build each side is running, and that is where the other side
reads it from.

---

## #114 — sync status and repair

**`sync_repair` resets only the pull cursors, never the `_at` liveness
columns.** Those record what actually happened; rewriting them to force a
re-pull would destroy the evidence you were reading when you decided a repair
was needed.

**Repairing a remote with no `sync_log` row reports that rather than
succeeding.** A remote never contacted has nothing to repair, and a false
success sends the caller waiting for a re-pull that is not coming.

**Direction and rate were deliberately separated after a real bug.** The first
implementation gated the drain verdict on elapsed time, and two calls a few
hundred microseconds apart elapse zero whole milliseconds — so a backlog that
had visibly not moved reported `Unknown`. The delta alone answers "is it
moving"; only the per-minute figure needs a clock.

---

## #115 — reconcile against hub and peer

**One classifier serves both remote kinds.** "Local greater than remote means
pushes are not landing" does not depend on which machine is on the other end,
and a second copy is how the two would eventually disagree about what drift
means. `reconcile_peer` and `reconcile_hub` are the same function with a
different base URL and `sync_log` row.

**`NodeAhead` is checked first and unconditionally**, ahead of any lag
reasoning. It is the only direction where records sit on one machine with
nothing coming to fix them, and mixed drift is exactly where reading the
numbers by eye goes wrong — a remote ahead by 38 somewhere is loud, and the 3
records at risk are quiet.

**An unreachable remote returns `Unavailable` rather than a verdict.** A
verdict computed against counts that could not be fetched would be a guess;
the reachability problem is the answer.

**The classifier is tested directly rather than through a live remote.** The
network half is one HTTP GET; driving a fake server through every verdict
would test the harness more than the judgment.

---

## Process corrections made mid-loop

**A tool can be advertised without being routable, and only clippy notices.**
While working #110 the schema entry landed but the dispatch arm did not — the
string the insertion anchored on had been reformatted. Nothing failed except a
`unused imports` warning, which `-D warnings` turned into a build failure by
luck rather than design. The check is now explicit: cross-reference the
`"name":` entries against the `=> {` dispatch arms and assert the two sets
match. At the time of writing, 53 each, no difference.

**The local gate now runs `cargo test --workspace --no-fail-fast`.** `cargo
test` stops at the first failing binary, and the four pre-existing
`updater::tests` failures (a container git-signing artifact, not a code defect)
abort the run before any integration test executes. Every "clean except the
four known failures" reported on #125–#130 was therefore describing only the
lib binary. CI caught what the local gate could not — the `SyncRecord.sensitive`
deserialisation bug on #130.

**Build and lint checks assert exit status, not `grep '^error'`.** Cargo
colourises its output, so ANSI escapes precede the word `error` and the pattern
never matches. Three real compile errors were reported as clean before this was
noticed.

**CI state is read with `actions_get` / `get_workflow_job`, not
`pull_request_read` / `get_check_runs`.** The latter lags, reporting
`in_progress` for minutes after a job has finished.
