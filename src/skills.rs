use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const SKILL_NAME: &str = "pi-sandbox";
const MANIFEST_NAME: &str = ".pi-sandbox-skill.json";
const MANAGED_BY: &str = "pi-sandbox";
const CLAUDE_LINK_TARGET: &str = "../../.agents/skills/pi-sandbox";

const BUNDLE: &[(&str, &str)] = &[
    (
        "SKILL.md",
        include_str!("../bundled-skills/pi-sandbox/SKILL.md"),
    ),
    (
        "references/execution-contract.md",
        include_str!("../bundled-skills/pi-sandbox/references/execution-contract.md"),
    ),
    (
        "references/orchestration.md",
        include_str!("../bundled-skills/pi-sandbox/references/orchestration.md"),
    ),
    (
        "references/prompting.md",
        include_str!("../bundled-skills/pi-sandbox/references/prompting.md"),
    ),
    (
        "assets/orchestrator.py",
        include_str!("../bundled-skills/pi-sandbox/assets/orchestrator.py"),
    ),
];

#[derive(Debug)]
pub struct InstallOptions {
    pub project_dir: Option<PathBuf>,
    pub force: bool,
    pub link_claude: bool,
    pub no_link_claude: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManagedManifest {
    managed_by: String,
    format_version: u32,
    bundle_version: String,
    files: BTreeMap<String, String>,
}

pub fn install(cwd: &Path, options: InstallOptions) -> Result<()> {
    let project_root = project_root(cwd, options.project_dir.as_deref())?;
    let skill_dir = project_root.join(".agents/skills").join(SKILL_NAME);
    ensure_no_symlink_parent(
        &project_root,
        Path::new(".agents/skills/pi-sandbox/bundled-file"),
    )?;
    let changed = install_bundle(&skill_dir, options.force)?;
    if changed {
        println!("Installed {}", skill_dir.display());
    } else {
        println!("Skill is current at {}", skill_dir.display());
    }

    let link = if options.link_claude {
        true
    } else if options.no_link_claude {
        false
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        prompt_yes_no(
            "Make this skill available to Claude via .claude/skills/pi-sandbox?",
            false,
        )?
    } else {
        println!("Skipped Claude link in non-interactive mode; pass --link-claude to create it.");
        false
    };

    if link {
        install_claude_link(&project_root)?;
    }
    Ok(())
}

fn project_root(cwd: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let root = candidate.canonicalize().with_context(|| {
            format!(
                "failed to resolve project directory {}",
                candidate.display()
            )
        })?;
        if !root.is_dir() {
            bail!("project directory is not a directory: {}", root.display());
        }
        return Ok(root);
    }

    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("failed to run Git while locating the project root")?;
    if !output.status.success() {
        bail!(
            "could not find a Git worktree root from {}; pass --project-dir PATH to choose the installation root",
            cwd.display()
        );
    }
    let text = String::from_utf8(output.stdout).context("Git returned a non-UTF-8 project path")?;
    let path = PathBuf::from(text.trim());
    path.canonicalize()
        .with_context(|| format!("failed to resolve Git worktree root {}", path.display()))
}

fn install_bundle(skill_dir: &Path, force: bool) -> Result<bool> {
    let expected = expected_manifest();
    let expected_text = manifest_text(&expected)?;

    match fs::symlink_metadata(skill_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to install through symlink {}",
                skill_dir.display()
            )
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!(
                "skill destination is not a directory: {}",
                skill_dir.display()
            )
        }
        Ok(_) => {
            if bundle_matches(skill_dir, &expected, &expected_text)? {
                return Ok(false);
            }
            if !force {
                verify_managed_files_unchanged(skill_dir)?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", skill_dir.display()));
        }
    }

    fs::create_dir_all(skill_dir)
        .with_context(|| format!("failed to create {}", skill_dir.display()))?;
    for (relative, contents) in BUNDLE {
        let path = skill_dir.join(relative);
        ensure_no_symlink_parent(skill_dir, Path::new(relative))?;
        let parent = path.parent().context("bundled skill path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        write_atomic(&path, contents.as_bytes())?;
    }
    write_atomic(&skill_dir.join(MANIFEST_NAME), expected_text.as_bytes())?;
    Ok(true)
}

fn expected_manifest() -> ManagedManifest {
    ManagedManifest {
        managed_by: MANAGED_BY.to_string(),
        format_version: 1,
        bundle_version: env!("CARGO_PKG_VERSION").to_string(),
        files: BUNDLE
            .iter()
            .map(|(path, contents)| ((*path).to_string(), fingerprint(contents.as_bytes())))
            .collect(),
    }
}

fn manifest_text(manifest: &ManagedManifest) -> Result<String> {
    let mut text = serde_json::to_string_pretty(manifest)
        .context("failed to serialize bundled skill manifest")?;
    text.push('\n');
    Ok(text)
}

