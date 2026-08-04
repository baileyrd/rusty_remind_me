//! Multi-node sync: push the local outbox to a hub, pull its changes back.
//!
//! See `docs/adr/0004-sync-protocol-and-conflict-resolution.md`,
//! `docs/adr/0005-graph-sync.md`, and `docs/adr/0006-peer-discovery.md` for
//! what this module does and does not implement yet. In short: `memories`
//! and the knowledge-graph tables (`entities`/`entity_relations`/
//! `memory_entities`) sync against a configured hub and every discovered
//! peer (a static list plus Tailscale's local API); no OAuth or
//! `remind_me_revoke_clients` yet — deferred to its own follow-up issue,
//! exactly as the epic asked for this to be split.
//!
//! Off unless [`NODE_ID_ENV`], [`HUB_URL_ENV`], and [`SYNC_SECRET_ENV`] are
//! all set — the same default-off posture as the webhook endpoint (`#56`)
//! and the folder watcher (`#55`).

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, Result};

mod graph;
// Public so `notifications` can reuse the one HTTP client this crate has
// rather than growing a second.
pub mod http;
mod peers;
mod pull;
mod push;
mod reconcile;
mod record;
mod server;
mod status;
mod worker;

pub use graph::{
    apply_incoming_record, upsert_entity_record, upsert_entity_relation_record, upsert_link_record,
    EntityRelationSyncRecord, EntitySyncRecord, GraphApplyError, LinkSyncRecord,
};
pub use peers::{discover_peers, probe_peer, Peer, STATIC_PEERS_ENV, TAILSCALE_SOCKET_ENV};
pub use pull::{
    pull_entities, pull_entity_relations, pull_links, pull_remote, PullError, PullReport,
};
pub use push::{push_outbox, PushError, PushReport};
pub use reconcile::{classify, reconcile, reconcile_hub, reconcile_peer, PULL_LAG_GRACE_SECONDS};
pub use record::{upsert_record, ApplyOutcome, SyncApplyError, SyncRecord};
pub use server::{serve_once, PeerServer, PeerServerConfig, PeerServerStatus, SyncPeer};
pub use status::{sync_repair, sync_status};
pub use worker::{
    disabled_status as sync_worker_disabled_status, superseded as sync_error_superseded,
    SyncWorker, SyncWorkerStatus, HUB_REMOTE_ID,
};

/// This node's identity in sync records. Empty (the default) means "no
/// identity configured" — matching the reference's own `NODE_ID = ""`
/// default, stamped onto every locally-created memory regardless of
/// whether sync is actually enabled, so a node that turns sync on later
/// does not have to guess which of its existing memories were its own.
pub const NODE_ID_ENV: &str = "REMIND_ME_NODE_ID";
/// A human-readable label for this device/install, stamped alongside
/// `node_id`. Defaults to `"unknown"`, matching the reference.
pub const CLIENT_ENV: &str = "REMIND_ME_CLIENT";
/// The hub this node pushes to and pulls from. Sync is off without one.
pub const HUB_URL_ENV: &str = "REMIND_ME_HUB_URL";
/// Bearer token required on every `/sync/push` and `/sync/pull` request,
/// both sent (to the hub) and required (of callers of this node's own peer
/// server). Sync is off without one.
pub const SYNC_SECRET_ENV: &str = "REMIND_ME_SYNC_SECRET";
/// Seconds between background sync cycles.
pub const SYNC_INTERVAL_ENV: &str = "REMIND_ME_SYNC_INTERVAL";
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 60;
/// Bind address for this node's own peer server (accepting another node's
/// push/pull). Defaults to all interfaces — unlike the webhook endpoint,
/// a sync peer server has to be reachable by other machines to be useful
/// at all, so there is no safe localhost-only default to fall back on.
pub const PEER_BIND_ENV: &str = "REMIND_ME_PEER_BIND";
pub const DEFAULT_PEER_BIND: &str = "0.0.0.0";
pub const PEER_PORT_ENV: &str = "REMIND_ME_PEER_PORT";
pub const DEFAULT_PEER_PORT: u16 = 8766;

pub const DEFAULT_CLIENT: &str = "unknown";

/// This node's configured identity, or `""` if unset — stamped on every
/// newly created memory regardless of whether sync is enabled, matching
/// the reference's own `memory_add` exactly (`NODE_ID`/`CLIENT` are plain
/// module-level config, read unconditionally).
pub fn configured_node_id() -> String {
    std::env::var(NODE_ID_ENV).unwrap_or_default()
}

