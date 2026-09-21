#!/usr/bin/env python3
"""Reusable pi-sandbox worker runner for a task-specific orchestration script.

Copy this file next to your controller and import it from there. If you import
it from the installed skill directory instead, set ``sys.dont_write_bytecode``
before the import so no ``__pycache__`` is written into the checkout.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import sys
import time
from typing import Callable, Iterable, Mapping, Sequence


@dataclass(frozen=True)
class Worker:
    name: str
    worktree: Path
    prompt: str
    resume_session: Path | None = None


@dataclass(frozen=True)
class Outcome:
    name: str
    output_dir: Path
    exit_code: int
    status: str
    response: str
    result: dict
    #: Set by ``run_with_recovery`` on the final outcome when it stopped short:
    #: why a human is needed now.
    escalation: str | None = None

    @property
    def completed(self) -> bool:
        return self.exit_code == 0 and self.status == "completed"

    @property
    def error(self) -> dict | None:
        """``result.json.error``: class, message, recoverable; None when absent."""
        error = self.result.get("error")
        return error if isinstance(error, dict) else None

    @property
    def recovery(self) -> str:
        """The launcher's hint for the next move: ``resume``,
        ``resume_adjusted``, ``restart``, ``fix_environment`` or ``escalate``.
        A run without a classified error (a launcher failure, a missing
        result) is ``escalate``."""
        error = self.error
        return str(error.get("recovery", "escalate")) if error else "escalate"

    @property
    def recoverable(self) -> bool:
        """True when resuming the recorded session as it is is the expected
        next step: a transient provider, transport, or stream failure with
        exactly one session file. It says nothing about the worktree: resume
        restores the conversation only.
        """
        return self.recovery == "resume" and len(
            self.result.get("session_files") or []
        ) == 1

    def describe_failure(self) -> str:
        error = self.error
        if error is None:
            text = f"exit={self.exit_code} status={self.status}"
        else:
            text = (
                f"exit={self.exit_code} status={self.status} "
                f"{error.get('class')}: {error.get('message')}"
            )
        if self.escalation:
            text += f"; needs a human: {self.escalation}"
        return text


_active: set[subprocess.Popen] = set()
_interrupted: int | None = None


def _interrupt(signum, _frame):
    global _interrupted
    _interrupted = signum
    for process in tuple(_active):
        try:
            process.terminate()
        except ProcessLookupError:
            pass


def install_signal_handlers() -> None:
    signal.signal(signal.SIGINT, _interrupt)
    signal.signal(signal.SIGTERM, _interrupt)


def _wait_for_worker(process: subprocess.Popen, binary: str, run_dir: Path) -> int:
    while True:
        try:
            return process.wait(timeout=0.2)
        except subprocess.TimeoutExpired:
            if _interrupted is None:
                continue
        try:
            return process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            try:
                subprocess.run(
                    [binary, "stop", "--output-dir", str(run_dir)],
                    check=True,
                    timeout=20,
                    stdout=sys.stderr,
                )
            except (OSError, subprocess.SubprocessError) as error:
                print(
                    f"Cleanup uncertain for {run_dir}: {error}; retain its artifacts",
                    file=sys.stderr,
                )
            process.kill()
            return process.wait()


def _allocate_run_dir(output_root: Path, name: str) -> Path:
    """Reserve ``<output_root>/<name>-NN`` for the lowest free NN.

    Creating the directory is the reservation, so a retry of the same worker
    gets ``name-02`` without the controller editing any path. The launcher
    accepts an existing empty directory as a fresh output directory.
    """
    for index in range(1, 10000):
        candidate = output_root / f"{name}-{index:02d}"
        try:
            candidate.mkdir(mode=0o700)
        except FileExistsError:
            continue
        return candidate
    raise RuntimeError(f"no free run directory for {name} under {output_root}")


def run_worker(
    worker: Worker,
    *,
    output_root: Path,
    binary: str = "pi-sandbox",
    manifest: Path | None = None,
    env_files: Sequence[Path] = (),
    runtime_env: Mapping[str, str] | None = None,
    namespace: str | None = None,
    timeout: int = 1800,
    pi_args: Sequence[str] = (),
) -> Outcome:
    """Run one worker; each call allocates ``<output_root>/<name>-NN`` so retries never collide."""
    if not re.fullmatch(r"[A-Za-z0-9_-]+", worker.name):
        raise ValueError("worker names may contain only letters, digits, _ and -")
    if not worker.prompt.strip():
        raise ValueError("worker prompt must not be empty")
    if timeout <= 0:
        raise ValueError("timeout must be positive")
    worktree = worker.worktree.resolve(strict=True)
    git_root = Path(subprocess.check_output(
        ["git", "rev-parse", "--show-toplevel"], cwd=worktree, text=True
    ).strip()).resolve(strict=True)
    if git_root != worktree:
        raise ValueError(f"worker path is not a Git worktree root: {worktree}")
    output_root = output_root.resolve()
    if output_root == worktree or worktree in output_root.parents:
        raise ValueError("output root must be outside the worker worktree")
    output_root.mkdir(mode=0o700, parents=True, exist_ok=True)
    output_root.chmod(0o700)
    resolved_binary = shutil.which(binary)
    if resolved_binary is None:
        raise FileNotFoundError(f"pi-sandbox executable not found: {binary}")
    binary = str(Path(resolved_binary).resolve(strict=True))
    run_dir = _allocate_run_dir(output_root, worker.name)
    attempt = run_dir.name
    prompt_path = output_root / f"{attempt}.prompt.txt"
    stdout_path = output_root / f"{attempt}.stdout.jsonl"
    launcher_log = output_root / f"{attempt}.launcher.log"
    for path in (prompt_path, stdout_path, launcher_log):
        if path.exists():
            raise FileExistsError(f"refusing to reuse worker artifact: {path}")
    prompt_path.write_text(worker.prompt, encoding="utf-8")
    prompt_path.chmod(0o600)

    command = [binary, "exec"]
    if manifest is not None:
        command.append(str(manifest.resolve(strict=True)))
    if namespace is not None:
        command.extend(["--namespace", namespace])
    command.extend([
        "--prompt-file", str(prompt_path),
        "--output-dir", str(run_dir),
        "--timeout", str(timeout),
    ])
    for env_file in env_files:
        command.extend(["--env-file", str(env_file.resolve(strict=True))])
    for key, value in (runtime_env or {}).items():
        command.extend(["--env", f"{key}={value}"])
    if worker.resume_session is not None:
        command.extend([
            "--resume-session", str(worker.resume_session.resolve(strict=True))
        ])
    if pi_args:
        command.extend(["--", *pi_args])

    with stdout_path.open("xb") as stdout, launcher_log.open("xb") as stderr:
        stdout_path.chmod(0o600)
        launcher_log.chmod(0o600)
        process = subprocess.Popen(
            command,
            cwd=worktree,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
        )
        _active.add(process)
        try:
            exit_code = _wait_for_worker(process, binary, run_dir)
        finally:
            _active.discard(process)

    try:
        result = json.loads((run_dir / "result.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        result = {}
    try:
        response = (run_dir / "response.txt").read_text(encoding="utf-8")
    except OSError:
        response = ""
    return Outcome(
        name=worker.name,
        output_dir=run_dir,
        exit_code=exit_code,
        status=result.get("status", "missing_result"),
        response=response,
        result=result,
    )


def run_parallel(
    workers: Iterable[Worker],
    **kwargs,
) -> list[Outcome]:
    """Run independent workers concurrently; preserve the supplied order."""
    items = list(workers)
    names = [worker.name for worker in items]
    if len(names) != len(set(names)):
        raise ValueError("worker names must be unique")
    roots = [worker.worktree.resolve(strict=True) for worker in items]
    if len(roots) != len(set(roots)):
        raise ValueError("parallel workers require distinct worktrees")
    with ThreadPoolExecutor(max_workers=len(items) or 1) as pool:
        futures = [pool.submit(run_worker, worker, **kwargs) for worker in items]
        return [future.result() for future in futures]


def session_file(outcome: Outcome) -> Path:
    """Select the sole recorded session or require task-specific disambiguation."""
    sessions = outcome.result.get("session_files") or []
    if len(sessions) != 1:
        raise RuntimeError(
            f"expected one session for {outcome.name}, found {len(sessions)}"
        )
    return Path(sessions[0]).resolve(strict=True)


RECOVERY_PROMPT = """\
Your previous run stopped before you finished, for a reason outside your control:
{error_class}: {error_message}
{advice}
This is the same conversation, resumed. The worktree is in whatever state you left
it, including any uncommitted edits. Before doing anything else, run `git status`
and `git log -3 --oneline` and compare them with your own last steps; then continue
the original assignment to completion and report as originally instructed.
"""

ADJUSTED_ADVICE = {
    "timeout": (
        "The run hit its time limit; this attempt has a longer one. Prefer finishing "
        "and committing what is already verified over starting new work, and avoid "
        "long-running commands that are not needed for the assignment."
    ),
    "worker_exit": (
        "The worker process itself exited; if a command you ran could have caused "
        "that (out of memory, killing the shell), avoid repeating it."
    ),
}

HANDOFF_PROMPT = """\
A previous session working on this assignment ran out of context and could not be
resumed ({error_message}). You are a fresh session taking over in the same worktree.