fn bundle_matches(
    skill_dir: &Path,
    expected: &ManagedManifest,
    expected_text: &str,
) -> Result<bool> {
    let marker = match fs::read_to_string(skill_dir.join(MANIFEST_NAME)) {
        Ok(marker) => marker,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("failed to read installed skill manifest"),
    };
    if marker != expected_text {
        return Ok(false);
    }
    for (relative, expected_hash) in &expected.files {
        let bytes = match fs::read(skill_dir.join(relative)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("failed to read installed skill file"),
        };
        if fingerprint(&bytes) != *expected_hash {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_managed_files_unchanged(skill_dir: &Path) -> Result<()> {
    let marker_path = skill_dir.join(MANIFEST_NAME);
    let marker = fs::read_to_string(&marker_path).with_context(|| {
        format!(
            "{} already exists and is not a recognized managed installation; pass --force to overwrite bundled files",
            skill_dir.display()
        )
    })?;
    let installed: ManagedManifest = serde_json::from_str(&marker).with_context(|| {
        format!(
            "{} has an invalid managed manifest; pass --force to overwrite bundled files",
            marker_path.display()
        )
    })?;
    if installed.managed_by != MANAGED_BY || installed.format_version != 1 {
        bail!(
            "{} is not a recognized pi-sandbox skill installation; pass --force to overwrite bundled files",
            skill_dir.display()
        );
    }
    for (relative, expected_hash) in installed.files {
        let relative_path = validated_relative_path(&relative)?;
        let path = skill_dir.join(relative_path);
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "managed skill file {} is missing; pass --force to repair the installation",
                path.display()
            )
        })?;
        if fingerprint(&bytes) != expected_hash {
            bail!(
                "managed skill file {} was modified; pass --force to overwrite bundled files",
                path.display()
            );
        }
    }
    Ok(())
}

fn ensure_no_symlink_parent(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("refusing to install through symlink {}", current.display())
                }
                Ok(metadata) if !metadata.is_dir() => {
                    bail!(
                        "skill path component is not a directory: {}",
                        current.display()
                    )
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error).context("failed to inspect skill destination"),
            }
        }
    }
    Ok(())
}

fn validated_relative_path(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!(
            "managed skill manifest contains unsafe path `{}`",
            path.display()
        );
    }
    Ok(path)
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("skill file name is not valid UTF-8")?;
    let temporary = path.with_file_name(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.display()))?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(())
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn prompt_yes_no(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    print!("{label} [{suffix}]: ");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    match input.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => bail!("expected yes or no for `{label}`"),
    }
}

#[cfg(unix)]
fn install_claude_link(project_root: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let skills_dir = project_root.join(".claude/skills");
    let link = skills_dir.join(SKILL_NAME);
    ensure_no_symlink_parent(project_root, Path::new(".claude/skills/pi-sandbox"))?;
    match fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(&link)
                .with_context(|| format!("failed to read symlink {}", link.display()))?;
            if target == Path::new(CLAUDE_LINK_TARGET) {
                println!("Claude link is current at {}", link.display());
                return Ok(());
            }
            bail!(
                "{} is already a different symlink; remove it explicitly before retrying",
                link.display()
            );
        }
        Ok(_) => bail!(
            "{} already exists and is not the managed symlink; move it explicitly before retrying",
            link.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", link.display()));
        }
    }
    fs::create_dir_all(&skills_dir)
        .with_context(|| format!("failed to create {}", skills_dir.display()))?;
    symlink(CLAUDE_LINK_TARGET, &link)
        .with_context(|| format!("failed to create symlink {}", link.display()))?;
    println!("Linked {} -> {CLAUDE_LINK_TARGET}", link.display());
    Ok(())
}

#[cfg(not(unix))]
fn install_claude_link(_project_root: &Path) -> Result<()> {
    bail!("--link-claude is currently supported on macOS and Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_is_idempotent_and_preserves_extra_files() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join(".agents/skills/pi-sandbox");
        assert!(install_bundle(&skill, false).unwrap());
        fs::write(skill.join("notes.md"), "mine").unwrap();
        assert!(!install_bundle(&skill, false).unwrap());
        assert_eq!(fs::read_to_string(skill.join("notes.md")).unwrap(), "mine");
    }

    #[test]
    fn modified_managed_file_requires_force() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join(".agents/skills/pi-sandbox");
        install_bundle(&skill, false).unwrap();
        fs::write(skill.join("SKILL.md"), "changed").unwrap();
        let error = install_bundle(&skill, false).unwrap_err();
        assert!(error.to_string().contains("was modified"));
        assert!(install_bundle(&skill, true).unwrap());
        assert!(
            fs::read_to_string(skill.join("SKILL.md"))
                .unwrap()
                .contains("# Pi Sandbox")
        );
    }

    #[cfg(unix)]
    #[test]
    fn installer_refuses_symlinked_agent_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), dir.path().join(".agents")).unwrap();
        let error = install(
            dir.path(),
            InstallOptions {
                project_dir: Some(dir.path().to_path_buf()),
                force: true,
                link_claude: false,
                no_link_claude: true,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to install through symlink")
        );
        assert!(!outside.path().join("skills").exists());
    }

    #[cfg(unix)]
    #[test]
    fn claude_link_is_relative_and_idempotent() {
        let dir = tempdir().unwrap();
        install_bundle(&dir.path().join(".agents/skills/pi-sandbox"), false).unwrap();
        install_claude_link(dir.path()).unwrap();
        install_claude_link(dir.path()).unwrap();
        assert_eq!(
            fs::read_link(dir.path().join(".claude/skills/pi-sandbox")).unwrap(),
            Path::new(CLAUDE_LINK_TARGET)
        );
    }
}
