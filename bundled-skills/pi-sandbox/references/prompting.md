# Worker prompting

Give each worker enough context to act without making it rediscover decisions
the orchestrator already made. Include:

- the objective and observable acceptance criteria;
- its role and whether it may edit and commit;
- exact scope plus important exclusions;
- relevant repository facts and prior findings;
- the repository's agent-instructions file by path (`AGENTS.md`/`CLAUDE.md`),
  so house style is read rather than assumed;
- the base or implementation commit it should inspect;
- required checks and expected evidence;
- the requested response format.

Do not paste broad documentation or speculative context into every prompt. Point
the worker to repository files when it can inspect them directly.

## Implementation assignment

Require one agent to own tests and implementation together. A useful contract is:

```text
Implement this change in the current worktree using test-first development.
Inspect existing test conventions, add or update a test for the requested
behavior, and run it first to confirm it fails for the expected reason. Then
implement the smallest coherent fix. Run the focused tests and broader relevant
checks, review the diff, and commit the tested result to this worker branch.
Do not merge. Report the commit hash, tests and checks run, and any unresolved
risks or assumptions.
```

If a failing test cannot be demonstrated, require the implementer to explain
the concrete reason before proceeding; it should not silently skip the
test-first requirement.

## Review assignment

Provide the base and candidate commits. Ask for correctness findings grounded
in the diff and surrounding code, ordered by severity, with file locations and
reproduction or test evidence where possible. The reviewer may run tests but
should not edit or commit. Ask it to state explicitly when it found no issues
and to identify residual testing gaps.

Make the read-only boundary concrete. Launch reviewers with
`-- --tools read,bash` so they have no edit or write tool, and include:

```text
Do not modify, delete, clean, or rebuild anything outside normal test runs:
no `cargo clean`, no removing build or cache directories, no `git checkout`
or `git stash`. Build caches may be shared with other workers. Report
`git status --porcelain` and `git rev-parse HEAD` at the start and end; if
they differ, say what changed and why.
```

A reviewer's view of untracked or ignored files is its own worktree's, not the
host's; the orchestrator verifies host state from the host.

## Revision assignment

Resume the implementation session in its original worktree. Include the review
findings verbatim or with faithful source references. Ask the implementer to
verify each finding, add a regression test that fails for confirmed defects,
make the correction, rerun relevant checks, commit the revision, and report the
new commit. It should explain rejected findings with evidence.
