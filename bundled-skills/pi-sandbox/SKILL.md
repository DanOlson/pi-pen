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

If it reports `worktree default`, or `no pi credentials` for a provider that
authenticates through pi (see [Cloud provider credentials](#cloud-provider-credentials)
for providers that do not):

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

### Cloud provider credentials

Some providers, such as `amazon-bedrock`, do not use pi's login at all: the
provider's SDK reads its own credentials. For Bedrock these are `AWS_*`
launch variables (for example `AWS_PROFILE` and `AWS_REGION` in `[run.env]`)
plus AWS config and SSO token files that the human copies into the namespace
home under `.aws/`. Copying those AWS files by hand is the intended setup; the
rule against copying auth files applies to pi's own credentials.

`namespace show` only checks for pi's own credentials, so for these providers
it reports `no pi credentials` and every launch prints the
`no saved pi credentials` warning even when the setup is correct. Do not ask
the human to log in because of either. Confirm the setup instead:

- `pi-sandbox exec --dry-run ...` lists the expected `AWS_*` keys among the
  launch environment.
- The home reported by `namespace show` contains `.aws/config` and
  `.aws/sso/cache/`.
- A short smoke-test prompt with
  `-- --provider amazon-bedrock --model MODEL` finishes with status
  `completed`.

If a Bedrock worker fails with `ExpiredTokenException` or a message saying the
security token is expired, the SSO session has lapsed. Treat it as
`fix_environment` even if the classifier did not: do not retry or resume. Ask
the human to log in again and recopy the AWS files.

Choose the fallback model pair within the same provider, as for any other
provider; the `openai-codex` pair in the orchestration reference does not
apply. Use the pair the human names, or ask for one.

## Launcher commands at a glance

| Need | Command |
| --- | --- |
| Which home, credentials, and worktrees this checkout resolves to | `pi-sandbox namespace show [--namespace N] [--json]` |
| Every persistent home on the machine, with credential state and last use | `pi-sandbox namespace list [--json]` |
| Carry credentials from an older home into the resolved one | `pi-sandbox namespace adopt --from DIR [--force]` |
| Declare the shared namespace in a new manifest | `pi-sandbox init --namespace NAME ...` |
| See the image, mounts, home path, and env keys without Docker | `pi-sandbox run --dry-run` or `pi-sandbox exec --dry-run ...` |
| Run one assignment into an exact fresh directory | `pi-sandbox exec --prompt-file P --output-dir DIR --timeout S -- PI_OPTS` |
| Run one assignment into an allocated `NAME-NN` directory (retries) | `pi-sandbox exec --prompt-file P --new-run-under BATCH --run-name NAME ...` |
| Continue a prior conversation in the same worktree | add `--resume-session <path from result.json.session_files>` |
| See where every worker in a batch is, without parsing events | `pi-sandbox status --batch DIR [--watch] [--json] [--inspect]` |
| Cancel one worker only | `pi-sandbox stop --output-dir DIR --grace 10` |
| Why a worker stopped short, and the one move to make next | `result.json.error` (`class`, `message`, `recovery`); also in `status` |

Prefer `--json` forms when a controller consumes the output. The bundled
starter's `run_worker` allocates `<output_root>/<name>-NN` itself, so calling
it twice with the same worker name is a retry, not an error, and its
`run_with_recovery` walks the escalation ladder automatically: it resumes
after a provider at capacity, resumes once with a longer timeout, restarts a
fresh session after a context overflow, and stops with a stated reason only
when a human is needed. A failed run is never a reason to stop the batch
silently: read the classified error, follow the ladder in the orchestration
reference, and report what you did and why you stopped.

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
