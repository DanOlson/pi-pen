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
pi-sandbox run [ENV] [-- <pi args...>]
pi-sandbox build [ENV]
pi-sandbox env list
pi-sandbox env show [ENV]
```

`ENV` can be a manifest path, a repo-local manifest name, or a user-library
environment name.

Use `pi-sandbox --help`, `pi-sandbox -h`, or `<command> --help` for command
usage.

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
pi-sandbox init --yes --no-default-packages --package ripgrep
pi-sandbox init --force
```

## Environment Discovery

Resolution order:

1. Explicit manifest path passed as `ENV`.
2. Repo-local `.pi-env.toml`.
3. User-library manifest at the platform config directory:
   `pi-sandbox/envs/<name>.toml`.
4. Built-in default environment based on `node:22-bookworm` plus a practical
   Debian package baseline.

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
RUN npm install -g --ignore-scripts @earendil-works/pi-coding-agent
```

Override that command with `pi_install` only when testing a different agent
package or install source.

## TODO

- Improve Dockerfile rendering so runtime-only environment changes, especially
  `PATH`, do not invalidate expensive install layers when possible.
- Add a Rust preset to `pi-sandbox init` that installs Rust under `/opt/cargo`
  and `/opt/rustup`, sets the correct runtime `PATH`, and works with the
  non-root container user.
- Add composable init presets, such as `--with rust,node` or `--with tauri`,
  that expand into packages, setup commands, and env vars. Presets should dedupe
  contributions, expand implied stacks like `tauri -> node + rust + Tauri Linux
  deps`, and still write plain editable TOML.

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
