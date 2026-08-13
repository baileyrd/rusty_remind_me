//! `dbs` — the command-line interface for rusty_dbs.
//!
//! Mirrors `src/dbs/cli.py`'s entry point in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`): a thin renderer over `dbs_core::service::BackupService`
//! — this binary is the only place permitted to read argv, print, or set
//! exit codes; every real behavior lives in `dbs-core` so a future web/API
//! layer can reuse it unchanged.
//!
//! **This issue (#63) is the CLI skeleton.** [`Command::Init`] is fully
//! wired; every other subcommand is a stub (prints a "not yet
//! implemented" notice and exits `1`) — each gets its own follow-up
//! issue per `gap-analysis.md`'s CLI cluster (`dbs backup` #64, `dbs
//! status`/`dbs history` #68, `dbs items`/`dbs stats` #69, `dbs
//! export*`/`dbs decrypt` #70, `dbs sources`/`dbs connectors` #71,
//! `dbs doctor` #72 — see the full row list for the rest). Subcommand
//! *names* and nesting (`sources`/`connectors`/`research` sub-apps)
//! are already wired to match the reference's surface, so those
//! issues only need to fill in flags and behavior, not invent new
//! dispatch.
//!
//! Exit codes, matching the reference's documented convention
//! (cron-friendly):
//! ```text
//! 0  all requested work succeeded
//! 2  at least one source ended `partial`
//! 3  at least one source `failed`
//! 4  configuration error
//! 5  no such source
//! ```
//! Stub subcommands use `1` (not one of the above) since they represent
//! no real outcome yet, not any of the reference's actual result codes.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use dbs_core::service::{BackupAllOptions, BackupService, BackupSourceOptions, ProgressSink};
use dbs_core::{
    load_config, parse_iso, write_scaffolding, BackupRunError, CancelToken, ConnectorRegistry,
    DbsError, ExportQuery, ItemRow, ProgressEvent, ProgressPhase, RunResult, RunStatus,
    SqliteStorage, Storage, SubprocessRunner, CURRENT_API_VERSION,
};

const STUB_EXIT_CODE: i32 = 1;
const CONFIG_ERROR_EXIT_CODE: i32 = 4;

