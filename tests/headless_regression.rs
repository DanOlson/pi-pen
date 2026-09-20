#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
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
      show) if [ "$TEST_CASE" = context-failure ]; then exit 1; fi; if [ "$TEST_CASE" = context-held-pipe ]; then sleep 30 & fi; printf 'test-context\n' ;;
      inspect) printf 'unix:///test/docker.sock\n' ;;
    esac ;;
  image) exit 0 ;;
  info) printf 'fixture-daemon-id\n' ;;
  network)
    if [ "$TEST_CASE" = missing-network ]; then exit 1; fi ;;
  run) printf '0.85.1\n' ;;
  create)
    cidfile=''
    previous=''
    for argument in "$@"; do
      if [ "$previous" = --cidfile ]; then cidfile="$argument"; fi
      previous="$argument"
    done
    if [ -n "$cidfile" ]; then printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n' > "$cidfile"; fi
    if [ "$TEST_CASE" = create-overflow ]; then head -c 1100000 /dev/zero >&2; exit 0; fi
    if [ "$TEST_CASE" = create-failure ]; then printf 'fixture create failure\n' >&2; exit 1; fi
    if [ "$TEST_CASE" = create-timeout ]; then sleep 3; fi
    printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n' ;;
  start)
    printf 'fixture worker diagnostic\n' >&2
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
    printf '%s\n' '{"type":"agent_end","messages":[]}'
    if [ "$TEST_CASE" = closed-output ]; then sleep 1; fi ;;
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
        let mut command = self.exec_without_output(case, timeout);
        command.arg("--output-dir").arg(&self.output);
        command
    }

    /// An exec invocation with everything but an output target, for tests that pick one.
    fn exec_without_output(&self, case: &str, timeout: &str) -> Command {
        let mut command = self.base_command(case);
        command
            .args(["exec", "--prompt-file"])
            .arg(self.root.path().join("prompt.txt"))
            .args(["--timeout", timeout]);
        command
    }

    /// The launcher with the fixture's fake Docker and private config/cache, no subcommand.
    fn base_command(&self, case: &str) -> Command {
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture worker diagnostic"));
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
fn closing_stdout_before_terminal_event_cannot_leave_success_artifact() {
    let fixture = Fixture::new();
    let mut child = fixture
        .command("closed-output")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "worker ended before agent_end"
        );
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["event"]["type"] == "agent_end" {
            break;
        }
    }
    drop(reader);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.result()["status"], "failed");
    assert_eq!(fixture.result()["wrapper_exit_code"], 3);
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

#[test]
fn context_failure_after_claim_has_terminal_artifacts() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("context-failure"));
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
fn create_output_failure_cleans_cidfile_container() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("create-overflow"));
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.result()["wrapper_exit_code"], 3);
    let log = fs::read_to_string(fixture.root.path().join("docker.log")).unwrap();
    assert!(log.lines().any(|line| line.starts_with("rm -f ")), "{log}");
}

#[test]
fn inherited_context_pipe_obeys_overall_timeout() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let output = bounded_output(fixture.command_with_timeout("context-held-pipe", "2"));
    assert_eq!(
        output.status.code(),
        Some(124),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(fixture.result()["wrapper_exit_code"], 124);
    assert_eq!(fixture.result()["status"], "timed_out");
}

#[test]
fn run_record_names_namespace_and_persistent_home_and_warns_without_credentials() {
    let fixture = Fixture::new();
    let output = bounded_output(fixture.command("success"));
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("persistent home: "), "{stderr}");
    assert!(stderr.contains("no saved pi credentials"), "{stderr}");
    let record: Value =
        serde_json::from_slice(&fs::read(fixture.output.join("run.json")).unwrap()).unwrap();
    let home = PathBuf::from(record["persistent_home"].as_str().unwrap());
    assert!(home.starts_with(fixture.root.path().join("config")));
    assert!(home.join("pi-sandbox-home.json").is_file());
    let namespace = record["namespace"].as_str().unwrap();
    assert_eq!(namespace, record["worktree_id"].as_str().unwrap());
    assert!(namespace.starts_with("wt_"));
}

#[test]
fn saved_credentials_or_api_key_variables_silence_the_warning() {
    let fixture = Fixture::new();
    let mut command = fixture.exec_without_output("success", "5");
    command
        .arg("--output-dir")
        .arg(fixture.root.path().join("with-key"))
        .args(["--env", "OPENAI_API_KEY=not-a-real-key"]);
    let output = bounded_output(command);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("no saved pi credentials"), "{stderr}");
}

