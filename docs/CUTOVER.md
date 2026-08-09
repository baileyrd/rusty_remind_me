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

## Feature parity gaps (found scoping a cutover on `home-pc-win`, 2026-08-08)

This host's topology differs from the inventory below: one shared process
(Windows Task Scheduler task `remind-me-mcp-http`, launching
`~/.remind-me/serve-mcp.ps1`) serves the MCP HTTP endpoint
(`127.0.0.1:8767`), the peer-sync receiver (`0.0.0.0:8766`), and the hub
push/pull worker all at once — every local client (Claude Desktop via
`mcp-remote`, Claude Code natively, this harness's own `remind-me-win` MCP
entry) points at that one fixed URL, so a cutover here needs zero client
config edits, only a backend-process swap. A second Task Scheduler process
(`python -m remind_me_mcp --serve-ui --ui-port 5199`) is the dashboard
sidecar, riding the first process's Windows Job object.

`rusty-remind-me remote` (port-overridden to 8767) already replicates the
combined shape: `McpServer::new` wires up `SyncPeer` (the port-8766
receiver, gated on `REMIND_ME_SYNC_SECRET`) and `SyncWorker` (the hub
push/pull loop, gated on node_id + hub_url + secret) unconditionally
(`crates/remind_me_mcp/src/lib.rs:351-357`) — no separate "sync" subcommand
needed.

The cutover was **deliberately deferred** on this host: `serve-mcp.ps1` also
sets `REMIND_ME_RERANK=onnx` and `REMIND_ME_QUERY_EXPANSION=hyde`, and the
Rust port's support for those was not at parity.

1. **Reranking — now closed.** `remind_me_core::reranker` supports it, but
   only behind `cargo build --features remind_me_core/rerank` (not in a
   default build) and only against a `.rten`-format model — the reference's
   HuggingFace cache
   (`~/.remind-me/models/models--cross-encoder--ms-marco-MiniLM-L6-v2`) is
   ONNX/safetensors, not `.rten`. Converted directly from the cached ONNX
   export, no re-download and no PyTorch needed:
   ```bash
   pip install --target <short-path-dir> rten-convert   # onnx export already in the HF cache
   python -m rten_convert <cache>/onnx/model.onnx out.rten --no-infer-shapes
   ```
   `--no-infer-shapes` works around a Windows-only bug in `rten-convert`
   0.22.0: `onnx.shape_inference.infer_shapes_path` writes through a
   `tempfile.NamedTemporaryFile` handle that is still open when its own C
   extension tries to reopen the same path, so the default (shape-inference
   on) path always raises `PermissionError` on Windows. Skipping inference
   costs a runtime graph optimization, not correctness.
   Verified against real weights (`cargo run -p remind_me_core --example
   verify_rerank --features rerank`, deleted after use): a deliberately
   RRF-inverted 4-candidate list was correctly reordered, on-topic
   candidates first, with sane cross-encoder logits (`status() == Ready`).
   Converted model kept at `~/.remind-me/rerank/ms-marco-MiniLM-L6-v2.rten`
   + `~/.remind-me/rerank/tokenizer.json`; point
   `REMIND_ME_RERANK_MODEL_PATH`/`REMIND_ME_RERANK_TOKENIZER_PATH` at those
   and build with `--features remind_me_core/rerank` (or add a
   `rerank = ["remind_me_core/rerank"]` passthrough to
   `remind_me_cli/Cargo.toml` so plain `--features rerank` works from the
   `rusty-remind-me` package without the `package/feature` qualifier).

