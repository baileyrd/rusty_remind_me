//! Visibility into a tool call that is stuck, not merely slow.
//!
//! The gap this closes (reference issue #128): when a call hangs, the only
//! symptom visible from outside the process is the client reporting a timeout.
//! Nothing in the server says *which* call, or how long it has been going.
//! Diagnosing a real incident on the reference side — a correlated subquery
//! pegging a core for minutes — meant attaching `py-spy` to a live process.
//!
//! # What this is not
//!
//! The reference arms `faulthandler.dump_traceback_later`, which dumps *every*
//! thread's stack, including one blocked in synchronous CPU-bound code. There
//! is no stdlib equivalent in Rust, and reaching for a stack-unwinding crate to
//! chase one would be a new third-party dependency for a diagnostic.
//!
//! So this reports identity and duration rather than a stack: *which* call has
//! been running, and for how long. That preserves the property that actually
//! mattered — a stuck call names itself from outside, without a debugger — and
//! is a strictly weaker signal than the reference's. Said plainly here rather
//! than left for someone to discover when the dump they expected is a log line.
//!
//! # Reference-counted, because calls overlap
//!
//! Arming is not per-call: several tool calls can be in flight at once, and a
//! watchdog tied to one of them would disarm while the others are still
//! running. The reference counts in-flight calls and only cancels on the
//! transition back to zero. Here that counting is [`CallGuard`]'s job —
//! dropping the guard is what disarms, so an early return or a panic cannot
//! leak a permanently-armed watchdog the way an unbalanced manual `disarm()`
//! can.
//!
//! # Off by default is not the same as disabled by default
//!
//! The threshold is read from [`SLOW_CALL_SECONDS_ENV`] and defaults to 30
//! seconds, matching the reference. `0` disables the watchdog entirely, also
//! matching. A malformed value falls back to the default rather than
//! disabling: a typo in a diagnostic's tuning knob should not silently turn the
//! diagnostic off.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Environment variable holding the threshold, in seconds.
pub const SLOW_CALL_SECONDS_ENV: &str = "REMIND_ME_SLOW_CALL_SECONDS";

/// Threshold used when [`SLOW_CALL_SECONDS_ENV`] is unset or malformed.
pub const DEFAULT_SLOW_CALL_SECONDS: f64 = 30.0;

/// A call that has been running longer than the threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckCall {
    /// The tool name, e.g. `remind_me_search`.
    pub tool: String,
    /// How long it had been running when the watchdog noticed.
    pub elapsed: Duration,
}

/// Where a stuck-call report goes.
///
/// Injectable so the firing behaviour is testable without capturing stderr or
/// waiting on a real 30-second threshold. Production uses [`stderr_sink`].
pub type Sink = Arc<dyn Fn(&StuckCall) + Send + Sync>;

/// The default sink: one line per stuck call, matching this crate's
/// `subsystem: message` convention.
pub fn stderr_sink() -> Sink {
    Arc::new(|stuck: &StuckCall| {
        eprintln!(
            "watchdog: {} has been running for {:.1}s",
            stuck.tool,
            stuck.elapsed.as_secs_f64()
        );
    })
}

/// Resolve the configured threshold. `None` means the watchdog is disabled.
///
/// Read on construction rather than per-call: the reference reads its own
/// `SLOW_CALL_SECONDS` once at import, and a threshold that changes underneath
/// an in-flight call would make the report unreproducible.
pub fn configured_threshold() -> Option<Duration> {
    let seconds = match std::env::var(SLOW_CALL_SECONDS_ENV) {
        Ok(raw) => raw
            .trim()
            .parse::<f64>()
            .unwrap_or(DEFAULT_SLOW_CALL_SECONDS),
        Err(_) => DEFAULT_SLOW_CALL_SECONDS,
    };
    if seconds > 0.0 && seconds.is_finite() {
        Some(Duration::from_secs_f64(seconds))
    } else {
        // 0 (or a negative/NaN value) is the reference's "off" switch.
        None
    }
}

/// What the watchdog reports about itself, for `remind_me_server_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchdogStatus {
    pub enabled: bool,
    /// `None` when disabled.
    pub threshold_seconds: Option<f64>,
    pub calls_in_flight: usize,
}

#[derive(Debug)]
struct InflightCall {
    tool: String,
    started: Instant,
    /// Set once this call has been reported, so a single stuck call produces
    /// one line rather than one per monitor wake-up.
    reported: bool,
}

#[derive(Debug, Default)]
struct State {
    inflight: HashMap<u64, InflightCall>,
    shutdown: bool,
}

/// A stuck-call watchdog over one set of in-flight calls.
///
/// Ordinary use is the process-wide instance behind [`arm`] / [`status`].
/// Constructing one directly is for tests, which need their own threshold and
/// sink without racing every other test through a global.
pub struct Watchdog {
    threshold: Option<Duration>,
    state: Arc<(Mutex<State>, Condvar)>,
    next_id: AtomicU64,
    monitor_started: AtomicBool,
    sink: Sink,
}