#[derive(Parser)]
#[command(
    name = "dbs",
    version,
    about = "Daily Backup System — incremental, multi-source backups into SQLite.",
    propagate_version = true
)]
struct Cli {
    /// Path to the config file (TOML).
    #[arg(
        long,
        short = 'c',
        env = "DBS_CONFIG",
        default_value = "dbs.toml",
        global = true
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a config + .env.example and initialize the database. Idempotent.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Back up one source, or every enabled source with --all.
    Backup {
        /// Source name (omit with --all).
        source: Option<String>,
        /// Back up every enabled source.
        #[arg(long = "all")]
        all_sources: bool,
        /// With --all: skip a source whose schedule cadence hasn't elapsed.
        #[arg(long)]
        only_due: bool,
        /// With --all: run up to N sources concurrently (default: the
        /// config's `parallel` key, itself defaulting to 1).
        #[arg(long)]
        parallel: Option<u32>,
        /// Full refetch, ignore cursor.
        #[arg(long)]
        force_full: bool,
        /// Force a reconcile (edits + deletions).
        #[arg(long)]
        reconcile: bool,
        /// Show the chosen mode without running.
        #[arg(long)]
        dry_run: bool,
        /// Stop after N items (smoke tests / first-run bound).
        #[arg(long)]
        limit: Option<u32>,
        /// Show a live progress line on stderr (default: auto — on for a TTY).
        #[arg(long, conflicts_with = "no_progress")]
        progress: bool,
        /// Never show the live progress line.
        #[arg(long)]
        no_progress: bool,
    },
    /// Show each configured source's last-run summary.
    Status {
        /// Limit to one source (omit for every configured source).
        source: Option<String>,
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Show recent run history.
    History {
        /// Limit to one source (omit for every source).
        source: Option<String>,
        /// Show at most N runs.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: u32,
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Browse backed-up items, or show one item's full detail by id.
    Items {
        /// Show one item's full detail (raw payload + media list)
        /// instead of listing.
        #[arg(value_name = "ID")]
        item_id: Option<i64>,
        /// Filter by source name (repeatable).
        #[arg(long = "source")]
        source: Vec<String>,
        /// Filter by item kind (repeatable).
        #[arg(long = "type")]
        item_type: Vec<String>,
        /// Full-text search over titles and bodies.
        #[arg(long = "search", short = 'q')]
        search: Option<String>,
        /// Only items created on/after (YYYY-MM-DD or full ISO-8601).
        #[arg(long)]
        since: Option<String>,
        /// Only items created on/before.
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        include_deleted: bool,
        /// Page size.
        #[arg(long, short = 'n', default_value_t = 50)]
        limit: u32,
        /// Skip the first N matches (pagination).
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Aggregate item/source statistics.
    Stats {
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Export items to a file in the given format.
    Export {
        /// Output file (or .zip for archive/obsidian).
        #[arg(long = "out", short = 'o')]
        out: PathBuf,
        /// json|ndjson|csv|markdown|archive|obsidian|wiki.
        #[arg(long = "format", short = 'f', default_value = "ndjson")]
        format: String,
        /// Filter by source name (repeatable).
        #[arg(long = "source")]
        source: Vec<String>,
        /// Filter by item kind (repeatable).
        #[arg(long = "type")]
        item_type: Vec<String>,
        /// Only items created on/after (YYYY-MM-DD or full ISO-8601).
        #[arg(long)]
        since: Option<String>,
        /// Only items created on/before.
        #[arg(long)]
        until: Option<String>,
        /// Only items updated on/after — independent of --since.
        #[arg(long = "since-updated")]
        since_updated: Option<String>,
        /// Only items updated on/before.
        #[arg(long = "until-updated")]
        until_updated: Option<String>,
        #[arg(long)]
        include_deleted: bool,
        /// (archive) full revision history.
        #[arg(long)]
        include_revisions: bool,
        /// Omit verbatim raw payloads.
        #[arg(long)]
        no_raw: bool,
        /// (wiki) 'topic' for hub pages, 'item' for one page per item.
        #[arg(long = "wiki-grouping", default_value = "topic")]
        wiki_grouping: String,
        /// Encrypt the output with a passphrase (scrypt + AES-256-GCM).
        #[arg(long)]
        encrypt: bool,
        /// Env var (or .env key) holding the passphrase.
        #[arg(long = "passphrase-env")]
        passphrase_env: Option<String>,
    },
    /// Incrementally export one Markdown note per item into a directory.
    #[command(name = "export-notes")]
    ExportNotes {
        /// Directory to write one Markdown note per item into.
        #[arg(long = "out-dir", short = 'd')]
        out_dir: PathBuf,
        /// Filter by source name (repeatable).
        #[arg(long = "source")]
        source: Vec<String>,
        /// Filter by item kind (repeatable).
        #[arg(long = "type")]
        item_type: Vec<String>,
        /// Only items created on/after — overrides the incremental state file.
        #[arg(long)]
        since: Option<String>,
        /// Ignore the incremental state file; consider every live item.
        #[arg(long)]
        full: bool,
    },
    /// Show each source's resolved export profile.
    #[command(name = "export-profiles")]
    ExportProfiles {
        /// Machine-readable output.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Export the wiki format's pages loose into a directory.
    #[command(name = "export-wiki")]
    ExportWiki {
        /// Directory to write loose wiki pages into.
        #[arg(long = "out-dir", short = 'd')]
        out_dir: PathBuf,
        /// 'topic' for hub pages, 'item' for one page per item.
        #[arg(long, default_value = "topic")]
        grouping: String,
        /// Filter by source name (repeatable).
        #[arg(long = "source")]
        source: Vec<String>,
        /// Filter by item kind (repeatable).
        #[arg(long = "type")]
        item_type: Vec<String>,
        /// Only items created on/after.
        #[arg(long)]
        since: Option<String>,
    },
    /// Check database integrity and per-source state, or an archive's checksums.
    Verify,
    /// Replay an exported backup into the database.
    Restore,
    /// Decrypt a `dbs export --encrypt`-produced bundle.
    Decrypt {
        /// A file written by `dbs export --encrypt`.
        src: PathBuf,
        /// Destination (default: SRC minus its .enc suffix, else SRC + .plain).
        #[arg(long = "out", short = 'o')]
        out: Option<PathBuf>,
        /// Env var (or .env key) holding the passphrase.
        #[arg(long = "passphrase-env")]
        passphrase_env: Option<String>,
    },
    /// Run environment/dependency health checks.
    Doctor {
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Update the bundled yt-dlp build.
    #[command(name = "update-ytdlp")]
    UpdateYtdlp {
        /// Print the command; run nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run scheduled maintenance (VACUUM, revision pruning, ...).
    Maintain,
    /// Print a cron/systemd (or Task Scheduler) snippet for unattended runs.
    Schedule {
        /// cron preset: daily|hourly.
        #[arg(long, default_value = "daily")]
        interval: String,
    },
    /// Run the optional local web UI.
    Serve {
        /// Bind address.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to listen on.
        #[arg(long, short = 'p', default_value_t = 8000)]
        port: u16,
        /// In-UI setup actions (install connector deps, browser login
        /// capture). On by default for local use; pass --no-setup to
        /// disable.
        #[arg(long, default_value_t = true, conflicts_with = "no_setup")]
        allow_setup: bool,
        /// Disable in-UI setup actions.
        #[arg(long)]
        no_setup: bool,
        /// Require this bearer token on every API call. Mandatory when
        /// binding to a non-localhost address.
        #[arg(long)]
        token: Option<String>,
        /// Run backups automatically while the server is up.
        #[arg(long, conflicts_with = "no_schedule")]
        schedule: bool,
        /// Explicitly disable automatic backups (the default).
        #[arg(long)]
        no_schedule: bool,
    },
    /// Capture a login session on this machine, for import into a
    /// headless server.
    Capture {
        /// Connector type or configured source name to capture a login for.
        target: String,
        /// Where to write the captured artifact. Defaults to
        /// ./<target>-cookies.txt / -storage_state.json / -session.zip
        /// depending on the capture kind.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
    },
    /// Print the installed version.
    Version,
    /// Manage configured sources.
    #[command(subcommand)]
    Sources(SourcesCommand),
    /// Inspect available connectors.
    #[command(subcommand)]
    Connectors(ConnectorsCommand),
    /// Ad-hoc research pipelines (not backups).
    #[command(subcommand)]
    Research(ResearchCommand),
}

#[derive(Subcommand)]
enum SourcesCommand {
    /// List configured sources.
    List {
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
    },
    /// Add a source to the config (validated against the connector's schema).
    Add {
        name: String,
        #[arg(long = "type", short = 't')]
        type_: String,
        /// Option as key=value (repeatable).
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// Check every configured source's connector loads and validates.
    Check,
}

#[derive(Subcommand)]
enum ConnectorsCommand {
    /// List every discovered connector.
    List {
        /// Emit JSON.
        #[arg(long = "json")]
        json_out: bool,
        /// Show load failures.
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// Describe one connector's config schema and capabilities.
    Describe {
        #[arg(value_name = "TYPE")]
        type_: String,
    },
}

#[derive(Subcommand)]
enum ResearchCommand {
    /// Search YouTube, feed videos into a NotebookLM notebook, write a
    /// markdown research report.
    Youtube {
        /// Research topic, e.g. "claude code skills".
        topic: String,
        /// Search query variant (repeatable). Default: one query derived
        /// from TOPIC.
        #[arg(long, short = 'q')]
        query: Vec<String>,
        /// Results to fetch per search query.
        #[arg(long, default_value_t = 10)]
        per_query_count: u32,
        /// Final video count after dedup/rank.
        #[arg(long, default_value_t = 10)]
        count: u32,
        /// Recency filter in months; 0 disables it.
        #[arg(long, default_value_t = 6)]
        months: u32,
        /// Repeatable; replaces the default 5-question analysis set.
        #[arg(long)]
        question: Vec<String>,
        /// Also generate a NotebookLM infographic.
        #[arg(long)]
        infographic: bool,
        #[arg(long, default_value = "landscape")]
        infographic_orientation: String,
        /// Output markdown path (default: ./<slug>.md).
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
        #[arg(long)]
        notebook_name: Option<String>,
        /// NotebookLM storageState JSON (default: the web UI's captured
        /// login, else `notebooklm login`'s own file).
        #[arg(long)]
        auth_state: Option<PathBuf>,
    },
    /// Send already backed-up YouTube videos through NotebookLM and write
    /// a markdown research report.
    #[command(name = "youtube-backup")]
    YoutubeBackup {
        /// Research topic, e.g. "claude code skills".
        topic: String,
        /// Configured YouTube source name (repeatable). Default: every
        /// youtube source.
        #[arg(long, short = 's')]
        source: Vec<String>,
        /// Only videos from this list (watch-later, liked,
        /// playlist:<title>). Repeatable.
        #[arg(long, short = 'l')]
        list: Vec<String>,
        /// Max videos to send to NotebookLM.
        #[arg(long, default_value_t = 10)]
        count: u32,
        /// Repeatable; replaces the default 5-question analysis set.
        #[arg(long)]
        question: Vec<String>,
        /// Also generate a NotebookLM infographic.
        #[arg(long)]
        infographic: bool,
        #[arg(long, default_value = "landscape")]
        infographic_orientation: String,
        /// Output markdown path (default: ./<slug>.md).
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
        #[arg(long)]
        notebook_name: Option<String>,
        /// NotebookLM storageState JSON (default: the web UI's captured
        /// login, else `notebooklm login`'s own file).
        #[arg(long)]
        auth_state: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Init { force } => cmd_init(&cli.config, force),
        Command::Backup {
            source,
            all_sources,
            only_due,
            parallel,
            force_full,
            reconcile,
            dry_run,
            limit,
            progress,
            no_progress,
        } => cmd_backup(
            &cli.config,
            source,
            all_sources,
            only_due,
            parallel,
            force_full,
            reconcile,
            dry_run,
            limit,
            progress,
            no_progress,
        ),
        Command::Status { source, json_out } => cmd_status(&cli.config, source, json_out),
        Command::History {
            source,
            limit,
            json_out,
        } => cmd_history(&cli.config, source, limit, json_out),
        Command::Items {
            item_id,
            source,
            item_type,
            search,
            since,
            until,
            include_deleted,
            limit,
            offset,
            json_out,
        } => cmd_items(
            &cli.config,
            item_id,
            source,
            item_type,
            search,
            since,
            until,
            include_deleted,
            limit,
            offset,
            json_out,
        ),
        Command::Stats { json_out } => cmd_stats(&cli.config, json_out),
        Command::Export {
            out,
            format,
            source,
            item_type,
            since,
            until,
            since_updated,
            until_updated,
            include_deleted,
            include_revisions,
            no_raw,
            wiki_grouping,
            encrypt,
            passphrase_env,
        } => cmd_export(
            &cli.config,
            out,
            format,
            source,
            item_type,
            since,
            until,
            since_updated,
            until_updated,
            include_deleted,
            include_revisions,
            no_raw,
            wiki_grouping,
            encrypt,
            passphrase_env,
        ),
        Command::ExportNotes {
            out_dir,
            source,
            item_type,
            since,
            full,
        } => cmd_export_notes(&cli.config, out_dir, source, item_type, since, full),
        Command::ExportProfiles { json_out } => cmd_export_profiles(&cli.config, json_out),
        Command::ExportWiki {
            out_dir,
            grouping,
            source,
            item_type,
            since,
        } => cmd_export_wiki(&cli.config, out_dir, grouping, source, item_type, since),
        Command::Decrypt {
            src,
            out,
            passphrase_env,
        } => cmd_decrypt(&cli.config, src, out, passphrase_env),
        Command::Sources(sub) => match sub {
            SourcesCommand::List { json_out } => cmd_sources_list(&cli.config, json_out),
            SourcesCommand::Add { name, type_, set } => {
                cmd_sources_add(&cli.config, name, type_, set)
            }
            SourcesCommand::Check => cmd_sources_check(&cli.config),
        },
        Command::Connectors(sub) => match sub {
            ConnectorsCommand::List { json_out, verbose } => {
                cmd_connectors_list(&cli.config, json_out, verbose)
            }
            ConnectorsCommand::Describe { type_ } => cmd_connectors_describe(&cli.config, type_),
        },
        Command::Doctor { json_out } => cmd_doctor(&cli.config, json_out),
        Command::UpdateYtdlp { dry_run } => cmd_update_ytdlp(dry_run),
        Command::Schedule { interval } => cmd_schedule(&cli.config, &interval),
        Command::Serve {
            host,
            port,
            allow_setup,
            no_setup,
            token,
            schedule,
            no_schedule: _,
        } => cmd_serve(host, port, allow_setup && !no_setup, token, schedule),
        Command::Capture { target, out } => cmd_capture(&cli.config, &target, out),
        Command::Research(sub) => match sub {
            ResearchCommand::Youtube {
                topic,
                query,
                per_query_count,
                count,
                months,
                question,
                infographic,
                infographic_orientation,
                out,
                notebook_name,
                auth_state,
            } => cmd_research_youtube(YoutubeResearchArgs {
                topic,
                query,
                per_query_count,
                count,
                months,
                question,
                infographic,
                infographic_orientation,
                out,
                notebook_name,
                auth_state,
            }),
            ResearchCommand::YoutubeBackup {
                topic,
                source,
                list,
                count,
                question,
                infographic,
                infographic_orientation,
                out,
                notebook_name,
                auth_state,
            } => cmd_research_youtube_backup(
                &cli.config,
                YoutubeBackupResearchArgs {
                    topic,
                    source,
                    list,
                    count,
                    question,
                    infographic,
                    infographic_orientation,
                    out,
                    notebook_name,
                    auth_state,
                },
            ),
        },
        Command::Version => cmd_version(),
        other => cmd_stub(command_name(&other)),
    };
    std::process::exit(code);
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Backup { .. } => "backup",
        Command::Status { .. } => "status",
        Command::History { .. } => "history",
        Command::Items { .. } => "items",
        Command::Stats { .. } => "stats",
        Command::Export { .. } => "export",
        Command::ExportNotes { .. } => "export-notes",
        Command::ExportProfiles { .. } => "export-profiles",
        Command::ExportWiki { .. } => "export-wiki",
        Command::Verify => "verify",
        Command::Restore => "restore",
        Command::Decrypt { .. } => "decrypt",
        Command::Doctor { .. } => "doctor",
        Command::UpdateYtdlp { .. } => "update-ytdlp",
        Command::Maintain => "maintain",
        Command::Schedule { .. } => "schedule",
        Command::Serve { .. } => "serve",
        Command::Capture { .. } => "capture",
        Command::Version => "version",
        Command::Sources(_) => "sources",
        Command::Connectors(_) => "connectors",
        Command::Research(_) => "research",
    }
}

fn cmd_stub(name: &str) -> i32 {
    eprintln!("dbs {name}: not yet implemented (tracked in a follow-up issue)");
    STUB_EXIT_CODE
}

/// Mirrors the reference's `init` command: writes the config template
/// (unless it exists and `--force` wasn't given) and `.env.example`
/// (never overwritten), then initializes the database by running
/// migrations against the configured path.
fn cmd_init(config_path: &Path, force: bool) -> i32 {
    let scaffold = match write_scaffolding(config_path, force) {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };
    if scaffold.config_written {
        println!("Wrote {}", scaffold.config_path.display());
    } else {
        println!(
            "Config already exists: {} (use --force to overwrite)",
            scaffold.config_path.display()
        );
    }
    if scaffold.env_example_written {
        println!("Wrote {}", scaffold.env_example_path.display());
    }

    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    println!("Initialized database at {}", cfg.database);
    println!(
        "\nNext steps:\n\
         \x20 1. Copy .env.example to .env and fill in your tokens (e.g. RAINDROP_TOKEN).\n\
         \x20 2. Edit the config to enable/add sources.\n\
         \x20 3. Run:  dbs backup --all\n"
    );
    0
}

fn report_config_error(e: &DbsError) -> i32 {
    eprintln!("{e}");
    CONFIG_ERROR_EXIT_CODE
}

/// A transient live status line for `dbs backup`, written to *stderr* so
/// it never pollutes the results table on stdout. Mirrors the
/// reference's `_ProgressRenderer` — minus its spinner and item-count
/// throttling, since [`ProgressSink`]'s doc-comment explains why only
/// `SourceStart`/`SourceDone` are emitted today: with nothing between
/// them to animate against, a static line is more honest than a fake
/// spinner over a value that never changes.
struct ProgressRenderer {
    enabled: bool,
    dirty: AtomicBool,
}

impl ProgressRenderer {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            dirty: AtomicBool::new(false),
        }
    }

    fn draw(&self, ev: &ProgressEvent) {
        let pos = match (ev.source_index, ev.source_total) {
            (Some(i), Some(n)) => format!("[{i}/{n}] "),
            _ => String::new(),
        };
        eprint!("\r\x1b[K{pos}{} [{}] running\u{2026}", ev.source, ev.mode);
        let _ = std::io::stderr().flush();
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Wipes the line, if one is currently drawn. Safe to call more
    /// than once, and from the Ctrl+C handler thread.
    fn close(&self) {
        if self.dirty.swap(false, Ordering::Relaxed) {
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        }
    }
}

impl ProgressSink for ProgressRenderer {
    fn emit(&self, ev: &ProgressEvent) {
        if !self.enabled {
            return;
        }
        match ev.phase {
            ProgressPhase::SourceStart => self.draw(ev),
            ProgressPhase::SourceDone => self.close(),
            ProgressPhase::Item | ProgressPhase::Checkpoint | ProgressPhase::Sweep => {}
        }
    }
}

/// Routes Ctrl+C (SIGINT) into a graceful early stop, mirroring the
/// reference's `_install_stop_handler`. The first Ctrl+C cancels
/// `renderer`'s line and sets the returned [`CancelToken`] — for
/// `--all`, `BackupService::backup_all` stops starting new sources but
/// lets an in-flight one finish and commit; a second Ctrl+C aborts the
/// whole process immediately via `exit(130)` (matching the reference's
/// documented `KeyboardInterrupt`-on-second-signal behavior), since a
/// single in-flight connector call can't be interrupted mid-fetch
/// without the run/stream protocol (ADR-0001 steps 2-3, not yet
/// implemented — same gap [`ProgressSink`] documents).
fn install_stop_handler(renderer: Arc<ProgressRenderer>, all_sources: bool) -> CancelToken {
    let cancel = CancelToken::new();
    let handler_cancel = cancel.clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let result = ctrlc::set_handler(move || {
        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
            handler_cancel.cancel();
            renderer.close();
            let msg = if all_sources {
                "\nStopping \u{2014} the current source will finish, then no more \
                 start (Ctrl+C again to abort now)."
            } else {
                "\nStopping the current backup (Ctrl+C again to abort now)."
            };
            eprintln!("{msg}");
        } else {
            renderer.close();
            eprintln!("\nAborted.");
            std::process::exit(130);
        }
    });
    if result.is_err() {
        eprintln!("warning: could not install a Ctrl+C handler");
    }
    cancel
}

/// Mirrors the reference's `backup` command. `--parallel`/`--only-due`
/// (#65/#66), the progress line + Ctrl+C handling (#67), and — as of
/// issue #157 — the actual connector run/stream bridge
/// ([`dbs_core::SubprocessRunner`]) are wired; see [`ProgressSink`]'s
/// doc-comment for the one honest gap that remains (per-item progress
/// needs a richer wire protocol than #157 added — only start/done are
/// reported today).
///
/// No connector-candidate discovery mechanism exists yet (scanning for
/// installed connector subprocesses on disk — a real gap surfaced
/// while implementing #157, not yet its own issue), so the registry
/// this constructs is always empty: every configured source's
/// connector type is reported "not found" until that lands. That's
/// accurate to the current state of the port, not a bug in this
/// command — the "connector error surfaced to CLI output" acceptance
/// scenario is exactly this path. Separately, none of the 14 built-in
/// `dbs-connector-*` crates are real subprocess binaries yet (another
/// #157-surfaced gap) — even with real discovery wired in, there would
/// be nothing on disk for it to find.
#[allow(clippy::too_many_arguments)]
fn cmd_backup(
    config_path: &Path,
    source: Option<String>,
    all_sources: bool,
    only_due: bool,
    parallel: Option<u32>,
    force_full: bool,
    reconcile: bool,
    dry_run: bool,
    limit: Option<u32>,
    progress: bool,
    no_progress: bool,
) -> i32 {
    if !all_sources && source.is_none() {
        eprintln!("Specify a SOURCE name or --all.");
        return CONFIG_ERROR_EXIT_CODE;
    }

    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }

    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let mut service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let show_progress = if no_progress {
        false
    } else if progress {
        true
    } else {
        std::io::stderr().is_terminal()
    };
    let renderer = Arc::new(ProgressRenderer::new(show_progress));
    let cancel = install_stop_handler(Arc::clone(&renderer), all_sources);

    if all_sources {
        let opts = BackupAllOptions {
            only_due,
            continue_on_error: true,
            force_full,
            force_reconcile: reconcile,
            dry_run,
            limit,
            parallel,
            on_progress: Some(renderer.as_ref()),
            cancel: Some(cancel),
        };
        return match service.backup_all(&opts) {
            Ok(results) => {
                renderer.close();
                println!("Backup results:");
                for result in &results {
                    print_run(result);
                }
                exit_code(&results)
            }
            Err(e) => {
                renderer.close();
                report_config_error(&e)
            }
        };
    }

    let name = source.expect("checked above: source is Some when not --all");
    let opts = BackupSourceOptions {
        mode: "auto".to_string(),
        force_full,
        force_reconcile: reconcile,
        dry_run,
        limit,
        reap: true,
        cancel: Some(cancel),
        on_progress: Some(renderer.as_ref()),
    };

    match service.backup_source(&name, &opts) {
        Ok(result) => {
            renderer.close();
            println!("Backup results:");
            print_run(&result);
            exit_code(std::slice::from_ref(&result))
        }
        Err(DbsError::Run(BackupRunError::SourceLocked(e))) => {
            renderer.close();
            eprintln!("source already locked: {e}");
            2
        }
        Err(DbsError::Run(BackupRunError::UnknownSource(e))) => {
            renderer.close();
            eprintln!("unknown source: {e}");
            5
        }
        Err(e) => {
            renderer.close();
            report_config_error(&e)
        }
    }
}

/// Mirrors the reference's `status` command: one line per source
/// (`--json` for the raw `SourceStatus` list instead).
fn cmd_status(config_path: &Path, source: Option<String>, json_out: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let statuses = match service.status(source.as_deref()) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };

    if json_out {
        match serde_json::to_string_pretty(&statuses) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode status as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    if statuses.is_empty() {
        println!("No sources configured.");
        return 0;
    }
    for s in &statuses {
        println!(
            "{:<24} {:<10} {:<4} items={} (deleted {}) runs={} last={}",
            s.name,
            s.type_,
            if s.enabled { "on" } else { "off" },
            s.live_items,
            s.deleted_items,
            s.run_count,
            s.last_run_status.as_deref().unwrap_or("-"),
        );
        if s.has_interrupted_runs {
            println!("    ! has interrupted runs");
        }
    }
    0
}

/// Mirrors the reference's `history` command: one line per run, newest
/// first (`--json` for the raw run rows instead).
fn cmd_history(config_path: &Path, source: Option<String>, limit: u32, json_out: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let runs = match service.history(source.as_deref(), limit) {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };

    if json_out {
        match serde_json::to_string_pretty(&runs) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode history as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    for run in &runs {
        let get_str = |key: &str| run.get(key).and_then(|v| v.as_str()).unwrap_or("?");
        let get_i64 = |key: &str| run.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let items_failed = get_i64("items_failed");
        let failed = if items_failed > 0 {
            format!(" !{items_failed}")
        } else {
            String::new()
        };
        let duration = run
            .get("duration_ms")
            .and_then(|v| v.as_i64())
            .map(human_duration)
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{}  {:<20} {:<11} [{}] +{} ~{} x{}{failed}  {duration}",
            get_str("started_at"),
            get_str("source_name"),
            get_str("status"),
            get_str("mode"),
            get_i64("items_created"),
            get_i64("items_updated"),
            get_i64("items_deleted"),
        );
        if let Some(error) = run.get("error").and_then(|v| v.as_str()) {
            println!("    {error}");
        }
        if let Some(warnings) = run.get("warnings").and_then(|v| v.as_array()) {
            for w in warnings {
                if let Some(w) = w.as_str() {
                    println!("    warning: {w}");
                }
            }
        }
    }
    0
}

/// Parses a CLI date argument: full ISO-8601 first (via
/// `dbs_core::parse_iso`), falling back to a bare `YYYY-MM-DD` date —
/// mirrors the reference's `_parse_date`. `None`/empty input is `Ok(None)`;
/// unparseable text is `Err` with a message ready to print.
fn parse_date_arg(value: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    let Some(text) = value else { return Ok(None) };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some(dt) = parse_iso(Some(text)) {
        return Ok(Some(dt));
    }
    match chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        Ok(date) => Ok(Some(
            date.and_hms_opt(0, 0, 0)
                .expect("midnight is always a valid time")
                .and_utc(),
        )),
        Err(e) => Err(format!("Invalid date {text:?}: {e}")),
    }
}

/// Compact byte count, e.g. `"512 B"`, `"3.4 KiB"`, `"1.2 GiB"`. Mirrors
/// the reference's `_human_bytes`.
fn human_bytes(n: i64) -> String {
    let mut value = n as f64;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value < 1024.0 {
            return if unit == "B" {
                format!("{value:.0} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} TiB")
}

/// Mirrors the reference's `items` command: browse/search/filter by
/// default, or one item's full detail with `ID`.
#[allow(clippy::too_many_arguments)]
fn cmd_items(
    config_path: &Path,
    item_id: Option<i64>,
    source: Vec<String>,
    item_type: Vec<String>,
    search: Option<String>,
    since: Option<String>,
    until: Option<String>,
    include_deleted: bool,
    limit: u32,
    offset: u32,
    json_out: bool,
) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    if let Some(id) = item_id {
        let item = match service.get_item(id) {
            Ok(item) => item,
            Err(e) => return report_config_error(&e),
        };
        let Some(item) = item else {
            eprintln!("no such item {id}");
            return 1;
        };
        if json_out {
            match serde_json::to_string_pretty(&item) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("failed to encode item as JSON: {e}");
                    return CONFIG_ERROR_EXIT_CODE;
                }
            }
        } else {
            print_item_detail(&item);
        }
        return 0;
    }

    let since_dt = match parse_date_arg(since.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let until_dt = match parse_date_arg(until.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let query = ExportQuery {
        sources: if source.is_empty() {
            None
        } else {
            Some(source)
        },
        item_types: if item_type.is_empty() {
            None
        } else {
            Some(item_type)
        },
        since: since_dt,
        until: until_dt,
        include_deleted,
        ..Default::default()
    };
    let (rows, total) = match service.browse_items(&query, search.as_deref(), limit, offset) {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };

    if json_out {
        let envelope = serde_json::json!({
            "items": rows,
            "total": total,
            "limit": limit,
            "offset": offset,
        });
        match serde_json::to_string_pretty(&envelope) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode items as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    if rows.is_empty() {
        if total > 0 {
            println!("No items at offset {offset} ({total} total matches).");
        } else {
            println!("No items matched.");
        }
        return 0;
    }
    for r in &rows {
        let get_str = |key: &str| r.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let get_i64 = |key: &str| r.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let get_bool = |key: &str| r.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        let mut title = {
            let t = get_str("title");
            if !t.is_empty() {
                t
            } else {
                let u = get_str("url");
                if !u.is_empty() {
                    u
                } else {
                    get_str("external_id")
                }
            }
        }
        .replace('\n', " ");
        if title.chars().count() > 60 {
            title = title.chars().take(59).collect::<String>() + "\u{2026}";
        }
        let created = get_str("created_at");
        let created = if created.len() >= 10 {
            &created[..10]
        } else {
            created
        };
        let mut line = format!(
            "{:>7}  {:<20.20} {:<10.10} {created:<10}  {title}",
            get_i64("id"),
            get_str("source"),
            get_str("item_kind"),
        );
        let media_count = get_i64("media_count");
        if media_count > 0 {
            line.push_str(&format!("  [{media_count} media]"));
        }
        if get_bool("deleted") {
            line.push_str("  [deleted]");
        }
        println!("{line}");
    }
    let end = offset + rows.len() as u32;
    let mut footer = format!("{}-{end} of {total}", offset + 1);
    if u64::from(end) < total {
        footer.push_str(&format!("  (next page: --offset {end})"));
    }
    println!("{footer}");
    0
}

/// Mirrors the reference's `_print_item_detail`.
fn print_item_detail(item: &ItemRow) {
    let get_str = |key: &str| item.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let title = {
        let t = get_str("title");
        if !t.is_empty() {
            t
        } else {
            let u = get_str("url");
            if !u.is_empty() {
                u
            } else {
                get_str("external_id")
            }
        }
    };
    println!("{title}");
    println!("  source:    {} ({})", get_str("source"), get_str("type"));
    println!(
        "  kind:      {}   external id: {}",
        get_str("item_kind"),
        get_str("external_id")
    );
    let url = get_str("url");
    if !url.is_empty() {
        println!("  url:       {url}");
    }
    if let Some(tags) = item.get("tags").and_then(|v| v.as_array()) {
        if !tags.is_empty() {
            let tags: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
            println!("  tags:      {}", tags.join(", "));
        }
    }
    let created = get_str("created_at");
    let updated = get_str("updated_at");
    println!(
        "  created:   {}   updated: {}   revision: {}",
        if created.is_empty() { "-" } else { created },
        if updated.is_empty() { "-" } else { updated },
        item.get("revision").and_then(|v| v.as_i64()).unwrap_or(0),
    );
    if item
        .get("deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let deleted_at = get_str("deleted_at");
        println!(
            "  deleted:   yes ({})",
            if deleted_at.is_empty() {
                "unknown when"
            } else {
                deleted_at
            }
        );
    }
    let body = get_str("body");
    if !body.is_empty() {
        let shown = if body.chars().count() > 500 {
            format!(
                "{}\u{2026} [{} chars total; --json for all]",
                body.chars().take(500).collect::<String>(),
                body.chars().count()
            )
        } else {
            body.to_string()
        };
        println!("  body:      {}", shown.replace('\n', "\n             "));
    }
    if let Some(media) = item.get("media").and_then(|v| v.as_array()) {
        if !media.is_empty() {
            println!("  media ({}):", media.len());
            for m in media {
                let has_data = m.get("has_data").and_then(|v| v.as_bool()).unwrap_or(false);
                let local_path = m.get("local_path").and_then(|v| v.as_str());
                let state = if has_data {
                    "archived".to_string()
                } else {
                    local_path.unwrap_or("not archived").to_string()
                };
                let byte_size = m.get("byte_size").and_then(|v| v.as_i64()).unwrap_or(0);
                let size = if byte_size > 0 {
                    format!(", {}", human_bytes(byte_size))
                } else {
                    String::new()
                };
                let name = m
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| m.get("url").and_then(|v| v.as_str()))
                    .unwrap_or("?");
                let mime = m.get("mime").and_then(|v| v.as_str()).unwrap_or("?");
                println!(
                    "    [{}] {name} ({mime}{size}) \u{2014} {state}",
                    m.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                );
            }
        }
    }
    println!("  raw:");
    let raw = item.get("raw").cloned().unwrap_or(serde_json::Value::Null);
    let pretty = serde_json::to_string_pretty(&raw).unwrap_or_else(|_| "null".to_string());
    println!("    {}", pretty.replace('\n', "\n    "));
}

/// Mirrors the reference's `stats` command.
fn cmd_stats(config_path: &Path, json_out: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let metrics = match service.metrics() {
        Ok(m) => m,
        Err(e) => return report_config_error(&e),
    };

    if json_out {
        match serde_json::to_string_pretty(&metrics) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode stats as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    let rows = metrics
        .get("by_source_kind")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let live: i64 = rows
        .iter()
        .filter_map(|r| r.get("live").and_then(|v| v.as_i64()))
        .sum();
    let total: i64 = rows
        .iter()
        .filter_map(|r| r.get("total").and_then(|v| v.as_i64()))
        .sum();
    let revision_count = metrics
        .get("revision_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let media_count = metrics
        .get("media_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let media_bytes = metrics
        .get("media_bytes")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    println!(
        "Items:     {live} live, {} deleted ({total} total)",
        total - live
    );
    println!("Revisions: {revision_count}");
    println!(
        "Media:     {media_count} archived blob(s), {}",
        human_bytes(media_bytes)
    );
    if rows.is_empty() {
        println!("\nNo items stored yet \u{2014} run `dbs backup` first.");
        return 0;
    }
    println!();
    println!(
        "{:<24} {:<12} {:>8} {:>8} {:>8}",
        "source", "kind", "live", "deleted", "total"
    );
    for r in &rows {
        let get_str = |key: &str| r.get(key).and_then(|v| v.as_str()).unwrap_or("?");
        let get_i64 = |key: &str| r.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        println!(
            "{:<24} {:<12} {:>8} {:>8} {:>8}",
            get_str("source"),
            get_str("kind"),
            get_i64("live"),
            get_i64("deleted"),
            get_i64("total"),
        );
    }
    0
}

/// Reads `<config's dir>/.env` (same convention as `.env.example`,
/// written by `dbs init`) into a `KEY=VALUE` map — the `secret_store`
/// [`resolve_passphrase`] checks before falling back to the process
/// environment. A missing file is an empty map, not an error.
fn load_env_secret_store(config_path: &Path) -> std::collections::HashMap<String, String> {
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    dbs_core::parse_env_file(&dir.join(".env"))
}

/// Mirrors the reference's `export` command.
#[allow(clippy::too_many_arguments)]
fn cmd_export(
    config_path: &Path,
    out: PathBuf,
    format: String,
    source: Vec<String>,
    item_type: Vec<String>,
    since: Option<String>,
    until: Option<String>,
    since_updated: Option<String>,
    until_updated: Option<String>,
    include_deleted: bool,
    include_revisions: bool,
    no_raw: bool,
    wiki_grouping: String,
    encrypt: bool,
    passphrase_env: Option<String>,
) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let since_dt = match parse_date_arg(since.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let until_dt = match parse_date_arg(until.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let since_updated_dt = match parse_date_arg(since_updated.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let until_updated_dt = match parse_date_arg(until_updated.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let query = ExportQuery {
        sources: if source.is_empty() {
            None
        } else {
            Some(source)
        },
        item_types: if item_type.is_empty() {
            None
        } else {
            Some(item_type)
        },
        since: since_dt,
        until: until_dt,
        since_updated: since_updated_dt,
        until_updated: until_updated_dt,
        include_deleted,
        include_revisions,
        include_raw: !no_raw,
        wiki_grouping,
    };

    let passphrase = if encrypt {
        let env_name = passphrase_env
            .clone()
            .unwrap_or_else(|| dbs_core::DEFAULT_PASSPHRASE_ENV.to_string());
        let secret_store = load_env_secret_store(config_path);
        match dbs_core::resolve_passphrase(Some(&secret_store), &env_name) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("{e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
    } else {
        None
    };

    let result = match service.export(&query, &format, &out, passphrase.as_deref()) {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };

    let media = result
        .extra
        .get("media")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let pages = result
        .extra
        .get("pages")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut line = format!("Exported {} item(s)", result.item_count);
    if result.revision_count > 0 {
        line.push_str(&format!(", {} revision(s)", result.revision_count));
    }
    if media > 0 {
        line.push_str(&format!(", {media} media file(s)"));
    }
    if pages > 0 {
        line.push_str(&format!(" as {pages} wiki page(s)"));
    }
    line.push_str(&format!(
        " to {} ({})",
        result.path.as_deref().unwrap_or("?"),
        result.format
    ));
    println!("{line}");
    0
}

/// Mirrors the reference's `export-notes` command.
fn cmd_export_notes(
    config_path: &Path,
    out_dir: PathBuf,
    source: Vec<String>,
    item_type: Vec<String>,
    since: Option<String>,
    full: bool,
) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let since_dt = match parse_date_arg(since.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let sources = if source.is_empty() {
        None
    } else {
        Some(source)
    };
    let item_types = if item_type.is_empty() {
        None
    } else {
        Some(item_type)
    };

    let result = match dbs_core::export_notes(
        &service,
        &out_dir,
        sources.as_deref(),
        item_types.as_deref(),
        since_dt,
        !full,
    ) {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };
    let since_desc = result
        .extra
        .get("since")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("the beginning");
    println!(
        "Wrote {} note(s) to {} (since {since_desc})",
        result.item_count,
        result.path.as_deref().unwrap_or("?"),
    );
    0
}

/// Mirrors the reference's `export-profiles` command.
fn cmd_export_profiles(config_path: &Path, json_out: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let mut profiles: Vec<(String, dbs_core::ExportProfile)> =
        service.export_profiles().into_iter().collect();
    profiles.sort_by(|a, b| a.0.cmp(&b.0));

    if json_out {
        let mut out = serde_json::Map::new();
        for (name, p) in &profiles {
            let type_ = cfg
                .sources
                .get(name)
                .map(|sc| sc.type_.clone())
                .unwrap_or_default();
            let overridden = source_export_overrides(&cfg, name);
            out.insert(
                name.clone(),
                serde_json::json!({
                    "type": type_,
                    "resolved": p,
                    "overridden": overridden,
                }),
            );
        }
        match serde_json::to_string_pretty(&serde_json::Value::Object(out)) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode export profiles as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    if profiles.is_empty() {
        println!("No sources configured.");
        return 0;
    }
    for (name, p) in &profiles {
        let type_ = cfg
            .sources
            .get(name)
            .map(|sc| sc.type_.as_str())
            .unwrap_or("?");
        let over = source_export_overrides(&cfg, name);
        let mark = |field: &str| {
            if over.contains(&field.to_string()) {
                "*"
            } else {
                " "
            }
        };
        let state = if p.enabled { "enabled" } else { "EXCLUDED" };
        println!("\n{name}  ({type_}) \u{2014} {state}{}", mark("enabled"));
        let kinds = p
            .item_kinds
            .as_ref()
            .map(|k| k.join(", "))
            .unwrap_or_else(|| "all".to_string());
        println!("  {} item kinds : {kinds}", mark("item_kinds"));
        let group_by = if p.group_by.is_empty() {
            "tags (generic fallback)".to_string()
        } else {
            p.group_by.join(", ")
        };
        println!("  {} group by   : {group_by}", mark("group_by"));
        let body_from = if p.body_from.is_empty() {
            "the item's body column".to_string()
        } else {
            p.body_from.join(", ")
        };
        println!("  {} body from  : {body_from}", mark("body_from"));
        println!(
            "  {} page per   : {}",
            mark("page_per"),
            p.page_per.as_deref().unwrap_or("follows --grouping"),
        );
    }
    println!("\n* = set by a [sources.NAME.export] block; the rest are connector defaults.");
    println!("group_by/body_from read the raw payload, so --no-raw falls back to tags.");
    0
}

/// Which `[sources.NAME.export]` fields the config explicitly set, for
/// `cmd_export_profiles`'s `*` marker.
fn source_export_overrides(cfg: &dbs_core::Config, name: &str) -> Vec<String> {
    let Some(over) = cfg.sources.get(name).and_then(|sc| sc.export.as_ref()) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    if over.enabled.is_some() {
        fields.push("enabled".to_string());
    }
    if over.item_kinds.is_some() {
        fields.push("item_kinds".to_string());
    }
    if over.group_by.is_some() {
        fields.push("group_by".to_string());
    }
    if over.body_from.is_some() {
        fields.push("body_from".to_string());
    }
    if over.page_per.is_some() {
        fields.push("page_per".to_string());
    }
    fields
}

/// Mirrors the reference's `export-wiki` command.
fn cmd_export_wiki(
    config_path: &Path,
    out_dir: PathBuf,
    grouping: String,
    source: Vec<String>,
    item_type: Vec<String>,
    since: Option<String>,
) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let since_dt = match parse_date_arg(since.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let sources = if source.is_empty() {
        None
    } else {
        Some(source)
    };
    let item_types = if item_type.is_empty() {
        None
    } else {
        Some(item_type)
    };

    let result = match dbs_core::export_wiki_dir(
        &service,
        &out_dir,
        sources.as_deref(),
        item_types.as_deref(),
        since_dt,
        &grouping,
    ) {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };
    let pages = result
        .extra
        .get("pages")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let grouping_used = result
        .extra
        .get("grouping")
        .and_then(|v| v.as_str())
        .unwrap_or(&grouping);
    println!(
        "Wrote {pages} page(s) + index from {} item(s) to {} (grouping: {grouping_used})",
        result.item_count,
        result.path.as_deref().unwrap_or("?"),
    );
    0
}

/// Mirrors the reference's `decrypt` command.
fn cmd_decrypt(
    config_path: &Path,
    src: PathBuf,
    out: Option<PathBuf>,
    passphrase_env: Option<String>,
) -> i32 {
    if !src.is_file() {
        eprintln!("no such file: {}", src.display());
        return CONFIG_ERROR_EXIT_CODE;
    }
    if !dbs_core::is_encrypted(&src) {
        eprintln!("{} is not a dbs-encrypted file", src.display());
        return CONFIG_ERROR_EXIT_CODE;
    }
    let dest = out.unwrap_or_else(|| {
        if src.extension().is_some_and(|ext| ext == "enc") {
            src.with_extension("")
        } else {
            let mut name = src.file_name().unwrap_or_default().to_os_string();
            name.push(".plain");
            src.with_file_name(name)
        }
    });
    if dest.exists() {
        eprintln!("refusing to overwrite {}", dest.display());
        return STUB_EXIT_CODE;
    }

    let env_name = passphrase_env.unwrap_or_else(|| dbs_core::DEFAULT_PASSPHRASE_ENV.to_string());
    let secret_store = load_env_secret_store(config_path);
    let passphrase = match dbs_core::resolve_passphrase(Some(&secret_store), &env_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };

    match dbs_core::decrypt_file(&src, &dest, &passphrase) {
        Ok(n) => {
            println!("Wrote {} ({n} bytes)", dest.display());
            0
        }
        Err(e) => {
            std::fs::remove_file(&dest).ok();
            eprintln!("{e}");
            STUB_EXIT_CODE
        }
    }
}

/// Mirrors the reference's `sources list`.
fn cmd_sources_list(config_path: &Path, json_out: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let rows = match service.list_sources() {
        Ok(r) => r,
        Err(e) => return report_config_error(&e),
    };

    if json_out {
        match serde_json::to_string_pretty(&rows) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode sources as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    if rows.is_empty() {
        println!("No sources configured. Add one with: dbs sources add ...");
        return 0;
    }
    for r in &rows {
        let get_str = |key: &str| r.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let enabled = r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let backed_up = r
            .get("backed_up")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "{:<24} {:<10} {:<9} {}",
            get_str("name"),
            get_str("type"),
            if enabled { "enabled" } else { "disabled" },
            if backed_up { "(backed up)" } else { "" },
        );
    }
    0
}

/// Best-effort coerce a `--set` string into bool/int/list/str for the
/// config file. Mirrors the reference's `_coerce`.
fn coerce_set_value(value: &str) -> serde_json::Value {
    let low = value.to_ascii_lowercase();
    if low == "true" || low == "false" {
        return serde_json::Value::Bool(low == "true");
    }
    if let Ok(n) = value.parse::<i64>() {
        return serde_json::Value::from(n);
    }
    if let Some(inner) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let inner = inner.trim();
        return serde_json::Value::Array(if inner.is_empty() {
            Vec::new()
        } else {
            inner
                .split(',')
                .map(|s| serde_json::Value::String(s.trim().to_string()))
                .collect()
        });
    }
    serde_json::Value::String(value.to_string())
}

/// Mirrors the reference's `sources add`.
fn cmd_sources_add(config_path: &Path, name: String, type_: String, set: Vec<String>) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let mut options = std::collections::HashMap::new();
    for pair in &set {
        let Some((key, value)) = pair.split_once('=') else {
            eprintln!("--set expects key=value, got {pair:?}");
            return CONFIG_ERROR_EXIT_CODE;
        };
        options.insert(key.trim().to_string(), coerce_set_value(value.trim()));
    }

    match service.add_source(&name, &type_, &options, false, 0, false) {
        Ok(()) => {
            println!("Added source {name:?} ({type_}).");
            0
        }
        Err(e) => report_config_error(&e),
    }
}

/// Mirrors the reference's `sources check`.
fn cmd_sources_check(config_path: &Path) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let results = service.check_sources();
    let mut bad = 0;
    for (name, err) in &results {
        match err {
            Some(e) => {
                bad += 1;
                println!("  {name}: {e}");
            }
            None => println!("  {name}: ok"),
        }
    }
    if bad > 0 {
        CONFIG_ERROR_EXIT_CODE
    } else {
        0
    }
}

/// Mirrors the reference's `connectors list`.
fn cmd_connectors_list(config_path: &Path, json_out: bool, verbose: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let infos = service.list_connectors();

    if json_out {
        let rows: Vec<_> = infos
            .iter()
            .map(|i| {
                serde_json::json!({
                    "type": i.type_,
                    "plugin_id": i.plugin_id,
                    "builtin": i.is_builtin,
                    "display_name": i.display_name,
                    "secret_keys": i.secret_keys,
                })
            })
            .collect();
        match serde_json::to_string_pretty(&rows) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode connectors as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
        return 0;
    }

    for i in &infos {
        let tag = if i.is_builtin {
            "built-in".to_string()
        } else {
            i.dist_name.clone()
        };
        println!("{:<14} {:<22} [{tag}]", i.type_, i.display_name);
    }
    let report = service.registry.report();
    if verbose && !report.failures.is_empty() {
        println!("\nLoad failures:");
        for f in &report.failures {
            println!("  {}: {}", f.dist_name, f.reason);
        }
    }
    if verbose && !report.shadowed.is_empty() {
        println!("\nShadowed (collision):");
        for s in &report.shadowed {
            println!("  {}", s.plugin_id);
        }
    }
    0
}

/// Mirrors the reference's `connectors describe`.
fn cmd_connectors_describe(config_path: &Path, type_: String) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let Some(rc) = service.registry.get(&type_) else {
        eprintln!("connector plugin not found: {type_}");
        return CONFIG_ERROR_EXIT_CODE;
    };

    println!(
        "{} ({})",
        rc.handshake.display_name.as_deref().unwrap_or(&rc.type_),
        rc.plugin_id,
    );
    if let Some(desc) = rc
        .handshake
        .description
        .as_deref()
        .filter(|d| !d.is_empty())
    {
        println!("{desc}");
    }
    println!("\nItem kinds: {}", rc.handshake.item_kinds.join(", "));
    let secrets = if rc.handshake.secret_keys.is_empty() {
        "(none)".to_string()
    } else {
        rc.handshake.secret_keys.join(", ")
    };
    println!("Required secrets: {secrets}");
    let caps = &rc.handshake.capabilities;
    println!(
        "Capabilities: incremental={}, full_enumeration={}, native_deletes={}, media={}",
        caps.supports_incremental,
        caps.supports_full_enumeration,
        caps.supports_native_deletes,
        caps.produces_media,
    );
    // No config-schema field travels over the spawn/handshake protocol
    // (ADR-0001 step 1) — unlike the reference's in-process Pydantic
    // model, there's nothing to introspect here yet.
    println!("\nConfig schema: {{}}");
    0
}

/// Mirrors the reference's `doctor` command. See
/// `BackupService::doctor`'s doc-comment for the two check categories
/// (Pydantic option/dependency validation, yt-dlp version) this port's
/// architecture has no equivalent for.
fn cmd_doctor(config_path: &Path, json_out: bool) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let secret_store = load_env_secret_store(config_path);
    let checks = service.doctor(Some(&secret_store));

    if json_out {
        match serde_json::to_string_pretty(&checks) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("failed to encode doctor checks as JSON: {e}");
                return CONFIG_ERROR_EXIT_CODE;
            }
        }
    } else {
        for c in &checks {
            println!("  [{:^4}] {}: {}", c.status, c.name, c.detail);
        }
    }
    if checks.iter().any(|c| c.status == "fail") {
        1
    } else {
        0
    }
}

/// Finds a Python interpreter on `PATH` to run `pip` through — this
/// binary has no `sys.executable` of its own (it isn't running inside
/// a Python process), and yt-dlp is only ever invoked as a subprocess
/// by the yt-dlp-dependent connectors/`dbs capture` (gap-analysis.md's
/// Decisions section, item 3), never linked into this binary. Tries
/// `python3` before `python`, matching most systems' convention of
/// `python3` being the unambiguous name.
fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Mirrors the reference's `update-ytdlp` command: `pip install
/// --upgrade "yt-dlp[default]"` through whichever Python interpreter
/// is on `PATH`.
fn cmd_update_ytdlp(dry_run: bool) -> i32 {
    let Some(python) = find_python() else {
        eprintln!("no python3/python found on PATH \u{2014} needed to run pip");
        return CONFIG_ERROR_EXIT_CODE;
    };
    let pip_args = ["-m", "pip", "install", "--upgrade", "yt-dlp[default]"];
    println!("$ {python} {}", pip_args.join(" "));
    if dry_run {
        return 0;
    }

    match std::process::Command::new(python).args(pip_args).status() {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            if code == 0 {
                println!("yt-dlp upgraded. Restart any running `dbs serve` to pick it up.");
            }
            code
        }
        Err(e) => {
            eprintln!("failed to run {python}: {e}");
            CONFIG_ERROR_EXIT_CODE
        }
    }
}

/// Builds the `dbs schedule` snippet text for `config_path`/`interval`
/// on the given platform. Pure and platform-parameterized (rather than
/// `cfg!`-gated) so both branches are exercisable from a single test
/// run regardless of which OS actually built the binary; `cmd_schedule`
/// passes the real `cfg!(target_os = "windows")` at the only call site
/// that needs to know.
///
/// The Windows branch mirrors the reference's Linux cron+systemd
/// snippets in spirit (a ready-to-paste unattended-run recipe) but has
/// no reference to port from — this repo's cross-platform floor
/// (gap-analysis.md) covers Windows from round 1, and `schtasks` is
/// the standard CLI-scriptable equivalent of cron/systemd there.
fn render_schedule(config_path: &Path, interval: &str, windows: bool) -> String {
    let cfg_display = config_path.display();
    if windows {
        let schtasks_schedule = if interval == "hourly" {
            "/SC HOURLY".to_string()
        } else {
            "/SC DAILY /ST 03:00".to_string()
        };
        format!(
            "# Windows Task Scheduler (run from an elevated PowerShell or Command Prompt):\n\
             schtasks /Create /TN \"DailyBackupSystem\" /TR \"dbs --config {cfg_display} backup --all\" {schtasks_schedule} /F\n\
             # remove with: schtasks /Delete /TN \"DailyBackupSystem\" /F\n"
        )
    } else {
        let cron_time = if interval == "hourly" {
            "0 * * * *"
        } else {
            "0 3 * * *"
        };
        format!(
            "# crontab -e   (runs the backup and logs output)\n\
             {cron_time} dbs --config {cfg_display} backup --all >> ~/dbs.log 2>&1\n\
             \n\
             # systemd: ~/.config/systemd/user/dbs.service\n\
             [Unit]\nDescription=Daily Backup System\n\n[Service]\nType=oneshot\n\
             ExecStart=dbs --config {cfg_display} backup --all\n\
             \n\
             # systemd timer: ~/.config/systemd/user/dbs.timer\n\
             [Unit]\nDescription=Run dbs daily\n\n[Timer]\nOnCalendar=*-*-* 03:00:00\n\
             Persistent=true\n\n[Install]\nWantedBy=timers.target\n\
             # enable with: systemctl --user enable --now dbs.timer\n"
        )
    }
}

/// Mirrors the reference's `schedule` command.
fn cmd_schedule(config_path: &Path, interval: &str) -> i32 {
    let absolute = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    print!(
        "{}",
        render_schedule(&absolute, interval, cfg!(target_os = "windows"))
    );
    0
}

/// `true` for a host that only ever accepts connections from this
/// machine. Mirrors the reference's `is_local` check exactly,
/// including its treatment of an empty host string as local.
fn is_local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "")
}

/// Mirrors the reference's `serve` command's flag parsing and its
/// security-relevant validation (an unauthenticated API must not bind
/// off-localhost). Starting the actual server is out of scope for
/// this issue — see gap-analysis.md's Web tier rows (app skeleton,
/// job manager, auth) — so once flags validate, this reports that
/// plainly instead of pretending to listen for real.
fn cmd_serve(
    host: String,
    port: u16,
    allow_setup: bool,
    token: Option<String>,
    schedule: bool,
) -> i32 {
    if !is_local_host(&host) && token.is_none() {
        eprintln!(
            "Refusing to bind to {host} without --token: the API is otherwise unauthenticated \
             (it can read your backups and write secrets).\nBind to 127.0.0.1 (the default), or \
             pass --token <secret>."
        );
        return CONFIG_ERROR_EXIT_CODE;
    }

    eprintln!("Serving rusty_dbs UI at http://{host}:{port}  (press Ctrl+C to stop)");
    if schedule {
        eprintln!(
            "  (--schedule noted, but the scheduler isn't wired into the app skeleton yet \
             \u{2014} tracked in a follow-up issue)"
        );
    }
    if !allow_setup {
        eprintln!("  (--no-setup noted, but there are no setup actions to disable yet)");
    }
    if token.is_some() {
        eprintln!("  (token auth required on every /api request)");
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dbs serve: failed to start the async runtime: {e}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    match rt.block_on(dbs_web::serve(&host, port, token)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("dbs serve: {e}");
            CONFIG_ERROR_EXIT_CODE
        }
    }
}

/// Mirrors the reference's `capture` command's target resolution and
/// default-output-path selection. Opening a real browser and driving an
/// interactive login is out of scope for this issue — see
/// gap-analysis.md's Connectors cluster rows — so once the target and
/// capture kind resolve, this reports plainly instead of pretending to
/// capture anything.
fn cmd_capture(config_path: &Path, target: &str, out: Option<PathBuf>) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let (rc, spec) = match service.resolve_capture_target(target) {
        Ok(v) => v,
        Err(e) => return report_config_error(&e),
    };

    let default_out = match spec.kind.as_str() {
        "browser_session" => format!("./{target}-session.zip"),
        "browser_cookies" => format!("./{target}-cookies.txt"),
        "browser_storage_state" => format!("./{target}-storage_state.json"),
        other => {
            eprintln!("Unsupported capture kind: {other:?}");
            return CONFIG_ERROR_EXIT_CODE;
        }
    };
    let out_path = out.unwrap_or_else(|| PathBuf::from(default_out));

    eprintln!(
        "dbs capture: interactive browser capture isn't implemented in this port yet (see \
         gap-analysis.md's Connectors cluster rows) \u{2014} resolved {target:?} to connector \
         {:?} ({} capture); would write to {}",
        rc.type_,
        spec.kind,
        out_path.display()
    );
    CONFIG_ERROR_EXIT_CODE
}

/// Mirrors the reference's `_slugify`: lowercase, runs of non-`[a-z0-9]`
/// collapse to a single `-`, leading/trailing `-` trimmed; `"research"`
/// if that leaves nothing.
fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "research".to_string()
    } else {
        slug
    }
}

