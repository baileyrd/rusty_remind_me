use remind_me_api::ApiServer;
use remind_me_core::db::queries;
use remind_me_core::{
    entity, reminders, stats, updater, wiki, wiki_import, Database, EntityInput, MemoryAddInput,
    MemoryListInput, MemorySearchInput, ResponseFormat, LIST_LIMIT_MAX, LIST_LIMIT_MIN,
};
use remind_me_mcp::McpServer;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;

fn get_target_config_paths() -> Vec<(PathBuf, &'static str)> {
    let mut targets = Vec::new();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    #[cfg(target_os = "windows")]
    {
        let appdata = env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
        targets.push((
            appdata.join("Claude").join("claude_desktop_config.json"),
            "Claude Desktop",
        ));
    }
    #[cfg(not(target_os = "windows"))]
    {
        targets.push((
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
            "Claude Desktop",
        ));
    }

    targets.push((
        home.join(".gemini")
            .join("antigravity")
            .join("mcp_config.json"),
        "Antigravity",
    ));
    targets.push((home.join(".cursor").join("mcp.json"), "Cursor"));
    targets.push((
        home.join(".mcp").join("config.json"),
        "Codex / Generic MCP Client",
    ));

    targets
}

fn configure_mcp_clients() -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let exe_str = current_exe.to_string_lossy().to_string();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let db_path = home.join(".remind_me").join("remind_me.db");

    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let server_entry = json!({
        "command": exe_str,
        "args": ["server"],
        "env": {
            "REMIND_ME_DB_PATH": db_path.to_string_lossy()
        }
    });

    for (config_path, name) in get_target_config_paths() {
        if let Some(parent) = config_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut data: Value = json!({ "mcpServers": {} });

        if config_path.exists() {
            if let Ok(content) = fs::read_to_string(&config_path) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                    if parsed.is_object() {
                        data = parsed;
                    }
                }
            }
        }

        if !data.get("mcpServers").is_some_and(|v| v.is_object()) {
            data["mcpServers"] = json!({});
        }

        data["mcpServers"]["rusty-remind-me"] = server_entry.clone();

        if let Ok(json_str) = serde_json::to_string_pretty(&data) {
            if fs::write(&config_path, json_str).is_ok() {
                println!("✔ Configured {}: {}", name, config_path.display());
            }
        }
    }

    println!("\nSetup complete! Restart your client application to activate rusty-remind-me.");
    Ok(())
}

/// Parsed form of `rusty-remind-me list [--limit N] [--category C] [--json]`.
///
/// The flag set is deliberately the reference's, not this crate's tool input's.
/// [`MemoryListInput`] also carries `tags`, `source`, `offset` and
/// `include_sensitive`, but `remind_me_mcp/cli.py`'s `list_p` exposes only
/// `--limit`, `--category` and `--json` — and a CLI that accepts flags the
/// reference rejects is the same drop-in divergence in the other direction.
#[derive(Debug)]
struct ListArgs {
    limit: usize,
    category: Option<String>,
    as_json: bool,
}

/// Parse `list`'s flags, or return the message to print to stderr.
///
/// Returns `Err` rather than exiting so this is testable without a process
/// boundary; `main` is what turns the message into an exit code.
fn parse_list_args(args: &[String]) -> Result<ListArgs, String> {
    let mut parsed = ListArgs {
        // Matches `list_p.add_argument("--limit", type=int, default=20)`.
        limit: 20,
        category: None,
        as_json: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {
                parsed.as_json = true;
                i += 1;
            }
            "--limit" | "--category" => {
                let flag = &args[i];
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("Error: {} expects a value.\n{}", flag, LIST_USAGE))?;
                if flag == "--limit" {
                    let n: usize = value.parse().map_err(|_| {
                        format!("Error: --limit expects a number, got {:?}.", value)
                    })?;
                    // The reference's MemoryListInput bounds `limit`, so an
                    // out-of-range value is a validation error there. Rejecting
                    // here rather than deferring to `list_memories`, which
                    // clamps silently — a caller who asked for 500 should be
                    // told they got 100, not quietly handed a short page.
                    if !(LIST_LIMIT_MIN..=LIST_LIMIT_MAX).contains(&n) {
                        return Err(format!(
                            "Error: --limit must be between {} and {}, got {}.",
                            LIST_LIMIT_MIN, LIST_LIMIT_MAX, n
                        ));
                    }
                    parsed.limit = n;
                } else {
                    parsed.category = Some(value.clone());
                }
                i += 2;
            }
            other => {
                return Err(format!("Error: unknown flag {:?}.\n{}", other, LIST_USAGE));
            }
        }
    }
    Ok(parsed)
}

