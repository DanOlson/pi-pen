# pi-sandbox

`pi-sandbox` launches the `pi` coding agent inside a Docker development
environment while mounting the current project as a writable `/workspace`.

## Install From Source

Prerequisites:

- Rust/Cargo
- Docker

From this repository:

```sh
cargo install --path .
```

Cargo installs the `pi-sandbox` binary into `~/.cargo/bin`. Make sure that
directory is on your `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Verify the install:

```sh
pi-sandbox --help
```

To update a local source install after pulling changes:

```sh
cargo install --path . --force
```

## Commands

```sh
pi-sandbox init
pi-sandbox run [ENV] [--rebuild] [--pull] [network options] [-- <pi args...>]
pi-sandbox exec [ENV] (--prompt-file PATH | --prompt-stdin) --output-dir PATH [--timeout SECONDS] [-- <pi options...>]
pi-sandbox rpc [ENV] --output-dir PATH [--timeout SECONDS] [-- <pi options...>]
pi-sandbox stop --output-dir PATH [--grace SECONDS]
pi-sandbox build [ENV] [--rebuild] [--pull]
pi-sandbox env list
pi-sandbox env show [ENV]
pi-sandbox skills install [--project-dir PATH] [--link-claude | --no-link-claude] [--force]
```

`ENV` can be a manifest path, a repo-local manifest name, or a user-library
environment name.

Use `pi-sandbox --help`, `pi-sandbox -h`, or `<command> --help` for command
usage.

## Agent Skill

Install the bundled skill that teaches coding agents how to use `pi-sandbox`
programmatically and orchestrate isolated investigation, test-first
implementation, review, and revision sessions:

```sh
pi-sandbox skills install
```

The command finds the current Git worktree root and writes the skill to
`.agents/skills/pi-sandbox`. Pass `--project-dir PATH` to choose an explicit
root, including a directory that is not a Git worktree. Existing managed files
are updated only when they are unchanged; use `--force` to replace modified or
unrecognized bundled files.

In an interactive terminal, the installer offers to create
`.claude/skills/pi-sandbox` as a relative symlink to the canonical installation.
For scripts and noninteractive agents, choose explicitly:

```sh
pi-sandbox skills install --link-claude
pi-sandbox skills install --no-link-claude
```

The installer never replaces an existing Claude skill directory or a different
symlink automatically.

## Init Wizard

Create a repo-local manifest with:

```sh
pi-sandbox init
```

The wizard prompts for an environment name, base image, default package choice,
additional apt packages, and setup commands. It writes `.pi-env.toml` and will
not overwrite an existing manifest unless `--force` is passed.

Use defaults without prompting:

```sh
pi-sandbox init --yes
```

Common overrides:

```sh
pi-sandbox init --yes --name my-env --base-image node:22-bookworm
pi-sandbox init --yes --extends rust-node
pi-sandbox init --yes --with rust,node
pi-sandbox init --yes --with tauri
pi-sandbox init --yes --git-user-name "Your Name" --git-user-email you@example.com
pi-sandbox init --yes --no-default-packages --package ripgrep
pi-sandbox init --force
```

Stack presets expand into plain manifest content:

- `node`: use the default Node LTS base image.
- `python`: add Python runtime packages and install pinned uv/uvx binaries.
- `rust`: install Rust under `/opt/cargo` and `/opt/rustup`, add `rustfmt`
  and `clippy`, and set runtime Cargo/Rust environment.
- `tauri`: includes `node` and `rust`, then adds common Tauri v2 Linux build
  dependencies.

The Python preset installs uv 0.12.0 into `/usr/local/bin` as an unmanaged
installation. Pinning the version keeps the generated environment reproducible;
update the command in the manifest when you want to upgrade uv.

For an existing manifest, ensure `ca-certificates` and `curl` are present, then
append the installer to `commands`:

```toml
packages = ["ca-certificates", "curl"]
commands = [
  "curl --proto '=https' --tlsv1.2 -LsSf https://releases.astral.sh/github/uv/releases/download/0.12.0/uv-installer.sh | env UV_UNMANAGED_INSTALL=/usr/local/bin sh"
]
```

Merge those values into existing `packages` and `commands` arrays rather than
declaring either TOML key a second time.

## Environment Discovery

Resolution order:

1. Explicit manifest path passed as `ENV`.
2. Repo-local `.pi-env.toml`.
3. User-library manifest at the platform config directory:
   `pi-sandbox/envs/<name>.toml`.
4. Built-in default environment based on `node:22-bookworm` plus a practical
   Debian package baseline.

## Reusing Images

Images are named from their rendered build content, using tags such as
`pi-sandbox-env:0123456789abcdef`. Environment names, project paths, Git
identity, and persistent-home paths are not part of that tag. Two projects with
the same build definition therefore use the same local image. `build` and `run`
skip `docker build` when that image already exists.

Use `--rebuild` to run Docker's build again while retaining Docker's normal
layer cache. Use `--pull` to rebuild and check for a newer external base image.

An environment can add layers to another environment with `extends`:

```toml
name = "tauri-app"
extends = "rust-node"
packages = ["libgtk-3-dev", "libwebkit2gtk-4.1-dev"]
```

The parent is resolved using the normal environment rules. Relative manifest
paths are resolved from the child manifest's directory. The parent is built as
its own content-addressed image, and the child Dockerfile starts from that image
and installs only the child's additions. This lets a larger stack reuse a
previously built subset rather than merely sharing the external base image.
The default package baseline is installed by the root environment only;
derived environments inherit it and do not install it again.

Derived environments inherit the parent's installed tools, build and runtime
environment variables, and Git identity defaults. Child Git fields override
the corresponding parent fields. `extends` and `base_image` cannot be used in
the same manifest.

## Manifest Format

```toml
name = "rust-node"
base_image = "node:22-bookworm"
packages = ["ripgrep"]
commands = [
  "npm --version"
]

