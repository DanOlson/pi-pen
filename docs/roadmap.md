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

## `init --with rust|tauri` emitting a shared `CARGO_TARGET_DIR`

Each fresh worktree recompiles the full graph because `target/` is
worktree-local. Pointing `CARGO_TARGET_DIR` into the shared namespace home
(documented in the skill) fixes it with no launcher change. Open question is
whether the presets should emit it by default or as a commented line, given
that one worker running `cargo clean` empties the cache for everyone.

## Declarative `pi-sandbox orchestrate --jobs jobs.json`

Stages with `depends_on`, replacing a bespoke Python controller per task.
This is the largest lever on per-command approval friction in sandboxed
harnesses (one file plus one command instead of a new script per batch), but
it moves task decomposition partly into the binary, against the skill's
current "controller owns decisions" stance. Prototype as an example script
first; promote to a subcommand only if the example stops needing per-task
edits.
