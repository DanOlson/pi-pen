# Roadmap

Ideas that came out of real orchestration runs but are not yet scheduled. Each
entry records the problem it would solve and what evidence would justify
building it, so the decision can be made later without re-deriving context.

## Declarative `pi-sandbox orchestrate --jobs jobs.json`

Stages with `depends_on`, replacing a bespoke Python controller per task.
This is the largest lever on per-command approval friction in sandboxed
harnesses (one file plus one command instead of a new script per batch), but
it moves task decomposition partly into the binary, against the skill's
current "controller owns decisions" stance. Prototype as an example script
first; promote to a subcommand only if the example stops needing per-task
edits.

## In-container orchestration through a host-side broker

**Status: undecided, possibly not worth it.** Recorded so the constraints are
not rediscovered if it comes up again.

**Problem.** A worker container has no way to launch siblings: it gets only
the workspace, Git administrative directories, and persistent home mounts, and
`host_access` provides a name for the host with nothing listening on it. The
orchestrator therefore has to be a host process, and in sandboxed harnesses
every launch it makes is a separate approval. Moving the orchestrator inside
a container would make one approval (`exec --broker`) cover a whole batch and
would sandbox the orchestrator itself.

**Shape.** `exec --broker` opens a Unix socket in the run's output directory
and bind-mounts it into the container. A small client on the container `PATH`
(`pi-sandbox-worker spawn|status|wait|result|stop`) talks to it from pi's
normal bash tool, so no pi extension is required. The broker executes a fixed
menu on the host: `spawn` creates a detached linked worktree of the mounted
repository under a broker-owned directory and runs an ordinary `pi-sandbox
exec` there; the others read `run.json`/`result.json` or call the existing
targeted `stop`. Children are siblings, not nested containers, so the
isolation model is unchanged and the container never sees Docker.

**Constraints that make it safe by construction, not by prompt.** The socket
is a capability available to anything running in the container, including code
a worker just cloned and executed, so the broker must be narrow:

- Same repository only: worktrees come from the mounted common Git directory;
  no arbitrary paths, no arbitrary host commands, no host shell.
- Prompts travel over the socket and are written by the broker into the batch
  directory; the child never receives a host path from the parent.
- Namespace, model options, env files, and network settings are inherited from
  the parent's launch config and cannot be requested per child.
- Depth limit (children get no broker socket by default), per-run concurrency
  cap, and a total-runs budget so a confused agent cannot fork-bomb the Docker
  daemon or the API bill.
- Every child is a normal run directory under the parent's batch directory, so
  `stop`, retention, auditing, and the completion contract still apply.

**Residual risk.** A compromised orchestrator can already push refs through
the writable Git common directory mount; a broker adds the ability to spend
CPU and API budget on N children, which the budget bounds.

**Why it may not be worth it.** Roughly 600 to 1000 lines of security-critical
Rust plus a new binary in every image, a socket protocol, and worktree
creation moving into the launcher, which is currently and deliberately
unsupported. A declarative `orchestrate --jobs` command or better harness
permission presets may recover most of the approval savings for far less.
If built, `wait` and `status` should reuse the `run.json`/`events.jsonl`
parsing in `src/status.rs`.
