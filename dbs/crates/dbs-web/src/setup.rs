//! In-UI setup actions: dependency installation and browser-auth
//! capture, run as background jobs (issue #83) — mirrors the
//! reference's `dbs.web.setup`.
//!
//! **Reuses [`crate::jobs::JobManager`] rather than porting a second
//! job manager.** The reference's `setup.py` defines its own
//! `SetupManager`/`SetupJob` — structurally identical to
//! `dbs.web.jobs.JobManager`/`BackupJob` (same lock/queue/SSE-stream
//! shape, just log lines instead of typed progress events) because
//! Python couldn't easily share one manager across two different
//! per-job payload shapes without more machinery than duplicating it.
//! `JobManager` here already carries an untyped `serde_json::Value` per
//! event, so a setup job's log line is just `json!({"line": line})` —
//! no second manager needed.
//!
//! **Two job kinds, two different levels of "real":**
//!
//! - [`run_install_job`] is fully real: it derives `pip
//!   install`/`playwright install chromium` steps from a connector's
//!   declared [`dbs_core::Handshake::pip_requirements`]/
//!   [`dbs_core::Handshake::needs_playwright_browser`] (or the fixed
//!   [`playwright_install_commands`] steps directly) and runs them for
//!   real via [`run_commands`], streaming each line to the job.
//! - [`run_capture_job`] is a documented stub: driving a real
//!   Playwright browser *login* session needs a dedicated capture
//!   script this port hasn't written yet — unlike the generic
//!   Playwright-subprocess launcher itself (#99's
//!   `dbs_connector_support::python_launch`), which is done and has
//!   real callers (`dbs-connector-reddit`/`-skool`'s own
//!   `scripts/acquire.py`, #187/#188). It fails cleanly with that
//!   explanation rather than pretending to open a browser.
//!
//! The pure validation/formatting helpers below
//! ([`validate_netscape_cookies`], [`validate_storage_state`],
//! [`to_netscape_cookies`], [`extract_session_zip`]) have no such
//! gap — they're plain data transforms the reference itself calls out
//! as "pure — unit-tested," ported directly.
//!
//! Every function in this module is wired into a real `/api` route
//! (`dbs-web::api`, #175/#177): `install_commands`/`run_install_job`
//! back `/api/connectors/:type/install`, `run_capture_job` backs
//! `/api/connectors/:type/capture` and `/api/sources/:name/capture`,
//! `extract_session_zip`/`validate_netscape_cookies`/
//! `validate_storage_state` back the `/api/*/import` routes, and
//! `research_install_commands`/`run_notebooklm_login_job` back
//! `/api/research/install`/`/api/research/login`.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use dbs_core::RegisteredConnector;
use serde_json::json;

use crate::jobs::Job;

/// Finds a Python interpreter on `PATH` — this binary has no
/// `sys.executable` of its own. Mirrors `dbs-cli`'s identical helper
/// for `dbs update-ytdlp` (issue #73); duplicated rather than shared
/// since the two crates don't otherwise depend on each other in that
/// direction and it's a few lines.
fn find_python() -> Option<&'static str> {
    ["python3", "python"]
        .into_iter()
        .find(|candidate| Command::new(candidate).arg("--version").output().is_ok())
}

