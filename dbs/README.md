# rusty_dbs

A Rust reimplementation of
[baileyrd/Daily-Backup-System](https://github.com/baileyrd/Daily-Backup-System)
(pinned `@6cc6491`): incremental, multi-source backups into a local SQLite
database, with a CLI, an optional local web UI, and 14 connectors.

## Status

Full feature parity with the pinned reference — every module in
[`gap-analysis.md`](./gap-analysis.md) is marked Done, including all 14
connectors, the CLI, the web UI, export formats, and the plugin/config
machinery described in [`docs/adr/`](./docs/adr/). See
[`RELEASE_NOTES.md`](./RELEASE_NOTES.md) for the detailed history of how it
got there.

## Getting started

```bash
# Build everything
cargo build --workspace

# Scaffold a config + .env.example, and initialize the database
cargo run -p dbs-cli -- init

# Edit dbs.toml to enable/configure sources, put secrets in .env
# (see `dbs sources add`, or hand-edit dbs.toml — a fully commented example
# with every connector type ships from `dbs init`)

# Run a backup
cargo run -p dbs-cli -- backup --all

# Inspect what's there
cargo run -p dbs-cli -- status
cargo run -p dbs-cli -- items --limit 20

# Optional local web UI (dashboard, setup wizard, browser-login capture)
cargo run -p dbs-cli -- serve
```

The `dbs` binary (`crates/dbs-cli`) is a thin renderer over `dbs-core`;
every subcommand — `init`, `backup`, `status`, `history`, `items`, `stats`,
`export`/`export-notes`/`export-profiles`/`export-wiki`, `verify`,
`restore`, `decrypt`, `doctor`, `update-ytdlp`, `maintain`, `schedule`,
`serve`, `capture`, `sources`, `connectors`, `research`, `version` — is
fully wired to real behavior. Run `dbs --help` or `dbs <subcommand>
--help` for the full flag reference.

Each `dbs-connector-<type>` crate builds its own standalone executable,
discovered at runtime (not compiled into `dbs` itself — see
[ADR-0001](./docs/adr/0001-dynamic-plugin-registry.md)). `cargo build
--workspace` builds all of them alongside `dbs`; `dbs connectors list`
shows what's discoverable on `PATH`/the configured connectors directory.

## Connectors

Raindrop, GitHub (stars/gists), Pinboard, Readwise, Mastodon, Bluesky,
Spotify, Pocket Casts, generic Podcast (RSS/Atom + OPML), Vimeo, Udemy,
Reddit, Skool, and YouTube. Each connector's config schema and
capabilities are inspectable via `dbs connectors describe <type>`; see
`dbs.toml`'s comments (written by `dbs init`) for a working example of
each.

## Architecture

See [ARCHITECTURE.md](./ARCHITECTURE.md) for boundaries, key decisions, and
data flow.

![System architecture: dbs-cli and dbs-web both drive dbs-core's BackupService, which spawns each dbs-connector-<type> as its own subprocess across a stdin/stdout JSON-line boundary, classifies and upserts results into SqliteStorage, and fans out to exporters, the CLI/API, and an optional webhook.](./docs/diagrams/architecture.svg)

![Use cases: an Operator configures sources, runs and schedules backups, browses, exports, verifies/restores, and researches a topic; a Scheduler can trigger a backup on its own; only "Pull via connector" and "Research a topic" reach outside the system, to a source's API and to NotebookLM respectively.](./docs/diagrams/use-cases.svg)

## Development

```bash
cargo build --workspace --all-targets
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
```

CI (`.github/workflows/ci-rust.yml`) runs the same four checks on every PR.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## Security

See [SECURITY.md](./SECURITY.md) to report a vulnerability.

## License

Internal — not for external distribution
