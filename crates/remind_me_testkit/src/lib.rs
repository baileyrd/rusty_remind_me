//! Shared test scaffolding for this workspace. Test-only: every crate depends
//! on it under `[dev-dependencies]`, so nothing here reaches a shipped build.
//!
//! Today it holds three things, all about *where* a test puts its throwaway
//! files: [`scratch_root`], [`import_export_root`] for the tests that exercise
//! import/export containment, and [`non_repo_scratch_root`] for the tests whose
//! subject walks up the directory tree.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Names an explicit scratch root, overriding every other source. For a CI job
/// that wants tests on a tmpfs or a specific volume, this is the knob — the
/// point of the resolution order below is that it never *guesses* somewhere
/// destructive, not that it refuses to be told where to go.
pub const SCRATCH_ROOT_ENV: &str = "REMIND_ME_TEST_TMPDIR";

/// The directory tests create their scratch files and per-test subdirectories
/// under. Guaranteed to exist by the time this returns.
///
/// Resolved once per process, in order:
///
/// 1. `$REMIND_ME_TEST_TMPDIR`, if set and non-empty.
/// 2. `$TMPDIR` / `$TEMP` / `$TMP`, first one set and non-empty — the platform
///    conventions, honoured so a deliberately-configured environment still
///    wins.
/// 3. `<target dir>/tmp`, where the target dir is `$CARGO_TARGET_DIR` when set
///    and this workspace's `target/` otherwise. Always writable, always
///    gitignored, and cleared by `cargo clean`.
///
/// # Why this exists rather than [`std::env::temp_dir`]
///
/// On Windows `temp_dir()` calls `GetTempPath`, which falls back to
/// `%USERPROFILE%` when both `TMP` and `TEMP` are unset — and then to the
/// Windows directory. A process launched without a full environment (a
/// service, a stripped-down CI shell, an editor-spawned test run) therefore
/// scatters test scratch directories directly into the developer's home
/// folder, where nothing ever cleans them up. That is not hypothetical — it is
/// what left a pile of stray `rrm_*` directories in one home directory before
/// anyone noticed.
///
/// Step 3 is the fix — an unset `TMP`/`TEMP` lands somewhere deliberate
/// instead of somewhere inherited. Reading the variables directly rather than
/// going through `temp_dir()` is what makes that possible: "unset" becomes a
/// case this function handles, not one the OS decides for it.
///
/// # Panics
///
/// If the resolved directory cannot be created. Tests cannot do anything
/// useful without scratch space, and failing loudly here beats every caller
/// failing obscurely later.
pub fn scratch_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = resolve_root();
        std::fs::create_dir_all(&root).unwrap_or_else(|e| {
            panic!(
                "could not create the test scratch root at {}: {e}",
                root.display()
            )
        });
        to_long_path(&root)
    })
    .clone()
}

/// `path` with any Windows 8.3 short components expanded to their long form,
/// and without the `\\?\` verbatim prefix `canonicalize` adds. A no-op
/// everywhere but Windows, and on a path that cannot be canonicalized.
///
/// # Why
///
/// `%TEMP%` is not guaranteed to be in long form. A GitHub Actions Windows
/// runner sets it to `C:\Users\RUNNER~1\AppData\Local\Temp`, because
/// `runneradmin` exceeds the 8.3 limit. Handing that straight back means a
/// test comparing a path it *passed in* against the path an API *reports*
/// fails on a difference that names the same directory:
///
/// ```text
/// left:  C:\Users\runneradmin\AppData\Local\Temp\...\export.json
/// right: C:\Users\RUNNER~1\AppData\Local\Temp\...\export.json
/// ```
///
/// That is a real failure of `export_test`'s
/// `an_export_writes_to_a_file_and_reports_its_size` on CI, and it does not
/// reproduce on a developer machine whose user name happens to fit in eight
/// characters. Before this crate the same tests took their paths from
/// `dirs::home_dir()`, which is always long form, so the problem is new with
/// the move to `%TEMP%` — normalising here is what keeps that move
/// behaviour-preserving.
///
/// The verbatim prefix is stripped rather than kept because a `\\?\` path is
/// not `Path`-equal to the ordinary spelling of the same location — the exact
/// trap `import_paths::roots_from` already documents. Only a plain drive path
/// sheds it unambiguously; anything else (`\\?\UNC\...`) is left alone.
fn to_long_path(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_path_buf();
    }
    let Ok(canonical) = path.canonicalize() else {
        return path.to_path_buf();
    };
    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// The scratch directory for tests that exercise import/export containment,