/// Whether the Playwright Python package is importable (capture needs
/// it). `false` if no Python interpreter is found at all.
pub fn playwright_present() -> bool {
    let Some(python) = find_python() else {
        return false;
    };
    Command::new(python)
        .args(["-c", "import playwright"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Fixed steps to make browser capture possible, as `(label, argv)`
/// pairs ready for [`run_commands`]. `None` if no Python interpreter
/// is on `PATH` at all — there's nothing to run `pip`/`playwright`
/// through.
pub fn playwright_install_commands() -> Option<Vec<(String, Vec<String>)>> {
    let python = find_python()?;
    Some(vec![
        (
            "pip install playwright".to_string(),
            vec![
                python.to_string(),
                "-m".to_string(),
                "pip".to_string(),
                "install".to_string(),
                "playwright".to_string(),
            ],
        ),
        (
            "playwright install chromium".to_string(),
            vec![
                python.to_string(),
                "-m".to_string(),
                "playwright".to_string(),
                "install".to_string(),
                "chromium".to_string(),
            ],
        ),
    ])
}

/// `pip install yt-dlp` — the one real, already-installable dependency
/// the research pipeline has (`dbs_research::youtube_search`'s
/// yt-dlp-subprocess search). `None` if no Python interpreter is on
/// `PATH` to run `pip` through.
pub fn research_install_commands() -> Option<Vec<(String, Vec<String>)>> {
    let python = find_python()?;
    Some(vec![(
        "pip install yt-dlp".to_string(),
        vec![
            python.to_string(),
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            "yt-dlp".to_string(),
        ],
    )])
}

/// A [`crate::jobs::JobManager::start`]-compatible NotebookLM login
/// capture job body. Always fails cleanly — same reason and shape as
/// [`run_capture_job`]: a real browser *login* session needs a
/// dedicated capture script this port hasn't written yet (the generic
/// Playwright-subprocess launcher itself, #99, is done).
pub fn run_notebooklm_login_job(job: &Arc<Job>) -> Result<(), String> {
    job.emit(json!({
        "line": "Opening a browser window on the server host for NotebookLM login."
    }));
    Err(
        "browser login capture needs a dedicated Playwright script this port hasn't written \
         yet — run `notebooklm login` on a desktop build of the reference instead, then copy \
         its storage_state.json into <config dir>/.notebooklm/storage_state.json."
            .to_string(),
    )
}

/// The `(label, argv)` steps to make `rc` runnable, derived entirely
/// from its declared handshake metadata. Empty when it declares
/// nothing to install. `None` if no Python interpreter is on `PATH`
/// but `rc` does declare something that would need one.
pub fn install_commands(rc: &RegisteredConnector) -> Option<Vec<(String, Vec<String>)>> {
    let reqs = &rc.handshake.pip_requirements;
    let needs_playwright = rc.handshake.needs_playwright_browser;
    if reqs.is_empty() && !needs_playwright {
        return Some(Vec::new());
    }
    let python = find_python()?;
    let mut steps = Vec::new();
    if !reqs.is_empty() {
        let mut argv = vec![
            python.to_string(),
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
        ];
        argv.extend(reqs.iter().cloned());
        steps.push((format!("pip install {}", reqs.join(" ")), argv));
    }
    if needs_playwright {
        steps.push((
            "playwright install chromium".to_string(),
            vec![
                python.to_string(),
                "-m".to_string(),
                "playwright".to_string(),
                "install".to_string(),
                "chromium".to_string(),
            ],
        ));
    }
    Some(steps)
}

/// Runs each `(label, argv)` step in turn, streaming its merged
/// stdout/stderr to `emit` line by line — mirrors the reference's
/// `run_commands`. Stops and returns `Err` on the first non-zero exit
/// (or a command that fails to launch at all); every step before that
/// has already run for real.
pub fn run_commands(
    commands: &[(String, Vec<String>)],
    mut emit: impl FnMut(String),
) -> Result<(), String> {
    for (label, argv) in commands {
        emit(format!("$ {label}"));
        let [program, args @ ..] = argv.as_slice() else {
            return Err(format!("`{label}` has an empty argv"));
        };
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| format!("`{label}` failed to launch: {e}"))?;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            emit(line.to_string());
        }
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            emit(line.to_string());
        }
        if !output.status.success() {
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            return Err(format!("`{label}` failed (exit {code})"));
        }
        emit(format!("[ok] {label}"));
    }
    emit("Done.".to_string());
    Ok(())
}

/// A [`crate::jobs::JobManager::start`]-compatible dependency-install
/// job body: runs `commands` via [`run_commands`], emitting each line
/// as `{"line": ...}` on `job`.
pub fn run_install_job(job: &Arc<Job>, commands: &[(String, Vec<String>)]) -> Result<(), String> {
    run_commands(commands, |line| job.emit(json!({ "line": line })))
}

