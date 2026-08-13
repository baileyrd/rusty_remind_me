//! The NotebookLM synthesis client boundary — mirrors
//! `dbs.research.notebooklm_client`.
//!
//! **The concrete adapter isn't implemented yet.** Per gap-analysis.md's
//! Decision 4, this port integrates with NotebookLM by shelling out to
//! [jacob-bd/gemini-notebook-mcp-cli](https://github.com/jacob-bd/gemini-notebook-mcp-cli)'s
//! `nlm` CLI or its `notebooklm-mcp` MCP server — a *different* tool
//! than the reference's in-process `notebooklm-py` async client (which
//! Rust can't import). Writing a correct subprocess/MCP adapter needs
//! that external tool's actual command surface confirmed against a
//! real install, which is out of scope for this port to guess at —
//! the same boundary issue #76 (`dbs capture`) and #83 (in-UI browser
//! capture) already drew around the missing Playwright helper (#99).
//!
//! What *is* here, real and tested: the [`NotebookLmClient`] trait
//! [`pipeline`](crate::pipeline) is generic over (mirroring the
//! reference's `client_module` swap-for-testing parameter exactly —
//! same reason: real network/auth stays out of both the pipeline's
//! tests and the eventual adapter's), [`UnimplementedClient`] (the
//! concrete stand-in until that adapter exists — same shape as
//! `dbs_core::service::UnimplementedRunner`), and the auth-state
//! resolution helpers, which are pure path logic with nothing
//! NotebookLM-specific to stub.

use std::path::{Path, PathBuf};

/// A freshly created (or reused) NotebookLM notebook.
#[derive(Debug, Clone)]
pub struct Notebook {
    pub id: Option<String>,
}

/// One video failed to index into NotebookLM — caught per-video by
/// the pipeline (tracked in an `IndexOutcome`, never aborts the run).
/// Deliberately narrower than [`NotebookLmError::Auth`], which is
/// left to propagate and abort the whole run.
#[derive(Debug, Clone)]
pub struct SourceIndexError(pub String);

/// Everything a [`NotebookLmClient`] call can fail with.
#[derive(Debug, Clone)]
pub enum NotebookLmError {
    /// The whole session is unusable (missing/expired auth) — fatal,
    /// aborts the pipeline as [`NotebookLmAuthError`].
    Auth(String),
    /// One video's source failed to add/index — caught per-video by
    /// [`crate::pipeline::run_pipeline`], never fatal on its own.
    SourceIndex(String),
    /// Anything else — fatal, aborts the pipeline as a
    /// [`crate::models::ResearchPipelineError`].
    Other(String),
}

/// Every network call the research pipeline makes into NotebookLM,
/// isolated behind a trait so [`crate::pipeline`] never depends on a
/// concrete transport — a real subprocess/MCP adapter and a test
/// double both just implement this. Mirrors the reference's
/// `client_module`'s `create_notebook`/`add_source`/`ask`/
/// `generate_infographic` surface.
pub trait NotebookLmClient {
    fn create_notebook(&mut self, title: &str) -> Result<Notebook, NotebookLmError>;

    /// Adds one URL source and waits for it to finish indexing.
    fn add_source(&mut self, notebook_id: &str, url: &str) -> Result<(), NotebookLmError>;

    fn ask(&mut self, notebook_id: &str, question: &str) -> Result<String, NotebookLmError>;

    /// Kicks off infographic generation, waits for it, downloads it.
    /// Returns the path it was written to.
    fn generate_infographic(
        &mut self,
        notebook_id: &str,
        output_path: &str,
        orientation: &str,
    ) -> Result<String, NotebookLmError>;
}

/// The documented stand-in until a real adapter exists (see the
/// module doc-comment) — every call fails the same clear way.
pub struct UnimplementedClient;

impl UnimplementedClient {
    fn err<T>() -> Result<T, NotebookLmError> {
        Err(NotebookLmError::Other(
            "NotebookLM synthesis needs the nlm CLI / notebooklm-mcp adapter (gap-analysis.md's \
             Decision 4), which this port doesn't have yet (issue #84's own documented follow-up \
             — its command surface needs confirming against a real install first)."
                .to_string(),
        ))
    }
}

impl NotebookLmClient for UnimplementedClient {
    fn create_notebook(&mut self, _title: &str) -> Result<Notebook, NotebookLmError> {
        Self::err()
    }

    fn add_source(&mut self, _notebook_id: &str, _url: &str) -> Result<(), NotebookLmError> {
        Self::err()
    }

    fn ask(&mut self, _notebook_id: &str, _question: &str) -> Result<String, NotebookLmError> {
        Self::err()
    }

    fn generate_infographic(
        &mut self,
        _notebook_id: &str,
        _output_path: &str,
        _orientation: &str,
    ) -> Result<String, NotebookLmError> {
        Self::err()
    }
}

/// Where the (future) DBS web UI's "NotebookLM login" capture would
/// write the Playwright storageState, relative to the config dir —
/// same subpath the reference's `notebooklm login` produces, so
/// either source of the file works.
pub fn dbs_state_subpath() -> PathBuf {
    Path::new(".notebooklm").join("storage_state.json")
}

/// The DBS-captured storage-state path if it exists, else `None` —
/// `None` means "use the concrete client's own default" (the file
/// `notebooklm login`/`nlm` writes on its own).
pub fn resolve_auth_state(base_dir: &Path) -> Option<PathBuf> {
    let candidate = base_dir.join(dbs_state_subpath());
    candidate.exists().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_auth_state_finds_a_captured_storage_state_file() {
        let dir = std::env::temp_dir().join(format!(
            "dbs-research-notebooklm-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let notebooklm_dir = dir.join(".notebooklm");
        std::fs::create_dir_all(&notebooklm_dir).unwrap();
        std::fs::write(notebooklm_dir.join("storage_state.json"), "{}").unwrap();
        assert_eq!(
            resolve_auth_state(&dir),
            Some(notebooklm_dir.join("storage_state.json"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_auth_state_is_none_when_nothing_was_captured() {
        let dir = std::env::temp_dir().join(format!(
            "dbs-research-notebooklm-test-missing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        assert_eq!(resolve_auth_state(&dir), None);
    }

    #[test]
    fn unimplemented_client_fails_every_call_with_the_same_clear_message() {
        let mut client = UnimplementedClient;
        for msg in [
            client.create_notebook("t").err().map(describe),
            client.add_source("n", "u").err().map(describe),
            client.ask("n", "q").err().map(describe),
            client
                .generate_infographic("n", "p", "landscape")
                .err()
                .map(describe),
        ] {
            let msg = msg.unwrap();
            assert!(msg.contains("nlm CLI"), "{msg}");
        }
    }

    fn describe(e: NotebookLmError) -> String {
        match e {
            NotebookLmError::Auth(m)
            | NotebookLmError::SourceIndex(m)
            | NotebookLmError::Other(m) => m,
        }
    }
}
