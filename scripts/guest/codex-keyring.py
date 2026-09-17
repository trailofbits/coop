#!/usr/bin/python3
"""Guest-only singleton Secret Service readiness and user-triggered recovery.

Never reads a password or takes over native Codex process ownership. Recovery
retires cached server authentication with the native stop command; the desktop
owns the next bootstrap. All state here is non-secret and scoped to this boot.
"""
import contextlib
import enum
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import sys
import tempfile
import time

import dbus

SERVICE = 'org.freedesktop.Secret.Service'
COLLECTION = 'org.freedesktop.Secret.Collection'
ITEM = 'org.freedesktop.Secret.Item'
PROPERTIES = 'org.freedesktop.DBus.Properties'
ROOT = '/org/freedesktop/secrets'
LOGIN = ROOT + '/collection/login'
PREFIX = b'GnomeKeyring\n\r\0\n' + bytes(4)
CODEX = '/usr/local/bin/codex'
PAM = '/usr/local/libexec/coop-codex-keyring-pam'
MIGRATION = Path('/var/lib/coop/codex-keyring-install-boot')


class Candidate(enum.Enum):
    ENCRYPTED = 'supported encrypted-format candidate'


class Failure(enum.Enum):
    POLICY = 'managed ChatGPT keyring policy is required; restart with auth = "chatgpt"'
    MIGRATION = 'guest support was installed this boot; restart the VM before unlocking'
    BUS = 'user bus or packaged keyring service is unavailable'
    BUSY = 'another unlock operation is busy; retry shortly'
    FORMAT = 'plaintext or unsupported keyring storage; explicitly migrate and reauthenticate'
    INVALID = 'existing keyring storage could not be loaded; preserve it for recovery'
    CONFLICT = 'conflicting keyring storage or live collections; resolve histories explicitly'
    MISSING = 'persistent login collection is missing or uninitialized; run coop codex-unlock in an interactive terminal'
    ADOPTION = 'keyring storage needs interactive verification; run coop codex-unlock in an interactive terminal'
    VERSION = 'installed GNOME Keyring version is unsupported; this helper requires 46.1'
    LOCKED = 'login collection is locked; run coop codex-unlock in an interactive terminal'
    ALIAS = 'default alias does not name the persistent login collection'
    UNLOCK = 'keyring unlock failed; the password or encrypted storage may be invalid'
    WRITE = 'disposable keyring write/read probe failed'
    CLEANUP = 'disposable keyring item cleanup failed; rerun unlock after restoring the service'
    CANCELLED = 'keyring operation cancelled'
    GENERATION = 'keyring service changed during unlock; reconnect and retry'
    NATIVE_BUSY = 'native Codex lifecycle lock or stop timed out; retry after desktop startup finishes'
    NATIVE_OWNER = 'native Codex server ownership conflict; resolve it with the native daemon commands'
    NATIVE = 'native Codex server recovery failed; no ready state was recorded'
    TERMINATION = 'native stop did not retire the observed server; retry after concurrent startup finishes'


class Error(Exception):
    def __init__(self, kind, secondary=None):
        self.kind = kind
        self.secondary = secondary
        super().__init__(kind.value)


def run(args, timeout=20):
    return subprocess.run(args, stdin=subprocess.DEVNULL, capture_output=True,
                          timeout=timeout, check=False)


def policy(home):
    if os.environ.get('CODEX_HOME') or os.environ.get('XDG_DATA_HOME', str(home / '.local/share')) != str(home / '.local/share'):
        raise Error(Failure.POLICY)
    try:
        with (home / '.codex/config.toml').open('rb') as config:
            first = config.readline(128)
        if first != b'cli_auth_credentials_store = "keyring"\n':
            raise Error(Failure.POLICY)
        if (home / '.codex/auth.json').exists():
            raise Error(Failure.POLICY)
    except OSError as error:
        raise Error(Failure.POLICY) from error


