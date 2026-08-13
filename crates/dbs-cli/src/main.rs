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

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use dbs_core::{load_config, write_scaffolding, DbsError, SqliteStorage, Storage};

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
    Backup,
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
        other => cmd_stub(command_name(&other)),
    };
    std::process::exit(code);
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Backup => "backup",
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
