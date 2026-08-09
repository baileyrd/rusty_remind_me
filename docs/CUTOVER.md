# Client Cutover Runbook

**What this is.** A runbook plus a dated record of the first real one: migrating
every process on a machine that talks to `remind_me`/`rusty_remind_me` from the
Python reference to this port, without downtime and without losing sync state.
**What this is not.** An architecture decision about the Rust codebase — see
`docs/adr/` for those. This document is operational: it describes *consumers*
(other people's config files, other apps' supervisors) that live outside this
repo, and is host-specific by nature. Keep it current anyway; it is the only
place this information is written down at all (see Lesson 1).

---

## Why this exists

The 2026-08-08 production switch recorded in `gap-analysis.md` and
`RELEASE_NOTES.md` replaced the **server-side** infrastructure — the sync hub,
the dashboard/API, the `claude.ai` remote connector — with `rusty_remind_me`.
That switch and the **client** cutover (every stdio MCP process that talks to
that infrastructure) are two separate migrations. The server-side switch was
declared complete; three independent client processes were still running the
Python reference for the next several hours, undetected, because nothing
checks client configs as part of "the switch." Conflating the two is the
mistake this document exists to prevent from repeating.

---

## The consumer inventory

Every process that can open `~/.remind-me/memory.db` or call into
`remind_me`/`rusty_remind_me`, as found on this host on 2026-08-08. Rebuild
this table (don't trust it) before the next cutover — see Lesson 1.

| Consumer | Config | Supervisor | Found running | Action |
| --- | --- | --- | --- | --- |
| Claude Code | `~/.claude.json` → `mcpServers.remind-me` | none (spawned per launch) | already `rusty-remind-me server` | none |
| omp agent (this harness) | `~/.omp/agent/mcp.json` | none (spawned per session) | Python `remind-me-mcp` v1.51.0 (`uv tool` install) | config edited; **required a full session restart** to take effect |
| hermes-dashboard | `~/.hermes/config.yaml` → `mcp_servers.remind-me` (shared) | `systemctl --user` (`hermes-dashboard.service`) | Python `python -m remind_me_mcp` (`~/remind_me/.venv`) | config edited; `systemctl --user restart hermes-dashboard.service` |
| hermes-gateway | same config file as above | `systemctl --user` (`hermes-gateway.service`) | same Python process, second instance | same config edit; `systemctl --user restart hermes-gateway.service` |
| `remind-me-hub` | `systemctl --user` unit → podman `remind-me-hub:latest` | `systemctl --user` | already the Rust image (91MB; old image cold-tagged `python-rollback`/`1.5.0`, 193MB) | none |
| `remind-me-connector` (claude.ai remote) | `systemctl --user` unit | `systemctl --user` | already `rusty-remind-me remote` | none |
| `remind-me-ui` (dashboard/API) | `systemctl --user` unit | `systemctl --user` | already `rusty-remind-me api 5199 100.83.168.90` | none |
| `remind-me-postgres` | `systemctl --user` unit → podman postgres | `systemctl --user` | unrelated engine, unchanged by either implementation | none |

Two consumers were already correct because they were the pilot for the
server-side switch. The other two (omp, hermes) were missed because nothing
enumerates "who has their own copy of the MCP launch command" — each is a
config file inside a *different* application's own state directory, none of
which this repo owns or can lint.

---

## Lessons learned

1. **No single registry of consumers exists, and one is needed.** This table
   was built by cross-referencing `ps -ef`, `ss -tlnp` (to catch the
   `REMIND_ME_PEER_PORT` listener), `systemctl --user list-units`, and a
   manual read of three independently-owned config files. There is no
   `remind-me consumers` command. Until there is, this document *is* the
   registry — update the table above whenever a consumer is added, removed,
   or its config file moves.

2. **"Production switch" and "client cutover" are different events.**
   Finishing the server-side switch (hub, connector, dashboard) implies
   nothing about client state. Treat them as separate checklists with
   separate completion criteria; don't declare victory on one and assume the
   other followed.

3. **A stdio client cannot hot-swap.** Editing a config file never affects an
   already-spawned process — the child keeps running the old binary until the
   *parent* (the client application itself) restarts and re-reads its config.
   Confirmed directly: after editing `~/.omp/agent/mcp.json`, `ps` still
   showed the old Python pid, unchanged, until the omp session itself was
   restarted. This is the same caveat `gap-analysis.md` already recorded for
   the server-side switch ("this machine's already-running stdio sessions...
   pick up the switch on their next restart, not mid-session") — it applies
   just as literally to every other stdio client, and it is easy to forget
   because editing the file *feels* like the fix.

4. **Verify against the live file before editing any config.** Before
   touching a single config, run `rusty-remind-me <verb>` by hand with the
   target consumer's exact environment variables against the real
   `~/.remind-me/memory.db`, and confirm sane output. This is the same
   discipline `gap-analysis.md`'s 2026-08-07 entry already learned the hard
   way ("None came from reading either codebase") — re-reading documentation
   is not a substitute for one real command against the real file.

5. **Recycle through the supervisor, not the subprocess.** `hermes-dashboard`
   and `hermes-gateway` are `systemctl --user` units whose main process
   spawns the MCP server as a child (through a watchdog wrapper). Restart the
   *unit* (`systemctl --user restart <name>`) so the whole tree re-execs
   against the new command line — signalling the child subprocess directly is
   not equivalent and was not attempted here for exactly that reason.

6. **Two things that look like bugs during this kind of audit and are not:**
   - **Multiple co-located processes sharing one `REMIND_ME_NODE_ID`**
     (here, `ai-server`, across omp + both hermes processes) **is correct**
     when they share one `REMIND_ME_MCP_DIR`. Node identity models the shared
     local *database*, not the process — `node_id`/`client` are stamped per
     row precisely so a node can tell its own writes apart from a peer's
     (`docs/adr/0004-sync-protocol-and-conflict-resolution.md`). `client`
     (`omp_agent`, `hermes_agent`, …) is the per-process distinction; `node_id`
     is not supposed to be.
   - **Only one of several co-located processes holds
     `REMIND_ME_PEER_PORT`** (default 8766, `remind_me_core::sync::DEFAULT_PEER_PORT`).
     The processes that lose the bind race simply never start their direct
     peer-push *receiver* — hub-mediated pull/push sync is a separate,
     unaffected path. Confirmed post-cutover: `ss -tlnp` showed exactly one
     holder, `mcp-stderr.log` showed continued clean `Hub sync complete`
     cycles, zero bind-error log lines anywhere.

7. **Keep the old install cold instead of deleting it.** Neither
   `~/remind_me/.venv` (the reference checkout) nor the `remind-me-mcp` v1.51.0
   `uv tool` install were removed. They cost nothing idle and are the entire
   rollback plan below — deleting them before confidence is established would
   turn a config edit back into a reinstall.

---

## Runbook: cutting a client over

1. **Enumerate.** `ps -ef | grep -iE remind`, `ss -tlnp | grep <peer port>`,
   `systemctl --user list-units 'remind-me-*'` plus every other app's own
   units, and a manual read of every known client config. Don't trust the
   table above without re-deriving it — see Lesson 1.
2. **Classify each hit**: config file location, current `command`/`args`/env,
   and its supervisor (systemd unit vs. bare per-session/per-launch process
   with nothing watching it).
3. **Verify first.** Run `rusty-remind-me <verb>` from a shell carrying that
   consumer's exact `REMIND_ME_*` environment against the real
   `REMIND_ME_MCP_DIR`. Confirm real, sane output before editing anything.
4. **Edit in place.** Swap `command`/`args` to the `rusty-remind-me server`
   binary path. Copy every `REMIND_ME_*` variable byte-for-byte from the
   existing entry. Add `REMIND_ME_PEER_BIND=127.0.0.1` and
   `REMIND_ME_DEFAULT_RESPONSE_FORMAT=markdown` to match the already-migrated
   reference config (Claude Code's, in this case) so tool output shape does
   not shift for existing callers mid-cutover.
5. **Recycle correctly.** `systemctl --user restart <unit>` for
   systemd-managed consumers. A full process/application restart for anything
   with no supervisor (this is the step that is easy to skip — see Lesson 3).
6. **Verify after.** `ps aux | grep remind_me_mcp` (or the reference's module
   name) returns nothing; the new process tree shows `rusty-remind-me`;
   `systemctl --user show <unit> -p NRestarts` reads `0` (no crash loop); a
   functional smoke test (e.g. `remind_me_stats`) through that consumer's own
   tool surface returns real data from the real database.
7. Repeat until the inventory has no legacy rows left, then re-run step 1 one
   more time — the enumeration is the part most likely to have missed
   something, not the edit.

---

## Rollback

Per consumer: revert `command`/`args` to the Python invocation recorded in
this document's "Found running" column, recycle the same way (systemd
restart, or full session/app restart). No data migration in either direction
— both implementations open the same SQLite file and schema version
(`README.md` § Database Location; `docs/adr/0016-memory-ids-are-opaque.md`),
so rollback is a config-and-restart operation, not a data operation. Keep the
reference install cold (Lesson 7) until it's actually safe to remove it.
