//! Generic background-job manager with live progress broadcast over
//! Server-Sent Events, mirroring the shape of the reference's
//! `dbs.web.jobs.JobManager` (issue #80).
//!
//! This module is domain-agnostic: it doesn't know what a job *does*,
//! only how to run one at a time, track its state through
//! running/done/error, and fan its progress events out to any number of
//! live subscribers (buffered replay first, then live — a late
//! subscriber never misses the start of a still-running job). A caller
//! supplies the actual work as a closure; [`JobManager::start`] runs it
//! on a blocking thread via [`tokio::task::spawn_blocking`], per the
//! sync/async boundary decided in issue #79 — matching the reference's
//! own reason for a dedicated thread (a `BackupService`'s SQLite
//! connection is single-thread, so the work can't share the async
//! runtime's worker threads).
//!
//! **Not ported:** the reference's VPN-subprocess routing
//! (`_run_vpn_source`/`_run_all_mixed`/`_finish_vpn_source`) — sources
//! outside the VPN network namespace re-executing themselves as
//! `<vpn_exec> dbs backup <name>` subprocesses. `dbs-core`'s
//! `BackupService` already made a different, deliberate choice here (an
//! earlier issue, not #80): it refuses a `requires_vpn` source run
//! outside the right namespace instead of relaunching itself through a
//! wrapper (see `dbs_core::netns` / `BackupService`'s vpn guard). Adding
//! a second, contradictory subprocess-relaunch path here — one only the
//! web tier would have — isn't this issue's call to make.
//!
//! **Not wired up:** a concrete `/api/backup` route driving
//! `BackupService` through this manager. That needs the auth gate
//! (#81) first — starting a backup is a mutating action, and this
//! skeleton has no auth yet (see the `dbs-web` crate root doc-comment).
//! This issue's own acceptance criteria anticipate that: its tests
//! exercise the manager (including the SSE endpoint) with a *fake* job.
//! [`JobManager`]/[`sse_router`] are the reusable primitive a later
//! issue mounts a real job onto.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as AxumPath, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::{http::StatusCode, Router};
use futures_util::stream::{Stream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;

const MAX_BUFFERED_EVENTS: usize = 1000;
const MAX_FINISHED_JOBS: usize = 20;

/// Cooperative early-stop flag a job's work closure polls at its own
/// natural checkpoints (mirrors `dbs_core::CancelToken`'s shape; kept
/// as a separate small type here rather than depending on `dbs-core`
/// for it — see the module doc-comment on why this crate doesn't
/// depend on `dbs-core` yet).
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Done,
    Error,
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // A plain Unix-seconds stamp — good enough for this UI-ephemeral
    // state (never persisted, never parsed back), and avoids pulling in
    // a datetime-formatting crate just for this module.
    secs.to_string()
}

/// A message on a job's broadcast channel: either a progress event to
/// deliver, or the terminal signal that ends every live stream.
#[derive(Clone)]
enum JobMessage {
    Event(Value),
    Finished,
}

struct JobState {
    status: JobStatus,
    error: Option<String>,
    started_at: String,
    finished_at: Option<String>,
    events: Vec<Value>,
    results: Vec<Value>,
}

/// A single background job: its identity, cooperative cancel flag, and
/// the state/broadcast machinery [`JobManager`] and its subscribers
/// share. Cheap to clone (an `Arc`) — the work closure, the manager,
/// and every SSE subscriber all hold their own handle to the same job.
pub struct Job {
    id: u64,
    spec: Value,
    cancel: CancelToken,
    state: Mutex<JobState>,
    tx: broadcast::Sender<JobMessage>,
}

#[derive(Serialize)]
pub struct JobSnapshot {
    pub id: u64,
    pub spec: Value,
    pub status: JobStatus,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub stopping: bool,
    pub events: Vec<Value>,
    pub results: Vec<Value>,
}

/// One item [`Job::subscribe`]'s stream yields: an ordinary progress
/// `Data` payload, or the terminal `End` payload (the job's final
/// [`JobSnapshot`]) every subscriber gets exactly once, whether it was
/// watching live or only attached after the job had already finished.
/// Every `/api/*/:id/stream` consumer (`app.js`'s `streamSetup`/
/// `resumeResearchIfRunning`/`openProgress`) listens for a named `end`
/// SSE event carrying this snapshot to know the job is over —
/// [`stream_handler`] is what actually names it.
enum SseItem {
    Data(Value),
    End(Value),
}

impl Job {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn spec(&self) -> &Value {
        &self.spec
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn status(&self) -> JobStatus {
        self.state.lock().unwrap().status
    }

