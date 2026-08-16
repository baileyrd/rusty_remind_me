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
//!
//! `python_launch` (issue #99) is new infrastructure this crate's other
//! modules don't have a direct source-file counterpart for: the
//! reference's `_playwright.py` drives Playwright in-process, which has
//! no Rust equivalent, so browser-automation connectors here instead
//! shell out to a separate Python/Playwright script — this module is the
//! generic launcher half of that split.
//!
//! `tiptap` (issue #100) is a direct, node-for-node port of
//! `connectors/_tiptap.py` — TipTap/ProseMirror rich-text JSON to
//! Markdown, used by `skool` lesson descriptions.
//!
//! `subprocess_main` (issue #161) is this crate's first module to
//! depend on `dbs-core` itself — every other module here is
//! `dbs-core`-agnostic connector plumbing, but the whole point of this
//! one is bridging a `dbs_core::Connector` implementation to
//! `dbs_core::run_stream`'s wire protocol (#157), so the dependency is
//! unavoidable and correct: this crate sits *below* the individual
//! `dbs-connector-<type>` binaries, which depend on both it and
//! `dbs-core`.

pub mod python_launch;
pub mod subprocess_main;
pub mod tiptap;
pub mod watchdog;

pub use python_launch::{
    find_python, run_python_script, run_python_script_using, PythonLaunchError,
};
pub use subprocess_main::run_connector_main;
pub use tiptap::tiptap_markdown;
pub use watchdog::{run_with_watchdog, WatchdogError, WatchdogTimeout};
