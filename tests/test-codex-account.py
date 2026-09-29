#!/usr/bin/env python3
"""Wrapper regressions; set COOP_TEST_CODEX to also test a real Linux Codex CLI."""
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import fcntl
import termios

ROOT = Path(__file__).resolve().parent.parent
SOURCE = (ROOT / 'scripts/guest/codex-account.sh').read_text()
WRAPPER = SOURCE.split("<<'CODEXACCOUNTEOF'\n", 1)[1].split('\nCODEXACCOUNTEOF', 1)[0]
OVERRIDE = ['-c', 'cli_auth_credentials_store="keyring"']


def executable(path, source):
    path.write_text(source)
    path.chmod(0o755)


def isolated_env(root):
    # Do not let host credentials or launch overrides affect the fixture.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('CODEX_', 'COOP_CODEX_', 'OPENAI_', 'XDG_', 'DBUS_', 'GNOME_KEYRING'))}
    env.update(HOME=str(root), XDG_DATA_HOME=str(root / 'data'),
               XDG_RUNTIME_DIR=str(root / 'run'), TERM='xterm-256color')
    (root / '.codex').mkdir()
    (root / 'run').mkdir(mode=0o700)
    return env


def stop(process):
    # dbus-run-session may exit before Codex; waiting only for the launcher
    # races children still writing into the temporary home.
    for sig in [signal.SIGTERM, signal.SIGKILL]:
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            process.wait(timeout=5)
            return
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            process.poll()
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                process.wait(timeout=5)
                return
            time.sleep(0.05)
    raise RuntimeError(f'fixture process group {process.pid} did not exit')


