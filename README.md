# `rusty_remind_me`

> High-performance, persistent long-term memory engine and Model Context Protocol (MCP) server written in Rust, built on the **Rusty Mill** ecosystem.

`rusty_remind_me` is a native Rust port of `remind-me`. It equips AI assistants (such as Claude Desktop, Antigravity, Cursor, OpenAI Codex, and custom LLM agents) with persistent, searchable memory using SQLite FTS5 full-text search, ACT-R inspired memory vitality decay, Reciprocal Rank Fusion (RRF) search ranking, a structured Knowledge Graph entity system, and automated markdown wiki compilation.

---

## Key Features

- **Hybrid Search Engine**: FTS5 BM25 keyword matching combined with Reciprocal Rank Fusion (RRF) rank scoring.
- **Semantic Search, Reranking & Query Expansion**: Optional vector search (Ollama or an in-process ONNX bi-encoder), cross-encoder reranking, and HyDE query expansion — each off by default, each degrading to keyword-only on any failure.
- **ACT-R Memory Vitality Model**: Time-based exponential decay, write-time priors by category/source, and bridge protection (decay rate halved for frequently accessed memories).
- **Forward-Compatible Model Context Protocol (MCP) Server**: Stdio JSON-RPC MCP server with dynamic protocol version negotiation (supporting `2024-11-05` through upcoming 2026 releases), resources, prompts, dynamic tool change notifications (`listChanged`), and tool execution.
- **Automated Client Setup**: Built-in `configure` command & scripts for 1-click MCP setup across **Claude Desktop**, **Antigravity**, **Cursor**, and **Codex**.
- **REST API Server**: Async HTTP daemon (`rusty_http` / `tokio`) exposing endpoints for health checks, memory storage, FTS5 retrieval, and stats.
- **Multi-Node Sync & Remote Connector**: Push/pull sync against a hub or discovered peers (static list or Tailscale), plus a Streamable HTTP remote MCP connector (secret-path/bearer or OAuth 2.1) for network-reachable clients like claude.ai.
- **Knowledge Graph Entities**: Canonical entity deduplication, alias resolution, and relation traversal up to 3 hops.
- **Markdown Wiki Synthesis**: Topic-based wiki page compilation, queryable topic search, and
  bulk import of Markdown directories (`wiki-import`) — the ingestion path for
  [`dbs export-wiki`](https://github.com/baileyrd/Daily-Backup-System).
- **Rusty Mill Ecosystem**: Part of the `Rusty Mill` project family; no `rusty_*` crates are wired in as dependencies yet — current dependencies are ordinary crates.io crates (`serde`, `tokio`, `rusqlite`, `chrono`, `uuid`).

---

## Workspace Structure

`rusty_remind_me` is organized as a Cargo workspace containing six modular crates:

```
rusty_remind_me/
├── Cargo.toml                  # Workspace manifest (6 crates)
├── README.md                   # User & Developer guide
├── ARCHITECTURE.md             # Technical design & schema documentation
├── CONTRIBUTING.md             # Development & testing guidelines
├── docs/
│   └── CUTOVER.md              # Runbook: migrating clients from the Python reference to this port
├── scripts/
│   ├── configure_mcp.ps1       # PowerShell auto-configuration script for Windows
│   └── configure_mcp.py        # Cross-platform Python auto-configuration script
└── crates/
    ├── remind_me_core/         # Domain models, SQLite/FTS5 database, ACT-R decay, RRF ranking
    ├── remind_me_mcp/          # Stdio JSON-RPC MCP protocol engine & tool handlers
    ├── remind_me_api/          # Async REST HTTP server daemon (`rusty_http` + `tokio`)
    ├── remind_me_remote/       # Remote MCP connector over Streamable HTTP (claude.ai, OAuth 2.1)
    ├── remind_me_hub/          # Multi-node sync hub: push/pull relay + Tailscale peer discovery
    └── remind_me_cli/          # Unified `rusty-remind-me` binary executable
```

---

## Automated Client Setup (Claude Desktop, Antigravity, Cursor, Codex)

You can automatically configure all installed AI client applications (Claude Desktop, Antigravity, Cursor, Codex / Generic MCP) to use `rusty_remind_me` as their memory backend:

### Option A: Via Built-in CLI Command
```bash
cargo run --bin rusty-remind-me -- configure
```

### Option B: Via PowerShell Script (Windows)
```powershell
.\scripts\configure_mcp.ps1
```

### Option C: Via Python Script (Cross-Platform)
```bash
python scripts/configure_mcp.py
```

Each setup option safely merges the `"rusty-remind-me"` MCP server configuration into:
- **Claude Desktop**: `%APPDATA%\Claude\claude_desktop_config.json`
- **Antigravity**: `%USERPROFILE%\.gemini\antigravity\mcp_config.json`
- **Cursor**: `%USERPROFILE%\.cursor\mcp.json`
- **Codex / Generic**: `%USERPROFILE%\.mcp\config.json`

---

## Database Location

By default the store is `~/.remind-me/memory.db` — the same path [`remind_me`](https://github.com/baileyrd/remind_me) uses, so an unconfigured install of either one opens the same database.

Two environment variables override it, most specific first:

| Variable | Names a | Notes |
| --- | --- | --- |
| `REMIND_ME_DB_PATH` | database **file** | Wins if both are set. Specific to this implementation. |
| `REMIND_ME_MCP_DIR` | **directory** holding `memory.db` | Shared with `remind_me`. Set this to point both implementations at one store. |

A leading `~` is expanded in either. A variable set to the empty string counts as unset.

To share a database with `remind_me`, set `REMIND_ME_MCP_DIR` only — `REMIND_ME_DB_PATH` has no meaning to `remind_me` and setting it there is silently ignored.

## Substituting for the `remind_me` MCP server

Two settings make this binary a drop-in replacement for [`remind_me`](https://github.com/baileyrd/remind_me)'s MCP server:

```bash
REMIND_ME_MCP_DIR=~/.remind-me                 # the same database (the default)
REMIND_ME_DEFAULT_RESPONSE_FORMAT=markdown     # the same output format
```

Or write both into every MCP client config at once:

```bash
rusty-remind-me configure --default-format markdown
```

`REMIND_ME_DEFAULT_RESPONSE_FORMAT` accepts `json` (the default) or `markdown`, and affects **only** the tools where `remind_me` has no `response_format` parameter at all — it returns Markdown from those and offers no JSON, whereas this port offers both and defaults to JSON so existing callers keep working.

Tools that mirror a `remind_me` input model already use that model's own default and are deliberately untouched by this setting: Markdown for `search`, `list`, `wiki_list`, `stats`, `history`, `digest` and `list_reminders`, JSON for `vitality_report`. Making `vitality_report` render Markdown because you asked for "markdown defaults" would move this port *away* from the reference.

A per-call `"response_format"` argument always wins over the setting, in both directions.

**Migrating an already-running client from `remind_me` to this binary?** A
stdio client will not pick up a config change until it restarts — see
`docs/CUTOVER.md` for the full runbook and the lessons learned cutting over
every consumer on a real machine.

## Multi-Node Sync, Hub & Remote Connector

Two additional crates extend a single-machine install into a synced fleet and
an internet-reachable MCP endpoint. Both are **off by default** — nothing
here changes single-machine behavior unless the corresponding environment
variables are set.

### Multi-node sync (`remind_me_hub`, `remind_me_core::sync`)

A node pushes its outbox to a hub and pulls the hub's changes back; peers are
also discovered directly (a static list, or Tailscale's local API) for
peer-to-peer push. Sync stays off until `REMIND_ME_NODE_ID`,
`REMIND_ME_HUB_URL`, and `REMIND_ME_SYNC_SECRET` are all set.

| Variable | Purpose | Default |
| --- | --- | --- |
| `REMIND_ME_NODE_ID` | This node's identity, stamped on every row it writes | unset (sync off) |
| `REMIND_ME_CLIENT` | Human-readable label for this install, alongside `node_id` | `unknown` |
| `REMIND_ME_HUB_URL` | Hub this node pushes to / pulls from | unset (sync off) |
| `REMIND_ME_SYNC_SECRET` | Bearer token for `/sync/push` and `/sync/pull`, sent and required | unset (sync off) |
| `REMIND_ME_SYNC_INTERVAL` | Seconds between background sync cycles | `60` |
| `REMIND_ME_PEER_BIND` | Bind address for this node's own peer server (accepts another node's push/pull) | `0.0.0.0` — all interfaces; narrow to `127.0.0.1` behind a tunnel-only setup |
| `REMIND_ME_PEER_PORT` | Port for the peer server above, and the port every discovered peer is assumed to listen on | `8766` |

`rusty-remind-me configure --node-id ID --hub-url URL [--peer-port N] [--sync-interval SECS]`
writes both the MCP entry and this sync environment for every configured
client in one command. There is deliberately no `--secret` flag — it is read
from `REMIND_ME_SYNC_SECRET` only, so it never lands in argv (`/proc`, shell
history).

### Remote MCP connector (`remind_me_remote`, `rusty-remind-me remote`)

Serves the MCP server over Streamable HTTP instead of stdio, for connectors
that need a network endpoint — e.g. `claude.ai`'s custom connector. Supports
a legacy secret-path/bearer token and, when an issuer is configured, OAuth
2.1.

Speaks both Streamable HTTP lifecycles `rmcp` implements: the
session-managed one every client through protocol version `2025-11-25`
uses (`Mcp-Session-Id`, including `mcp-remote`-fronted clients like Claude
Desktop) keeps working unchanged, and SEP-2567's newer per-request
"discover" lifecycle for `2026-07-28`+ clients, which can call a tool in a
single POST with no `initialize`/session at all. Which one a given request
gets is decided per request from its negotiated protocol version, not by
configuration.

| Variable | Purpose | Default |
| --- | --- | --- |
| `REMIND_ME_REMOTE_MCP` | Enables the connector (`1`/`true`/`yes`) | disabled |
| `REMIND_ME_REMOTE_HOST` | Bind address | `127.0.0.1` |
| `REMIND_ME_REMOTE_PORT` | Bind port | `8768` |
| `REMIND_ME_REMOTE_TOKEN` | Connector bearer token; always wins when set | auto-generated on first use and persisted to disk |
| `REMIND_ME_REMOTE_ISSUER` | Public HTTPS origin for OAuth 2.1 discovery | unset (OAuth off; secret-path/bearer only) |

Loopback by default — expose it through an HTTPS tunnel (e.g. Tailscale
Funnel) rather than widening the bind address directly.

## Search Quality: Embeddings, Reranking & Query Expansion

Three independent, optional layers on top of FTS5 keyword search. Each is
**off by default** and degrades to keyword-only (or its next-cheapest
fallback) on any failure — a missing daemon, an unconfigured model, or a
disabled build feature is never a reason a search should fail.

### Semantic search (`remind_me_core::embedder`)

`REMIND_ME_EMBEDDING_BACKEND` selects the backend; unset, semantic search is
off entirely.

| Backend | `REMIND_ME_EMBEDDING_BACKEND` | Needs |
| --- | --- | --- |
| Ollama | `ollama` | A running Ollama daemon (`REMIND_ME_OLLAMA_URL`, default `http://localhost:11434`; `REMIND_ME_OLLAMA_EMBED_MODEL`, default `nomic-embed-text`) |
| In-process ONNX | `onnx` | Build with `--features remind_me_core/local-embed`; `REMIND_ME_ONNX_MODEL_PATH`/`REMIND_ME_ONNX_TOKENIZER_PATH` pointing at a `.rten` bi-encoder + its `tokenizer.json` — no implicit download, same convention as reranking below |

`REMIND_ME_EMBEDDING_DIM` (default `384`, sized for
`sentence-transformers/all-MiniLM-L6-v2`) must match the configured model's
actual output dimension.

### Cross-encoder reranking (`remind_me_core::reranker`)

Rescores the head of the RRF-ranked list with a cross-encoder for more
precise ordering than keyword/semantic fusion alone. Build with
`--features remind_me_core/rerank`; `REMIND_ME_RERANK` defaults to on but
does nothing until `REMIND_ME_RERANK_MODEL_PATH`/
`REMIND_ME_RERANK_TOKENIZER_PATH` name a `.rten` cross-encoder and its
`tokenizer.json` — same no-implicit-download rule.

### Query expansion (`remind_me_core::query_expansion`)

`REMIND_ME_QUERY_EXPANSION=hyde` enables HyDE (Hypothetical Document
Embeddings): a local LLM writes a short passage that would plausibly answer
the query, and that passage's embedding is fused with the query's own
before the semantic search runs — helps most on questions phrased nothing
like the memory that answers them. Generation uses the same Ollama daemon
the `ollama` embedding backend talks to.

| Variable | Purpose | Default |
| --- | --- | --- |
| `REMIND_ME_QUERY_EXPANSION` | `hyde` to enable | unset (off) |
| `REMIND_ME_HYDE_MODEL` | Ollama model that writes the passage | `llama3.2` |
| `REMIND_ME_HYDE_TIMEOUT` | Seconds before falling back to the plain query | `15` |

### Converting a HuggingFace ONNX export to `.rten`

Both `onnx` embeddings and reranking take `rten`-format models, not raw
ONNX. If a model's HuggingFace repo already ships an `onnx/model.onnx`
export (most `sentence-transformers`/cross-encoder repos do), convert it
directly — no PyTorch, no re-download:

```bash
pip install rten-convert
python -m rten_convert path/to/model.onnx path/to/model.rten
```

On Windows, `rten-convert` 0.22.0's default shape-inference step raises a
`PermissionError` (`tempfile.NamedTemporaryFile` stays open while its own C
extension tries to reopen the same path) — add `--no-infer-shapes` to skip
it; that costs a runtime graph optimization, not correctness.

## Command Line Interface (CLI)

The compiled binary `rusty-remind-me` provides subcommands for interactive management, MCP stdio protocol hosting, and REST API daemon mode:

### 1. Stdio MCP Server (Default)
Starts the stdio JSON-RPC MCP server for integration with MCP clients (Claude Desktop, Antigravity, Cursor, etc.):
```bash
rusty-remind-me server
# OR simply run without arguments:
rusty-remind-me
```

### 2. Auto-Configuration
Configures all installed MCP client applications:
```bash
rusty-remind-me configure
```

`configure` also accepts `--node-id ID --hub-url URL` (writes the sync
environment too — see [Multi-Node Sync, Hub & Remote
Connector](#multi-node-sync-hub--remote-connector) below), plus `--peer-port
N`, `--sync-interval SECS`, `--db-path PATH`, and `--default-format
json|markdown`. The sync secret always comes from `REMIND_ME_SYNC_SECRET`,
never a flag — argv is world-readable via `/proc` and shell history.

### 3. REST API Server Daemon
Starts the async HTTP REST server listening on the specified port (default: 8080):
```bash
rusty-remind-me api 8080
```

### 4. Remote MCP Connector
Serves the MCP server over Streamable HTTP for network-reachable clients
(e.g. `claude.ai`'s custom connector), instead of stdio:
```bash
REMIND_ME_REMOTE_MCP=1 rusty-remind-me remote
```
Off by default; binds `127.0.0.1:8768` unless `REMIND_ME_REMOTE_HOST` /
`REMIND_ME_REMOTE_PORT` are set. See [Multi-Node Sync, Hub & Remote
Connector](#multi-node-sync-hub--remote-connector) above for the full
environment variable reference.

### 5. Adding Memories
Stores a new memory note, fact, or preference in SQLite with automatic FTS5 indexing:
```bash
rusty-remind-me add "User prefers dark mode and Rust for low-level server development"
rusty-remind-me add "Deploy runbook lives in the ops wiki" --category engineering --tags ops,runbook
```

`--category` defaults to `general`; `--tags` is comma-separated and drops blanks. Use `--` before content that starts with dashes.

### 6. Searching Memories
Executes an FTS5 BM25 search with RRF rank fusion and ACT-R vitality scoring:
```bash
rusty-remind-me search "dark mode Rust"
rusty-remind-me search "dark mode Rust" --limit 5 --json
```

Output is Markdown by default — the same rendering `list` uses — with `--json` returning the full results including scores.

### 7. Fetching Memory by ID
Retrieves a specific memory record by its primary key:
```bash
rusty-remind-me get mem_4b307c2bd6ec4deb8c891a4b28cc592a
```

### 8. Knowledge Graph Entity Management
Upserts an entity with optional category/kind classification:
```bash
rusty-remind-me entity "Bailey Robertson" "person"
```

### 9. Markdown Wiki Synthesis
Creates or updates a structured wiki topic page:
```bash
rusty-remind-me wiki-write architecture "System Architecture" "Rusty Remind Me is a high performance memory engine written in Rust."
```

Reads a wiki page by slug:
```bash
rusty-remind-me wiki-read architecture
```

Imports a whole directory of Markdown files into the wiki (recursively):
```bash
rusty-remind-me wiki-import ./wiki-pages
```

Each file's `slug`, `title` and `topic` are read from its YAML front matter,
which is stripped from the stored `content` since those three become columns.
Any field that is absent falls back independently — `title` from the file's
first `# ` heading then its filename, `slug` derived from the resolved title,
`topic` to `general` — so a hand-written note imports without ceremony. The
import upserts on `slug`, so re-importing an updated directory revises pages
in place rather than duplicating them.

This pairs with [`daily-backup-system`](https://github.com/baileyrd/Daily-Backup-System),
whose `dbs export-wiki --out-dir DIR` writes exactly this layout — one page per
backup source and per tag, cross-linked with `[[wikilinks]]`:
```bash
dbs export-wiki --out-dir ./wiki-pages    # in the dbs repo
rusty-remind-me wiki-import ./wiki-pages
```

### 10. System Statistics
Prints total memory counts and database metrics:
```bash
rusty-remind-me stats
```

---

## REST API Endpoints

When running `rusty-remind-me api [port]`, the HTTP server exposes the following endpoints:

| Method | Endpoint | Description | Request Body | Response |
| ------ | -------- | ----------- | ------------ | -------- |
| `GET` | `/health` | Server health check | N/A | `{"status": "ok", "version": "0.1.0"}` |
| `GET` | `/stats` | Memory count & statistics | N/A | `{"total_memories": 42}` |
| `POST` | `/api/v1/memories` | Store a new memory | `MemoryAddInput` JSON | Created `Memory` JSON |
| `POST` | `/api/v1/search` | Search memories | `MemorySearchInput` JSON | Array of `MemorySearchResult` JSON |

---

## License

MIT License. See [LICENSE](LICENSE) for details.
