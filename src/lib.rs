use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub mod launch;
pub use launch::{
    ContainerContext, ContainerIdFile, HostAccess, LaunchArgs, LaunchEnvFile, ResolvedRunConfig,
    RunConfig, RunContext, RunOverrides,
};

const PROJECT_MANIFEST: &str = ".pi-env.toml";
const DEFAULT_BASE_IMAGE: &str = "node:22-bookworm";
const DEFAULT_PI_INSTALL: &str =
    "npm install -g --ignore-scripts @earendil-works/pi-coding-agent@0.85.1";

pub mod headless;
pub mod skills;
const DEFAULT_PACKAGES: &[&str] = &[
    "bash",
    "ca-certificates",
    "curl",
    "wget",
    "git",
    "gh",
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
const RUST_PATH: &str =
    "/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const RUST_INSTALL_COMMAND: &str =
    "mkdir -p /opt/cargo /opt/rustup && chmod -R 0777 /opt/cargo /opt/rustup";
const RUSTUP_COMMAND: &str = "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup sh -s -- -y --profile minimal --no-modify-path";
const RUST_COMPONENTS_COMMAND: &str = "CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup /opt/cargo/bin/rustup component add rustfmt clippy";
const RUST_PERMISSIONS_COMMAND: &str = "chmod -R a+rX /opt/cargo /opt/rustup";
const UV_VERSION: &str = "0.12.0";
/// Shared build cache inside the persistent home so linked worktrees using one
/// namespace reuse a single compiled graph instead of rebuilding per worktree.
const RUST_TARGET_DIR: &str = "/tmp/pi-home/.cache/cargo-target";
const RAILS_RUBY_IMAGE: &str = "ruby:4.0.1-bookworm";
const RAILS_BASE_STAGE: &str = "rails-ruby-node";
const RAILS_NODE_STAGE: &str = "pi-node-runtime";
const RAILS_BUNDLE_PATH: &str = "/tmp/pi-home/bundle";
const RAILS_PACKAGES: &[&str] = &[
    "libpq-dev",
    "postgresql-client",
    "libvips",
    "sqlite3",
    "libsqlite3-dev",
];
const TAURI_PACKAGES: &[&str] = &[
    "file",
    "libssl-dev",
    "libgtk-3-dev",
    "libayatana-appindicator3-dev",
    "librsvg2-dev",
    "libwebkit2gtk-4.1-dev",
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
        rebuild: bool,
        pull: bool,
    },
    Env {
        command: EnvCommand,
    },
    Init(InitOptions),
    Run {
        env: Option<String>,
        rebuild: bool,
        pull: bool,
        pi_args: Vec<String>,
        overrides: RunOverrides,
    },
    Headless(headless::HeadlessCommand),
    Skills {
        command: SkillsCommand,
    },
    Namespace {
        command: NamespaceCommand,
    },
}

#[derive(Debug)]
pub enum NamespaceCommand {
    Show {
        env: Option<String>,
        namespace: Option<String>,
        json: bool,
    },
    List {
        json: bool,
    },
    Adopt {
        env: Option<String>,
        namespace: Option<String>,
        from: PathBuf,
        force: bool,
    },
}

#[derive(Debug)]
pub enum EnvCommand {
    List,
    Show { env: Option<String> },
}

#[derive(Debug)]
pub enum SkillsCommand {
    Install(skills::InstallOptions),
}