impl Watchdog {
    /// Build a watchdog. `threshold` of `None` makes every operation a no-op.
    pub fn new(threshold: Option<Duration>, sink: Sink) -> Self {
        Self {
            threshold,
            state: Arc::new((Mutex::new(State::default()), Condvar::new())),
            next_id: AtomicU64::new(0),
            monitor_started: AtomicBool::new(false),
            sink,
        }
    }

    /// The process-wide watchdog, configured from the environment.
    pub fn global() -> &'static Watchdog {
        static GLOBAL: OnceLock<Watchdog> = OnceLock::new();
        GLOBAL.get_or_init(|| Watchdog::new(configured_threshold(), stderr_sink()))
    }

    /// Register a call as in flight. Dropping the returned guard ends it.
    ///
    /// When the watchdog is disabled the guard is inert, so callers need no
    /// enabled-check of their own.
    pub fn arm(&self, tool: &str) -> CallGuard<'_> {
        let Some(_) = self.threshold else {
            return CallGuard {
                watchdog: self,
                id: None,
            };
        };
        self.ensure_monitor();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (lock, condvar) = &*self.state;
        {
            let mut state = lock.lock().expect("watchdog state poisoned");
            state.inflight.insert(
                id,
                InflightCall {
                    tool: tool.to_string(),
                    started: Instant::now(),
                    reported: false,
                },
            );
        }
        // Wake the monitor so it recomputes its deadline: this call may be due
        // sooner than whatever it was already waiting for.
        condvar.notify_all();
        CallGuard {
            watchdog: self,
            id: Some(id),
        }
    }

    fn disarm(&self, id: u64) {
        let (lock, condvar) = &*self.state;
        {
            let mut state = lock.lock().expect("watchdog state poisoned");
            state.inflight.remove(&id);
        }
        condvar.notify_all();
    }

    /// Report the watchdog's own state.
    pub fn status(&self) -> WatchdogStatus {
        let (lock, _) = &*self.state;
        let inflight = lock.lock().map(|state| state.inflight.len()).unwrap_or(0);
        WatchdogStatus {
            enabled: self.threshold.is_some(),
            threshold_seconds: self.threshold.map(|t| t.as_secs_f64()),
            calls_in_flight: inflight,
        }
    }

    /// Start the monitor thread, once, on the first armed call.
    ///
    /// Lazily rather than at construction so a process that never serves a tool
    /// call — the CLI's one-shot subcommands, every test that only builds a
    /// server — never pays for a thread it will not use.
    fn ensure_monitor(&self) {
        if self.monitor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(threshold) = self.threshold else {
            return;
        };
        let state = Arc::clone(&self.state);
        let sink = Arc::clone(&self.sink);
        std::thread::Builder::new()
            .name("remind-me-watchdog".to_string())
            .spawn(move || monitor_loop(state, threshold, sink))
            // A watchdog that cannot start is a lost diagnostic, never a
            // reason to fail the server that was about to do real work.
            .map(|_| ())
            .unwrap_or_else(|e| eprintln!("watchdog: could not start monitor thread: {}", e));
    }

    /// Stop the monitor thread. Used by tests; the global instance runs for the
    /// life of the process.
    fn shutdown(&self) {
        let (lock, condvar) = &*self.state;
        if let Ok(mut state) = lock.lock() {
            state.shutdown = true;
        }
        condvar.notify_all();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn monitor_loop(state: Arc<(Mutex<State>, Condvar)>, threshold: Duration, sink: Sink) {
    let (lock, condvar) = &*state;
    let mut guard = lock.lock().expect("watchdog state poisoned");
    loop {
        if guard.shutdown {
            return;
        }

        // Report anything past the threshold, and find when the soonest
        // not-yet-due call becomes due.
        let now = Instant::now();
        let mut soonest: Option<Duration> = None;
        let mut due: Vec<StuckCall> = Vec::new();
        for call in guard.inflight.values_mut() {
            if call.reported {
                continue;
            }
            let elapsed = now.saturating_duration_since(call.started);
            if elapsed >= threshold {
                call.reported = true;
                due.push(StuckCall {
                    tool: call.tool.clone(),
                    elapsed,
                });
            } else {
                let remaining = threshold - elapsed;
                soonest = Some(soonest.map_or(remaining, |s: Duration| s.min(remaining)));
            }
        }

        if !due.is_empty() {
            // Report without the lock held: a sink is caller-supplied code and
            // must not be able to block every arm/disarm in the process.
            drop(guard);
            for stuck in &due {
                sink(stuck);
            }
            guard = lock.lock().expect("watchdog state poisoned");
            continue;
        }

        guard = match soonest {
            Some(wait) => {
                condvar
                    .wait_timeout(guard, wait)
                    .expect("watchdog state poisoned")
                    .0
            }
            // Nothing pending: sleep until an arm or a shutdown wakes us.
            None => condvar.wait(guard).expect("watchdog state poisoned"),
        };
    }
}

/// Holds one call's registration. Dropping it disarms that call.
pub struct CallGuard<'a> {
    watchdog: &'a Watchdog,
    /// `None` when the watchdog is disabled — nothing was registered.
    id: Option<u64>,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.watchdog.disarm(id);
        }
    }
}

