# Programmatic invocation and orchestration

Status: original implementation specification. See
[validation and limits](implementation-validation.md) and the README for the
delivered interface and the refinements made during review.
Written September 19, 2026.

## Goal

An orchestrator should launch several independent pi agents in containers to
explore, implement, and review features; stream their progress; collect results;
and cancel or follow up on individual sessions. Each worker operates in an
explicit worktree and receives its own task and connection configuration.

This is feasible. The underlying agent already supplies the headless interfaces.
The work in pi-sandbox is to expose them with dependable process management,
stream handling, state ownership, and error reporting.

This plan complements [networked resources](networked-resources-plan.md). Reuse
its launch-time environment configuration, network selection, namespace rules,
and linked-worktree Git support. Neither plan is implemented yet. Preserve
existing user changes in `README.md` and `src/lib.rs` when implementing.

## Verified upstream capabilities

Pi JSON mode emits newline-delimited session events. Final message events carry
authoritative message content; consumers should not assume every streaming event
contains a cumulative snapshot. See the upstream
[JSON mode specification](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/json.md).

Pi RPC mode accepts commands and streams responses/events over stdin/stdout.
It supports prompts, steering, follow-ups, abort, and state queries. Command
acceptance is distinct from eventual task completion. Records use LF framing;
Unicode line separators inside strings are not record delimiters. See the
[RPC specification](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md).

Upstream sources were inspected on the document date, but `main` is mutable.
During implementation, select and test a released pi version, record its exact
version and relevant protocol fixtures, and pin the default install command to
that version. The current default installs an unpinned npm package, and cached
images can therefore contain different pi versions despite identical generated
Dockerfiles. A version pin changes the Dockerfile and its content-addressed tag.
Custom `pi_install` remains supported, but protocol compatibility must be checked
and recorded rather than assumed.

## Responsibility boundary

The orchestrator owns task decomposition, worktree/branch creation, concurrency
limits, dependency scheduling, retries, review assignment, validation of the
agent's work, and integrating approved changes. It supplies an existing worktree
as the subprocess working directory. It also owns any database provisioning.

Pi-sandbox owns one worker container per invocation, launch configuration,
headless I/O, explicit session selection, cancellation, timeout enforcement,
local run metadata, and preserving worker output after container removal.

Explore/implement/review are task intents, not new privileged execution modes.
Use separate worktrees, prompts, and appropriate pi tool selections. A prompt
asking for review does not make a writable workspace read-only. Do not claim a
tool allowlist is a security sandbox. Enforced read-only review environments can
be a later feature with explicit filesystem and Git-metadata policy.

No daemon, hosted queue, automatic merge/commit, browser UI, or containerized
orchestrator is required for the first version. The orchestrator runs on the
workstation and invokes the CLI. Remote daemon and Docker-in-Docker operation
remain outside the first version.

## Public CLI

Keep `pi-sandbox run` interactive and compatible. Add separate commands so a
caller cannot accidentally open a TUI because of missing pi arguments:

```text
pi-sandbox exec [ENV] [shared launch flags]
  (--prompt-file PATH | --prompt-stdin)
  --output-dir PATH
  [--timeout SECONDS] [--resume-session PATH]
  [-- <pi options...>]

pi-sandbox rpc [ENV] [shared launch flags]
  --output-dir PATH
  [--timeout SECONDS] [--resume-session PATH]
  [-- <pi options...>]

pi-sandbox stop --output-dir PATH [--grace SECONDS]
```

Shared launch flags include the existing build controls and the networking
plan's environment/network/namespace overrides. Reuse one resolver. `exec` and
`rpc` are always noninteractive; do not infer their mode from whether stdin is a
terminal. Default timeout is unlimited; accept positive integer seconds only.
Recommend explicit timeouts in orchestrator examples.

`exec` performs one initial assignment and exits when pi terminates. Internally
use the tested pi print/JSON invocation (`pi --print --mode json`), with prompt
input piped to stdin. Confirm piped prompt behavior against the pinned release.
Read the supplied prompt exactly once, forward it without shell interpretation,
and close the container input after sending it. Reject empty prompts and invalid
UTF-8 before creating a container. Bound initial prompt size at 8 MiB and report
oversize input clearly. Treat prompt files as local input, not container paths.

`rpc` keeps a session process alive for the caller to converse with. Internally
launch `pi --mode rpc`; stdin is exclusively protocol input, so it has no prompt
file/stdin switches. The orchestrator supplies LF-delimited messages, flushes
writes, and drains output continuously.