#[derive(Debug)]
pub struct InitOptions {
    pub yes: bool,
    pub force: bool,
    pub name: Option<String>,
    pub base_image: Option<String>,
    pub extends: Option<String>,
    pub git_user_name: Option<String>,
    pub git_user_email: Option<String>,
    pub default_packages: Option<bool>,
    pub presets: Vec<String>,
    pub packages: Vec<String>,
    pub commands: Vec<String>,
    pub namespace: Option<String>,
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
    pub extends: Option<String>,
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_env: BTreeMap<String, String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_install: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<DockerfileHooks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_email: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedImage {
    tag: String,
    dockerfile: String,
}

#[derive(Debug, Clone)]
struct PreparedEnvironment {
    images: Vec<PreparedImage>,
    git: Option<GitConfig>,
}

pub fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Build { env, rebuild, pull } => {
            let resolved = resolve_env(env.as_deref(), &cli.cwd)?;
            let cache = cache_dir()?;
            let prepared = prepare_environment(&resolved, &cli.cwd)?;
            build_images(&cache, &prepared.images, rebuild, pull)?;
        }
        Command::Env { command } => match command {
            EnvCommand::List => list_envs(&cli.cwd)?,
            EnvCommand::Show { env } => {
                let resolved = resolve_env(env.as_deref(), &cli.cwd)?;
                let prepared = prepare_environment(&resolved, &cli.cwd)?;
                show_images(&prepared.images);
            }
        },
        Command::Init(options) => {
            init_manifest_interactive(&cli.cwd, options)?;
        }
        Command::Run {
            env,
            rebuild,
            pull,
            pi_args,
            overrides,
        } => {
            // Install before resolving credentials or creating their temporary file.
            let signals = SignalState::install()?;
            let resolved = resolve_env(env.as_deref(), &cli.cwd)?;
            let prepared = prepare_environment(&resolved, &cli.cwd)?;
            let launch = launch::resolve_run_config(&resolved, &cli.cwd, &overrides)?;
            let tag = &prepared
                .images
                .last()
                .context("resolved environment did not produce an image")?
                .tag;
            let identity = persistent_home_identity(&cli.cwd, &resolved, &launch)?;
            if overrides.dry_run {
                launch::print_dry_run(tag, &launch, &identity);
                return Ok(());
            }
            launch::preflight_networks(&launch)?;
            let cache = cache_dir()?;
            build_images(&cache, &prepared.images, rebuild, pull)?;

            let uid = command_output("id", &["-u"])?;
            let gid = command_output("id", &["-g"])?;
            let home = persistent_home_dir_for_run(&cli.cwd, &resolved, &launch)?;
            seed_git_config(&home, prepared.git.as_ref())?;
            let env_file = LaunchEnvFile::new(&launch)?;
            let cid_file = ContainerIdFile::new()?;
            let context = RunContext {
                cwd: &cli.cwd,
                home: &home,
                image: tag,
                uid: &uid,
                gid: &gid,
                pi_args: &pi_args,
                interactive: true,
                env_file: Some(env_file.path()),
                cid_file: Some(cid_file.path()),
            };
            run_process_interactive(
                launch::run_invocation(&context, &launch),
                &signals,
                &cid_file,
            )?;
        }
        Command::Headless(command) => {
            headless::run(command, &cli.cwd)?;
        }
        Command::Skills { command } => match command {
            SkillsCommand::Install(options) => skills::install(&cli.cwd, options)?,
        },
        Command::Namespace { command } => match command {
            NamespaceCommand::Show {
                env,
                namespace,
                json,
            } => namespace_show(&cli.cwd, env.as_deref(), namespace, json)?,
            NamespaceCommand::List { json } => namespace_list(json)?,
            NamespaceCommand::Adopt {
                env,
                namespace,
                from,
                force,
            } => namespace_adopt(&cli.cwd, env.as_deref(), namespace, &from, force)?,
        },
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct NamespaceReport {
    namespace: String,
    shared: bool,
    worktree_id: String,
    persistent_home: PathBuf,
    home_exists: bool,
    pi_credentials: bool,
    repository: PathBuf,
    manifest_name: String,
    manifest_source: String,
    /// Linked worktrees that resolve to this same home, when the namespace is shared.
    sharing_worktrees: Vec<PathBuf>,
}

fn namespace_show(
    cwd: &Path,
    env: Option<&str>,
    namespace: Option<String>,
    json: bool,
) -> Result<()> {
    let resolved = resolve_env(env, cwd)?;
    let overrides = RunOverrides {
        namespace,
        ..RunOverrides::default()
    };
    let launch = launch::resolve_run_config(&resolved, cwd, &overrides)?;
    let identity = persistent_home_identity(cwd, &resolved, &launch)?;
    let sharing_worktrees = if identity.shared {
        linked_worktrees(cwd)?
    } else {
        Vec::new()
    };
    let report = NamespaceReport {
        namespace: identity.namespace.clone(),
        shared: identity.shared,
        worktree_id: identity.worktree_id.clone(),
        home_exists: identity.path.is_dir(),
        pi_credentials: home_has_pi_credentials(&identity.path),
        persistent_home: identity.path.clone(),
        repository: identity.repository.clone(),
        manifest_name: identity.manifest_name.clone(),
        manifest_source: identity.manifest_source.clone(),
        sharing_worktrees,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!(
        "namespace: {} ({})",
        report.namespace,
        if report.shared {
            "explicit, shared by linked worktrees"
        } else {
            "worktree default, isolated per worktree"
        }
    );
    println!("worktree_id: {}", report.worktree_id);
    println!(
        "persistent home: {} ({})",
        report.persistent_home.display(),
        describe_home_state(&report.persistent_home)
    );
    println!("repository: {}", report.repository.display());
    println!(
        "manifest: {} ({})",
        report.manifest_name, report.manifest_source
    );
    if report.shared {
        println!("sharing worktrees:");
        for path in &report.sharing_worktrees {
            println!("  {}", path.display());
        }
    }
    if !report.pi_credentials {
        println!("hint: {}", authenticate_hint(&identity));
    }
    Ok(())
}

fn linked_worktrees(cwd: &Path) -> Result<Vec<PathBuf>> {
    let output = ProcessCommand::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
        .context("failed to run git worktree list")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

/// Pi state worth carrying between homes: credentials plus the settings that
/// select providers and models. Caches and installed packages are left behind.
const ADOPTABLE_PI_FILES: &[&str] = &[
    PI_AUTH_FILE,
    ".pi/agent/settings.json",
    ".pi/agent/models-store.json",
];

fn namespace_adopt(
    cwd: &Path,
    env: Option<&str>,
    namespace: Option<String>,
    from: &Path,
    force: bool,
) -> Result<()> {
    let source = if from.is_absolute() {
        from.to_path_buf()
    } else {
        cwd.join(from)
    };
    let source = fs::canonicalize(&source)
        .with_context(|| format!("source home does not exist: {}", source.display()))?;
    if !source.join(PI_AUTH_FILE).is_file() {
        bail!(
            "{} holds no pi credentials ({PI_AUTH_FILE} is missing)",
            source.display()
        );
    }
    let resolved = resolve_env(env, cwd)?;
    let overrides = RunOverrides {
        namespace,
        ..RunOverrides::default()
    };
    let launch = launch::resolve_run_config(&resolved, cwd, &overrides)?;
    let base = homes_dir()?;
    let identity = resolve_home_identity(&base, cwd, &resolved, &launch);
    let destination = ensure_home(&base, cwd, &resolved, &identity)?;
    let destination_canonical =
        fs::canonicalize(&destination).unwrap_or_else(|_| destination.clone());
    if destination_canonical == source {
        bail!(
            "{} is already the home for namespace {}",
            source.display(),
            identity.namespace
        );
    }
    if home_has_pi_credentials(&destination) && !force {
        bail!(
            "{} already holds pi credentials; pass --force to replace them",
            destination.display()
        );
    }
    let mut copied = Vec::new();
    for relative in ADOPTABLE_PI_FILES {
        let from_file = source.join(relative);
        if !from_file.is_file() {
            continue;
        }
        let to_file = destination.join(relative);
        if let Some(parent) = to_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&from_file, &to_file).with_context(|| format!("failed to copy {relative}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&to_file, fs::Permissions::from_mode(0o600))?;
        }
        copied.push(*relative);
    }
    println!(
        "adopted {} into {} (namespace {})",
        copied.join(", "),
        destination.display(),
        identity.namespace
    );
    println!("source left untouched: {}", source.display());
    Ok(())
}

#[derive(Debug, Serialize)]
struct HomeListing {
    path: PathBuf,
    pi_credentials: bool,
    metadata: Option<HomeMetadata>,
}

fn namespace_list(json: bool) -> Result<()> {
    let base = homes_dir()?;
    let mut listings = Vec::new();
    if base.is_dir() {
        for entry in
            fs::read_dir(&base).with_context(|| format!("failed to read {}", base.display()))?
        {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let metadata = fs::read(path.join(HOME_METADATA_FILE))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<HomeMetadata>(&bytes).ok());
            listings.push(HomeListing {
                pi_credentials: home_has_pi_credentials(&path),
                path,
                metadata,
            });
        }
    }
    listings.sort_by(|a, b| a.path.cmp(&b.path));
    if json {
        println!("{}", serde_json::to_string_pretty(&listings)?);
        return Ok(());
    }
    println!("homes directory: {}", base.display());
    for listing in &listings {
        let credentials = if listing.pi_credentials {
            "credentials"
        } else {
            "no credentials"
        };
        match &listing.metadata {
            Some(meta) => println!(
                "{}\t{}\t{}\t{}\t{}",
                listing
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                if meta.shared {
                    format!("namespace {}", meta.namespace)
                } else {
                    "worktree default".to_string()
                },
                meta.repository.display(),
                meta.manifest_name,
                credentials
            ),
            None => println!(
                "{}\tlegacy (no metadata)\t?\t?\t{}",
                listing
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                credentials
            ),
        }
    }
    Ok(())
}

pub fn init_manifest_interactive(cwd: &Path, options: InitOptions) -> Result<PathBuf> {
    let manifest = if options.yes {
        init_manifest_from_options(cwd, &options)?
    } else {
        prompt_for_manifest(cwd, &options)?
    };

    let path = write_project_manifest(cwd, &manifest, options.force)?;
    println!("Wrote {}", path.display());
    println!("Next: pi-sandbox run");
    Ok(path)
}

pub fn init_manifest_from_options(cwd: &Path, options: &InitOptions) -> Result<EnvManifest> {
    validate_preset_options(options)?;
    let mut manifest = EnvManifest {
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
        )
        .filter(|_| options.extends.is_none()),
        extends: options.extends.clone(),
        default_packages: options
            .default_packages
            .and_then(|enabled| (!enabled).then_some(false)),
        ..EnvManifest::default()
    };

    apply_presets(&mut manifest, &options.presets)?;
    extend_unique(&mut manifest.packages, options.packages.clone());
    extend_unique(&mut manifest.commands, options.commands.clone());
    manifest.git = git_config_from_options(options);
    set_manifest_namespace(&mut manifest, options.namespace.as_deref())?;
    Ok(manifest)
}

/// Declare a shared persistent-home namespace in `[run]`; `worktree` or blank keeps
/// the default per-worktree homes and writes nothing.
fn set_manifest_namespace(manifest: &mut EnvManifest, namespace: Option<&str>) -> Result<()> {
    let Some(namespace) = namespace.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(());
    };
    if namespace == "worktree" {
        return Ok(());
    }
    launch::validate_namespace(namespace)?;
    manifest
        .run
        .get_or_insert_with(RunConfig::default)
        .namespace = Some(namespace.to_string());
    Ok(())
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
    let base_image = if options.extends.is_some() {
        None
    } else {
        Some(
            options
                .base_image
                .clone()
                .map(Ok)
                .unwrap_or_else(|| prompt_default("Base image", DEFAULT_BASE_IMAGE))?,
        )
    };
    let default_packages = match options.default_packages {
        Some(value) => value,
        None => prompt_yes_no("Include default packages", true)?,
    };
    let mut presets = options.presets.clone();
    presets.extend(prompt_list("Stack presets")?);
    if preset_selected(&presets, "rails") && options.base_image.is_some() {
        bail!("the rails preset cannot be combined with `--base-image`");
    }

