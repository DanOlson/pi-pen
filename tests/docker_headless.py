#!/usr/bin/env python3
"""Opt-in end-to-end Docker acceptance for headless orchestration.

Run from the repository root after `cargo build`:

    python3 tests/docker_headless.py

The test uses a deterministic local `pi` fixture (no credentials), three linked
Git worktrees, unique temporary outputs, and only cleans up containers recorded
by those outputs.
"""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = pathlib.Path(os.environ.get("PI_SANDBOX_BIN", ROOT / "target/debug/pi-sandbox"))


def run(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
    return subprocess.run([str(arg) for arg in args], check=True, text=True, **kwargs)


def read_json(path: pathlib.Path) -> dict[str, object]:
    return json.loads(path.read_text(encoding="utf-8"))


def wait_for(path: pathlib.Path, timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {path}")


def wait_for_container(output: pathlib.Path, timeout: float = 30) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        run_file = output / "run.json"
        if run_file.exists():
            container_id = read_json(run_file).get("container_id")
            if isinstance(container_id, str) and container_id:
                return container_id
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for container ID in {output}")


def assert_private(root: pathlib.Path) -> None:
    for path in [root, *root.rglob("*")]:
        mode = stat.S_IMODE(path.stat().st_mode)
        assert mode & 0o077 == 0, f"artifact is not private: {oct(mode)} {path}"


def main() -> None:
    if not BIN.exists():
        raise SystemExit(f"build the binary first: cargo build ({BIN} missing)")
    temp_parent = pathlib.Path("/private/tmp") if pathlib.Path("/private/tmp").is_dir() else None
    root = pathlib.Path(tempfile.mkdtemp(prefix="pi-headless-docker-", dir=temp_parent))
    outputs: list[pathlib.Path] = []
    processes: list[subprocess.Popen[str]] = []
    try:
        fixture = root / "fixture.toml"
        fixture.write_text(
            r"""name = "headless-docker-fixture"
base_image = "alpine:3.20"
default_packages = false
pi_install = "printf '%s\\n' '#!/bin/sh' '[ \"$1\" = \"--version\" ] && { echo 0.85.1; exit 0; }' 'mode=exec' 'for arg in \"$@\"; do [ \"$arg\" = rpc ] && mode=rpc; done' 'if [ \"$mode\" = rpc ]; then while IFS= read -r line; do printf \"{\\\"type\\\":\\\"response\\\",\\\"command\\\":\\\"fixture\\\",\\\"success\\\":true,\\\"echo\\\":%s}\\n\" \"$line\"; done; exit 0; fi' 'IFS= read -r prompt' 'case \"$prompt\" in *SLOW*) sleep 30;; *) sleep 1;; esac' 'printf \"{\\\"type\\\":\\\"session\\\",\\\"version\\\":3,\\\"id\\\":\\\"fixture\\\"}\\n\"' 'printf \"{\\\"type\\\":\\\"agent_start\\\"}\\n\"' 'printf \"{\\\"type\\\":\\\"message_end\\\",\\\"message\\\":{\\\"role\\\":\\\"assistant\\\",\\\"content\\\":[{\\\"type\\\":\\\"text\\\",\\\"text\\\":\\\"fixture response\\\"}],\\\"stopReason\\\":\\\"stop\\\"}}\\n\"' 'printf \"{\\\"type\\\":\\\"agent_end\\\",\\\"messages\\\":[]}\\n\"' > /usr/local/bin/pi && chmod 0755 /usr/local/bin/pi"
""",
            encoding="utf-8",
        )

        repo = root / "repo"
        repo.mkdir()
        run("git", "init", "-q", cwd=repo)
        (repo / "base").write_text("base\n", encoding="utf-8")
        run("git", "add", "base", cwd=repo)
        run("git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "base", cwd=repo)
        worktrees = [root / f"worktree-{index}" for index in range(3)]
        for index, worktree in enumerate(worktrees):
            run("git", "worktree", "add", "-q", "-b", f"headless-{index}", worktree, cwd=repo)

        env = os.environ.copy()
        env["PI_SANDBOX_CONFIG_DIR"] = str(root / "config")
        env["PI_SANDBOX_CACHE_DIR"] = str(root / "cache")
        (root / "tmp").mkdir()
        env["TMPDIR"] = str(root / "tmp")
        managed_before = run("docker", "ps", "-a", "--filter", "label=io.pi-sandbox.managed=true", "--format", "{{.ID}}", capture_output=True).stdout.splitlines()
        run(BIN, "build", fixture, env=env)

        resume_source = root / "resume-source.jsonl"
        resume_bytes = b'{"type":"session","version":3,"id":"source"}\n'
        resume_source.write_bytes(resume_bytes)
        prompts = ["normal one\n", "SLOW cancel exactly this worker\n", "normal three\n"]
        for index, (worktree, prompt) in enumerate(zip(worktrees, prompts, strict=True)):
            prompt_file = root / f"prompt-{index}.txt"
            prompt_file.write_text(prompt, encoding="utf-8")
            output = root / f"output-{index}"
            outputs.append(output)
            command = [str(BIN), "exec", str(fixture), "--prompt-file", str(prompt_file), "--output-dir", str(output), "--timeout", "60"]
            if index == 0:
                command.extend(["--resume-session", str(resume_source)])
            processes.append(subprocess.Popen(command, cwd=worktree, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE))

        container_ids = [wait_for_container(output) for output in outputs]
        slow_id = container_ids[1]
        stop = run(BIN, "stop", "--output-dir", outputs[1], "--grace", "1", env=env, capture_output=True)
        assert stop.returncode == 0

        results: list[dict[str, object]] = []
        for process, output in zip(processes, outputs, strict=True):
            stdout, stderr = process.communicate(timeout=30)
            assert "credential" not in stdout.lower()
            wait_for(output / "result.json")
            results.append(read_json(output / "result.json"))
            assert (output / "events.jsonl").is_file()
            assert (output / "stderr.log").is_file()
            assert_private(output)
            if process.returncode not in (0, 130):
                raise AssertionError(f"worker failed {process.returncode}: {stderr}")

        assert [result["status"] for result in results] == ["completed", "cancelled", "completed"]
        assert results[1]["reason"] == "explicit_stop"
        assert results[1]["container_id"] == slow_id
        assert resume_source.read_bytes() == resume_bytes
        clone = outputs[0] / "sessions" / resume_source.name
        assert clone.read_bytes() == resume_bytes

        rpc_output = root / "rpc-output"
        outputs.append(rpc_output)
        rpc = subprocess.run([str(BIN), "rpc", str(fixture), "--output-dir", str(rpc_output), "--timeout", "20"], cwd=worktrees[0], env=env, input="", text=True, capture_output=True, timeout=30)
        assert rpc.returncode == 0, rpc.stderr
        assert rpc.stdout == "", "RPC EOF must not synthesize protocol output"
        assert read_json(rpc_output / "result.json")["status"] == "completed"
        assert_private(rpc_output)

        jobs_file = root / "jobs.json"
        jobs_file.write_text(json.dumps([
            {"name": f"worker-{i}", "worktree": str(worktree), "prompt": "normal batch task\n"}
            for i, worktree in enumerate(worktrees)
        ]), encoding="utf-8")
        batch = root / "batch"
        outputs.extend(batch / f"worker-{i}" for i in range(3))
        example = run(sys.executable, ROOT / "examples/orchestrate.py",
                      "--binary", BIN, "--manifest", fixture, "--jobs", jobs_file,
                      "--output-root", batch, "--timeout", "30",
                      env=env, capture_output=True, timeout=60)
        summaries = [json.loads(line) for line in example.stdout.splitlines()]
        assert len(summaries) == 3 and all(s["status"] == "completed" for s in summaries)
        assert_private(batch)

        managed_after = run("docker", "ps", "-a", "--filter", "label=io.pi-sandbox.managed=true", "--format", "{{.ID}}", capture_output=True).stdout.splitlines()
        assert sorted(managed_after) == sorted(managed_before), f"test leaked managed containers: before={managed_before}, after={managed_after}"
        print("headless Docker lifecycle/resume/RPC acceptance: PASS")
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
        for output in outputs:
            if output.exists():
                subprocess.run([str(BIN), "stop", "--output-dir", str(output), "--grace", "1"], env=os.environ | {"PI_SANDBOX_CONFIG_DIR": str(root / "config"), "PI_SANDBOX_CACHE_DIR": str(root / "cache"), "TMPDIR": str(root / "tmp")}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