struct YoutubeResearchArgs {
    topic: String,
    query: Vec<String>,
    per_query_count: u32,
    count: u32,
    months: u32,
    question: Vec<String>,
    infographic: bool,
    infographic_orientation: String,
    out: Option<PathBuf>,
    notebook_name: Option<String>,
    auth_state: Option<PathBuf>,
}

/// Mirrors the reference's `research youtube` command's flag surface
/// and its default output path (`./<slug>.md`). The pipeline itself —
/// a live YouTube search feeding a NotebookLM notebook — isn't
/// implemented in this port yet: it depends on the research subsystem
/// (gap-analysis.md's Research subsystem row, not yet its own issue)
/// and the NotebookLM integration strategy (gap-analysis.md's
/// Decisions section item 4: shell out to `nlm`/`notebooklm-mcp` as a
/// subprocess or MCP client). So once flags parse, this reports what
/// it would do instead of pretending to run a real search.
fn cmd_research_youtube(args: YoutubeResearchArgs) -> i32 {
    let slug = slugify(&args.topic);
    let out_path = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("{slug}.md")));
    let queries = if args.query.is_empty() {
        vec![args.topic.clone()]
    } else {
        args.query
    };

    eprintln!(
        "dbs research youtube: the research pipeline isn't implemented in this port yet (see \
         gap-analysis.md's Research subsystem row) \u{2014} would search {:?} ({} results/query, \
         {} final, {}-month recency) and write a report to {}",
        queries,
        args.per_query_count,
        args.count,
        args.months,
        out_path.display()
    );
    if !args.question.is_empty() {
        eprintln!(
            "  ({} custom analysis question(s) given)",
            args.question.len()
        );
    }
    if args.infographic {
        eprintln!(
            "  (would also generate a {} infographic)",
            args.infographic_orientation
        );
    }
    if let Some(name) = &args.notebook_name {
        eprintln!("  (notebook name: {name})");
    }
    if let Some(path) = &args.auth_state {
        eprintln!("  (auth state: {})", path.display());
    }
    CONFIG_ERROR_EXIT_CODE
}

