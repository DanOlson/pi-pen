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
pi-sandbox run [ENV] [--rebuild] [--pull] [-- <pi args...>]
pi-sandbox build [ENV] [--rebuild] [--pull]
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

Use `[env]` for variables needed while building the image. Use `[runtime_env]`
for variables only needed when the container runs, such as final `PATH` values;
runtime env is rendered after install commands so edits are less likely to
invalidate expensive Docker layers.

Use `[git]` to seed Git identity into the container's persistent home without
mounting host Git, SSH, or GitHub CLI config. On `run`, these values are written
to `/tmp/pi-home/.gitconfig`.

## TODO

- Add optional explicit host mounts for tool config such as SSH keys, cloud CLI
  config, npm, and cargo when users want to share existing host credentials
  with the container.

## Persistent Home

`pi-sandbox run` creates a per-project persistent home directory under the
platform config directory, then mounts it at `/tmp/pi-home` in the container.
This lets the pi agent and other tools keep credentials/config between runs
even though each container is recreated.

The project itself is still mounted separately at `/workspace`.

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
