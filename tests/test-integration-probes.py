#!/usr/bin/env python3
"""Regression tests for VM integration probes, without booting a VM."""
import http.server
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import unittest

SOURCE = (Path(__file__).parent / "integration.sh").read_text()


def functions(*names):
    return "\n".join(
        re.search(r"^" + name + r"\(\) \{\n.*?^\}", SOURCE, re.M | re.S)[0]
        for name in names
    )


def shell(script, **env):
    return subprocess.run(
        ["bash", "-c", script], env={**os.environ, **env},
        capture_output=True, text=True, timeout=45,
    )


class ProbeTests(unittest.TestCase):
    def test_codex_installer_checks_its_compatibility_link(self):
        installer = (Path(__file__).parent.parent / "scripts/guest/codex.sh").read_text()
        for download_status, install_status, launch_status in [
            (0, 0, 0), (7, 0, 0), (0, 9, 0), (0, 0, 11),
        ]:
            with self.subTest(download=download_status, install=install_status,
                              launch=launch_status), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                native = root / "guest/.local/bin/codex"
                system_bin = root / "bin"
                system_bin.mkdir()
                binary = root / "codex"
                binary.write_text('#!/bin/sh\necho codex-cli 1.2.3\nexit "$LAUNCH_STATUS"\n')
                upstream = root / "install.sh"
                upstream.write_text('''
                    set -eu
                    test "$INSTALL_STATUS" = 0 || exit "$INSTALL_STATUS"
                    mkdir -p "$(dirname "$NATIVE_BIN")"
                    cp "$FIXTURE_BINARY" "$NATIVE_BIN"
                    chmod +x "$NATIVE_BIN"
                ''')
                fragment = installer.replace("/home/${GUEST_USER}", str(root / "guest"))
                fragment = fragment.replace("/usr/local/bin", str(system_bin))
                result = shell('''
                    curl() {
                        test "$DOWNLOAD_STATUS" = 0 || return "$DOWNLOAD_STATUS"
                        while [[ "$1" != -o ]]; do shift; done
                        cp "$UPSTREAM_INSTALLER" "$2"
                    }
                    su() {
                        [[ "$1" == - && "$2" == ubuntu && "$3" == -c ]] || return 99
                        bash -c "$4"
                    }
                    mv() { shift; command mv -f "$@"; }
                ''' + fragment, GUEST_USER="ubuntu", COOP_FORCE_INSTALL="1",
                    DOWNLOAD_STATUS=str(download_status), INSTALL_STATUS=str(install_status),
                    LAUNCH_STATUS=str(launch_status), NATIVE_BIN=str(native),
                    FIXTURE_BINARY=str(binary), UPSTREAM_INSTALLER=str(upstream))
                self.assertEqual(result.returncode == 0,
                                 not (download_status or install_status or launch_status),
                                 result.stdout + result.stderr)
                link = system_bin / "codex"
                if download_status or install_status:
                    self.assertFalse(link.is_symlink())
                else:
                    self.assertTrue(link.samefile(native))
                    self.assertIn("codex-cli 1.2.3", result.stdout)
                self.assertFalse(list(system_bin.glob("codex.new.*")))

    def test_codex_self_update_requires_a_version_change(self):
        for update_status, version_status, after, succeeds in [
            (0, 0, "codex-cli 0.154.0", True),
            (0, 0, "codex-cli 0.153.0", False),
            (1, 0, "codex-cli 0.154.0", False),
            (0, 1, "codex-cli 0.154.0", False),
            (0, 0, "", False),
        ]:
            with self.subTest(update=update_status, version=version_status, after=after):
                result = shell(functions("check_codex_self_update") + '''
                    current="codex-cli 0.153.0"
                    update_called=0
                    guest_exec() {
                        case "$*" in
                            "codex update")
                                update_called=1
                                current="$AFTER"
                                return "$UPDATE_STATUS" ;;
                            "codex --version")
                                printf '%s\\n' "$current"
                                return "$VERSION_STATUS" ;;
                            *) return 99 ;;
                        esac
                    }
                    guest_stderr() { echo fixture-error; }
                    pass() { echo "PASS $*"; }
                    fail() { echo "FAIL $*"; }
                    check_codex_self_update "$current"
                    test "$update_called" = 1
                ''', UPDATE_STATUS=str(update_status), VERSION_STATUS=str(version_status),
                    AFTER=after)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.startswith("PASS"), succeeds, result.stdout)

    def test_codex_config_preservation_checks_actual_contents(self):
        fragment = functions("seed_codex_update_config", "check_codex_config_preserved")
        fragment = fragment.replace("$HOME/", "$FIXTURE_HOME/")
        for mutation in ("unchanged", "changed", "deleted", "missing-snapshot"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                result = shell(fragment + '''
                    set -eu
                    guest_exec() { "$@"; }
                    guest_stderr() { echo fixture-error; }
                    pass() { echo "PASS $*"; }
                    fail() { echo "FAIL $*"; }
                    seed_codex_update_config
                    case "$MUTATION" in
                        changed) echo '# changed' >> "$FIXTURE_HOME/.codex/config.toml" ;;
                        deleted) rm "$FIXTURE_HOME/.codex/config.toml" ;;
                        missing-snapshot) rm "$FIXTURE_HOME/.codex/coop-update-config.expected" ;;
                    esac
                    check_codex_config_preserved update
                ''', FIXTURE_HOME=directory, MUTATION=mutation)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.startswith("PASS"), mutation == "unchanged",
                                 result.stdout)

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux guest provisioning")
    def test_fcnet_mask_replaces_existing_unit(self):
        setup = (Path(__file__).parent.parent / "scripts/guest/guest-config.sh").read_text()
        fragment = setup[:setup.index("echo '  [guest] Configuring guest networking...'")]
        with tempfile.TemporaryDirectory() as directory:
            unit = Path(directory) / "fcnet.service"
            unit.write_text("[Service]\nExecStart=/usr/local/bin/fcnet-setup.sh\n")
            fragment = fragment.replace("/etc/systemd/system/fcnet.service", str(unit))
            result = shell("systemctl() { return 1; }\n" + fragment)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(unit.is_symlink())
            self.assertEqual(os.readlink(unit), "/dev/null")

    def test_addresses(self):
        for addresses, expected in [
            ("2: eth0 inet 172.16.0.2/24\n2: eth0 inet 172.16.0.2/30", "172.16.0.2"),
            ("2: eth0 inet 172.16.0.3/24", "172.16.0.3"),
            ("", None),
            ("2: eth0 inet 172.16.0.2/24\n2: eth0 inet 172.16.0.3/24", None),
            ("2: eth0 inet 999.1.1.1/24", None),
            ("garbage", None),
        ]:
            with self.subTest(addresses=addresses):
                result = shell(functions("guest_ip_of") + '''
                    guest_exec() { printf '%s\n' "$FIXTURE"; }
                    guest_stderr() { echo transport-error; }
                    guest_ip_of example
                ''', FIXTURE=addresses)
                self.assertEqual(result.returncode == 0, expected is not None)
                if expected is not None:
                    self.assertEqual(result.stdout.strip(), expected)
        result = shell(functions("guest_ip_of") + '''
            guest_exec() { echo '2: eth0 inet 172.16.0.2/24'; return 255; }
            guest_stderr() { echo transport-error; }
            guest_ip_of example
        ''')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("transport-error", result.stderr)

    def test_ping_status_and_sudo(self):
        with tempfile.TemporaryDirectory() as directory:
            ping = Path(directory) / "ping"
            ping.write_text('#!/bin/sh\nexit "$PING_STATUS"\n')
            ping.chmod(0o755)
            for status, expected in [(0, 0), (1, 42), (2, 43), (127, 43)]:
                with self.subTest(status=status):
                    result = shell(functions("guest_ping") + '''
                        guest_exec() {
                            [[ "$1" == sudo && "$2" == -n ]] || return 99
                            shift 2
                            "$@" || return 1
                        }
                        guest_ping 172.16.0.3
                    ''', PATH=directory + ":" + os.environ["PATH"], PING_STATUS=str(status))
                    self.assertEqual(result.returncode, expected)

    def test_ping_transport_errors_and_unexpected_output(self):
        for status, output in [(1, ""), (255, ""), (0, "unexpected"),
                               (0, "no-reply\nextra"), (1, "no-reply")]:
            with self.subTest(status=status, output=output):
                result = shell(functions("guest_ping") + '''
                    guest_exec() { printf '%s\n' "$OUTPUT"; return "$STATUS"; }
                    guest_ping 172.16.0.3
                ''', STATUS=str(status), OUTPUT=output)
                self.assertEqual(result.returncode, 43)

    def test_only_ping_no_reply_counts_as_blocked(self):
        for status in (0, 1, 42, 43, 127, 255):
            with self.subTest(status=status):
                result = shell(functions("assert_guest_ping_blocked") + '''
                    guest_ping() { return "$PROBE_STATUS"; }
                    guest_stderr() { echo probe-error; }
                    pass() { echo PASS; }
                    fail() { echo "FAIL $*"; }
                    assert_guest_ping_blocked 172.16.0.3 blocked
                ''', PROBE_STATUS=str(status))
                self.assertEqual(result.stdout.startswith("PASS"), status == 42)
                if status not in (0, 42):
                    self.assertIn("probe-error", result.stdout)

    def test_http_retry_and_persistent_failures(self):
        for responses, expected, requests in [
            ([504, 200], "PASS HTTPS", 2),
            ([504], "FAIL HTTPS", 3),
            ([404], "FAIL HTTPS", 1),
        ]:
            with self.subTest(responses=responses), tempfile.TemporaryDirectory() as directory:
                seen = []

                class Handler(http.server.BaseHTTPRequestHandler):
                    def do_GET(self):
                        code = responses[min(len(seen), len(responses) - 1)]
                        seen.append(code)
                        self.send_response(code)
                        self.send_header("Content-Length", "0")
                        self.end_headers()

                    def log_message(self, *_args):
                        pass

                with http.server.HTTPServer(("127.0.0.1", 0), Handler) as server:
                    thread = threading.Thread(target=server.serve_forever)
                    thread.start()
                    try:
                        result = shell(functions("test_network") + '''
                            guest_exec() {
                                if [[ "$1" != curl ]]; then return 0; fi
                                local args=("$@")
                                args[${#args[@]}-1]="$TEST_URL"
                                "${args[@]}" 2>"$TEST_ERR"
                            }
                            guest_stderr() { cat "$TEST_ERR"; }
                            pass() { echo "PASS $*"; }
                            fail() { echo "FAIL $*"; }
                            test_network
                        ''', TEST_URL=f"http://127.0.0.1:{server.server_port}",
                            TEST_ERR=str(Path(directory) / "stderr"))
                    finally:
                        server.shutdown()
                        thread.join()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(expected, result.stdout)
                self.assertEqual(len(seen), requests)
                if expected.startswith("FAIL"):
                    self.assertIn(f"HTTP {responses[-1]}", result.stdout)


if __name__ == "__main__":
    unittest.main()
