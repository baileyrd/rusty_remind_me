//! `dbs serve --schedule`'s background scheduler loop (issue #190) —
//! mirrors `src/dbs/web/app.py`'s `create_app(schedule_seconds=...)`
//! in baileyrd/Daily-Backup-System (pinned `@6cc6491`): wakes on a
//! fixed interval, checks which enabled sources are due
//! ([`dbs_core::service::BackupService::due_sources`]), and if any
//! are, starts the same `{all: true, only_due: true}` job the web
//! UI's "Backup all" button would — on the *same* [`crate::jobs::JobManager`]
//! `/api/backup` uses, so a scheduled run shows up in the UI's live
//! progress and history like any other. `JobAlreadyRunning` is
//! swallowed (a run already in flight just gets picked up again next
//! tick); any other tick failure is logged to stderr and the loop
//! keeps going — it must survive anything, the same way the
//! reference's own tick handler catches and logs rather than letting
//! one bad tick kill the loop.
//!
//! This port's `dbs serve --schedule` is a bare on/off flag (the
//! reference's is a float interval in seconds) — [`TICK_INTERVAL`] is
//! this port's fixed substitute rather than a new CLI knob.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use dbs_core::service::{BackupAllOptions, BackupService};
use dbs_core::{build_registry, CancelToken, Config, DbsError, SubprocessRunner};

use crate::api::{open_storage, CancelBridge, JobProgressSink};
use crate::jobs::{JobAlreadyRunning, JobManager};

/// How often the loop checks for due sources — a plain "check every
/// minute" cadence, fine granularity for "daily"/"hourly" schedules
/// without being wasteful. [`spawn`]'s own `interval` parameter is
/// what tests override to something short instead of waiting a real
/// minute; production callers always pass this constant.
pub(crate) const TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Spawns the loop as a detached Tokio task. `dbs serve`'s process
/// exits (dropping the Tokio runtime) on Ctrl+C, which aborts it —
/// no separate shutdown signal is needed for a single-process local
/// tool like this, unlike the reference's ASGI lifespan hook (which
/// has to explicitly join a non-daemon-adjacent OS thread).
pub(crate) fn spawn(config: Arc<Config>, job_manager: Arc<JobManager>, interval: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            tick(&config, &job_manager).await;
        }
    });
}