The launcher owns pi mode, prompt delivery, and session-path flags. Reject
conflicting passthrough flags, including alternate modes, positional prompts,
`@file` prompt attachments, implicit continue/resume selectors, and launcher-
managed session directories. Validate aliases and `--option=value` spellings.
Implement this against the pinned pi option schema, including value arity; do
not naively treat values of options such as `--model` as positional prompts.
Document supported passthrough options and fail closed on unknown options until
their parsing is supported. Provider, model, thinking, and tool options should
be available from the first version. Add multimodal input later if needed.

## Streams and artifact contract

The caller must consume stdout and stderr concurrently. Pipe all Docker build
and launcher diagnostics to stderr. Existing image-reuse messages currently go
to stdout and must be routed appropriately in machine modes. No ANSI progress,
TTY prompts, banners, or shell setup output may appear in protocol stdout.

For `exec`, stdout is a pi-sandbox JSONL stream with a versioned envelope:

```json
{"schema_version":1,"type":"run_started","run_id":"uuid","seq":1}
{"schema_version":1,"type":"pi_event","run_id":"uuid","seq":2,"event":{"type":"agent_start"}}
{"schema_version":1,"type":"run_finished","run_id":"uuid","seq":3,"status":"completed","exit_code":0}
```

These are illustrative records, not a complete event sequence. Document the
exact schema in the implementation. Require monotonically increasing per-run
sequence numbers and allow additive fields. Preserve unknown valid pi event
objects inside the envelope; do not discard newer event types. Invalid pi JSON
is a protocol failure, not text to silently wrap as success. Emit one terminal
record for handled termination when stdout is writable. Artifact state remains
the source of truth if stdout is lost. CLI parse errors may occur before a run
exists and need not produce JSON.

For `rpc`, stdout preserves the upstream pi protocol without wrappers or launcher
events. This supports existing RPC clients. Launcher lifecycle metadata belongs
in artifacts and stderr. Do not name this JSON-RPC 2.0: use pi's actual protocol.

Require a fresh `--output-dir`: either nonexistent or an empty directory. Resolve
relative paths against the invoking cwd. Atomically claim it before starting;
reject concurrent reuse and never overwrite an earlier run. Generate a random
run ID independently of the directory name and worktree identity. Store:

```text
run.json         # launcher-owned identity, image/version, cwd, lifecycle state
result.json      # final structured outcome, written atomically
events.jsonl     # exec envelope stream, or received raw RPC events
stderr.log       # worker/build diagnostics
response.txt     # exec final assistant text, if any
sessions/       # pi session files, persisted outside the container
```

Keep launcher-owned metadata outside container mounts. Bind only `sessions/`
into the container at `/tmp/pi-sessions`. Mount any explicit resume session in
its own controlled location as described below. Artifacts have restrictive
permissions and may contain source code, prompts, credentials echoed by tools,
or model output. Never put raw secrets, prompt text, or full argument vectors
in `run.json`. Do not promise that arbitrary agent output is secret-redacted.

Stream incrementally and flush records; never buffer the full job in RAM. Use
bounded queues and a documented maximum protocol-record size (initially 32 MiB).
Fail explicitly on oversized records. Do not drop events under backpressure.
Artifact write failures, full disks, and broken stdout pipes trigger controlled
termination, with best-effort recording of the failure. Continuously drain
stderr to prevent deadlocks; prevent log draining from delaying timeout signals.

## Lifecycle, completion, and exit codes

Use these launcher states:

```text
preparing -> starting -> running -> stopping -> terminal
```

Terminal outcome is one of `completed`, `failed`, `cancelled`, or `timed_out`.
`completed` means the worker process ended normally according to its mode; it
does not certify that the feature is correct or that tests passed. Preserve
structured pi/model errors separately from Docker/launcher failures. A failed
tool call may be recoverable during a task and is not automatically a failed job.

For one-shot execution, require normal process exit, a well-formed event stream,
and no final unrecovered agent error. Do not treat the first `agent_end` as
process completion, nor any prose such as "done" as a success signal. Verify
actual error/abort/stop-reason behavior in the pinned release with fixtures.
Capture final assistant text from authoritative final message events; do not
concatenate every streaming delta across turns. Allow a successful session with
no text response and leave `response.txt` empty in that case.

