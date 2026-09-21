# Programmatic usage

Use `exec` for one task per worker and `rpc` for a long-lived, interactive agent
protocol without a terminal. Both run inside Docker. This guide describes the
implemented interface; the implementation plans are design history.

## Prerequisites and authentication

- Start Docker (Docker Desktop on macOS), install `pi-sandbox`, and prepare a
  manifest with the dependencies your project needs.
- Build once before starting a batch: `pi-sandbox build /absolute/path/.pi-env.toml`.
  Workers using the same build configuration reuse its image.
- Explicitly supply provider credentials. Exporting an API key in the host shell
  does **not** automatically forward it into the container. Use a private,
  Git-ignored env file, or use the same explicit namespace for an interactive
  authentication run and every worker so linked worktrees share the namespace's
  persistent container home. The default `worktree` namespace keeps logins
  separate.

For example, a file `/absolute/private/provider.env` can contain:

```dotenv
OPENAI_API_KEY=your-key-here
```

Restrict it with `chmod 600 /absolute/private/provider.env`. Env files use literal
`KEY=VALUE` lines: no `export`, shell expansion, or surrounding quotation marks.
Never put secrets in `[env]` or `[runtime_env]`; those become image defaults.
Avoid secrets in CLI arguments, which may appear in process listings.

## One task

Run from the target worktree. Prepare `/tmp/task.txt` with your prompt, then:

```sh
mkdir -p /tmp/pi-runs
pi-sandbox exec /absolute/path/.pi-env.toml \
  --prompt-file /tmp/task.txt \
  --env-file /absolute/private/provider.env \
  --output-dir /tmp/pi-runs/task-001 --timeout 900 \
  -- --provider openai --model gpt-5
```

Choose a provider/model available to your account. Pi options go after `--`;
provider, model, thinking, tools, extensions, and `--no-tools` are supported.
Launcher-owned mode/session flags, positional prompts, attachments, and unknown
options are rejected. `--prompt-stdin` is an alternative to `--prompt-file`.
Prompts must be nonempty UTF-8, at most 8 MiB.

Each output directory must be new or empty, with an existing parent, and outside
all container mounts: workspace, persistent home, and Git metadata. Never reuse
an old run directory. The launcher validates these boundaries. To let the
launcher pick the directory, pass `--new-run-under /tmp/pi-runs --run-name task`
instead of `--output-dir`; it creates `task-01`, `task-02`, ... and prints the
chosen path as the first stderr line.

## Parallel worktrees

A headless worker exclusively locks its canonical worktree. Use distinct linked
worktrees, not multiple subdirectories or symlinks into one worktree. From your
project repository, for example:

```sh
git worktree add -b agents/explore /tmp/project-explore HEAD
git worktree add -b agents/implement /tmp/project-implement HEAD
git worktree add -b agents/review /tmp/project-review HEAD
```

Create `/tmp/jobs.json`:

```json
[
  {"name":"explore","worktree":"/tmp/project-explore","prompt":"Inspect the architecture and report risks. Do not edit files."},
  {"name":"implement","worktree":"/tmp/project-implement","prompt":"Implement the requested feature and run relevant tests."},
  {"name":"review","worktree":"/tmp/project-review","prompt":"Review the existing code for correctness risks. Do not edit files."}
]
```

Run the example from this repository (or use its absolute script path):

```sh
python3 examples/orchestrate.py \
  --jobs /tmp/jobs.json --output-root /tmp/pi-runs/batch-001 \
  --manifest /absolute/path/.pi-env.toml \
  --env-file /absolute/private/provider.env \
  --namespace batch_shared --timeout 900 \
  -- --provider openai --model gpt-5
```

Worktree paths are relative to the jobs file unless absolute. Other paths are
relative to the invoking shell. `--binary /absolute/path/to/pi-sandbox` selects a
local build. Omit `--manifest` to use normal manifest discovery in each worktree.
The batch root must not already exist; its parent must exist.

A named namespace also selects a persistent home shared by linked worktrees in
the same repository and environment. Initialize provider authentication or
home-installed dependencies once with `pi-sandbox run --namespace batch_shared`,
then give the example the same namespace. Declaring `[run] namespace =
"batch_shared"` in the manifest makes every launch inherit it instead. Check
what a worktree resolves to, and whether that home holds credentials, with
`pi-sandbox namespace show`. Because that home includes mutable
package caches and install locations, complete dependency provisioning before
parallel work and serialize package-manager writes to shared state.

The example launches all jobs concurrently, redirects each worker's stdout and
stderr to separate private files, and prints one JSON summary per job afterward.
Regular files avoid bounded pipe deadlocks and unbounded in-memory event buffers.
If you replace files with pipes, continuously drain **both streams for every
worker concurrently**. Never wait for worker A before reading worker B. A stdout
write stalled for one second fails the worker. Plan disk capacity and retention:
artifacts and these extra stream copies can be large and contain secrets.

The script exits 0 only if every child exits 0 and its result says `completed`,
1 for a failed batch, and 130/143 when interrupted. One worker failure does not
cancel the others. SIGINT/SIGTERM terminates active launchers, waits for cleanup,
and attempts targeted `stop` if a launcher does not exit. Worktrees and outputs
are preserved. This is a small batch example, not a persistent job scheduler.

All three example jobs run concurrently, which is why each has its own
worktree; the `review` job reviews the existing code, not the new feature. A
review of the newly implemented feature must be a **subsequent** job, after the
implementation is committed. Run it in the implementation worktree: a headless
run releases its worktree lock on exit, and the reviewer then sees the commit
and anything left uncommitted. A separate detached worktree is only needed for
reviewers that run at the same time. The orchestrator must manage dependencies,
inspect diffs, run checks, and decide whether to merge; `completed` does not
mean the feature is correct. No automatic merge is performed.