[env]
RUSTUP_HOME = "/opt/rustup"

[runtime_env]
PATH = "/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

[git]
user_name = "Your Name"
user_email = "you@example.com"

[dockerfile]
prepend = "# syntax=docker/dockerfile:1"
append = "RUN true"
```

The default base image is `node:22-bookworm`. By default, every environment
also installs these convenience packages:

```toml
packages = [
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
  "python3-venv"
]
```

Manifest `packages` extend that baseline. Set `default_packages = false` for a
lean image that only installs packages listed in the manifest.

Every generated image installs `pi` with:

```dockerfile
RUN npm install -g --ignore-scripts @earendil-works/pi-coding-agent@0.85.1
```

Override that command with `pi_install` only when testing a different agent
package or install source. Headless option and basic framing behavior has been
checked against 0.85.1; a custom install may use incompatible JSON/RPC records
or options.

## Programmatic workers

See the [programmatic usage guide](docs/programmatic-usage.md) for authentication,
parallel worktrees, runnable orchestration and RPC examples, exit codes, and cancellation.

`exec` runs pi without a terminal, emits schema-versioned JSONL on stdout, and
writes durable artifacts to a fresh output directory. `rpc` forwards pi's
native LF-delimited RPC protocol byte-for-byte on stdout. All launcher and
worker diagnostics are streamed to stderr, with the durable diagnostic log in
`stderr.log`. The output directory must be outside all worker mounts, including
the workspace, persistent home, and shared Git metadata.

```sh
mkdir -p /tmp/pi-runs
pi-sandbox exec --prompt-file /tmp/task.txt \
  --output-dir /tmp/pi-runs/task-1 --timeout 900 \
  -- --provider openai --model gpt-5

# Keeps stdin open until the RPC response arrives:
python3 examples/rpc_state.py --output-dir /tmp/pi-runs/review-1 --timeout 60
```

Exec envelopes contain `schema_version`, `type`, `run_id`, and a monotonically
increasing `seq`. `pi_event` envelopes preserve the complete upstream event in
`event`; the terminal `run_finished` envelope includes status and exit code.
RPC stdout is not wrapped. A command-acceptance RPC response is not task
completion, and the RPC process exit status does not summarize every prompt.

Each run stores `run.json`, atomic `result.json`, `events.jsonl`, `stderr.log`,
`response.txt` for exec, and an isolated `sessions/` directory. Directories are
mode 0700 and artifact files are mode 0600; they can contain source, model output,
or credentials echoed by tools. `result.json` includes session-file paths for
explicit follow-up. Its optional `usage` is the final assistant message's
reported usage, not a total across the whole conversation. A resume source is
cloned into the new run:

```sh
pi-sandbox exec --prompt-file /tmp/follow-up.txt \
  --resume-session /tmp/pi-runs/task-1/sessions/session.jsonl \
  --output-dir /tmp/pi-runs/task-2 --timeout 900
```

Supported pi passthrough options currently include provider, model, thinking,
tools, extensions, and `--no-tools`. Launcher-owned mode, print, session,
continue/resume, positional prompt, attachment, and unknown options are rejected
fail-closed. Prompts are UTF-8, nonempty, limited to 8 MiB, and read once.
Protocol records are limited to 32 MiB. Consume stdout and stderr concurrently:
a stdout write that makes no progress for one second fails the worker, allowing
cleanup to proceed. If stderr cannot keep up, the durable log remains the
authoritative diagnostic record. `stop` checks Docker context, endpoint, daemon
identity, and the exact run ownership label before touching a container. A headless run takes
an OS-backed exclusive lock for its canonical worktree; use distinct worktrees
for parallel editing. `SIGKILL` and host crashes cannot guarantee cleanup, so
retain the output directory and use `stop` to reconcile an orphan.

Both `[env]` and the historically named `[runtime_env]` are image defaults:
they are rendered into Dockerfile `ENV` instructions and affect the image tag.
Do not put connection secrets in either section. Use `[run]` for launch-only
settings and explicitly selected ignored env files for credentials.

Use `[git]` to seed Git identity into the container's persistent home without
mounting host Git, SSH, or GitHub CLI config. On `run`, these values are written
to `/tmp/pi-home/.gitconfig`.

## TODO

- Add optional explicit host mounts for tool config such as SSH keys, cloud CLI
  config, npm, and cargo when users want to share existing host credentials
  with the container.

## Persistent Home

`pi-sandbox run` creates a persistent home directory under the platform config
directory, then mounts it at `/tmp/pi-home` in the container. The default
`worktree` namespace keeps a separate home for each linked worktree. An explicit
namespace selects a home shared by linked worktrees in the same repository and
environment, allowing credentials, home-installed dependencies, caches, and
tool configuration to persist across orchestrated workers:

```sh
pi-sandbox run --namespace batch_shared
pi-sandbox exec --namespace batch_shared --prompt-file /tmp/task.txt --output-dir /tmp/run-1
```

Provision shared dependencies before parallel work and serialize concurrent
package-manager writes to the shared home.

The project itself is still mounted separately at `/workspace`.
For hermetic automation, host-side `PI_SANDBOX_CONFIG_DIR` and
`PI_SANDBOX_CACHE_DIR` override the normal platform locations. They are
launcher controls and cannot be passed through as container variables.

## Networked services and launch environment

`pi-sandbox` can join existing user-defined Docker networks and pass
launch-only application settings without rebuilding the environment image:

```toml
[run]
networks = ["myapp_default"]
host_access = "desktop"
env_files = [".pi-session.env"]
namespace = "worktree"

