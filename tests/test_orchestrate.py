"""Stdlib regression tests for the runnable examples; no Docker or credentials."""
import json
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SKILL_RUNNER_PATH = ROOT / 'bundled-skills/pi-sandbox/assets/orchestrator.py'
SKILL_RUNNER_SPEC = importlib.util.spec_from_file_location('skill_orchestrator',
                                                           SKILL_RUNNER_PATH)
SKILL_RUNNER = importlib.util.module_from_spec(SKILL_RUNNER_SPEC)
sys.modules[SKILL_RUNNER_SPEC.name] = SKILL_RUNNER
SKILL_RUNNER_SPEC.loader.exec_module(SKILL_RUNNER)
FAKE = '''#!/usr/bin/env python3
import json, pathlib, signal, sys, time
args = sys.argv
out = pathlib.Path(args[args.index('--output-dir') + 1])
# Like the real launcher: a nonexistent or existing-but-empty directory is fresh.
out.mkdir(exist_ok=True)
assert not any(out.iterdir())
(out / 'args.json').write_text(json.dumps(args))
if args[1] == 'rpc':
    command = json.loads(input())
    print(json.dumps({'type':'response','id':command['id'],'success':True}), flush=True)
    assert sys.stdin.read() == ''
    sys.exit(0)
prompt = pathlib.Path(args[args.index('--prompt-file') + 1]).read_text()
if prompt == 'missing':
    sys.exit(2)
def finish(code, status, **extra):
    (out / 'result.json').write_text(json.dumps({'status':status, **extra}))
    sys.exit(code)
if prompt.startswith('capacity'):
    # A transient provider failure: the launcher records the classified error
    # and the session file that a resume would continue from.
    (out / 'sessions').mkdir()
    session = out / 'sessions' / 'session.jsonl'
    session.write_text('{}\\n')
    finish(1, 'failed', reason='agent_error', session_files=[str(session)],
           error={'class':'provider_capacity','recoverable':True,
                  'message':'Selected model is at capacity. Please try a different model.'})
if prompt == 'auth':
    finish(1, 'failed', reason='agent_error', session_files=[],
           error={'class':'auth','recoverable':False,'message':'401 Unauthorized'})
signal.signal(signal.SIGTERM, lambda *args: finish(143, 'cancelled'))
(out / 'started').write_text(str(pathlib.Path.cwd()))
if prompt == 'slow':
    time.sleep(30)
else:
    # Exceed normal pipe capacity on both streams.
    for _ in range(2048):
        print(json.dumps({'type':'pi_event','data':'x' * 1024}))
        print('diagnostic' * 100, file=sys.stderr)
    time.sleep(.2)
finish(1 if prompt == 'fail' else 0, 'failed' if prompt == 'fail' else 'completed')
'''


class ExamplesTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.binary = self.root / 'fake-pi-sandbox'
        self.binary.write_text(FAKE)
        self.binary.chmod(0o700)
        repo = self.root / 'repo'
        repo.mkdir()
        def git(*args):
            subprocess.run(['git', *args], cwd=repo, check=True, capture_output=True)
        git('init', '-q')
        git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
            'commit', '--allow-empty', '-qm', 'base')
        self.worktrees = [self.root / f'wt-{i}' for i in range(3)]
        for i, worktree in enumerate(self.worktrees):
            git('worktree', 'add', '-qb', f'worker-{i}', str(worktree))

    def command(self, prompts, namespace=None):
        jobs = [{'name':str(i), 'worktree':str(w), 'prompt':p}
                for i, (w, p) in enumerate(zip(self.worktrees, prompts))]
        path = self.root / 'jobs.json'
        path.write_text(json.dumps(jobs))
        command = [sys.executable, str(ROOT / 'examples/orchestrate.py'),
                   '--binary', str(self.binary), '--jobs', str(path),
                   '--output-root', str(self.root / 'batch')]
        if namespace:
            command += ['--namespace', namespace]
        return command

    def test_parallel_large_streams_and_private_files(self):
        result = subprocess.run(self.command(['ok'] * 3), capture_output=True,
                                text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(result.stdout.splitlines()), 3)
        for i, worktree in enumerate(self.worktrees):
            self.assertEqual((self.root / f'batch/{i}/started').read_text(), str(worktree))
            self.assertGreater((self.root / f'batch/{i}.stdout.jsonl').stat().st_size, 1000000)
        for path in (self.root / 'batch').rglob('*'):
            self.assertEqual(path.stat().st_mode & 0o077, 0)

    def test_failure_does_not_hide_success(self):
        result = subprocess.run(self.command(['fail', 'ok']), capture_output=True,
                                text=True, timeout=15)
        self.assertEqual(result.returncode, 1)
        self.assertEqual([json.loads(line)['status'] for line in result.stdout.splitlines()],
                         ['failed', 'completed'])

    def test_duplicate_worktrees_rejected(self):
        self.worktrees[1] = self.worktrees[0]
        result = subprocess.run(self.command(['ok', 'ok']), capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertFalse((self.root / 'batch').exists())

    def test_missing_result_is_failure(self):
        result = subprocess.run(self.command(['missing']), capture_output=True,
                                text=True, timeout=10)
        self.assertEqual(result.returncode, 1)
        summary = json.loads(result.stdout)
        self.assertEqual(summary['status'], 'missing_result')
        self.assertEqual(summary['exit_code'], 2)

    def test_namespace_is_forwarded_to_every_worker(self):
        result = subprocess.run(self.command(['ok', 'ok'], namespace='shared_home'),
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        for i in range(2):
            args = json.loads((self.root / f'batch/{i}/args.json').read_text())
            self.assertEqual(args[args.index('--namespace') + 1], 'shared_home')

    def test_cancellation(self):
        process = subprocess.Popen(self.command(['slow'] * 3), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 10
            while not all((self.root / f'batch/{i}/started').exists() for i in range(3)):
                if time.monotonic() > deadline:
                    self.fail('workers did not start')
                time.sleep(.02)
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 143, stderr)
            self.assertEqual([json.loads(line)['status'] for line in stdout.splitlines()],
                             ['cancelled'] * 3)
        finally:
            if process.poll() is None:
                process.terminate()
                process.communicate(timeout=25)

    def test_rpc_waits_for_response(self):
        result = subprocess.run([sys.executable, str(ROOT / 'examples/rpc_state.py'),
                                 '--output-dir', str(self.root / 'rpc'), '--timeout', '5'],
                                env=os.environ | {'PI_SANDBOX_BIN':str(self.binary)},
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)['success'])

    def test_bundled_skill_runner_executes_and_collects_a_worker(self):
        env_file = self.root / 'worker.env'
        env_file.write_text('TOKEN=secret\n')
        outcome = SKILL_RUNNER.run_worker(
            SKILL_RUNNER.Worker('skill-worker', self.worktrees[0], 'ok'),
            output_root=self.root / 'skill-run', binary=str(self.binary), timeout=10,
            env_files=[env_file], runtime_env={'APP_ENV': 'test'},
            namespace='shared_home')

        self.assertTrue(outcome.completed)
        self.assertEqual(outcome.status, 'completed')
        args = json.loads((outcome.output_dir / 'args.json').read_text())
        self.assertEqual(args[args.index('--env-file') + 1], str(env_file))
        self.assertEqual(args[args.index('--env') + 1], 'APP_ENV=test')
        self.assertEqual(args[args.index('--namespace') + 1], 'shared_home')
        self.assertEqual(outcome.output_dir, self.root / 'skill-run/skill-worker-01')
        self.assertGreater((self.root / 'skill-run/skill-worker-01.stdout.jsonl').stat().st_size,
                           1000000)
        self.assertTrue((self.root / 'skill-run/skill-worker-01.launcher.log').is_file())

        retry = SKILL_RUNNER.run_worker(
            SKILL_RUNNER.Worker('skill-worker', self.worktrees[0], 'again'),
            output_root=self.root / 'skill-run', binary=str(self.binary), timeout=10)
        self.assertTrue(retry.completed)
        self.assertEqual(retry.output_dir, self.root / 'skill-run/skill-worker-02')
        self.assertTrue((self.root / 'skill-run/skill-worker-02.prompt.txt').is_file())
        self.assertTrue((self.root / 'skill-run/skill-worker-01/args.json').is_file())


    def test_run_with_recovery_resumes_recoverable_failures(self):
        # Prompt "capacity" fails; the resumed prompt (the recovery text) succeeds.
        logs = []
        attempts = SKILL_RUNNER.run_with_recovery(
            SKILL_RUNNER.Worker('flaky', self.worktrees[0], 'capacity'),
            output_root=self.root / 'recover', binary=str(self.binary), timeout=10,
            backoff_seconds=(0,), max_attempts=3, log=logs.append,
            pi_args=['--model', 'primary'], fallback_pi_args=['--model', 'backup'])
        self.assertEqual([a.status for a in attempts], ['failed', 'completed'])
        first, second = attempts
        self.assertEqual(first.error['class'], 'provider_capacity')
        self.assertTrue(first.recoverable)
        self.assertEqual(second.output_dir, self.root / 'recover/flaky-02')
        args = json.loads((second.output_dir / 'args.json').read_text())
        self.assertEqual(args[args.index('--resume-session') + 1],
                         str(first.output_dir / 'sessions/session.jsonl'))
        self.assertEqual(args[args.index('--model') + 1], 'backup')
        resumed_prompt = (self.root / 'recover/flaky-02.prompt.txt').read_text()
        self.assertIn('provider_capacity: Selected model is at capacity', resumed_prompt)
        self.assertIn('git status', resumed_prompt)
        self.assertTrue(any('resuming in 0s' in line for line in logs), logs)

    def test_run_with_recovery_stops_on_unrecoverable_and_bounded_attempts(self):
        attempts = SKILL_RUNNER.run_with_recovery(
            SKILL_RUNNER.Worker('locked', self.worktrees[0], 'auth'),
            output_root=self.root / 'recover', binary=str(self.binary), timeout=10,
            backoff_seconds=(0,), log=lambda _: None)
        self.assertEqual(len(attempts), 1)
        self.assertFalse(attempts[0].recoverable)
        with self.assertRaisesRegex(RuntimeError, 'auth: 401 Unauthorized'):
            SKILL_RUNNER.require_completed(attempts[0])

        # Every attempt fails the same way: bounded by max_attempts. The fake
        # keeps failing because the resumed prompt is rewritten to start with
        # "capacity" below.
        attempts = SKILL_RUNNER.run_with_recovery(
            SKILL_RUNNER.Worker('down', self.worktrees[1], 'capacity'),
            output_root=self.root / 'recover', binary=str(self.binary), timeout=10,
            backoff_seconds=(0,), max_attempts=2, log=lambda _: None,
            recovery_prompt='capacity again: {error_class} {error_message}')
        self.assertEqual([a.status for a in attempts], ['failed', 'failed'])
        self.assertEqual(attempts[1].output_dir, self.root / 'recover/down-02')


if __name__ == '__main__':
    unittest.main()