Proposed wrapper exit codes:

| Code | Meaning |
| --- | --- |
| 0 | Normal completion or orderly RPC connection closure |
| 1 | Worker/model/protocol failure |
| 2 | CLI or local configuration error |
| 3 | Launcher, artifact I/O, build, or Docker failure |
| 124 | Timeout |
| 130 | Cancelled by SIGINT or explicit stop |
| 143 | Cancelled by SIGTERM |

Record raw Docker/container exit status and signal independently in `result.json`
along with timestamps, run ID, selected image and pi version, terminal reason,
session references, and available usage. Unknown usage/cost is null, not zero.
Do not calculate totals by summing cumulative streaming counters. Document that
the `rpc` transport exit status does not summarize the success of all prompts
sent through it; the orchestrator interprets each prompt's events.

Refactor `run_cli()`/`main()` to return deliberate process statuses rather than
collapsing every nonzero child status into a generic `anyhow` exit.

## Container ownership and cancellation

Use a random run ID in container labels and an exact container ID in metadata.
Do not use a worktree name as the unique container identity. Avoid global Docker
cleanup operations and do not let cancellation affect other workers or services.

Prefer `docker create` followed by attached `docker start` for headless modes:
this provides a container ID before starting the agent and supports connecting
all required networks before execution. Keep stdin available, never allocate a
TTY, and use Docker's init support to reap descendant processes. Execute pi as
the application process, preserving argument boundaries through any setup
entrypoint. Test attach behavior and stdout/stderr separation on Docker Desktop.

The lifecycle supervisor owns explicit cleanup. Do not rely solely on `--rm`,
because inspection is needed before removal. On exit, collect container state,
write outcome artifacts, and remove only this run's container. Partial create,
network attach, build, and start failures must be covered. Store metadata
atomically as soon as the exact ID is available.

Timeout starts after local argument validation and includes build/start time.
On timeout or termination signal, stop the exact worker container, allow a
default 10-second grace period, then force-kill/remove if necessary. Interrupt
any launcher-owned build process too; do not kill unrelated builds. Preserve
worktree changes and completed artifacts. Cleanup failure is reported with the
container ID and must not be reported as successful cleanup.

`stop` verifies ownership labels against the run record before acting. Record
the explicit cancellation request atomically so the supervising process can
distinguish it from an unexpected container death. Stopping an already-terminal
run is idempotent. Validate Docker context/endpoint identity so a changed default
context cannot redirect cleanup. Store no TLS credentials in metadata.

If the supervisor receives SIGKILL or the host crashes, cleanup cannot be
guaranteed. The retained run record and ownership labels must allow `stop` to
clean up an orphan afterward. Report stale state as unknown until reconciled
with Docker; do not claim a finished outcome from a missing local process alone.

## RPC-specific behavior

Forward input/output concurrently without injecting protocol requests from the
launcher. Request IDs and task queues belong to the orchestrator. Before issuing
work, the client can perform a state query to confirm responsiveness. A prompt
acceptance response does not mean the work succeeded. Steering and follow-up
commands support ongoing direction without recreating the container.

To cancel a turn while keeping the session, the client should clear pending
queues before aborting when it intends all queued work to stop. To end the whole
worker, use launcher cancellation. Upstream documents that abort can otherwise
leave queued work eligible to continue. See the
[RPC cancellation commands](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md).

Define stdin EOF as end of the RPC connection: initiate worker shutdown even if
upstream would remain alive, using the bounded stop procedure. An orderly EOF
can yield exit 0, but record `connection_closed` and do not label any in-flight
task successful. Unexpected worker exit before client EOF is a failed connection.
Test pending tasks, slow consumers, partial records, and abrupt client death.

Extension requests for UI input need client handling according to the pinned
protocol. Do not synthesize approval responses. Unsupported requests should
produce a clear client error and cancellation rather than an unattended hang.

## Sessions, credentials, and concurrent worktrees

Start a fresh pi conversation by default. Never use "continue most recent" or
an interactive session picker in orchestrated mode. Store each run's sessions
in its output directory so unrelated runs cannot resume each other accidentally.

`--resume-session PATH` requires an exact existing session file. Clone it into
the new run's session directory before opening it with pi, so parallel follow-up
runs cannot write the same history. Record the source reference; do not modify
the original artifact. Confirm the pinned pi version's session path and forking
semantics and document that follow-up artifacts belong to the new run. Resuming
history does not restore filesystem contents or database state.

