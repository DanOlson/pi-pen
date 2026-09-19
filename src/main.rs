use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

fn main() -> Result<()> {
    pi_sandbox::run_cli(Cli::parse().into())
}

#[derive(Debug, Parser)]
#[command(name = "pi-sandbox")]
#[command(about = "Launch the pi coding agent inside reusable Docker development environments.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build an environment image.
    Build(BuildArgs),
    /// List or inspect discovered environments.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Create a repo-local environment manifest.
    Init(InitArgs),
    /// Start an interactive pi session in an environment.
    Run(RunArgs),
}

#[derive(Debug, Args)]
struct EnvArg {
    /// Environment name or manifest path.
    env: Option<String>,
}

#[derive(Debug, Args)]
struct BuildArgs {
    /// Environment name or manifest path.
    env: Option<String>,
    /// Build even when the content-addressed image already exists locally.
    #[arg(long)]
    rebuild: bool,
    /// Always check for a newer base image while building.
    #[arg(long)]
    pull: bool,
}

#[derive(Debug, Subcommand)]
enum EnvCommand {
    /// List repo-local and user-library environments.
    List,
    /// Show the resolved Dockerfile for an environment.
    Show(EnvArg),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Environment name or manifest path.
    env: Option<String>,
    /// Build even when the content-addressed image already exists locally.
    #[arg(long)]
    rebuild: bool,
    /// Always check for a newer base image while building.
    #[arg(long)]
    pull: bool,
    #[command(flatten)]
    launch: pi_sandbox::LaunchArgs,
    /// Arguments passed through to pi.
    #[arg(last = true)]
    pi_args: Vec<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Use defaults and do not prompt.
    #[arg(long)]
    yes: bool,
    /// Overwrite an existing .pi-env.toml.
    #[arg(long)]
    force: bool,
    /// Environment name.
    #[arg(long)]
    name: Option<String>,
    /// Docker base image.
    #[arg(long)]
    base_image: Option<String>,
    /// Reusable parent environment to extend.
    #[arg(long, conflicts_with = "base_image")]
    extends: Option<String>,
    /// Git user.name to seed in the container's persistent home.
    #[arg(long)]
    git_user_name: Option<String>,
    /// Git user.email to seed in the container's persistent home.
    #[arg(long)]
    git_user_email: Option<String>,
    /// Disable the default Debian package baseline.
    #[arg(long)]
    no_default_packages: bool,
    /// Composable stack preset. Use comma-separated values or repeat the flag.
    #[arg(long = "with", value_delimiter = ',')]
    presets: Vec<String>,
    /// Additional apt package. Can be repeated.
    #[arg(long = "package")]
    packages: Vec<String>,
    /// Setup command to add to the Dockerfile. Can be repeated.
    #[arg(long = "command")]
    commands: Vec<String>,
}

impl From<Cli> for pi_sandbox::Cli {
    fn from(value: Cli) -> Self {
        Self {
            command: match value.command {
                Command::Build(args) => pi_sandbox::Command::Build {
                    env: args.env,
                    rebuild: args.rebuild,
                    pull: args.pull,
                },
                Command::Env { command } => pi_sandbox::Command::Env {
                    command: match command {
                        EnvCommand::List => pi_sandbox::EnvCommand::List,
                        EnvCommand::Show(args) => pi_sandbox::EnvCommand::Show { env: args.env },
                    },
                },
                Command::Init(args) => pi_sandbox::Command::Init(pi_sandbox::InitOptions {
                    yes: args.yes,
                    force: args.force,
                    name: args.name,
                    base_image: args.base_image,
                    extends: args.extends,
                    git_user_name: args.git_user_name,
                    git_user_email: args.git_user_email,
                    default_packages: Some(!args.no_default_packages),
                    presets: args.presets,
                    packages: args.packages,
                    commands: args.commands,
                }),
                Command::Run(args) => pi_sandbox::Command::Run {
                    env: args.env,
                    rebuild: args.rebuild,
                    pull: args.pull,
                    pi_args: args.pi_args,
                    overrides: args.launch.into(),
                },
            },
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}