2. **Query expansion (HyDE) — now closed.** Implemented from scratch —
   `remind_me_core::query_expansion`, ported from the reference's
   `query_expansion.py`: `REMIND_ME_QUERY_EXPANSION=hyde` sends the query to
   a local Ollama daemon's `/api/generate` (`REMIND_ME_HYDE_MODEL`, default
   `llama3.2`; `REMIND_ME_HYDE_TIMEOUT`, default 15s) for a short
   hypothetical passage, capped at 600 chars, cached per query (bounded
   LRU). Any failure — daemon down, model missing, timeout, empty response —
   silently falls back to the plain query, never an error. The expansion
   text is fused into the search vector by
   `remind_me_core::vectors::fuse_query_embedding` (query text embedded with
   `EmbedRole::Query`, expansion text with `EmbedRole::Passage`, mean then
   L2-renormalised — the reference's own `db._fuse_query_embedding`), wired
   into `search_memories_budgeted` ahead of `semantic_search_scored`.
   One deliberate simplification: the reference coalesces concurrent
   callers racing the same uncached query behind a `threading.Event`, so two
   threads never both pay a redundant generation; this port omits that
   (cache-only, no coalescing) since it changes resource cost, not any
   caller-visible result — see the module's own doc comment if that ever
   needs revisiting. Ollama was not running on this host to verify against
   a real LLM, so the HTTP client contract (request shape, response
   parsing, timeout/failure fallback, cache hit avoiding a second request)
   is covered by 11 tests against a fake `/api/generate` server
   (`tests/query_expansion_test.rs`), the same pattern
   `ollama_embedder_test.rs` already established for `OllamaEmbedder`. The
   fusion math and its effect on real rankings is covered separately
   (`tests/vectors_test.rs`): a fake embedder proves an expansion text can
   flip which of two candidates wins over the plain query alone.

3. **Embeddings — now closed.** Added `remind_me_core::embedder::OnnxEmbedder`
   (`REMIND_ME_EMBEDDING_BACKEND=onnx`), an in-process ONNX bi-encoder via
   `rten` — the same pure-Rust runtime `reranker`/`ocr` already use, behind
   a new `local-embed` feature — mean-pooled over non-padding tokens and
   L2-normalised, matching the reference's `_Embedder._embed_forward`
   exactly. Takes explicit `REMIND_ME_ONNX_MODEL_PATH`/
   `REMIND_ME_ONNX_TOKENIZER_PATH` (`.rten` + `tokenizer.json`), same
   no-implicit-download convention as `rerank`. Converted the reference's
   already-cached `sentence-transformers/all-MiniLM-L6-v2` ONNX export the
   same way as the cross-encoder above (`rten-convert --no-infer-shapes`);
   kept at `~/.remind-me/embed/all-MiniLM-L6-v2.rten` +
   `~/.remind-me/embed/tokenizer.json`. Verified against real weights
   (`cargo run -p remind_me_core --example verify_onnx_embed --features
   local-embed`, deleted after use): 384-dimensional output, norms ~1.0,
   and a real semantic gap — cosine similarity 0.53 between the query and
   an on-topic passage vs. 0.016 against an off-topic one. `resolve_embedder`/
   `available_embedder` now return `Box<dyn Embedder>` instead of the
   concrete `OllamaEmbedder` so both backends share one call path — every
   existing caller kept working via `&*embedder`/`&**embedder` at the few
   sites that needed it (`Box<dyn Embedder>` does not `as`-cast to
   `&dyn Embedder` directly; deref does). 6 tests cover backend dispatch and
   graceful-failure paths for both build configurations
   (`tests/onnx_embedder_test.rs`), plus 4 new `tests/vectors_test.rs` cases
   for `fuse_query_embedding` itself; the full existing `vectors_test.rs`/
   `reranker_test.rs`/`ollama_embedder_test.rs`/`injectable_embedder_test.rs`
   suites (83 tests total across the six directly-touched files) pass
   unchanged.

Cutover on this host is no longer blocked by a missing Rust capability.
What remains before actually cutting it over: rebuild with
`--features "remind_me_core/rerank remind_me_core/local-embed"`, point
`REMIND_ME_RERANK_MODEL_PATH`/`_TOKENIZER_PATH` and
`REMIND_ME_ONNX_MODEL_PATH`/`_TOKENIZER_PATH` at the converted files above,
and re-verify `REMIND_ME_QUERY_EXPANSION=hyde` against a real Ollama daemon
(not done here — none was running on this host). The live Python process
was **not** touched while closing these gaps — see the Runbook below when
it is time to actually cut it over.

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
