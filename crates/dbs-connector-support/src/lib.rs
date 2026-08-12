//! Shared helpers for `rusty_dbs` connector implementations.
//!
//! Mirrors `src/dbs/connectors/_util.py` in baileyrd/Daily-Backup-System
//! (pinned `@6cc6491`): "not part of the `dbs.core` public contract ...
//! implementation details of the built-in connectors only, free to change
//! without a `CORE_API_VERSION` bump." That's a real crate boundary in
//! this port, not just a doc note — `dbs-core` is the host's contract;
//! this crate is what a `dbs-connector-<type>` binary (ADR-0001) links
//! against internally. New crate, first connector-side shared code.
//!
//! `impersonate_target` (yt-dlp/`curl_cffi` TLS-fingerprint tuning) isn't
//! ported — round-1's browser-automation decision (`gap-analysis.md`) has
//! `rusty_dbs` shell out to the yt-dlp *binary*, not call its Python
//! library API, so this Python-library-specific helper has no Rust
//! equivalent to write. `ext_for_mime` is deferred to whichever media/
//! export issue actually needs it — out of scope for this one.

pub mod watchdog;

pub use watchdog::{run_with_watchdog, WatchdogError, WatchdogTimeout};
