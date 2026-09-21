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
import json, os, pathlib, signal, sys, time
args = sys.argv
if args[1] == 'namespace':
    print(json.dumps({'pi_credentials': 'CREDS' in os.environ.get('FAKE_NAMESPACE', '')}))
    sys.exit(0)
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
def fail_with(cls, recovery, message):
    # Like the real launcher: a classified error plus the session file a
    # resume would continue from.
    (out / 'sessions').mkdir()
    path = out / 'sessions' / 'session.jsonl'
    path.write_text(json.dumps({'type':'message','message':{'role':'assistant',
        'content':[{'type':'text','text':'I finished step one; step two remains.'}]}}) + '\\n')
    finish(1, 'failed', reason='agent_error', session_files=[str(path)],
           error={'class':cls,'recovery':recovery,'recoverable':recovery == 'resume',
                  'message':message})
if prompt.startswith('capacity'):
    fail_with('provider_capacity', 'resume',
              'Selected model is at capacity. Please try a different model.')
if prompt.startswith('timeout'):
    fail_with('timeout', 'resume_adjusted', 'run exceeded --timeout before the agent finished')
if prompt.startswith('overflow'):
    fail_with('context_overflow', 'restart', 'maximum context length exceeded')
if prompt == 'auth':
    fail_with('auth', 'fix_environment', '401 Unauthorized')
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


    def recover(self, name, prompt, worktree=0, **kwargs):
        logs = []
        kwargs.setdefault('log', logs.append)
        attempts = SKILL_RUNNER.run_with_recovery(
            SKILL_RUNNER.Worker(name, self.worktrees[worktree], prompt),
            output_root=self.root / 'recover', binary=str(self.binary), timeout=10,
            backoff_seconds=(0,), **kwargs)
        return attempts, logs

    def test_run_with_recovery_resumes_transient_failures(self):
        # Prompt "capacity" fails; the resumed prompt (the recovery text) succeeds.
        attempts, logs = self.recover(
            'flaky', 'capacity', max_attempts=3, fallback_after=1,
            pi_args=['--model', 'primary'], fallback_pi_args=['--model', 'backup'])
        self.assertEqual([a.status for a in attempts], ['failed', 'completed'])
        first, second = attempts
        self.assertEqual(first.error['class'], 'provider_capacity')
        self.assertTrue(first.recoverable)
        self.assertIsNone(second.escalation)
        self.assertEqual(second.output_dir, self.root / 'recover/flaky-02')
        args = json.loads((second.output_dir / 'args.json').read_text())
        self.assertEqual(args[args.index('--resume-session') + 1],
                         str(first.output_dir / 'sessions/session.jsonl'))
        self.assertEqual(args[args.index('--model') + 1], 'backup')
        resumed_prompt = (self.root / 'recover/flaky-02.prompt.txt').read_text()
        self.assertIn('provider_capacity: Selected model is at capacity', resumed_prompt)
        self.assertIn('git status', resumed_prompt)
        self.assertTrue(any('resume in 0s' in line for line in logs), logs)
        self.assertTrue(any('swapping to backup' in line for line in logs), logs)

    def test_run_with_recovery_alternates_between_model_pair(self):
        # A single capacity error gets one retry on the same model; a second
        # in a row swaps to the fallback, and the fallback hands back the
        # same way rather than keeping the work on the weaker model.
        attempts, logs = self.recover(
            'ping-pong', 'capacity',
            recovery_prompt='capacity again: {error_class}{advice}',
            pi_args=['--provider', 'openai-codex', '--model', 'gpt-5.6-sol'],
            fallback_pi_args=['--provider', 'openai-codex', '--model', 'gpt-5.5'])
        models = []
        for outcome in attempts:
            args = json.loads((outcome.output_dir / 'args.json').read_text())
            models.append(args[args.index('--model') + 1])
        self.assertEqual(models, ['gpt-5.6-sol', 'gpt-5.6-sol', 'gpt-5.5', 'gpt-5.5', 'gpt-5.6-sol'])
        swaps = [line for line in logs if 'swapping to' in line]
        self.assertEqual(len(swaps), 2, logs)
        self.assertIn('2 capacity failures in a row; swapping to openai-codex/gpt-5.5', swaps[0])
        self.assertIn('swapping to openai-codex/gpt-5.6-sol', swaps[1])
        self.assertIn('giving up after 5 attempts', attempts[-1].escalation)

    def test_run_with_recovery_adjusts_once_for_timeouts(self):
        attempts, _ = self.recover('slowpoke', 'timeout', timeout_growth=3)
        self.assertEqual([a.status for a in attempts], ['failed', 'completed'])
        args = json.loads((attempts[1].output_dir / 'args.json').read_text())
        self.assertEqual(args[args.index('--timeout') + 1], '30')
        prompt = (self.root / 'recover/slowpoke-02.prompt.txt').read_text()
        self.assertIn('time limit', prompt)

        # A second timeout in a row is an escalation, not another attempt.
        attempts, _ = self.recover(
            'stuck', 'timeout', worktree=1,
            recovery_prompt='timeout again {error_class} {error_message}{advice}')
        self.assertEqual([a.status for a in attempts], ['failed', 'failed'])
        self.assertIn('recurred after an adjusted attempt', attempts[-1].escalation)
        with self.assertRaisesRegex(RuntimeError, 'needs a human: timeout recurred'):
            SKILL_RUNNER.require_completed(attempts[-1])

    def test_run_with_recovery_restarts_after_context_overflow(self):
        subprocess.run(['git', '-c', 'user.name=T', '-c', 'user.email=t@x.invalid',
                        'commit', '-q', '--allow-empty', '-m', 'progress: step one'],
                       cwd=self.worktrees[0], check=True, capture_output=True)
        attempts, _ = self.recover('bloated', 'overflow: do the thing')
        self.assertEqual([a.status for a in attempts], ['failed', 'completed'])
        args = json.loads((attempts[1].output_dir / 'args.json').read_text())
        self.assertNotIn('--resume-session', args)
        handoff = (self.root / 'recover/bloated-02.prompt.txt').read_text()
        self.assertIn('overflow: do the thing', handoff)
        self.assertIn('progress: step one', handoff)
        self.assertIn('I finished step one; step two remains.', handoff)

        attempts, _ = self.recover('hopeless', 'overflow', worktree=1,
                                   handoff_prompt=lambda w, o: 'overflow again')
        self.assertEqual([a.status for a in attempts], ['failed', 'failed'])
        self.assertIn('split the assignment', attempts[-1].escalation)

    def test_run_with_recovery_checks_credentials_before_escalating(self):
        attempts, _ = self.recover('locked', 'auth')
        self.assertEqual(len(attempts), 1)
        self.assertEqual(attempts[0].recovery, 'fix_environment')
        self.assertIn('human must log in', attempts[0].escalation)
        with self.assertRaisesRegex(RuntimeError, 'auth: 401 Unauthorized; needs a human'):
            SKILL_RUNNER.require_completed(attempts[0])

        os.environ['FAKE_NAMESPACE'] = 'CREDS'
        self.addCleanup(os.environ.pop, 'FAKE_NAMESPACE', None)
        attempts, logs = self.recover('refreshed', 'auth', worktree=1)
        self.assertEqual([a.status for a in attempts], ['failed', 'completed'])
        self.assertTrue(any('transient once' in line for line in logs), logs)

    def test_run_with_recovery_is_bounded(self):
        attempts, _ = self.recover(
            'down', 'capacity', max_attempts=2,
            recovery_prompt='capacity again: {error_class} {error_message}{advice}')
        self.assertEqual([a.status for a in attempts], ['failed', 'failed'])
        self.assertEqual(attempts[1].output_dir, self.root / 'recover/down-02')
        self.assertIn('giving up after 2 attempts', attempts[-1].escalation)


if __name__ == '__main__':
    unittest.main()