class WrapperTests(unittest.TestCase):
    def test_cleanup_waits_for_launcher_children(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = root / 'child.py'
            child.write_text('''
import signal, sys, time
from pathlib import Path
def finish(*args):
    time.sleep(0.2)
    Path(sys.argv[1], 'finished').touch()
    sys.exit(0)
signal.signal(signal.SIGTERM, finish)
Path(sys.argv[1], 'ready').touch()
time.sleep(60)
''')
            parent = subprocess.Popen([sys.executable, '-c', '''
import subprocess, sys, time
subprocess.Popen([sys.executable, sys.argv[1], sys.argv[2]])
time.sleep(60)
''', str(child), str(root)], start_new_session=True)
            try:
                deadline = time.monotonic() + 5
                while not (root / 'ready').exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue((root / 'ready').exists())
            finally:
                stop(parent)
            self.assertTrue((root / 'finished').exists(), 'cleanup returned before child exit')

    def test_arguments_and_passthrough(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = isolated_env(root)
            binary = root / 'codex'
            executable(binary, '#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\nsys.exit(23)\n')
            wrapper = root / 'codex-account'
            executable(wrapper, WRAPPER.replace('/usr/local/bin/codex', str(binary)))
            for tool in ['secret-tool', 'gnome-keyring-daemon']:
                executable(root / tool, '#!/bin/sh\nexit 0\n')
            env['PATH'] = str(root) + ':' + env['PATH']
            env.update(COOP_CODEX_ACCOUNT_DBUS='1', COOP_CODEX_ACCOUNT_UNLOCKED='1')
            config = root / '.codex/config.toml'
            for keyring in [False, True]:
                config.write_text('cli_auth_credentials_store = "keyring"\n' if keyring else '')
                for args in [[], ['login', '--device-auth'], ['logout'],
                             ['--', 'a prompt with spaces; $(false)'],
                             ['-c', 'model="example"', '--model', 'explicit'],
                             ['-c', 'cli_auth_credentials_store="file"'],
                             ['--remote', 'unix:///explicit.sock']]:
                    with self.subTest(keyring=keyring, args=args):
                        result = subprocess.run([str(wrapper), *args], env=env,
                                                capture_output=True, text=True, timeout=10)
                        self.assertEqual(result.returncode, 23, result.stderr)
                        self.assertEqual(json.loads(result.stdout), (OVERRIDE if keyring else []) + args)
            # Exercise the actual provisioned yolo command, including its bypass flag.
            lima = (ROOT / 'src/lima.rs').read_text()
            yolo = lima.split("cat > /usr/local/bin/codex-yolo <<'YOLOEOF'\n", 1)[1].split('\nYOLOEOF', 1)[0]
            shortcut = root / 'codex-yolo'
            executable(shortcut, yolo.replace('/usr/local/bin/codex-account', str(wrapper)))
            result = subprocess.run([str(shortcut), 'hello world'], env=env,
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertEqual(json.loads(result.stdout), OVERRIDE +
                             ['--dangerously-bypass-approvals-and-sandbox', 'hello world'])


@unittest.skipUnless(os.environ.get('COOP_TEST_CODEX'), 'set COOP_TEST_CODEX for real daemon regression')
class RealDaemonTests(unittest.TestCase):
    def test_terminal_avoids_daemon_with_unusable_keyring(self):
        binary = str(Path(os.environ['COOP_TEST_CODEX']).resolve())
        for tool in ['dbus-run-session', 'gnome-keyring-daemon', 'secret-tool', 'strace']:
            self.assertIsNotNone(shutil.which(tool), f'missing prerequisite: {tool}')
        print(subprocess.check_output([binary, '--version'], text=True).strip(), flush=True)
        with tempfile.TemporaryDirectory(prefix='c481-') as directory:
            root = Path(directory)
            env = isolated_env(root)
            # Disable unrelated plugin downloads without a CLI override, which would
            # itself prevent the control launch from reusing the daemon.
            (root / '.codex/config.toml').write_text(
                'cli_auth_credentials_store = "keyring"\n[features]\nplugins = false\n')
            wrapper = root / 'codex-account'
            executable(wrapper, WRAPPER.replace('/usr/local/bin/codex', binary))
            socket = root / '.codex/app-server-control/app-server-control.sock'
            # The existing server gets a different bus and an empty keyring directory.
            daemon_env = {**env, 'XDG_DATA_HOME': str(root / 'desktop-data')}
            with (root / 'daemon.log').open('w') as log:
                daemon = subprocess.Popen(['dbus-run-session', '--', 'bash', '-c', '''
                    printf '%s' "$DBUS_SESSION_BUS_ADDRESS" > "$HOME/desktop-bus"
                    exec "$1" app-server --listen unix://
                ''', 'bash', binary], env=daemon_env, stdout=log, stderr=log, start_new_session=True)
                try:
                    deadline = time.monotonic() + 20
                    while not socket.exists() and daemon.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.05)
                    self.assertTrue(socket.exists(), (root / 'daemon.log').read_text())
                    bad_env = {**daemon_env, 'DBUS_SESSION_BUS_ADDRESS': (root / 'desktop-bus').read_text()}
                    probe = subprocess.run(['timeout', '5', 'secret-tool', 'store', '--label=probe',
                                            'service', 'coop-regression'], input='disposable', text=True,
                                           capture_output=True, env=bad_env, timeout=10)
                    self.assertNotEqual(probe.returncode, 0, 'desktop keyring unexpectedly writable')
                    # Positive witness: removing only the fix must connect to that daemon.
                    mutant = root / 'codex-account-mutant'
                    executable(mutant, wrapper.read_text().replace(
                        "-c 'cli_auth_credentials_store=\"keyring\"' ", ''))
                    old_trace = self.launch(root, env, mutant, 'old')
                    self.assertIn(str(socket), old_trace, 'control never reused the existing daemon')
                    new_trace = self.launch(root, env, wrapper, 'new')
                    self.assertNotIn(str(socket), new_trace, 'terminal reused the desktop daemon')
                    self.assertIsNone(daemon.poll(), 'test must leave the existing server running')
                    self.assertFalse((root / '.codex/auth.json').exists())
                finally:
                    stop(daemon)

    def launch(self, root, env, wrapper, label):
        trace = root / (label + '.trace')
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
        process = subprocess.Popen(['strace', '-f', '-e', 'connect', '-s', '256', '-o', str(trace),
                                    str(wrapper)], env=env, cwd=root, stdin=slave, stdout=slave,
                                   stderr=slave, start_new_session=True)
        os.close(slave)
        output = b''
        deadline = time.monotonic() + 30
        try:
            while time.monotonic() < deadline and process.poll() is None:
                if not select.select([master], [], [], 0.1)[0]:
                    continue
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                output += chunk
                if b'\x1b[6n' in chunk:
                    os.write(master, b'\x1b[1;1R')
                if b'keyring password: ' in chunk:
                    os.write(master, b'coop-test-password\n')
                if b'Sign in with ChatGPT' in output:
                    break
            self.assertIn(b'Sign in with ChatGPT', output, output.decode(errors='replace'))
            return trace.read_text()
        finally:
            stop(process)
            os.close(master)


if __name__ == '__main__':
    unittest.main()
