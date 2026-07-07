use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

const PROJECT_MANIFEST: &str = ".pi-env.toml";
const DEFAULT_BASE_IMAGE: &str = "node:22-bookworm";
const DEFAULT_PI_INSTALL: &str = "npm install -g --ignore-scripts @earendil-works/pi-coding-agent";
const DEFAULT_PACKAGES: &[&str] = &[
    "bash",
    "ca-certificates",
    "curl",
    "wget",
    "git",
    "openssh-client",
    "jq",
    "unzip",
    "zip",
    "tar",
    "gzip",
    "xz-utils",
    "build-essential",
    "pkg-config",
    "python3",
    "python3-pip",
    "python3-venv",
];

#[derive(Debug)]
pub struct Cli {
    pub command: Command,
    pub cwd: PathBuf,
}

#[derive(Debug)]
pub enum Command {
    Build {
        env: Option<String>,
    },
    Env {
        command: EnvCommand,
    },
    Init(InitOptions),
    Run {
        env: Option<String>,
        pi_args: Vec<String>,
    },
}

#[derive(Debug)]
pub enum EnvCommand {
    List,
    Show { env: Option<String> },
}

#[derive(Debug)]
pub struct InitOptions {
    pub yes: bool,
    pub force: bool,
    pub name: Option<String>,
    pub base_image: Option<String>,
    pub default_packages: Option<bool>,
    pub packages: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedEnv {
    pub source: EnvSource,
    pub manifest: EnvManifest,
}

#[derive(Debug, Clone)]
pub enum EnvSource {
    Project(PathBuf),
    User(PathBuf),
    Explicit(PathBuf),
    BuiltIn,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnvManifest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_packages: Option<bool>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_install: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<DockerfileHooks>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DockerfileHooks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerInvocation {
    pub program: String,
    pub args: Vec<String>,
}

pub fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Build { env } => {
            let resolved = resolve_env(env.as_deref(), &cli.cwd)?;
            let dockerfile = render_dockerfile(&resolved.manifest);
            let cache = cache_dir()?;
            let tag = image_tag(&resolved);
            write_resolved_dockerfile(&cache, &tag, &dockerfile)?;
            run_process(build_invocation(&cache, &tag)?)?;
        }
        Command::Env { command } => match command {
            EnvCommand::List => list_envs(&cli.cwd)?,
            EnvCommand::Show { env } => {
                let resolved = resolve_env(env.as_deref(), &cli.cwd)?;
                println!("{}", render_dockerfile(&resolved.manifest));
            }
        },
        Command::Init(options) => {
            init_manifest_interactive(&cli.cwd, options)?;
        }
        Command::Run { env, pi_args } => {
            let resolved = resolve_env(env.as_deref(), &cli.cwd)?;
            let dockerfile = render_dockerfile(&resolved.manifest);
            let cache = cache_dir()?;
            let tag = image_tag(&resolved);
            write_resolved_dockerfile(&cache, &tag, &dockerfile)?;
            run_process(build_invocation(&cache, &tag)?)?;

            let uid = command_output("id", &["-u"])?;
            let gid = command_output("id", &["-g"])?;
            run_process_interactive(run_invocation(&cli.cwd, &tag, &uid, &gid, &pi_args))?;
        }
    }

    Ok(())
}

pub fn init_manifest_interactive(cwd: &Path, options: InitOptions) -> Result<PathBuf> {
    let manifest = if options.yes {
        init_manifest_from_options(cwd, &options)
    } else {
        prompt_for_manifest(cwd, &options)?
    };

    let path = write_project_manifest(cwd, &manifest, options.force)?;
    println!("Wrote {}", path.display());
    println!("Next: pi-sandbox run");
    Ok(path)
}

pub fn init_manifest_from_options(cwd: &Path, options: &InitOptions) -> EnvManifest {
    EnvManifest {
        name: Some(
            options
                .name
                .clone()
                .unwrap_or_else(|| default_env_name(cwd)),
        ),
        base_image: Some(
            options
                .base_image
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_IMAGE.to_string()),
        ),
        default_packages: options
            .default_packages
            .and_then(|enabled| (!enabled).then_some(false)),
        packages: options.packages.clone(),
        commands: options.commands.clone(),
        ..EnvManifest::default()
    }
}