struct YoutubeBackupResearchArgs {
    topic: String,
    source: Vec<String>,
    list: Vec<String>,
    count: u32,
    question: Vec<String>,
    infographic: bool,
    infographic_orientation: String,
    out: Option<PathBuf>,
    notebook_name: Option<String>,
    auth_state: Option<PathBuf>,
}

/// Mirrors the reference's `research youtube-backup` command: unlike
/// `research youtube`, video *selection* is real — it queries already
/// backed-up items via [`BackupService::select_youtube_backup_videos`]
/// and reports the reference's own "no videos matched" error when
/// nothing does. Sending the selected videos through NotebookLM is the
/// same not-yet-implemented pipeline step as `research youtube` (see
/// that function's doc-comment) — reported once selection succeeds.
fn cmd_research_youtube_backup(config_path: &Path, args: YoutubeBackupResearchArgs) -> i32 {
    let cfg = match load_config(config_path) {
        Ok(cfg) => cfg,
        Err(e) => return report_config_error(&e),
    };
    let mut storage = match SqliteStorage::open(&cfg.database) {
        Ok(s) => s,
        Err(e) => return report_config_error(&e),
    };
    if let Err(e) = storage.migrate() {
        return report_config_error(&e);
    }
    let registry = ConnectorRegistry::from_resolved([]);
    let runner = SubprocessRunner::new(&cfg);
    let service = BackupService::new(&mut storage, &cfg, &registry, &runner);

    let sources = if args.source.is_empty() {
        None
    } else {
        Some(args.source.as_slice())
    };
    let lists = if args.list.is_empty() {
        None
    } else {
        Some(args.list.as_slice())
    };
    let videos =
        match service.select_youtube_backup_videos(sources, lists, Some(args.count as usize)) {
            Ok(v) => v,
            Err(e) => return report_config_error(&e),
        };
    if videos.is_empty() {
        let scope = if args.source.is_empty() {
            "any youtube source".to_string()
        } else {
            format!("source(s) {}", args.source.join(", "))
        };
        let list_note = if args.list.is_empty() {
            String::new()
        } else {
            format!(", list(s) {}", args.list.join(", "))
        };
        eprintln!(
            "No backed-up YouTube videos matched ({scope}{list_note}). Run `dbs backup` on a \
             youtube source first."
        );
        return CONFIG_ERROR_EXIT_CODE;
    }

    let slug = slugify(&args.topic);
    let out_path = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("{slug}.md")));

    eprintln!(
        "dbs research youtube-backup: the research pipeline isn't implemented in this port yet \
         (see gap-analysis.md's Research subsystem row) \u{2014} {} backed-up video(s) selected, \
         would write a report to {}",
        videos.len(),
        out_path.display()
    );
    if !args.question.is_empty() {
        eprintln!(
            "  ({} custom analysis question(s) given)",
            args.question.len()
        );
    }
    if args.infographic {
        eprintln!(
            "  (would also generate a {} infographic)",
            args.infographic_orientation
        );
    }
    if let Some(name) = &args.notebook_name {
        eprintln!("  (notebook name: {name})");
    }
    if let Some(path) = &args.auth_state {
        eprintln!("  (auth state: {})", path.display());
    }
    CONFIG_ERROR_EXIT_CODE
}

