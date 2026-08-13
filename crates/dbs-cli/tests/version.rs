//! Integration test for `dbs version` (issue #78).

use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

#[test]
fn version_prints_the_tool_name_crate_version_and_core_api_version() {
    let output = Command::new(dbs_bin()).arg("version").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("rusty_dbs {} (core API v1)", env!("CARGO_PKG_VERSION"))
    );
}