pub fn write_project_manifest(cwd: &Path, manifest: &EnvManifest, force: bool) -> Result<PathBuf> {
    let path = cwd.join(PROJECT_MANIFEST);
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }

    let mut text = toml::to_string_pretty(manifest).context("failed to serialize manifest")?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn prompt_for_manifest(cwd: &Path, options: &InitOptions) -> Result<EnvManifest> {
    let name = options
        .name
        .clone()
        .map(Ok)
        .unwrap_or_else(|| prompt_default("Environment name", &default_env_name(cwd)))?;
    let base_image = options
        .base_image
        .clone()
        .map(Ok)
        .unwrap_or_else(|| prompt_default("Base image", DEFAULT_BASE_IMAGE))?;
    let default_packages = match options.default_packages {
        Some(value) => value,
        None => prompt_yes_no("Include default packages", true)?,
    };
    let mut packages = options.packages.clone();
    packages.extend(prompt_list("Additional apt packages")?);

    let mut commands = options.commands.clone();
    commands.extend(prompt_repeated("Setup command")?);

    Ok(EnvManifest {
        name: Some(name),
        base_image: Some(base_image),
        default_packages: (!default_packages).then_some(false),
        packages,
        commands,
        ..EnvManifest::default()
    })
}

pub fn resolve_env(env: Option<&str>, cwd: &Path) -> Result<ResolvedEnv> {
    if let Some(env) = env {
        let candidate = PathBuf::from(env);
        if candidate.exists() {
            return load_manifest(candidate, EnvSource::Explicit);
        }

        if let Some(project) = load_project_manifest_if_named(cwd, env)? {
            return Ok(project);
        }

        if let Some(path) = user_env_path(env) {
            if path.exists() {
                return load_manifest(path, EnvSource::User);
            }
        }

        bail!("environment `{env}` was not found as a path, repo manifest name, or user env");
    }

    let project_path = cwd.join(PROJECT_MANIFEST);
    if project_path.exists() {
        return load_manifest(project_path, EnvSource::Project);
    }

    Ok(ResolvedEnv {
        source: EnvSource::BuiltIn,
        manifest: EnvManifest {
            name: Some("default".to_string()),
            base_image: Some(DEFAULT_BASE_IMAGE.to_string()),
            ..EnvManifest::default()
        },
    })
}

pub fn render_dockerfile(manifest: &EnvManifest) -> String {
    let mut lines = Vec::new();

    if let Some(hooks) = &manifest.dockerfile {
        if let Some(prepend) = &hooks.prepend {
            lines.push(prepend.trim_end().to_string());
        }
    }

    lines.push(format!(
        "FROM {}",
        manifest.base_image.as_deref().unwrap_or(DEFAULT_BASE_IMAGE)
    ));

    lines.push("SHELL [\"/bin/sh\", \"-c\"]".to_string());

    for (key, value) in &manifest.env {
        lines.push(format!("ENV {}={}", key, shell_escape(value)));
    }

    let packages = resolved_packages(manifest);
    if !packages.is_empty() {
        lines.push(format!(
            "RUN apt-get update && apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/*",
            packages.join(" ")
        ));
    }

    for command in &manifest.commands {
        lines.push(format!("RUN {command}"));
    }

    lines.push(format!(
        "RUN {}",
        manifest.pi_install.as_deref().unwrap_or(DEFAULT_PI_INSTALL)
    ));
    lines.push("RUN mkdir -p /tmp/pi-home && chmod 0777 /tmp/pi-home".to_string());

    if let Some(hooks) = &manifest.dockerfile {
        if let Some(append) = &hooks.append {
            lines.push(append.trim_end().to_string());
        }
    }

    lines.push(String::new());
    lines.join("\n")
}

pub fn resolved_packages(manifest: &EnvManifest) -> Vec<String> {
    let mut packages = Vec::new();
    if manifest.default_packages.unwrap_or(true) {
        packages.extend(DEFAULT_PACKAGES.iter().map(|package| package.to_string()));
    }

    for package in &manifest.packages {
        if !packages.contains(package) {
            packages.push(package.clone());
        }
    }

    packages
}

pub fn build_invocation(cache: &Path, tag: &str) -> Result<DockerInvocation> {
    let dockerfile = cache.join(tag).join("Dockerfile");
    let context = dockerfile
        .parent()
        .context("resolved Dockerfile path has no parent")?;

    Ok(DockerInvocation {
        program: "docker".to_string(),
        args: vec![
            "build".to_string(),
            "-t".to_string(),
            tag.to_string(),
            "-f".to_string(),
            dockerfile.display().to_string(),
            context.display().to_string(),
        ],
    })
}

