//! Cooperative cancellation for backup runs.
//!
//! Mirrors `src/dbs/core/cancel.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`). A caller creates one [`CancelToken`], hands it to
//! the run, and calls [`CancelToken::cancel`] to request a graceful early
//! stop — the CLI's SIGINT (Ctrl+C) handler and a future web UI's "Stop"
//! button both do exactly this.
//!
//! The token is *polled*, never forced: whatever polls it (the service
//! between sources, the engine between fetched items — both land with
//! later issues) decides how to react; this module only owns the
//! thread-safe one-way signal itself. Backed by `Arc<AtomicBool>` rather
//! than the reference's `threading.Event` — cloning a [`CancelToken`]
//! shares the same underlying flag, so a single token is safe to hand to
//! every `--parallel` worker thread, same guarantee as the reference.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A thread-safe, one-way cooperative cancellation signal.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests cancellation. Idempotent; never un-sets.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True once [`Self::cancel`] has been called (on this token or any
    /// clone of it).
    pub fn cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled() {
        assert!(!CancelToken::new().cancelled());
    }

    #[test]
    fn cancel_sets_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        assert!(token.cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        assert!(token.cancelled());
    }

    #[test]
    fn clones_share_the_same_underlying_flag() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.cancelled());
        token.cancel();
        assert!(
            clone.cancelled(),
            "a clone must observe cancellation set on the original"
        );
    }

    #[test]
    fn independent_tokens_do_not_share_state() {
        let a = CancelToken::new();
        let b = CancelToken::new();
        a.cancel();
        assert!(a.cancelled());
        assert!(!b.cancelled());
    }

    #[test]
    fn cancellation_is_visible_across_threads() {
        let token = CancelToken::new();
        let worker_token = token.clone();
        let handle = std::thread::spawn(move || {
            while !worker_token.cancelled() {
                std::thread::yield_now();
            }
        });
        token.cancel();
        handle
            .join()
            .expect("worker thread should observe cancellation and exit");
    }
}
