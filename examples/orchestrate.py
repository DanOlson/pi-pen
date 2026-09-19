#!/usr/bin/env python3
"""Small stdlib-only example: concurrent exec workers and one RPC query."""
import json
import pathlib
import subprocess
import tempfile

root = pathlib.Path(tempfile.mkdtemp(prefix="pi-orchestrator-"))
tasks = ["Inspect the tests", "Inspect the architecture", "Suggest one improvement"]
workers = []
for index, task in enumerate(tasks):
    prompt = root / f"task-{index}.txt"
    prompt.write_text(task, encoding="utf-8")
    workers.append(subprocess.Popen([
        "pi-sandbox", "exec", "--prompt-file", str(prompt),
        "--output-dir", str(root / f"run-{index}"), "--timeout", "300",
    ], stdout=subprocess.PIPE, text=True))

for worker in workers:
    for line in worker.stdout:
        event = json.loads(line)
        print(event["run_id"], event["type"])
    worker.wait()

rpc = subprocess.Popen([
    "pi-sandbox", "rpc", "--output-dir", str(root / "rpc"), "--timeout", "60",
], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
reply, _ = rpc.communicate(json.dumps({"id": "state", "type": "get_state"}) + "\n")
print(reply, end="")
