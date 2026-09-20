use crate::{DockerInvocation, EnvSource, ResolvedEnv, resolve_parent_env, shell_escape};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum HostAccess {
    #[default]
    Default,
    Desktop,
    Gateway,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub networks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_access: Option<HostAccess>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_files: Option<Vec<PathBuf>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunOverrides {
    pub networks: Option<Vec<String>>,
    pub host_access: Option<HostAccess>,
    pub env_files: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub namespace: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, Args)]
pub struct LaunchArgs {
    /// Existing user-defined Docker network. May be repeated.
    #[arg(long = "network", conflicts_with = "default_network")]
    pub networks: Vec<String>,
    /// Clear inherited configured networks.
    #[arg(long)]
    pub default_network: bool,
    /// Host service name behavior.
    #[arg(long, value_enum)]
    pub host_access: Option<HostAccess>,
    /// Literal KEY=VALUE env file. May be repeated.
    #[arg(long = "env-file")]
    pub env_files: Vec<PathBuf>,
    /// Launch environment KEY=VALUE. May be repeated.
    #[arg(long = "env", value_parser = parse_cli_env)]
    pub env_vars: Vec<(String, String)>,
    /// Runtime namespace for shared home state and resource templates, or `worktree`.
    #[arg(long)]
    pub namespace: Option<String>,
    /// Resolve launch details without Docker or filesystem mutations.
    #[arg(long)]
    pub dry_run: bool,
}

fn parse_cli_env(value: &str) -> std::result::Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_string())?;
    if key.is_empty() {
        return Err("environment key cannot be empty".into());
    }
    Ok((key.into(), value.into()))
}

