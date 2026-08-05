//! `rusty-remind-me-hub` — the hub server binary.
//!
//! A separate deployable from `rusty-remind-me`, matching the reference, where
//! the hub is its own container built from its own file. It shares the wire
//! protocol with the node's peer server and nothing else: no MCP, no database
//! of memories to serve locally, no scheduler.

use remind_me_hub::http::{read_body, read_head, write_response, HeadOutcome, Response};
use remind_me_hub::store::{sqlite::SqliteStore, HubStore};
use remind_me_hub::{dispatch, Config, HUB_VERSION};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long to wait for the database before giving up at startup.
///
/// A service manager's ordering only sequences unit *start*, not readiness, so
/// a hub started alongside its database routinely comes up first.
const DB_WAIT: Duration = Duration::from_secs(120);
const DB_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Default listen port, shared by the server and `--health-check` so the two
/// can never disagree about where the hub is.
const DEFAULT_PORT: u16 = 8765;
/// Short on purpose: a healthcheck that hangs is a healthcheck that reports
/// nothing, and the container runtime has its own (longer) timeout on top.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> std::process::ExitCode {
    // `--health-check` asks an already-running hub on this host whether it is
    // healthy, and exits 0/1. It exists for container healthchecks: the image
    // ships the binary and nothing else — no curl, no wget — so without this
    // a `HEALTHCHECK` would mean installing a whole HTTP client into the
    // runtime layer purely to make one loopback request.
    //
    // It works against `/health` specifically because that route is
    // unauthenticated by design, so the check needs no secret.
    if std::env::args().any(|a| a == "--health-check") {
        return match health_check() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("hub: health check failed: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hub: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Ask the local hub for `/health` and succeed only on a 200.
///
/// Deliberately a raw request rather than anything shared with the server:
/// this must work when the hub is degraded, and it is the one code path whose
/// whole job is to disagree with the process it is checking.
fn health_check() -> Result<(), String> {
    let port: u16 = std::env::var("REMIND_ME_HUB_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT);
    // Always loopback, never REMIND_ME_HUB_BIND: the bind address may be
    // 0.0.0.0, which is not a valid destination to connect to.
    let addr = format!("127.0.0.1:{port}");

    let mut stream =
        TcpStream::connect(&addr).map_err(|e| format!("could not connect to {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(HEALTH_CHECK_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(HEALTH_CHECK_TIMEOUT)))
        .map_err(|e| format!("could not set a timeout: {e}"))?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|e| format!("could not send the request: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("could not read the response: {e}"))?;
    let head = String::from_utf8_lossy(&response);
    let status = head.lines().next().unwrap_or_default();

    // 200 only. `/health` answers 503 when the database is unreachable, and
    // that is exactly the state a healthcheck must report as unhealthy.
    if status.contains(" 200 ") {
        Ok(())
    } else {
        Err(format!("hub reported {status:?}"))
    }
}

fn run() -> Result<(), String> {
    let config = Config::from_env();
    // Refusing to start beats starting insecure. An unconfigured hub would
    // reject every request anyway (see `authorized`), so coming up "healthy"
    // and 401-ing everything would be the worst of both: it looks deployed.
    if config.secret.is_empty() {
        return Err("SYNC_SECRET is not configured — refusing to start".to_string());
    }

    let store = open_store()?;

    // Wait for the database rather than crash-looping against one that is
    // still starting.
    let deadline = Instant::now() + DB_WAIT;
    loop {
        match store.migrate() {
            Ok(()) => break,
            Err(e) if Instant::now() < deadline => {
                eprintln!("hub: waiting for the database: {e}");
                std::thread::sleep(DB_RETRY_INTERVAL);
            }
            Err(e) => return Err(format!("database never became ready: {e}")),
        }
    }

    let bind = std::env::var("REMIND_ME_HUB_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("REMIND_ME_HUB_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let listener = TcpListener::bind((bind.as_str(), port))
        .map_err(|e| format!("could not bind {bind}:{port}: {e}"))?;

    eprintln!("hub: schema ready; v{HUB_VERSION} listening on {bind}:{port}");

    let store: Arc<dyn HubStore> = Arc::from(store);
    let config = Arc::new(config);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let store = Arc::clone(&store);
                let config = Arc::clone(&config);
                // A thread per connection, like this workspace's other
                // servers. A hub's concurrency is bounded by the number of
                // nodes syncing with it, which is small.
                if let Err(e) = std::thread::Builder::new()
                    .name("hub-conn".to_string())
                    .spawn(move || serve_connection(stream, store.as_ref(), &config))
                {
                    eprintln!("hub: could not spawn a connection thread: {e}");
                }
            }
            // One failed accept is not a reason to stop serving.
            Err(e) => eprintln!("hub: accept failed: {e}"),
        }
    }
    Ok(())
}

/// Build the configured store.
///
/// `DATABASE_URL` selects Postgres, matching the reference's only
/// configuration. `REMIND_ME_HUB_DB_PATH` selects SQLite. Setting neither is
/// an error rather than a silent default: a hub that quietly created an empty
/// SQLite file when its `DATABASE_URL` was misspelled would look healthy while
/// serving nothing.
fn open_store() -> Result<Box<dyn HubStore>, String> {
    let database_url = std::env::var("DATABASE_URL").unwrap_or_default();
    let sqlite_path = std::env::var("REMIND_ME_HUB_DB_PATH").unwrap_or_default();

    match (database_url.is_empty(), sqlite_path.is_empty()) {
        (false, true) => open_postgres(&database_url),
        (true, false) => {
            eprintln!("hub: using the SQLite backend at {sqlite_path}");
            Ok(Box::new(SqliteStore::open(&sqlite_path).map_err(|e| e.0)?))
        }
        (false, false) => Err("both DATABASE_URL and REMIND_ME_HUB_DB_PATH are set — \
             set exactly one so it is unambiguous which store is serving"
            .to_string()),
        (true, true) => Err("no store configured — set DATABASE_URL for Postgres or \
             REMIND_ME_HUB_DB_PATH for SQLite"
            .to_string()),
    }
}

#[cfg(feature = "postgres-store")]
fn open_postgres(url: &str) -> Result<Box<dyn HubStore>, String> {
    eprintln!("hub: using the Postgres backend");
    Ok(Box::new(
        remind_me_hub::store::postgres::PostgresStore::new(url),
    ))
}

/// Built without the Postgres backend, `DATABASE_URL` must fail loudly.
///
/// Silently falling back to SQLite would be the worst possible outcome: the
/// hub would come up healthy, serving an empty database, while the real one
/// sat untouched.
#[cfg(not(feature = "postgres-store"))]
fn open_postgres(_url: &str) -> Result<Box<dyn HubStore>, String> {
    Err("DATABASE_URL is set but this binary was built without the \
         `postgres-store` feature — rebuild with it, or set \
         REMIND_ME_HUB_DB_PATH to use SQLite"
        .to_string())
}

fn serve_connection(mut stream: TcpStream, store: &dyn HubStore, config: &Config) {
    // Timeouts on both directions: a client that opens a connection and stops
    // sending must not hold a thread forever.
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    if let Err(e) = handle(&mut stream, store, config) {
        // A dropped connection is routine, not an incident.
        if e.kind() != std::io::ErrorKind::UnexpectedEof {
            eprintln!("hub: connection error: {e}");
        }
    }
}

fn handle<S: Read + Write>(
    stream: &mut S,
    store: &dyn HubStore,
    config: &Config,
) -> std::io::Result<()> {
    let (head, buffered) = match read_head(stream)? {
        HeadOutcome::Parsed(head, buffered) => (head, buffered),
        HeadOutcome::Rejected(status, detail) => {
            return write_response(stream, &Response::error(status, detail));
        }
    };

    let body = match read_body(stream, &head, buffered)? {
        Ok(body) => body,
        Err((status, detail)) => {
            return write_response(stream, &Response::error(status, detail));
        }
    };

    let response = dispatch(store, config, &head, &body);
    write_response(stream, &response)
}
