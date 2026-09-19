use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_pi-sandbox")
}

#[test]
fn top_level_help_flag_prints_usage() {
    let output = Command::new(bin()).arg("--help").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: pi-sandbox <COMMAND>"));
    assert!(stdout.contains("Options:"));
    assert!(stdout.contains("-h, --help"));
}

#[test]
fn short_help_flag_prints_usage() {
    let output = Command::new(bin()).arg("-h").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: pi-sandbox <COMMAND>"));
    assert!(stdout.contains("-h, --help"));
}

#[test]
fn subcommand_help_flag_prints_usage() {
    let output = Command::new(bin())
        .args(["init", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: pi-sandbox init [OPTIONS]"));
    assert!(stdout.contains("--yes"));
    assert!(stdout.contains("--force"));
}

#[test]
fn build_help_includes_image_refresh_controls() {
    let output = Command::new(bin())
        .args(["build", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--rebuild"));
    assert!(stdout.contains("--pull"));
}

#[test]
fn run_help_includes_image_refresh_controls() {
    let output = Command::new(bin())
        .args(["run", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--rebuild"));
    assert!(stdout.contains("--pull"));
    assert!(stdout.contains("--network"));
    assert!(stdout.contains("--default-network"));
    assert!(stdout.contains("--host-access"));
    assert!(stdout.contains("--env-file"));
    assert!(stdout.contains("--namespace"));
    assert!(stdout.contains("--dry-run"));
}

#[test]
fn dry_run_redacts_values_and_does_not_need_docker() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".pi-env.toml"),
        "[run]\nnetworks=[\"missing-test-network\"]\n[run.env]\nSECRET=\"do-not-print\"\n",
    )
    .unwrap();
    let output = Command::new(bin())
        .args(["run", "--dry-run", "--env", "OTHER=also-secret"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("missing-test-network"));
    assert!(stdout.contains("unverified"));
    assert!(stdout.contains("SECRET=<redacted>"));
    assert!(!stdout.contains("do-not-print"));
    assert!(!stdout.contains("also-secret"));
}

#[test]
fn default_network_conflicts_with_network() {
    let output = Command::new(bin())
        .args(["run", "--network", "one", "--default-network", "--dry-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[cfg(unix)]
#[test]
fn termination_removes_the_secret_launch_file() {
    let dir = tempdir().unwrap();
    let bin_dir = dir.path().join("bin");
    let temp_dir = dir.path().join("tmp");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&temp_dir).unwrap();
    let docker = bin_dir.join("docker");
    let marker = dir.path().join("docker-terminated");
    fs::write(&docker, "#!/bin/sh\ncase \"$1 $2\" in\n  'image inspect') exit 0;;\n  'run --rm') trap 'touch \"$MARKER\"; exit 0' TERM INT HUP; while :; do sleep 1; done;;\n  *) exit 0;;\nesac\n").unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        dir.path().join(".pi-env.toml"),
        "[run.env]\nSECRET=\"distinctive-secret\"\n",
    )
    .unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    let mut child = Command::new(bin())
        .arg("run")
        .current_dir(dir.path())
        .env("PATH", path)
        .env("TMPDIR", &temp_dir)
        .env("MARKER", &marker)
        .env("PI_SANDBOX_CONFIG_DIR", dir.path().join("config"))
        .env("PI_SANDBOX_CACHE_DIR", dir.path().join("cache"))
        .spawn()
        .unwrap();
    let mut appeared = false;
    for _ in 0..100 {
        appeared = fs::read_dir(&temp_dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("pi-sandbox-env-")
        });
        if appeared {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(appeared, "launch env file was not created");
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let _ = child.wait().unwrap();
    assert!(marker.exists(), "termination was not forwarded to Docker");
    assert!(!fs::read_dir(&temp_dir).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("pi-sandbox-env-")
    }));
}

#[test]
fn env_show_renders_builtin_default() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["env", "show"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("FROM node:22-bookworm"));
    assert!(stdout.contains("npm install -g --ignore-scripts @earendil-works/pi-coding-agent"));
}

#[test]
fn env_show_uses_repo_manifest() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".pi-env.toml"),
        r#"
name = "repo"
base_image = "node:20-bookworm"
packages = ["ripgrep"]
"#,
    )
    .unwrap();

    let output = Command::new(bin())
        .args(["env", "show"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("FROM node:20-bookworm"));
    assert!(stdout.contains("build-essential"));
    assert!(stdout.contains("ripgrep"));
}

#[test]
fn env_show_renders_an_inherited_image_chain() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("base.toml"),
        "name = \"base\"\npackages = [\"ripgrep\"]\n",
    )
    .unwrap();
    fs::write(
        dir.path().join(".pi-env.toml"),
        "name = \"child\"\nextends = \"base.toml\"\npackages = [\"fd-find\"]\n",
    )
    .unwrap();

    let output = Command::new(bin())
        .args(["env", "show"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("# image pi-sandbox-env:").count(), 2);
    assert!(stdout.contains("FROM node:22-bookworm"));
    assert!(stdout.contains("FROM pi-sandbox-env:"));
    assert!(stdout.contains("ripgrep"));
    assert!(stdout.contains("fd-find"));
}

