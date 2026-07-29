//! Filesystem containment for import and export destinations.
//!
//! # Why the check order is the security property
//!
//! Containment is tested **before** anything touches the filesystem. A check
//! that tested existence first would answer "does this path exist?" for any
//! path on the machine — the tool becomes a filesystem oracle, reporting "file
//! not found" for `/etc/shadow` and "not in roots" for `/etc/passwd`, which is
//! an information leak even though neither is readable.
//!
//! The reference is explicit about this for imports (`SE-02`) and mirrors it
//! for exports. Both sides live here so there is one implementation: two copies
//! of a containment check drift, and the drift is a vulnerability rather than a
//! bug.
//!
//! Paths are resolved before the test, so `..` segments and symlinks cannot
//! step outside a root.

use std::path::{Path, PathBuf};

/// Colon-separated roots an import may read from.
pub const IMPORT_ROOTS_ENV: &str = "REMIND_ME_IMPORT_ROOTS";
/// Colon-separated roots an export may write to.
pub const EXPORT_ROOTS_ENV: &str = "REMIND_ME_EXPORT_ROOTS";

/// Extensions the importer accepts.
pub const SUPPORTED_SUFFIXES: [&str; 5] = ["json", "jsonl", "md", "markdown", "txt"];
/// Extensions a *document* import accepts — a document is prose, not a chat log.
pub const DOCUMENT_SUFFIXES: [&str; 3] = ["md", "markdown", "txt"];

fn roots_from(env: &str) -> Vec<PathBuf> {
    match std::env::var(env) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .split(':')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| PathBuf::from(expand_home(r)))
            .collect(),
        // Default to the home directory, matching the reference.
        _ => std::env::var("HOME")
            .map(|home| vec![PathBuf::from(home)])
            .unwrap_or_default(),
    }
}

/// Roots an import may read from.
pub fn import_roots() -> Vec<PathBuf> {
    roots_from(IMPORT_ROOTS_ENV)
}

/// Roots an export may write to.
pub fn export_roots() -> Vec<PathBuf> {
    roots_from(EXPORT_ROOTS_ENV)
}

pub fn expand_home(raw: &str) -> String {
    match (raw.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{}/{}", home.trim_end_matches('/'), rest),
        _ => raw.to_string(),
    }
}

/// Normalise a path without requiring it to exist.
///
/// Resolves the longest existing prefix through the filesystem — so symlinks
/// are followed — then reattaches the rest with `.` and `..` folded away.
/// `canonicalize` alone fails on a path that does not exist yet, which an
/// export destination usually does not.
pub fn resolve_lexically(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }

    let mut existing = out.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            None => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    resolved
}

/// Whether a resolved path sits inside one of `roots`.
pub fn is_contained(resolved: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| resolved == root || resolved.starts_with(root))
}

/// Why an import source was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportPathError {
    OutsideRoots(PathBuf),
    NotFound(PathBuf),
    NotAFile(PathBuf),
    NotADirectory(PathBuf),
    UnsupportedSuffix(String),
}

impl std::fmt::Display for ImportPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoots(p) => {
                write!(f, "Path not in allowed import roots: {}", p.display())
            }
            Self::NotFound(p) => write!(f, "File not found: {}", p.display()),
            Self::NotAFile(p) => write!(f, "Not a file: {}", p.display()),
            Self::NotADirectory(p) => write!(f, "Directory not found: {}", p.display()),
            Self::UnsupportedSuffix(s) => write!(
                f,
                "Unsupported file type: .{}. Use .json, .jsonl, .md, .markdown or .txt",
                s
            ),
        }
    }
}

impl std::error::Error for ImportPathError {}

/// Resolve and validate a file to import.
///
/// Containment first, then existence, then the extension — see the module
/// docs for why that order is not negotiable.
pub fn validate_import_file(raw: &str) -> Result<PathBuf, ImportPathError> {
    let resolved = resolve_lexically(&PathBuf::from(expand_home(raw.trim())));

    if !is_contained(&resolved, &import_roots()) {
        return Err(ImportPathError::OutsideRoots(resolved));
    }
    if !resolved.exists() {
        return Err(ImportPathError::NotFound(resolved));
    }
    if !resolved.is_file() {
        return Err(ImportPathError::NotAFile(resolved));
    }
    let suffix = suffix_of(&resolved);
    if !SUPPORTED_SUFFIXES.contains(&suffix.as_str()) {
        return Err(ImportPathError::UnsupportedSuffix(suffix));
    }
    Ok(resolved)
}

/// Resolve and validate a directory to import from.
pub fn validate_import_dir(raw: &str) -> Result<PathBuf, ImportPathError> {
    let resolved = resolve_lexically(&PathBuf::from(expand_home(raw.trim())));

    if !is_contained(&resolved, &import_roots()) {
        return Err(ImportPathError::OutsideRoots(resolved));
    }
    if !resolved.is_dir() {
        return Err(ImportPathError::NotADirectory(resolved));
    }
    Ok(resolved)
}

/// Lowercased extension without the dot, or the empty string.
pub fn suffix_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
}