    let mut manifest = EnvManifest {
        name: Some(name),
        extends: options.extends.clone(),
        base_image,
        default_packages: (!default_packages).then_some(false),
        ..EnvManifest::default()
    };
    apply_presets(&mut manifest, &presets)?;

    extend_unique(&mut manifest.packages, options.packages.clone());
    extend_unique(
        &mut manifest.packages,
        prompt_list("Additional apt packages")?,
    );

    extend_unique(&mut manifest.commands, options.commands.clone());
    extend_unique(&mut manifest.commands, prompt_repeated("Setup command")?);
    manifest.git = git_config_from_options(options);
    let namespace = match &options.namespace {
        Some(namespace) => namespace.clone(),
        None => prompt_default(
            "Persistent home namespace shared by linked worktrees (`worktree` keeps one home per worktree)",
            "worktree",
        )?,
    };
    set_manifest_namespace(&mut manifest, Some(&namespace))?;

    Ok(manifest)
}

fn git_config_from_options(options: &InitOptions) -> Option<GitConfig> {
    if options.git_user_name.is_none() && options.git_user_email.is_none() {
        None
    } else {
        Some(GitConfig {
            user_name: options.git_user_name.clone(),
            user_email: options.git_user_email.clone(),
        })
    }
}

fn validate_preset_options(options: &InitOptions) -> Result<()> {
    if preset_selected(&options.presets, "rails") && options.base_image.is_some() {
        bail!("the rails preset cannot be combined with `--base-image`");
    }
    Ok(())
}

fn preset_selected(presets: &[String], expected: &str) -> bool {
    expand_presets(presets)
        .iter()
        .any(|preset| preset == expected)
}

fn apply_presets(manifest: &mut EnvManifest, presets: &[String]) -> Result<()> {
    for preset in expand_presets(presets) {
        match preset.as_str() {
            "node" => {
                if manifest.base_image.is_none() && manifest.extends.is_none() {
                    manifest.base_image = Some(DEFAULT_BASE_IMAGE.to_string());
                }
            }
            "python" => {
                extend_unique(
                    &mut manifest.packages,
                    [
                        "ca-certificates",
                        "curl",
                        "python3",
                        "python3-pip",
                        "python3-venv",
                    ]
                    .into_iter()
                    .map(ToString::to_string),
                );
                push_unique(&mut manifest.commands, uv_install_command());
            }
            "rust" => {
                extend_unique(
                    &mut manifest.commands,
                    [
                        RUST_INSTALL_COMMAND,
                        RUSTUP_COMMAND,
                        RUST_COMPONENTS_COMMAND,
                        RUST_PERMISSIONS_COMMAND,
                    ]
                    .into_iter()
                    .map(ToString::to_string),
                );
                manifest
                    .runtime_env
                    .insert("CARGO_HOME".to_string(), "/opt/cargo".to_string());
                manifest
                    .runtime_env
                    .insert("RUSTUP_HOME".to_string(), "/opt/rustup".to_string());
                manifest
                    .runtime_env
                    .insert("PATH".to_string(), RUST_PATH.to_string());
                manifest
                    .run
                    .get_or_insert_with(RunConfig::default)
                    .env
                    .entry("CARGO_TARGET_DIR".to_string())
                    .or_insert_with(|| RUST_TARGET_DIR.to_string());
            }
            "rails" => {
                if manifest.extends.is_some() {
                    bail!("the rails preset cannot be combined with `extends`");
                }
                if manifest.base_image.as_deref().is_some_and(|base_image| {
                    base_image != DEFAULT_BASE_IMAGE && base_image != RAILS_BASE_STAGE
                }) {
                    bail!(
                        "the rails preset selects `{RAILS_RUBY_IMAGE}` and cannot be combined with a custom base image"
                    );
                }
                manifest.base_image = Some(RAILS_BASE_STAGE.to_string());
                extend_unique(
                    &mut manifest.packages,
                    RAILS_PACKAGES.iter().map(|package| package.to_string()),
                );
                manifest
                    .runtime_env
                    .insert("BUNDLE_PATH".to_string(), RAILS_BUNDLE_PATH.to_string());
                manifest
                    .runtime_env
                    .insert("BUNDLE_JOBS".to_string(), "4".to_string());
                manifest
                    .runtime_env
                    .insert("BUNDLE_RETRY".to_string(), "3".to_string());
                manifest.dockerfile = Some(DockerfileHooks {
                    prepend: Some(rails_dockerfile_prelude()),
                    append: None,
                });
            }
            "tauri" => {
                extend_unique(
                    &mut manifest.packages,
                    TAURI_PACKAGES.iter().map(|package| package.to_string()),
                );
            }
            _ => bail!(
                "unknown preset `{preset}`; expected one of: node, python, rails, rust, tauri"
            ),
        }
    }
    Ok(())
}

fn rails_dockerfile_prelude() -> String {
    format!(
        "# syntax=docker/dockerfile:1\n\
FROM {DEFAULT_BASE_IMAGE} AS {RAILS_NODE_STAGE}\n\
FROM {RAILS_RUBY_IMAGE} AS {RAILS_BASE_STAGE}\n\
COPY --from={RAILS_NODE_STAGE} /usr/local/bin/node /usr/local/bin/node\n\
COPY --from={RAILS_NODE_STAGE} /usr/local/lib/node_modules /usr/local/lib/node_modules\n\
COPY --from={RAILS_NODE_STAGE} /usr/local/include/node /usr/local/include/node\n\
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx && ln -s ../lib/node_modules/corepack/dist/corepack.js /usr/local/bin/corepack && corepack enable"
    )
}

fn uv_install_command() -> String {
    format!(
        "curl --proto '=https' --tlsv1.2 -LsSf https://releases.astral.sh/github/uv/releases/download/{UV_VERSION}/uv-installer.sh | env UV_UNMANAGED_INSTALL=/usr/local/bin sh"
    )
}

fn expand_presets(presets: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for preset in presets {
        match normalize_preset(preset).as_deref() {
            Some("tauri") => {
                push_unique(&mut expanded, "node".to_string());
                push_unique(&mut expanded, "rust".to_string());
                push_unique(&mut expanded, "tauri".to_string());
            }
            Some(name) => push_unique(&mut expanded, name.to_string()),
            None => {}
        }
    }
    expanded
}

fn normalize_preset(preset: &str) -> Option<String> {
    let normalized = preset.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn extend_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for value in values {
        push_unique(target, value);
    }
}

fn push_unique(target: &mut Vec<String>, value: String) {
    if !target.contains(&value) {
        target.push(value);
    }
}

pub fn resolve_env(env: Option<&str>, cwd: &Path) -> Result<ResolvedEnv> {
    if let Some(env) = env {
        let candidate = if Path::new(env).is_absolute() {
            PathBuf::from(env)
        } else {
            cwd.join(env)
        };
        if candidate.exists() {
            return load_manifest(candidate, EnvSource::Explicit);
        }

        if let Some(project) = load_project_manifest_if_named(cwd, env)? {
            return Ok(project);
        }

        if let Some(path) = user_env_path(env)
            && path.exists()
        {
            return load_manifest(path, EnvSource::User);
        }

        if env == "default" {
            return Ok(builtin_env());
        }

        bail!("environment `{env}` was not found as a path, repo manifest name, or user env");
    }

    let project_path = cwd.join(PROJECT_MANIFEST);
    if project_path.exists() {
        return load_manifest(project_path, EnvSource::Project);
    }

    Ok(builtin_env())
}

fn builtin_env() -> ResolvedEnv {
    ResolvedEnv {
        source: EnvSource::BuiltIn,
        manifest: EnvManifest {
            name: Some("default".to_string()),
            base_image: Some(DEFAULT_BASE_IMAGE.to_string()),
            ..EnvManifest::default()
        },
    }
}

