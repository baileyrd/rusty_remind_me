//! Real-subprocess integration tests for the `dbs-connector-youtube`
//! binary (issue #164) — proves ADR-0001's protocol end to end for a
//! real, fully-implemented connector: spawns the actual compiled
//! binary, drives it through `dbs_core::registry`'s handshake
//! discovery (#45) and `dbs_core::run_stream`'s run/stream bridge
//! (#157) exactly the way `dbs-cli` would, and checks a real
//! `SqliteStorage` ends up with the items a fake `yt-dlp` served.
//!
//! Unlike `dbs-connector-raindrop` (its sibling in structure, see that
//! crate's own integration test), this connector has no HTTP layer at
//! all — `fetch()` shells out to a `yt-dlp` binary and parses its
//! JSON stdout (see `src/lib.rs`'s module doc for why). So instead of
//! a mock HTTP server, the redirect here
//! (`DBS_YOUTUBE_TEST_YT_DLP_BIN`, read by `src/main.rs`) points the
//! spawned binary at a fake `yt-dlp` executable — a tiny shell script
//! on disk that branches on its own argv (the same fake-executable
//! pattern the crate's own unit tests use via `branching_fake_yt_dlp`
//! in `src/lib.rs`, recreated here inline since that helper is private
//! to the lib crate's `#[cfg(test)]` module and not reachable from an
//! external integration test binary).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dbs_core::{
    run_connector_subprocess, ConnectorCandidate, ConnectorRegistry, Cursor, SqliteStorage,
    Storage, WireRunContext,
};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs-connector-youtube"))
}

fn candidate() -> ConnectorCandidate {
    ConnectorCandidate {
        dist_name: "rusty_dbs".to_string(),
        is_builtin: true,
        command: binary_path(),
        args: Vec::new(),
    }
}

#[test]
fn the_real_binarys_handshake_is_valid_and_matches_the_connector() {
    let mut registry = ConnectorRegistry::new();
    let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));

    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.loaded.len(), 1);
    let rc = &report.loaded[0];
    assert_eq!(rc.type_, "youtube");
    assert_eq!(
        rc.handshake.secret_keys,
        vec!["YOUTUBE_COOKIES_FILE".to_string()]
    );
    assert!(rc.handshake.item_kinds.contains(&"video".to_string()));
    assert!(rc.handshake.capabilities.supports_full_enumeration);
}

/// Writes a fake, executable `yt-dlp` shell script to `dir` that
/// branches on a URL substring in its own arguments — mirroring
/// `src/lib.rs`'s `write_fake_yt_dlp`/`branching_fake_yt_dlp` unit
/// test fixtures — so a single script can stand in for every list the
/// default `YouTubeConfig` fetches: Watch Later (`list=WL`), Liked
/// (`list=LL`), and playlist discovery (`feed/playlists`, answered
/// empty so the run stays a full — not partial — enumeration).
fn write_fake_yt_dlp(dir: &Path) -> PathBuf {
    let path = dir.join("fake-yt-dlp.sh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
args="$*"
case "$args" in
  *"list=WL"*)
    echo '{"title":"Watch Later","entries":[{"id":"v1","title":"Video One","url":"https://www.youtube.com/watch?v=v1","channel":"Chan"}]}'
    ;;
  *"list=LL"*)
    echo '{"title":"Liked","entries":[{"id":"v2","title":"Video Two","url":"https://www.youtube.com/watch?v=v2","channel":"Chan"}]}'
    ;;
  *"feed/playlists"*)
    echo '{"entries":[]}'
    ;;
  *)
    echo '{"entries":[]}'
    ;;
esac
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[test]
fn a_real_run_against_a_fake_yt_dlp_commits_items_through_the_full_subprocess_boundary() {
    let dir = std::env::temp_dir().join(format!(
        "dbs-connector-youtube-subprocess-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt_dlp = write_fake_yt_dlp(&dir);

    // A real (if content-empty) cookies file: `fetch()` checks the
    // configured path exists on disk before shelling out, same as a
    // real `cookies.txt` exported from a logged-in browser would.
    let cookies_file = dir.join("cookies.txt");
    std::fs::write(&cookies_file, "# Netscape HTTP Cookie File\n").unwrap();

    // SAFETY (test-only, single-threaded w.r.t. this env var): no
    // other test in this binary reads or writes
    // DBS_YOUTUBE_TEST_YT_DLP_BIN. `std::process::Command` (used by
    // `run_connector_subprocess`) inherits it, which is how the real
    // spawned binary is pointed at the fake `yt-dlp` script instead of
    // a real one on PATH.
    std::env::set_var("DBS_YOUTUBE_TEST_YT_DLP_BIN", &fake_yt_dlp);

    let mut registry = ConnectorRegistry::new();
    {
        let report = registry.discover(&[candidate()], &HashMap::new(), Duration::from_secs(5));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }
    let rc = registry.get("youtube").unwrap().clone();

    let mut storage = SqliteStorage::open(":memory:").unwrap();
    storage.migrate().unwrap();
    let source = storage
        .upsert_source("my-youtube", "youtube", &rc.plugin_id, "{}", 1)
        .unwrap();
    let run_id = storage
        .begin_run(source.id, &rc.plugin_id, "full", None)
        .unwrap();

    let wire_ctx = WireRunContext {
        source_id: source.id,
        source_name: "my-youtube".to_string(),
        cursor: Some(Cursor {
            value: serde_json::json!(null),
        }),
        since: None,
        secrets: HashMap::from([(
            "YOUTUBE_COOKIES_FILE".to_string(),
            cookies_file.to_string_lossy().to_string(),
        )]),
        run_id,
        mode: "full".to_string(),
        full_refresh: true,
        limit: None,
        store_media: false,
        max_media_bytes: 0,
        download_dir: None,
        config: HashMap::new(),
        http_timeout: 30.0,
        http_rate_limit_per_min: 0,
    };

    let outcome = run_connector_subprocess(&mut storage, &rc, wire_ctx, 0.5, None).unwrap();
    std::env::remove_var("DBS_YOUTUBE_TEST_YT_DLP_BIN");

    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.items_seen, 2);
    let live = storage.live_external_ids(source.id, None).unwrap();
    assert!(live.contains("watch-later:v1"));
    assert!(live.contains("liked:v2"));
}
