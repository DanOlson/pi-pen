# Networking and programmatic invocation validation

Validated locally on macOS with Docker Desktop 4.36.0, Docker client/server
27.3.1, and Linux/arm64 containers. No remote branch push or deployment is part
of this work.

## Delivered interfaces

- Existing interactive `run` supports named Docker networks, Desktop host DNS,
  explicit Linux gateway mapping, launch-time environment configuration,
  redacted dry runs, and stable worktree namespaces.
- Linked worktrees receive their own workspace/home with deliberate shared Git
  metadata mounts. Connection configuration selects already-running resources;
  it does not provision databases or enforce application data isolation.
- `exec`, `rpc`, and `stop` provide noninteractive worker processes, isolated
  session artifacts, explicit resumption, cancellation, and timeout handling.
  The orchestrator owns scheduling, worktree creation, and integrating changes.

The design specifications remain in [networked resources](networked-resources-plan.md)
and [programmatic invocation](programmatic-invocation-plan.md). See the README
for implemented command syntax and runtime limits.

## Automated checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --test docker_network -- --ignored --nocapture
```

The normal suite currently contains 39 unit tests, 20 CLI tests, and 14
independent headless process regressions. The four opt-in Docker tests were
also executed locally. They cover:

- Two actual linked worktrees resolving different homes and stable namespaces,
  accessing Postgres and Redis across two existing networks, and selecting
  shared or separate data. Stopping one leaves its peer and services alive.
- `host.docker.internal` access to a real host TCP listener and to published
  Postgres/Redis ports.
- Concurrent Git status, staging, and commits in linked worktrees, preserving
  host Git pointers and normal host Git operations.

Independent CLI regressions cover stream purity, authoritative final text,
agent error/recovery semantics, empty or malformed events, private artifact
permissions, symlink containment, network preflight ordering, create failures,
preparation timeouts, cleanup failure, and a broken terminal stdout pipe.
The final regressions also cover oversized Docker creation output with an
already-created container and a descendant retaining a pipe past its parent's
exit; both preserve the correct cleanup and exit-status contract.

## Actual pi protocol compatibility

`tests/real_pi_compat.py` runs actual pi 0.85.1 against a local deterministic
OpenAI-compatible test server. It verifies successful print/JSON operation,
explicit session resumption, two sequential RPC prompts, and orderly EOF.
No external model credentials or paid inference are used.

Run it with a local image that contains the pinned pi release:

```sh
PI_REAL_IMAGE=pi-sandbox-env:50ff6cf9183b3133 python3 tests/real_pi_compat.py
```

The image tag above is the image used on this workstation; substitute another
image containing pi 0.85.1 elsewhere. The fixture verifies the actual version,
uses an ephemeral local port and temporary session state, and cleans them up.
Basic missing-credential behavior was also checked against the actual package.

The reusable real-Docker lifecycle harness exercises three linked-worktree
workers at once, exact-target cancellation, unchanged resume sources, private
persisted artifacts after container removal, and exec/RPC cleanup. It uses a
deterministic worker fixture and dedicated configuration/cache directories:

```sh
cargo build
python3 tests/docker_headless.py
```

## Practical limits

- Native Linux gateway mode is documented and argument-tested; it was not
  exercised on a Linux workstation in this Mac-only validation.
- Paid external-provider inference was not exercised because no API credentials
  were configured. The local compatibility fixture tests actual agent protocol
  behavior, not provider availability or model quality.
- Custom `pi_install` definitions are explicitly unverified; they may emit
  incompatible records. The default release is pinned and verified.
- The orchestrator must drain output streams. Protocol stdout has a one-second
  no-progress limit and records have a 32 MiB maximum; diagnostics remain in
  the artifact log if stderr cannot keep up.
- Container separation does not isolate shared Git metadata or shared service
  data. Database names, application Redis prefixes, networks, and credentials
  determine the chosen resource isolation.
- SIGKILL and workstation crashes can leave an orphan. Retain its output
  directory and use ownership-checked `stop` to reconcile it.

## Review record

Separate Sol implementation and review work was used for both features.
Networking received final approval at `258e5f1`; headless received final approval
at `3c6627e`. All blocking review findings were fixed and rechecked. The combined
integration branch passed the checks above before the local merge into main.