#[test]
fn env_list_includes_builtin() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["env", "list"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("built-in\tdefault\tnode:22-bookworm"));
}

#[test]
fn init_yes_writes_default_manifest() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["init", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let manifest = fs::read_to_string(dir.path().join(".pi-env.toml")).unwrap();
    assert!(manifest.contains("base_image = \"node:22-bookworm\""));
    assert!(manifest.contains("name = "));
    assert!(!manifest.contains("default_packages"));
}

#[test]
fn init_flags_write_manifest_overrides() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args([
            "init",
            "--yes",
            "--name",
            "custom",
            "--base-image",
            "node:20-bookworm",
            "--no-default-packages",
            "--package",
            "ripgrep",
            "--command",
            "npm --version",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let manifest = fs::read_to_string(dir.path().join(".pi-env.toml")).unwrap();
    assert!(manifest.contains("name = \"custom\""));
    assert!(manifest.contains("base_image = \"node:20-bookworm\""));
    assert!(manifest.contains("default_packages = false"));
    assert!(manifest.contains("packages = [\"ripgrep\"]"));
    assert!(manifest.contains("commands = [\"npm --version\"]"));
}

#[test]
fn init_extends_writes_parent_without_a_base_image() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["init", "--yes", "--extends", "rust-node"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let manifest = fs::read_to_string(dir.path().join(".pi-env.toml")).unwrap();
    assert!(manifest.contains("extends = \"rust-node\""));
    assert!(!manifest.contains("base_image"));
}

#[test]
fn init_git_flags_write_git_section() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args([
            "init",
            "--yes",
            "--git-user-name",
            "Dan",
            "--git-user-email",
            "dan@example.com",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let manifest = fs::read_to_string(dir.path().join(".pi-env.toml")).unwrap();
    assert!(manifest.contains("[git]"));
    assert!(manifest.contains("user_name = \"Dan\""));
    assert!(manifest.contains("user_email = \"dan@example.com\""));
}

#[test]
fn init_with_python_writes_pinned_uv_installer() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["init", "--yes", "--with", "python"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let manifest = fs::read_to_string(dir.path().join(".pi-env.toml")).unwrap();
    assert!(manifest.contains("uv/releases/download/0.12.0/uv-installer.sh"));
    assert!(manifest.contains("UV_UNMANAGED_INSTALL=/usr/local/bin"));
    assert!(manifest.contains("ca-certificates"));
    assert!(manifest.contains("curl"));
}

#[test]
fn init_with_tauri_writes_preset_manifest() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["init", "--yes", "--with", "tauri"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let manifest = fs::read_to_string(dir.path().join(".pi-env.toml")).unwrap();
    assert!(manifest.contains("libgtk-3-dev"));
    assert!(manifest.contains("libwebkit2gtk-4.1-dev"));
    assert!(manifest.contains("/opt/cargo/bin"));
    assert!(manifest.contains("[runtime_env]"));
}

#[test]
fn init_with_unknown_preset_fails() {
    let dir = tempdir().unwrap();
    let output = Command::new(bin())
        .args(["init", "--yes", "--with", "nope"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown preset"));
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".pi-env.toml"), "name = \"existing\"\n").unwrap();

    let output = Command::new(bin())
        .args(["init", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("already exists"));
}
