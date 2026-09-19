#!/usr/bin/env python3
"""Query RPC state, keeping stdin open until the matching response arrives.

Pass pi-sandbox rpc arguments after this script's name. Diagnostics inherit
stderr (never an undrained pipe). Use --timeout to bound launcher lifetime.
Set PI_SANDBOX_BIN to select a locally built binary.
"""
import json
import os
import subprocess
import sys


def main():
    process = subprocess.Popen(
        [os.environ.get("PI_SANDBOX_BIN", "pi-sandbox"), "rpc", *sys.argv[1:]],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    )
    response = None
    try:
        process.stdin.write(json.dumps({"id": "state", "type": "get_state"}) + "\n")
        process.stdin.flush()
        for line in process.stdout:
            event = json.loads(line)
            if event.get("type") == "response" and event.get("id") == "state":
                response = event
                break
        # EOF is a shutdown request, not a request to wait for pending work.
        process.stdin.close()
        for _ in process.stdout:
            pass
        code = process.wait()
        if response is not None:
            print(json.dumps(response))
        return code if code else (0 if response and response.get("success") else 1)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                print("Cleanup uncertain; use pi-sandbox stop with this output directory.",
                      file=sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
