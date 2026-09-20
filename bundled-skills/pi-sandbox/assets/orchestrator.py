#!/usr/bin/env python3
"""Reusable pi-sandbox worker runner for a task-specific orchestration script.

Copy this file next to your controller and import it from there. If you import
it from the installed skill directory instead, set ``sys.dont_write_bytecode``
before the import so no ``__pycache__`` is written into the checkout.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import sys
from typing import Iterable, Mapping, Sequence


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

    @property
    def completed(self) -> bool:
        return self.exit_code == 0 and self.status == "completed"


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


def require_completed(outcome: Outcome) -> None:
    if not outcome.completed:
        raise RuntimeError(
            f"{outcome.name} failed: exit={outcome.exit_code} status={outcome.status}; "
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