/// with `REMIND_ME_IMPORT_ROOTS` and `REMIND_ME_EXPORT_ROOTS` pointed at it.
///
/// Containment refuses any import source outside the configured roots, and the
/// default root is the *home directory*. A test that just wants a fixture the
/// importer will accept therefore has two choices: write into the developer's
/// home folder, or configure a root. Every such test used to take the first,
/// which is the larger of the two sources of stray `rrm_*` directories in a
/// home directory — it does not need `TMP`/`TEMP` to be unset to bite.
///
/// # Why one function rather than per-test setup
///
/// `REMIND_ME_IMPORT_ROOTS` is process-global and read on each call, while
/// libtest runs a binary's tests on parallel threads. Funnelling every
/// containment-sensitive test through this one `OnceLock` means the variables
/// are written exactly once per process, before the first test that reads them
/// gets its paths — a test that sets them itself would instead race every
/// sibling test resolving roots at the same moment.
///
/// The contract that makes that hold: **a test touching a roots-sensitive API
/// must obtain its paths from this function**, not from
/// `import_paths::home_dir_var` or a literal. It is fine for such a test to
/// only need an in-roots path it never creates — call this and join onto it.
///
/// Tests asserting the *default* (unset-variable) behaviour deliberately are a
/// different case and must not call this; `wiki_fs_test`'s
/// `the_default_wiki_dir_is_hyphenated` is the one such test today.
pub fn import_export_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        // Per-process, so two test binaries running concurrently under `cargo
        // test` cannot see each other's fixtures inside their own root.
        let root = scratch_root().join(format!("rrm_roots_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap_or_else(|e| {
            panic!(
                "could not create the containment scratch root at {}: {e}",
                root.display()
            )
        });
        std::env::set_var("REMIND_ME_IMPORT_ROOTS", &root);
        std::env::set_var("REMIND_ME_EXPORT_ROOTS", &root);
        root
    })
    .clone()
}

/// A scratch root with no `.git` directory in any ancestor, for tests whose
/// subject walks *up* the directory tree.
///
/// [`scratch_root`]'s `<target dir>/tmp` fallback sits inside this repository,
/// which is exactly right for a test that just needs somewhere to put a file
/// and exactly wrong for one that asks a question about the tree above it.
/// Both ways it bites were observed in `updater`'s tests, and only when
/// `TMP`/`TEMP` were unset — the case that reaches the fallback:
///
/// * `find_repo_root_from` walks upward for a `.git` beside a `Cargo.toml`
///   naming this workspace. Started from `target/tmp/...` it finds this
///   repo's own, so a test asserting "no repository anywhere" got
///   `Some(<workspace>)` instead of `None`.
/// * A scratch crate written under `target/tmp` falls inside this workspace's
///   manifest, so building it fails with "current package believes it's in a
///   workspace when it's not".
///
/// Resolution is [`scratch_root`]'s order with the in-tree fallback replaced:
/// the first of `$REMIND_ME_TEST_TMPDIR`, `$TMPDIR`, `$TEMP`, `$TMP` that has
/// no `.git` above it, and failing all of those a `rrm-test-tmp` directory
/// beside the workspace — outside the repo, and deliberately never the home
/// directory.
///
/// # Panics
///
/// If every candidate lies inside a git repository. Set
/// `REMIND_ME_TEST_TMPDIR` to somewhere outside one; silently handing back an
/// in-repo path would just reproduce the failures above somewhere less
/// obvious.
pub fn non_repo_scratch_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = resolve_non_repo_root(non_empty_var, has_git_ancestor);
        std::fs::create_dir_all(&root).unwrap_or_else(|e| {
            panic!(
                "could not create the out-of-repo scratch root at {}: {e}",
                root.display()
            )
        });
        to_long_path(&root)
    })
    .clone()
}

/// [`scratch_root`]'s resolution order, without the creation step.
fn resolve_root() -> PathBuf {
    resolve_root_from(non_empty_var)
}

