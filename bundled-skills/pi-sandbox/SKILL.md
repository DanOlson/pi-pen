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

## Namespace setup (do this first)

Workers only have provider credentials if the persistent home they mount holds
them. Homes are selected by namespace, and the default `worktree` namespace
gives every linked worktree its own empty home, so a batch that spans worktrees
must use one explicit namespace. Check before anything else:

```text
pi-sandbox namespace show
```

If it reports `worktree default` or `no pi credentials`:

1. Declare the namespace in the repository manifest so every `run`, `exec`, and
   `rpc` in any linked worktree inherits it without a flag. For a new manifest,
   `pi-sandbox init --namespace <repo>_agents` writes it; for an existing one,
   add this to `.pi-env.toml`. Commit it with the repository either way:

   ```toml
   [run]
   namespace = "<repo>_agents"
   ```

2. Ask the human to authenticate that namespace once, interactively:

   ```text
   pi-sandbox run
   ```

   Provider login (OAuth or API key entry) is the one step that needs a
   terminal; do not attempt it from a worker or by copying auth files by hand.
   If `pi-sandbox namespace list` shows another home for this repository that
   already holds credentials, `pi-sandbox namespace adopt --from <that-home>`
   copies them into the resolved home without an interactive run. If the human
   prefers an API key, a private env file passed with `--env-file` works
   instead.

3. Confirm with `pi-sandbox namespace show` that the home now reports
   `pi credentials present` and lists the worktrees you intend to use.

Every headless launch prints the resolved home and its credential state to
stderr, and `run.json` records `namespace`, `worktree_id`, and
`persistent_home`. When a worker fails at startup, read that line before
changing anything else.

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
worker worktrees. Copy [assets/orchestrator.py](assets/orchestrator.py) next to
the controller and import it from there; it handles the fragile subprocess,
stream, signal, and result-handling mechanics. Keep task decomposition, prompts,
review sequencing, and integration decisions in the task-specific controller
rather than forcing them into a generic scheduler.

## Harness permissions

Orchestration is only unattended if the host agent can launch workers without a
prompt for each one. Before a batch, make sure the harness allows the commands
the controller will run, and say so explicitly if it cannot:

- Claude Code: allow `Bash(pi-sandbox:*)`, `Bash(git worktree:*)`, and
  `Bash(python3:*)` for the controller, or run the batch in a session where the
  user has pre-approved them.
- Codex: the default sandbox cannot reach the Docker socket and cannot write
  outside the workspace, so every launch and every file under `/tmp` escalates.
  Run orchestration in a session whose sandbox and approval policy permit
  Docker and the controller directory, or expect one approval per worker.

Keep the number of distinct commands small: one controller script, one batch
directory, and `--new-run-under` for retries, rather than a fresh script and
path per attempt.