const LIST_USAGE: &str = "Usage: rusty-remind-me list [--limit N] [--category CATEGORY] [--json]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let db_path = env::var("REMIND_ME_DB_PATH").unwrap_or_else(|_| "remind_me.db".to_string());

    let db = Database::open(&db_path)?;

    if args.len() < 2 || args[1] == "server" || args[1] == "mcp" {
        // Non-blocking, and deliberately not inside McpServer::new: that
        // constructor is also what the test suite uses to build a server
        // per-test, and a background `git fetch` on every one of those
        // would be slow, network-dependent, and racy across parallel tests.
        updater::start_background_check();
        // Unconditional, unlike the folder watcher: reminders have no enable
        // switch, only a poll interval. Without this the scheduler would be
        // code nothing ever runs, and a reminder would only ever fire if
        // someone happened to call a tool.
        let scheduler = remind_me_core::scheduler::start_scheduler_for(&db.conn());
        let server = McpServer::new(db);
        let result = server.run_stdio_loop();
        // Joined before the database goes out of scope, so an in-flight poll
        // cannot still be writing while the handle is torn down underneath it.
        if let Some(scheduler) = scheduler {
            scheduler.stop();
        }
        result?;
    } else {
        match args[1].as_str() {
            "configure" | "setup" => {
                configure_mcp_clients()?;
            }
            "api" => {
                let port_arg = args.get(2).cloned().unwrap_or_else(|| "8080".to_string());
                let port: u16 = port_arg.parse().map_err(|_| {
                    format!("invalid port {:?}; expected a number 0-65535", port_arg)
                })?;
                let host = "127.0.0.1";
                let addr = format!("{}:{}", host, port);

                // #90: refuse a double start for the same DB, matching the
                // reference's `--serve-ui` guard in `__main__.py`. An
                // in-memory database has nowhere to put a PID file -- that
                // is not an error, it just means this protection is
                // unavailable, same as an in-memory DB having no backup
                // directory (`status::server_status`).
                let pid_path = match remind_me_core::pid::pid_file_path(&db.conn()) {
                    Ok(path) => Some(path),
                    Err(remind_me_core::pid::PidError::InMemory) => {
                        eprintln!(
                            "Warning: in-memory database has no on-disk location for a PID \
                             file; double-start protection is unavailable."
                        );
                        None
                    }
                    Err(e) => return Err(e.into()),
                };
                if let Some(path) = &pid_path {
                    let status = remind_me_core::pid::dashboard_status(path);
                    if status.running {
                        eprintln!(
                            "Dashboard is already running at {} (PID {}). Stop it first or use \
                             a different port with `rusty-remind-me api <port>`.",
                            status.url.as_deref().unwrap_or("unknown"),
                            status
                                .pid
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "unknown".to_string()),
                        );
                        std::process::exit(1);
                    }
                    remind_me_core::pid::write_pid_file(path, host, port)?;
                }

                let scheduler = remind_me_core::scheduler::start_scheduler_for(&db.conn());
                let api_server = ApiServer::new(db);
                let result = api_server.run(&addr);
                if let Some(scheduler) = scheduler {
                    scheduler.stop();
                }
                if let Some(path) = &pid_path {
                    remind_me_core::pid::remove_pid_file(path);
                }
                result?;
            }
            "remote" => {
                // The only place this crate ever touches the async side of
                // the workspace: remind_me_remote::run_blocking owns
                // spinning up its own tokio runtime and blocks this thread
                // on it, so this crate stays synchronous like every other
                // subcommand here -- no `async fn`, no tokio dependency of
                // its own. Bind host/port and the connector token are
                // resolved from the environment inside run_blocking
                // (REMIND_ME_REMOTE_HOST/_PORT/_TOKEN), matching how "api"
                // above takes its port from argv while "server" takes its
                // config from the environment.
                let server = McpServer::new(db);
                remind_me_remote::run_blocking(server)?;
            }
            "search" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me search <query>");
                    std::process::exit(1);
                }
                let query = args[2..].join(" ");
                let search_input = MemorySearchInput {
                    strategy: Default::default(),
                    include_sensitive: false,
                    query,
                    category: None,
                    tags: None,
                    limit: 20,
                    token_budget: 800,
                    response_format: Default::default(),
                    include_dormant: false,
                    min_vitality: 0.0,
                    verbose: false,
                    expand_entities: false,
                    include_neighbors: false,
                    expand_co_retrieval: false,
                };
                let conn = db.conn();
                let results = queries::search_memories(&conn, &search_input)?;
                println!("{}", serde_json::to_string_pretty(&results)?);
            }
            "add" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me add <content>");
                    std::process::exit(1);
                }
                let content = args[2..].join(" ");
                let add_input = MemoryAddInput {
                    sensitive: false,
                    content,
                    category: "general".to_string(),
                    tags: vec![],
                    source: "cli".to_string(),
                    metadata: serde_json::json!({}),
                    subject: None,
                    predicate: None,
                    object: None,
                    entities: vec![],
                };
                let conn = db.conn();
                let mem = queries::add_memory(&conn, add_input)?;
                println!("Added memory: {}", mem.id);
            }
            "list" => {
                // Browse by filter, no ranking -- the counterpart to `search`,
                // and the reason both exist: `search` answers "what do I know
                // about X", `list` answers "show me the X slice".
                let list_args = match parse_list_args(&args[2..]) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        eprintln!("{}", message);
                        std::process::exit(1);
                    }
                };
                let list_input = MemoryListInput {
                    category: list_args.category,
                    limit: list_args.limit,
                    response_format: if list_args.as_json {
                        ResponseFormat::Json
                    } else {
                        ResponseFormat::Markdown
                    },
                    ..Default::default()
                };
                let conn = db.conn();
                let page = queries::list_memories(&conn, &list_input)?;
                match list_input.response_format {
                    // The reference's `_fmt_memories` JSON branch carries
                    // `count`/`memories`/`total`; `MemoryListResult` also
                    // carries the pagination cursor, so it is serialized whole
                    // rather than reshaped into a strictly smaller payload.
                    ResponseFormat::Json => {
                        println!("{}", serde_json::to_string_pretty(&page)?)
                    }
                    ResponseFormat::Markdown => {
                        println!("{}", reminders::render_memories_markdown(&page.memories))
                    }
                }
            }
            "get" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me get <id>");
                    std::process::exit(1);
                }
                let id = &args[2];
                let conn = db.conn();
                if let Some(mem) = queries::get_memory_by_id(&conn, id)? {
                    println!("{}", serde_json::to_string_pretty(&mem)?);
                } else {
                    eprintln!("Memory not found: {}", id);
                }
            }
            "entity" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me entity <name> [kind]");
                    std::process::exit(1);
                }
                let name = args[2].clone();
                let kind = args.get(3).cloned();
                let conn = db.conn();
                let ent = entity::upsert_entity(
                    &conn,
                    &EntityInput {
                        name,
                        kind,
                        aliases: vec![],
                    },
                )?;
                println!("{}", serde_json::to_string_pretty(&ent)?);
            }
            "wiki-write" => {
                if args.len() < 5 {
                    eprintln!("Usage: rusty-remind-me wiki-write <slug> <title> <content>");
                    std::process::exit(1);
                }
                let slug = &args[2];
                let title = &args[3];
                let content = &args[4];
                let conn = db.conn();
                let page = wiki::write_wiki_page(&conn, slug, title, content, "")?;
                println!("Saved wiki page: {}", page.slug);
            }
            "wiki-read" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me wiki-read <slug>");
                    std::process::exit(1);
                }
                let slug = &args[2];
                let conn = db.conn();
                if let Some(page) = wiki::get_wiki_page(&conn, slug)? {
                    println!("{}", serde_json::to_string_pretty(&page)?);
                } else {
                    eprintln!("Wiki page not found: {}", slug);
                }
            }
            "wiki-import" => {
                if args.len() < 3 {
                    eprintln!("Usage: rusty-remind-me wiki-import <dir>");
                    eprintln!("Imports every .md file in <dir> into the wiki. Pairs with");
                    eprintln!("`dbs export-wiki --out-dir <dir>` from daily-backup-system.");
                    std::process::exit(1);
                }
                let dir = PathBuf::from(&args[2]);
                let conn = db.conn();
                let report = wiki_import::import_wiki_dir(&conn, &dir, true)?;
                for page in &report.imported {
                    println!("{}  <- {}", page.slug, page.path);
                }
                for (path, reason) in &report.skipped {
                    eprintln!("skipped {}: {}", path, reason);
                }
                println!(
                    "Imported {} page(s), skipped {}.",
                    report.imported.len(),
                    report.skipped.len()
                );
            }
            "stats" => {
                let conn = db.conn();
                println!("{}", serde_json::to_string_pretty(&stats::collect(&conn)?)?);
            }
            cmd => {
                eprintln!("Unknown subcommand: {}. Available: configure, api, remote, server, search, add, list, get, entity, wiki-write, wiki-read, wiki-import, stats", cmd);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_flags_matches_the_reference_defaults() {
        let parsed = parse_list_args(&args(&[])).expect("bare `list` is valid");
        assert_eq!(parsed.limit, 20);
        assert_eq!(parsed.category, None);
        assert!(!parsed.as_json);
    }

    #[test]
    fn every_flag_parses_together() {
        let parsed =
            parse_list_args(&args(&["--limit", "5", "--category", "work", "--json"])).unwrap();
        assert_eq!(parsed.limit, 5);
        assert_eq!(parsed.category.as_deref(), Some("work"));
        assert!(parsed.as_json);
    }

    #[test]
    fn flag_order_does_not_matter() {
        let parsed =
            parse_list_args(&args(&["--json", "--category", "work", "--limit", "5"])).unwrap();
        assert_eq!(parsed.limit, 5);
        assert_eq!(parsed.category.as_deref(), Some("work"));
        assert!(parsed.as_json);
    }

    #[test]
    fn a_non_numeric_limit_is_rejected() {
        let err = parse_list_args(&args(&["--limit", "lots"])).unwrap_err();
        assert!(err.contains("--limit expects a number"), "got: {}", err);
    }

    /// The boundary that matters: `list_memories` clamps, so without this the
    /// out-of-range value would be silently honoured as something else.
    #[test]
    fn an_out_of_range_limit_is_rejected_rather_than_clamped() {
        for bad in [0, LIST_LIMIT_MAX + 1] {
            let err = parse_list_args(&args(&["--limit", &bad.to_string()])).unwrap_err();
            assert!(
                err.contains("must be between"),
                "limit {} should be refused, got: {}",
                bad,
                err
            );
        }
    }

    #[test]
    fn the_range_bounds_themselves_are_accepted() {
        for good in [LIST_LIMIT_MIN, LIST_LIMIT_MAX] {
            let parsed = parse_list_args(&args(&["--limit", &good.to_string()]))
                .unwrap_or_else(|e| panic!("limit {} should be accepted, got: {}", good, e));
            assert_eq!(parsed.limit, good);
        }
    }

    #[test]
    fn a_flag_missing_its_value_is_rejected() {
        for flag in ["--limit", "--category"] {
            let err = parse_list_args(&args(&[flag])).unwrap_err();
            assert!(err.contains("expects a value"), "got: {}", err);
        }
    }

    #[test]
    fn an_unknown_flag_is_rejected() {
        let err = parse_list_args(&args(&["--tags", "work"])).unwrap_err();
        assert!(err.contains("unknown flag"), "got: {}", err);
    }
}
