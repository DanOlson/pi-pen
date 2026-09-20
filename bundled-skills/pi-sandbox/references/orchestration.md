# Orchestration

## Shape the workflow around dependencies

Parallelize only independent work. A useful feature workflow is:

1. Run focused investigations concurrently when they answer distinct questions.
2. Synthesize their evidence into one implementation assignment.
3. Give one implementation agent ownership of tests, code, validation, and
   commits in its worktree.
4. After it commits, expose that exact commit to an independent review worktree.
5. Have the reviewer inspect the diff and run useful checks without editing.
6. Send actionable findings back to the same implementation session for
   test-first corrections and another commit.
7. Independently verify the final commit and integrate it only when authorized.

A worktree created from the original base cannot review uncommitted changes in
another worktree. Linked worktrees share one object store, so once the
implementer has committed, create the reviewer's worktree with
`git worktree add --detach <review-path> <sha>`. Verify it with
`git -C <review-path> rev-parse HEAD` before launching the reviewer.

## Worktree plus shared namespace, end to end

This is the complete sequence for one implementer and one reviewer once the
namespace in `SKILL.md` is set up and authenticated. Paths are examples.

```text
# Once per batch, from the main checkout.
pi-sandbox namespace show                      # must say: pi credentials present
pi-sandbox build                               # so workers reuse one image
mkdir -p -m 0700 /private/tmp/batch-42
BASE=$(git rev-parse HEAD)

# Implementer: its own branch and worktree.
git worktree add -b agents/feature-42 /private/tmp/wt-implement "$BASE"
pi-sandbox exec --prompt-file /private/tmp/batch-42/implement.prompt \
  --new-run-under /private/tmp/batch-42 --run-name implement --timeout 1800 \
  -- --provider PROVIDER --model MODEL
# (run from /private/tmp/wt-implement; the controller sets cwd)

# Reviewer: detached at the exact commit the implementer reported.
SHA=$(git -C /private/tmp/wt-implement rev-parse HEAD)
git worktree add --detach /private/tmp/wt-review "$SHA"
git -C /private/tmp/wt-review rev-parse HEAD        # confirm == $SHA
pi-sandbox exec --prompt-file /private/tmp/batch-42/review.prompt \
  --new-run-under /private/tmp/batch-42 --run-name review --timeout 1800 \
  -- --provider PROVIDER --model MODEL --tools read,bash
# (run from /private/tmp/wt-review)

# Revision: resume the implementer in its original worktree.
pi-sandbox exec --prompt-file /private/tmp/batch-42/revise.prompt \
  --resume-session /private/tmp/batch-42/implement-01/sessions/session.jsonl \
  --new-run-under /private/tmp/batch-42 --run-name implement --timeout 1800 \
  -- --provider PROVIDER --model MODEL
```

Because the namespace is declared in the manifest, none of these commands need
`--namespace`; all of them mount the same authenticated home. If the manifest
does not declare one, pass the same `--namespace NAME` to every command above.

Build artifacts are worktree-local: `target/`, `node_modules/`, and similar
directories are not shared through the home, so each fresh worktree rebuilds
from scratch. For Rust projects, point `CARGO_TARGET_DIR` into the shared home
from the manifest so every worker reuses one compiled graph; Cargo's build lock
serializes concurrent access:

```toml
[run.env]
CARGO_TARGET_DIR = "/tmp/pi-home/.cache/cargo-target"
```

Tell workers not to run `cargo clean` or delete cache directories when a cache
is shared.

## Worktree and branch rules

- Record the base commit before starting.
- Use unique branches and distinct worktree roots for workers that may overlap
  in time.
- Give every worker that should share provider credentials and home-installed
  dependencies the same explicit namespace. That namespace also shares mutable
  package caches and install locations: authenticate and provision dependencies
  before parallel work, and serialize `bundle install`, `npm install`, or other
  package-manager operations that could write the same shared state.
- Keep one owner for an implementation area. Multiple reviews are safe;
  overlapping writers create avoidable reconciliation work.
- Treat worker commits as candidates. Do not merge merely because a run says
  `completed`.
- Preserve unrelated pre-existing changes and make the integration action
  explicit: merge, cherry-pick, request another revision, or stop.
- A worker's `git status` describes its own worktree. Ignored or untracked
  build directories a reviewer deletes inside its container do not affect the
  host checkout; verify host state from the host, not from a worker's report.

## Task-specific controller

For multiple stages, write a small Python controller in a private directory
outside all worker worktrees and place run artifacts there. It should:

- set a restrictive umask;
- invoke subprocesses with argument arrays and explicit working directories;
- write prompts and both streams to private regular files;
- propagate SIGINT and SIGTERM to active launchers;
- attempt targeted `pi-sandbox stop` if bounded shutdown fails;
- parse each `result.json` and retain every artifact;
- make dependencies explicit instead of launching review too early;
- stop before destructive integration or external side effects that were not
  authorized.

Copy `assets/orchestrator.py` beside the controller and import it from there.
Importing it in place from the installed skill directory is also fine, but set
`sys.dont_write_bytecode = True` (or `PYTHONDONTWRITEBYTECODE=1`) before the
import so no `__pycache__` lands in the checked-out skill. Adapt its main flow
to the task instead of rebuilding process supervision from scratch. The
controller may use `exec` for each stage and `--resume-session` for implementer
revisions. Use RPC only when live steering materially improves the workflow.

## Monitoring a running worker

`events.jsonl` is a durable protocol log, not a progress view: it is dominated
by token deltas and partial tool output. To see where a worker is, read
`run.json` for `state` and `container_id`, then filter events by type:

```text
jq -c 'select(.type=="pi_event") | .event | select(.type|test("^(tool_execution_(start|end)|turn_|agent_)")) | {type, tool: .toolName, args: .args.command}' OUTPUT/events.jsonl | tail -n 5
```

`response.txt` and `result.json` appear only at the end; their absence means
the worker is still running or the launcher died. If `stderr.log` is quiet and
`run.json` has not been updated for longer than a tool call should take, use
`docker inspect <container_id>` before deciding the worker is stuck.

## Failure handling

Reconcile the filesystem, Git branch, recorded commit, result, and cleanup state
before deciding what to do after a failure. A safe retry has a new output
directory and a prompt that explains the observed state; `--new-run-under
BATCH --run-name NAME` allocates `NAME-02`, `NAME-03`, ... without editing
controller paths, and prints the chosen path as the first stderr line. Bound
retries and escalate recurring failures rather than looping indefinitely.

A startup failure that mentions credentials or a missing API key is a namespace
problem, not a retry candidate: check the `persistent home:` line the launcher
printed, compare it with `pi-sandbox namespace show`, and fix the namespace
before launching again.