/// Register a call as in flight on the process-wide watchdog.
pub fn arm(tool: &str) -> CallGuard<'static> {
    Watchdog::global().arm(tool)
}

/// The process-wide watchdog's state, for `remind_me_server_status`.
pub fn status() -> WatchdogStatus {
    Watchdog::global().status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A sink that forwards every report to a channel the test can drain.
    fn channel_sink() -> (Sink, mpsc::Receiver<StuckCall>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        (
            Arc::new(move |stuck: &StuckCall| {
                let _ = tx.lock().expect("sink lock").send(stuck.clone());
            }),
            rx,
        )
    }

    /// Short enough to keep the suite fast, long enough that a fast call
    /// finishing inside it is not a race.
    const THRESHOLD: Duration = Duration::from_millis(120);

    #[test]
    fn a_call_that_exceeds_the_threshold_is_reported() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let _guard = watchdog.arm("remind_me_search");
        let stuck = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a call held past the threshold should be reported");
        assert_eq!(stuck.tool, "remind_me_search");
        assert!(stuck.elapsed >= THRESHOLD);
    }

    #[test]
    fn a_fast_call_is_not_reported() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        drop(watchdog.arm("remind_me_add"));

        // Wait past the threshold: if the disarm did not take, this is when a
        // spurious report would arrive.
        assert!(
            rx.recv_timeout(THRESHOLD * 4).is_err(),
            "a call that finished before the threshold must not be reported"
        );
    }

    /// The reference's reason for reference-counting: concurrent calls are
    /// normal, and the first one finishing must not silence the others.
    #[test]
    fn an_overlapping_call_is_still_watched_after_the_first_finishes() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let slow = watchdog.arm("remind_me_maintenance");
        drop(watchdog.arm("remind_me_add")); // finishes immediately

        let stuck = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the still-running call must still be reported");
        assert_eq!(stuck.tool, "remind_me_maintenance");
        drop(slow);
    }

    #[test]
    fn a_stuck_call_is_reported_once_not_once_per_wakeup() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let _guard = watchdog.arm("remind_me_search");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("first report");

        assert!(
            rx.recv_timeout(THRESHOLD * 4).is_err(),
            "one stuck call should produce one report, not a stream"
        );
    }

    #[test]
    fn a_disabled_watchdog_reports_nothing_and_counts_nothing() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(None, sink);

        let _guard = watchdog.arm("remind_me_search");
        assert_eq!(watchdog.status().calls_in_flight, 0);
        assert!(!watchdog.status().enabled);
        assert!(
            rx.recv_timeout(THRESHOLD * 4).is_err(),
            "a disabled watchdog must never report"
        );
    }

    #[test]
    fn status_tracks_calls_in_flight() {
        let (sink, _rx) = channel_sink();
        let watchdog = Watchdog::new(Some(Duration::from_secs(60)), sink);

        assert_eq!(watchdog.status().calls_in_flight, 0);
        let first = watchdog.arm("a");
        assert_eq!(watchdog.status().calls_in_flight, 1);
        let second = watchdog.arm("b");
        assert_eq!(watchdog.status().calls_in_flight, 2);
        drop(second);
        assert_eq!(watchdog.status().calls_in_flight, 1);
        drop(first);
        assert_eq!(watchdog.status().calls_in_flight, 0);
    }

    #[test]
    fn status_reports_the_configured_threshold() {
        let (sink, _rx) = channel_sink();
        let watchdog = Watchdog::new(Some(Duration::from_secs(30)), sink);
        assert_eq!(watchdog.status().threshold_seconds, Some(30.0));

        let (sink, _rx) = channel_sink();
        let disabled = Watchdog::new(None, sink);
        assert_eq!(disabled.status().threshold_seconds, None);
    }

    /// A panic must not leave the watchdog permanently armed — the failure the
    /// reference's manual `arm()`/`disarm()` pairing is exposed to and this
    /// guard is not.
    ///
    /// Only `calls_in_flight` is asserted, deliberately. Asserting "and nothing
    /// was reported" fails here under parallel test load, and correctly so:
    /// unwinding prints a backtrace before the guard drops, which on a loaded
    /// machine takes longer than this suite's deliberately-short threshold. The
    /// resulting report is a true positive about the panic machinery, not a
    /// disarm that did not happen — and the disarm is what this test is about.
    #[test]
    fn a_panicking_call_still_disarms() {
        let (sink, _rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = watchdog.arm("remind_me_search");
            panic!("boom");
        }));
        assert!(result.is_err(), "the test's own panic should propagate");

        assert_eq!(
            watchdog.status().calls_in_flight,
            0,
            "unwinding past the guard must still disarm the call"
        );
    }
}
