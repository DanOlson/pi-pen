# Networked resources and parallel worktrees

Status: implementation specification; no runtime changes implemented by this document.
Written September 19, 2026.

## Goal and agreed scope

Run multiple pi sessions concurrently, each in its own container and working
directory, while applications connect to already-running Postgres, Redis, or
other network services. Docker Desktop on macOS is the primary target. Document
and provide an explicit configuration path for native Linux Docker.

The user confirmed:

- Infrastructure can run directly on the workstation or in other containers.
- Resource isolation should be configurable when needed.
- Infrastructure is already running; pi-sandbox need not start or manage it.

For this implementation, configurable isolation means selecting existing
resources with explicit connection settings, optionally using a stable worktree
namespace. It does not mean automatically provisioning databases. A selected
database must already exist, or the application's normal setup must create it.
Do not silently create, migrate, flush, or delete external resources.

Out of scope: Compose lifecycle management, database provisioning, automatic
port allocation for application web servers, transparent rewriting of localhost
URLs, host network mode, Docker socket mounts, and general remote-daemon support.

## Current implementation and constraints

Inspect the current code before editing; line numbers will drift:

- `src/main.rs`: clap `RunArgs` currently accepts the environment, build refresh
  options, and arguments passed to pi.
- `src/lib.rs`: `EnvManifest` uses `deny_unknown_fields`; add typed configuration.
- `prepare_environment()` / `prepare_image_recursive()` resolve an inherited
  image chain and Git identity. Runtime launch settings need their own resolved
  representation alongside this chain.
- `render_dockerfile_from()` writes both `[env]` and `[runtime_env]` into image
  `ENV` instructions. Despite its name, `[runtime_env]` is not launch-only.
- `run_invocation()` currently emits `docker run --rm -it`, host UID/GID, a
  persistent home mount, a writable `/workspace` mount, and the pi command.
  There are no custom networks or application environment overrides.
- `persistent_home_dir()` keys homes using the working-directory path and
  environment metadata. Separate worktrees already get separate homes.
- Linked Git worktrees are incomplete: their `.git` files point outside the
  workspace mount, where the shared repository metadata is unavailable.
- Tests include pure invocation tests in `src/lib.rs` and CLI tests in
  `tests/cli.rs`. Preserve these styles and add opt-in real Docker coverage.

At planning time, `README.md` and `src/lib.rs` already had uncommitted changes.
Those belong to the user; preserve them. This specification adds only this file.

## Connection model

| Service location | Session connection target | Launcher behavior |
| --- | --- | --- |
| macOS workstation | `host.docker.internal:<port>` | Use Docker Desktop DNS |
| Existing Docker network | Service name or alias, e.g. `postgres:5432` | Join that network |
| Container published to workstation | `host.docker.internal:<published-port>` | Same as workstation service; verify binding |
| Native Linux workstation | `host.docker.internal:<port>` | Explicit host-gateway mapping |

