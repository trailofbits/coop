#!/usr/bin/env python3
"""Exercise run-integration.sh gate dispatch without a VM, a build, or a remote.

Stubs cargo/git/tar/ssh/scp on PATH and asserts which host-only gates the
runner invokes locally and on a remote, that a failing gate stops the run and
names itself, and that the remote tree is cleaned up either way.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent

GATES = ('integration-network.sh', 'integration-proxy-forward.sh')


class RunIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.calls = self.root / 'calls'
        self.remote_dir = self.root / 'remote'
        self.remote_dir.mkdir()

        (self.root / 'tests').mkdir()
        (self.root / 'Cargo.toml').write_text('[workspace]\nmembers = ["coop"]\n')
        self.runner = self.root / 'tests/run-integration.sh'
        self.runner.write_text((ROOT / 'tests/run-integration.sh').read_text())
        self.runner.chmod(0o755)
        # The VM suite itself is not under test here; it only has to succeed so
        # the assertions can tell "the gates ran" from "the run got that far".
        self.executable('tests/integration.sh',
                        '#!/bin/bash\nprintf \'integration.sh %s\\n\' "$*" >> "$RUNNER_CALLS"\n')
        for name in GATES:
            self.executable('tests/' + name, f'''#!/bin/bash
printf '%s\\n' "${{0##*/}}" >> "$RUNNER_CALLS"
if [[ -n "${{RUNNER_FAIL:-}}" && "${{0##*/}}" == "$RUNNER_FAIL" ]]; then exit 1; fi
''')

        self.bin = self.root / 'bin'
        self.bin.mkdir()
        # ssh answers the two uname probes and mktemp -d, records every command
        # it is handed, and fails when that command names RUNNER_FAIL.
        self.executable('bin/ssh', '''#!/bin/bash
printf 'ssh %s\\n' "$*" >> "$RUNNER_CALLS"
case "$*" in
    *"uname -s"*) echo Linux ;;
    *"uname -m"*) echo x86_64 ;;
    *"mktemp -d"*) echo "$RUNNER_REMOTE_DIR" ;;
esac
if [[ -n "${RUNNER_FAIL:-}" && "$*" == *"$RUNNER_FAIL"* ]]; then exit 1; fi
exit 0
''')
        for name in ('scp', 'cargo', 'git', 'uname'):
            self.executable('bin/' + name,
                            f'#!/bin/bash\nprintf "{name} %s\\n" "$*" >> "$RUNNER_CALLS"\n')
        # `git ls-files -z | tar --null -czf <archive>` still has to produce the
        # archive the runner copies, so create whatever -czf names.
        self.executable('bin/tar', '''#!/bin/bash
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "-czf" ]]; then : > "$2"; fi
    shift
done
printf 'tar %s\\n' "$*" >> "$RUNNER_CALLS"
''')

    def executable(self, name, contents):
        path = self.root / name
        path.write_text(contents)
        path.chmod(0o755)

    def run_runner(self, *args, fail=''):
        return subprocess.run(
            ['bash', str(self.runner), *args],
            env={**os.environ,
                 'PATH': f'{self.bin}:{os.environ["PATH"]}',
                 'RUNNER_CALLS': str(self.calls),
                 'RUNNER_REMOTE_DIR': str(self.remote_dir),
                 'RUNNER_FAIL': fail},
            capture_output=True, text=True, timeout=30)

    def log(self):
        return self.calls.read_text() if self.calls.exists() else ''

    def test_full_runs_both_host_gates_locally(self):
        result = self.run_runner('--full')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for gate in GATES:
            self.assertIn(gate, self.log())
        self.assertIn('PASS: bridge isolation integration test', result.stdout)
        self.assertIn('PASS: proxy reverse forwarding integration test', result.stdout)

    def test_remote_full_runs_both_host_gates_on_the_remote(self):
        result = self.run_runner('--remote', 'user@host', '--full')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for gate in GATES:
            # The gate has to run in the copied source tree, over ssh, not here.
            self.assertIn(f'./tests/{gate}', self.log())
            self.assertNotIn(gate, self.log().split('integration.sh ')[0])
        self.assertIn('PASS: bridge isolation integration test on user@host',
                      result.stdout)
        self.assertIn('PASS: proxy reverse forwarding integration test on user@host',
                      result.stdout)

    def test_remote_without_full_runs_neither_host_gate(self):
        result = self.run_runner('--remote', 'user@host')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for gate in GATES:
            self.assertNotIn(f'./tests/{gate}', self.log())

    def test_a_failing_local_gate_stops_the_run_and_names_itself(self):
        result = self.run_runner('--full', fail='integration-proxy-forward.sh')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('error: proxy reverse forwarding integration test failed',
                      result.stderr)
        self.assertNotIn('integration.sh', self.log())

    def test_a_failing_remote_gate_stops_the_run_and_names_itself(self):
        result = self.run_runner('--remote', 'user@host', '--full',
                                 fail='integration-proxy-forward.sh')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('error: proxy reverse forwarding integration test failed on user@host',
                      result.stderr)
        # A broken gate must not be reported as a passing full run.
        self.assertNotIn('Running tests on user@host', result.stdout)
        self.assertNotIn('integration.sh --binary', self.log())

    def test_remote_tree_is_cleaned_up_on_success_and_on_failure(self):
        for fail in ('', 'integration-proxy-forward.sh'):
            with self.subTest(gate_failure=fail):
                self.calls.unlink(missing_ok=True)
                self.run_runner('--remote', 'user@host', '--full', fail=fail)
                self.assertIn(f'rm -rf {self.remote_dir}', self.log())


if __name__ == '__main__':
    unittest.main()
