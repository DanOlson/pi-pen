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

Use `assets/orchestrator.py` as a starter for worker execution. Adapt its main
flow to the task instead of rebuilding process supervision from scratch. The
controller may use `exec` for each stage and `--resume-session` for implementer
revisions. Use RPC only when live steering materially improves the workflow.

## Failure handling

Reconcile the filesystem, Git branch, recorded commit, result, and cleanup state
before deciding what to do after a failure. A safe retry has a new output
directory and a prompt that explains the observed state. Bound retries and
escalate recurring failures rather than looping indefinitely.
