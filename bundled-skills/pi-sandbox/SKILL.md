---
name: pi-sandbox
description: Orchestrate isolated coding-agent workers with pi-sandbox for repository investigation, test-first implementation, review, and revision. Use when work should run through pi-sandbox exec or rpc, especially across multiple Git worktrees.
---

# Pi Sandbox

Use `pi-sandbox` as the worker runtime and remain responsible for the overall
workflow. The launcher isolates and supervises a worker; it does not decide task
dependencies, validate code, or integrate branches.

For a single bounded assignment, use `pi-sandbox exec`. For a conversation that
needs steering or several turns in one live process, use `pi-sandbox rpc`. Prefer
`exec` unless the workflow genuinely needs RPC's additional protocol complexity.

Before launching a worker, read [references/execution-contract.md](references/execution-contract.md).
For multi-worker or multi-stage work, also read
[references/orchestration.md](references/orchestration.md). When constructing
worker assignments, read [references/prompting.md](references/prompting.md).

## Implementation ownership

Give each implementation agent end-to-end ownership of its change in one
worktree. Require it to:

1. Inspect the relevant behavior and existing test conventions.
2. Add or change a test that expresses the requested behavior.
3. Run that test and confirm it fails for the expected reason.
4. Implement the smallest coherent solution.
5. Run focused tests followed by the broader relevant checks.
6. Review its diff and commit the tested change to its worker branch.
7. Report the commit hash, checks run, and unresolved concerns.

Do not split test authorship and implementation between different agents.
Reviewers may run tests and identify missing coverage, but findings go back to
the same implementation agent, preferably by resuming its session. That agent
adds regression coverage, makes corrections, validates them, and commits the
revision.

## Orchestrator ownership

The orchestrator chooses task boundaries, orders dependent work, supplies
context, evaluates worker evidence, requests revisions, and integrates approved
commits when authorized. Independent investigations can run concurrently.
Overlapping implementations should have one clear owner unless their boundaries
and integration plan are explicit.

For nontrivial workflows, write a task-specific Python controller outside the
worker worktrees. Reuse or adapt [assets/orchestrator.py](assets/orchestrator.py)
for the fragile subprocess, stream, signal, and result-handling mechanics. Keep
task decomposition, prompts, review sequencing, and integration decisions in
the task-specific controller rather than forcing them into a generic scheduler.
