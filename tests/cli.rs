//! End-to-end tests: drive the compiled `flighty-wall` binary through its CLI.
//!
//! Everything that needs Google or the wall stops at "configuration error" here; those
//! paths are covered by the unit tests behind fakes. What this file pins is the shell
//! contract: flags, exit codes, stderr wording, and the files a command leaves behind.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::TempDir;

/// A temporary home with a valid config, a 0600 Google key, and a 0700 state directory.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.path().join("state"))
            .unwrap();
        let google = root.path().join("google.json");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/google_auth/service-account.json"),
            &google,
        )
        .unwrap();
        chmod(&google, 0o600);
        let sandbox = Self { root };
        sandbox.write_config("");
        sandbox
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn config(&self) -> PathBuf {
        self.path("config.toml")
    }

    fn write_config(&self, extra: &str) {
        let body = format!(
            r#"
[google]
calendar_id = "friends@example.invalid"
credentials_path = "{google}"

[service]
poll_interval_seconds = 120
lookahead_days = 7

[storage]
state_path = "{state}"
{extra}
"#,
            google = self.path("google.json").display(),
            state = self.path("state/state.sqlite3").display(),
        );
        fs::write(self.config(), body.trim_start()).unwrap();
    }

    fn with_flightwall(&self) {
        let creds = self.path("flightwall.toml");
        fs::write(&creds, "api_key = \"k\"\nuser_id = \"u\"\n").unwrap();
        chmod(&creds, 0o600);
        self.write_config(&format!(
            "\n[flightwall]\ncredentials_path = \"{}\"\nhost = \"127.0.0.1\"\ntimeout_seconds = 0.2\n",
            creds.display()
        ));
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn flighty_wall() -> Command {
    Command::cargo_bin("flighty-wall").unwrap()
}

// ---------------------------------------------------------------------------------------
// Configuration errors are exit 2 with a clear line, for every command.
// ---------------------------------------------------------------------------------------

#[test]
fn every_command_reports_a_missing_config_file_as_exit_2() {
    for args in [
        vec!["sync", "--config", "/nonexistent/config.toml"],
        vec!["probe-wall", "--config", "/nonexistent/config.toml"],
        vec!["run", "--config", "/nonexistent/config.toml"],
        vec![
            "inspect-calendar",
            "--config",
            "/nonexistent/config.toml",
            "--output",
            "/tmp/out.json",
        ],
    ] {
        flighty_wall()
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("configuration error"))
            .stderr(predicate::str::contains("does not exist"));
    }
}

#[test]
fn a_config_error_names_the_field_not_the_value() {
    let sandbox = Sandbox::new();
    let body = fs::read_to_string(sandbox.config())
        .unwrap()
        .replace("poll_interval_seconds = 120", "poll_interval_seconds = 1");
    fs::write(sandbox.config(), body).unwrap();

    flighty_wall()
        .args(["sync", "--config"])
        .arg(sandbox.config())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("service.poll_interval_seconds"))
        .stderr(predicate::str::contains("between 30 and 86400"));
}

#[test]
fn wall_commands_require_the_flightwall_table() {
    let sandbox = Sandbox::new();
    for command in ["probe-wall", "sync"] {
        flighty_wall()
            .args([command, "--config"])
            .arg(sandbox.config())
            .assert()
            .code(2)
            .stderr(predicate::str::contains("no [flightwall] table"));
    }
}

