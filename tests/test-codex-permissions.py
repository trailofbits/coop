#!/usr/bin/env python3
"""System-default installation and opt-in real Codex permission precedence."""
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time
import unittest

class InstallTests(unittest.TestCase):
    def test_defaults_are_complete_and_existing_config_survives(self):
        root = Path(__file__).resolve().parent.parent
        installer = (root / 'scripts/guest/codex-permissions.sh').read_text()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'codex'
            script = installer.replace('/etc/codex', str(root))
            subprocess.run(['sh', '-c', script], check=True)
            config = root / 'config.toml'
            self.assertIn('approval_policy = "never"', config.read_text())
            self.assertIn('default_permissions = ":danger-full-access"', config.read_text())
            self.assertEqual(config.stat().st_mode & 0o777, 0o644)
            custom = 'approval_policy = "on-request"\nsandbox_mode = "read-only"\n'
            config.write_text(custom)
            subprocess.run(['sh', '-c', script], check=True)
            self.assertEqual(config.read_text(), custom)
            self.assertEqual(sorted(p.name for p in root.iterdir()), ['config.toml'])
            config.unlink()
            config.symlink_to(root / 'absent-admin-config')
            subprocess.run(['sh', '-c', script], check=True)
            self.assertTrue(config.is_symlink())
            self.assertFalse(config.exists())


@unittest.skipUnless(os.environ.get('COOP_TEST_CODEX'), 'set COOP_TEST_CODEX for real app-server tests')
class ServerTests(unittest.TestCase):
    def test_system_defaults_and_explicit_thread_choices(self):
        # The integration guest must have the production system defaults. The
        # empty temporary CODEX_HOME proves these are not user-config defaults.
        self.assertTrue(Path('/etc/codex/config.toml').is_file())
        for label, cli, config, expected in [
            ('default', [], '', ('never', 'dangerFullAccess')),
            ('ask', ['-c', 'sandbox_mode="workspace-write"', '-c',
                     'approval_policy="on-request"'], '', ('on-request', 'workspaceWrite')),
            ('caller flags', ['-c', 'sandbox_mode="workspace-write"', '-c',
                              'approval_policy="on-request"', '-c',
                              'sandbox_mode="read-only"', '-c',
                              'approval_policy="never"'], '', ('never', 'readOnly')),
            ('user legacy config', [], 'sandbox_mode="read-only"\napproval_policy="on-request"\n',
             ('on-request', 'readOnly')),
            ('user profile config', [], 'default_permissions=":read-only"\napproval_policy="on-request"\n',
             ('on-request', 'readOnly')),
        ]:
            with self.subTest(label=label), tempfile.TemporaryDirectory(prefix='coop-permissions-') as directory:
                env = {k: v for k, v in os.environ.items()
                       if not k.startswith(('CODEX_', 'OPENAI_'))}
                env['CODEX_HOME'] = directory
                Path(directory, 'config.toml').write_text(config + '[features]\nplugins=false\n')
                process = subprocess.Popen([os.environ['COOP_TEST_CODEX'], *cli, 'app-server', '--stdio'],
                                           env=env, cwd=directory, stdin=subprocess.PIPE,
                                           stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                           text=True)
                self.sequence = 0
                self.pending = b''
                try:
                    self.rpc(process, 'initialize', {'clientInfo': {'name': 'coop_permissions_test', 'version': '1'},
                                                     'capabilities': {'experimentalApi': True}})
                    result = self.rpc(process, 'thread/start', {'cwd': directory, 'ephemeral': True})
                    self.assertEqual((result['approvalPolicy'], result['sandbox']['type']), expected)
                    if label == 'default':
                        result = self.rpc(process, 'command/exec', {'command': ['/usr/bin/true'],
                                                                  'cwd': directory, 'timeoutMs': 5000})
                        self.assertEqual(result['exitCode'], 0, result)
                        for permissions, approval, sandbox in [
                            (':workspace', 'on-request', 'workspaceWrite'),
                            (':danger-full-access', 'never', 'dangerFullAccess'),
                        ]:
                            result = self.rpc(process, 'thread/start', {
                                'cwd': directory, 'ephemeral': True,
                                'permissions': permissions, 'approvalPolicy': approval})
                            self.assertEqual((result['approvalPolicy'], result['sandbox']['type']),
                                             (approval, sandbox))
                    self.assertFalse(Path(directory, 'auth.json').exists())
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                    process.stdin.close()
                    process.stdout.close()

    def rpc(self, process, method, params):
        self.sequence += 1
        process.stdin.write(json.dumps({'id': self.sequence, 'method': method, 'params': params}) + '\n')
        process.stdin.flush()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if b'\n' not in self.pending:
                if not select.select([process.stdout], [], [], 1)[0]:
                    continue
                chunk = os.read(process.stdout.fileno(), 65536)
                self.assertTrue(chunk, 'server exited before responding')
                self.pending += chunk
                continue
            line, self.pending = self.pending.split(b'\n', 1)
            message = json.loads(line)
            if message.get('id') == self.sequence:
                self.assertNotIn('error', message, message)
                return message['result']
        self.fail('app-server request timed out: ' + method)


if __name__ == '__main__':
    unittest.main()