async fn tick(config: &Arc<Config>, job_manager: &Arc<JobManager>) {
    let due_config = config.clone();
    let due = tokio::task::spawn_blocking(move || -> Result<Vec<String>, DbsError> {
        let mut storage = open_storage(&due_config)?;
        let (registry, _report) = build_registry(&due_config);
        let runner = SubprocessRunner::new(&due_config);
        let service = BackupService::new(&mut storage, &due_config, &registry, &runner);
        service.due_sources(chrono::Utc::now())
    })
    .await;

    let due = match due {
        Ok(Ok(due)) => due,
        Ok(Err(e)) => {
            eprintln!("dbs serve: scheduler tick failed: {e}");
            return;
        }
        Err(e) => {
            eprintln!("dbs serve: scheduler tick panicked: {e}");
            return;
        }
    };
    if due.is_empty() {
        return;
    }
    eprintln!(
        "dbs serve: scheduler: {} due \u{2014} starting backup",
        due.join(", ")
    );

    let job_config = config.clone();
    let result = job_manager.start(
        json!({"all": true, "only_due": true, "scheduled": true}),
        move |job| {
            let sink = JobProgressSink { job: job.clone() };
            let core_cancel = CancelToken::new();
            let bridge = CancelBridge::spawn(job.clone(), core_cancel.clone());

            let mut storage = open_storage(&job_config).map_err(|e| e.to_string())?;
            let (registry, _report) = build_registry(&job_config);
            let runner = SubprocessRunner::new(&job_config);
            let mut service = BackupService::new(&mut storage, &job_config, &registry, &runner);
            let opts = BackupAllOptions {
                only_due: true,
                on_progress: Some(&sink),
                cancel: Some(core_cancel),
                ..Default::default()
            };
            let outcome = service.backup_all(&opts).map(|results| {
                for result in &results {
                    job.record_result(
                        serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
                    );
                }
            });
            drop(bridge);
            outcome.map_err(|e| e.to_string())
        },
    );
    if let Err(JobAlreadyRunning) = result {
        // A run is already in flight (manual or a previous tick's) —
        // the next tick re-checks; nothing to do now.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A minimal, valid `Config` for scheduler tests — mirrors `lib.rs`'s
    /// own `test_config()` (this module can't reach that one directly;
    /// it lives in a different `#[cfg(test)] mod tests`).
    fn test_config(database: &str) -> Config {
        Config {
            database: database.to_string(),
            export_dir: "exports".to_string(),
            download_root: "downloads".to_string(),
            default_overlap_seconds: 0,
            vpn_exec: String::new(),
            vpn_status: String::new(),
            vpn_netns: String::new(),
            vpn_guard: dbs_core::VpnGuard::default(),
            notify_url: None,
            notify_on: Default::default(),
            http_timeout: 30.0,
            http_rate_limit_per_min: 0,
            batch_max: 500,
            sweep_safety_fraction: 0.5,
            parallel: 1,
            sources: HashMap::new(),
            connectors: HashMap::new(),
            connectors_dir: None,
            base_dir: std::path::PathBuf::from("."),
            source_path: None,
        }
    }

    fn temp_db(label: &str) -> std::path::PathBuf {
        use dbs_core::Storage;
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-scheduler-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("dbs.sqlite3");
        let mut storage = dbs_core::SqliteStorage::open(db_path.to_str().unwrap()).unwrap();
        storage.migrate().unwrap();
        db_path
    }

    fn enabled_source(name: &str) -> dbs_core::SourceConfig {
        dbs_core::SourceConfig {
            name: name.to_string(),
            type_: "fixture".to_string(),
            enabled: true,
            schedule: None,
            reconcile_every_runs: None,
            store_media: false,
            max_media_mb: 0,
            requires_vpn: false,
            keep_revisions: 0,
            export: None,
            options: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn tick_does_nothing_when_no_source_is_due() {
        let db_path = temp_db("idle");
        let config = Arc::new(test_config(db_path.to_str().unwrap()));
        let job_manager = Arc::new(JobManager::new());

        tick(&config, &job_manager).await;

        assert!(job_manager.current().is_none());
    }

    #[tokio::test]
    async fn tick_starts_a_job_when_a_source_is_due() {
        let db_path = temp_db("due");
        let mut config = test_config(db_path.to_str().unwrap());
        // A never-run enabled source is always due (`source_is_due`'s own
        // contract) — no explicit schedule needed.
        config.sources.insert("a".to_string(), enabled_source("a"));
        let config = Arc::new(config);
        let job_manager = Arc::new(JobManager::new());

        tick(&config, &job_manager).await;

        let job = job_manager.current().expect("a job should have started");
        assert_eq!(job.spec()["only_due"], true);
        for _ in 0..200 {
            if job.status() != crate::jobs::JobStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snap = job.snapshot();
        assert_eq!(snap.status, crate::jobs::JobStatus::Done);
        assert_eq!(snap.results.len(), 1);
    }

    #[tokio::test]
    async fn tick_is_a_no_op_when_a_job_is_already_running() {
        let db_path = temp_db("already-running");
        let mut config = test_config(db_path.to_str().unwrap());
        config.sources.insert("a".to_string(), enabled_source("a"));
        let config = Arc::new(config);
        let job_manager = Arc::new(JobManager::new());
        let existing = job_manager
            .start(json!({"kind": "manual"}), |_job| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .unwrap();

        tick(&config, &job_manager).await;

        // The manual job is still `current` — the tick didn't replace it.
        let current = job_manager.current().unwrap();
        assert_eq!(current.id(), existing.id());
    }

    #[tokio::test]
    async fn spawn_starts_a_job_once_a_source_becomes_due() {
        let db_path = temp_db("spawn");
        let mut config = test_config(db_path.to_str().unwrap());
        config.sources.insert("a".to_string(), enabled_source("a"));
        let config = Arc::new(config);
        let job_manager = Arc::new(JobManager::new());

        spawn(config, job_manager.clone(), Duration::from_millis(20));

        for _ in 0..200 {
            if job_manager.current().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("scheduler never started a job");
    }
}