def storage(home):
    """Classify storage without claiming ciphertext integrity."""
    directory = home / '.local/share/keyrings'
    if directory.is_symlink():
        raise Error(Failure.CONFLICT)
    if directory.exists():
        names = {p.name for p in directory.glob('*.keyring')}
        if names - {'login.keyring'}:
            raise Error(Failure.CONFLICT)
    alias = directory / 'default'
    if alias.is_symlink():
        raise Error(Failure.ALIAS)
    if alias.exists():
        try:
            with alias.open('rb') as stream:
                if stream.read(7) not in (b'login', b'login\n'):
                    raise Error(Failure.ALIAS)
        except OSError as error:
            raise Error(Failure.ALIAS) from error
    path = directory / 'login.keyring'
    if path.is_symlink():
        raise Error(Failure.CONFLICT)
    if not path.exists():
        return None
    if not path.is_file():
        raise Error(Failure.INVALID)
    if path.stat().st_size > 16 * 1024 * 1024:
        raise Error(Failure.INVALID)
    with path.open('rb') as stream:
        if stream.read(len(PREFIX)) != PREFIX:
            raise Error(Failure.FORMAT)
    return Candidate.ENCRYPTED


@contextlib.contextmanager
def operation_lock(directory):
    directory.mkdir(mode=0o700, exist_ok=True)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise Error(Failure.BUS)
    fd = os.open(directory / 'operation.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        deadline = time.monotonic() + 15
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise Error(Failure.BUSY)
                time.sleep(0.1)
        yield
    finally:
        os.close(fd)


def systemctl(operation):
    try:
        # An explicit retry may follow several failures inside systemd's start
        # limit window. Reset the failure counter once; startup stays bounded.
        reset = run(['/usr/bin/systemctl', '--user', 'reset-failed',
                     'gnome-keyring-daemon.service'], timeout=10)
        if reset.returncode:
            raise Error(Failure.BUS)
        result = run(['/usr/bin/systemctl', '--user', operation,
                      'gnome-keyring-daemon.service'], timeout=20)
    except subprocess.TimeoutExpired as error:
        raise Error(Failure.BUS) from error
    if result.returncode:
        raise Error(Failure.BUS)


class Keyring:
    def __init__(self, runtime):
        self.bus = None
        try:
            self.bus = dbus.bus.BusConnection('unix:path=' + str(runtime / 'bus'))
            bus_object = self.bus.get_object('org.freedesktop.DBus', '/org/freedesktop/DBus', introspect=False)
            self.server = str(bus_object.GetId(dbus_interface='org.freedesktop.DBus', timeout=5))
            deadline = time.monotonic() + 5
            while not bus_object.NameHasOwner('org.freedesktop.secrets', dbus_interface='org.freedesktop.DBus', timeout=5):
                if time.monotonic() >= deadline:
                    raise Error(Failure.BUS)
                time.sleep(0.05)
            self.owner = str(bus_object.GetNameOwner('org.freedesktop.secrets', dbus_interface='org.freedesktop.DBus', timeout=5))
            owner_pid = int(bus_object.GetConnectionUnixProcessID(self.owner, dbus_interface='org.freedesktop.DBus', timeout=5))
            managed = run(['/usr/bin/systemctl', '--user', 'show', 'gnome-keyring-daemon.service', '-p', 'MainPID', '--value'])
            if managed.returncode or managed.stdout.strip() != str(owner_pid).encode():
                raise Error(Failure.BUS)
        except (dbus.DBusException, Error) as error:
            if self.bus is not None:
                self.bus.close()
            raise Error(Failure.BUS) from error

    def call(self, path, interface, method, *args):
        # Bind to the observed unique owner: never auto-activate a replacement
        # halfway through a probe or accidentally delete its items.
        obj = self.bus.get_object(self.owner, path, introspect=False)
        return obj.get_dbus_method(method, interface)(*args, timeout=5)

    def generation(self, boot):
        if str(self.bus.get_name_owner('org.freedesktop.secrets')) != self.owner:
            raise Error(Failure.GENERATION)
        return [boot, self.server, self.owner]

    def collection(self, candidate):
        collections = self.call(ROOT, PROPERTIES, 'Get', SERVICE, 'Collections')
        persistent = set(map(str, collections)) - {ROOT + '/collection/session'}
        if persistent - {LOGIN}:
            raise Error(Failure.CONFLICT)
        alias = str(self.call(ROOT, SERVICE, 'ReadAlias', 'default'))
        if alias not in ('/', LOGIN):
            raise Error(Failure.ALIAS)
        if LOGIN not in persistent:
            if candidate is not None:
                raise Error(Failure.INVALID)
            if alias != '/':
                raise Error(Failure.ALIAS)
            return None
        if candidate is None:
            raise Error(Failure.CONFLICT)
        if alias != LOGIN:
            raise Error(Failure.ALIAS)
        return bool(self.call(LOGIN, PROPERTIES, 'Get', COLLECTION, 'Locked'))

    def probe(self):
        import uuid
        attributes = dbus.Dictionary({'service': 'coop-codex-readiness', 'operation': uuid.uuid4().hex}, signature='ss')
        session = None
        attempted = False
        original = None
        try:
            _, session = self.call(ROOT, SERVICE, 'OpenSession', 'plain', dbus.String('', variant_level=1))
            properties = dbus.Dictionary({
                ITEM + '.Label': 'coop disposable readiness probe',
                ITEM + '.Attributes': attributes,
            }, signature='sv')
            secret = dbus.Struct((session, dbus.ByteArray(b''), dbus.ByteArray(b'coop-probe'), 'text/plain'), signature='oayays')
            attempted = True
            item, prompt = self.call(LOGIN, COLLECTION, 'CreateItem', properties, secret, False)
            if str(prompt) != '/' or str(item) == '/':
                raise Error(Failure.WRITE)
            read = self.call(item, ITEM, 'GetSecret', session)
            if bytes(read[2]) != b'coop-probe':
                raise Error(Failure.WRITE)
        except (dbus.DBusException, Error, KeyboardInterrupt) as error:
            original = error
        finally:
            try:
                if attempted:
                    # Also cleans a successfully created item after a lost reply.
                    unlocked, locked = self.call(ROOT, SERVICE, 'SearchItems', attributes)
                    for item in [*unlocked, *locked]:
                        prompt = self.call(item, ITEM, 'Delete')
                        if str(prompt) != '/':
                            raise Error(Failure.CLEANUP)
                    unlocked, locked = self.call(ROOT, SERVICE, 'SearchItems', attributes)
                    if unlocked or locked:
                        raise Error(Failure.CLEANUP)
                if session is not None:
                    self.call(session, 'org.freedesktop.Secret.Session', 'Close')
            except (dbus.DBusException, Error) as cleanup:
                if original:
                    # Preserve the first failure as well as the cleanup failure.
                    primary = Failure.CANCELLED if isinstance(original, KeyboardInterrupt) else Failure.WRITE
                    raise Error(primary, Failure.CLEANUP) from cleanup
                raise Error(Failure.CLEANUP) from cleanup
        if isinstance(original, KeyboardInterrupt):
            raise original
        if original:
            raise Error(Failure.WRITE) from original


def retire_server(home):
    """Use native ownership checks; pidfd observes termination but never signals.

    Do not start/bootstrap here: native failed-start rollback cannot be scoped
    to our invocation when the desktop also bootstraps. Retirement alone clears
    the stale cache; the desktop owns subsequent startup and updater lifecycle.
    """
    descriptor = None
    try:
        record = home / '.codex/app-server-daemon/app-server.pid'
        if record.exists():
            try:
                pid = json.loads(record.read_text())['pid']
                if not isinstance(pid, int) or pid <= 0:
                    raise Error(Failure.NATIVE_OWNER)
                descriptor = os.pidfd_open(pid)
            except ProcessLookupError:
                pass
            except (ValueError, KeyError, OSError) as error:
                raise Error(Failure.NATIVE_OWNER) from error
        try:
            result = run([CODEX, 'app-server', 'daemon', 'stop'], timeout=90)
        except subprocess.TimeoutExpired as error:
            raise Error(Failure.NATIVE_BUSY) from error
        if result.returncode:
            if b'not managed by codex' in result.stderr:
                raise Error(Failure.NATIVE_OWNER)
            if b'operation lock' in result.stderr:
                raise Error(Failure.NATIVE_BUSY)
            raise Error(Failure.NATIVE)
        try:
            status = json.loads(result.stdout)['status']
        except (ValueError, KeyError) as error:
            raise Error(Failure.NATIVE) from error
        if status not in ('stopped', 'notRunning'):
            raise Error(Failure.NATIVE)
        if descriptor is not None and not select.select([descriptor], [], [], 5)[0]:
            raise Error(Failure.TERMINATION)
    finally:
        if descriptor is not None:
            os.close(descriptor)


def pam_unlock(creating):
    args = [PAM] + (['--create'] if creating else [])
    process = subprocess.Popen(args)
    try:
        if process.wait(timeout=160):
            raise Error(Failure.UNLOCK)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def record_success(path, value):
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix='.ready-')
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def unlock(home, runtime, boot):
    policy(home)
    if not MIGRATION.exists() or MIGRATION.read_text().strip() == boot:
        raise Error(Failure.MIGRATION)
    with operation_lock(runtime / 'coop-codex'):
        path = runtime / 'coop-codex/ready.json'
        keyring = None
        try:
            try:
                with path.open() as stream:
                    prior = json.loads(stream.read(1024))
            except (OSError, ValueError):
                prior = None
            candidate = storage(home)  # Before service activation, prompts or writes.
            version = run(['/usr/bin/gnome-keyring-daemon', '--version'])
            if version.returncode or version.stdout.splitlines()[0:1] != [b'gnome-keyring-daemon: 46.1']:
                raise Error(Failure.VERSION)
            systemctl('start')
            keyring = Keyring(runtime)
            generation = keyring.generation(boot)
            adopted = prior == generation
            if not adopted:
                if not sys.stdin.isatty():
                    raise Error(Failure.MISSING if candidate is None else Failure.ADOPTION)
                # Never let an old success survive any failed adoption/recovery.
                path.unlink(missing_ok=True)
                # Reject conflicting live state before replacing the service.
                try:
                    keyring.collection(candidate)
                except Error as error:
                    # Only the fresh daemon can classify an existing file as
                    # unloadable; the current service may predate its adoption.
                    if error.kind is not Failure.INVALID:
                        raise
                retire_server(home)
                keyring.bus.close()
                systemctl('restart')
                keyring = Keyring(runtime)
                generation = keyring.generation(boot)
            locked = keyring.collection(candidate)
            if locked is not False:
                path.unlink(missing_ok=True)
                if not sys.stdin.isatty():
                    raise Error(Failure.MISSING if locked is None else Failure.LOCKED)
                pam_unlock(candidate is None)
                candidate = storage(home)
                if candidate is None or keyring.collection(candidate) is not False:
                    raise Error(Failure.UNLOCK)
            keyring.probe()
            if not adopted or locked is not False:
                retire_server(home)
            if keyring.collection(storage(home)) is not False:
                raise Error(Failure.LOCKED)
            if keyring.generation(boot) != generation:
                raise Error(Failure.GENERATION)
            record_success(path, generation)
            # A concurrent service crash must not leave a successful record.
            if keyring.generation(boot) != generation:
                path.unlink(missing_ok=True)
                raise Error(Failure.GENERATION)
        except BaseException:
            path.unlink(missing_ok=True)
            raise
        finally:
            if keyring is not None:
                keyring.bus.close()


