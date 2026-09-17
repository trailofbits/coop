#!/usr/bin/python3
"""Production readiness decision tests. Real PAM/SSH tests live in the systemd suite."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('keyring', ROOT / 'scripts/guest/codex-keyring.py')
k = importlib.util.module_from_spec(spec)
spec.loader.exec_module(k)


class StorageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.directory = self.home / '.local/share/keyrings'
        self.directory.mkdir(parents=True)
        self.store = self.directory / 'login.keyring'

    def test_absent_is_only_new_store(self):
        self.assertIsNone(k.storage(self.home))
        for content in (b'', b'[keyring]\ndisplay-name=Login\n', b'unknown',
                        k.PREFIX[:-1] + b'\1'):
            with self.subTest(content=content):
                self.store.write_bytes(content)
                with self.assertRaises(k.Error) as raised:
                    k.storage(self.home)
                self.assertEqual(raised.exception.kind, k.Failure.FORMAT)
                self.assertEqual(self.store.read_bytes(), content)

    def test_prefix_is_candidate_not_integrity(self):
        self.store.write_bytes(k.PREFIX)
        self.assertIsNotNone(k.storage(self.home))
        self.assertEqual(self.store.read_bytes(), k.PREFIX)

    def test_alias_conflict_and_other_stores(self):
        (self.directory / 'default').write_text('session')
        with self.assertRaises(k.Error) as error:
            k.storage(self.home)
        self.assertEqual(error.exception.kind, k.Failure.ALIAS)
        (self.directory / 'default').unlink()
        (self.directory / 'other.keyring').write_bytes(k.PREFIX)
        with self.assertRaises(k.Error) as error:
            k.storage(self.home)
        self.assertEqual(error.exception.kind, k.Failure.CONFLICT)

    def test_symlink_is_never_new_storage(self):
        self.store.symlink_to(self.home / 'absent')
        with self.assertRaises(k.Error):
            k.storage(self.home)

    def test_policy_rejects_overrides_and_plaintext(self):
        config = self.home / '.codex/config.toml'
        config.parent.mkdir()
        config.write_text('cli_auth_credentials_store = "keyring"\n')
        with patch.dict(os.environ, {}, clear=True):
            k.policy(self.home)
            with patch.dict(os.environ, {'CODEX_HOME': str(self.home / 'elsewhere')}):
                with self.assertRaises(k.Error):
                    k.policy(self.home)
            (config.parent / 'auth.json').write_text('{}')
            with self.assertRaises(k.Error):
                k.policy(self.home)


class FakeKeyring:
    locked = False
    generation_value = ['boot', 'bus', ':1.2']
    events = []
    collection_error = None
    probe_error = None
    generation_error = False

    def __init__(self, runtime):
        self.bus = self

    def close(self):
        pass

    def generation(self, boot):
        if self.generation_error and 'probe' in self.events:
            raise k.Error(k.Failure.GENERATION)
        return self.generation_value

    def collection(self, candidate):
        self.events.append('collection')
        if self.collection_error:
            raise k.Error(self.collection_error)
        return self.locked

    def probe(self):
        self.events.append('probe')
        if self.probe_error:
            raise k.Error(self.probe_error)


class OperationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.marker = self.root / 'install-boot'
        self.marker.write_text('old-boot')
        self.state = self.root / 'coop-codex/ready.json'
        FakeKeyring.events = []
        FakeKeyring.locked = False
        FakeKeyring.collection_error = None
        FakeKeyring.probe_error = None
        FakeKeyring.generation_error = False
        self.patches = [
            patch.object(k, 'MIGRATION', self.marker),
            patch.object(k, 'policy'),
            patch.object(k, 'storage', return_value='encrypted-candidate'),
            patch.object(k, 'Keyring', FakeKeyring),
            patch.object(k, 'run', return_value=subprocess.CompletedProcess([], 0, b'gnome-keyring-daemon: 46.1\n', b'')),
            patch.object(k, 'systemctl', side_effect=lambda op: FakeKeyring.events.append(op)),
            patch.object(k, 'retire_server', side_effect=lambda home: FakeKeyring.events.append('retire')),
            patch.object(k.sys.stdin, 'isatty', return_value=True),
        ]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)

    def invoke(self):
        k.unlock(self.root, self.root, 'boot')

    def test_first_adoption_retires_before_fresh_service_and_after_probe(self):
        self.invoke()
        events = FakeKeyring.events
        self.assertLess(events.index('retire'), events.index('restart'))
        self.assertLess(events.index('restart'), events.index('probe'))
        self.assertEqual(events.count('retire'), 2)
        self.assertEqual(json.loads(self.state.read_text()), FakeKeyring.generation_value)

    def test_repeated_operation_does_not_churn(self):
        self.invoke()
        FakeKeyring.events.clear()
        self.invoke()
        self.assertNotIn('restart', FakeKeyring.events)
        self.assertNotIn('retire', FakeKeyring.events)
        self.assertIn('probe', FakeKeyring.events)

    def test_changed_owner_even_already_unlocked_requires_recovery(self):
        self.invoke()
        self.state.write_text(json.dumps(['boot', 'bus', ':1.old']))
        FakeKeyring.events.clear()
        self.invoke()
        self.assertIn('restart', FakeKeyring.events)
        self.assertEqual(FakeKeyring.events.count('retire'), 2)

    def test_locked_transition_requires_pam_and_recovery(self):
        self.invoke()
        FakeKeyring.events.clear()
        FakeKeyring.locked = True
        def pam(creating):
            self.assertFalse(creating)
            FakeKeyring.locked = False
            FakeKeyring.events.append('pam')
        with patch.object(k, 'pam_unlock', side_effect=pam):
            self.invoke()
        self.assertIn('pam', FakeKeyring.events)
        self.assertEqual(FakeKeyring.events.count('retire'), 1)

    def test_migration_barrier_precedes_service_or_probe(self):
        self.marker.write_text('boot')
        with self.assertRaises(k.Error) as error:
            self.invoke()
        self.assertEqual(error.exception.kind, k.Failure.MIGRATION)
        self.assertEqual(FakeKeyring.events, [])

    def test_invalid_storage_requires_fresh_parser_and_refuses_before_pam(self):
        FakeKeyring.collection_error = k.Failure.INVALID
        with patch.object(k, 'pam_unlock') as pam:
            with self.assertRaises(k.Error):
                self.invoke()
            pam.assert_not_called()
        self.assertIn('restart', FakeKeyring.events)
        self.assertFalse(self.state.exists())

    def test_failure_cannot_publish_or_preserve_success(self):
        for failure in (k.Failure.WRITE, k.Failure.CLEANUP):
            with self.subTest(failure=failure):
                FakeKeyring.probe_error = None
                self.invoke()
                FakeKeyring.probe_error = failure
                with self.assertRaises(k.Error):
                    self.invoke()
                self.assertFalse(self.state.exists())

    def test_service_change_during_probe_invalidates_record(self):
        FakeKeyring.generation_error = True
        with self.assertRaises(k.Error) as error:
            self.invoke()
        self.assertEqual(error.exception.kind, k.Failure.GENERATION)
        self.assertFalse(self.state.exists())

    def test_recovery_failure_leaves_retry_eligible(self):
        with patch.object(k, 'retire_server', side_effect=k.Error(k.Failure.NATIVE_BUSY)):
            with self.assertRaises(k.Error):
                self.invoke()
        self.assertFalse(self.state.exists())
        self.invoke()
        self.assertTrue(self.state.exists())

    def test_noninteractive_adoption_does_not_replace_service(self):
        with patch.object(k.sys.stdin, 'isatty', return_value=False):
            with self.assertRaises(k.Error):
                self.invoke()
        self.assertNotIn('restart', FakeKeyring.events)
        self.assertNotIn('retire', FakeKeyring.events)


    def test_unadopted_service_does_not_invent_a_locked_state(self):
        FakeKeyring.locked = False
        with patch.object(k.sys.stdin, 'isatty', return_value=False):
            with self.assertRaises(k.Error) as error:
                self.invoke()
            self.assertEqual(error.exception.kind, k.Failure.ADOPTION)
            with patch.object(k, 'storage', return_value=None):
                with self.assertRaises(k.Error) as error:
                    self.invoke()
                self.assertEqual(error.exception.kind, k.Failure.MISSING)

    def test_unsupported_daemon_version_is_not_a_storage_format_error(self):
        with patch.object(k, 'run', return_value=subprocess.CompletedProcess([], 0, b'gnome-keyring-daemon: 99.0\n', b'')):
            with self.assertRaises(k.Error) as error:
                self.invoke()
        self.assertEqual(error.exception.kind, k.Failure.VERSION)
        self.assertEqual(FakeKeyring.events, [])


class ProbeBus:
    def __init__(self, fail=None):
        self.fail = fail
        self.items = []
        self.calls = []
        self.attributes = None

    def call(self, path, interface, method, *args):
        self.calls.append(method)
        if method == 'OpenSession':
            return '', k.dbus.ObjectPath('/session/one')
        if method == 'CreateItem':
            self.attributes = args[0][k.ITEM + '.Attributes']
            self.items.append(k.dbus.ObjectPath('/item/one'))
            if self.fail == 'lost-create-reply':
                raise k.dbus.DBusException('fixture lost reply')
            return self.items[0], k.dbus.ObjectPath('/')
        if method == 'GetSecret':
            if self.fail in ('cancelled', 'cancelled-and-cleanup'):
                raise KeyboardInterrupt
            if self.fail in ('read', 'read-and-cleanup'):
                raise k.dbus.DBusException('fixture read failure')
            value = b'wrong-value' if self.fail == 'wrong-value' else b'coop-probe'
            return '', b'', value, 'text/plain'
        if method == 'SearchItems':
            assert args[0] == self.attributes
            return list(self.items), []
        if method == 'Delete':
            if self.fail in ('cleanup', 'read-and-cleanup', 'cancelled-and-cleanup'):
                raise k.dbus.DBusException('fixture cleanup failure')
            if self.fail == 'delete-prompt':
                return k.dbus.ObjectPath('/prompt/one')
            if self.fail != 'residual-item':
                self.items.remove(path)
            return k.dbus.ObjectPath('/')
        if method == 'Close':
            return None
        raise AssertionError(method)


class ProductionProbeTests(unittest.TestCase):
    def probe(self, bus):
        keyring = k.Keyring.__new__(k.Keyring)
        keyring.call = bus.call
        keyring.probe()

    def test_writes_reads_deletes_and_checks_absence(self):
        bus = ProbeBus()
        self.probe(bus)
        self.assertEqual(bus.calls, ['OpenSession', 'CreateItem', 'GetSecret',
                                    'SearchItems', 'Delete', 'SearchItems', 'Close'])
        self.assertEqual(bus.items, [])
        first_operation = bus.attributes['operation']
        self.probe(bus)
        self.assertNotEqual(first_operation, bus.attributes['operation'])

    def test_lost_create_reply_still_deletes_its_item(self):
        bus = ProbeBus('lost-create-reply')
        with self.assertRaises(k.Error) as error:
            self.probe(bus)
        self.assertEqual(error.exception.kind, k.Failure.WRITE)
        self.assertEqual(bus.items, [])
        self.assertIn('Delete', bus.calls)

    def test_read_failure_and_wrong_value_still_clean_up(self):
        for failure in ('read', 'wrong-value'):
            with self.subTest(failure=failure):
                bus = ProbeBus(failure)
                with self.assertRaises(k.Error) as error:
                    self.probe(bus)
                self.assertEqual(error.exception.kind, k.Failure.WRITE)
                self.assertEqual(bus.items, [])

    def test_cleanup_failure_prompt_and_residual_item_fail(self):
        for failure in ('cleanup', 'delete-prompt', 'residual-item'):
            with self.subTest(failure=failure):
                bus = ProbeBus(failure)
                with self.assertRaises(k.Error) as error:
                    self.probe(bus)
                self.assertEqual(error.exception.kind, k.Failure.CLEANUP)
                self.assertTrue(bus.items)

    def test_original_and_cleanup_failures_are_both_preserved(self):
        bus = ProbeBus('read-and-cleanup')
        with self.assertRaises(k.Error) as error:
            self.probe(bus)
        self.assertEqual(error.exception.kind, k.Failure.WRITE)
        self.assertEqual(error.exception.secondary, k.Failure.CLEANUP)

    def test_cancellation_still_cleans_up_and_preserves_cleanup_failure(self):
        bus = ProbeBus('cancelled')
        with self.assertRaises(KeyboardInterrupt):
            self.probe(bus)
        self.assertEqual(bus.items, [])
        bus = ProbeBus('cancelled-and-cleanup')
        with self.assertRaises(k.Error) as error:
            self.probe(bus)
        self.assertEqual(error.exception.kind, k.Failure.CANCELLED)
        self.assertEqual(error.exception.secondary, k.Failure.CLEANUP)


class CollectionTests(unittest.TestCase):
    def collection(self, paths, alias, locked, candidate):
        keyring = k.Keyring.__new__(k.Keyring)
        def call(path, interface, method, *args):
            if method == 'ReadAlias':
                return alias
            if args[-1] == 'Collections':
                return paths
            if args[-1] == 'Locked':
                return locked
            raise AssertionError((method, args))
        keyring.call = call
        return keyring.collection(candidate)

    def test_locked_and_unlocked_persistent_login(self):
        for locked in (False, True):
            self.assertIs(self.collection([k.LOGIN], k.LOGIN, locked, k.Candidate.ENCRYPTED), locked)

    def test_missing_new_store_vs_invalid_existing_store(self):
        self.assertIsNone(self.collection([], '/', False, None))
        with self.assertRaises(k.Error) as error:
            self.collection([], '/', False, k.Candidate.ENCRYPTED)
        self.assertEqual(error.exception.kind, k.Failure.INVALID)

    def test_session_alias_and_missing_default_are_refused(self):
        for alias in ('/', k.ROOT + '/collection/session', '/other'):
            with self.subTest(alias=alias), self.assertRaises(k.Error) as error:
                self.collection([k.LOGIN], alias, False, k.Candidate.ENCRYPTED)
            self.assertEqual(error.exception.kind, k.Failure.ALIAS)

    def test_conflicting_live_collection_is_not_first_use(self):
        with self.assertRaises(k.Error) as error:
            self.collection([k.LOGIN], k.LOGIN, False, None)
        self.assertEqual(error.exception.kind, k.Failure.CONFLICT)
        with self.assertRaises(k.Error) as error:
            self.collection([k.LOGIN, '/other'], k.LOGIN, False, k.Candidate.ENCRYPTED)
        self.assertEqual(error.exception.kind, k.Failure.CONFLICT)



class MigrationTests(unittest.TestCase):
    def test_conflicting_daemons_and_probe_errors_precede_installation(self):
        source = ROOT / 'scripts/guest/codex-keyring-migrate.sh'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pgrep = root / 'pgrep'
            apt = root / 'apt-get'
            marker = root / 'apt-ran'
            apt.write_text('#!/bin/sh\ntouch "$APT_MARKER"\n')
            apt.chmod(0o755)
            env = {**os.environ, 'PATH': str(root) + ':' + os.environ['PATH'],
                   'GUEST_USER': 'fixture', 'APT_MARKER': str(marker)}
            for output, status, allowed in (('', 1, True), ('123', 0, True),
                                             ('123\n456', 0, False), ('', 2, False),
                                             ('', 124, False)):
                with self.subTest(output=output, status=status):
                    marker.unlink(missing_ok=True)
                    pgrep.write_text('#!/bin/sh\nprintf \"%s\\n\" \"' + output + '\"\nexit ' + str(status) + '\n')
                    pgrep.chmod(0o755)
                    result = subprocess.run(['bash', str(source)], env=env,
                                            capture_output=True, timeout=5)
                    self.assertEqual(result.returncode == 0, allowed)
                    self.assertEqual(marker.exists(), allowed)


if __name__ == '__main__':
    unittest.main()
