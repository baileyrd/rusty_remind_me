# `rusty_remind_me`

> High-performance, persistent long-term memory engine and Model Context Protocol (MCP) server written in Rust, built on the **Rusty Mill** ecosystem.

`rusty_remind_me` is a native Rust port of `remind-me`. It equips AI assistants (such as Claude Desktop, Antigravity, Cursor, OpenAI Codex, and custom LLM agents) with persistent, searchable memory using SQLite FTS5 full-text search, ACT-R inspired memory vitality decay, Reciprocal Rank Fusion (RRF) search ranking, a structured Knowledge Graph entity system, and automated markdown wiki compilation.

---

## Key Features

- **Hybrid Search Engine**: FTS5 BM25 keyword matching combined with Reciprocal Rank Fusion (RRF) rank scoring.
- **ACT-R Memory Vitality Model**: Time-based exponential decay, write-time priors by category/source, and bridge protection (decay rate halved for frequently accessed memories).
- **Forward-Compatible Model Context Protocol (MCP) Server**: Stdio JSON-RPC MCP server with dynamic protocol version negotiation (supporting `2024-11-05` through upcoming 2026 releases), resources, prompts, dynamic tool change notifications (`listChanged`), and tool execution.
- **Automated Client Setup**: Built-in `configure` command & scripts for 1-click MCP setup across **Claude Desktop**, **Antigravity**, **Cursor**, and **Codex**.
- **REST API Server**: Async HTTP daemon (`rusty_http` / `tokio`) exposing endpoints for health checks, memory storage, FTS5 retrieval, and stats.
- **Knowledge Graph Entities**: Canonical entity deduplication, alias resolution, and relation traversal up to 3 hops.
- **Markdown Wiki Synthesis**: Topic-based wiki page compilation, queryable topic search, and
  bulk import of Markdown directories (`wiki-import`) — the ingestion path for
  [`dbs export-wiki`](https://github.com/baileyrd/Daily-Backup-System).
- **Rusty Mill Integration**: Zero-dependency philosophy built on `c:\dev\Rusty_Mill` crates (`rusty_tokio`, `rusty-db`, `rusty_json`, `rusty-search`, `rusty_http`, `rusty_lines`, `rusty_term`, `rusty_time`, `rusty_config`).

---

## Workspace Structure

`rusty_remind_me` is organized as a Cargo workspace containing four modular crates:

```
rusty_remind_me/
├── Cargo.toml                  # Workspace manifest linking to ../Rusty_Mill crates
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

### 3. REST API Server Daemon
Starts the async HTTP REST server listening on the specified port (default: 8080):
```bash
rusty-remind-me api 8080
```

### 4. Adding Memories
Stores a new memory note, fact, or preference in SQLite with automatic FTS5 indexing:
```bash
rusty-remind-me add "User prefers dark mode and Rust for low-level server development"
rusty-remind-me add "Deploy runbook lives in the ops wiki" --category engineering --tags ops,runbook
```

`--category` defaults to `general`; `--tags` is comma-separated and drops blanks. Use `--` before content that starts with dashes.

### 5. Searching Memories
Executes an FTS5 BM25 search with RRF rank fusion and ACT-R vitality scoring:
```bash
rusty-remind-me search "dark mode Rust"
rusty-remind-me search "dark mode Rust" --limit 5 --json
```

Output is Markdown by default — the same rendering `list` uses — with `--json` returning the full results including scores.

### 6. Fetching Memory by ID
Retrieves a specific memory record by its primary key:
```bash
rusty-remind-me get mem_4b307c2bd6ec4deb8c891a4b28cc592a
```

### 7. Knowledge Graph Entity Management
Upserts an entity with optional category/kind classification:
```bash
rusty-remind-me entity "Bailey Robertson" "person"
```

### 8. Markdown Wiki Synthesis
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

### 9. System Statistics
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