#[test]
fn new_run_under_allocates_numbered_directories() {
    let fixture = Fixture::new();
    let batch = fixture.root.path().join("batch");
    fs::create_dir(&batch).unwrap();
    for expected in ["review-01", "review-02"] {
        let mut command = fixture.exec_without_output("success", "5");
        command
            .arg("--new-run-under")
            .arg(&batch)
            .args(["--run-name", "review"]);
        let output = bounded_output(command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let allocated = batch.join(expected);
        assert!(
            stderr
                .lines()
                .any(|line| line == format!("output_dir: {}", allocated.display())),
            "{stderr}"
        );
        let result: Value =
            serde_json::from_slice(&fs::read(allocated.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["status"], "completed");
    }
    let mut command = fixture.exec_without_output("success", "5");
    command
        .arg("--new-run-under")
        .arg(fixture.root.path().join("absent"));
    let missing = bounded_output(command);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("batch directory does not exist"));
}

#[test]
fn dry_run_and_namespace_show_report_the_same_home() {
    let fixture = Fixture::new();
    let mut command = fixture.command("success");
    command.args(["--dry-run", "--namespace", "batch_shared"]);
    let dry = bounded_output(command);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_stdout = String::from_utf8_lossy(&dry.stdout);
    let home_line = dry_stdout
        .lines()
        .find(|line| line.starts_with("persistent home mount: "))
        .unwrap();
    assert!(!home_line.contains("<created on real run>"));
    assert!(home_line.contains("batch_shared"));
    assert!(home_line.contains("not created yet"));

    let mut show = fixture.base_command("success");
    show.args(["namespace", "show", "--namespace", "batch_shared", "--json"]);
    let show_out = bounded_output(show);
    assert!(
        show_out.status.success(),
        "{}",
        String::from_utf8_lossy(&show_out.stderr)
    );
    let report: Value = serde_json::from_slice(&show_out.stdout).unwrap();
    assert_eq!(report["namespace"], "batch_shared");
    assert_eq!(report["shared"], true);
    assert_eq!(report["home_exists"], false);
    assert_eq!(report["pi_credentials"], false);
    let home = report["persistent_home"].as_str().unwrap();
    assert!(home_line.contains(home));
    assert!(!fixture.root.path().join("config/homes").exists());
}

#[test]
fn namespace_adopt_copies_credentials_into_the_resolved_home() {
    let fixture = Fixture::new();
    let legacy = fixture.root.path().join("legacy-home");
    fs::create_dir_all(legacy.join(".pi/agent")).unwrap();
    fs::write(legacy.join(".pi/agent/auth.json"), b"{\"openai\":{}}").unwrap();
    fs::write(legacy.join(".pi/agent/settings.json"), b"{}").unwrap();

    let mut adopt = fixture.base_command("success");
    adopt
        .args([
            "namespace",
            "adopt",
            "--namespace",
            "batch_shared",
            "--from",
        ])
        .arg(&legacy);
    let output = bounded_output(adopt);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(".pi/agent/auth.json, .pi/agent/settings.json"),
        "{stdout}"
    );
    assert!(
        legacy.join(".pi/agent/auth.json").is_file(),
        "source must be left in place"
    );

    let mut show = fixture.base_command("success");
    show.args(["namespace", "show", "--namespace", "batch_shared", "--json"]);
    let report: Value = serde_json::from_slice(&bounded_output(show).stdout).unwrap();
    assert_eq!(report["pi_credentials"], true);
    let home = PathBuf::from(report["persistent_home"].as_str().unwrap());
    assert_eq!(
        fs::read(home.join(".pi/agent/auth.json")).unwrap(),
        b"{\"openai\":{}}"
    );
    assert_eq!(
        fs::metadata(home.join(".pi/agent/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let mut again = fixture.base_command("success");
    again
        .args([
            "namespace",
            "adopt",
            "--namespace",
            "batch_shared",
            "--from",
        ])
        .arg(&legacy);
    let refused = bounded_output(again);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("pass --force"));

    let mut forced = fixture.base_command("success");
    forced
        .args([
            "namespace",
            "adopt",
            "--namespace",
            "batch_shared",
            "--force",
            "--from",
        ])
        .arg(&legacy);
    assert!(bounded_output(forced).status.success());
}
