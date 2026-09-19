#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{TempDir, tempdir};

struct Fixture {
    root: TempDir,
    workspace: PathBuf,
    output: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(workspace.join(".pi-env.toml"), "default_packages = false\n").unwrap();
        fs::write(
            root.path().join("prompt.txt"),
            "A test task, not a real model request.",
        )
        .unwrap();
        let docker = root.path().join("bin/docker");
        fs::write(&docker, r#"#!/bin/sh
printf '%s\n' "$*" >> "$TEST_DOCKER_LOG"
case "$1" in
  context)
    case "$2" in
      show) printf 'test-context\n' ;;
      inspect) printf 'unix:///test/docker.sock\n' ;;
    esac ;;
  image) exit 0 ;;
  network)
    if [ "$TEST_CASE" = missing-network ]; then exit 1; fi ;;
  create)
    if [ "$TEST_CASE" = create-failure ]; then printf 'fixture create failure\n' >&2; exit 1; fi
    if [ "$TEST_CASE" = create-timeout ]; then sleep 3; fi
    printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n' ;;
  start)
    case "$TEST_CASE" in
      malformed) printf 'not-json\n'; exit 0 ;;
      empty-stream) exit 0 ;;
    esac
    printf '%s\n' '{"type":"session","version":3,"id":"fixture-session","cwd":"/workspace"}'
    printf '%s\n' '{"type":"agent_start"}'
    if [ "$TEST_CASE" = recovered-error ]; then
      printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"temporary fixture error"}}'
    fi
    if [ "$TEST_CASE" = agent-error ]; then
      printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"fixture model error"}}'
    else
      printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"fixture result"}],"stopReason":"stop"}}'
    fi
    printf '%s\n' '{"type":"agent_end","messages":[]}' ;;
  inspect)
    case "$*" in
      *ExitCode*) printf '0\n' ;;
      *) printf '{}\n' ;;
    esac ;;
  rm)
    if [ "$TEST_CASE" = cleanup-failure ]; then exit 1; fi ;;
  stop|kill) exit 0 ;;
  *) printf 'unexpected fake docker command\n' >&2; exit 99 ;;
esac
"#).unwrap();
        fs::set_permissions(docker, fs::Permissions::from_mode(0o755)).unwrap();
        let output = root.path().join("output");
        Self {
            root,
            workspace,
            output,
        }
    }

    fn command(&self, case: &str) -> Command {
        self.command_with_timeout(case, "5")
    }

    fn command_with_timeout(&self, case: &str, timeout: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi-sandbox"));
        command
            .current_dir(&self.workspace)
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.root.path().join("bin").display()
                ),
            )
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("PI_SANDBOX_CONFIG_DIR", self.root.path().join("config"))
            .env("PI_SANDBOX_CACHE_DIR", self.root.path().join("cache"))
            .env("TEST_CASE", case)
            .env("TEST_DOCKER_LOG", self.root.path().join("docker.log"))
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_CONTEXT")
            .args(["exec", "--prompt-file"])
            .arg(self.root.path().join("prompt.txt"))
            .arg("--output-dir")
            .arg(&self.output)
            .args(["--timeout", timeout])
            .stdin(Stdio::null());
        command
    }

    fn result(&self) -> Value {
        serde_json::from_slice(&fs::read(self.output.join("result.json")).unwrap()).unwrap()
    }
}

fn bounded_output(mut command: Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "headless CLI hung: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exec_without_tty_streams_only_json_and_persists_final_text() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("success"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["type"], "run_started");
    assert_eq!(events.last().unwrap()["type"], "run_finished");
    for pair in events.windows(2) {
        assert!(pair[0]["seq"].as_u64().unwrap() < pair[1]["seq"].as_u64().unwrap());
    }
    assert_eq!(fixture.result()["status"], "completed");
    assert_eq!(
        fs::read_to_string(fixture.output.join("response.txt")).unwrap(),
        "fixture result"
    );
}

#[test]
fn final_agent_error_is_failure_even_when_process_exits_zero() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("agent-error"));
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.result()["status"], "failed");
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["type"], "run_finished");
    assert_eq!(events.last().unwrap()["status"], "failed");
    assert_eq!(events.last().unwrap()["exit_code"], 1);
}

#[test]
fn empty_and_malformed_streams_are_not_success() {
    for case in ["empty-stream", "malformed"] {
        let fixture = Fixture::new();
        let output = bounded_output(fixture.command(case));
        assert_eq!(
            output.status.code(),
            Some(1),
            "case {case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.result()["status"], "failed");
    }
}

#[test]
fn recovered_agent_error_does_not_poison_final_success() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("recovered-error"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.result()["status"], "completed");
}

#[test]
fn cleanup_failure_agrees_across_exit_event_and_result() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("cleanup-failure"));
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["type"], "run_finished");
    assert_eq!(events.last().unwrap()["status"], "failed");
    assert_eq!(fixture.result()["status"], "failed");
    assert_eq!(fixture.result()["wrapper_exit_code"], 3);
}

#[test]
fn create_failure_produces_terminal_artifacts() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("create-failure"));
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.result()["status"], "failed");
    let run: Value =
        serde_json::from_slice(&fs::read(fixture.output.join("run.json")).unwrap()).unwrap();
    assert_eq!(run["state"], "terminal");
}

#[test]
fn create_timeout_preserves_timeout_status_in_artifacts() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command_with_timeout("create-timeout", "1"));
    assert_eq!(
        output.status.code(),
        Some(124),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.result()["status"], "timed_out");
    assert_eq!(fixture.result()["wrapper_exit_code"], 124);
}

#[test]
fn artifacts_and_session_directory_are_private() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("success"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in [
        "run.json",
        "result.json",
        "events.jsonl",
        "stderr.log",
        "response.txt",
        "sessions",
    ] {
        let path = fixture.output.join(name);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0,
            "{} is public",
            path.display()
        );
    }
}

#[test]
fn output_symlink_into_workspace_is_rejected_before_docker() {
    let mut fixture = Fixture::new();
    symlink(&fixture.workspace, fixture.root.path().join("alias")).unwrap();
    fixture.output = fixture.root.path().join("alias/output");
    let output = bounded_output(fixture.command("success"));
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.root.path().join("docker.log").exists());
}

#[test]
fn missing_network_is_checked_before_image_work() {
    let fixture = Fixture::new();
    let mut command = fixture.command("missing-network");
    command.args(["--network", "missing-test-network"]);
    let output = bounded_output(command);
    assert!(!output.status.success());
    let log = fs::read_to_string(fixture.root.path().join("docker.log")).unwrap();
    assert!(
        !log.lines()
            .any(|line| line.starts_with("build ") || line.starts_with("image ")),
        "{log}"
    );
}