pub fn run_invocation(
    cwd: &Path,
    tag: &str,
    uid: &str,
    gid: &str,
    pi_args: &[String],
) -> DockerInvocation {
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--user".to_string(),
        format!("{uid}:{gid}"),
        "-e".to_string(),
        "HOME=/tmp/pi-home".to_string(),
        "-v".to_string(),
        format!("{}:/workspace", cwd.display()),
        "-w".to_string(),
        "/workspace".to_string(),
        tag.to_string(),
        "sh".to_string(),
        "-c".to_string(),
    ];

    let command = if pi_args.is_empty() {
        "mkdir -p /tmp/pi-home && git config --global --add safe.directory /workspace 2>/dev/null || true; exec pi".to_string()
    } else {
        let escaped_args = pi_args
            .iter()
            .map(|arg| shell_escape(arg))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "mkdir -p /tmp/pi-home && git config --global --add safe.directory /workspace 2>/dev/null || true; exec pi {escaped_args}"
        )
    };
    args.push(command);

    DockerInvocation {
        program: "docker".to_string(),
        args,
    }
}

fn load_manifest(path: PathBuf, source: impl FnOnce(PathBuf) -> EnvSource) -> Result<ResolvedEnv> {
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    let manifest: EnvManifest = toml::from_str(&text)
        .with_context(|| format!("failed to parse manifest {}", path.display()))?;
    Ok(ResolvedEnv {
        source: source(path),
        manifest,
    })
}

fn default_env_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string()
}

fn prompt_default(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_yes_no(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    print!("{label} [{suffix}]: ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    let trimmed = input.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "" => Ok(default),
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => bail!("expected yes or no for `{label}`"),
    }
}

fn prompt_list(label: &str) -> Result<Vec<String>> {
    print!("{label} (comma or space separated, blank for none): ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    Ok(split_list(&input))
}

fn prompt_repeated(label: &str) -> Result<Vec<String>> {
    println!("{label}s (one per line, blank to finish):");
    let mut values = Vec::new();
    loop {
        print!("> ");
        io::stdout().flush().context("failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("failed to read stdin")?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            break;
        }
        values.push(trimmed.to_string());
    }
    Ok(values)
}

fn split_list(input: &str) -> Vec<String> {
    input
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn load_project_manifest_if_named(cwd: &Path, name: &str) -> Result<Option<ResolvedEnv>> {
    let path = cwd.join(PROJECT_MANIFEST);
    if !path.exists() {
        return Ok(None);
    }

    let resolved = load_manifest(path, EnvSource::Project)?;
    if resolved.manifest.name.as_deref() == Some(name) || name == "project" {
        Ok(Some(resolved))
    } else {
        Ok(None)
    }
}

fn write_resolved_dockerfile(cache: &Path, tag: &str, dockerfile: &str) -> Result<()> {
    let dir = cache.join(tag);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create cache dir {}", dir.display()))?;
    fs::write(dir.join("Dockerfile"), dockerfile)
        .with_context(|| format!("failed to write resolved Dockerfile for {tag}"))?;
    Ok(())
}

fn cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().context("could not determine user cache directory")?;
    Ok(base.join("pi-sandbox"))
}

fn user_env_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("pi-sandbox").join("envs"))
}

fn user_env_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() != 1 {
        return None;
    }

    user_env_dir().map(|dir| dir.join(format!("{name}.toml")))
}

fn list_envs(cwd: &Path) -> Result<()> {
    let project_path = cwd.join(PROJECT_MANIFEST);
    if project_path.exists() {
        let resolved = load_manifest(project_path.clone(), EnvSource::Project)?;
        let name = resolved.manifest.name.as_deref().unwrap_or("project");
        println!("project\t{}\t{}", name, project_path.display());
    }

    if let Some(dir) = user_env_dir() {
        if dir.exists() {
            for entry in fs::read_dir(&dir)
                .with_context(|| format!("failed to read env dir {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                    let name = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("?");
                    println!("user\t{name}\t{}", path.display());
                }
            }
        }
    }

    println!("built-in\tdefault\t{}", DEFAULT_BASE_IMAGE);
    Ok(())
}

