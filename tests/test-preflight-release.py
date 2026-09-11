#!/usr/bin/env python3
"""Exercise release preflight dispatch and version gates without building VMs."""
import os
import re
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class PreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for directory in ('scripts', 'tests', 'bin'):
            (self.root / directory).mkdir()
        (self.root / 'scripts/preflight-release.sh').write_text(
            (ROOT / 'scripts/preflight-release.sh').read_text())
        (self.root / 'Cargo.toml').write_text(
            '[workspace.package]\nversion = "9.8.7"\n'
            '[package]\nname = "coop"\nversion.workspace = true\n')
        self.lock()
        (self.root / 'CHANGELOG.md').write_text('## v9.8.7\n\nRelease notes.\n')
        self.log = self.root / 'calls'
        for name in ('cargo', 'cargo-deny', 'taplo', 'zizmor', 'cargo-kani'):
            self.executable('bin/' + name, '''#!/bin/bash
printf '%s %s\n' "${0##*/}" "$*" >> "$PREFLIGHT_CALLS"
if [[ "${0##*/} $*" == "${PREFLIGHT_FAIL:-}" ]]; then exit 1; fi
''')
        self.executable('bin/rustup', '#!/bin/bash\necho aarch64-unknown-linux-gnu\necho aarch64-unknown-linux-musl\n')
        self.executable('bin/uname', '#!/bin/bash\necho Linux\n')
        self.executable('bin/git', '''#!/bin/bash
if [[ "$1" == rev-parse ]]; then exit 1; fi
''')
        for name in ('integration-install.sh', 'integration-update.sh',
                     'integration-uninstall.sh', 'integration-network.sh',
                     'integration-proxy-forward.sh'):
            self.executable('tests/' + name, '''#!/bin/bash
printf '%s\n' "${0##*/}" >> "$PREFLIGHT_CALLS"
if [[ "${0##*/}" == "${PREFLIGHT_FAIL:-}" ]]; then exit 1; fi
''')
        for name in ('test-integration-probes.py', 'test-preflight-release.py'):
            (self.root / 'tests' / name).write_text(
                'import os\nwith open(os.environ["PREFLIGHT_CALLS"], "a") as f:\n'
                f'    f.write("{name}\\n")\n')

    def executable(self, name, contents):
        path = self.root / name
        path.write_text(contents)
        path.chmod(0o755)

    def lock(self, proxy='9.8.7'):
        (self.root / 'Cargo.lock').write_text(
            '[[package]]\nname = "coop"\nversion = "9.8.7"\n'
            f'[[package]]\nname = "coop-proxy"\nversion = "{proxy}"\n')

    def run_preflight(self, fail=''):
        return subprocess.run(
            ['bash', str(self.root / 'scripts/preflight-release.sh'), '--quick'],
            env={**os.environ, 'PATH': str(self.root / 'bin') + ':' + os.environ['PATH'],
                 'PREFLIGHT_CALLS': str(self.log), 'PREFLIGHT_FAIL': fail},
            capture_output=True, text=True, timeout=20)

    def test_workspace_version_and_gates(self):
        result = self.run_preflight()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Version sources agree; tag v9.8.7 is free.', result.stdout)
        calls = self.log.read_text().splitlines()
        for call in ('cargo clippy --workspace --all-targets --all-features -- -D warnings',
                     'cargo test --workspace', 'cargo deny --workspace check',
                     'cargo build --release --workspace --target aarch64-unknown-linux-musl',
                     'taplo format --check', 'test-integration-probes.py',
                     'test-preflight-release.py', 'integration-network.sh',
                     'integration-proxy-forward.sh'):
            self.assertIn(call, calls)
        self.assertNotIn('Next: tag', result.stdout)
        self.assertIn('unrun gates before tagging', result.stdout)

    def test_non_linux_namespace_gates_are_reported_as_unrun(self):
        self.executable('bin/uname', '#!/bin/bash\necho Darwin\n')
        result = self.run_preflight()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('Bridge isolation requires Linux', result.stdout)
        self.assertIn('Proxy reverse forwarding requires Linux', result.stdout)
        self.assertNotIn('integration-proxy-forward.sh', self.log.read_text().splitlines())
        self.assertNotIn('integration-network.sh', self.log.read_text().splitlines())
        self.assertNotIn('Next: tag', result.stdout)

    def test_proxy_lock_mismatch_fails(self):
        self.lock(proxy='9.8.6')
        result = self.run_preflight()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Cargo.lock coop-proxy version (9.8.6)', result.stdout)

    def test_proxy_forward_failure_is_fatal(self):
        result = self.run_preflight(fail='integration-proxy-forward.sh')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('FAIL: Integration — proxy reverse forwarding', result.stdout)

    def test_workspace_test_failure_is_fatal(self):
        result = self.run_preflight(fail='cargo test --workspace')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('FAIL: Unit tests', result.stdout)


class ReleaseBinaryTests(unittest.TestCase):
    def test_packaging_requires_release_identity_and_companion(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        block = re.search(
            r"      - name: Verify release binaries\n.*?        run: \|\n(.*?)\n      - name:",
            workflow, re.S)[1]
        script = "\n".join(line[10:] for line in block.splitlines())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary_dir = root / 'target/test-target/release'
            binary_dir.mkdir(parents=True)
            (root / 'bin').mkdir()
            git = root / 'bin/git'
            git.write_text('#!/bin/sh\necho abc1234\n')
            git.chmod(0o755)
            proxy = binary_dir / 'coop-proxy'
            proxy.write_text('#!/bin/sh\nexit 0\n')
            proxy.chmod(0o755)
            coop = binary_dir / 'coop'
            for version, companion, expected in [('coop 9.8.7 (abc1234)', True, 0),
                                                 ('coop 9.8.7-dev (abc1234+dirty)', True, 1),
                                                 ('coop 9.8.6 (abc1234)', True, 1),
                                                 ('coop 9.8.7 (abc1234)', False, 1)]:
                with self.subTest(version=version, companion=companion):
                    if not companion:
                        proxy.unlink()
                    coop.write_text(f"#!/bin/sh\nprintf '%s\\n' '{version}'\n")
                    coop.chmod(0o755)
                    result = subprocess.run(
                        ['bash', '-euc', script], cwd=root,
                        env={**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH'],
                             'TAG': 'v9.8.7', 'TARGET': 'test-target'},
                        capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, expected, result.stderr)


if __name__ == '__main__':
    unittest.main()