pub fn render_dockerfile(manifest: &EnvManifest) -> String {
    render_dockerfile_from(
        manifest,
        manifest.base_image.as_deref().unwrap_or(DEFAULT_BASE_IMAGE),
        false,
    )
}

fn render_inherited_dockerfile(manifest: &EnvManifest, parent_tag: &str) -> String {
    render_dockerfile_from(manifest, parent_tag, true)
}

fn render_dockerfile_from(manifest: &EnvManifest, base_image: &str, inherited: bool) -> String {
    let mut lines = Vec::new();

    if let Some(hooks) = &manifest.dockerfile
        && let Some(prepend) = &hooks.prepend
    {
        lines.push(prepend.trim_end().to_string());
    }

    lines.push(format!("FROM {base_image}"));

    lines.push("SHELL [\"/bin/sh\", \"-c\"]".to_string());

    for (key, value) in &manifest.env {
        lines.push(format!("ENV {}={}", key, shell_escape(value)));
    }

    let packages = if inherited {
        manifest.packages.clone()
    } else {
        resolved_packages(manifest)
    };
    if !packages.is_empty() {
        lines.push(format!(
            "RUN apt-get update && apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/*",
            packages.join(" ")
        ));
    }

    for command in &manifest.commands {
        lines.push(format!("RUN {command}"));
    }

    if !inherited || manifest.pi_install.is_some() {
        lines.push(format!(
            "RUN {}",
            manifest.pi_install.as_deref().unwrap_or(DEFAULT_PI_INSTALL)
        ));
    }
    if !inherited {
        lines.push("RUN mkdir -p /tmp/pi-home && chmod 0777 /tmp/pi-home".to_string());
    }

    for (key, value) in &manifest.runtime_env {
        lines.push(format!("ENV {}={}", key, shell_escape(value)));
    }

    if let Some(hooks) = &manifest.dockerfile
        && let Some(append) = &hooks.append
    {
        lines.push(append.trim_end().to_string());
    }

    lines.push(String::new());
    lines.join("\n")
}

fn prepare_environment(resolved: &ResolvedEnv, cwd: &Path) -> Result<PreparedEnvironment> {
    let mut images = Vec::new();
    let mut ancestors = Vec::new();
    let (_, git) = prepare_image_recursive(resolved, cwd, &mut ancestors, &mut images)?;
    Ok(PreparedEnvironment { images, git })
}

fn prepare_image_recursive(
    resolved: &ResolvedEnv,
    cwd: &Path,
    ancestors: &mut Vec<String>,
    images: &mut Vec<PreparedImage>,
) -> Result<(String, Option<GitConfig>)> {
    let fingerprint = source_fingerprint(&resolved.source);
    if ancestors.contains(&fingerprint) {
        let mut chain = ancestors.clone();
        chain.push(fingerprint);
        bail!("environment inheritance cycle: {}", chain.join(" -> "));
    }
    ancestors.push(fingerprint);

    let (dockerfile, inherited_git) = if let Some(parent_ref) = resolved.manifest.extends.as_deref()
    {
        if resolved.manifest.base_image.is_some() {
            bail!("an environment cannot set both `extends` and `base_image`");
        }
        let parent = resolve_parent_env(parent_ref, &resolved.source, cwd)?;
        let (parent_tag, parent_git) = prepare_image_recursive(&parent, cwd, ancestors, images)?;
        (
            render_inherited_dockerfile(&resolved.manifest, &parent_tag),
            parent_git,
        )
    } else {
        (render_dockerfile(&resolved.manifest), None)
    };

    ancestors.pop();
    let tag = image_tag(&dockerfile);
    if !images.iter().any(|image| image.tag == tag) {
        images.push(PreparedImage {
            tag: tag.clone(),
            dockerfile,
        });
    }
    let git = merge_git_config(inherited_git, resolved.manifest.git.as_ref());
    Ok((tag, git))
}

fn merge_git_config(parent: Option<GitConfig>, child: Option<&GitConfig>) -> Option<GitConfig> {
    let mut merged = parent.unwrap_or_default();
    if let Some(child) = child {
        if child.user_name.is_some() {
            merged.user_name.clone_from(&child.user_name);
        }
        if child.user_email.is_some() {
            merged.user_email.clone_from(&child.user_email);
        }
    }

    (merged.user_name.is_some() || merged.user_email.is_some()).then_some(merged)
}

fn resolve_parent_env(reference: &str, source: &EnvSource, cwd: &Path) -> Result<ResolvedEnv> {
    let parent_dir = match source {
        EnvSource::Project(path) | EnvSource::User(path) | EnvSource::Explicit(path) => {
            path.parent().unwrap_or(cwd)
        }
        EnvSource::BuiltIn => cwd,
    };
    resolve_env(Some(reference), parent_dir)
        .with_context(|| format!("failed to resolve parent environment `{reference}`"))
}

fn show_images(images: &[PreparedImage]) {
    for (index, image) in images.iter().enumerate() {
        if images.len() > 1 {
            if index > 0 {
                println!();
            }
            println!("# image {}", image.tag);
        }
        print!("{}", image.dockerfile);
    }
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

pub fn build_invocation(cache: &Path, tag: &str, pull: bool) -> Result<DockerInvocation> {
    let dockerfile = cache.join(tag).join("Dockerfile");
    let context = dockerfile
        .parent()
        .context("resolved Dockerfile path has no parent")?;

    let mut args = vec!["build".to_string()];
    if pull {
        args.push("--pull".to_string());
    }
    args.extend([
        "-t".to_string(),
        tag.to_string(),
        "-f".to_string(),
        dockerfile.display().to_string(),
        context.display().to_string(),
    ]);

    Ok(DockerInvocation {
        program: "docker".to_string(),
        args,
    })
}

fn ensure_image(cache: &Path, tag: &str, rebuild: bool, pull: bool) -> Result<()> {
    if !rebuild && !pull && docker_image_exists(tag)? {
        eprintln!("Reusing image {tag}");
        return Ok(());
    }

    run_process(build_invocation(cache, tag, pull)?)
}

fn build_images(cache: &Path, images: &[PreparedImage], rebuild: bool, pull: bool) -> Result<()> {
    for (index, image) in images.iter().enumerate() {
        write_resolved_dockerfile(cache, &image.tag, &image.dockerfile)?;
        ensure_image(cache, &image.tag, rebuild || pull, pull && index == 0)?;
    }
    Ok(())
}

fn docker_image_exists(tag: &str) -> Result<bool> {
    let status = ProcessCommand::new("docker")
        .args(["image", "inspect", tag])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to inspect local Docker images")?;
    Ok(status.success())
}

pub fn run_invocation(
    cwd: &Path,
    home: &Path,
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
        "-v".to_string(),
        format!("{}:/tmp/pi-home", home.display()),
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

/// Resolved identity of a persistent home, computed without touching the filesystem.
#[derive(Debug, Clone, Serialize)]
pub struct HomeIdentity {
    pub path: PathBuf,
    pub namespace: String,
    pub worktree_id: String,
    /// True when the namespace is explicit and therefore shared by linked worktrees.
    pub shared: bool,
    pub repository: PathBuf,
    pub manifest_name: String,
    pub manifest_source: String,
}

/// Metadata written into every persistent home so `namespace list` can explain it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeMetadata {
    pub schema_version: u32,
    pub namespace: String,
    pub shared: bool,
    pub repository: PathBuf,
    pub manifest_name: String,
    pub created_at_unix_ms: u128,
    pub created_from_worktree: PathBuf,
}

pub const HOME_METADATA_FILE: &str = "pi-sandbox-home.json";
pub const PI_AUTH_FILE: &str = ".pi/agent/auth.json";

pub fn homes_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("homes"))
}

