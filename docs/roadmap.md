# Roadmap

Ideas that came out of real orchestration runs but are not yet scheduled. Each
entry records the problem it would solve and what evidence would justify
building it, so the decision can be made later without re-deriving context.

## `pi-sandbox status`: a progress view for running workers

**Problem.** During an unattended batch, the only ways to answer "is this
worker stuck or working?" are tailing `events.jsonl` (dominated by
`message_update` token deltas and `tool_execution_update` partial output),
`docker ps`, or waiting for `result.json`. An orchestrating agent that wants
to narrate progress ends up writing its own JSONL parser.

**Shape.** `pi-sandbox status --output-dir RUN` for one run and
`pi-sandbox status --batch DIR [--watch] [--json]` for every run under a batch
directory. One line per run:

```text
implement-01  running    14m32s  bash: cargo test --lib (2m10s)  last: "Added regression test for…"
review-01     completed   6m05s  exit 0  cleanup ok
review-02     failed      0m03s  no saved pi credentials in home …
```

Derived entirely from artifacts the launcher already writes: `run.json`
(state, timestamps, `container_id`, `persistent_home`), the tail of
`events.jsonl` filtered to `tool_execution_start`/`tool_execution_end`,
`turn_*`, `agent_*`, and `message_end`, plus `result.json` when present.
Optionally `docker inspect <container_id>` to confirm a `running` worker is
alive. No new launcher-side writes are required, so it can be built without
changing the run contract.

**Not in scope.** Aggregating worker outputs (responses, diffs, review
findings) across sessions. That is task-specific orchestrator work and a
generic merge would be the wrong altitude. `result.json` plus exit status
remains the completion contract.

**Decide when.** After a few more real batches. Signals that it is worth
~200 lines: a human regularly sits and watches batches rather than firing and
checking back; an orchestrating agent repeatedly parses `events.jsonl` by hand
to report progress; or a hung worker goes unnoticed because nothing surfaced
the elapsed time of its last tool call. Until then the `jq` filter in the
skill's orchestration reference covers most of the need.

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
If built, it should follow the `status` view, whose `run.json`/`events.jsonl`
parsing `wait` and `status` would reuse.