## Original assignment

{assignment}

## State the previous session left behind

`git status --porcelain`:
```
{git_status}
```

`git log -5 --oneline`:
```
{git_log}
```

`git diff --stat` (uncommitted):
```
{git_diff_stat}
```

Last progress report from the previous session:
```
{last_text}
```

Verify this state yourself before trusting it, then continue the assignment to
completion and report as originally instructed. Keep your own context small: read
only what you need and avoid dumping large files or logs.
"""


def build_handoff_prompt(worker: Worker, outcome: Outcome, template: str = HANDOFF_PROMPT) -> str:
    """Prompt for a fresh session after ``context_overflow``: the original
    assignment plus the worktree state and the failed session's last words."""
    def git(*args: str) -> str:
        try:
            text = subprocess.run(
                ["git", *args], cwd=worker.worktree, check=False,
                capture_output=True, text=True, timeout=30,
            ).stdout.strip()
        except (OSError, subprocess.SubprocessError) as error:
            text = f"(unavailable: {error})"
        return text[:4000] or "(clean)"
    error = outcome.error or {}
    return template.format(
        error_message=error.get("message", "context overflow"),
        assignment=worker.prompt.strip(),
        git_status=git("status", "--porcelain"),
        git_log=git("log", "-5", "--oneline"),
        git_diff_stat=git("diff", "--stat"),
        last_text=last_assistant_text(outcome) or "(none)",
    )