/// A [`crate::jobs::JobManager::start`]-compatible browser-auth
/// capture job body. Always fails cleanly — see the module doc-comment
/// on why real browser login capture isn't implemented in this port
/// yet (a dedicated capture script, not the generic Playwright
/// launcher itself, which is done — #99).
pub fn run_capture_job(job: &Arc<Job>, target: &str) -> Result<(), String> {
    job.emit(json!({
        "line": format!("Opening a browser window on the server host for {target:?}.")
    }));
    Err(
        "browser login capture needs a dedicated Playwright script this port hasn't written \
         yet — capture on a desktop build of the reference and import the resulting session \
         file instead."
            .to_string(),
    )
}

/// Raises unless `text` looks like a Netscape `cookies.txt`: the
/// conventional `# Netscape HTTP Cookie File` header, or at least one
/// well-formed tab-delimited 7-field cookie row (blank/`#` lines
/// skipped). Rejects an obviously-wrong upload before it's written
/// into place.
pub fn validate_netscape_cookies(text: &str) -> Result<(), String> {
    let has_valid_row = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .any(|l| l.split('\t').count() == 7);
    if has_valid_row || text.contains("Netscape HTTP Cookie File") {
        return Ok(());
    }
    Err("doesn't look like a Netscape cookies.txt (expected tab-delimited rows)".to_string())
}

/// Parses `text` as a Playwright `storageState` JSON (an object with
/// `cookies` and `origins` keys), or an error describing why not.
pub fn validate_storage_state(text: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = value.as_object();
    let looks_right = obj.is_some_and(|o| o.contains_key("cookies") && o.contains_key("origins"));
    if !looks_right {
        return Err(
            "doesn't look like a Playwright storageState (expected 'cookies' and 'origins' \
             keys)"
                .to_string(),
        );
    }
    Ok(value)
}

/// Formats Playwright cookies (as `serde_json::Value` objects, the
/// shape a `storageState`'s `cookies` array holds) as a Netscape
/// `cookies.txt` — the format yt-dlp reads.
pub fn to_netscape_cookies(cookies: &[serde_json::Value]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n# Generated by rusty_dbs\n\n");
    for c in cookies {
        let domain = c.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if domain.is_empty() || name.is_empty() {
            continue;
        }
        let include_sub = if domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let path = c.get("path").and_then(|v| v.as_str()).unwrap_or("/");
        let secure = if c.get("secure").and_then(|v| v.as_bool()).unwrap_or(false) {
            "TRUE"
        } else {
            "FALSE"
        };
        let expires = c.get("expires").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let expiry = if expires > 0.0 {
            (expires as i64).to_string()
        } else {
            "0".to_string()
        };
        let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let http_only = c.get("httpOnly").and_then(|v| v.as_bool()).unwrap_or(false);
        let domain_field = if http_only {
            format!("#HttpOnly_{domain}")
        } else {
            domain.to_string()
        };
        out.push_str(
            &[
                domain_field.as_str(),
                include_sub,
                path,
                secure,
                &expiry,
                name,
                value,
            ]
            .join("\t"),
        );
        out.push('\n');
    }
    out
}

