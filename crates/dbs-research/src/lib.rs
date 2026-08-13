//! Ad-hoc YouTube-to-NotebookLM research pipeline (issue #84),
//! mirroring `dbs.research` in baileyrd/Daily-Backup-System. Not a
//! backup source — `dbs research youtube`/`dbs research
//! youtube-backup` are one-shot pipelines with nothing persisted
//! between invocations, which is why this lives in its own crate
//! rather than folded into `dbs-core`'s `Connector`/`Storage`
//! machinery (see [`models`]'s doc-comment).
//!
//! - [`models`] — pure data types, no I/O.
//! - [`youtube_search`] — real, yt-dlp-subprocess-backed video search.
//! - [`notebooklm`] — the synthesis client boundary; concrete adapter
//!   not implemented yet, see its doc-comment.
//! - [`pipeline`] — orchestrates the two into a [`models::ResearchResult`].
//! - [`report`] — renders that result as a Markdown report.
//!
//! **Not wired up:** `dbs-cli`'s `dbs research` subcommands (issue
//! #77) still report their own "not yet implemented" stub — wiring
//! them to call into this crate is a natural follow-up, but the
//! [`notebooklm::UnimplementedClient`] this crate ships means that
//! wiring wouldn't change user-visible behavior yet anyway (every real
//! run would still fail at the NotebookLM step). Landing the pipeline
//! itself first, fully real and tested up to that boundary, mirrors
//! how issue #80's job manager landed ahead of its own route wiring.

pub mod models;
pub mod notebooklm;
pub mod pipeline;
pub mod report;
pub mod youtube_search;

/// An RFC 3339 UTC timestamp, seconds precision — used for
/// [`models::ResearchResult::generated_at`]. This crate deliberately
/// doesn't depend on `dbs-core` (see the module doc-comment), so it
/// doesn't reuse `dbs_core::iso_z`; the format is compatible (both
/// produce a `Z`-suffixed UTC RFC 3339 string).
pub(crate) fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
