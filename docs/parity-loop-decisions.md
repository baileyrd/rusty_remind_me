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

## #116 — reminders: set, clear, list

**The reference does not apply `remind_at` on the receiving side of sync, and
this crate does.** `sync.py`'s `_upsert_one` writes 24 columns and neither
`remind_at` nor `sensitive` is among them, even though its own outbox trigger
puts both in the payload — so the reference ships a payload carrying two fields
nobody reads back. The issue's acceptance criteria asked for a round-trip, and
the same call was already made for `sensitive` under #105. A reminder is a
property of the memory, not of the machine holding it: dropping it would make a
reminder set on your laptop invisible on your desktop while every other
property of that same memory arrived intact. Divergence from the reference,
deliberate, with an end-to-end test in `reminders_sync_test.rs`.

**One consequence of that is accepted rather than worked around.**
`reminder_deliveries` is local-only, so a reminder already delivered on one
node is still pending on another and will fire there too. Being told twice on
two machines beats being told on neither because the machine that fired it was
the one you weren't sitting at.

**A past `remind_at` is rejected, not stored.** Stored, it lands straight in
the overdue bucket — the one that means "nothing was running when this came
due" — so a typo would be indistinguishable from a genuine missed delivery.
The reference makes the same call, in a pydantic validator.

**Setting a reminder writes no revision.** In the reference this falls out of a
shared helper's column gate rather than being stated; here it is explicit. The
revision log exists to recover a value a human replaced, and a vault whose
history is half scheduling noise is harder to read back than one that only
records edits.

**Naive timestamps are read as UTC, not local.** Local would be friendlier to
type and worse to store: the same string would mean different instants on two
synced machines, and `remind_at` is compared as a *string* against a UTC `now`
in every window query.

**`Memory` gained `remind_at` and `sensitive`, so every memory-returning tool
now carries both.** `remind_at` is required by the listing. `sensitive` came
with it because the reference's `_fmt_memory_md` — which this crate now
mirrors for the markdown listing — marks sensitive memories with a lock, and a
renderer that cannot see the flag cannot show it. Wider than issue #116 asked
for, and toward the reference in both cases.

**The digest's reminder and sync sections are wired up in the same change.**
Both were modelled as `Option` and omitted while their subsystems did not
exist, with the module docs promising "a one-line change for whichever issue
lands first". #114 landed the sync half and #116 the reminder half, so both
now report real answers — including an explicit "no reminders set" for an
empty vault, which is now a true statement rather than the false one it would
have been before.

---

## #117 — the reminder scheduler and notification channels

The issue said to stop and ask which channels are in scope, because some need
a new crate. Asked, and answered "keep going", so the calls below were made
and are recorded here rather than deferred.

**The webhook channel ships; SMTP does not.** The reference has exactly two,
both gated on env-var presence. Webhook needs no new dependency — this crate
already has a hand-rolled `TcpStream` HTTP client in `sync::http`. SMTP is
free in Python (`smtplib`) and is not in Rust: it needs `lettre` plus a TLS
backend, and the workspace has no TLS dependency at all today. That is a
dependency decision, so it is left for its own issue rather than taken here.

**The webhook is `http://` only, and that is a sharper limitation here than
elsewhere.** `sync::http` and `embedder` both already say "put a reverse proxy
in front", and for a sync peer on your own network that is reasonable. The
webhook endpoints people actually configure — Slack, Discord, ntfy — are
public HTTPS, so this channel cannot reach them directly. Reaching them needs
a TLS-capable client, which is the same dependency decision as SMTP and is
flagged alongside it. What ships is honest and works against a local relay;
it is not yet the whole feature the reference has.

**The issue's "channel failure is logged and retried per the reference" does
not match the reference — it does not retry**, and that is followed here. Its
`poll_once` writes the `reminder_deliveries` row after calling the delivery
hook unconditionally, and `notify()` swallows every channel error, so a
webhook that is down means the reminder is logged, marked delivered, and never
attempted again. The reasoning holds up: the log line is the channel that
cannot fail, so the reminder *has* been delivered somewhere. Retrying would
mean re-logging the same reminder every 60 seconds for as long as the webhook
stayed down, turning one missed notification into an unbounded stream of
duplicates through the channel that was working. Seventh issue whose
acceptance criteria diverge from the reference; commented on the issue.

**"A reminder due while the process was down is delivered on next start" —
confirmed, not skipped.** The issue flagged this as needing checking against
the reference. Its due query is `remind_at <= now` with no lower bound, so a
late reminder fires once on the next pass. Skipping would lose precisely the
reminders a scheduler exists to catch.