    /// Broadcasts one progress event to every live subscriber and
    /// appends it to the replay buffer (capped at
    /// [`MAX_BUFFERED_EVENTS`], oldest dropped first — job history is
    /// ephemeral UI state, not a durable record). A no-op once the job
    /// has finished (a work closure that emits after returning would
    /// otherwise "corrupt" an already-closed stream).
    pub fn emit(&self, event: Value) {
        let mut state = self.state.lock().unwrap();
        if state.status != JobStatus::Running {
            return;
        }
        state.events.push(event.clone());
        if state.events.len() > MAX_BUFFERED_EVENTS {
            let excess = state.events.len() - MAX_BUFFERED_EVENTS;
            state.events.drain(0..excess);
        }
        // Held across the send so a concurrent `subscribe()` can never
        // observe a buffered snapshot that's missing an event this
        // broadcast already delivered, or vice versa (see `subscribe`).
        let _ = self.tx.send(JobMessage::Event(event));
    }

    pub fn record_result(&self, result: Value) {
        self.state.lock().unwrap().results.push(result);
    }

    fn finish(&self, outcome: Result<(), String>) {
        let mut state = self.state.lock().unwrap();
        match outcome {
            Ok(()) => state.status = JobStatus::Done,
            Err(e) => {
                state.status = JobStatus::Error;
                state.error = Some(e);
            }
        }
        state.finished_at = Some(now_iso());
        let _ = self.tx.send(JobMessage::Finished);
    }

    pub fn snapshot(&self) -> JobSnapshot {
        let state = self.state.lock().unwrap();
        JobSnapshot {
            id: self.id,
            spec: self.spec.clone(),
            status: state.status,
            error: state.error.clone(),
            started_at: state.started_at.clone(),
            finished_at: state.finished_at.clone(),
            stopping: state.status == JobStatus::Running && self.cancel.is_cancelled(),
            events: state.events.clone(),
            results: state.results.clone(),
        }
    }

    /// Buffered events first, then live ones, ending with exactly one
    /// [`SseItem::End`] carrying the job's final snapshot — whether the
    /// job finishes while this subscriber is attached, or had already
    /// finished before `subscribe` was even called (a late attach still
    /// gets a real terminal event, not a stream that silently never
    /// closes). Mirrors the reference's `JobManager.stream` generator,
    /// extended with the terminal snapshot every `end`-event consumer
    /// needs.
    ///
    /// Runs the replay-then-forward loop on its own task so it can
    /// `.await` the broadcast channel; the returned stream is just that
    /// task's output channel, so a dropped/cancelled stream (a
    /// disconnected SSE client) cleanly stops the forwarding task too.
    fn subscribe(self: &Arc<Self>) -> impl Stream<Item = SseItem> {
        let job = self.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (buffered, live_rx) = {
                // Locked across the buffered-snapshot read and the
                // broadcast subscribe so no event emitted concurrently
                // can be missed (subscribed too late) or delivered
                // twice (subscribed too early, then also replayed).
                let state = job.state.lock().unwrap();
                let buffered = state.events.clone();
                let live_rx = (state.status == JobStatus::Running).then(|| job.tx.subscribe());
                (buffered, live_rx)
            };
            for event in buffered {
                if tx.send(SseItem::Data(event)).is_err() {
                    return;
                }
            }
            let snapshot_value = || serde_json::to_value(job.snapshot()).unwrap_or(Value::Null);
            let Some(mut live_rx) = live_rx else {
                // Already finished before this subscriber attached —
                // still owed a terminal event, just immediately.
                let _ = tx.send(SseItem::End(snapshot_value()));
                return;
            };
            loop {
                match live_rx.recv().await {
                    Ok(JobMessage::Event(event)) => {
                        if tx.send(SseItem::Data(event)).is_err() {
                            return;
                        }
                    }
                    Ok(JobMessage::Finished) => {
                        let _ = tx.send(SseItem::End(snapshot_value()));
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                    // A slow subscriber fell behind the broadcast's
                    // ring buffer; skip ahead rather than end the
                    // stream over it — the buffered replay already
                    // covers everything before this subscription.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
        UnboundedReceiverStream::new(rx)
    }
}

pub struct JobAlreadyRunning;

struct Inner {
    counter: u64,
    current: Option<u64>,
    by_id: HashMap<u64, Arc<Job>>,
}

/// Owns the at-most-one active job and every job's recent history
/// (finished jobs beyond [`MAX_FINISHED_JOBS`] are evicted, oldest
/// first — same ephemeral-history bound as the reference).
pub struct JobManager {
    inner: Mutex<Inner>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                counter: 0,
                current: None,
                by_id: HashMap::new(),
            }),
        }
    }

