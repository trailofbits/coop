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