#[test]
fn sync_refuses_to_run_while_another_process_holds_the_lock() {
    let sandbox = Sandbox::new();
    sandbox.with_flightwall();
    let lock_path = sandbox.path("state/state.sqlite3.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();

    flighty_wall()
        .args(["sync", "--config"])
        .arg(sandbox.config())
        .assert()
        .code(5)
        .stderr(predicate::str::contains("busy:"))
        .stderr(predicate::str::contains(
            "another flighty-wall process holds",
        ));
}

#[test]
fn sync_against_an_unreachable_wall_is_exit_3_and_writes_nothing() {
    // 127.0.0.1:443 with nobody listening: the calendar side fails first (no network to
    // Google either), which is still a non-authoritative cycle, still exit 3.
    let sandbox = Sandbox::new();
    sandbox.with_flightwall();

    flighty_wall()
        .args(["sync", "--config"])
        .arg(sandbox.config())
        .env("HTTPS_PROXY", "http://127.0.0.1:9") // make the Google call fail fast too
        .assert()
        .code(3)
        .stdout(predicate::str::contains(
            "status=calendar_not_authoritative",
        ))
        .stdout(predicate::str::contains(
            "calendar_reason=calendar_request_failed:",
        ));

    // The cycle ran far enough to create the private state database and nothing else.
    assert!(sandbox.path("state/state.sqlite3").exists());
    let mode = fs::metadata(sandbox.path("state/state.sqlite3"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

// ---------------------------------------------------------------------------------------
// sanitize-capture is fully offline and leaves files behind.
// ---------------------------------------------------------------------------------------

fn har_with(entries: &[Value]) -> Value {
    json!({"log": {"version": "1.2", "creator": {"name": "test"}, "entries": entries}})
}

fn configuration_get() -> Value {
    json!({
        "request": {
            "method": "GET",
            "url": "https://api.theflightwall.com/configuration",
            "headers": [
                {"name": "x-api-key", "value": "SECRET-KEY-0123456789abcdef0123456789abcdef"},
                {"name": "user-agent", "value": "TheFlightWall/1"}
            ],
            "queryString": []
        },
        "response": {
            "status": 200,
            "headers": [{"name": "cf-ray", "value": "ray-000"}],
            "content": {
                "mimeType": "application/json",
                "text": "{\"request_config\":{\"radius_request\":{\"latitude\":53.349805,\"longitude\":-6.26031},\"tracked_flights\":[{\"flight_number\":\"EI61\"}]},\"userId\":\"fw_ios_abc\"}"
            }
        }
    })
}

#[test]
fn sanitize_capture_writes_private_fixtures_and_a_manifest() {
    let sandbox = Sandbox::new();
    let har = sandbox.path("capture.har");
    fs::write(&har, har_with(&[configuration_get()]).to_string()).unwrap();
    let out = sandbox.path("fixtures");

    flighty_wall()
        .args(["sanitize-capture", "--input"])
        .arg(&har)
        .arg("--output-dir")
        .arg(&out)
        .args(["--host", "api.theflightwall.com"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "GET    200  api.theflightwall.com/configuration -> 001-get-configuration.json",
        ))
        .stdout(predicate::str::contains(
            "wrote 1 sanitized entr(ies) and manifest.json",
        ))
        .stdout(predicate::str::contains("review every file by hand"));

    let fixture_path = out.join("001-get-configuration.json");
    let fixture = fs::read_to_string(&fixture_path).unwrap();
    assert!(fixture.ends_with('\n'));
    assert!(!fixture.contains("SECRET-KEY"));
    assert!(!fixture.contains("ray-000"));
    assert!(!fixture.contains("53.349805"));
    assert!(!fixture.contains("fw_ios_abc"));
    assert!(fixture.contains("EI61"));
    assert_eq!(
        fs::metadata(&fixture_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["entry_count"], 1);
    assert_eq!(manifest["hosts"], json!(["api.theflightwall.com"]));
}

#[test]
fn sanitize_capture_with_no_matching_host_lists_the_hosts_and_exits_1() {
    let sandbox = Sandbox::new();
    let har = sandbox.path("capture.har");
    fs::write(&har, har_with(&[configuration_get()]).to_string()).unwrap();

    flighty_wall()
        .args(["sanitize-capture", "--input"])
        .arg(&har)
        .arg("--output-dir")
        .arg(sandbox.path("fixtures"))
        .args(["--host", "other.invalid"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "no entries matched. hosts in this capture: api.theflightwall.com",
        ));

    assert!(!sandbox.path("fixtures").exists());
}

#[test]
fn sanitize_capture_rejects_missing_and_non_har_input() {
    let sandbox = Sandbox::new();

    flighty_wall()
        .args(["sanitize-capture", "--input"])
        .arg(sandbox.path("missing.har"))
        .arg("--output-dir")
        .arg(sandbox.path("fixtures"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("capture file does not exist"));

    let not_har = sandbox.path("list.json");
    fs::write(&not_har, "[1, 2, 3]").unwrap();
    flighty_wall()
        .args(["sanitize-capture", "--input"])
        .arg(&not_har)
        .arg("--output-dir")
        .arg(sandbox.path("fixtures"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("top level is not an object"));

    let no_log = sandbox.path("nolog.json");
    fs::write(&no_log, "{}").unwrap();
    flighty_wall()
        .args(["sanitize-capture", "--input"])
        .arg(&no_log)
        .arg("--output-dir")
        .arg(sandbox.path("fixtures"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "capture error: capture is not a HAR archive",
        ));
}

// ---------------------------------------------------------------------------------------
// Shape of the CLI itself.
// ---------------------------------------------------------------------------------------

#[test]
fn help_lists_every_command() {
    let output = flighty_wall().arg("--help").assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    for command in [
        "inspect-calendar",
        "sanitize-capture",
        "probe-wall",
        "sync",
        "run",
    ] {
        assert!(stdout.contains(command), "missing {command} in:\n{stdout}");
    }
}

#[test]
fn inspect_calendar_bounds_its_window() {
    let sandbox = Sandbox::new();
    flighty_wall()
        .args(["inspect-calendar", "--config"])
        .arg(sandbox.config())
        .arg("--output")
        .arg(sandbox.path("out.json"))
        .args(["--lookahead-days", "366"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("366"))
        .stderr(predicate::str::contains("not in 0..=365"));
}