Reuse per-worktree persistent homes from the existing launcher initially.
Require distinct worktrees for parallel workers; add a per-canonical-worktree
exclusive execution lock for headless runs to prevent accidental concurrent
edits and home mutation. Acquire before seeding Git config; release on completion.
Use an OS-backed lock rather than PID-only stale-lock detection. Document that
existing interactive runs do not participate unless updated to share the lock.
Concurrent worktrees still share Git refs/objects; integration remains the
orchestrator's responsibility.

Provision authentication before launch through the networking plan's explicit
launch-time env mechanism or an already-authenticated per-worktree home. Do not
implicitly copy the host's complete home, auth files, or Docker socket. Missing
credentials must fail without a login/browser prompt. Do not claim to validate
every provider's credentials locally; use bounded startup and model error handling.

The orchestrator should prebuild shared images before launching a batch. Also
make cached Dockerfile writes atomic and serialize same-tag build preparation
with a per-image lock: parallel cold starts must not race on the current shared
cache path. Recheck image existence after acquiring the lock. Different image
builds should remain independent.

## Example orchestration

These are proposed commands, not commands available today:

```sh
# Run with cwd set to a dedicated worktree by the orchestrator.
pi-sandbox exec --prompt-file /tmp/tasks/explore.txt \
  --output-dir /tmp/runs/explore-001 --timeout 900

pi-sandbox exec --prompt-file /tmp/tasks/implement.txt \
  --output-dir /tmp/runs/implement-001 --timeout 3600 \
  --network myapp_default --env-file .pi-session.env

pi-sandbox rpc --output-dir /tmp/runs/review-001 --timeout 1800
```

Use subprocess argument arrays, a separate cwd per worker, and piped stdout and
stderr. For one-shot jobs, collect the JSONL stream and final result artifact.
For live sessions, write protocol messages to stdin and consume responses/events
until intentionally closing or cancelling the connection.

The orchestrator can run exploration workers concurrently, choose an approach,
assign implementation in another worktree, and then create a review worktree
at an explicit implementation commit. Uncommitted changes are not automatically
visible in a separate review worktree; transfer a patch or commit deliberately.
Keep task dependencies and acceptance checks outside the launcher.

## Implementation order

1. Pin/test the pi release and record executable protocol fixtures, including
   error outcomes and piped prompt behavior.
2. Introduce a shared typed launch context and mode-specific process runners;
   preserve interactive `run`. Coordinate with the networking plan rather than
   duplicating configuration resolution.
3. Implement `exec`, prompt input, clean stream routing, artifact schema, explicit
   statuses, isolated session storage, and per-worktree locks.
4. Implement owned container lifecycle, timeout/signals, orphan cleanup through
   `stop`, atomic cache preparation, and concurrent cold-start handling.
5. Implement RPC passthrough and its EOF/termination contract using the same
   supervisor. Add a small example orchestrator client demonstrating concurrent
   workers and follow-up requests.
6. Document limits and test the complete Mac workflow with networked resources
   and linked worktrees before calling orchestration support complete.

Deliverable one-shot jobs can be reviewed first, but persistent RPC is part of
this plan and should not be described as completed until exercised end to end.

## Verification

Use fake Docker/pi executables for deterministic tests of argument construction,
protocol parsing, exit codes, EOF, timeout, cancellation races, output-directory
claiming, redaction of metadata, missing credentials, and malformed streams.
Test both small and large stdout/stderr concurrently and deliberate slow readers.
Verify no terminal access is needed and no diagnostics contaminate stdout.

Add opt-in Docker integration tests using a deterministic agent fixture with
the same I/O contract. Cover concurrent containers, worktree file ownership,
Git operations, persisted artifacts after removal, exact-target cancellation,
child-process cleanup, abrupt supervisor death and orphan recovery, failed
network attachment, and parallel image preparation. Real Docker tests should
not require paid model credentials by default.

Finally run a small opt-in smoke test against the pinned real pi release and a
configured provider: one-shot success/error, two concurrent worktrees, a live RPC
prompt followed by another prompt, and turn abort. Explicitly record platform,
Docker and pi versions, which tests ran, and which require external credentials.

Acceptance requires an orchestrator to start at least three independent workers,
stream useful results, cancel one without affecting the others, retain artifacts,
and resume a selected conversation explicitly. A successful worker exit alone
does not replace application tests or feature review.