impl From<LaunchArgs> for RunOverrides {
    fn from(args: LaunchArgs) -> Self {
        Self {
            networks: if args.default_network {
                Some(Vec::new())
            } else if args.networks.is_empty() {
                None
            } else {
                Some(args.networks)
            },
            host_access: args.host_access,
            env_files: args.env_files,
            env: args.env_vars.into_iter().collect(),
            namespace: args.namespace,
            dry_run: args.dry_run,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvProvenance {
    ManifestFile(PathBuf),
    Manifest,
    CliFile(PathBuf),
    Cli,
    Injected,
}

#[derive(Debug, Clone)]
pub struct ResolvedVariable {
    pub value: String,
    pub provenance: EnvProvenance,
}

#[derive(Debug, Clone)]
pub struct GitMounts {
    pub worktree_root: PathBuf,
    pub workspace_cwd: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub separate_git_dir: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedRunConfig {
    pub networks: Vec<String>,
    pub host_access: HostAccess,
    pub namespace: String,
    pub worktree_id: String,
    pub env: BTreeMap<String, ResolvedVariable>,
    pub git: Option<GitMounts>,
}

#[derive(Debug, Clone)]
pub struct RunContext<'a> {
    pub cwd: &'a Path,
    pub home: &'a Path,
    pub image: &'a str,
    pub uid: &'a str,
    pub gid: &'a str,
    pub pi_args: &'a [String],
    pub interactive: bool,
    pub env_file: Option<&'a Path>,
    pub cid_file: Option<&'a Path>,
}

/// Runtime paths and identity needed to render Docker container flags.
#[derive(Debug, Clone)]
pub struct ContainerContext<'a> {
    pub cwd: &'a Path,
    pub home: &'a Path,
    pub uid: &'a str,
    pub gid: &'a str,
    pub env_file: Option<&'a Path>,
}

/// Owns a mode-0600 temporary file understood by `docker --env-file`.
/// The file is automatically unlinked when this value is dropped.
pub struct LaunchEnvFile(NamedTempFile);

pub struct ContainerIdFile(PathBuf);

impl ContainerIdFile {
    pub fn new() -> Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("pi-sandbox-cid-")
            .tempfile()
            .context("failed to reserve container id path")?;
        let path = file.path().to_path_buf();
        file.close()
            .context("failed to prepare container id path")?;
        Ok(Self(path))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn id(&self) -> Option<String> {
        fs::read_to_string(&self.0)
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    }
}

impl Drop for ContainerIdFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl LaunchEnvFile {
    pub fn new(config: &ResolvedRunConfig) -> Result<Self> {
        let mut file = tempfile::Builder::new()
            .prefix("pi-sandbox-env-")
            .tempfile()
            .context("failed to create temporary launch environment")?;
        for (key, variable) in &config.env {
            validate_transport_value(key, &variable.value)?;
            writeln!(file, "{key}={}", variable.value)
                .context("failed to write temporary launch environment")?;
        }
        if let Some(git) = &config.git {
            let container_git_dir = if git.separate_git_dir {
                "/tmp/pi-git-worktree".to_string()
            } else {
                format!(
                    "/tmp/pi-git/{}",
                    git.git_dir
                        .strip_prefix(&git.common_dir)
                        .expect("validated Git layout")
                        .display()
                )
            };
            for (key, value) in [
                ("GIT_DIR", container_git_dir),
                ("GIT_COMMON_DIR", "/tmp/pi-git".into()),
                ("GIT_WORK_TREE", "/workspace".into()),
            ] {
                writeln!(file, "{key}={value}")
                    .context("failed to write temporary launch environment")?;
            }
        }
        file.flush()
            .context("failed to flush temporary launch environment")?;
        Ok(Self(file))
    }
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

fn validate_transport_value(key: &str, value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        bail!(
            "environment value for `{key}` contains a newline, which Docker env files cannot represent"
        );
    }
    Ok(())
}

#[derive(Default)]
struct Accumulated {
    networks: Option<Vec<String>>,
    host_access: Option<HostAccess>,
    env_files: Option<Vec<PathBuf>>,
    env: BTreeMap<String, String>,
    namespace: Option<String>,
}

pub fn resolve_run_config(
    resolved: &ResolvedEnv,
    cwd: &Path,
    overrides: &RunOverrides,
) -> Result<ResolvedRunConfig> {
    let mut accumulated = Accumulated::default();
    let mut ancestors = Vec::new();
    accumulate(resolved, cwd, &mut ancestors, &mut accumulated)?;
    let git = discover_git(cwd)?;
    let identity_root = git
        .as_ref()
        .map(|g| g.worktree_root.as_path())
        .unwrap_or(cwd);
    let canonical = fs::canonicalize(identity_root).unwrap_or_else(|_| identity_root.to_path_buf());
    let worktree_id = format!(
        "wt_{:016x}",
        crate::fnv1a64(canonical.to_string_lossy().as_bytes())
    );
    let requested_namespace = overrides
        .namespace
        .as_ref()
        .or(accumulated.namespace.as_ref())
        .map(String::as_str)
        .unwrap_or("worktree");
    let namespace = if requested_namespace == "worktree" {
        worktree_id.clone()
    } else {
        validate_namespace(requested_namespace)?.to_string()
    };

    let networks = dedupe_and_validate_networks(
        overrides
            .networks
            .clone()
            .or(accumulated.networks)
            .unwrap_or_default(),
    )?;
    let host_access = overrides
        .host_access
        .or(accumulated.host_access)
        .unwrap_or_default();
    let mut env = BTreeMap::new();
    for path in accumulated.env_files.unwrap_or_default() {
        merge_env_file(&mut env, &path, EnvProvenance::ManifestFile(path.clone()))?;
    }
    for (key, value) in accumulated.env {
        insert_checked(
            &mut env,
            key,
            expand_templates(&value, &namespace, &worktree_id)?,
            EnvProvenance::Manifest,
        )?;
    }
    for path in &overrides.env_files {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        merge_env_file(&mut env, &path, EnvProvenance::CliFile(path.clone()))?;
    }
    for (key, value) in &overrides.env {
        insert_checked(&mut env, key.clone(), value.clone(), EnvProvenance::Cli)?;
    }
    insert_checked(
        &mut env,
        "PI_SANDBOX_WORKTREE_ID".into(),
        worktree_id.clone(),
        EnvProvenance::Injected,
    )?;
    insert_checked(
        &mut env,
        "PI_SANDBOX_NAMESPACE".into(),
        namespace.clone(),
        EnvProvenance::Injected,
    )?;
    Ok(ResolvedRunConfig {
        networks,
        host_access,
        namespace,
        worktree_id,
        env,
        git,
    })
}

fn accumulate(
    resolved: &ResolvedEnv,
    cwd: &Path,
    ancestors: &mut Vec<String>,
    out: &mut Accumulated,
) -> Result<()> {
    let fingerprint = format!("{:?}", resolved.source);
    if ancestors.contains(&fingerprint) {
        bail!("environment inheritance cycle while resolving run configuration");
    }
    ancestors.push(fingerprint);
    if let Some(parent) = resolved.manifest.extends.as_deref() {
        let parent = resolve_parent_env(parent, &resolved.source, cwd)?;
        accumulate(&parent, cwd, ancestors, out)?;
    }
    if let Some(run) = &resolved.manifest.run {
        if let Some(networks) = &run.networks {
            out.networks = Some(networks.clone());
        }
        if let Some(mode) = run.host_access {
            out.host_access = Some(mode);
        }
        if let Some(files) = &run.env_files {
            let base = source_dir(&resolved.source, cwd);
            out.env_files = Some(
                files
                    .iter()
                    .map(|p| {
                        if p.is_absolute() {
                            p.clone()
                        } else {
                            base.join(p)
                        }
                    })
                    .collect(),
            );
        }
        out.env.extend(run.env.clone());
        if let Some(namespace) = &run.namespace {
            out.namespace = Some(namespace.clone());
        }
    }
    ancestors.pop();
    Ok(())
}

fn source_dir(source: &EnvSource, cwd: &Path) -> PathBuf {
    match source {
        EnvSource::Project(p) | EnvSource::User(p) | EnvSource::Explicit(p) => {
            p.parent().unwrap_or(cwd).to_path_buf()
        }
        EnvSource::BuiltIn => cwd.to_path_buf(),
    }
}

fn validate_namespace(value: &str) -> Result<&str> {
    let valid = value.len() <= 40
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid {
        bail!("invalid namespace `{value}`; expected [a-z][a-z0-9_]{{0,39}} or `worktree`");
    }
    Ok(value)
}

fn dedupe_and_validate_networks(values: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for value in values {
        if value == "host"
            || value == "none"
            || value == "bridge"
            || value.starts_with("container:")
        {
            bail!("network `{value}` is not a named user-defined network");
        }
        if value.is_empty() {
            bail!("network name cannot be empty");
        }
        if seen.insert(value.clone()) {
            result.push(value);
        }
    }
    Ok(result)
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn reserved(key: &str) -> bool {
    key == "HOME"
        || key.starts_with("PI_SANDBOX_")
        || matches!(
            key,
            "GIT_DIR"
                | "GIT_COMMON_DIR"
                | "GIT_WORK_TREE"
                | "GIT_INDEX_FILE"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
        )
}

fn insert_checked(
    env: &mut BTreeMap<String, ResolvedVariable>,
    key: String,
    value: String,
    provenance: EnvProvenance,
) -> Result<()> {
    if !valid_env_key(&key) {
        bail!("invalid environment key `{key}`");
    }
    if reserved(&key) && provenance != EnvProvenance::Injected {
        bail!("environment key `{key}` is reserved");
    }
    env.insert(key, ResolvedVariable { value, provenance });
    Ok(())
}

fn merge_env_file(
    env: &mut BTreeMap<String, ResolvedVariable>,
    path: &Path,
    provenance: EnvProvenance,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "invalid env file {}:{}: expected KEY=VALUE",
                path.display(),
                index + 1
            )
        })?;
        if !valid_env_key(key) {
            bail!(
                "invalid environment key in {}:{}: `{key}`",
                path.display(),
                index + 1
            );
        }
        insert_checked(env, key.to_string(), value.to_string(), provenance.clone())
            .with_context(|| format!("in {}:{}", path.display(), index + 1))?;
    }
    Ok(())
}