fn image_tag(resolved: &ResolvedEnv) -> String {
    let input = format!(
        "{}\n{}",
        render_dockerfile(&resolved.manifest),
        source_fingerprint(&resolved.source)
    );
    let hash = fnv1a64(input.as_bytes());
    let mut name = resolved
        .manifest
        .name
        .as_deref()
        .unwrap_or("default")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    name = name.trim_matches('-').to_string();
    if name.is_empty() {
        name = "default".to_string();
    }
    format!("pi-sandbox-{name}:{hash:x}")
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn source_fingerprint(source: &EnvSource) -> String {
    match source {
        EnvSource::Project(path) => format!("project:{}", path.display()),
        EnvSource::User(path) => format!("user:{}", path.display()),
        EnvSource::Explicit(path) => format!("explicit:{}", path.display()),
        EnvSource::BuiltIn => "builtin".to_string(),
    }
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = ProcessCommand::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!("{program} failed with status {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_process(invocation: DockerInvocation) -> Result<()> {
    let status = ProcessCommand::new(&invocation.program)
        .args(&invocation.args)
        .status()
        .with_context(|| format!("failed to run {}", invocation.program))?;
    if !status.success() {
        bail!("{} failed with status {status}", invocation.program);
    }
    Ok(())
}

fn run_process_interactive(invocation: DockerInvocation) -> Result<()> {
    let status = ProcessCommand::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run {}", invocation.program))?;
    if !status.success() {
        bail!("{} failed with status {status}", invocation.program);
    }
    Ok(())
}

fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn renders_default_dockerfile_with_pi_install() {
        let dockerfile = render_dockerfile(&EnvManifest::default());

        assert!(dockerfile.contains("FROM node:22-bookworm"));
        assert!(dockerfile.contains(DEFAULT_PI_INSTALL));
        assert!(dockerfile.contains("/tmp/pi-home"));
    }

    #[test]
    fn renders_manifest_packages_env_commands_and_hooks() {
        let manifest = EnvManifest {
            name: Some("rust".to_string()),
            base_image: Some("node:22-bookworm".to_string()),
            default_packages: Some(false),
            packages: vec!["git".to_string(), "curl".to_string()],
            env: BTreeMap::from([("RUSTUP_HOME".to_string(), "/opt/rustup".to_string())]),
            commands: vec!["npm --version".to_string()],
            pi_install: Some(
                "npm install -g --ignore-scripts @earendil-works/pi-coding-agent".to_string(),
            ),
            dockerfile: Some(DockerfileHooks {
                prepend: Some("# syntax=docker/dockerfile:1".to_string()),
                append: Some("RUN true".to_string()),
            }),
        };

        let dockerfile = render_dockerfile(&manifest);

        assert!(dockerfile.starts_with("# syntax=docker/dockerfile:1\nFROM node:22-bookworm"));
        assert!(
            dockerfile.contains(
                "RUN apt-get update && apt-get install -y --no-install-recommends git curl"
            )
        );
        assert!(dockerfile.contains("ENV RUSTUP_HOME=/opt/rustup"));
        assert!(dockerfile.contains("RUN npm --version"));
        assert!(dockerfile.contains("RUN true"));
    }

    #[test]
    fn resolves_project_manifest_before_builtin() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(PROJECT_MANIFEST),
            "name = \"repo\"\nbase_image = \"node:20-bookworm\"\n",
        )
        .unwrap();

        let resolved = resolve_env(None, dir.path()).unwrap();

        assert_eq!(resolved.manifest.name.as_deref(), Some("repo"));
        assert!(matches!(resolved.source, EnvSource::Project(_)));
    }

    #[test]
    fn default_packages_are_included_unless_disabled() {
        let dockerfile = render_dockerfile(&EnvManifest {
            packages: vec!["git".to_string(), "ripgrep".to_string()],
            ..EnvManifest::default()
        });

        assert!(dockerfile.contains("apt-get install -y --no-install-recommends bash"));
        assert!(dockerfile.contains("python3-venv"));
        assert!(dockerfile.contains("ripgrep"));
        assert_eq!(dockerfile.matches(" git").count(), 1);

        let lean = render_dockerfile(&EnvManifest {
            default_packages: Some(false),
            packages: vec!["ripgrep".to_string()],
            ..EnvManifest::default()
        });

        assert!(!lean.contains("build-essential"));
        assert!(lean.contains("apt-get install -y --no-install-recommends ripgrep"));
    }

    #[test]
    fn init_manifest_uses_defaults_from_directory_name() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: None,
            base_image: None,
            default_packages: Some(true),
            packages: Vec::new(),
            commands: Vec::new(),
        };

        let manifest = init_manifest_from_options(dir.path(), &options);

        assert_eq!(
            manifest.name.as_deref(),
            dir.path().file_name().and_then(|name| name.to_str())
        );
        assert_eq!(manifest.base_image.as_deref(), Some(DEFAULT_BASE_IMAGE));
        assert_eq!(manifest.default_packages, None);
        assert!(manifest.packages.is_empty());
    }

    #[test]
    fn write_project_manifest_refuses_overwrite_without_force() {
        let dir = tempdir().unwrap();
        let manifest = EnvManifest {
            name: Some("repo".to_string()),
            base_image: Some(DEFAULT_BASE_IMAGE.to_string()),
            ..EnvManifest::default()
        };

        write_project_manifest(dir.path(), &manifest, false).unwrap();
        let err = write_project_manifest(dir.path(), &manifest, false).unwrap_err();

        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn write_project_manifest_serializes_optional_fields() {
        let dir = tempdir().unwrap();
        let manifest = EnvManifest {
            name: Some("repo".to_string()),
            base_image: Some("node:20-bookworm".to_string()),
            default_packages: Some(false),
            packages: vec!["ripgrep".to_string()],
            commands: vec!["npm --version".to_string()],
            ..EnvManifest::default()
        };

        let path = write_project_manifest(dir.path(), &manifest, false).unwrap();
        let text = fs::read_to_string(path).unwrap();

        assert!(text.contains("name = \"repo\""));
        assert!(text.contains("base_image = \"node:20-bookworm\""));
        assert!(text.contains("default_packages = false"));
        assert!(text.contains("packages = [\"ripgrep\"]"));
        assert!(text.contains("commands = [\"npm --version\"]"));
    }

    #[test]
    fn split_list_accepts_commas_and_whitespace() {
        assert_eq!(
            split_list("ripgrep, fd-find jq"),
            vec!["ripgrep", "fd-find", "jq"]
        );
    }

    #[test]
    fn falls_back_to_builtin_manifest() {
        let dir = tempdir().unwrap();

        let resolved = resolve_env(None, dir.path()).unwrap();

        assert_eq!(resolved.manifest.name.as_deref(), Some("default"));
        assert!(matches!(resolved.source, EnvSource::BuiltIn));
    }

    #[test]
    fn run_invocation_uses_host_identity_and_workspace_mount() {
        let args = run_invocation(
            Path::new("/repo"),
            "pi-sandbox-default:abc",
            "501",
            "20",
            &["--model".to_string(), "fast model".to_string()],
        );

        assert_eq!(args.program, "docker");
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["--user", "501:20"])
        );
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["-e", "HOME=/tmp/pi-home"])
        );
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["-v", "/repo:/workspace"])
        );
        assert!(
            args.args
                .windows(2)
                .any(|pair| pair == ["-w", "/workspace"])
        );
        assert!(args.args.windows(2).any(|pair| pair == ["sh", "-c"]));
        assert!(!args.args.windows(2).any(|pair| pair == ["sh", "-lc"]));
        assert!(
            args.args
                .last()
                .unwrap()
                .contains("exec pi --model 'fast model'")
        );
    }

    #[test]
    fn build_invocation_points_at_cached_dockerfile() {
        let invocation =
            build_invocation(Path::new("/cache/pi-sandbox"), "pi-sandbox-default:abc").unwrap();

        assert_eq!(invocation.program, "docker");
        assert_eq!(
            invocation.args,
            vec![
                "build",
                "-t",
                "pi-sandbox-default:abc",
                "-f",
                "/cache/pi-sandbox/pi-sandbox-default:abc/Dockerfile",
                "/cache/pi-sandbox/pi-sandbox-default:abc",
            ]
        );
    }

    #[test]
    fn image_tag_is_deterministic_and_sanitized() {
        let resolved = ResolvedEnv {
            source: EnvSource::BuiltIn,
            manifest: EnvManifest {
                name: Some("Rust + Node!".to_string()),
                ..EnvManifest::default()
            },
        };

        let first = image_tag(&resolved);
        let second = image_tag(&resolved);

        assert_eq!(first, second);
        assert!(first.starts_with("pi-sandbox-rust---node:"));
    }
}