[run.env]
PGHOST = "postgres"
PGDATABASE = "app_{{namespace}}"
REDIS_URL = "redis://redis:6379/0"
APP_REDIS_PREFIX = "app:{{namespace}}:"
```

The services and named networks must already exist. `pi-sandbox` validates
networks but never creates, migrates, flushes, or deletes external resources.
Repeat `--network NAME` to replace the configured network list, or use
`--default-network` to clear it. Other launch controls are
`--host-access default|desktop|gateway`, repeated `--env-file PATH`, repeated
`--env KEY=VALUE`, `--namespace NAME`, and `--dry-run`. A dry run performs
local resolution and validation only; it redacts every environment value and
does not contact Docker or create the persistent home.

Manifest launch settings inherit parent-first. Omitted lists inherit, while an
explicit empty list clears. `[run.env]` maps merge with child values winning.
Environment precedence is image defaults, manifest env files, merged
`[run.env]`, CLI env files, then CLI `--env`. Manifest env-file paths are
relative to the manifest declaring the selected list; CLI paths are relative
to the invoking directory.

Env files use a deliberately small literal grammar: UTF-8 `KEY=VALUE` lines,
blank lines, and comments whose first non-whitespace character is `#`. Values
retain spaces, quotes, `#`, dollar signs, and additional equals signs. This is
not a shell or dotenv parser: `export`, bare keys, and multiline values are
rejected. Values are moved to Docker through a short-lived mode-0600 file and
are not placed in the Docker command arguments. Docker administrators can
still inspect a running container's environment; this is not a secret store.

Only `[run.env]` expands `{{namespace}}` and `{{worktree_id}}`. The default
namespace is a stable identifier derived from the canonical Git worktree root.
An explicit namespace must match `[a-z][a-z0-9_]{0,39}` and also selects shared
persistent home state for linked worktrees in the same repository and
environment. Resource namespace values are naming aids, not enforced service
isolation: applications must select separate pre-created databases or implement
prefixes themselves. A Redis prefix does not isolate admin commands, pub/sub,
or frameworks that ignore it.

On Docker Desktop, workstation services are reached at
`host.docker.internal:<port>` with `host_access = "desktop"`. For native Linux,
`gateway` adds `host.docker.internal:host-gateway`; the service must listen on
an address reachable from the bridge and its firewall/authentication should be
configured accordingly. The gateway is the Docker host, not necessarily its
loopback interface. Docker Desktop on Linux should normally use `desktop`.
Remote daemons, Windows, rootless networking, and alternative VM runtimes are
not verified in this version. The implementation is tested with Docker Engine
27.3.1; multiple networks are attached using repeated `--network` options.

Inside a session, `localhost` means the session container. No URL rewriting or
port publishing is performed. When diagnosing connectivity, distinguish DNS
failure, connection refusal/timeout, and application authentication errors;
network existence validation does not prove a database is reachable.

When run inside a linked Git worktree, pi-sandbox mounts only that worktree and
the repository's Git administrative directories, then supplies `GIT_DIR`,
`GIT_COMMON_DIR`, and `GIT_WORK_TREE`. Git status, staging, and commits work,
but shared repository metadata remains writable shared state. Creating/removing
worktrees inside the container, submodules, and unusual external object stores
are not supported by this first version.

## File Ownership

`pi-sandbox run` starts Docker with the host numeric UID/GID:

```text
--user "$(id -u):$(id -g)"
```

The project is mounted as writable:

```text
-v "$PWD:/workspace" -w /workspace
```

The important acceptance condition is that files created by `pi` under
`/workspace` are owned by the host user and can be edited or deleted without
`sudo`. On Docker Desktop for macOS, the host-visible group may differ from the
container GID, so UID ownership is the primary portability guarantee.
