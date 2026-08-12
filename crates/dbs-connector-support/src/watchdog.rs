//! A wall-clock (or stall) watchdog around a blocking call.
//!
//! Mirrors `run_with_watchdog`/`WatchdogTimeout` in
//! `src/dbs/connectors/_util.py` (pinned `@6cc6491`). Exists because a
//! shelled-out subprocess (yt-dlp, a browser-automation helper — round-1's
//! decision, see `gap-analysis.md`) can hang indefinitely with no
//! call-level timeout of its own, which would otherwise block a scheduled
//! backup run forever.
//!
//! Rust threads can't be force-killed either (same constraint the
//! reference calls out for Python threads), so on timeout the worker is
//! *abandoned* — left running detached (it exits on its own once the
//! subprocess it's blocked on finishes or its own I/O times out) while the
//! caller gets a [`WatchdogError::Timeout`] to classify as transient and
//! move on.

use std::fmt;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// A watched call exceeded its deadline and was abandoned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogTimeout {
    pub description: String,
    pub timeout: Duration,
    /// True if a `heartbeat` was supplied (a *stall* deadline); false if
    /// this was a plain wall-clock deadline from the start of the call.
    pub had_heartbeat: bool,
}

impl fmt::Display for WatchdogTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: no {} in {}s; abandoning the call",
            self.description,
            if self.had_heartbeat {
                "progress"
            } else {
                "completion"
            },
            self.timeout.as_secs()
        )
    }
}

impl std::error::Error for WatchdogTimeout {}

/// Outcome of [`run_with_watchdog`] beyond a plain success.
#[derive(Debug)]
pub enum WatchdogError<E> {
    /// The call was abandoned past its deadline.
    Timeout(WatchdogTimeout),
    /// `f` returned an error itself (propagated, not a watchdog concern).
    Inner(E),
    /// The worker thread panicked before it could report a result.
    WorkerPanicked,
}

impl<E: fmt::Display> fmt::Display for WatchdogError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(t) => write!(f, "{t}"),
            Self::Inner(e) => write!(f, "{e}"),
            Self::WorkerPanicked => write!(f, "watchdog worker thread panicked"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for WatchdogError<E> {}

/// Runs `f` on a background thread and abandons it past `timeout`.
///
/// Without `heartbeat`, the deadline is wall-clock from the start of the
/// call. With it — a callback returning the [`Instant`] of the last
/// observed activity (e.g. fed by a subprocess's progress output) — it
/// becomes a *stall* deadline that keeps resetting while progress is
/// being made, so a big-but-healthy transfer is never cut off.
///
/// `timeout` of zero disables the watchdog (`f` runs inline on the
/// calling thread, and any panic propagates normally instead of becoming
/// [`WatchdogError::WorkerPanicked`]).
pub fn run_with_watchdog<F, T, E>(
    f: F,
    timeout: Duration,
    description: &str,
    heartbeat: Option<&(dyn Fn() -> Instant + Sync)>,
) -> Result<T, WatchdogError<E>>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    if timeout.is_zero() {
        return f().map_err(WatchdogError::Inner);
    }

    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    thread::Builder::new()
        .name(format!("dbs-watchdog: {description}"))
        .spawn(move || {
            let _ = tx.send(f());
        })
        .expect("failed to spawn watchdog worker thread");

    let poll_interval = Duration::from_secs(1).min(timeout);
    loop {
        match rx.recv_timeout(poll_interval) {
            Ok(result) => return result.map_err(WatchdogError::Inner),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let last_activity = heartbeat.map(|h| h()).unwrap_or(start);
                if last_activity.elapsed() > timeout {
                    return Err(WatchdogError::Timeout(WatchdogTimeout {
                        description: description.to_string(),
                        timeout,
                        had_heartbeat: heartbeat.is_some(),
                    }));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(WatchdogError::WorkerPanicked);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn zero_timeout_runs_inline_and_returns_result() {
        let result: Result<i32, WatchdogError<()>> =
            run_with_watchdog(|| Ok(42), Duration::ZERO, "inline", None);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn fast_call_completes_before_the_deadline() {
        let result: Result<i32, WatchdogError<()>> =
            run_with_watchdog(|| Ok(7), Duration::from_secs(5), "fast", None);
        assert_eq!(result.unwrap(), 7);
    }

    #[test]
    fn inner_error_propagates_as_inner_variant() {
        let result: Result<i32, WatchdogError<&str>> =
            run_with_watchdog(|| Err("boom"), Duration::from_secs(5), "erroring", None);
        assert!(matches!(result, Err(WatchdogError::Inner("boom"))));
    }

    #[test]
    fn stalled_call_without_heartbeat_times_out() {
        let result: Result<i32, WatchdogError<()>> = run_with_watchdog(
            || {
                thread::sleep(Duration::from_millis(500));
                Ok(1)
            },
            Duration::from_millis(50),
            "stalled",
            None,
        );
        match result {
            Err(WatchdogError::Timeout(t)) => assert!(!t.had_heartbeat),
            other => panic!("expected a Timeout error, got {other:?}"),
        }
    }

    #[test]
    fn active_heartbeat_prevents_timeout_during_long_but_healthy_work() {
        let last_activity = Arc::new(std::sync::Mutex::new(Instant::now()));
        let heartbeat_activity = Arc::clone(&last_activity);
        let heartbeat = move || *heartbeat_activity.lock().unwrap();

        let ticker_activity = Arc::clone(&last_activity);
        let stop = Arc::new(AtomicBool::new(false));
        let ticker_stop = Arc::clone(&stop);
        let ticker = thread::spawn(move || {
            while !ticker_stop.load(Ordering::Relaxed) {
                *ticker_activity.lock().unwrap() = Instant::now();
                thread::sleep(Duration::from_millis(20));
            }
        });

        let result: Result<i32, WatchdogError<()>> = run_with_watchdog(
            || {
                thread::sleep(Duration::from_millis(300));
                Ok(99)
            },
            Duration::from_millis(80),
            "long-but-healthy",
            Some(&heartbeat),
        );

        stop.store(true, Ordering::Relaxed);
        ticker.join().unwrap();
        assert_eq!(result.unwrap(), 99);
    }

    #[test]
    fn worker_panic_reports_worker_panicked() {
        let result: Result<i32, WatchdogError<()>> = run_with_watchdog(
            || panic!("simulated worker panic"),
            Duration::from_millis(500),
            "panicking",
            None,
        );
        assert!(matches!(result, Err(WatchdogError::WorkerPanicked)));
    }

    #[test]
    fn timeout_display_mentions_progress_when_heartbeat_present() {
        let heartbeat = || Instant::now() - Duration::from_secs(100);
        let result: Result<i32, WatchdogError<()>> = run_with_watchdog(
            || {
                thread::sleep(Duration::from_millis(300));
                Ok(1)
            },
            Duration::from_millis(50),
            "watched",
            Some(&heartbeat),
        );
        match result {
            Err(WatchdogError::Timeout(t)) => assert!(t.to_string().contains("progress")),
            other => panic!("expected a Timeout error, got {other:?}"),
        }
    }
}
