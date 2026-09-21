# Execution contract

## Preflight

- Run workers from the intended Git worktree root.
- Build the selected environment before a batch when workers share it.
- Supply credentials and other runtime variables explicitly as described below.
  Host environment variables are not forwarded automatically.
- For persisted Pi/OAuth credentials or home-installed project dependencies
  shared by linked worktrees, use one explicit namespace: declare it as
  `[run] namespace` in the manifest (preferred, inherited by every launch) or
  pass the same `--namespace NAME` to the initial interactive setup and every
  worker. The default `worktree` namespace intentionally gives each worktree a
  separate persistent home. Confirm with `pi-sandbox namespace show` before a
  batch; it reports the resolved home path and whether it holds credentials.
- Give every invocation a fresh output directory outside the workspace,
  persistent home, and Git metadata mounts: either an exact `--output-dir`, or
  `--new-run-under BATCH --run-name NAME` to allocate `BATCH/NAME-NN`.
- Use an explicit timeout for unattended work.

## One-shot worker

Write the assignment to a private UTF-8 file, then invoke the launcher with an
argument array rather than a shell command:

```text
pi-sandbox exec [ENV] --prompt-file PROMPT --output-dir OUTPUT --timeout SECONDS -- [PI OPTIONS]
```

For example, the pinned Pi release accepts this concrete passthrough:

```text
pi-sandbox exec --prompt-file PROMPT --output-dir OUTPUT --timeout 1800 -- --provider openai --model gpt-4o-mini --thinking high --tools read,bash,edit,write
```

Provider, model, thinking, and tool options belong after `--`. Never place
credentials in arguments or prompts.

## Runtime environment

Launcher options must appear before the `--` that introduces Pi options. Pass a
small non-secret value with a repeatable `--env KEY=VALUE` argument:

```text
pi-sandbox exec --env APP_ENV=test --env WORKER_ROLE=implement ... -- --provider openai
```

For secrets or several values, use one or more private env files:

```text
pi-sandbox exec --env-file /absolute/private/provider.env ...
```

Env files contain literal `KEY=VALUE` lines. They do not support `export`, shell
expansion, or surrounding quotation marks. Restrict their permissions, keep
them out of version control, and prefer them for secrets because command-line
values may be visible in process listings. An authenticated persistent
namespace home is another option for provider credentials.

Values from later sources replace earlier ones; explicit `--env` values take
precedence over CLI env files and manifest defaults. Do not put secrets in
manifest `[env]` or `[runtime_env]` values because those become image defaults.

The bundled Python starter exposes these as `env_files` and `runtime_env`:

```python
outcome = run_worker(
    worker,
    output_root=output_root,
    env_files=[Path("/absolute/private/provider.env")],
    runtime_env={"APP_ENV": "test", "WORKER_ROLE": worker.name},
    namespace="feature_orchestrator",
    pi_args=["--provider", "openai", "--model", "MODEL"],
)
```

Construct subprocess arguments as an array. Do not interpolate environment
values into a shell command.

## Shared persistent home

A named namespace is also the boundary for the persistent `/tmp/pi-home` bind
mount. Authenticate and provision the namespace once, for example:

```text
pi-sandbox run --namespace feature_orchestrator
```

Then pass `--namespace feature_orchestrator` to every `exec` or `rpc` worker,
or declare it once as `[run] namespace = "feature_orchestrator"` in the
manifest so it is inherited. Linked worktrees in the same repository and environment will share Pi's
persisted provider credentials, Ruby gems installed under the home, npm and
other package caches, and tool configuration. Unrelated repositories do not
share a home merely because they use the same namespace name. Headless session
files remain isolated in each run's output directory.

The Python starter accepts the same value as `namespace="feature_orchestrator"`.

Drain stdout and stderr concurrently. A worker can fail if stdout remains
blocked, and unread stderr can deadlock a subprocess. Regular files are a simple
safe destination for unattended runs and may contain sensitive material.

Each headless launch prints `persistent home: <path> (namespace <name>, ...)`
to stderr before starting Docker, and `run.json` records `namespace`,
`worktree_id`, and `persistent_home`. A warning on that line means the home
has no saved credentials and no `*_API_KEY` launch variable was supplied.

## Completion

Use both the launcher exit status and `OUTPUT/result.json`:

- Exit zero plus `status: completed` means the agent process completed.
- It does not mean the requested code is correct, committed, or sufficiently
  tested. Inspect the response, branch, diff, commit, and test evidence.
- A missing result is a failure even if some output exists.
- Check `cleanup_succeeded`; uncertain cleanup requires reconciliation.
- A failed run's `error` field (`class`, `message`, `recoverable`, and the
  provider's own diagnostics) says why the model or transport stopped. It is
  also on the `run_finished` stdout envelope and in `pi-sandbox status`. The
  orchestration reference maps each class to a recovery.
- Do not blindly retry. The prior worker may already have changed files,
  commits, databases, or external systems. A `recoverable` error means
  resuming the session is the right next step after checking the worktree,
  not that nothing happened.

Keep each output directory for diagnosis and possible continuation. Select an
actual path from `result.json.session_files` when using `--resume-session`; a
new invocation still needs a fresh output directory. Resuming restores the
conversation, not filesystem or database state, so use the same intended
worktree and inspect its current state first.

## Concurrency and cancellation

Only one headless worker can own a canonical worktree at a time. Use distinct
linked worktrees for concurrent agents; subdirectories and symlinks do not make
independent workspaces.

Cancel a specific worker with:

```text
pi-sandbox stop --output-dir OUTPUT --grace 10
```

Never perform broad Docker cleanup. Preserve the output directory if shutdown
or cleanup is uncertain.