    /// Starts `work` as a new job unless one is already running.
    /// `work` receives the new [`Job`] to `emit`/`record_result` on as
    /// it goes, and returns `Ok(())` or `Err(message)` to finish it —
    /// it runs on a blocking thread ([`tokio::task::spawn_blocking`]),
    /// so it may block freely (synchronous I/O, a `BackupService`
    /// call, ...).
    pub fn start<F>(&self, spec: Value, work: F) -> Result<Arc<Job>, JobAlreadyRunning>
    where
        F: FnOnce(Arc<Job>) -> Result<(), String> + Send + 'static,
    {
        let mut inner = self.inner.lock().unwrap();
        if let Some(current) = inner.current.and_then(|id| inner.by_id.get(&id)) {
            if current.status() == JobStatus::Running {
                return Err(JobAlreadyRunning);
            }
        }
        inner.counter += 1;
        let id = inner.counter;
        let (tx, _rx) = broadcast::channel(1024);
        let job = Arc::new(Job {
            id,
            spec,
            cancel: CancelToken::new(),
            state: Mutex::new(JobState {
                status: JobStatus::Running,
                error: None,
                started_at: now_iso(),
                finished_at: None,
                events: Vec::new(),
                results: Vec::new(),
            }),
            tx,
        });
        inner.current = Some(id);
        inner.by_id.insert(id, job.clone());
        evict_finished(&mut inner.by_id, MAX_FINISHED_JOBS);
        drop(inner);

        let job_for_thread = job.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = work(job_for_thread.clone());
            job_for_thread.finish(outcome);
        });
        Ok(job)
    }

    /// Requests a graceful early stop of a running job (its cancel
    /// token is set; the job itself decides when it's safe to notice).
    /// `true` iff `job_id` names a currently-running job.
    pub fn cancel(&self, job_id: u64) -> bool {
        match self.inner.lock().unwrap().by_id.get(&job_id) {
            Some(job) if job.status() == JobStatus::Running => {
                job.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, job_id: u64) -> Option<Arc<Job>> {
        self.inner.lock().unwrap().by_id.get(&job_id).cloned()
    }

    pub fn current(&self) -> Option<Arc<Job>> {
        let inner = self.inner.lock().unwrap();
        inner.current.and_then(|id| inner.by_id.get(&id).cloned())
    }
}

/// Drops all but the newest `keep` finished jobs (caller holds the
/// manager lock). Mirrors the reference's `_evict_finished`.
fn evict_finished(by_id: &mut HashMap<u64, Arc<Job>>, keep: usize) {
    let mut finished: Vec<u64> = by_id
        .iter()
        .filter(|(_, job)| job.status() != JobStatus::Running)
        .map(|(id, _)| *id)
        .collect();
    finished.sort_unstable();
    if finished.len() > keep {
        for id in &finished[..finished.len() - keep] {
            by_id.remove(id);
        }
    }
}

async fn snapshot_handler(
    State(manager): State<Arc<JobManager>>,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    match manager.get(id) {
        Some(job) => Json(job.snapshot()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stream_handler(
    State(manager): State<Arc<JobManager>>,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    let Some(job) = manager.get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let events = job.subscribe().map(|item| {
        let event = match item {
            SseItem::Data(v) => Event::default().data(v.to_string()),
            SseItem::End(v) => Event::default().event("end").data(v.to_string()),
        };
        Ok::<_, Infallible>(event)
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// `GET /:id` (a [`JobSnapshot`] as JSON) and `GET /:id/stream` (its
/// progress as Server-Sent Events) over `manager`. Not mounted into
/// [`crate::router`] yet — see the module doc-comment.
pub fn sse_router(manager: Arc<JobManager>) -> Router {
    Router::new()
        .route("/:id", get(snapshot_handler))
        .route("/:id/stream", get(stream_handler))
        .with_state(manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::time::Duration as StdDuration;
    use tower::ServiceExt;

    #[tokio::test]
    async fn a_job_starts_running_and_a_second_start_is_refused_while_it_runs() {
        let manager = JobManager::new();
        let job = manager
            .start(json!({"kind": "fake"}), |_job| {
                std::thread::sleep(StdDuration::from_millis(200));
                Ok(())
            })
            .ok()
            .unwrap();
        assert_eq!(job.status(), JobStatus::Running);
        assert!(manager.start(json!({}), |_| Ok(())).is_err());
    }

    #[tokio::test]
    async fn a_successful_job_transitions_to_done_and_records_its_result() {
        let manager = JobManager::new();
        let job = manager
            .start(json!({"kind": "fake"}), |job| {
                job.record_result(json!({"ok": true}));
                Ok(())
            })
            .ok()
            .unwrap();
        for _ in 0..200 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert_eq!(job.status(), JobStatus::Done);
        let snap = job.snapshot();
        assert!(snap.error.is_none());
        assert!(snap.finished_at.is_some());
        assert_eq!(snap.results, vec![json!({"ok": true})]);
    }

    #[tokio::test]
    async fn a_failing_job_transitions_to_error_with_its_message() {
        let manager = JobManager::new();
        let job = manager
            .start(json!({}), |_job| Err("boom".to_string()))
            .ok()
            .unwrap();
        for _ in 0..200 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert_eq!(job.status(), JobStatus::Error);
        assert_eq!(job.snapshot().error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn cancel_sets_the_token_a_running_job_can_observe() {
        let manager = JobManager::new();
        let job = manager
            .start(json!({}), |job| {
                for _ in 0..500 {
                    if job.is_cancelled() {
                        return Ok(());
                    }
                    std::thread::sleep(StdDuration::from_millis(5));
                }
                Err("never noticed cancellation".to_string())
            })
            .ok()
            .unwrap();
        assert!(manager.cancel(job.id()));
        for _ in 0..200 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert_eq!(job.status(), JobStatus::Done);
    }

    #[tokio::test]
    async fn cancel_of_an_unknown_or_already_finished_job_is_a_no_op() {
        let manager = JobManager::new();
        assert!(!manager.cancel(999));
        let job = manager.start(json!({}), |_| Ok(())).ok().unwrap();
        std::thread::sleep(StdDuration::from_millis(50));
        assert!(!manager.cancel(job.id()));
    }

    #[tokio::test]
    async fn the_snapshot_route_serves_a_finished_jobs_state_as_json() {
        let manager = Arc::new(JobManager::new());
        let job = manager
            .start(json!({"kind": "fake"}), |job| {
                job.emit(json!({"phase": "start"}));
                Ok(())
            })
            .ok()
            .unwrap();
        for _ in 0..200 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }

        let router = sse_router(manager);
        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/{}", job.id()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let snap: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snap["status"], "done");
        assert_eq!(snap["events"], json!([{"phase": "start"}]));
    }

    #[tokio::test]
    async fn the_snapshot_route_404s_for_an_unknown_job_id() {
        let manager = Arc::new(JobManager::new());
        let router = sse_router(manager);
        let response = router
            .oneshot(Request::builder().uri("/999").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_stream_route_emits_every_event_a_fake_job_produces_then_closes() {
        let manager = Arc::new(JobManager::new());
        let job = manager
            .start(json!({"kind": "fake"}), |job| {
                job.emit(json!({"phase": "one"}));
                job.emit(json!({"phase": "two"}));
                Ok(())
            })
            .ok()
            .unwrap();

        let router = sse_router(manager);
        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/{}/stream", job.id()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains(r#"data: {"phase":"one"}"#), "{body}");
        assert!(body.contains(r#"data: {"phase":"two"}"#), "{body}");
        assert!(body.contains("event: end"), "{body}");
        assert!(body.contains(r#""status":"done""#), "{body}");
    }

    #[tokio::test]
    async fn the_stream_route_emits_end_immediately_for_a_job_that_already_finished() {
        // A late subscriber — attached only after the job finished —
        // must still get a real terminal event rather than a stream
        // that opens and silently never closes.
        let manager = Arc::new(JobManager::new());
        let job = manager.start(json!({}), |_job| Ok(())).ok().unwrap();
        for _ in 0..200 {
            if job.status() != JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(job.status(), JobStatus::Done);

        let router = sse_router(manager);
        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/{}/stream", job.id()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("event: end"), "{body}");
        assert!(body.contains(r#""status":"done""#), "{body}");
    }

    #[tokio::test]
    async fn the_stream_route_404s_for_an_unknown_job_id() {
        let manager = Arc::new(JobManager::new());
        let router = sse_router(manager);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/999/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn eviction_keeps_only_the_newest_finished_jobs() {
        // Eviction runs inside `start()`, over whatever's already
        // finished at that moment (mirrors the reference: there's no
        // eviction pass triggered by a job finishing on its own, only
        // by the next job starting) — so 25 finished jobs plus one more
        // `start()` to trigger the pass over all of them.
        let manager = JobManager::new();
        let mut ids = Vec::new();
        for _ in 0..25 {
            let job = manager.start(json!({}), |_| Ok(())).ok().unwrap();
            ids.push(job.id());
            // Force sequential (not concurrent) jobs — the manager only
            // allows one running job at a time, so wait it out.
            for _ in 0..200 {
                if job.status() != JobStatus::Running {
                    break;
                }
                std::thread::sleep(StdDuration::from_millis(5));
            }
        }
        let last = manager.start(json!({}), |_| Ok(())).ok().unwrap();

        let remaining: Vec<u64> = ids
            .iter()
            .filter(|id| manager.get(**id).is_some())
            .copied()
            .collect();
        assert_eq!(remaining.len(), MAX_FINISHED_JOBS);
        // The newest jobs survive, oldest evicted first.
        assert_eq!(remaining, ids[ids.len() - MAX_FINISHED_JOBS..]);
        assert!(manager.get(last.id()).is_some());
    }
}
