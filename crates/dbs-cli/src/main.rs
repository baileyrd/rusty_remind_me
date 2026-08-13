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
    load_config, write_scaffolding, BackupRunError, CancelToken, ConnectorRegistry, DbsError,
    ProgressEvent, ProgressPhase, RunResult, RunStatus, SqliteStorage, Storage,
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
    Status,
    /// Show recent run history.
    History,
    /// Browse backed-up items.
    Items,
    /// Aggregate item/source statistics.
    Stats,
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
        other => cmd_stub(command_name(&other)),
    };
    std::process::exit(code);
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Backup { .. } => "backup",
        Command::Status => "status",
        Command::History => "history",
        Command::Items => "items",
        Command::Stats => "stats",
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