/// [`non_repo_scratch_root`]'s resolution, against an arbitrary environment
/// and repository test — both injected for the same reason `resolve_root_from`
/// takes a lookup: the real ones read process-global state the tests must not
/// mutate.
fn resolve_non_repo_root(
    lookup: impl Fn(&str) -> Option<PathBuf>,
    in_repo: impl Fn(&Path) -> bool,
) -> PathBuf {
    let from_env = [SCRATCH_ROOT_ENV, "TMPDIR", "TEMP", "TMP"]
        .into_iter()
        .filter_map(&lookup);
    let candidates = from_env.chain(std::iter::once(workspace_sibling_scratch()));

    for candidate in candidates {
        if !in_repo(&candidate) {
            return candidate;
        }
    }
    panic!(
        "every candidate scratch directory is inside a git repository; set \
         {SCRATCH_ROOT_ENV} to a directory outside one"
    )
}

/// Whether `dir` — which need not exist yet — has a `.git` in it or above it.
fn has_git_ancestor(dir: &Path) -> bool {
    dir.ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

/// A scratch directory beside this workspace rather than inside it.
fn workspace_sibling_scratch() -> PathBuf {
    let workspace = workspace_target_dir();
    let workspace = workspace.parent().unwrap_or(&workspace);
    workspace.parent().unwrap_or(workspace).join("rrm-test-tmp")
}

/// [`resolve_root`] against an arbitrary environment.
///
/// `lookup` is the indirection that makes the ordering testable: reading the
/// process environment directly would mean a test had to *mutate* it, and the
/// test binary runs its tests on parallel threads sharing one environment, so
/// those tests would race each other and anything else reading `TMPDIR`.
fn resolve_root_from(lookup: impl Fn(&str) -> Option<PathBuf>) -> PathBuf {
    for var in [SCRATCH_ROOT_ENV, "TMPDIR", "TEMP", "TMP"] {
        if let Some(dir) = lookup(var) {
            return dir;
        }
    }
    let target = lookup("CARGO_TARGET_DIR").unwrap_or_else(workspace_target_dir);
    target.join("tmp")
}

/// This workspace's `target/` directory.
///
/// `CARGO_MANIFEST_DIR` is this crate's own directory, fixed when *this* crate
/// compiles, so it points at the workspace the same way no matter which
/// crate's tests end up calling in.
///
/// The two parents are popped rather than appended as `..` segments: a path
/// carrying literal `..` is equal to the normalised one as a *location* but
/// not as a `Path`, and tests here compare reported paths against the paths
/// they passed in (`status_test`'s `an_on_disk_database_reports_its_path_and_size`
/// is the one that caught it). Popping is also why this does not
/// `canonicalize` — on Windows that prepends the `\\?\` verbatim prefix, which
/// fails the same comparisons a different way and is the exact trap
/// `import_paths::roots_from` documents.
fn workspace_target_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(std::path::Path::parent)
        .map(|workspace| workspace.join("target"))
        .unwrap_or_else(|| manifest.join("target"))
}