pub fn configured_client() -> String {
    std::env::var(CLIENT_ENV).unwrap_or_else(|_| DEFAULT_CLIENT.to_string())
}

fn configured_hub_url() -> String {
    std::env::var(HUB_URL_ENV).unwrap_or_default()
}

fn configured_sync_secret() -> String {
    std::env::var(SYNC_SECRET_ENV).unwrap_or_default()
}

/// `NODE_ID`, `HUB_URL`, and `SYNC_SECRET` are all non-empty — matching the
/// reference's `SYNC_ENABLED = bool(NODE_ID and HUB_URL and SYNC_SECRET)`
/// exactly. Gates: whether `delete_memory` tombstones instead of hard
/// deleting, whether the background sync worker does anything, and (via
/// [`SyncPeer::from_env`]'s own additional secret-only check) whether this
/// node's peer server binds a port at all.
pub fn sync_enabled() -> bool {
    !configured_node_id().is_empty()
        && !configured_hub_url().is_empty()
        && !configured_sync_secret().is_empty()
}

/// How long a probed hub version stays fresh.
///
/// Several dashboard tabs refreshing must not turn a page load into hub
/// traffic; a build number does not change between requests anyway.
const HUB_VERSION_TTL: std::time::Duration = std::time::Duration::from_secs(60);

static HUB_VERSION_CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
    std::sync::Mutex::new(None);

/// The hub's build, probed from its `/health`.
///
/// `None` when sync is not configured, when the hub is unreachable, or when it
/// reports no version — all three are the same thing to a caller: no line to
/// render. Deliberately best-effort: a version display must never be able to
/// fail the page it decorates.
///
/// **Probed live rather than read from sync state.** The reference makes this
/// argument and it applies here too: a dashboard started standalone never runs
/// a sync cycle, so anything populated by that cycle would report `null`
/// forever while looking like a working feature. Asking the hub is correct
/// regardless of which process is serving the page.
///
/// Wrapped here rather than exposing `sync::http`: the API crate has no
/// business making raw HTTP requests, and this keeps the auth header and the
/// cache in one place.
pub fn probe_hub_version() -> Option<String> {
    if !sync_enabled() {
        return None;
    }

    let mut cache = HUB_VERSION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed() < HUB_VERSION_TTL {
            return value.clone();
        }
    }

    let url = format!("{}/health", configured_hub_url().trim_end_matches('/'));
    let version = match http::get(&url, &configured_sync_secret()) {
        Ok((200, body)) => serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
        _ => None,
    };

    *cache = Some((std::time::Instant::now(), version.clone()));
    version
}

const NOW_ISO_EXPR: &str = "strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00'";

