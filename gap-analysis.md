# Gap Analysis — `rusty_remind_me` vs. `remind_me`

**Run date:** 2026-08-05
**Target:** `baileyrd/rusty_remind_me` @ `1fecc54`, 5 crates
**Reference (pinned):** `baileyrd/remind_me` @ `caad798` — **v1.54.0**
**Previous run:** 2026-08-03 against target `a2cce8b` / reference `935eb98`.
That analysis is superseded in full: its entire gap table was worked to
completion (issues #100–#122, PRs #123–#166), and every headline number it
reported has moved.

---

## Headline: the port has reached surface parity

Every number below was re-derived independently for this run, from both
codebases, rather than carried forward.

| Surface | `remind_me` v1.54.0 | `rusty_remind_me` | Covered |
| --- | --- | --- | --- |
| MCP tools | 61 | 61 + 1 target-only | **100%** |
| HTTP API routes | 25 | 25 | **100%** |
| Peer-server routes | 7 | 7 | **100%** |
| SQLite schema version | 27 | 27 | **match** |
| SQLite tables / indexes / triggers | — | no missing object | **100%** |
| Import formats | 12 extensions | 12 | **100%** |

The previous run reported 70% tool coverage, 80% routes, 83% tables, and a
schema 8 steps behind. All of it closed.

**Method.** The two codebases share no structurally diffable surface (Python
package vs. Cargo workspace), so `cargo public-api` does not apply — the
assessment path is `spec`, same as the last run. Tools were extracted from
`@mcp.tool(name=…)` registrations against the target's dispatch table; routes
from `api.py`'s `Route(...)` list and `peer_server.py`'s path dispatch; schema
by parsing every `CREATE TABLE/INDEX/TRIGGER` in the reference package and
materializing the target's `schema_*.sql` into a real SQLite database, then
diffing object-by-object.

### Reference drift since the last pin — already absorbed

The reference advanced 5 commits past `935eb98`. Two are substantive, and
**both were already ported before this run began**:

| Reference commit | What | Target |
| --- | --- | --- |
| `e7523d7` | Claude Code JSONL envelope unwrap in the importer | present — `importer.rs:223`, same branch placement |
| `ebf555e` | `server_status` stops reporting a recovered sync as failing | present — `sync/worker.rs` `superseded_error`, with `sync_error_supersession_test.rs` |

The remaining three are a lint fix and two merge commits.

---

## Scope definition

`rusty_remind_me` still has no `ROADMAP.md`. `ARCHITECTURE.md` §1 Tenet 3
remains the hand-curated scope statement and is again treated as the
definition of parity:

> **Data Parity with `remind-me`**: Identical SQLite schema and JSON tool
> signatures for drop-in interoperability.

**That tenet is now fully satisfied.** Schema version, every schema object,
every tool name, and every route match. What follows is the residue: things
the reference has that the port does not, which sit *outside* the tenet's
letter but inside "this is the successor."

`remind_me`'s `BACKLOG.md` is the reference's own improvement backlog, not a
roadmap for the port, and is again deliberately **not** used as the scope
source.

---

## Gap table

| ID | Symbol | Category | Source | Platforms | Reference | Breaking? | Size | Issue |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **C1** | `cli list` | CLI subcommand | spec | both | `cli.py:74` — `SUBCOMMANDS = {"add","search","list"}` | no | S | [#167](https://github.com/baileyrd/rusty_remind_me/issues/167) |
| **D1** | `watchdog` | module | spec | both | `watchdog.py` (ref issue #128) | no | S | [#168](https://github.com/baileyrd/rusty_remind_me/issues/168) |
| **E5** | `sidecars` | module | spec | both | `sidecars.py` | no | M | [#169](https://github.com/baileyrd/rusty_remind_me/issues/169) |
| **E1** | the hub | deployable | spec | — | `hub/` — 1,341 LOC, 10 routes | — | XL | **not filed — scope decision; now ported, ADR-0015** |

All three filed gaps are pure additions. None touches an existing public
signature, so none is `breaking-change`-labelled.

### C1 — the `list` CLI subcommand

The reference dispatches `add`/`search`/`list`; the target dispatched
`add`/`search` and ten others, but not `list`. The tool logic
(`remind_me_list`, `queries::list_memories`) already existed — only the
command-line route was missing.

### D1 — the stuck-call watchdog

Nothing in the target reported *which* tool call was hung; the only external
symptom was the client's timeout.

Worth recording, because it constrains any future work here: the reference
uses `faulthandler.dump_traceback_later`, which dumps every thread's stack
including one blocked in synchronous CPU-bound code. **Rust has no stdlib
equivalent.** A faithful port needs a stack-unwinding crate — a new
third-party dependency for a diagnostic. The implementation reports identity
and duration instead, and says so in its own module docs.

**Closed behind a feature.** `stack-dumps` (Linux-only, off by default) now
dumps every thread's stack exactly as the reference does, by `ptrace`-ing this
process from a short-lived child. Feature-off behaviour is unchanged, so the
paragraph above still describes the default build. The estimate in it was
optimistic in one respect worth correcting: the cost is a *system* library
(`libunwind-ptrace`) and permission to `ptrace`, not merely a crate. See
`docs/adr/0014`.

### E5 — sidecar processes

No counterpart existed: `Command::new` appeared only in `updater.rs` for
git/cargo one-shots, and `REMIND_ME_TUNNEL` nowhere.

The finding that shaped the implementation: **the reference's teardown
guarantee is Windows-only.** Its `_job()` returns `None` immediately when
`sys.platform != "win32"`, so on Linux and macOS the reference orphans its
sidecars on abnormal exit exactly as a naive implementation would. The gap
against the reference was therefore one platform/exit cell, not four — see
`docs/adr/0013`.

**Closed in [#186](https://github.com/baileyrd/rusty_remind_me/pull/186).**
That one cell is now matched: children are assigned to a Windows Job object
with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, taking the workspace's first FFI
dependency (`windows-sys`, target-gated to `cfg(windows)`). ADR-0013 is
amended in place. The Unix abnormal-exit row still orphans, in both
implementations — deliberately, since closing it would overshoot the
reference. The Windows path is type-checked against
`x86_64-pc-windows-gnu`, not runtime-tested; CI is ubuntu-only.

### E1 — the hub

**Deliberately not filed.** `hub/main.py` is 1,341 LOC of Python serving
`/sync/pull`, `/sync/push`, `/count`, `/health`, `/metrics`, `/stats`,
`/admin/compact_tombstones`, deployed as a container (Quadlet, Compose, Fly,
Railway) with its own Postgres.

This is not an addition to the Rust workspace — it is a separate deployable in
another language and runtime, and the target already implements the
*peer* side of the same protocol (all 7 peer routes). Whether the successor
ships its own hub, or keeps syncing against the existing Python one, is a
scope decision that belongs to a human. Filing it as a `parity-gap` issue
would imply the loop should work it unattended, which it should not.

**Resolved: ported.** The reasoning above is kept as the record of why this was
never filed, but the decision has since been made and taken. The answer was to
ship a hub, with storage behind a `HubStore` trait — Postgres as a drop-in for
an existing deployment, SQLite for a self-hosted one wanting no database server
(`docs/adr/0015`). `crates/remind_me_hub` serves all ten routes, and its
deployment packaging — Containerfile, `setup.sh`, `client-setup.sh`, and
Quadlet/Compose/Fly/Railway templates — followed in
[#190](https://github.com/baileyrd/rusty_remind_me/pull/190).

**Asked, answered, and ported.** All ten routes now exist as `remind_me_hub` /
`rusty-remind-me-hub`, with storage behind a trait: Postgres (a drop-in for an
existing deployment, legacy migration included) and SQLite (self-hosted, one
file, no server). `docs/adr/0015` records the decision.

The port found a real bug in the reference's legacy migration — its
trailing-zero regex turns `.500000` into `.5`, which sorts *before* the
client's own value under `COLLATE "C"` and so corrupts both the pull cursor and
LWW conflict resolution. That is the one deliberate behavioural divergence in
the whole hub, and it was found by running the migration against a live
Postgres rather than by reading it.

---

## Verified as *not* gaps

Recorded because each looked like one:

- **`memories_vec`** — the reference's `vec0` virtual table for embedding
  vectors has no target counterpart. This is a documented, deliberate
  divergence, not an omission: `docs/adr/0002` records why the target uses a
  plain `vec_embeddings` table and a Rust-side cosine scan (no loadable
  `sqlite-vec` extension available to this crate), and notes that neither side
  reads the other's vector store, so a shared database is unaffected. The
  `vec_chunks` rowid map — the part that *is* shared — matches exactly.
- **`webhook_server.py`** — present as `webhook.rs`, including the
  constant-time token comparison and the off-unless-a-secret-is-set default.
- **`ics_export.py`** — present as `ics.rs`.
- **`query_expansion.py`** — present as `expansion.rs`.
- **`exporter.py`** — present as `export.rs`.
- **`storage_interfaces.py`** — interface documentation, no runtime behavior.
- **`benchmarks/`** — a reference-side evaluation harness, not served surface.
- Python packaging (`pyproject.toml`, `uv.lock`, hatchling).

---

## Stop-and-ask items

Under the parity-loop skill's rules these are never auto-implemented. All
three were put to a human; all three came back yes and are done.

1. ~~**E1, the hub**~~ — **asked, answered, and done.** A new deployable in
   another language, so the scope decision came first, as it should have. The
   answer was to port it, with both storage backends behind a trait; see
   `docs/adr/0015`.
2. ~~**The Windows Job object for E5**~~ — **asked, answered, and done in
   [#186](https://github.com/baileyrd/rusty_remind_me/pull/186).** It was
   stopped-and-asked precisely because it needed a direct `windows-sys`
   dependency against a workspace that had deliberately had no FFI dependency
   at all (`docs/adr/0012` took the same decision against `libc`). The answer
   was to take it, target-gated to `cfg(windows)`; ADR-0013 is amended in
   place rather than quietly contradicted. ADR-0012's refusal of `libc`
   stands — that probe has a pure-`std` alternative and `CreateJobObjectW`
   does not.
3. ~~**A stack-dumping crate for D1**~~ — **asked, answered, and done.** The
   framing in the row above was too generous to itself: there is no pure-Rust
   way to dump another thread's stack, so the real cost was a *system* library
   (`libunwind-ptrace`) plus permission to `ptrace`, not just a crate. Taken
   behind an off-by-default, Linux-only `stack-dumps` feature. The in-process
   signal-handler alternative was rejected because capturing a backtrace is not
   async-signal-safe and would deadlock precisely when the diagnostic fires.
   `docs/adr/0014` records it.

None was taken unattended. **All three are now decided and done** — E1 was the
last, and its answer landed in [#189](https://github.com/baileyrd/rusty_remind_me/pull/189)
(the hub) and [#190](https://github.com/baileyrd/rusty_remind_me/pull/190)
(its deployment packaging).

---

## Sequencing

All three filed gaps are independent — no ordering constraint. They were
worked C1 → D1 → E5, smallest first.