fn expand_templates(value: &str, namespace: &str, worktree_id: &str) -> Result<String> {
    let expanded = value
        .replace("{{namespace}}", namespace)
        .replace("{{worktree_id}}", worktree_id);
    if let Some(start) = expanded.find("{{")
        && expanded[start + 2..].contains("}}")
    {
        bail!("unknown run environment template (only namespace and worktree_id are supported)");
    }
    Ok(expanded)
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to inspect Git worktree")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).context("Git returned non-UTF-8 path")?;
    let path = PathBuf::from(value.trim());
    Ok(Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }))
}

fn discover_git(cwd: &Path) -> Result<Option<GitMounts>> {
    let Some(worktree_root) = git_output(cwd, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };
    let git_dir = git_output(cwd, &["rev-parse", "--absolute-git-dir"])?
        .context("Git did not return its directory")?;
    let common_dir = git_output(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?
    .context("Git did not return its common directory")?;
    let worktree_root =
        fs::canonicalize(worktree_root).context("failed to resolve Git worktree root")?;
    let cwd_canonical = fs::canonicalize(cwd).context("failed to resolve working directory")?;
    let workspace_cwd = cwd_canonical
        .strip_prefix(&worktree_root)
        .context("working directory is outside Git worktree")?
        .to_path_buf();
    let git_dir = fs::canonicalize(git_dir).context("failed to resolve Git directory")?;
    let common_dir =
        fs::canonicalize(common_dir).context("failed to resolve common Git directory")?;
    let separate_git_dir = !git_dir.starts_with(&common_dir);
    Ok(Some(GitMounts {
        worktree_root,
        workspace_cwd,
        git_dir,
        common_dir,
        separate_git_dir,
    }))
}

pub fn run_invocation(context: &RunContext<'_>, config: &ResolvedRunConfig) -> DockerInvocation {
    let mut args = vec!["run".into(), "--rm".into()];
    if context.interactive {
        args.push("-it".into());
    }
    if let Some(path) = context.cid_file {
        args.extend(["--cidfile".into(), path.display().to_string()]);
    }
    config.append_docker_create_args(
        &mut args,
        &ContainerContext {
            cwd: context.cwd,
            home: context.home,
            uid: context.uid,
            gid: context.gid,
            env_file: context.env_file,
        },
    );
    args.extend([context.image.into(), "sh".into(), "-c".into()]);
    let suffix = context
        .pi_args
        .iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");
    args.push(if suffix.is_empty() { "mkdir -p /tmp/pi-home && git config --global --add safe.directory /workspace 2>/dev/null || true; exec pi".into() } else { format!("mkdir -p /tmp/pi-home && git config --global --add safe.directory /workspace 2>/dev/null || true; exec pi {suffix}") });
    DockerInvocation {
        program: "docker".into(),
        args,
    }
}

impl ResolvedRunConfig {
    pub fn append_docker_create_args(
        &self,
        args: &mut Vec<String>,
        context: &ContainerContext<'_>,
    ) {
        let workspace = self
            .git
            .as_ref()
            .map(|g| g.worktree_root.as_path())
            .unwrap_or(context.cwd);
        let workdir = self
            .git
            .as_ref()
            .map(|g| {
                if g.workspace_cwd.as_os_str().is_empty() {
                    "/workspace".into()
                } else {
                    format!("/workspace/{}", g.workspace_cwd.display())
                }
            })
            .unwrap_or_else(|| "/workspace".into());
        args.extend([
            "--user".into(),
            format!("{}:{}", context.uid, context.gid),
            "--env".into(),
            "HOME=/tmp/pi-home".into(),
        ]);
        for network in &self.networks {
            args.extend(["--network".into(), network.clone()]);
        }
        if self.host_access == HostAccess::Gateway {
            args.extend([
                "--add-host".into(),
                "host.docker.internal:host-gateway".into(),
            ]);
        }
        if let Some(path) = context.env_file {
            args.extend(["--env-file".into(), path.display().to_string()]);
        }
        args.extend([
            "--volume".into(),
            format!("{}:/workspace", workspace.display()),
            "--volume".into(),
            format!("{}:/tmp/pi-home", context.home.display()),
        ]);
        if let Some(git) = &self.git {
            args.extend([
                "--volume".into(),
                format!("{}:/tmp/pi-git", git.common_dir.display()),
            ]);
            let container_git_dir = if git.separate_git_dir {
                args.extend([
                    "--volume".into(),
                    format!("{}:/tmp/pi-git-worktree", git.git_dir.display()),
                ]);
                "/tmp/pi-git-worktree".to_string()
            } else {
                format!(
                    "/tmp/pi-git/{}",
                    git.git_dir.strip_prefix(&git.common_dir).unwrap().display()
                )
            };
            let _ = container_git_dir; // values are transported through the protected env file
        }
        args.extend(["--workdir".into(), workdir]);
    }
}

pub fn preflight_networks(config: &ResolvedRunConfig) -> Result<()> {
    for network in &config.networks {
        let status = Command::new("docker")
            .args(["network", "inspect", network])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to inspect Docker networks")?;
        if !status.success() {
            bail!(
                "Docker network `{network}` does not exist in the active context; select an existing network"
            );
        }
    }
    Ok(())
}

pub fn print_dry_run(image: &str, config: &ResolvedRunConfig, home: &crate::HomeIdentity) {
    println!("image: {image}");
    println!(
        "namespace: {} ({})",
        config.namespace,
        if home.shared {
            "explicit, shared by linked worktrees"
        } else {
            "worktree default, isolated"
        }
    );
    println!("worktree_id: {}", config.worktree_id);
    println!("host_access: {:?}", config.host_access);
    println!(
        "networks (unverified): {}",
        if config.networks.is_empty() {
            "<default>".into()
        } else {
            config.networks.join(", ")
        }
    );
    if let Some(git) = &config.git {
        println!(
            "workspace mount: {} -> /workspace",
            git.worktree_root.display()
        );
        println!(
            "git common mount: {} -> /tmp/pi-git",
            git.common_dir.display()
        );
        if git.separate_git_dir {
            println!(
                "git worktree mount: {} -> /tmp/pi-git-worktree",
                git.git_dir.display()
            );
        }
    }
    println!(
        "persistent home mount: {} -> /tmp/pi-home ({})",
        home.path.display(),
        crate::describe_home_state(&home.path)
    );
    println!("environment (values redacted):");
    for (key, variable) in &config.env {
        println!("  {key}=<redacted> ({:?})", variable.provenance);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve_env;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn resolve_at(dir: &Path, overrides: RunOverrides) -> Result<ResolvedRunConfig> {
        let resolved = resolve_env(None, dir)?;
        resolve_run_config(&resolved, dir, &overrides)
    }

    #[test]
    fn resolves_inheritance_paths_and_precedence() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("parent")).unwrap();
        fs::write(
            dir.path().join("parent/base.env"),
            "A=file\r\nB=parent=file\r\n",
        )
        .unwrap();
        fs::write(dir.path().join("cli.env"), "B=cli-file\nC=\n").unwrap();
        fs::write(
            dir.path().join("parent/base.toml"),
            r#"
base_image = "node:22-bookworm"
[run]
networks = ["one", "one", "two"]
env_files = ["base.env"]
[run.env]
B = "manifest"
T = "db_{{namespace}}_{{worktree_id}}"
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join(".pi-env.toml"),
            r#"
extends = "parent/base.toml"
[run.env]
B = "child"
"#,
        )
        .unwrap();
        let config = resolve_at(
            dir.path(),
            RunOverrides {
                env_files: vec!["cli.env".into()],
                env: BTreeMap::from([("B".into(), "cli".into())]),
                namespace: Some("feature_1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(config.networks, ["one", "two"]);
        assert_eq!(config.env["A"].value, "file");
        assert_eq!(config.env["B"].value, "cli");
        assert_eq!(config.env["C"].value, "");
        assert_eq!(
            config.env["T"].value,
            format!("db_feature_1_{}", config.worktree_id)
        );
    }

    #[test]
    fn explicit_empty_child_lists_clear_parent() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("base.toml"),
            "[run]\nnetworks=[\"one\"]\nenv_files=[\"missing\"]\n",
        )
        .unwrap();
        fs::write(
            dir.path().join(".pi-env.toml"),
            "extends=\"base.toml\"\n[run]\nnetworks=[]\nenv_files=[]\n",
        )
        .unwrap();
        let config = resolve_at(dir.path(), RunOverrides::default()).unwrap();
        assert!(config.networks.is_empty());
    }

    #[test]
    fn rejects_reserved_invalid_and_unknown_templates_without_values() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(".pi-env.toml"),
            "[run.env]\nHOME=\"very-secret\"\n",
        )
        .unwrap();
        let error = resolve_at(dir.path(), RunOverrides::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("HOME"));
        assert!(!error.contains("very-secret"));
        fs::write(
            dir.path().join(".pi-env.toml"),
            "[run.env]\nOK=\"secret-{{other}}\"\n",
        )
        .unwrap();
        let error = resolve_at(dir.path(), RunOverrides::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown"));
        assert!(!error.contains("secret"));
    }

    #[test]
    fn env_file_parser_is_literal_and_reports_lines() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("values"),
            " # ignored\nQ=\"x # $y\"\nE=a=b\nBAD KEY=x\n",
        )
        .unwrap();
        fs::write(
            dir.path().join(".pi-env.toml"),
            "[run]\nenv_files=[\"values\"]\n",
        )
        .unwrap();
        let error = resolve_at(dir.path(), RunOverrides::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("values:4"));
        assert!(!error.contains("=x"));
        fs::write(dir.path().join("values"), "Q=\"x # $y\"\nE=a=b\n").unwrap();
        let config = resolve_at(dir.path(), RunOverrides::default()).unwrap();
        assert_eq!(config.env["Q"].value, "\"x # $y\"");
        assert_eq!(config.env["E"].value, "a=b");
    }

    #[test]
    fn validates_namespaces_and_network_modes() {
        let dir = tempdir().unwrap();
        for value in ["Bad", "a-b", "1bad", &"a".repeat(41)] {
            assert!(
                resolve_at(
                    dir.path(),
                    RunOverrides {
                        namespace: Some(value.into()),
                        ..Default::default()
                    }
                )
                .is_err()
            );
        }
        for value in ["host", "none", "bridge", "container:x", ""] {
            assert!(
                resolve_at(
                    dir.path(),
                    RunOverrides {
                        networks: Some(vec![value.into()]),
                        ..Default::default()
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn create_flags_precede_image_and_gateway_is_explicit() {
        let dir = tempdir().unwrap();
        let config = resolve_at(
            dir.path(),
            RunOverrides {
                networks: Some(vec!["db".into(), "cache".into()]),
                host_access: Some(HostAccess::Gateway),
                ..Default::default()
            },
        )
        .unwrap();
        let env_file = LaunchEnvFile::new(&config).unwrap();
        let mut args = vec!["create".into()];
        config.append_docker_create_args(
            &mut args,
            &ContainerContext {
                cwd: dir.path(),
                home: dir.path(),
                uid: "1",
                gid: "2",
                env_file: Some(env_file.path()),
            },
        );
        args.push("image".into());
        assert_eq!(args.iter().filter(|a| a.as_str() == "--network").count(), 2);
        assert!(
            args.windows(2)
                .any(|p| p == ["--add-host", "host.docker.internal:host-gateway"])
        );
        assert_eq!(args.last().unwrap(), "image");
        assert!(!args.iter().any(|a| a.contains(&config.namespace)));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(env_file.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn worktree_identity_is_stable_and_git_paths_are_derived() {
        let repo = tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let first = resolve_at(repo.path(), RunOverrides::default()).unwrap();
        let second = resolve_at(repo.path(), RunOverrides::default()).unwrap();
        assert_eq!(first.worktree_id, second.worktree_id);
        assert_eq!(first.worktree_id.len(), 19);
        assert_eq!(
            first.git.unwrap().worktree_root,
            fs::canonicalize(repo.path()).unwrap()
        );
    }
}
