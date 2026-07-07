use std::fs;
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
