#!/usr/bin/env python3
"""Exercise the production SSH session installer without changing host PAM."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parent.parent / 'scripts/guest/codex-session.sh'


class SessionTests(unittest.TestCase):
    def test_preserves_pam_and_installs_missing_session_idempotently(self):
        for common, ssh in [
            ('session required pam_unix.so\n# session optional pam_systemd.so\n', '@include common-session\n'),
            ('session optional pam_systemd.so\n', '@include common-session\n'),
            ('session required pam_unix.so\n', 'session optional /usr/lib/security/pam_systemd.so\n'),
        ]:
            with self.subTest(common=common, ssh=ssh), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                pam = root / 'etc/pam.d'
                pam.mkdir(parents=True)
                (pam / 'common-session').write_text(common)
                (pam / 'sshd').write_text(ssh)
                script = SOURCE.read_text().replace('/etc/', str(root / 'etc') + '/').replace(
                    '/var/lib/', str(root / 'var/lib') + '/')
                env = dict(os.environ, GUEST_USER='ubuntu')
                def install():
                    return subprocess.run(['sh', '-eu', '-c', script + '\nprintf "%s" "$KEYRING_SESSION_UPGRADE"'],
                                          env=env, check=True, capture_output=True, text=True).stdout
                self.assertEqual(install(), '1')
                result = (pam / 'sshd').read_text()
                active = not common.startswith('session optional') and not ssh.startswith('session optional')
                self.assertEqual(result, ssh + (
                    '\n# coop: register SSH sessions with the systemd user manager.\n'
                    'session optional pam_systemd.so\n' if active else ''))
                self.assertEqual((pam / 'common-session').read_text(), common)
                # A failed install has not published its completion marker.
                self.assertEqual(install(), '1')
                marker = root / 'var/lib/coop/codex-session-v1'
                marker.parent.mkdir(parents=True)
                marker.touch()
                self.assertEqual(install(), '0')
                self.assertEqual((pam / 'sshd').read_text(), result)
                self.assertTrue((root / 'var/lib/systemd/linger/ubuntu').is_file())
                rule = root / 'etc/tmpfiles.d/coop-codex-keyring.conf'
                self.assertIn('f ' + str(root / 'var/lib/systemd/linger/ubuntu') + ' 0644 root root -\n', rule.read_text())
                self.assertEqual(rule.stat().st_mode & 0o777, 0o644)
                rule.unlink()
                self.assertEqual(install(), '1')
                self.assertFalse(marker.exists())


if __name__ == '__main__':
    unittest.main()