Docker Desktop documents `host.docker.internal` as the host service endpoint.
It differs from the Docker VM gateway. Avoid hardcoded bridge IPs.
See [Docker Desktop networking](https://docs.docker.com/desktop/features/networking/networking-how-tos/).

User-defined Docker bridge networks provide service-name resolution. Sessions
can join an existing Compose network using its actual Docker network name;
do not infer that name from the worktree directory. Services on the same network
use container ports, not published host ports. See
[Docker bridge networking](https://docs.docker.com/engine/network/drivers/bridge/)
and [Compose networking](https://docs.docker.com/compose/how-tos/networking/).

`localhost` and `127.0.0.1` inside a session refer to that session container.
Preserve user-provided URLs verbatim. Do not add `--publish` to enable outbound
database connections; several sessions can independently connect to port 5432.

## Proposed public configuration

Add an optional `[run]` section to `.pi-env.toml`:

```toml
[run]
networks = ["myapp_default"]
host_access = "desktop"
env_files = [".pi-session.env"]
namespace = "worktree"

[run.env]
PGHOST = "postgres"
PGPORT = "5432"
PGDATABASE = "app_{{namespace}}"
REDIS_URL = "redis://redis:6379/0"
APP_REDIS_PREFIX = "app:{{namespace}}:"
```

`APP_REDIS_PREFIX` is an example application-defined variable; the application
must implement prefixing. It is not a Redis or pi-sandbox isolation feature.
Put credentials in an explicitly selected, ignored env file. Do not auto-load
the application's `.env` because its parser and intended audience may differ.

Fields and defaults:

- `networks`: optional list of existing user-defined network names. Unset or
  empty at the root means Docker's ordinary default networking. Reject special
  modes (`host`, `none`, `container:...`) and the built-in `bridge` name in this
  field; this feature is for named user-defined networks. Deduplicate names.
- `host_access`: `desktop`, `gateway`, or `default`; default is `default`.
  `desktop` relies on Docker Desktop DNS and adds no host override. `gateway`
  adds `--add-host host.docker.internal:host-gateway`. `default` leaves host
  resolution untouched. These settings configure resolution, not an access
  control boundary. They do not disable host reachability.
- `env_files`: optional ordered list. Relative paths are resolved against the
  manifest that declares the list, including inherited manifests.
- `env`: launch-only string map; defaults empty.
- `namespace`: `worktree` by default, or an explicit validated identifier.
  This supplies a value for templates; it does not itself isolate a service.

CLI additions, all before the existing `-- <pi args...>` separator:

```text
pi-sandbox run [ENV] [existing flags]
  [--network NAME ... | --default-network]
  [--host-access default|desktop|gateway]
  [--env-file PATH ...] [--env KEY=VALUE ...]
  [--namespace NAME] [--dry-run]
  [-- <pi args...>]
```

CLI `--network` occurrences replace the resolved manifest network list.
`--default-network` explicitly clears it and conflicts with `--network`.
CLI env files append after manifest env files, resolve relative to the invoking
directory, and are applied in argument order. CLI scalar options override
manifest scalars. `--env` requires `KEY=VALUE`, splits only at the first equals
sign, allows empty values, and uses the last occurrence of a duplicate key.
There is no implicit host-variable forwarding or shell expansion.

## Inheritance and environment resolution

Keep `[env]` and `[runtime_env]` behavior unchanged for compatibility. Document
that both are image defaults and unsuitable for connection secrets. The new
`[run]` configuration must never be rendered into a Dockerfile or image hash.

Resolve inheritance parent first:

- An omitted child scalar/list inherits the parent's value.
- An explicitly present child list replaces the parent list, including `[]`.
  Use `Option<Vec<_>>` to preserve omitted versus explicitly empty.
- Merge `[run.env]` by key, with child values overriding parent values.
- Preserve each list's declaration source so inherited paths resolve correctly.

Application environment precedence, lowest to highest:

1. Image environment defaults.
2. Manifest env files, in order.
3. Resolved `[run.env]` values.
4. CLI env files, in order.
5. CLI `--env` values.

Use a deliberately small env-file grammar: UTF-8, blank lines and lines whose
first non-whitespace character is `#` ignored; other lines are `KEY=VALUE`.
Validate keys as `[A-Za-z_][A-Za-z0-9_]*`. Values are literal, including spaces,
quotes, `#`, equals signs, and dollar signs. Strip line endings, including CRLF;
do not strip value whitespace. Reject `export`, bare keys, and multiline syntax
with a filename/line-number error that omits the value. Duplicate keys use the
last occurrence. Document explicitly that this is not a shell or dotenv parser.

Expand only `{{namespace}}` and `{{worktree_id}}` in resolved `[run.env]` values.
Do not expand env files, CLI values, network names, paths, or inherited image
values. Reject unknown `{{...}}` tokens in template-enabled values. This avoids
interpreting arbitrary credential text as an expression.

Reserve `HOME`, the `PI_SANDBOX_*` namespace, and Git environment variables used
for linked-worktree support. Reject attempts to override these through launch
config; report the key without its value. Preserve ordinary overrides such as
`PATH`. Inject `PI_SANDBOX_WORKTREE_ID` and `PI_SANDBOX_NAMESPACE` for apps/tools.

## Configurable isolation

Generate a stable worktree identifier from the canonical Git worktree root
(fall back to canonical cwd outside Git): `wt_` followed by 16 lowercase hex
digits of the existing deterministic FNV-1a hash. Do not use Rust's randomized
default hasher. This identifier is a naming aid, not a security identity.

The identifier stays the same across restarts and branch changes in the same
directory; different canonical paths differ, including equal directory basenames
in separate locations. Moving a worktree changes its generated identifier.
An explicit namespace pins resource names across moves.

Accept explicit namespaces matching `[a-z][a-z0-9_]{0,39}`. Reserve the literal
`worktree` to select generated identity. Bound generated names so the example
Postgres database names fit normal identifier limits. Applications remain
responsible for constraints on longer prefixes/suffixes they add themselves.

Supported patterns:

- Shared data: set literal connection settings in an env file; all sessions can
  select the same database. Templates are optional.
- Separate databases on one server: use `PGDATABASE = "app_{{namespace}}"`,
  or configure a worktree-specific `DATABASE_URL` in an env file. Existing
  databases and grants must match those names.
- Redis namespace: configure an app-supported prefix using the namespace.
  Prefixes do not isolate Redis administrative commands, pub/sub, or every job
  framework. Applications must explicitly use the prefix for relevant data.
- Separate infrastructure: select an existing per-worktree Docker network and
  connection settings. Explicit networks/credentials remain the way to select
  stronger isolation or distinct existing Redis instances.

Do not auto-assign Redis logical database numbers or infer database names from
branch names. Do not promise isolation when two sessions use identical endpoints
and database names. Two sessions in the same worktree intentionally get the same
default namespace; users can override it for separate data if needed.

## Linked Git worktree support

This is required for the requested workflow, even though it is independent of
database networking. Detect repository metadata on the host using Git commands,
not by guessing directory layout. Obtain the absolute Git directory, common Git
directory, worktree root, and cwd relative to that root.

For a linked worktree, the proposed approach is:

1. Mount the worktree root at `/workspace`, and set the container working
   directory to the corresponding subdirectory when launched below the root.
   Keep current manifest discovery rules; do not silently add ancestor discovery.
2. Mount the common Git directory at `/tmp/pi-git`, writable so commits, refs,
   the index, and object storage can work. If the per-worktree Git directory is
   underneath it, derive its container-relative path. If it is external, mount
   it separately at `/tmp/pi-git-worktree` after validating the layout.
3. Set `GIT_DIR`, `GIT_COMMON_DIR`, and `GIT_WORK_TREE` to container paths. Never
   rewrite the host worktree's `.git` pointer or Git administrative backreferences.
4. Verify status, diff, staging, and commits in actual linked worktrees. Git
   worktree creation/removal inside the container is outside this version's
   contract. Submodules and unusual external object stores need explicit
   follow-up support rather than silently mounting unrelated host directories.

Git documents these environment overrides in its
[environment variable reference](https://git-scm.com/docs/git).
Validate this approach experimentally before settling the implementation.
Shared Git metadata is writable shared state across linked worktrees; separate
containers do not provide repository-metadata isolation. Do not mount the
entire host repository parent or sibling working directories to fix pointers.
Ordinary repositories and non-Git directories must continue to work.

## Launch implementation and diagnostics

Introduce typed `RunConfig`, `RunOverrides`, `ResolvedRunConfig`, and a
`RunContext`/equivalent struct instead of growing positional argument lists.
Separate pure configuration resolution and invocation rendering from execution.

Before building or launching:

1. Resolve manifests and launch overrides, compute identity, validate/read env
   files, and resolve mounts. No persistent-home writes for a dry run.
2. On a real run, inspect each requested network using the active Docker
   context. Missing networks fail with the requested name and advice to select
   the existing network; never create an empty lookalike network automatically.
3. Build/reuse the existing image chain, prepare the home, and launch the session.

Attach all selected networks before starting pi. Prefer repeated `--network`
arguments on a supported Docker Engine; document the tested minimum version.
If compatibility requires a create/connect/start lifecycle, implement complete
cleanup and signal handling before using that fallback. Never silently drop
secondary networks. See [Docker run reference](https://docs.docker.com/reference/cli/docker/container/run/).

Avoid fixed container names and network aliases. Preserve `--rm`, interactive
behavior, UID/GID, and command argument escaping. Construct arguments using
`std::process::Command`, not shell concatenation. Never log credential-bearing
invocations. A practical way to avoid putting values in process arguments is
to pass selected variables in the Docker child process environment and use
`--env KEY` arguments. Ensure reserved HOME/Git values still take precedence.
Docker administrators can inspect container environment values; this is not a
secret store. Do not persist resolved credentials to build cache files.

`run --dry-run` should resolve locally without contacting Docker, building,
creating directories, seeding Git configuration, or starting containers. Print
image tags, mounts, networks, host access mode, namespace, and environment key
names/provenance. Redact all environment values, not just guessed secret keys.
Label network availability as unverified. Do not print raw pi arguments if
they might contain user secrets. Keep existing `env show` Dockerfile behavior.

Do not add a mandatory database readiness probe: services and protocols are
arbitrary, and probing may require credentials. Documentation should separate
DNS failures, connection refusal/timeouts, and application authentication errors.
Network existence validation does not prove that a database is reachable.

## macOS examples and portability

Workstation Postgres/Redis, using a local ignored `.pi-session.env`:

```toml
[run]
host_access = "desktop"
env_files = [".pi-session.env"]
```

```text
DATABASE_URL=postgresql://app:example-password@host.docker.internal:5432/app_feature_a
REDIS_URL=redis://host.docker.internal:6379/0
```

The password above is a placeholder. Use actual credentials in the local file,
and add that file to the application's ignore rules. An application that only
consumes `DATABASE_URL` must receive that variable; setting `PGDATABASE` alone
does not override a database name already embedded in its URL.

Docker-hosted services on an existing network:

```sh
pi-sandbox run --network myapp_default --env-file .pi-session.env
```

Here the URLs use the actual service aliases, such as `postgres` and `redis`.
Repeat `--network` when services occupy different networks. Avoid ambiguous
duplicate aliases across attached networks. External connectivity also depends
on the selected networks; an internal-only network may prevent pi API access.

Native Linux uses `host_access = "gateway"` or `--host-access gateway`.
The mapping targets the Docker host gateway, not that host's loopback listener.
Services must listen on an address reachable from the bridge, with appropriate
firewall and service authentication settings. Do not recommend opening services
to all interfaces without explaining the exposure. See
[Docker host-gateway configuration](https://docs.docker.com/reference/cli/dockerd/).

Docker Desktop on Linux should use `desktop`. Do not select gateway behavior
solely from the client's OS: Docker context and daemon location matter. On a
remote daemon, host-gateway means the remote host, and local workspace paths
are not automatically available there. Remote contexts, Colima/other VM
runtimes, rootless networking, and Windows hosts are not first-version verified
targets; document this rather than claiming portability from hostname support.

## Implementation sequence

1. Add typed manifest and CLI configuration, validation, precedence, and tests.
2. Resolve launch settings alongside inherited images without changing image
   contents; implement namespaces and literal env-file parsing.
3. Add Docker networks, host mapping, environment transport, preflight checks,
   and redacted dry-run output.
4. Implement and verify linked-worktree mounts and Git operation support.
5. Add opt-in Docker integration tests and run the macOS acceptance scenarios.
6. Update README with configuration, isolation limitations, migration guidance
   for connection values currently in `[runtime_env]`, and Linux instructions.

## Verification and acceptance criteria

Unit/CLI tests must cover:

- Existing manifests and commands retain their behavior.
- Parent/child inheritance, explicit empty lists, declaration-relative paths,
  CLI replacement/clearing, and every environment precedence boundary.
- Literal env parsing, CRLF, duplicate keys, empty values, equals signs, invalid
  keys, missing files, unknown template tokens, and reserved-variable rejection.
- Stable identity across restarts/branch changes; distinct equal-basename paths;
  explicit namespace validation and override behavior.
- Launch-only config edits do not change image tags or Dockerfile output.
- Docker flags precede the image; pi arguments are preserved; Desktop mode
  does not override its built-in hostname; gateway mode adds the expected map.
- Dry run has no Docker/filesystem mutations and contains no secret values.
- Errors redact secrets and no raw environment-bearing invocation is logged.

Opt-in Docker integration tests should use temporary worktrees, uniquely named
test networks/containers, and disposable Postgres/Redis fixtures. Test fixtures
may provision resources; production launching must not. Reuse launch rendering
with a noninteractive test command so tests require no pi credentials or TTY.
Track exact fixture IDs and clean up only test-owned resources, including on
failure. Never use global Docker prune or a user's existing database for tests.

Acceptance scenarios:

1. Two simultaneous sessions in separate linked worktrees attach to one existing
   network and connect by service name to the same Postgres and Redis services.
2. Shared configuration sees a shared test record. Separate pre-created Postgres
   databases see only their own records. An example Redis client implementing
   prefixes demonstrates separate keys and documents the weaker guarantee.
3. Closing one session leaves the other session and external services running.
   Relaunching a worktree selects the same namespace and retains its data.
4. Changing URLs, namespace, or networks reuses the same environment image.
5. Workstation service access succeeds through `host.docker.internal` on the
   Mac. Test actual local services or a dedicated host listener, not only
   another container on the same network.
6. Git status, diff, add, and commit work independently in two linked worktrees;
   host Git remains functional and host `.git` pointers remain unchanged.
7. Native Linux gateway behavior is tested on Linux when available. If no Linux
   runner is available, record it as documented but unverified; argument tests
   alone do not establish host connectivity.

Run `cargo test` and appropriate formatting/lint checks for implementation
changes. Record Docker client/server versions and which platform scenarios
were actually exercised. Completion requires the Mac workflow and simultaneous
worktree scenarios, not just correct-looking Docker argument strings.