def last_assistant_text(outcome: Outcome, limit: int = 3000) -> str:
    """Final assistant prose in the run's session file, for a handoff."""
    sessions = outcome.result.get("session_files") or []
    if not sessions:
        return ""
    text = ""
    try:
        with open(sessions[-1], encoding="utf-8") as handle:
            for line in handle:
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                message = entry.get("message") or {}
                if message.get("role") != "assistant":
                    continue
                parts = [
                    part.get("text", "") for part in message.get("content") or []
                    if isinstance(part, dict) and part.get("type") == "text"
                ]
                if any(parts):
                    text = "\n".join(parts).strip()
    except OSError:
        return ""
    return text[-limit:]


def credentials_present(binary: str, worktree: Path, namespace: str | None,
                        manifest: Path | None) -> bool | None:
    """Ask the launcher whether the worker's home has Pi credentials; None if unknown."""
    command = [binary, "namespace", "show", "--json"]
    if manifest is not None:
        command.append(str(manifest))
    if namespace is not None:
        command.extend(["--namespace", namespace])
    try:
        report = subprocess.run(command, cwd=worktree, check=True, capture_output=True,
                                text=True, timeout=30)
        return bool(json.loads(report.stdout).get("pi_credentials"))
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def run_with_recovery(
    worker: Worker,
    *,
    max_attempts: int = 4,
    backoff_seconds: Sequence[float] = (30, 120, 300),
    fallback_pi_args: Sequence[str] | None = None,
    timeout_growth: float = 2.0,
    recovery_prompt: str = RECOVERY_PROMPT,
    handoff_prompt: Callable[[Worker, Outcome], str] = build_handoff_prompt,
    log: Callable[[str], None] | None = None,
    **kwargs,
) -> list[Outcome]:
    """Run a worker and follow the launcher's recovery hint until it completes
    or a human is needed.

    Returns every attempt's outcome, oldest first; the last one is the final
    state and, when the loop stopped short, carries ``escalation`` saying why.
    Each attempt gets its own ``<name>-NN`` directory. The ladder, keyed on
    ``result.json.error.recovery``:

    - ``resume`` (capacity, rate limit, transport, interrupted stream): wait
      the next ``backoff_seconds`` entry (the last repeats), then resume the
      session with ``recovery_prompt``. After a ``provider_capacity`` failure
      the next attempt swaps ``pi_args`` and ``fallback_pi_args``, so the
      worker alternates between the primary and fallback model for as long as
      capacity errors continue: a fallback that is itself at capacity hands
      back to the primary. Off by default because a model swap mid-task is a
      judgment call; see the orchestration reference for choosing the pair.
    - ``resume_adjusted`` (timeout, worker exit, aborted, unknown): resume
      once per class with a changed launch: a timeout multiplied by
      ``timeout_growth``, and class-specific advice in the prompt. A second
      failure of the same class escalates.
    - ``restart`` (context overflow): start one fresh session from
      ``handoff_prompt(worker, outcome)``, which by default quotes the
      assignment, the worktree's git state, and the old session's last report.
    - ``fix_environment`` (auth): check ``namespace show``; if credentials are
      present the failure may have been a token refresh, so resume once.
      Otherwise escalate: only a human can log in.
    - ``escalate``: stop.

    ``max_attempts`` bounds the total number of runs. Every decision is one
    ``log`` line so the orchestrator narrating progress can see why a worker
    restarted.
    """
    if max_attempts < 1:
        raise ValueError("max_attempts must be at least 1")
    if not backoff_seconds:
        raise ValueError("backoff_seconds must not be empty")
    if timeout_growth < 1:
        raise ValueError("timeout_growth must be at least 1")
    log = log or (lambda text: print(text, file=sys.stderr, flush=True))
    pi_args = list(kwargs.pop("pi_args", ()))
    alternate_pi_args = list(fallback_pi_args) if fallback_pi_args is not None else None
    timeout = int(kwargs.pop("timeout", 1800))
    attempts: list[Outcome] = []
    adjusted_classes: set[str] = set()
    restarted = False
    environment_checked = False
    current = worker

    def stop(outcome: Outcome, why: str) -> None:
        log(f"{worker.name}: {why} ({outcome.describe_failure()}); inspect {outcome.output_dir}")
        attempts[-1] = replace(outcome, escalation=why)

    for attempt in range(1, max_attempts + 1):
        outcome = run_worker(current, pi_args=pi_args, timeout=timeout, **kwargs)
        attempts.append(outcome)
        if outcome.completed or _interrupted is not None:
            break
        error = outcome.error or {}
        error_class = error.get("class", "unknown")
        recovery = outcome.recovery
        sessions = outcome.result.get("session_files") or []
        if attempt == max_attempts:
            stop(outcome, f"giving up after {attempt} attempts")
            break
        advice = ""
        resume = True
        if recovery == "resume":
            if alternate_pi_args is not None and error_class == "provider_capacity":
                pi_args, alternate_pi_args = alternate_pi_args, pi_args
                log(f"{worker.name}: swapping to {_describe_model(pi_args)} for the next attempt")
        elif recovery == "resume_adjusted":
            if error_class in adjusted_classes:
                stop(outcome, f"{error_class} recurred after an adjusted attempt")
                break
            adjusted_classes.add(error_class)
            advice = ADJUSTED_ADVICE.get(error_class, "")
            if error_class == "timeout":
                timeout = int(timeout * timeout_growth)
                log(f"{worker.name}: raising timeout to {timeout}s for the next attempt")
        elif recovery == "restart":
            if restarted:
                stop(outcome, "a fresh session also overflowed its context; split the assignment")
                break
            restarted = True
            resume = False
        elif recovery == "fix_environment":
            if environment_checked:
                stop(outcome, "credentials still failing; a human must re-authenticate")
                break
            environment_checked = True
            present = credentials_present(
                kwargs.get("binary", "pi-sandbox"), worker.worktree,
                kwargs.get("namespace"), kwargs.get("manifest"))
            if not present:
                stop(outcome, "the worker's home has no pi credentials; a human must log in")
                break
            log(f"{worker.name}: credentials are present; treating {error_class} as transient once")
        else:
            stop(outcome, "no autonomous recovery for this failure")
            break
        if resume and len(sessions) != 1:
            stop(outcome, f"cannot resume: {len(sessions)} session files recorded")
            break
        delay = backoff_seconds[min(attempt - 1, len(backoff_seconds) - 1)]
        log(f"{worker.name}: attempt {attempt}/{max_attempts} stopped "
            f"({outcome.describe_failure()}); {recovery} in {delay:g}s")
        if not _interruptible_sleep(delay):
            break
        if resume:
            current = Worker(
                worker.name,
                worker.worktree,
                recovery_prompt.format(
                    error_class=error_class,
                    error_message=error.get("message", ""),
                    advice=f"\n{advice}\n" if advice else "",
                ),
                resume_session=session_file(outcome),
            )
        else:
            current = Worker(worker.name, worker.worktree, handoff_prompt(worker, outcome))
    return attempts


def _describe_model(pi_args: Sequence[str]) -> str:
    """``provider/model`` from pi args, for log lines."""
    args = list(pi_args)
    def value(flag: str) -> str | None:
        return args[args.index(flag) + 1] if flag in args[:-1] else None
    parts = [part for part in (value("--provider"), value("--model")) if part]
    return "/".join(parts) if parts else "the same pi args"


def _interruptible_sleep(seconds: float) -> bool:
    """Sleep in short steps; return False if an interrupt arrived."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if _interrupted is not None:
            return False
        time.sleep(min(0.5, max(0.0, deadline - time.monotonic())))
    return _interrupted is None


def require_completed(outcome: Outcome) -> None:
    if not outcome.completed:
        raise RuntimeError(
            f"{outcome.name} failed: {outcome.describe_failure()}; "
            f"inspect {outcome.output_dir}"
        )


def main() -> int:
    print(
        "Import this module from a task-specific controller; "
        "see the pi-sandbox skill orchestration reference.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    os.umask(0o077)
    install_signal_handlers()
    raise SystemExit(main())
