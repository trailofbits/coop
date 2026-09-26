#!/usr/bin/env python3
"""Real-process discovery gate for #480; no real credentials or desktop automation.

Run with COOP_TEST_CODEX=/absolute/path/to/codex python3
tests/test-codex-desktop-prototype.py -v. Requires Linux, python3-dbus, python3-websocket,
dbus-daemon, gnome-keyring-daemon, and secret-tool. All state is temporary.
The native installer package is referenced, never modified or downloaded.
"""
import json
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import unittest


class Session:
    """Own a real private bus and foreground keyring independently of clients."""

    def __init__(self, root, name, data):
        self.run = root / name
        self.run.mkdir(mode=0o700)
        self.env = {
            'PATH': '/usr/local/bin:/usr/bin:/bin',
            'HOME': str(root),
            'LANG': 'C.UTF-8',
            'XDG_DATA_HOME': str(data),
            'XDG_RUNTIME_DIR': str(self.run),
            'DBUS_SESSION_BUS_ADDRESS': 'unix:path=' + str(self.run / 'bus'),
            'GNOME_KEYRING_CONTROL': str(self.run / 'control'),
        }
        self.children = []
        self.keyring = None
        self.bus = None

    def open(self):
        import dbus
        # No standard service directories: a probe or graphical prompter must
        # not auto-activate a competing Secret Service on this owned bus.
        config = self.run / 'bus.conf'
        config.write_text('''<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
''')
        child = subprocess.Popen(
            ['dbus-daemon', '--config-file=' + str(config), '--nofork',
             '--address=' + self.env['DBUS_SESSION_BUS_ADDRESS']],
            env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.children.append(child)
        deadline = time.monotonic() + 5
        while not (self.run / 'bus').exists():
            if child.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError('private D-Bus startup failed')
            time.sleep(0.02)
        self.bus = dbus.bus.BusConnection(self.env['DBUS_SESSION_BUS_ADDRESS'])
        return self

    def unlock(self, password):
        if self.keyring is not None:
            raise RuntimeError('replace the owned keyring before unlocking again')
        args = ['gnome-keyring-daemon', '--unlock', '--components=secrets',
                '--control-directory=' + self.env['GNOME_KEYRING_CONTROL']]
        self.keyring = subprocess.Popen(
            [*args, '--foreground'], env=self.env, stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.children.append(self.keyring)
        self.keyring.stdin.write(password.encode())
        self.keyring.stdin.close()
        deadline = time.monotonic() + 5
        while self.state() != 'unlocked':
            if self.keyring.poll() is not None:
                raise RuntimeError('keyring did not expose its login collection')
            if time.monotonic() >= deadline:
                if self.state() == 'locked':
                    return
                raise RuntimeError('keyring did not expose its login collection')
            time.sleep(0.02)

    def keyring_owner_pid(self):
        bus = self.bus.get_object('org.freedesktop.DBus', '/org/freedesktop/DBus',
                                  introspect=False)
        return int(bus.GetConnectionUnixProcessID('org.freedesktop.secrets',
                   dbus_interface='org.freedesktop.DBus', timeout=2))

    def state(self):
        import dbus
        if not self.bus.name_has_owner('org.freedesktop.secrets'):
            return 'unavailable'
        service = self.bus.get_object('org.freedesktop.secrets',
                                      '/org/freedesktop/secrets', introspect=False)
        collection = service.ReadAlias('default',
                                       dbus_interface='org.freedesktop.Secret.Service',
                                       timeout=2)
        if collection == '/':
            return 'missing'
        if collection != '/org/freedesktop/secrets/collection/login':
            return 'unexpected-default'
        try:
            obj = self.bus.get_object('org.freedesktop.secrets', collection,
                                      introspect=False)
            locked = obj.Get('org.freedesktop.Secret.Collection', 'Locked',
                             dbus_interface='org.freedesktop.DBus.Properties', timeout=2)
        except dbus.DBusException as error:
            if error.get_dbus_name() == 'org.freedesktop.DBus.Error.UnknownMethod':
                return 'missing'
            raise
        return 'locked' if locked else 'unlocked'

    def lock(self):
        import dbus
        service = self.bus.get_object('org.freedesktop.secrets',
                                      '/org/freedesktop/secrets', introspect=False)
        service.Lock(dbus.Array(['/org/freedesktop/secrets/collection/login'], signature='o'),
                     dbus_interface='org.freedesktop.Secret.Service', timeout=2)

    def restart_keyring(self, password):
        # A production owner must first retire every associated app-server.
        # These fixtures call this only when no app-server is running.
        self.keyring.terminate()
        self.keyring.wait(timeout=5)
        self.keyring = None
        self.unlock(password)

    def secret(self, action, value=None, account='disposable'):
        args = ['secret-tool', action]
        if action == 'store':
            args.append('--label=coop desktop disposable probe')
        result = subprocess.run(
            [*args, 'service', 'coop-desktop-prototype', 'account', account],
            env=self.env, input=value, text=True, capture_output=True, timeout=5)
        if result.returncode == 0:
            return result.stdout.strip()
        if action == 'lookup' and result.returncode == 1 and not result.stderr:
            # secret-tool also exits nonzero when its bus or collection fails.
            # Prove absence against the live service before treating it as logout.
            if self.state() != 'unlocked':
                raise RuntimeError('lookup failed while the collection was not unlocked')
            service = self.bus.get_object('org.freedesktop.secrets',
                                          '/org/freedesktop/secrets', introspect=False)
            unlocked, locked = service.SearchItems(
                {'service': 'coop-desktop-prototype', 'account': account},
                dbus_interface='org.freedesktop.Secret.Service', timeout=2)
            if not unlocked and not locked:
                return None
        raise RuntimeError(f'disposable {action} failed ({result.returncode}): {result.stderr}')

    def close(self):
        if self.bus is not None:
            self.bus.close()
        for child in reversed(self.children):
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)


@unittest.skipUnless(os.environ.get('COOP_TEST_CODEX'),
                     'set COOP_TEST_CODEX to run the real-process prototype')
class DesktopPrototypeTests(unittest.TestCase):
    def setUp(self):
        for tool in ['dbus-daemon', 'gnome-keyring-daemon', 'secret-tool']:
            self.assertIsNotNone(shutil.which(tool), f'missing prerequisite: {tool}')
        self.temp = tempfile.TemporaryDirectory(prefix='c480-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = Path(os.environ['COOP_TEST_CODEX']).resolve()
        self.home = self.root / '.codex'
        self.home.mkdir()
        (self.home / 'config.toml').write_text(
            'cli_auth_credentials_store = "keyring"\n[features]\nplugins = false\n')
        # Native daemon management requires the installer layout. Reuse the
        # whole package so bundled components resolve beside the executable.
        self.assertEqual(self.binary.parent.name, 'bin', 'use a native Codex install')
        standalone = self.home / 'packages/standalone'
        standalone.mkdir(parents=True)
        (standalone / 'current').symlink_to(self.binary.parent.parent,
                                          target_is_directory=True)

    def session(self, name, data=None):
        session = Session(self.root, name, data or self.root / 'data')
        self.addCleanup(session.close)
        return session.open()

    def native(self, session, operation):
        result = subprocess.run(
            [str(self.binary), 'app-server', 'daemon', operation],
            env=session.env, cwd=self.root, text=True, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def stop_native(self, session):
        record = self.home / 'app-server-daemon/app-server.pid'
        process_fd = None
        if record.exists():
            try:
                process_fd = os.pidfd_open(json.loads(record.read_text())['pid'])
            except ProcessLookupError:
                pass
        try:
            self.native(session, 'stop')
            self.assertFalse(record.exists(), 'native stop left its process record')
            if process_fd is not None:
                self.assertTrue(select.select([process_fd], [], [], 5)[0],
                                'native stop removed state but left the process alive')
        finally:
            if process_fd is not None:
                if not select.select([process_fd], [], [], 0)[0]:
                    signal.pidfd_send_signal(process_fd, signal.SIGKILL)
                    select.select([process_fd], [], [], 5)
                os.close(process_fd)

    def rpc(self, path, method, params, expect_error=False):
        import websocket
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(path)
            # The native control socket carries WebSocket frames, not the
            # newline JSON used by `app-server --stdio`.
            client = websocket.create_connection('ws://localhost', socket=connection, timeout=5)
            try:
                def send(message):
                    client.send(json.dumps(message))

                def response(request_id):
                    for _ in range(100):
                        payload = client.recv()
                        self.assertTrue(payload, 'app-server closed the connection')
                        message = json.loads(payload)
                        if message.get('id') == request_id:
                            if expect_error and request_id == 1:
                                self.assertIn('error', message)
                                return message['error']
                            self.assertNotIn('error', message)
                            return message['result']
                    self.fail('app-server sent too many notifications before the response')

                send({'id': 0, 'method': 'initialize', 'params': {
                    'clientInfo': {'name': 'coop_desktop_probe', 'version': '0.1.0'}}})
                response(0)
                send({'method': 'initialized'})
                send({'id': 1, 'method': method, 'params': params})
                return response(1)
            finally:
                client.close()

    def test_native_start_reconnect_and_crash(self):
        session = self.session('desktop')
        self.assertEqual(session.state(), 'unavailable')
        session.unlock('disposable-prototype-password')
        self.assertEqual(session.state(), 'unlocked')
        session.secret('store', 'disposable-token')
        self.assertEqual(session.secret('lookup'), 'disposable-token')
        session.secret('clear')

        # Register cleanup before startup: failed readiness may leave a child.
        self.addCleanup(self.stop_native, session)
        first = self.native(session, 'start')
        self.assertEqual(first['status'], 'started')
        self.rpc(first['socketPath'], 'account/login/start', {
            'type': 'apiKey', 'apiKey': 'sk-coop-disposable-prototype-not-a-real-key'})
        self.assertFalse((self.home / 'auth.json').exists(),
                         'login fell back to plaintext credential storage')
        self.assertEqual(self.rpc(first['socketPath'], 'account/read', {
            'refreshToken': False})['account']['type'], 'apiKey')
        record_path = self.home / 'app-server-daemon/app-server.pid'
        first_record = json.loads(record_path.read_text())
        # The launching CLI has exited. Check only the non-secret bus variable,
        # never print or copy the server's entire environment.
        environment = Path(f'/proc/{first_record["pid"]}/environ').read_bytes().split(b'\0')
        bus_variable = ('DBUS_SESSION_BUS_ADDRESS=' +
                        session.env['DBUS_SESSION_BUS_ADDRESS']).encode()
        self.assertIn(bus_variable, environment)
        self.assertEqual(self.native(session, 'start')['status'], 'alreadyRunning')
        self.assertEqual(json.loads(record_path.read_text()), first_record)

        os.kill(first_record['pid'], signal.SIGKILL)
        deadline = time.monotonic() + 5
        while Path(f'/proc/{first_record["pid"]}').exists() and time.monotonic() < deadline:
            # A zombie is no longer a running socket owner.
            stat = Path(f'/proc/{first_record["pid"]}/stat')
            try:
                if stat.read_text().split(') ', 1)[1].startswith('Z'):
                    break
            except FileNotFoundError:
                break
            time.sleep(0.02)
        self.assertEqual(self.native(session, 'start')['status'], 'started')
        replacement = json.loads(record_path.read_text())
        self.assertNotEqual(replacement['pid'], first_record['pid'])
        environment = Path(f'/proc/{replacement["pid"]}/environ').read_bytes().split(b'\0')
        self.assertIn(bus_variable, environment)
        self.assertEqual(self.rpc(first['socketPath'], 'account/read', {
            'refreshToken': False})['account']['type'], 'apiKey')
        self.assertFalse((self.home / 'auth.json').exists())
        self.rpc(first['socketPath'], 'account/logout', None)
        self.assertIsNone(self.rpc(first['socketPath'], 'account/read', {
            'refreshToken': False})['account'])
        self.assertFalse((self.home / 'auth.json').exists())

    def test_lock_wrong_password_and_owned_replacement(self):
        session = self.session('desktop')
        session.unlock('disposable-prototype-password')
        session.secret('store', 'disposable-token')
        session.lock()
        self.assertEqual(session.state(), 'locked')
        with self.assertRaisesRegex(RuntimeError, 'replace the owned keyring'):
            session.unlock('disposable-prototype-password')
        session.restart_keyring('incorrect-disposable-password')
        self.assertEqual(session.state(), 'locked')
        session.restart_keyring('disposable-prototype-password')
        self.assertEqual(session.state(), 'unlocked')
        self.assertEqual(session.keyring_owner_pid(), session.keyring.pid)
        self.assertEqual(session.secret('lookup'), 'disposable-token')
        session.secret('clear')

    def test_duplicate_native_starts(self):
        session = self.session('desktop')
        session.unlock('disposable-prototype-password')
        self.addCleanup(self.stop_native, session)
        with ThreadPoolExecutor(max_workers=2) as workers:
            results = list(workers.map(lambda _: self.native(session, 'start'), range(2)))
        self.assertCountEqual([result['status'] for result in results],
                              ['started', 'alreadyRunning'])
        self.assertEqual(results[0]['socketPath'], results[1]['socketPath'])

    def test_native_reuses_existing_server_on_a_different_bus(self):
        first = self.session('first')
        first.unlock('disposable-prototype-password')
        second = self.session('second', self.root / 'second-data')
        second.unlock('another-disposable-password')
        self.addCleanup(self.stop_native, first)
        self.native(first, 'start')
        self.assertEqual(self.native(second, 'start')['status'], 'alreadyRunning')
        record = json.loads((self.home / 'app-server-daemon/app-server.pid').read_text())
        environment = Path(f'/proc/{record["pid"]}/environ').read_bytes().split(b'\0')
        self.assertIn(('DBUS_SESSION_BUS_ADDRESS=' +
                       first.env['DBUS_SESSION_BUS_ADDRESS']).encode(), environment)
        self.assertNotIn(('DBUS_SESSION_BUS_ADDRESS=' +
                          second.env['DBUS_SESSION_BUS_ADDRESS']).encode(), environment)

    def test_native_readiness_does_not_prove_keyring_unlocked(self):
        session = self.session('desktop')
        session.unlock('disposable-prototype-password')
        self.addCleanup(self.stop_native, session)
        first = self.native(session, 'start')
        self.rpc(first['socketPath'], 'account/login/start', {
            'type': 'apiKey', 'apiKey': 'sk-coop-disposable-prototype-not-a-real-key'})
        self.stop_native(session)
        session.lock()
        self.assertEqual(self.native(session, 'start')['status'], 'started')
        self.assertEqual(session.state(), 'locked')
        error = self.rpc(first['socketPath'], 'account/login/start', {
            'type': 'apiKey', 'apiKey': 'sk-coop-another-disposable-invalid-key'}, expect_error=True)
        self.assertIn('keyring', error['message'])
        self.assertFalse((self.home / 'auth.json').exists())
        self.stop_native(session)
        session.restart_keyring('disposable-prototype-password')
        self.assertEqual(self.native(session, 'start')['status'], 'started')

    def test_failed_native_readiness_requires_explicit_cleanup(self):
        session = self.session('desktop')
        session.unlock('disposable-prototype-password')
        # Deliberately break server readiness while retaining the real native
        # lifecycle manager. Replace only the temporary package symlink.
        current = self.home / 'packages/standalone/current'
        current.unlink()
        (current / 'bin').mkdir(parents=True)
        stalled = current / 'bin/codex'
        stalled.write_text('''#!/bin/sh
if [ "$1" = "--version" ]; then
    echo 'codex-cli 0.154.0'
    exit 0
fi
exec sleep 120
''')
        stalled.chmod(0o755)
        self.addCleanup(self.stop_native, session)
        result = subprocess.run(
            [str(self.binary), 'app-server', 'daemon', 'start'], env=session.env,
            cwd=self.root, text=True, capture_output=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('did not become ready', result.stderr)
        record_path = self.home / 'app-server-daemon/app-server.pid'
        self.assertTrue(record_path.exists(), 'native startup now rolls back; revisit ownership')
        self.stop_native(session)

    def test_owner_restart_requires_password(self):
        session = self.session('first-boot')
        session.unlock('disposable-prototype-password')
        session.secret('store', 'persisted-disposable-token')
        session.close()
        session.bus = None
        restarted = self.session('second-boot')
        self.assertEqual(restarted.state(), 'unavailable')
        restarted.unlock('incorrect-disposable-password')
        self.assertEqual(restarted.state(), 'locked')
        restarted.restart_keyring('disposable-prototype-password')
        self.assertEqual(restarted.state(), 'unlocked')
        self.assertEqual(restarted.secret('lookup'), 'persisted-disposable-token')
        restarted.secret('clear')

    def test_missing_default_is_distinct_from_locked(self):
        import dbus
        session = self.session('desktop')
        session.unlock('disposable-prototype-password')
        service = session.bus.get_object('org.freedesktop.secrets',
                                          '/org/freedesktop/secrets', introspect=False)
        service.SetAlias('default', dbus.ObjectPath('/'),
                         dbus_interface='org.freedesktop.Secret.Service', timeout=2)
        self.assertEqual(session.state(), 'missing')
        service.SetAlias('default', dbus.ObjectPath('/org/freedesktop/secrets/collection/login'),
                         dbus_interface='org.freedesktop.Secret.Service', timeout=2)
        session.lock()
        self.assertEqual(session.state(), 'locked')

    def test_shared_files_have_stale_refresh_and_logout(self):
        # Characterization of an implementation blocker, not desired semantics.
        # Separate buses alone cannot provide coherent shared credentials.
        desktop = self.session('desktop')
        desktop.unlock('disposable-prototype-password')
        desktop.secret('store', 'version-one')
        terminal = self.session('terminal')
        terminal.unlock('disposable-prototype-password')
        self.assertEqual(terminal.secret('lookup'), 'version-one')
        desktop.secret('store', 'version-two')
        self.assertEqual(terminal.secret('lookup'), 'version-one')
        desktop.secret('clear')
        self.assertEqual(terminal.secret('lookup'), 'version-one')
        # An unrelated write must not resurrect the deleted account. This
        # distinguishes stale whole-file state from deliberately logging in.
        terminal.secret('store', 'unrelated-value', account='unrelated')
        fresh = self.session('fresh')
        fresh.unlock('disposable-prototype-password')
        self.assertEqual(fresh.secret('lookup'), 'version-one')
        fresh.secret('clear')

    def test_one_service_has_coherent_independent_clients(self):
        session = self.session('shared')
        session.unlock('disposable-prototype-password')
        owner = session.keyring_owner_pid()
        # Each secret-tool call creates a separate process and D-Bus client.
        # Standard runtime-directory discovery works without borrowed env.
        del session.env['DBUS_SESSION_BUS_ADDRESS']
        del session.env['GNOME_KEYRING_CONTROL']
        session.secret('store', 'version-one')
        self.assertEqual(session.secret('lookup'), 'version-one')
        session.secret('store', 'version-two')
        self.assertEqual(session.secret('lookup'), 'version-two')
        session.secret('clear')
        session.secret('store', 'unrelated-value', account='unrelated')
        self.assertIsNone(session.secret('lookup'))
        self.assertEqual(session.keyring_owner_pid(), owner)
        session.close()
        session.bus = None
        fresh = self.session('fresh')
        fresh.unlock('disposable-prototype-password')
        self.assertIsNone(fresh.secret('lookup'), 'unrelated write resurrected deleted credential')
        self.assertEqual(fresh.secret('lookup', account='unrelated'), 'unrelated-value')

    def test_lookup_failure_is_not_credential_absence(self):
        session = self.session('shared')
        session.unlock('disposable-prototype-password')
        self.assertIsNone(session.secret('lookup'))
        session.secret('store', 'disposable-token')
        self.assertEqual(session.secret('lookup'), 'disposable-token')
        session.env['DBUS_SESSION_BUS_ADDRESS'] = 'unix:path=' + str(self.root / 'absent-bus')
        with self.assertRaisesRegex(RuntimeError, 'disposable lookup failed'):
            session.secret('lookup')

    def test_unlocked_writable_login_does_not_prove_encryption(self):
        # Minimal GNOME 46.1 empty-password keyring, containing no credentials.
        # Ordinary clients can unlock it without a password when they write.
        data = self.root / 'data/keyrings'
        data.mkdir(parents=True, mode=0o700)
        keyring_file = data / 'login.keyring'
        keyring_file.write_text(
            '[keyring]\ndisplay-name=Login\nctime=1\nmtime=0\n'
            'lock-on-idle=false\nlock-after=false\n')
        keyring_file.chmod(0o600)
        (data / 'default').write_text('login')
        session = self.session('shared')
        session.keyring = subprocess.Popen(
            ['gnome-keyring-daemon', '--foreground', '--components=secrets',
             '--control-directory=' + session.env['GNOME_KEYRING_CONTROL']],
            env=session.env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        session.children.append(session.keyring)
        deadline = time.monotonic() + 5
        while session.state() == 'unavailable':
            self.assertIsNone(session.keyring.poll())
            self.assertLess(time.monotonic(), deadline, 'keyring service startup timed out')
            time.sleep(0.02)
        session.secret('store', 'disposable-plaintext-witness')
        self.assertEqual(session.state(), 'unlocked')
        self.assertEqual(session.secret('lookup'), 'disposable-plaintext-witness')
        self.assertIn(b'disposable-plaintext-witness', keyring_file.read_bytes())

    def test_codex_shared_store_and_independent_auth_cache(self):
        session = self.session('shared')
        session.unlock('disposable-prototype-password')
        del session.env['DBUS_SESSION_BUS_ADDRESS']
        del session.env['GNOME_KEYRING_CONTROL']
        login = subprocess.run(
            [str(self.binary), '-c', 'cli_auth_credentials_store="keyring"',
             'login', '--with-api-key'], input='sk-disposable-invalid-key',
            env=session.env, cwd=self.root, text=True, capture_output=True, timeout=10)
        self.assertEqual(login.returncode, 0, login.stderr)
        self.addCleanup(self.stop_native, session)
        desktop = self.native(session, 'start')
        params = {'refreshToken': False}
        self.assertEqual(self.rpc(desktop['socketPath'], 'account/read', params)
                         ['account']['type'], 'apiKey')
        logout = subprocess.run(
            [str(self.binary), '-c', 'cli_auth_credentials_store="keyring"', 'logout'],
            env=session.env, cwd=self.root, text=True, capture_output=True, timeout=10)
        self.assertEqual(logout.returncode, 0, logout.stderr)
        # Shared storage does not invalidate another AuthManager's RAM cache.
        # This is Codex API-key cache behavior, not a ChatGPT revocation test.
        self.assertEqual(self.rpc(desktop['socketPath'], 'account/read', params)
                         ['account']['type'], 'apiKey')
        self.native(session, 'restart')
        self.assertIsNone(self.rpc(desktop['socketPath'], 'account/read', params)['account'])
        self.assertFalse((self.home / 'auth.json').exists())

    @unittest.skip('historical private-bus fixture; production wrapper is covered by test-codex-keyring-systemd.py')
    def test_terminal_server_isolation_with_one_shared_keyring(self):
        import importlib.util
        self.assertIsNotNone(shutil.which('strace'), 'this probe requires strace')
        path = Path(__file__).with_name('test-codex-account.py')
        spec = importlib.util.spec_from_file_location('account_fixture', path)
        account = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(account)
        session = self.session('shared')
        session.unlock('disposable-prototype-password')
        self.addCleanup(self.stop_native, session)
        desktop = self.native(session, 'start')
        wrapper = self.root / 'codex-account'
        account.executable(wrapper, account.WRAPPER.replace('/usr/local/bin/codex', str(self.binary)))
        env = {**session.env, 'COOP_CODEX_ACCOUNT_DBUS': '1',
               'COOP_CODEX_ACCOUNT_UNLOCKED': '1', 'TERM': 'xterm-256color'}
        probe = account.RealDaemonTests()
        trace = probe.launch(self.root, env, wrapper, 'shared-keyring')
        self.assertNotIn(desktop['socketPath'], trace)
        # Positive witness: removing only #481's override attaches to desktop.
        mutant = self.root / 'codex-account-mutant'
        account.executable(mutant, wrapper.read_text().replace(
            "-c 'cli_auth_credentials_store=\"keyring\"' ", ''))
        trace = probe.launch(self.root, env, mutant, 'shared-keyring-mutant')
        self.assertIn(desktop['socketPath'], trace)
        self.assertEqual(session.keyring_owner_pid(), session.keyring.pid)

    @unittest.skipUnless(os.environ.get('COOP_TEST_PAM_PROBE'),
                         'compile codex-keyring-pam-probe.c and set COOP_TEST_PAM_PROBE')
    def test_pam_creates_and_unlocks_existing_service(self):
        import pwd
        session = self.session('shared')
        session.keyring = subprocess.Popen(
            ['gnome-keyring-daemon', '--foreground', '--components=secrets',
             '--control-directory=' + session.env['GNOME_KEYRING_CONTROL']],
            env=session.env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        session.children.append(session.keyring)
        deadline = time.monotonic() + 5
        while not session.bus.name_has_owner('org.freedesktop.secrets'):
            self.assertIsNone(session.keyring.poll())
            self.assertLess(time.monotonic(), deadline, 'keyring service startup timed out')
            time.sleep(0.02)
        self.assertEqual(session.state(), 'missing')
        owner = session.keyring_owner_pid()
        config = self.root / 'pam'
        config.mkdir()
        module = os.environ.get('COOP_TEST_PAM_MODULE', 'pam_gnome_keyring.so')
        (config / 'coop-keyring').write_text(
            'auth required pam_exec.so expose_authtok /usr/bin/true\n'
            f'auth required {module}\n')
        command = [os.environ['COOP_TEST_PAM_PROBE'], pwd.getpwuid(os.getuid()).pw_name,
                   str(config), session.env['GNOME_KEYRING_CONTROL']]

        def unlock(password):
            result = subprocess.run(command, input=password + '\n', env=session.env,
                                    text=True, capture_output=True, timeout=5)
            self.assertNotIn(password, result.stdout + result.stderr)
            self.assertEqual(session.keyring_owner_pid(), owner)
            return result.returncode

        self.assertEqual(unlock('disposable-password'), 0)
        self.assertEqual(session.state(), 'unlocked')
        session.secret('store', 'disposable-token')
        session.lock()
        self.assertNotEqual(unlock('wrong-password'), 0)
        self.assertEqual(session.state(), 'locked')
        self.assertEqual(unlock('disposable-password'), 0)
        self.assertEqual(session.state(), 'unlocked')
        self.assertEqual(session.secret('lookup'), 'disposable-token')
        session.secret('clear')


if __name__ == '__main__':
    unittest.main()