/// Mirrors the reference's `version` command: `<tool> <version> (core
/// API v<N>)`. `rusty_dbs` is this port's self-identifying tool name,
/// matching the manifest `tool` field every export writes
/// (`BackupService::export_manifest_row`); the version comes from this
/// crate's own `Cargo.toml` rather than the reference's `dbs.__version__`.
fn cmd_version() -> i32 {
    println!(
        "rusty_dbs {} (core API v{CURRENT_API_VERSION})",
        env!("CARGO_PKG_VERSION")
    );
    0
}

fn run_status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "success",
        RunStatus::Partial => "partial",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
        RunStatus::Interrupted => "interrupted",
    }
}

/// Mirrors the reference's `_print_run` (colors dropped — no color
/// dependency added; a future issue can add one if desired).
fn print_run(r: &RunResult) {
    let failed = if r.items_failed > 0 {
        format!(" !{}", r.items_failed)
    } else {
        String::new()
    };
    println!(
        "  {:<24} {:<11} [{}] +{} ~{} ={} x{} ^{}{failed} (fetched {}) {}",
        r.source,
        run_status_str(r.status),
        r.mode,
        r.created,
        r.updated,
        r.unchanged,
        r.deleted,
        r.undeleted,
        r.fetched,
        human_duration(r.duration_ms()),
    );
    if let Some(err) = &r.error {
        println!("      error: {err}");
    }
    for w in &r.warnings {
        println!("      warning: {w}");
    }
}