/// Legacy default-home location keyed on the worktree path, manifest path, and name.
fn legacy_persistent_home_dir(base: &Path, cwd: &Path, resolved: &ResolvedEnv) -> PathBuf {
    let mut input = cwd.display().to_string();
    input.push('\n');
    input.push_str(&source_fingerprint(&resolved.source));
    input.push('\n');
    input.push_str(resolved.manifest.name.as_deref().unwrap_or("default"));

    let name = format!("{}-{:x}", project_slug(cwd), fnv1a64(input.as_bytes()));
    base.join(name)
}

pub fn resolve_home_identity(
    base: &Path,
    cwd: &Path,
    resolved: &ResolvedEnv,
    launch: &ResolvedRunConfig,
) -> HomeIdentity {
    let repository_identity = launch
        .git
        .as_ref()
        .map(|git| git.common_dir.as_path())
        .unwrap_or(cwd);
    let repository_identity =
        fs::canonicalize(repository_identity).unwrap_or_else(|_| repository_identity.to_path_buf());
    let repository_root = if repository_identity
        .file_name()
        .is_some_and(|name| name == ".git")
    {
        repository_identity.parent().unwrap_or(&repository_identity)
    } else {
        &repository_identity
    };
    let manifest_name = resolved
        .manifest
        .name
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let mut input = repository_identity.display().to_string();
    input.push('\n');
    input.push_str(&manifest_name);
    input.push('\n');
    input.push_str(&launch.namespace);

    let name = format!(
        "{}-{}-{:x}",
        project_slug(repository_root),
        launch.namespace,
        fnv1a64(input.as_bytes())
    );
    HomeIdentity {
        path: base.join(name),
        namespace: launch.namespace.clone(),
        worktree_id: launch.worktree_id.clone(),
        shared: launch.namespace != launch.worktree_id,
        repository: repository_root.to_path_buf(),
        manifest_name,
        manifest_source: source_fingerprint(&resolved.source),
    }
}

pub fn persistent_home_identity(
    cwd: &Path,
    resolved: &ResolvedEnv,
    launch: &ResolvedRunConfig,
) -> Result<HomeIdentity> {
    Ok(resolve_home_identity(&homes_dir()?, cwd, resolved, launch))
}

pub fn persistent_home_dir_for_run(
    cwd: &Path,
    resolved: &ResolvedEnv,
    launch: &ResolvedRunConfig,
) -> Result<PathBuf> {
    let base = homes_dir()?;
    let identity = resolve_home_identity(&base, cwd, resolved, launch);
    ensure_home(&base, cwd, resolved, &identity)
}

fn ensure_home(
    base: &Path,
    cwd: &Path,
    resolved: &ResolvedEnv,
    identity: &HomeIdentity,
) -> Result<PathBuf> {
    let path = &identity.path;
    if !path.exists() && !identity.shared {
        // Default homes were once keyed on the worktree and manifest path. Carry
        // credentials forward once instead of silently starting from an empty home.
        let legacy = legacy_persistent_home_dir(base, cwd, resolved);
        if legacy.is_dir() && fs::rename(&legacy, path).is_ok() {
            eprintln!(
                "migrated persistent home {} -> {}",
                legacy.display(),
                path.display()
            );
        }
    }
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create persistent home {}", path.display()))?;
    let metadata = path.join(HOME_METADATA_FILE);
    if !metadata.exists() {
        let record = HomeMetadata {
            schema_version: 1,
            namespace: identity.namespace.clone(),
            shared: identity.shared,
            repository: identity.repository.clone(),
            manifest_name: identity.manifest_name.clone(),
            created_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            created_from_worktree: fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        };
        let json = serde_json::to_vec_pretty(&record)?;
        fs::write(&metadata, json)
            .with_context(|| format!("failed to write {}", metadata.display()))?;
    }
    Ok(path.clone())
}

pub fn home_has_pi_credentials(home: &Path) -> bool {
    home.join(PI_AUTH_FILE).is_file()
}

/// Short human-readable state of a persistent home for diagnostics.
pub fn describe_home_state(home: &Path) -> &'static str {
    if !home.is_dir() {
        "not created yet"
    } else if home_has_pi_credentials(home) {
        "pi credentials present"
    } else {
        "no pi credentials"
    }
}

/// The interactive command that would authenticate this home.
pub fn authenticate_hint(identity: &HomeIdentity) -> String {
    if identity.shared {
        format!(
            "authenticate once with `pi-sandbox run --namespace {}` (or set `[run] namespace` in the manifest), or pass --env-file",
            identity.namespace
        )
    } else {
        "authenticate once with `pi-sandbox run` in this worktree; to share credentials across linked worktrees, set `[run] namespace` in the manifest and authenticate that namespace instead, or pass --env-file".to_string()
    }
}

/// Explain on stderr where credentials will come from, before launching a worker.
/// Missing credentials are not fatal here: an env file or manifest may supply them.
pub fn report_credential_sources(identity: &HomeIdentity, launch: &ResolvedRunConfig) {
    let has_api_key_env = launch.env.keys().any(|key| key.ends_with("_API_KEY"));
    eprintln!(
        "persistent home: {} (namespace {}, {})",
        identity.path.display(),
        launch.namespace,
        describe_home_state(&identity.path)
    );
    if !home_has_pi_credentials(&identity.path) && !has_api_key_env {
        eprintln!(
            "warning: no saved pi credentials in the persistent home and no *_API_KEY launch variable; {}",
            authenticate_hint(identity)
        );
    }
}

pub fn seed_git_config(home: &Path, git: Option<&GitConfig>) -> Result<()> {
    let Some(git) = git else {
        return Ok(());
    };

    fs::create_dir_all(home)
        .with_context(|| format!("failed to create persistent home {}", home.display()))?;
    let gitconfig = home.join(".gitconfig");

    if let Some(name) = &git.user_name {
        run_git_config(&gitconfig, "user.name", name)?;
    }
    if let Some(email) = &git.user_email {
        run_git_config(&gitconfig, "user.email", email)?;
    }

    Ok(())
}

fn run_git_config(gitconfig: &Path, key: &str, value: &str) -> Result<()> {
    let status = ProcessCommand::new("git")
        .arg("config")
        .arg("--file")
        .arg(gitconfig)
        .arg(key)
        .arg(value)
        .status()
        .context("failed to run git config")?;
    if !status.success() {
        bail!("git config failed with status {status}");
    }
    Ok(())
}

fn load_manifest(path: PathBuf, source: impl FnOnce(PathBuf) -> EnvSource) -> Result<ResolvedEnv> {
    let path = fs::canonicalize(&path)
        .with_context(|| format!("failed to resolve manifest path {}", path.display()))?;
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

fn project_slug(cwd: &Path) -> String {
    let slug = default_env_name(cwd)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    }
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
    let path = dir.join("Dockerfile");
    let temporary = dir.join(format!(".Dockerfile.{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, dockerfile)
        .with_context(|| format!("failed to write resolved Dockerfile for {tag}"))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("failed to publish resolved Dockerfile for {tag}"))?;
    Ok(())
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PI_SANDBOX_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::cache_dir().context("could not determine user cache directory")?;
    Ok(base.join("pi-sandbox"))
}

fn config_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PI_SANDBOX_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(dirs::config_dir()
        .context("could not determine user config directory")?
        .join("pi-sandbox"))
}

