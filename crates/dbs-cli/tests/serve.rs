//! Integration tests for `dbs serve` flag wiring (issue #75).
//!
//! The actual web server is out of scope for this issue (see
//! gap-analysis.md's Web tier rows) — these tests cover the flag
//! parsing and security-relevant validation that *is* in scope:
//! default host/port, the off-localhost-without-token refusal, and
//! that a valid invocation reports plainly rather than pretending to
//! serve.

use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

#[test]
fn default_host_and_port_are_accepted_without_a_token() {
    let output = Command::new(dbs_bin()).arg("serve").output().unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("http://127.0.0.1:8000"), "{stderr}");
    assert!(!stderr.contains("Refusing to bind"), "{stderr}");
}

#[test]
fn a_custom_port_is_reflected_in_the_reported_address() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--port")
        .arg("9000")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("http://127.0.0.1:9000"), "{stderr}");
}

#[test]
fn binding_off_localhost_without_a_token_is_refused() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--host")
        .arg("0.0.0.0")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Refusing to bind"), "{stderr}");
    assert!(stderr.contains("--token"), "{stderr}");
}

#[test]
fn binding_off_localhost_with_a_token_is_accepted() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--host")
        .arg("0.0.0.0")
        .arg("--token")
        .arg("secret")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("Refusing to bind"), "{stderr}");
    assert!(stderr.contains("token auth would be required"), "{stderr}");
}

#[test]
fn localhost_by_name_is_accepted_without_a_token() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--host")
        .arg("localhost")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("Refusing to bind"), "{stderr}");
}

#[test]
fn schedule_flag_is_reflected_in_the_report() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--schedule")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("scheduler"), "{stderr}");
}

#[test]
fn no_setup_flag_is_reflected_in_the_report() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--no-setup")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("setup actions would be disabled"),
        "{stderr}"
    );
}

#[test]
fn allow_setup_and_no_setup_together_is_a_usage_error() {
    let output = Command::new(dbs_bin())
        .arg("serve")
        .arg("--allow-setup")
        .arg("--no-setup")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
}