/// Compact wall-clock duration, e.g. `"0.8s"`, `"55.0s"`, `"2m45s"`.
/// Mirrors the reference's `_human_duration` (minus its `None` case:
/// `RunResult::duration_ms` is always known here, never absent).
fn human_duration(ms: i64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total_secs = ms.max(0) / 1000;
        let minutes = total_secs / 60;
        let rem_secs = total_secs % 60;
        format!("{minutes}m{rem_secs:02}s")
    }
}

/// Mirrors the reference's `_exit_code`: warnings deliberately don't
/// change the exit code (a success-with-caveats run exiting non-zero
/// would be a permanent false alarm for cron).
fn exit_code(results: &[RunResult]) -> i32 {
    if results.iter().any(|r| r.status == RunStatus::Failed) {
        3
    } else if results.iter().any(|r| r.status == RunStatus::Partial) {
        2
    } else {
        0
    }
}

#[cfg(test)]
mod progress_renderer_tests {
    //! `ProgressRenderer`'s drawn *text* can't be exercised through the
    //! compiled binary yet — every source the CLI can construct a
    //! registry for reports "connector not found" before `backup_source`
    //! ever reaches its `SourceStart` emission point (no
    //! connector-candidate discovery exists yet, #85-100), so there's no
    //! way to make a real `dbs backup` invocation draw a line to check
    //! against. These tests instead exercise the renderer's dirty-line
    //! state machine directly against synthetic events — the same
    //! `emit`/`draw`/`close` logic a real run would drive once a
    //! connector can succeed.