fn user_env_dir() -> Option<PathBuf> {
    config_dir().ok().map(|dir| dir.join("envs"))
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

    if let Some(dir) = user_env_dir()
        && dir.exists()
    {
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

    println!("built-in\tdefault\t{}", DEFAULT_BASE_IMAGE);
    Ok(())
}

fn image_tag(dockerfile: &str) -> String {
    let hash = fnv1a64(dockerfile.as_bytes());
    format!("pi-sandbox-env:{hash:016x}")
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
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
    let mut child = ProcessCommand::new(&invocation.program)
        .args(&invocation.args)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {}", invocation.program))?;
    let mut stdout = child
        .stdout
        .take()
        .context("failed to capture process stdout")?;
    let drain = std::thread::spawn(move || io::copy(&mut stdout, &mut io::stderr()));
    let status = child.wait().context("failed to wait for child process")?;
    drain
        .join()
        .map_err(|_| anyhow::anyhow!("process output drain panicked"))??;
    if !status.success() {
        bail!("{} failed with status {status}", invocation.program);
    }
    Ok(())
}

struct SignalState {
    interrupt: Arc<AtomicBool>,
    terminate: Arc<AtomicBool>,
    hangup: Arc<AtomicBool>,
}

impl SignalState {
    fn install() -> Result<Self> {
        let state = Self {
            interrupt: Arc::new(AtomicBool::new(false)),
            terminate: Arc::new(AtomicBool::new(false)),
            hangup: Arc::new(AtomicBool::new(false)),
        };
        #[cfg(unix)]
        {
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&state.interrupt))?;
            signal_hook::flag::register(
                signal_hook::consts::SIGTERM,
                Arc::clone(&state.terminate),
            )?;
            signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&state.hangup))?;
        }
        Ok(state)
    }

    fn take(&self) -> Option<i32> {
        if self.interrupt.swap(false, Ordering::Relaxed) {
            Some(libc::SIGINT)
        } else if self.terminate.swap(false, Ordering::Relaxed) {
            Some(libc::SIGTERM)
        } else if self.hangup.swap(false, Ordering::Relaxed) {
            Some(libc::SIGHUP)
        } else {
            None
        }
    }
}

