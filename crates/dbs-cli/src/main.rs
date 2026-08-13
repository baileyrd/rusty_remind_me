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

use dbs_core::service::{
    BackupAllOptions, BackupService, BackupSourceOptions, ProgressSink, UnimplementedRunner,
};
use dbs_core::{
    load_config, parse_iso, write_scaffolding, BackupRunError, CancelToken, ConnectorRegistry,
    DbsError, ExportQuery, ItemRow, ProgressEvent, ProgressPhase, RunResult, RunStatus,
    SqliteStorage, Storage,
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
    Export,
    /// Incrementally export one Markdown note per item into a directory.
    #[command(name = "export-notes")]
    ExportNotes,
    /// Show each source's resolved export profile.
    #[command(name = "export-profiles")]
    ExportProfiles,
    /// Export the wiki format's pages loose into a directory.
    #[command(name = "export-wiki")]
    ExportWiki,
    /// Check database integrity and per-source state, or an archive's checksums.
    Verify,
    /// Replay an exported backup into the database.
    Restore,
    /// Decrypt a `dbs export --encrypt`-produced bundle.
    Decrypt,
    /// Run environment/dependency health checks.
    Doctor,
    /// Update the bundled yt-dlp build.
    #[command(name = "update-ytdlp")]
    UpdateYtdlp,
    /// Run scheduled maintenance (VACUUM, revision pruning, ...).
    Maintain,
    /// Print a cron/systemd (or Task Scheduler) snippet for unattended runs.
    Schedule,
    /// Run the optional local web UI.
    Serve,
    /// Headless browser-session capture for connectors that need one.
    Capture,
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
    List,
    /// Add a source to the config.
    Add,
    /// Check every configured source's connector loads and validates.
    Check,
}

#[derive(Subcommand)]
enum ConnectorsCommand {
    /// List every discovered connector.
    List,
    /// Describe one connector's config schema and capabilities.
    Describe,
}

#[derive(Subcommand)]
enum ResearchCommand {
    /// Ad-hoc YouTube research (not a backup source).
    Youtube,
    /// Back up ad-hoc YouTube research results.
    #[command(name = "youtube-backup")]
    YoutubeBackup,
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
        Command::Export => "export",
        Command::ExportNotes => "export-notes",
        Command::ExportProfiles => "export-profiles",
        Command::ExportWiki => "export-wiki",
        Command::Verify => "verify",
        Command::Restore => "restore",
        Command::Decrypt => "decrypt",
        Command::Doctor => "doctor",
        Command::UpdateYtdlp => "update-ytdlp",
        Command::Maintain => "maintain",
        Command::Schedule => "schedule",
        Command::Serve => "serve",
        Command::Capture => "capture",
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
/// (#65/#66) and the progress line + Ctrl+C handling (#67) are wired;
/// see [`ProgressSink`]'s doc-comment for the one honest gap that
/// remains (per-item progress needs the run/stream protocol).
///
/// No connector-candidate discovery mechanism exists yet (scanning
/// for installed connector subprocesses on disk — an implicit
/// prerequisite of the connectors cluster, #85-100), so the registry
/// this constructs is always empty: every configured source's
/// connector type is reported "not found" until that lands. That's
/// accurate to the current state of the port, not a bug in this
/// command — the "connector error surfaced to CLI output" acceptance
/// scenario is exactly this path.
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
    let runner = UnimplementedRunner;
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
    let runner = UnimplementedRunner;
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
    let runner = UnimplementedRunner;
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
    let runner = UnimplementedRunner;
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
    let runner = UnimplementedRunner;
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
