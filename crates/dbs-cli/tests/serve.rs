//! Integration tests for `dbs serve` (issues #75 flag wiring, #79 web
//! app skeleton).
//!
//! A loopback bind now actually serves the app skeleton (#79) — those
//! tests spawn the real binary, poll the port until it answers, check
//! a real HTTP response, then kill the child. A non-loopback bind isn't
//! wired to serve for real yet (no auth gate — #81), so those cases
//! still quick-exit with a validation report, same as the off-
//! localhost-without-token refusal and the `--allow-setup --no-setup`
//! usage error.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

/// Kills the wrapped child on drop, so a failing assertion never leaks
/// a `dbs serve` process past the test that started it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A quick GET over a raw `TcpStream` — no HTTP client dependency
/// needed just to check "is this the app skeleton answering".
fn get(port: u16, path: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream
                    .write_all(
                        format!(
                            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                let mut response = Vec::new();
                stream.read_to_end(&mut response).unwrap();
                return String::from_utf8_lossy(&response).into_owned();
            }
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("dbs serve never accepted a connection on :{port}: {e}"),
        }
    }
}

#[test]
fn default_host_and_port_serve_the_app_skeleton() {
    let child = Command::new(dbs_bin())
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = KillOnDrop(child);

    let response = get(8000, "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("<!DOCTYPE html"), "{response}");
}

#[test]
fn a_custom_port_is_actually_bound() {
    let child = Command::new(dbs_bin())
        .arg("serve")
        .arg("--port")
        .arg("18123")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = KillOnDrop(child);

    let response = get(18123, "/static/app.js");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("text/javascript"), "{response}");
}

#[test]
fn localhost_by_name_is_actually_bound() {
    let child = Command::new(dbs_bin())
        .arg("serve")
        .arg("--host")
        .arg("localhost")
        .arg("--port")
        .arg("18124")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = KillOnDrop(child);

    let response = get(18124, "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

/// Reads lines from `child`'s stderr (must be `Stdio::piped()`) on a
/// background thread until one contains `pattern` or `timeout` elapses.
fn wait_for_stderr_line(child: &mut Child, pattern: &str, timeout: Duration) -> bool {
    let mut stderr = child.stderr.take().expect("stderr not piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(&mut stderr);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains(pattern) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

#[test]
fn schedule_flag_is_reflected_in_the_report() {
    let mut child = Command::new(dbs_bin())
        .arg("serve")
        .arg("--port")
        .arg("18125")
        .arg("--schedule")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_for_stderr_line(
        &mut child,
        "scheduler",
        Duration::from_secs(5)
    ));
    let _guard = KillOnDrop(child);
}

#[test]
fn no_setup_flag_is_reflected_in_the_report() {
    let mut child = Command::new(dbs_bin())
        .arg("serve")
        .arg("--port")
        .arg("18126")
        .arg("--no-setup")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_for_stderr_line(
        &mut child,
        "--no-setup noted",
        Duration::from_secs(5)
    ));
    let _guard = KillOnDrop(child);
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
fn binding_off_localhost_with_a_token_is_validated_but_not_yet_served_for_real() {
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
    assert!(stderr.contains("not yet served for real"), "{stderr}");
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