/// Align `sync_flags.sync_enabled` with [`sync_enabled`], every time the
/// schema is opened — matching the reference's own `_reconcile_sync_enabled_flag`,
/// called on every startup, verbatim:
///
/// - already matches: no-op.
/// - stored `"0"`, now enabled: the outbox is backfilled with an `insert` row
///   for every current `memories`/`entities`/`memory_entities` row, so
///   changes made while sync was off still reach a remote once it's
///   configured. Matches the reference exactly in NOT backfilling
///   `entity_relations` either — preserved rather than "fixed," since
///   covering more tables than the reference does would make the two diverge
///   on what a first sync actually sends.
/// - now disabled (from any prior state): `sync_outbox`/`sync_sends` are
///   cleared — nothing is left to drain.
/// - unset (a fresh database) and now enabled: no backfill, matching the
///   reference's own reasoning verbatim even though the reference's stated
///   justification ("pre-gate triggers were unconditional, so the outbox is
///   already complete") describes reference history this crate never had.
///   Reproducing the exact stored/desired matrix rather than the reasoning
///   behind one cell of it keeps this one reconciliation function, not two
///   diverging ones for "true fresh" vs. "upgraded from an older,
///   once-ungated build."
pub fn reconcile_sync_enabled_flag(conn: &Connection) -> Result<()> {
    use rusqlite::OptionalExtension;

    let desired = if sync_enabled() { "1" } else { "0" };
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_flags WHERE key = 'sync_enabled'",
            [],
            |r| r.get(0),
        )
        .optional()?;

    if stored.as_deref() == Some(desired) {
        return Ok(());
    }

    if desired == "1" && stored.as_deref() == Some("0") {
        conn.execute_batch(&format!(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT id, 'insert', json_object(
                 'id', id, 'content', content, 'category', category, 'tags', tags,
                 'source', source, 'metadata', metadata, 'created_at', created_at,
                 'updated_at', updated_at, 'capture_id', capture_id, 'node_id', node_id,
                 'client', client, 'accessed_at', accessed_at, 'access_count', access_count,
                 'decay_rate', decay_rate, 'vitality', vitality, 'base_weight', base_weight,
                 'status', status, 'memory_type', memory_type,
                 'source_capture_id', source_capture_id, 'subject', subject,
                 'predicate', predicate, 'object', object, 'superseded_by', superseded_by,
                 'doc_id', doc_id, 'chunk_index', chunk_index, 'deleted_at', deleted_at
             ), {now}
             FROM memories;

             INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT id, 'insert', json_object(
                 'record_type', 'entity', 'id', id, 'name', name, 'kind', kind,
                 'aliases', aliases, 'created_at', created_at, 'updated_at', updated_at,
                 'node_id', node_id
             ), {now}
             FROM entities;

             INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT memory_id, 'insert', json_object(
                 'record_type', 'memory_entity',
                 'id', memory_id || '|' || entity_id,
                 'memory_id', memory_id, 'entity_id', entity_id, 'created_at', created_at
             ), {now}
             FROM memory_entities;",
            now = NOW_ISO_EXPR
        ))?;
    } else if desired == "0" {
        conn.execute_batch("DELETE FROM sync_outbox; DELETE FROM sync_sends;")?;
    }

    conn.execute(
        "INSERT INTO sync_flags (key, value) VALUES ('sync_enabled', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![desired],
    )?;

    Ok(())
}

/// Days an unsent outbox row is kept before being pruned.
///
/// Mirrors the reference's `OUTBOX_RETENTION_DAYS`, including the environment
/// variable that overrides it, so a database shared between the two systems is
/// governed by one policy rather than two.
pub const DEFAULT_OUTBOX_RETENTION_DAYS: i64 = 30;
const OUTBOX_RETENTION_ENV: &str = "REMIND_ME_OUTBOX_RETENTION_DAYS";

fn outbox_retention_days() -> i64 {
    std::env::var(OUTBOX_RETENTION_ENV)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d >= 0)
        .unwrap_or(DEFAULT_OUTBOX_RETENTION_DAYS)
}

/// Prune already-sent outbox rows and anything past the retention window.
///
/// Returns the number of rows removed.
///
/// # Why this runs at all
///
/// `memories_outbox_ai` and `memories_outbox_au` fire on every insert and every
/// update of `memories`, writing a full JSON snapshot of the row. Since
/// retrieval records access — which is an `UPDATE` — the outbox grows on reads
/// as well as writes. Nothing in this crate drains it, so without a prune it
/// grows without bound, carrying a copy of every memory's content each time.
///
/// # Why this policy and not another
///
/// This is the reference's own rule, verbatim: rows already marked sent are
/// echo-suppressed and never pushed, so they go immediately; the rest are kept
/// for the retention window so an intermittently-reachable remote can still
/// catch up, then dropped along with their per-remote send markers.
///
/// Copying the policy rather than inventing one matters because a database can
/// be shared with `remind_me` — it opens the same file and prunes on the same
/// rule, so anything this deletes is something the reference would have deleted
/// too. A tighter rule here would silently drop changes the reference still
/// intended to push.
///
/// # Where it runs
///
/// The reference prunes on each sync cycle. This runs both on database open
/// (bounding a long-lived database even with sync disabled) and, when sync
/// is enabled, once per [`SyncWorker`] cycle — matching the reference's own
/// arrangement now that one exists.
pub fn prune_outbox(conn: &Connection) -> Result<usize> {
    let cutoff = (Utc::now() - Duration::days(outbox_retention_days())).to_rfc3339();
    let removed = conn.execute(
        "DELETE FROM sync_outbox WHERE sent_at != '' OR created_at < ?",
        params![cutoff],
    )?;
    conn.execute(
        "DELETE FROM sync_sends WHERE outbox_id NOT IN (SELECT id FROM sync_outbox)",
        [],
    )?;
    Ok(removed)
}