/// An environment variable as a path, treating unset and empty alike — an
/// empty `TMPDIR` is a misconfiguration, and honouring it would resolve
/// scratch space to the process's working directory.
fn non_empty_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `lookup` over a fixed set of variables, standing in for the process
    /// environment.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> {
        let map: HashMap<String, String> = pairs
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name| map.get(name).map(PathBuf::from)
    }

    #[test]
    fn explicit_override_wins_over_every_platform_variable() {
        let root = resolve_root_from(env_of(&[
            (SCRATCH_ROOT_ENV, "/explicit"),
            ("TMPDIR", "/tmpdir"),
            ("TEMP", "/temp"),
            ("TMP", "/tmp"),
        ]));
        assert_eq!(root, PathBuf::from("/explicit"));
    }

    #[test]
    fn platform_variables_are_tried_in_order() {
        let root = resolve_root_from(env_of(&[("TEMP", "/temp"), ("TMP", "/tmp")]));
        assert_eq!(root, PathBuf::from("/temp"));
    }

    /// The bug this crate exists for: with `TMP` and `TEMP` unset, Windows'
    /// `GetTempPath` — and so `std::env::temp_dir` — resolves to
    /// `%USERPROFILE%`. Resolution must reach the target directory instead,
    /// and in particular must not land on anything derived from the home
    /// directory.
    #[test]
    fn an_empty_environment_falls_back_to_the_target_directory_not_home() {
        let root = resolve_root_from(env_of(&[]));
        assert!(
            root.ends_with("target/tmp") || root.ends_with("target\\tmp"),
            "expected a target-directory fallback, got {}",
            root.display()
        );
        assert!(
            !root.components().any(|c| c.as_os_str() == ".."),
            "the fallback must be normalised -- callers compare it against \
             reported paths: {}",
            root.display()
        );
        for home in ["HOME", "USERPROFILE"] {
            if let Some(dir) = non_empty_var(home) {
                assert_ne!(root, dir, "fell back to ${home}");
            }
        }
    }

    /// An empty value is a misconfigured variable, not a request to use the
    /// working directory.
    #[test]
    fn empty_values_are_skipped_like_unset_ones() {
        let root = resolve_root_from(env_of(&[(SCRATCH_ROOT_ENV, ""), ("TMPDIR", "/tmpdir")]));
        assert_eq!(root, PathBuf::from("/tmpdir"));
    }

    #[test]
    fn cargo_target_dir_relocates_the_fallback() {
        let root = resolve_root_from(env_of(&[("CARGO_TARGET_DIR", "/build")]));
        assert_eq!(root, PathBuf::from("/build").join("tmp"));
    }

    #[test]
    fn scratch_root_exists_and_is_a_directory() {
        let root = scratch_root();
        assert!(root.is_dir(), "{} is not a directory", root.display());
    }

    /// The regression this function exists for: the first candidate is inside
    /// a repository, so resolution must skip it rather than hand back a path
    /// whose ancestors contain a `.git`.
    #[test]
    fn an_in_repo_candidate_is_skipped_for_one_outside() {
        let root = resolve_non_repo_root(
            env_of(&[(SCRATCH_ROOT_ENV, "/repo/target/tmp"), ("TEMP", "/outside")]),
            |dir| dir.starts_with("/repo"),
        );
        assert_eq!(root, PathBuf::from("/outside"));
    }

    /// With every variable inside a repository, resolution falls back beside
    /// the workspace -- not into the repo, and not into the home directory.
    #[test]
    fn all_candidates_in_a_repo_falls_back_beside_the_workspace() {
        let root = resolve_non_repo_root(env_of(&[("TEMP", "/repo/tmp")]), |dir| {
            dir.starts_with("/repo")
        });
        assert!(
            root.ends_with("rrm-test-tmp"),
            "expected the workspace-sibling fallback, got {}",
            root.display()
        );
        for home in ["HOME", "USERPROFILE"] {
            if let Some(dir) = non_empty_var(home) {
                assert_ne!(root, dir, "fell back to ${home}");
            }
        }
    }

    /// Nothing qualifies -- including the fallback -- so this must fail loudly
    /// rather than return a path that reproduces the original bug.
    #[test]
    #[should_panic(expected = "inside a git repository")]
    fn no_candidate_outside_a_repo_panics() {
        resolve_non_repo_root(env_of(&[("TEMP", "/repo/tmp")]), |_| true);
    }

    /// The roots handed to tests must already be in long form, so a test
    /// comparing a path it passed in against a path an API reports back does
    /// not fail on an 8.3-vs-long spelling of the same directory. Vacuous on a
    /// machine whose `%TEMP%` is already long; it bites on a CI runner, where
    /// `%TEMP%` contains `RUNNER~1`.
    #[test]
    fn the_scratch_roots_are_already_in_long_form() {
        for root in [
            scratch_root(),
            import_export_root(),
            non_repo_scratch_root(),
        ] {
            assert_eq!(
                root,
                to_long_path(&root),
                "{} is not in long form -- an 8.3 path would break tests that \
                 compare a reported path against the one they passed in",
                root.display()
            );
        }
    }

    #[test]
    fn non_repo_scratch_root_has_no_git_above_it() {
        let root = non_repo_scratch_root();
        assert!(root.is_dir(), "{} is not a directory", root.display());
        assert!(
            !has_git_ancestor(&root),
            "{} is inside a git repository",
            root.display()
        );
    }
}