Containers do not automatically isolate external databases. Configure `[run]`
networks, env files, and namespace-specific connection settings separately in
each worktree when isolation is needed; see [networked resources](networked-resources-plan.md).
Services must already be running.

## Results and failure handling

`exec` stdout is JSONL: envelopes have `schema_version`, `type`, `run_id`, and
monotonically increasing `seq`. Upstream events are nested in `pi_event.event`;
`run_finished` reports the terminal outcome. RPC stdout is raw upstream JSONL,
not these envelopes. Maximum protocol record size is 32 MiB.

Use process exit status **and** the atomic `result.json` as the completion
contract. Early validation failures may have no artifacts or terminal event:
always retain launcher stderr and handle a missing result. Do not treat a
partially written run, an RPC acknowledgment, or an available response as success.

| Exit | Meaning |
| --- | --- |
| 0 | Successful completion (RPC: successful transport lifecycle) |
| 1 | Worker/model/protocol failure |
| 2 | Usage or configuration error |
| 3 | Launcher, Docker, or I/O failure |
| 124 | Timeout |
| 130 | SIGINT or explicit stop |
| 143 | SIGTERM |

Inspect `status`, `reason`, `wrapper_exit_code`, `container_exit_code`, and
`cleanup_succeeded` in the result. A cleanup failure is not safe to ignore.
Artifacts include:

- `run.json`: run identity, container identity, and lifecycle metadata.
- `result.json`: terminal outcome and `session_files` for follow-up.
- `response.txt`: extracted exec response, if available.
- `events.jsonl`: durable protocol events; `stderr.log`: durable diagnostics.
- `sessions/`: isolated conversation files.

Directories are private (0700), files 0600. `usage`, when present, describes the
final assistant message, not total session usage. Do not blindly retry a failed
agent: its filesystem or database side effects may already have happened.

## Watching progress

`events.jsonl` is dominated by token deltas and partial tool output, so it is
a poor progress view. `status` derives one line per run from `run.json`, the
significant events, and `result.json`:

```sh
pi-sandbox status --batch /tmp/pi-runs/batch-001
pi-sandbox status --output-dir /tmp/pi-runs/task-001 --json
```

```text
implement-01  running     14m32s  bash: cargo test --lib  idle 2m10s  last: "Added regression test for…"
review-01     completed    6m05s  exit 0  cleanup ok
review-02     failed       0m03s  exit 1  worker_exit  cleanup ok  warning: no saved pi credentials…
```

A running line shows the tool call still executing (or `awaiting model`
between tool calls), how long `events.jsonl` has been quiet, and the last
assistant text. Pi events carry no timestamps, so `idle` is the age of the last
event write, not the age of the tool call. A terminal line shows the wrapper
exit code, the reason when not `completed`, cleanup state, and the last
`stderr.log` line for failures. `--batch DIR` lists every subdirectory with a
`run.json`; `--watch` redraws every `--interval` seconds until all runs are
terminal; `--json` prints the same fields for a controller (one snapshot per
line with `--watch`); `--inspect` additionally asks Docker whether each running
worker's container is alive, which is how a launcher killed with SIGKILL shows
up: `run.json` still says `running` while the container is missing or exited.

## Cancellation and continuation

To cancel one worker without touching others:

```sh
pi-sandbox stop --output-dir /tmp/pi-runs/batch-001/implement --grace 10
```

You can also send SIGINT/SIGTERM to its launcher. `stop` verifies Docker context,
endpoint, daemon identity, and exact run ownership before touching the container.
Keep the run directory for reconciliation after a crash or SIGKILL; those cannot
guarantee cleanup. A missing result is not proof the container stopped.

For a follow-up, select the actual session path from `result.json.session_files`
and use another fresh output directory:

```sh
pi-sandbox exec /absolute/path/.pi-env.toml \
  --prompt-file /tmp/follow-up.txt \
  --resume-session /actual/path/from/result/session.jsonl \
  --env-file /absolute/private/provider.env \
  --output-dir /tmp/pi-runs/follow-up-001 --timeout 900 \
  -- --provider openai --model gpt-5
```

The source session is cloned, not modified. This restores conversation history,
not a worktree snapshot or database state; choose the working tree deliberately.

## RPC clients

For a runnable state-query example:

```sh
python3 examples/rpc_state.py /absolute/path/.pi-env.toml \
  --output-dir /tmp/pi-runs/rpc-001 --timeout 60
```

Set `PI_SANDBOX_BIN` to an absolute local binary path if needed. This example
sends `{"id":"state","type":"get_state"}`, flushes stdin, reads until the
matching response, and **only then** closes stdin and drains remaining stdout.
Stderr is inherited, not captured in an unread pipe. Run it from the intended
worktree. For prompts, provide credentials and model options as with `exec`.

A full client must keep stdin open throughout pending commands/turns, correlate
responses by ID, consume asynchronous events continuously, and distinguish
command acceptance from successful turn completion (including retries and model
errors). EOF requests bounded shutdown; piping one command and immediately
closing stdin is not a reliable request/response client. RPC process exit 0 is
not evidence that every prompt succeeded. Use `exec` if you do not need this
interactive protocol complexity.

## Portability and validation

The examples use Python 3's standard library, Git, and POSIX signals; they target
macOS and Linux, not native Windows. Docker Desktop host services use
`host.docker.internal`; native Linux typically requires gateway host access and
services listening on a container-reachable address. See the networking guide
for security and isolation considerations.

Run `python3 tests/test_orchestrate.py` for local example regression tests and
`python3 tests/docker_headless.py` after `cargo build` for opt-in real-Docker
lifecycle acceptance without provider credentials.