**With no channel configured, a due reminder is still logged and still
recorded as delivered.** "Channels are opt-in" governs whether a *notification*
is attempted, not whether the reminder is handled — otherwise a vault with no
webhook would accumulate undelivered reminders forever and re-log every one of
them on every pass.

**The delivery row is written after the hook, not before.** A hook that panics
outright leaves the reminder pending for the next pass. Marked first, a crash
mid-delivery would silently consume it. `INSERT OR IGNORE` keeps the unique
index as the real exactly-once guarantee, so two racing pollers produce one
delivery rather than an error that strands every later reminder in the batch.

**The scheduler thread opens its own connection from a path.**
`rusqlite::Connection` is not `Sync`, so sharing the caller's would trade a
compile error for a runtime serialisation problem. Shutdown is a condvar
rather than a sleep, so stopping does not block for up to a full poll interval.

**The loop carries reminders only.** The reference's equivalent also
piggybacks scheduled digests, watched-search polling, revision compaction and
analytics snapshot capture/retention on the same thread, for stated reasons.
Those belong to already-merged issues (#108, #109, #111, #112) whose functions
exist here but currently run only when a tool is called; wiring them onto this
loop is a follow-up rather than part of "reminders 2/3".

**`sync::http` gained a default port and optional auth.** A webhook URL
usually has no explicit port, and `parse_url` rejected that outright; and a
user's webhook shares no secret with this node, where a bare
`Authorization: Bearer` is a malformed credential some endpoints reject
outright when they would have accepted no header at all. Both are additive —
`post_json`/`get` keep sending the bearer token exactly as before.

---

## #118 — the ICS calendar feed

**Revocation, as the issue asked me to confirm: the reference has none.**
Rotation is deleting the token file, which mints a fresh token on next
resolution and silently invalidates every existing subscription. There is no
revocation list, no second valid token, and no way to revoke one calendar's
access without re-pointing all of them. Mirrored exactly rather than improved
on — a bespoke revocation scheme here would be a security mechanism invented
mid-port, which is worse than a documented limitation.

**A wrong token gets a bare 404, not a 401.** A 401 confirms the route exists
and that a token was checked, which tells a prober they have the right shape
and need only the secret. Matching the reference. The rejected token is also
never logged: a wrong value is still a secret somebody typed, and logs outlive
rotation.

**The token is compared with `constant_time_eq`**, the same helper the API key
already uses. A long secret compared with `==` is guessable a byte at a time
through a timing oracle.

**There is deliberately no "disabled" opt-out**, unlike `REMIND_ME_API_KEY`.
The token *is* the URL path, so falling open would publish every reminder to
anyone who guessed the route. When the token file can be neither read nor
written, an ephemeral per-process token is generated — a feed nobody can
subscribe to beats a feed anybody can read.

**The route is exempt from the `Authorization` gate, and nothing else is.** A
calendar app's "subscribe by URL" polls from the provider's own servers on a
schedule the user does not control, with no way to attach a header. The
exemption is matched on an exact prefix and suffix and tested against the
near-misses that would inherit it.

**"Timezone handling correct for all-day and timed reminders" — the reference
has no all-day events.** Every VEVENT is a timed `DTSTART` in UTC with a `Z`
suffix; there is no `VALUE=DATE` handling anywhere in `ics_export.py`, and
`remind_at` is a timestamp rather than a date, so an all-day reminder is not
representable upstream either. Followed the reference. What *is* asserted is
that a non-UTC offset is converted rather than having its wall-clock digits
copied and a `Z` appended, which is the actual timezone bug available here.
Ninth issue whose acceptance criteria diverge from the reference.

**No iCalendar crate.** The read-only subset needed is one VCALENDAR of
VEVENTs, small enough to write directly and test exhaustively, which is the
call this workspace has made consistently. Folding and escaping each have
their own tests because both produce output that reads fine and fails in a
real client — and an unescaped comma corrupts every VEVENT *after* the one
containing it, not just its own.

**The feed calls `reminders::list_reminders` rather than repeating its SQL.**
The reference re-derives the window inline in `api.py`; here the feed and the
tool share one definition so a calendar cannot disagree with
`remind_me_list_reminders` about what is on it. Uncapped, because a subscriber
wants every reminder rather than the first page.

---

## #119 — `/metrics` and `/manifest.json`

**`/metrics` is off by default and 404s while disabled.** The issue's criteria
do not mention a gate; the reference has one (`REMIND_ME_METRICS_ENABLED`) and
it is implemented. A 404 rather than a 403 or an empty 200 is the point: "off"
should be indistinguishable from "this build does not have it", so a scrape
pointed at a metrics-disabled server fails loudly instead of silently
recording nothing forever.

**Unauthenticated, gated on the enable flag rather than a bearer token** —
matching the reference and this crate's own `/health` posture. Prometheus
scrape configs typically send no custom headers, so requiring one would mean
hand-rolling a static-bearer config for a single target. The tradeoff is
stated plainly in the handler doc rather than hand-waved: while enabled this
reveals usage patterns — which tools are called and how often, search volume,
memory and outbox counts — to anyone who can reach the port. It exposes no
memory *content*.

**Counters vs. gauges, kept as the reference draws the line.** Only genuine
events-over-time are tracked in module state (tool calls, durations, search
tiers, rate-limit rejections) because no query can reconstruct them after the
fact. Anything already answerable by a cheap point-in-time query — total
memories, outbox depth — is computed per scrape and passed in as a
`GaugeSpec`, so it cannot drift from the table it describes.

**Tool-call timing is wired into the MCP dispatch**, not left as a recorder
nothing calls. Without it the three tool families would emit their headers
forever and never a sample, which is valid exposition and a dead metric.
Recorded for failed calls too: a tool that errors still consumed time, and
counting only successes would make a wholly broken tool look like an unused
one.

**No Prometheus client crate.** The text exposition format is a few
`# HELP`/`# TYPE` lines and `name{labels} value` samples. The one thing a
client library would buy is safe concurrent counters, which a `Mutex` around
plain maps covers — the same call this workspace made for the webhook POST,
the HTTP client and the ICS document.

**The exposition is tested by parsing it, not by substring match.** A scrape
Prometheus rejects and one it accepts differ structurally — a `# TYPE` with no
family, a sample whose name has no header — and substring assertions pass on
both. The parser is deliberately strict, and every sample is asserted to have
both a `HELP` and a `TYPE`.

**Search tiers are zero-filled rather than omitted.** A server that has served
no searches still emits all three at zero; omitted, a dashboard query returns
no data and renders as a gap rather than a flat line.

**The manifest has no `icons` key**, matching the reference. There is no icon
asset in the repository, and pointing at one that does not exist is worse than
omitting it — a manifest without icons is valid and the OS falls back to a
glyph.

---

## #120 — named, scope-limited API keys

**Unknown scopes fail closed.** A scope this build does not recognise — hand
edited, or written by a future version — is treated as read-only rather than
read-write. The alternative grants write access on a typo, which is exactly
the failure the issue warns about: a key handed out on the understanding that
it is safe.

**A corrupt or unreadable store authorises nothing.** It is reported and
treated as empty, so a damaged file locks the door rather than opening it.

**The store is re-read on every request rather than cached.** A cached store
keeps honouring a key revoked seconds ago in another terminal, and "revocation
that takes effect at the next restart" is not revocation.

**Permissions are tightened before the rename, not after.** Between a
world-readable create and a later chmod there is a window in which every hash
is readable by any local account.

**401 and 403 are kept distinct.** 401 means "I do not know this credential";
403 means "I know it and it is not allowed to do that". Collapsing them sends
a read-key holder hunting for an auth problem they do not have. The reference
draws the same line.

**The flat `REMIND_ME_API_KEY` stays implicitly read-write.** It was
unscoped-and-full-access before this feature existed, so scoping it now would
silently break a deployment that already depends on it. It is also reserved by
name and not revocable through the store, because it is config-managed and was
never stored there.

**Scope enforcement is tested route by route, not on one representative.** The
failure mode is a single mutating handler reachable by a path the gate does
not cover, and a spot check sails straight past it.

**`verify` checks every key even after a match**, so the work does not depend
on a key's position in the file. Comparison is over fixed-length hex digests
via `constant_time_eq`.

---

## Correction: the tool-count claim at #118

The #118 release note and PR said tool coverage reached "61 of 61 — full
parity". That was wrong: it counted the target-only `remind_me_wiki_import`
toward the reference's 61. The real figure was **60 of 61**, with
`remind_me_api_key` outstanding — which is what #120 adds. Verified by diffing
the two tool-name sets rather than comparing totals, which is what should have
been done the first time; comparing counts cannot notice that one set has an
extra member and is missing a different one. Corrected in `RELEASE_NOTES.md`
and `gap-analysis.md` rather than quietly restated.

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
