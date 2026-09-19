#!/usr/bin/env python3
"""Deterministic real-pi protocol check with no paid credentials.

Run on Docker Desktop after building/pulling an image containing pinned pi 0.85.1:

    PI_REAL_IMAGE=pi-sandbox-env:50ff6cf9183b3133 python3 tests/real_pi_compat.py

The fixture binds an ephemeral host port, uses temporary model/session paths,
and exercises print JSON, exact-session resume, and two sequential RPC prompts.
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class MockModelHandler(BaseHTTPRequestHandler):
    counter = 0

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        type(self).counter += 1
        number = type(self).counter
        users = [message.get("content") for message in request.get("messages", []) if message.get("role") == "user"]
        text = f"mock-reply-{number}:{users[-1] if users else 'no-user'}"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        chunks = [
            {
                "id": f"fixture-{number}",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": "fixture-model",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": None}],
            },
            {
                "id": f"fixture-{number}",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": "fixture-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5},
            },
        ]
        for chunk in chunks:
            self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def parse_jsonl(output: str) -> list[dict]:
    return [json.loads(line) for line in output.splitlines() if line]


def assert_successful_turn(events: list[dict], prompt: str) -> None:
    assistant = [
        event["message"]
        for event in events
        if event.get("type") == "message_end" and event.get("message", {}).get("role") == "assistant"
    ]
    assert assistant and assistant[-1].get("stopReason") == "stop"
    assert prompt in json.dumps(assistant[-1].get("content"))
    ends = [event for event in events if event.get("type") == "agent_end"]
    assert ends and ends[-1].get("willRetry") is False


def main() -> None:
    image = os.environ.get("PI_REAL_IMAGE")
    if not image:
        raise SystemExit("set PI_REAL_IMAGE to an image containing pi 0.85.1")
    version = subprocess.run(
        ["docker", "run", "--rm", image, "pi", "--version"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    assert version == "0.85.1", version

    server = ThreadingHTTPServer(("0.0.0.0", 0), MockModelHandler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="pi-real-compat-", dir="/private/tmp") as temp:
            root = pathlib.Path(temp)
            home = root / "home"
            sessions = root / "sessions"
            rpc_sessions = root / "rpc-sessions"
            (home / ".pi" / "agent").mkdir(parents=True)
            sessions.mkdir()
            rpc_sessions.mkdir()
            models = {
                "providers": {
                    "fixture": {
                        "baseUrl": f"http://host.docker.internal:{server.server_port}/v1",
                        "api": "openai-completions",
                        "apiKey": "fixture-key",
                        "compat": {"supportsDeveloperRole": False, "supportsReasoningEffort": False},
                        "models": [{
                            "id": "fixture-model",
                            "name": "Deterministic fixture",
                            "reasoning": False,
                            "input": ["text"],
                            "contextWindow": 4096,
                            "maxTokens": 512,
                            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                        }],
                    }
                }
            }
            (home / ".pi" / "agent" / "models.json").write_text(json.dumps(models), encoding="utf-8")

            mounts = ["-e", "HOME=/tmp/pi-home", "-v", f"{home}:/tmp/pi-home"]
            base = ["docker", "run", "--rm", "-i", *mounts, "-v", f"{sessions}:/sessions", image]
            pi = ["pi", "--print", "--mode", "json", "--provider", "fixture", "--model", "fixture-model", "--session-dir", "/sessions"]
            first = subprocess.run([*base, *pi], input="first prompt\n", check=True, capture_output=True, text=True)
            first_events = parse_jsonl(first.stdout)
            assert_successful_turn(first_events, "first prompt")
            session_file = next(sessions.glob("*.jsonl"))
            session_id = first_events[0]["id"]

            resumed = subprocess.run(
                [*base, *pi, "--session", f"/sessions/{session_file.name}"],
                input="resume prompt\n",
                check=True,
                capture_output=True,
                text=True,
            )
            resumed_events = parse_jsonl(resumed.stdout)
            assert resumed_events[0]["id"] == session_id
            assert_successful_turn(resumed_events, "resume prompt")

            rpc_cmd = [
                "docker", "run", "--rm", "-i", *mounts,
                "-v", f"{rpc_sessions}:/sessions", image,
                "pi", "--mode", "rpc", "--provider", "fixture", "--model", "fixture-model",
                "--session-dir", "/sessions",
            ]
            rpc = subprocess.Popen(rpc_cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            assert rpc.stdin is not None and rpc.stdout is not None
            rpc_events: list[dict] = []
            for number in (1, 2):
                prompt = f"rpc prompt {number}"
                rpc.stdin.write(json.dumps({"id": f"prompt-{number}", "type": "prompt", "message": prompt}) + "\n")
                rpc.stdin.flush()
                turn: list[dict] = []
                while True:
                    line = rpc.stdout.readline()
                    assert line, "unexpected RPC EOF"
                    event = json.loads(line)
                    rpc_events.append(event)
                    turn.append(event)
                    if event.get("type") == "agent_end":
                        break
                assert_successful_turn(turn, prompt)
            rpc.stdin.close()
            assert rpc.wait(timeout=10) == 0
            response_ids = {event.get("id") for event in rpc_events if event.get("type") == "response"}
            assert {"prompt-1", "prompt-2"} <= response_ids
            print("real pi 0.85.1 print/resume/two-prompt RPC compatibility: PASS")
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)


if __name__ == "__main__":
    main()
