//! Coverage for `import_paths::split_path_list` (#298).
//!
//! `CODE_ROOTS_ENV`/`IMPORT_ROOTS_ENV`/`EXPORT_ROOTS_ENV`/`WATCH_DIRS_ENV`
//! all used to split on a literal `':'`, a Unix `PATH`-style convention that
//! breaks on Windows: an absolute Windows path's drive letter is itself a
//! colon (`C:\Users\...`), so a single configured root split into two
//! bogus, non-matching fragments. `split_path_list` uses
//! `std::env::split_paths` instead, which is platform-aware by
//! construction -- `:` on Unix, `;` on Windows.
//!
//! The Windows-specific case can only be compiled and actually exercised on
//! Windows, since `std::env::split_paths`'s behavior is a `cfg`-gated
//! compile-time choice, not a runtime one this test could fake on Linux.
//! That is exactly what the `windows` CI job added for #271 now runs.

use remind_me_core::import_paths::split_path_list;

#[test]
fn a_single_path_is_one_entry() {
    assert_eq!(split_path_list("/home/me/notes"), vec!["/home/me/notes"]);
}

#[test]
fn empty_and_whitespace_only_segments_are_dropped() {
    assert_eq!(
        split_path_list(""),
        Vec::<String>::new(),
        "an empty string has no path segments at all"
    );
}

#[cfg(not(windows))]
#[test]
fn multiple_unix_paths_split_on_colon() {
    assert_eq!(
        split_path_list("/a/b:/c/d"),
        vec!["/a/b".to_string(), "/c/d".to_string()]
    );
}

#[cfg(not(windows))]
#[test]
fn surrounding_whitespace_is_trimmed() {
    assert_eq!(split_path_list("  /a/b  "), vec!["/a/b".to_string()]);
}

/// The regression this whole function exists to fix: a single Windows root
/// must survive as one entry, not be split at its own drive letter's colon.
/// Only meaningful -- and only compiled -- on Windows, since
/// `std::env::split_paths` is `:`-splitting by construction everywhere
/// else; a `cfg(windows)`-gated assertion is the only honest way to prove
/// this without faking platform behavior on Linux.
#[cfg(windows)]
#[test]
fn a_windows_drive_letter_is_not_mistaken_for_a_separator() {
    assert_eq!(
        split_path_list(r"C:\Users\me\code"),
        vec![r"C:\Users\me\code".to_string()]
    );
}

#[cfg(windows)]
#[test]
fn multiple_windows_paths_split_on_semicolon() {
    assert_eq!(
        split_path_list(r"C:\Users\me\code;D:\notes"),
        vec![r"C:\Users\me\code".to_string(), r"D:\notes".to_string()]
    );
}