fn run_process_interactive(
    invocation: DockerInvocation,
    signals: &SignalState,
    cid_file: &ContainerIdFile,
) -> Result<()> {
    let mut child = ProcessCommand::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to run {}", invocation.program))?;
    let mut forwarded_at = None;
    let status = loop {
        if let Some(status) = child.try_wait().context("failed waiting for Docker")? {
            break status;
        }
        if let Some(signal) = signals.take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as i32, signal);
            }
            forwarded_at = Some(Instant::now());
        }
        if forwarded_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(5)) {
            if let Some(id) = cid_file.id() {
                let _ = ProcessCommand::new("docker")
                    .args(["rm", "-f", &id])
                    .status();
            }
            let _ = child.kill();
            break child
                .wait()
                .context("failed waiting for terminated Docker")?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
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
            extends: None,
            base_image: Some("node:22-bookworm".to_string()),
            default_packages: Some(false),
            packages: vec!["git".to_string(), "curl".to_string()],
            env: BTreeMap::from([("RUSTUP_HOME".to_string(), "/opt/rustup".to_string())]),
            runtime_env: BTreeMap::new(),
            commands: vec!["npm --version".to_string()],
            pi_install: Some(
                "npm install -g --ignore-scripts @earendil-works/pi-coding-agent".to_string(),
            ),
            git: None,
            dockerfile: Some(DockerfileHooks {
                prepend: Some("# syntax=docker/dockerfile:1".to_string()),
                append: Some("RUN true".to_string()),
            }),
            run: None,
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
        assert!(dockerfile.contains(" gh"));
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
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: Vec::new(),
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let manifest = init_manifest_from_options(dir.path(), &options).unwrap();

        assert_eq!(
            manifest.name.as_deref(),
            dir.path().file_name().and_then(|name| name.to_str())
        );
        assert_eq!(manifest.base_image.as_deref(), Some(DEFAULT_BASE_IMAGE));
        assert_eq!(manifest.default_packages, None);
        assert!(manifest.packages.is_empty());
    }

    #[test]
    fn runtime_env_renders_after_install_layers() {
        let manifest = EnvManifest {
            commands: vec!["expensive install".to_string()],
            runtime_env: BTreeMap::from([("PATH".to_string(), "/opt/bin:/usr/bin".to_string())]),
            ..EnvManifest::default()
        };

        let dockerfile = render_dockerfile(&manifest);
        let command_index = dockerfile.find("RUN expensive install").unwrap();
        let pi_index = dockerfile.find(DEFAULT_PI_INSTALL).unwrap();
        let path_index = dockerfile.find("ENV PATH=/opt/bin:/usr/bin").unwrap();

        assert!(command_index < pi_index);
        assert!(pi_index < path_index);
    }

    #[test]
    fn python_preset_installs_pinned_uv_for_non_root_users() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("python-app".to_string()),
            base_image: None,
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(false),
            presets: vec!["python".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let manifest = init_manifest_from_options(dir.path(), &options).unwrap();
        let dockerfile = render_dockerfile(&manifest);

        assert!(manifest.packages.contains(&"ca-certificates".to_string()));
        assert!(manifest.packages.contains(&"curl".to_string()));
        assert!(manifest.packages.contains(&"python3".to_string()));
        assert!(manifest.commands.contains(&uv_install_command()));
        assert!(dockerfile.contains(&format!("download/{UV_VERSION}/uv-installer.sh")));
        assert!(dockerfile.contains("UV_UNMANAGED_INSTALL=/usr/local/bin"));
    }

    #[test]
    fn rust_preset_installs_rust_into_opt_runtime_env() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("rust-app".to_string()),
            base_image: None,
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["rust".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let manifest = init_manifest_from_options(dir.path(), &options).unwrap();

        assert!(manifest.commands.contains(&RUSTUP_COMMAND.to_string()));
        assert!(
            manifest
                .commands
                .contains(&RUST_COMPONENTS_COMMAND.to_string())
        );
        assert_eq!(
            manifest.runtime_env.get("CARGO_HOME").map(String::as_str),
            Some("/opt/cargo")
        );
        assert_eq!(
            manifest.runtime_env.get("PATH").map(String::as_str),
            Some(RUST_PATH)
        );
        assert_eq!(
            manifest
                .run
                .as_ref()
                .and_then(|run| run.env.get("CARGO_TARGET_DIR"))
                .map(String::as_str),
            Some(RUST_TARGET_DIR)
        );
    }

    #[test]
    fn tauri_preset_implies_node_rust_and_tauri_packages() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("tauri-app".to_string()),
            base_image: None,
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["tauri".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let manifest = init_manifest_from_options(dir.path(), &options).unwrap();

        assert_eq!(manifest.base_image.as_deref(), Some(DEFAULT_BASE_IMAGE));
        assert!(manifest.commands.contains(&RUSTUP_COMMAND.to_string()));
        assert!(manifest.packages.contains(&"libgtk-3-dev".to_string()));
        assert!(
            manifest
                .packages
                .contains(&"libwebkit2gtk-4.1-dev".to_string())
        );
    }

    #[test]
    fn rails_preset_uses_pinned_ruby_with_node_and_persistent_bundle_path() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("rails-app".to_string()),
            base_image: None,
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["rails".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let manifest = init_manifest_from_options(dir.path(), &options).unwrap();
        let dockerfile = render_dockerfile(&manifest);

        assert_eq!(manifest.base_image.as_deref(), Some(RAILS_BASE_STAGE));
        assert!(manifest.packages.contains(&"libpq-dev".to_string()));
        assert!(manifest.packages.contains(&"libvips".to_string()));
        assert_eq!(
            manifest.runtime_env.get("BUNDLE_PATH").map(String::as_str),
            Some(RAILS_BUNDLE_PATH)
        );
        assert!(dockerfile.contains(&format!("FROM {DEFAULT_BASE_IMAGE} AS {RAILS_NODE_STAGE}")));
        assert!(dockerfile.contains(&format!("FROM {RAILS_RUBY_IMAGE} AS {RAILS_BASE_STAGE}")));
        assert!(dockerfile.contains(&format!("FROM {RAILS_BASE_STAGE}")));
        assert!(dockerfile.contains("COPY --from=pi-node-runtime /usr/local/bin/node"));
        assert!(dockerfile.contains("ln -s ../lib/node_modules/npm/bin/npm-cli.js"));
        assert!(dockerfile.contains("corepack enable"));
        assert!(dockerfile.contains(DEFAULT_PI_INSTALL));
    }

    #[test]
    fn rails_preset_rejects_environment_inheritance() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("rails-app".to_string()),
            base_image: None,
            extends: Some("shared".to_string()),
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["rails".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let err = init_manifest_from_options(dir.path(), &options).unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot be combined with `extends`")
        );
    }

    #[test]
    fn rails_preset_rejects_a_custom_base_image() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("rails-app".to_string()),
            base_image: Some("debian:bookworm".to_string()),
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["rails".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let err = init_manifest_from_options(dir.path(), &options).unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot be combined with `--base-image`")
        );
    }

    #[test]
    fn init_namespace_writes_run_section_and_validates() {
        let dir = tempdir().unwrap();
        let options = |namespace: &str| InitOptions {
            yes: true,
            force: false,
            name: Some("app".to_string()),
            base_image: None,
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["rust".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: Some(namespace.to_string()),
        };

        let shared = init_manifest_from_options(dir.path(), &options("app_agents")).unwrap();
        let run = shared.run.as_ref().unwrap();
        assert_eq!(run.namespace.as_deref(), Some("app_agents"));
        assert!(
            run.env.contains_key("CARGO_TARGET_DIR"),
            "presets must survive"
        );

        let default = init_manifest_from_options(dir.path(), &options("worktree")).unwrap();
        assert!(default.run.as_ref().unwrap().namespace.is_none());

        let err = init_manifest_from_options(dir.path(), &options("Bad-Name")).unwrap_err();
        assert!(err.to_string().contains("invalid namespace"));
    }

    #[test]
    fn unknown_preset_errors() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: None,
            base_image: None,
            extends: None,
            git_user_name: None,
            git_user_email: None,
            default_packages: Some(true),
            presets: vec!["unknown".to_string()],
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let err = init_manifest_from_options(dir.path(), &options).unwrap_err();

        assert!(err.to_string().contains("unknown preset"));
    }

    #[test]
    fn init_manifest_includes_git_config_from_options() {
        let dir = tempdir().unwrap();
        let options = InitOptions {
            yes: true,
            force: false,
            name: Some("repo".to_string()),
            base_image: None,
            extends: None,
            git_user_name: Some("Dan".to_string()),
            git_user_email: Some("dan@example.com".to_string()),
            default_packages: Some(true),
            presets: Vec::new(),
            packages: Vec::new(),
            commands: Vec::new(),
            namespace: None,
        };

        let manifest = init_manifest_from_options(dir.path(), &options).unwrap();

        assert_eq!(
            manifest
                .git
                .as_ref()
                .and_then(|git| git.user_name.as_deref()),
            Some("Dan")
        );
        assert_eq!(
            manifest
                .git
                .as_ref()
                .and_then(|git| git.user_email.as_deref()),
            Some("dan@example.com")
        );
    }

    #[test]
    fn seed_git_config_writes_persistent_home_gitconfig() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");

        seed_git_config(
            &home,
            Some(&GitConfig {
                user_name: Some("Dan".to_string()),
                user_email: Some("dan@example.com".to_string()),
            }),
        )
        .unwrap();

        let gitconfig = fs::read_to_string(home.join(".gitconfig")).unwrap();

        assert!(gitconfig.contains("name = Dan"));
        assert!(gitconfig.contains("email = dan@example.com"));
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
    fn named_namespace_shares_home_across_linked_worktrees() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("homes");
        let repository = dir.path().join("repo");
        let common = repository.join(".git");
        let first_worktree = dir.path().join("first");
        let second_worktree = dir.path().join("second");
        fs::create_dir_all(&common).unwrap();
        fs::create_dir_all(&first_worktree).unwrap();
        fs::create_dir_all(&second_worktree).unwrap();

        let resolved = ResolvedEnv {
            source: EnvSource::Project(first_worktree.join(PROJECT_MANIFEST)),
            manifest: EnvManifest {
                name: Some("app".into()),
                ..EnvManifest::default()
            },
        };
        let config = |worktree: &Path, worktree_id: &str, namespace: &str| ResolvedRunConfig {
            networks: Vec::new(),
            host_access: HostAccess::Default,
            namespace: namespace.into(),
            worktree_id: worktree_id.into(),
            env: BTreeMap::new(),
            git: Some(launch::GitMounts {
                worktree_root: worktree.to_path_buf(),
                workspace_cwd: PathBuf::new(),
                git_dir: common.clone(),
                common_dir: common.clone(),
                separate_git_dir: true,
            }),
        };

        let home = |cwd: &Path, resolved: &ResolvedEnv, launch: &ResolvedRunConfig| {
            let identity = resolve_home_identity(&base, cwd, resolved, launch);
            ensure_home(&base, cwd, resolved, &identity).unwrap()
        };
        let first = home(
            &first_worktree,
            &resolved,
            &config(&first_worktree, "wt_first", "orchestration"),
        );
        let second = home(
            &second_worktree,
            &resolved,
            &config(&second_worktree, "wt_second", "orchestration"),
        );
        let explicitly_selected = home(
            &second_worktree,
            &ResolvedEnv {
                source: EnvSource::Explicit(repository.join(".pi-env.toml")),
                manifest: resolved.manifest.clone(),
            },
            &config(&second_worktree, "wt_second", "orchestration"),
        );
        let isolated = home(
            &second_worktree,
            &resolved,
            &config(&second_worktree, "wt_second", "other"),
        );
        let default_first = home(
            &first_worktree,
            &resolved,
            &config(&first_worktree, "wt_first", "wt_first"),
        );
        let default_second = home(
            &second_worktree,
            &resolved,
            &config(&second_worktree, "wt_second", "wt_second"),
        );

        assert_eq!(first, second);
        assert_eq!(first, explicitly_selected);
        assert_ne!(first, isolated);
        assert_ne!(default_first, default_second);
        assert_ne!(default_first, first);
        assert!(first.is_dir());

        let metadata: HomeMetadata =
            serde_json::from_slice(&fs::read(first.join(HOME_METADATA_FILE)).unwrap()).unwrap();
        assert_eq!(metadata.namespace, "orchestration");
        assert!(metadata.shared);
        assert_eq!(metadata.manifest_name, "app");
        let default_metadata: HomeMetadata =
            serde_json::from_slice(&fs::read(default_first.join(HOME_METADATA_FILE)).unwrap())
                .unwrap();
        assert!(!default_metadata.shared);
    }

    #[test]
    fn default_home_keys_on_repository_and_survives_manifest_moves() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("homes");
        let worktree = dir.path().join("repo");
        let common = worktree.join(".git");
        fs::create_dir_all(&common).unwrap();
        let manifest = EnvManifest {
            name: Some("app".into()),
            ..EnvManifest::default()
        };
        let launch = ResolvedRunConfig {
            networks: Vec::new(),
            host_access: HostAccess::Default,
            namespace: "wt_repo".into(),
            worktree_id: "wt_repo".into(),
            env: BTreeMap::new(),
            git: Some(launch::GitMounts {
                worktree_root: worktree.clone(),
                workspace_cwd: PathBuf::new(),
                git_dir: common.clone(),
                common_dir: common.clone(),
                separate_git_dir: false,
            }),
        };
        let project = ResolvedEnv {
            source: EnvSource::Project(worktree.join(PROJECT_MANIFEST)),
            manifest: manifest.clone(),
        };
        let explicit = ResolvedEnv {
            source: EnvSource::Explicit(dir.path().join("elsewhere.toml")),
            manifest,
        };

        let from_project = resolve_home_identity(&base, &worktree, &project, &launch);
        let from_explicit = resolve_home_identity(&base, &worktree, &explicit, &launch);
        assert_eq!(from_project.path, from_explicit.path);
        assert!(!from_project.shared);
        assert!(
            !base.exists(),
            "identity resolution must not create directories"
        );
    }

    #[test]
    fn default_home_migrates_legacy_directory_once() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("homes");
        let worktree = dir.path().join("repo");
        fs::create_dir_all(worktree.join(".git")).unwrap();
        let resolved = ResolvedEnv {
            source: EnvSource::Project(worktree.join(PROJECT_MANIFEST)),
            manifest: EnvManifest {
                name: Some("app".into()),
                ..EnvManifest::default()
            },
        };
        let launch = ResolvedRunConfig {
            networks: Vec::new(),
            host_access: HostAccess::Default,
            namespace: "wt_repo".into(),
            worktree_id: "wt_repo".into(),
            env: BTreeMap::new(),
            git: None,
        };
        let legacy = legacy_persistent_home_dir(&base, &worktree, &resolved);
        fs::create_dir_all(legacy.join(".pi/agent")).unwrap();
        fs::write(legacy.join(PI_AUTH_FILE), b"{}").unwrap();

        let identity = resolve_home_identity(&base, &worktree, &resolved, &launch);
        let home = ensure_home(&base, &worktree, &resolved, &identity).unwrap();

        assert_ne!(home, legacy);
        assert!(!legacy.exists());
        assert!(home_has_pi_credentials(&home));
        assert!(home.join(HOME_METADATA_FILE).is_file());
    }

    #[test]
    fn run_invocation_uses_host_identity_and_workspace_mount() {
        let args = run_invocation(
            Path::new("/repo"),
            Path::new("/config/pi-sandbox/homes/repo-abc"),
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
                .any(|pair| pair == ["-v", "/config/pi-sandbox/homes/repo-abc:/tmp/pi-home"])
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
            build_invocation(Path::new("/cache/pi-sandbox"), "pi-sandbox-env:abc", false).unwrap();

        assert_eq!(invocation.program, "docker");
        assert_eq!(
            invocation.args,
            vec![
                "build",
                "-t",
                "pi-sandbox-env:abc",
                "-f",
                "/cache/pi-sandbox/pi-sandbox-env:abc/Dockerfile",
                "/cache/pi-sandbox/pi-sandbox-env:abc",
            ]
        );
    }

    #[test]
    fn pull_build_invocation_refreshes_the_base_image() {
        let invocation =
            build_invocation(Path::new("/cache/pi-sandbox"), "pi-sandbox-env:abc", true).unwrap();

        assert_eq!(invocation.args[0..2], ["build", "--pull"]);
    }

    #[test]
    fn image_tag_is_deterministic_and_independent_of_project_metadata() {
        let first = EnvManifest {
            name: Some("first".to_string()),
            git: Some(GitConfig {
                user_name: Some("First User".to_string()),
                user_email: None,
            }),
            ..EnvManifest::default()
        };
        let second = EnvManifest {
            name: Some("second".to_string()),
            git: Some(GitConfig {
                user_name: Some("Second User".to_string()),
                user_email: None,
            }),
            ..EnvManifest::default()
        };

        let first_tag = image_tag(&render_dockerfile(&first));
        let second_tag = image_tag(&render_dockerfile(&second));

        assert_eq!(first_tag, second_tag);
        assert!(first_tag.starts_with("pi-sandbox-env:"));
        assert_eq!(first_tag.len(), "pi-sandbox-env:".len() + 16);
    }

    #[test]
    fn launch_only_configuration_does_not_change_image() {
        let plain = EnvManifest::default();
        let configured = EnvManifest {
            run: Some(RunConfig {
                networks: Some(vec!["db".into()]),
                env: BTreeMap::from([("SECRET".into(), "value".into())]),
                ..Default::default()
            }),
            ..EnvManifest::default()
        };
        assert_eq!(render_dockerfile(&plain), render_dockerfile(&configured));
        assert_eq!(
            image_tag(&render_dockerfile(&plain)),
            image_tag(&render_dockerfile(&configured))
        );
    }

    #[test]
    fn image_tag_changes_when_build_content_changes() {
        let first = EnvManifest::default();
        let second = EnvManifest {
            packages: vec!["ripgrep".to_string()],
            ..EnvManifest::default()
        };

        assert_ne!(
            image_tag(&render_dockerfile(&first)),
            image_tag(&render_dockerfile(&second))
        );
    }

    #[test]
    fn inherited_environment_builds_on_the_parent_image() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("base.toml"),
            r#"
name = "rust"
base_image = "node:22-bookworm"
packages = ["ripgrep"]

[git]
user_name = "Parent User"
user_email = "parent@example.com"
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join(PROJECT_MANIFEST),
            r#"
name = "tauri"
extends = "base.toml"
packages = ["libgtk-3-dev"]

[git]
user_email = "child@example.com"
"#,
        )
        .unwrap();

        let resolved = resolve_env(None, dir.path()).unwrap();
        let prepared = prepare_environment(&resolved, dir.path()).unwrap();

        assert_eq!(prepared.images.len(), 2);
        let parent = &prepared.images[0];
        let child = &prepared.images[1];
        assert!(parent.dockerfile.contains("FROM node:22-bookworm"));
        assert!(parent.dockerfile.contains("ripgrep"));
        assert!(parent.dockerfile.contains(DEFAULT_PI_INSTALL));
        assert!(
            child
                .dockerfile
                .starts_with(&format!("FROM {}", parent.tag))
        );
        assert!(child.dockerfile.contains("libgtk-3-dev"));
        assert!(!child.dockerfile.contains(DEFAULT_PI_INSTALL));
        assert!(!child.dockerfile.contains("build-essential"));
        assert_eq!(
            prepared
                .git
                .as_ref()
                .and_then(|git| git.user_name.as_deref()),
            Some("Parent User")
        );
        assert_eq!(
            prepared
                .git
                .as_ref()
                .and_then(|git| git.user_email.as_deref()),
            Some("child@example.com")
        );
    }

    #[test]
    fn inherited_image_tag_changes_with_its_parent() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("base.toml");
        fs::write(&parent, "packages = [\"ripgrep\"]\n").unwrap();
        fs::write(
            dir.path().join(PROJECT_MANIFEST),
            "extends = \"base.toml\"\npackages = [\"fd-find\"]\n",
        )
        .unwrap();
        let resolved = resolve_env(None, dir.path()).unwrap();
        let first = prepare_environment(&resolved, dir.path()).unwrap();

        fs::write(&parent, "packages = [\"jq\"]\n").unwrap();
        let second = prepare_environment(&resolved, dir.path()).unwrap();

        assert_ne!(
            first.images.last().unwrap().tag,
            second.images.last().unwrap().tag
        );
    }

    #[test]
    fn environment_inheritance_cycles_are_rejected() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.toml"), "extends = \"b.toml\"\n").unwrap();
        fs::write(dir.path().join("b.toml"), "extends = \"a.toml\"\n").unwrap();
        let resolved = resolve_env(Some("a.toml"), dir.path()).unwrap();

        let err = prepare_environment(&resolved, dir.path()).unwrap_err();

        assert!(err.to_string().contains("inheritance cycle"));
    }

    #[test]
    fn inherited_environment_cannot_also_set_a_base_image() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("base.toml"), "name = \"base\"\n").unwrap();
        fs::write(
            dir.path().join(PROJECT_MANIFEST),
            "extends = \"base.toml\"\nbase_image = \"node:20-bookworm\"\n",
        )
        .unwrap();
        let resolved = resolve_env(None, dir.path()).unwrap();

        let err = prepare_environment(&resolved, dir.path()).unwrap_err();

        assert!(err.to_string().contains("both `extends` and `base_image`"));
    }
}
