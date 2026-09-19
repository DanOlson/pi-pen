#!/usr/bin/env python3
"""Concurrent workers in distinct existing worktrees; see docs/programmatic-usage.md."""
import argparse
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=pathlib.Path, required=True)
    parser.add_argument("--output-root", type=pathlib.Path, required=True)
    parser.add_argument("--binary", default="pi-sandbox")
    parser.add_argument("--manifest", type=pathlib.Path)
    parser.add_argument("--env-file", type=pathlib.Path)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("pi_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    binary = shutil.which(args.binary)
    if not binary:
        parser.error(f"executable not found: {args.binary}")
    binary = str(pathlib.Path(binary).resolve())
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    jobs_file = args.jobs.resolve()
    jobs = json.loads(jobs_file.read_text(encoding="utf-8"))
    if not isinstance(jobs, list) or not jobs:
        parser.error("jobs must be a nonempty JSON array")
    names, roots = set(), set()
    for job in jobs:
        name = job["name"]
        if not re.fullmatch(r"[A-Za-z0-9_-]+", name) or name in names:
            parser.error("job names must be unique and contain only letters, digits, _ or -")
        worktree = (jobs_file.parent / job["worktree"]).resolve()
        root = pathlib.Path(subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], cwd=worktree, text=True
        ).strip()).resolve()
        if root != worktree or root in roots:
            parser.error("each job must name a distinct Git worktree root")
        if not isinstance(job["prompt"], str) or not job["prompt"].strip():
            parser.error("each job needs a nonempty prompt string")
        job["worktree"] = worktree
        names.add(name)
        roots.add(root)
    output = args.output_root.resolve()
    if any(output == root or root in output.parents for root in roots):
        parser.error("output root must be outside worker worktrees")
    # The launcher also validates home and Git metadata mount boundaries.
    os.umask(0o077)
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    common = []
    if args.env_file:
        common += ["--env-file", str(args.env_file.resolve())]
    passthrough = args.pi_args
    if passthrough[:1] == ["--"]:
        passthrough = passthrough[1:]
    interrupted = []
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda number, frame: interrupted.append(number))
    workers = []
    try:
        for job in jobs:
            if interrupted:
                break
            name = job["name"]
            prompt = output / f"{name}.prompt.txt"
            prompt.write_text(job["prompt"], encoding="utf-8")
            run_dir = output / name
            command = [binary, "exec"]
            if args.manifest:
                command.append(str(args.manifest.resolve()))
            command += ["--prompt-file", str(prompt), "--output-dir", str(run_dir),
                        "--timeout", str(args.timeout), *common]
            if passthrough:
                command += ["--", *passthrough]
            # Regular files avoid bounded pipes: all workers write concurrently.
            with (output / f"{name}.stdout.jsonl").open("xb") as stdout, \
                    (output / f"{name}.launcher.log").open("xb") as stderr:
                process = subprocess.Popen(command, cwd=job["worktree"],
                                           stdin=subprocess.DEVNULL,
                                           stdout=stdout, stderr=stderr)
            workers.append((name, run_dir, process))
        while not interrupted and any(p.poll() is None for _, _, p in workers):
            time.sleep(0.05)
    finally:
        active = [(name, path, p) for name, path, p in workers if p.poll() is None]
        for _, _, process in active:
            try:
                process.terminate()
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + 20
        for name, run_dir, process in active:
            try:
                process.wait(timeout=max(0.01, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                try:
                    subprocess.run([binary, "stop", "--output-dir", str(run_dir)],
                                   check=True, timeout=20, stdout=sys.stderr)
                except (subprocess.SubprocessError, OSError) as error:
                    print(f"Cleanup uncertain for {name}: {error}; retain {run_dir}",
                          file=sys.stderr)
                process.kill()
                process.wait()
    failed = len(workers) != len(jobs)
    for name, run_dir, process in workers:
        try:
            result = json.loads((run_dir / "result.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            result = {}
        failed |= process.returncode != 0 or result.get("status") != "completed"
        print(json.dumps({"name": name, "exit_code": process.returncode,
                          "status": result.get("status", "missing_result"),
                          "output_dir": str(run_dir)}))
    return 128 + interrupted[0] if interrupted else int(failed)


if __name__ == "__main__":
    sys.exit(main())
