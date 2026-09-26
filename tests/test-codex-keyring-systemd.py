#!/usr/bin/python3
"""Disposable-user production PAM/systemd/SSH tests (Linux, explicit opt-in).

sudo COOP_TEST_KEYRING_SYSTEMD=1 COOP_TEST_CODEX=/path/to/native/bin/codex \
  python3 tests/test-codex-keyring-systemd.py -v
Requires OpenSSH on localhost, systemd, python3-dbus, python3-websocket, strace, libpam development headers,
and libpam-gnome-keyring. Optional COOP_TEST_PAM_INCLUDE / COOP_TEST_PAM_MODULE
point to unpacked distribution packages without installing a global PAM hook.
Only disposable fixture passwords and invalid API-key strings are used.
"""
import json
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import pwd
import select
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
import uuid

ROOT = Path(__file__).resolve().parent.parent
ENABLED = os.environ.get('COOP_TEST_KEYRING_SYSTEMD') == '1'


def run(args, **kwargs):
    return subprocess.run(args, text=True, capture_output=True, timeout=120, check=True, **kwargs)


@unittest.skipUnless(ENABLED, 'explicit disposable-user systemd test opt-in required')
class SystemdTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.geteuid() != 0:
            raise RuntimeError('run this fixture with sudo')
        cls.pam_config = Path('/etc/pam.d/coop-codex-keyring')
        if cls.pam_config.exists():
            raise RuntimeError('fixture requires an unused /etc/pam.d/coop-codex-keyring')
        cls.root = Path(tempfile.mkdtemp(prefix='coop-keyring-systemd-', dir='/var/tmp'))
        cls.root.chmod(0o755)
        cls.name = 'coop-kr-' + uuid.uuid4().hex[:8]
        cls.uid = None
        cls.addClassCleanup(cls.cleanup)
        run(['useradd', '--home-dir', str(cls.root / 'home'), '--create-home', '--shell', '/bin/bash', cls.name])
        cls.uid = pwd.getpwnam(cls.name).pw_uid
        cls.home = cls.root / 'home'
        (cls.home / '.ssh').mkdir(mode=0o700)
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(cls.root / 'id')])
        shutil.copy(cls.root / 'id.pub', cls.home / '.ssh/authorized_keys')
        cls.ssh = ['ssh', '-i', str(cls.root / 'id'), '-o', 'BatchMode=yes',
                   '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=/dev/null',
                   '-o', 'LogLevel=ERROR', cls.name + '@127.0.0.1']
        cls.binary = Path(os.environ['COOP_TEST_CODEX']).resolve()
        standalone = cls.home / '.codex/packages/standalone'
        standalone.mkdir(parents=True)
        (standalone / 'current').symlink_to(cls.binary.parent.parent)
        (cls.home / '.codex/config.toml').write_text('cli_auth_credentials_store = "keyring"\n[features]\nplugins = false\n')
        run(['chown', '-R', cls.name + ':' + cls.name, str(cls.home)])
        include = os.environ.get('COOP_TEST_PAM_INCLUDE', '/usr/include')
        run(['cc', '-std=c11', '-O2', '-Wall', '-Wextra', '-Werror', '-I' + include,
             os.environ.get('COOP_TEST_PAM_SOURCE', str(ROOT / 'scripts/guest/codex-keyring-pam.c')), '-l:libpam.so.0', '-o', str(cls.root / 'pam')])
        module = os.environ.get('COOP_TEST_PAM_MODULE', 'pam_gnome_keyring.so')
        cls.pam_config.write_text('auth required pam_exec.so expose_authtok /usr/bin/true\n'
                                  'auth required ' + module + '\n')
        (cls.root / 'old-boot').write_text('disposable-install-boot')
        cls.launch = cls.root / 'launch.py'
        cls.launch.write_text(f'''import importlib.util,sys,os
from pathlib import Path
spec=importlib.util.spec_from_file_location('keyring', {str(ROOT / 'scripts/guest/codex-keyring.py')!r})
k=importlib.util.module_from_spec(spec);spec.loader.exec_module(k)
k.PAM={str(cls.root / 'pam')!r}
k.CODEX={str(cls.binary)!r}
k.MIGRATION=Path({str(cls.root / 'old-boot')!r})
operation=sys.argv[1];sys.argv=sys.argv[:1]
if operation=='unlock':
    try:k.main()
    except k.Error as error:
        print(error.kind.name, file=sys.stderr);sys.exit(1)
elif operation=='state':
    keyring=k.Keyring(Path(os.environ['XDG_RUNTIME_DIR']))
    print(keyring.collection(k.storage(Path.home())))
elif operation=='native-credentials':
    import json
    prototype_spec=importlib.util.spec_from_file_location('prototype', {str(ROOT / 'tests/test-codex-desktop-prototype.py')!r})
    prototype=importlib.util.module_from_spec(prototype_spec);prototype_spec.loader.exec_module(prototype)
    rpc=prototype.DesktopPrototypeTests().rpc
    native=json.loads(k.run([k.CODEX,'app-server','daemon','start']).stdout)
    rpc(native['socketPath'],'account/login/start',{{'type':'apiKey','apiKey':'sk-coop-disposable-invalid-key'}})
    assert rpc(native['socketPath'],'account/read',{{'refreshToken':False}})['account']['type']=='apiKey'
    assert not (Path.home()/'.codex/auth.json').exists()
    rpc(native['socketPath'],'account/logout',None)
    assert rpc(native['socketPath'],'account/read',{{'refreshToken':False}})['account'] is None
elif operation=='probe-items':
    keyring=k.Keyring(Path(os.environ['XDG_RUNTIME_DIR']))
    unlocked,locked=keyring.call(k.ROOT,k.SERVICE,'SearchItems',{{'service':'coop-codex-readiness'}})
    print(len(unlocked)+len(locked))
elif operation=='lock':
    keyring=k.Keyring(Path(os.environ['XDG_RUNTIME_DIR']))
    keyring.call(k.ROOT,k.SERVICE,'Lock',k.dbus.Array([k.LOGIN],signature='o'))
''')
        run(['loginctl', 'enable-linger', cls.name])
        cls.remote('systemctl --user add-wants default.target gnome-keyring-daemon.service')
        cls.remote('systemctl --user start gnome-keyring-daemon.service')

    @classmethod
    def cleanup(cls):
        if cls.uid is not None:
            # User manager termination kills the fixture's native updater too;
            # native stop by itself deliberately does not claim that ownership.
            for args in (['loginctl', 'disable-linger', cls.name],
                         ['loginctl', 'terminate-user', cls.name],
                         ['systemctl', 'stop', f'user@{cls.uid}.service', f'user-runtime-dir@{cls.uid}.service'],
                         ['pkill', '-KILL', '-u', str(cls.uid)],
                         ['userdel', '--remove', cls.name]):
                subprocess.run(args, capture_output=True, timeout=20)
        cls.pam_config.unlink(missing_ok=True)
        shutil.rmtree(cls.root)

    @classmethod
    def remote(cls, command, **kwargs):
        return run(cls.ssh + [command], **kwargs).stdout.strip()

    def unlock(self, password='disposable-guest-password', confirmation=None, success=True):
        # -tt allocates a real remote TTY even though this harness uses pipes.
        process = subprocess.Popen(self.ssh[:-1] + ['-tt', self.ssh[-1],
            '/usr/bin/python3 ' + str(self.launch) + ' unlock'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        output = b''
        prompted = confirmed = False
        deadline = time.monotonic() + 120
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([process.stdout], [], [], 0.2)
                if ready:
                    chunk = os.read(process.stdout.fileno(), 65536)
                    if not chunk:
                        break
                    output += chunk
                    if not prompted and b'Codex keyring password: ' in output:
                        process.stdin.write(password.encode() + b'\n')
                        process.stdin.flush()
                        prompted = True
                    if not confirmed and b'Confirm keyring password: ' in output:
                        process.stdin.write((confirmation if confirmation is not None else password).encode() + b'\n')
                        process.stdin.flush()
                        confirmed = True
            code = process.wait(timeout=5)
            if success:
                self.assertEqual(code, 0, output.decode(errors='replace') + self.remote('systemctl --user show gnome-keyring-daemon.service -p Result -p ActiveState -p NRestarts'))
            else:
                self.assertNotEqual(code, 0, output.decode(errors='replace'))
            if password:
                self.assertNotIn(password.encode(), output, 'password was echoed')
            return output
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            process.stdin.close()
            process.stdout.close()

    def pid(self):
        return self.remote('systemctl --user show gnome-keyring-daemon.service -p MainPID --value')

    def state(self):
        return self.remote('/usr/bin/python3 ' + str(self.launch) + ' state')

    def test_production_lifecycle(self):
        self.unlock(password='', success=False)
        self.unlock(confirmation='different-disposable-password', success=False)
        self.assertFalse((self.home / '.local/share/keyrings/login.keyring').exists())
        self.unlock()
        first = self.pid()
        self.assertEqual(self.state(), 'False')
        self.remote('secret-tool store --label=disposable service coop-systemd-test', input='disposable-token')
        self.assertEqual(self.remote('secret-tool lookup service coop-systemd-test'), 'disposable-token')
        raw = (self.home / '.local/share/keyrings/login.keyring').read_bytes()
        self.assertTrue(raw.startswith(b'GnomeKeyring\n\r\0\n' + bytes(4)))
        self.assertNotIn(b'disposable-token', raw)
        self.unlock()
        with ThreadPoolExecutor(max_workers=2) as workers:
            list(workers.map(lambda _: self.unlock(), range(2)))
        self.assertEqual(self.pid(), first, 'repeated/duplicate unlock churned the service')
        self.assertEqual(self.remote('/usr/bin/python3 ' + str(self.launch) + ' probe-items'), '0')
        # No SSH sessions remain; a fresh login uses the same unlocked service.
        time.sleep(1)
        self.assertEqual(self.pid(), first)
        self.assertEqual(self.remote('env -u DBUS_SESSION_BUS_ADDRESS secret-tool lookup service coop-systemd-test'), 'disposable-token')
        # Native bootstrap owns server and updater. Recovery must work while
        # that updater exists, without trying to become its process manager.
        started = json.loads(self.remote(str(self.binary) + ' app-server daemon bootstrap'))
        self.assertEqual(started['status'], 'bootstrapped')
        raw = (self.home / '.local/share/keyrings/login.keyring').read_bytes()
        self.remote('/usr/bin/python3 ' + str(self.launch) + ' lock')
        self.assertEqual(self.state(), 'True')
        self.unlock(password='\x03', success=False)
        self.assertEqual(self.state(), 'True')
        self.unlock(password='incorrect-disposable-password', success=False)
        self.assertEqual(self.state(), 'True')
        self.assertEqual((self.home / '.local/share/keyrings/login.keyring').read_bytes(), raw)
        self.unlock()
        self.assertEqual(self.state(), 'False')
        self.assertFalse((self.home / '.codex/app-server-daemon/app-server.pid').exists())
        self.assertTrue((self.home / '.codex/app-server-daemon/app-server-updater.pid').exists())
        # A desktop-style bootstrap uses its native lock while unlock takes
        # coop's independent lock. Race both against the locked collection.
        self.remote('/usr/bin/python3 ' + str(self.launch) + ' lock')
        with ThreadPoolExecutor(max_workers=2) as workers:
            bootstrapped = workers.submit(self.remote, str(self.binary) + ' app-server daemon bootstrap')
            unlocked = workers.submit(self.unlock)
            self.assertEqual(json.loads(bootstrapped.result())['status'], 'bootstrapped')
            unlocked.result()
        self.assertEqual(self.state(), 'False')
        self.remote('/usr/bin/python3 ' + str(self.launch) + ' native-credentials')
        # Reconnect via the real native bootstrap, then crash its server.
        self.remote(str(self.binary) + ' app-server daemon bootstrap')
        record = json.loads((self.home / '.codex/app-server-daemon/app-server.pid').read_text())
        os.kill(record['pid'], signal.SIGKILL)
        self.assertEqual(json.loads(self.remote(str(self.binary) + ' app-server daemon start'))['status'], 'started')
        # Unexpected keyring crash requires another unlock and retirement.
        self.remote('systemctl --user kill --signal=KILL --kill-who=main gnome-keyring-daemon.service')
        deadline = time.monotonic() + 10
        while (self.pid() in ('0', first)) and time.monotonic() < deadline:
            time.sleep(0.2)
        self.assertEqual(self.state(), 'True')
        self.unlock()
        self.assertEqual(self.remote('secret-tool lookup service coop-systemd-test'), 'disposable-token')
        self.assertFalse((self.home / '.codex/auth.json').exists())
        # Run the production terminal wrapper as the guest, using the existing
        # PTY/strace witness from #481. Removing just its override must attach.
        wrapper_source = (ROOT / 'scripts/guest/codex-account.sh').read_text().split("<<'CODEXACCOUNTEOF'\n", 1)[1].split('\nCODEXACCOUNTEOF', 1)[0]
        helper = self.root / 'codex-keyring'
        helper.write_text('#!/bin/sh\nexec /usr/bin/python3 ' + str(self.launch) + ' unlock\n')
        helper.chmod(0o755)
        wrapper = self.root / 'codex-account'
        wrapper.write_text(wrapper_source.replace('/usr/local/bin/codex-keyring', str(helper)).replace('/usr/local/bin/codex', str(self.binary)))
        wrapper.chmod(0o755)
        mutant = self.root / 'codex-account-mutant'
        mutant.write_text(wrapper.read_text().replace("-c 'cli_auth_credentials_store=\"keyring\"' ", ''))
        mutant.chmod(0o755)
        desktop = json.loads(self.remote(str(self.binary) + ' app-server daemon bootstrap'))
        terminal_probe = self.root / 'terminal-probe.py'
        terminal_probe.write_text(f'''import importlib.util,os
from pathlib import Path
spec=importlib.util.spec_from_file_location('account', {str(ROOT / 'tests/test-codex-account.py')!r})
a=importlib.util.module_from_spec(spec);spec.loader.exec_module(a)
probe=a.RealDaemonTests()
env={{**os.environ, 'TERM':'xterm-256color'}}
trace=probe.launch(Path.home(),env,Path({str(wrapper)!r}),'shared')
assert {desktop['socketPath']!r} not in trace
trace=probe.launch(Path.home(),env,Path({str(mutant)!r}),'mutant')
assert {desktop['socketPath']!r} in trace
''')
        self.remote('/usr/bin/python3 ' + str(terminal_probe))
        # Initial adoption may not trust a cached unlocked daemon after the
        # backing file changes. Unsupported formats refuse before touching it;
        # a valid prefix still needs the fresh parser and successful PAM unlock.
        ready = Path(f'/run/user/{self.uid}/coop-codex/ready.json')
        store = self.home / '.local/share/keyrings/login.keyring'
        raw = store.read_bytes()
        for invalid in (b'[keyring]\ndisplay-name=Login\n', b'unknown-format',
                        raw[:16] + b'\x01' + raw[17:]):
            with self.subTest(storage='unsupported'):
                before = self.pid()
                store.write_bytes(invalid)
                self.unlock(success=False)
                self.assertEqual(store.read_bytes(), invalid)
                self.assertEqual(self.pid(), before)
                self.assertFalse(ready.exists())
        for invalid in (raw[:20], raw[:-1] + bytes([raw[-1] ^ 0x80])):
            with self.subTest(storage='invalid-encrypted-candidate'):
                store.write_bytes(invalid)
                self.unlock(success=False)
                self.assertEqual(store.read_bytes(), invalid)
                self.assertFalse(ready.exists())
        # A repaired valid file must be loaded by a fresh daemon, even if the
        # current daemon never loaded the previous truncated collection.
        store.write_bytes(raw)
        self.unlock()
        self.assertEqual(self.remote('secret-tool lookup service coop-systemd-test'), 'disposable-token')



if __name__ == '__main__':
    unittest.main()