def main():
    if len(sys.argv) != 1:
        return 2
    os.umask(0o077)
    home = Path.home()
    runtime = Path('/run/user') / str(os.getuid())
    if os.getuid() == 0 or os.environ.get('XDG_RUNTIME_DIR') != str(runtime):
        raise Error(Failure.BUS)
    # Standard PAM/systemd environment, never a private terminal bus.
    os.environ['DBUS_SESSION_BUS_ADDRESS'] = 'unix:path=' + str(runtime / 'bus')
    os.environ['GNOME_KEYRING_CONTROL'] = str(runtime / 'keyring')
    boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    unlock(home, runtime, boot)
    print('Guest keyring ready. Connect or reconnect the desktop over SSH.')
    return 0


if __name__ == '__main__':
    def cancelled(signum, frame):
        raise KeyboardInterrupt
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, cancelled)
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print('codex-keyring: cancelled; rerun unlock to retry', file=sys.stderr)
        sys.exit(130)
    except Error as error:
        print('codex-keyring: ' + error.kind.value +
              ('; ' + error.secondary.value if error.secondary else ''), file=sys.stderr)
        sys.exit(130 if error.kind is Failure.CANCELLED else 1)
    except (dbus.DBusException, OSError, subprocess.TimeoutExpired):
        print('codex-keyring: guest operation failed or timed out; rerun unlock to retry', file=sys.stderr)
        sys.exit(1)