/// Extracts a zipped Playwright profile directory into `target_dir`,
/// guarding against zip-slip: every entry must resolve to a path
/// *inside* `target_dir`, or the whole upload is rejected before
/// anything is written.
pub fn extract_session_zip(data: &[u8], target_dir: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| format!("not a valid zip file: {e}"))?;

    std::fs::create_dir_all(target_dir)
        .map_err(|e| format!("could not create {}: {e}", target_dir.display()))?;
    let base = target_dir
        .canonicalize()
        .map_err(|e| format!("could not resolve {}: {e}", target_dir.display()))?;

    for i in 0..archive.len() {
        let entry_name = archive
            .by_index(i)
            .map_err(|e| format!("could not read zip entry {i}: {e}"))?
            .name()
            .to_string();
        let joined = target_dir.join(&entry_name);
        // The entry needn't exist yet to check containment: walk its
        // components against `base` directly instead of canonicalizing
        // (which requires the path to already exist).
        let mut resolved = base.clone();
        for component in joined
            .strip_prefix(target_dir)
            .unwrap_or(&joined)
            .components()
        {
            match component {
                std::path::Component::ParentDir => {
                    if !resolved.pop() {
                        return Err(format!(
                            "zip entry escapes target directory: {entry_name:?}"
                        ));
                    }
                }
                std::path::Component::Normal(part) => resolved.push(part),
                _ => {}
            }
        }
        if resolved != base && !resolved.starts_with(&base) {
            return Err(format!(
                "zip entry escapes target directory: {entry_name:?}"
            ));
        }
    }

    archive
        .extract(target_dir)
        .map_err(|e| format!("failed to extract zip: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-setup-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "{body}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn run_commands_streams_output_and_reports_ok_on_success() {
        let dir = temp_dir("run-ok");
        let script = write_script(&dir, "ok.sh", "echo hello\necho world");
        let mut lines = Vec::new();
        let result = run_commands(
            &[(
                "say hi".to_string(),
                vec![script.to_string_lossy().to_string()],
            )],
            |line| lines.push(line),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            lines,
            vec!["$ say hi", "hello", "world", "[ok] say hi", "Done."]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_stops_at_the_first_failing_step_with_a_clear_error() {
        let dir = temp_dir("run-fail");
        let bad = write_script(&dir, "bad.sh", "echo trying\nexit 7");
        let never = write_script(&dir, "never.sh", "echo should-not-run");
        let mut lines = Vec::new();
        let result = run_commands(
            &[
                (
                    "bad step".to_string(),
                    vec![bad.to_string_lossy().to_string()],
                ),
                (
                    "never step".to_string(),
                    vec![never.to_string_lossy().to_string()],
                ),
            ],
            |line| lines.push(line),
        );
        let err = result.unwrap_err();
        assert!(err.contains("bad step") && err.contains("exit 7"), "{err}");
        assert!(lines.contains(&"trying".to_string()));
        assert!(!lines.iter().any(|l| l.contains("should-not-run")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_reports_a_command_that_fails_to_launch() {
        let result = run_commands(
            &[(
                "nonexistent".to_string(),
                vec!["/no/such/binary-xyz".to_string()],
            )],
            |_| {},
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("nonexistent") && err.contains("failed to launch"),
            "{err}"
        );
    }

    #[test]
    fn install_commands_is_empty_when_the_connector_declares_nothing() {
        let rc = fake_rc(&[], false);
        assert_eq!(install_commands(&rc), Some(Vec::new()));
    }

    #[test]
    fn install_commands_derives_pip_and_playwright_steps() {
        let rc = fake_rc(&["foo".to_string(), "bar".to_string()], true);
        let steps = install_commands(&rc).unwrap();
        assert_eq!(steps.len(), 2);
        assert!(steps[0].0.contains("pip install foo bar"));
        assert!(steps[1].0.contains("playwright install chromium"));
    }

    fn fake_rc(pip_requirements: &[String], needs_playwright_browser: bool) -> RegisteredConnector {
        use dbs_core::{Capabilities, Handshake};
        RegisteredConnector {
            type_: "fake".to_string(),
            plugin_id: "fake:fake".to_string(),
            dist_name: "fake".to_string(),
            is_builtin: true,
            command: std::path::PathBuf::from("/bin/true"),
            args: Vec::new(),
            handshake: Handshake {
                type_: "fake".to_string(),
                core_api_version: 1,
                schema_version: 1,
                capabilities: Capabilities::default(),
                secret_keys: vec![],
                item_kinds: vec!["note".to_string()],
                display_name: None,
                description: None,
                export_profile: None,
                auth_capture: None,
                volatile_fields: Vec::new(),
                pip_requirements: pip_requirements.to_vec(),
                needs_playwright_browser,
            },
        }
    }

    #[tokio::test]
    async fn run_install_job_emits_lines_and_succeeds_through_a_real_job() {
        let dir = temp_dir("install-job");
        let script = write_script(&dir, "ok.sh", "echo installed");
        let manager = crate::jobs::JobManager::new();
        let commands = vec![(
            "install".to_string(),
            vec![script.to_string_lossy().to_string()],
        )];
        let job = manager
            .start(json!({"kind": "install"}), move |job| {
                run_install_job(&job, &commands)
            })
            .ok()
            .unwrap();
        for _ in 0..200 {
            if job.status() != crate::jobs::JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(job.status(), crate::jobs::JobStatus::Done);
        let snap = job.snapshot();
        assert!(
            snap.events.contains(&json!({"line": "installed"})),
            "{:?}",
            snap.events
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_capture_job_fails_clearly_through_a_real_job() {
        let manager = crate::jobs::JobManager::new();
        let job = manager
            .start(json!({"kind": "capture"}), |job| {
                run_capture_job(&job, "reddit")
            })
            .ok()
            .unwrap();
        for _ in 0..200 {
            if job.status() != crate::jobs::JobStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(job.status(), crate::jobs::JobStatus::Error);
        let error = job.snapshot().error.unwrap();
        assert!(error.contains("dedicated Playwright script"), "{error}");
    }

    #[test]
    fn validate_netscape_cookies_accepts_the_conventional_header() {
        assert!(validate_netscape_cookies("# Netscape HTTP Cookie File\n").is_ok());
    }

    #[test]
    fn validate_netscape_cookies_accepts_a_seven_field_row() {
        let row = ".example.com\tTRUE\t/\tFALSE\t0\tname\tvalue";
        assert!(validate_netscape_cookies(row).is_ok());
    }

    #[test]
    fn validate_netscape_cookies_rejects_garbage() {
        assert!(validate_netscape_cookies("not cookies at all").is_err());
    }

    #[test]
    fn validate_storage_state_accepts_cookies_and_origins() {
        let text = r#"{"cookies": [], "origins": []}"#;
        assert!(validate_storage_state(text).is_ok());
    }

    #[test]
    fn validate_storage_state_rejects_invalid_json() {
        let err = validate_storage_state("not json").unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn validate_storage_state_rejects_json_missing_the_expected_keys() {
        let err = validate_storage_state("{}").unwrap_err();
        assert!(err.contains("storageState"), "{err}");
    }

    #[test]
    fn to_netscape_cookies_formats_a_cookie_as_a_tab_delimited_row() {
        let cookies = vec![json!({
            "domain": ".example.com",
            "name": "sid",
            "value": "abc123",
            "path": "/",
            "secure": true,
            "httpOnly": true,
            "expires": 1700000000,
        })];
        let text = to_netscape_cookies(&cookies);
        assert!(
            text.contains("#HttpOnly_.example.com\tTRUE\t/\tTRUE\t1700000000\tsid\tabc123"),
            "{text}"
        );
    }

    #[test]
    fn to_netscape_cookies_skips_a_cookie_with_no_domain_or_name() {
        let cookies = vec![json!({"name": "sid", "value": "x"})];
        let text = to_netscape_cookies(&cookies);
        assert_eq!(
            text,
            "# Netscape HTTP Cookie File\n# Generated by rusty_dbs\n\n"
        );
    }

    fn write_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for (name, content) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extract_session_zip_writes_every_entry_under_the_target_dir() {
        let dir = temp_dir("extract-ok");
        let target = dir.join("profile");
        let data = write_zip(&[("Cookies", b"data"), ("sub/dir/file.txt", b"nested")]);
        extract_session_zip(&data, &target).unwrap();
        assert_eq!(std::fs::read(target.join("Cookies")).unwrap(), b"data");
        assert_eq!(
            std::fs::read(target.join("sub/dir/file.txt")).unwrap(),
            b"nested"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_session_zip_rejects_a_zip_slip_entry() {
        let dir = temp_dir("extract-slip");
        let target = dir.join("profile");
        let data = write_zip(&[("../../etc/passwd", b"pwned")]);
        let err = extract_session_zip(&data, &target).unwrap_err();
        assert!(err.contains("escapes target directory"), "{err}");
        assert!(!dir.join("../etc/passwd").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_session_zip_rejects_a_non_zip_payload() {
        let dir = temp_dir("extract-bad");
        let target = dir.join("profile");
        let err = extract_session_zip(b"not a zip", &target).unwrap_err();
        assert!(err.contains("not a valid zip file"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