    use super::*;

    fn event(
        phase: ProgressPhase,
        source_index: Option<u32>,
        source_total: Option<u32>,
    ) -> ProgressEvent {
        ProgressEvent {
            phase,
            source: "a".to_string(),
            mode: "incremental".to_string(),
            fetched: 0,
            created: 0,
            updated: 0,
            unchanged: 0,
            deleted: 0,
            source_index,
            source_total,
            result: None,
            note: String::new(),
        }
    }

    #[test]
    fn a_disabled_renderer_never_becomes_dirty() {
        let renderer = ProgressRenderer::new(false);
        renderer.emit(&event(ProgressPhase::SourceStart, None, None));
        assert!(!renderer.dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn source_start_marks_dirty_and_source_done_clears_it() {
        let renderer = ProgressRenderer::new(true);
        renderer.emit(&event(ProgressPhase::SourceStart, Some(1), Some(2)));
        assert!(renderer.dirty.load(Ordering::Relaxed));
        renderer.emit(&event(ProgressPhase::SourceDone, None, None));
        assert!(!renderer.dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn close_is_idempotent_when_nothing_was_drawn() {
        let renderer = ProgressRenderer::new(true);
        renderer.close();
        renderer.close();
        assert!(!renderer.dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn item_and_checkpoint_and_sweep_are_ignored() {
        let renderer = ProgressRenderer::new(true);
        for phase in [
            ProgressPhase::Item,
            ProgressPhase::Checkpoint,
            ProgressPhase::Sweep,
        ] {
            renderer.emit(&event(phase, None, None));
        }
        assert!(!renderer.dirty.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod schedule_tests {
    //! `render_schedule` takes `windows` as a plain argument (not a
    //! `cfg!(target_os = ...)` check inside the function) specifically
    //! so both platform branches are testable in one run, regardless
    //! of which OS actually built and ran the test binary.

    use super::*;

    #[test]
    fn linux_daily_prints_cron_systemd_service_and_timer_with_the_config_path() {
        let out = render_schedule(Path::new("/home/me/dbs.toml"), "daily", false);
        assert!(out.contains("crontab -e"), "{out}");
        assert!(
            out.contains("0 3 * * * dbs --config /home/me/dbs.toml backup --all"),
            "{out}"
        );
        assert!(out.contains("~/.config/systemd/user/dbs.service"), "{out}");
        assert!(out.contains("Type=oneshot"), "{out}");
        assert!(
            out.contains("ExecStart=dbs --config /home/me/dbs.toml backup --all"),
            "{out}"
        );
        assert!(out.contains("~/.config/systemd/user/dbs.timer"), "{out}");
        assert!(out.contains("OnCalendar=*-*-* 03:00:00"), "{out}");
        assert!(out.contains("WantedBy=timers.target"), "{out}");
    }

    #[test]
    fn linux_hourly_changes_only_the_cron_time() {
        let out = render_schedule(Path::new("/home/me/dbs.toml"), "hourly", false);
        assert!(
            out.contains("0 * * * * dbs --config /home/me/dbs.toml backup --all"),
            "{out}"
        );
        // The systemd timer isn't parameterized by interval, matching
        // the reference exactly (its own inconsistency, not a bug to
        // "fix" in the port).
        assert!(out.contains("OnCalendar=*-*-* 03:00:00"), "{out}");
    }

    #[test]
    fn linux_snippet_has_no_windows_content() {
        let out = render_schedule(Path::new("/home/me/dbs.toml"), "daily", false);
        assert!(!out.contains("schtasks"), "{out}");
    }

    #[test]
    fn windows_daily_prints_a_schtasks_create_command_with_the_config_path() {
        let out = render_schedule(Path::new(r"C:\Users\me\dbs.toml"), "daily", true);
        assert!(out.contains("schtasks /Create"), "{out}");
        assert!(
            out.contains(r#"/TR "dbs --config C:\Users\me\dbs.toml backup --all""#),
            "{out}"
        );
        assert!(out.contains("/SC DAILY"), "{out}");
        assert!(out.contains("/ST 03:00"), "{out}");
        assert!(out.contains("schtasks /Delete"), "{out}");
    }

    #[test]
    fn windows_hourly_uses_the_hourly_schedule_flag() {
        let out = render_schedule(Path::new(r"C:\Users\me\dbs.toml"), "hourly", true);
        assert!(out.contains("/SC HOURLY"), "{out}");
        assert!(!out.contains("/SC DAILY"), "{out}");
    }

    #[test]
    fn windows_snippet_has_no_cron_or_systemd_content() {
        let out = render_schedule(Path::new(r"C:\Users\me\dbs.toml"), "daily", true);
        assert!(!out.contains("crontab"), "{out}");
        assert!(!out.contains("systemd"), "{out}");
    }
}

#[cfg(test)]
mod serve_tests {
    use super::*;

    #[test]
    fn is_local_host_accepts_loopback_names_and_the_empty_string() {
        for host in ["127.0.0.1", "localhost", "::1", ""] {
            assert!(is_local_host(host), "{host:?} should be local");
        }
    }

    #[test]
    fn is_local_host_rejects_everything_else() {
        for host in ["0.0.0.0", "192.168.1.5", "example.com", "10.0.0.1"] {
            assert!(!is_local_host(host), "{host:?} should not be local");
        }
    }
}
