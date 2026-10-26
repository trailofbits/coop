#!/usr/bin/env python3
"""Native code-server auth and direct SSH lifecycle checks in a disposable VM."""

import argparse
import contextlib
import http.client
from pathlib import Path
import queue
import re
import signal
import socket
import subprocess
import threading
import time
from urllib.parse import urlsplit


def request(url, path="/", method="GET", headers=None, body=None):
    parsed = urlsplit(url)
    connection = http.client.HTTPConnection("127.0.0.1", parsed.port, timeout=10)
    supplied = {"Host": parsed.netloc, **(headers or {})}
    connection.request(method, path, body=body, headers=supplied)
    response = connection.getresponse()
    result = response.status, {name.lower(): value for name, value in response.getheaders()}, response.read()
    connection.close()
    return result


def login(url, password):
    status, headers, _ = request(
        url, "/login", "POST",
        {"Origin": f"http://{urlsplit(url).netloc}",
         "Content-Type": "application/x-www-form-urlencoded"},
        f"password={password}",
    )
    assert status == 302, status
    cookie = headers["set-cookie"]
    assert "Domain=" not in cookie
    return cookie.split(";", 1)[0]


def reader(pipe):
    lines = queue.Queue()

    def read():
        for line in pipe:
            print(line, end="", flush=True)
            lines.put(line)
        lines.put(None)

    threading.Thread(target=read, daemon=True).start()
    return lines


@contextlib.contextmanager
def editor(command):
    child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
    try:
        lines = reader(child.stderr)
        diagnostics = []
        url = password_path = None
        deadline = time.monotonic() + 620
        while time.monotonic() < deadline:
            line = lines.get(timeout=630)
            if line is None:
                raise AssertionError(f"editor exited before ready: {child.wait()}\n{''.join(diagnostics)}")
            diagnostics.append(line)
            diagnostics = diagnostics[-10:]
            clean = re.sub(r"\x1b\[[0-9;]*m", "", line)
            if "Open: " in clean:
                url = clean.split("Open: ", 1)[1].strip()
            if "Password file: " in clean:
                password_path = Path(clean.split("Password file: ", 1)[1].strip())
            if url and password_path:
                assert password_path.stat().st_mode & 0o777 == 0o600
                yield child, url, password_path
                return
        raise AssertionError("editor readiness timed out")
    finally:
        if child.poll() is None:
            child.send_signal(signal.SIGINT)
            child.wait(timeout=20)


def vm(binary, name, config):
    command = [binary] + (["--config", config] if config else [])
    launch = command + ["web-editor", name]
    # A busy host port must fail before publishing an editor URL.
    with socket.socket() as busy:
        busy.bind(("127.0.0.1", 0))
        busy.listen()
        failed = subprocess.run(launch + ["--port", str(busy.getsockname()[1])],
                                capture_output=True, text=True, timeout=30)
        assert failed.returncode != 0 and "Open: " not in failed.stderr
    with editor(launch) as (child, url, path):
        password = path.read_text()
        assert request(url)[0] == 302
        assert request(url, "/login")[0] == 200
        bad_status, bad_headers, _ = request(url, "/login", "POST",
            {"Content-Type": "application/x-www-form-urlencoded"}, "password=wrong")
        assert bad_status == 200 and "set-cookie" not in bad_headers
        cookie = login(url, password)
        # /healthz is intentionally public in native code-server.
        assert request(url, "/healthz")[0] == 200
        assert request(url, "/healthz", headers={"Cookie": cookie})[0] == 200
        assert request(url, "/?folder=/workspace", headers={"Cookie": cookie})[0] == 200
        # Inspect the real guest process, not just the launch-script text.
        expected_config = f"/tmp/coop-we-{urlsplit(url).hostname.split('.')[0][:24]}/config.yaml"
        inspected = subprocess.run(command + ["exec", name, "--", "python3", "-c", '''
from pathlib import Path
import sys
for path in Path("/proc").glob("[0-9]*/cmdline"):
    try:
        args = path.read_bytes().split(b"\\0")
    except OSError:
        continue
    if b"--disable-workspace-trust" in args and sys.argv[1].encode() in args and any(b"coop-web-editor/code-server-" in arg for arg in args):
        assert args[args.index(b"--auth") + 1] == b"password"
        config_path = Path(args[args.index(b"--config") + 1].decode())
        config = config_path.read_text()
        assert "auth: password" in config and "password:" in config
        assert config_path.stat().st_mode & 0o777 == 0o600
        secret = config.split('password: "')[1].split('"')[0].encode()
        assert len(secret) == 64 and all(secret not in arg for arg in args)
        assert secret not in (Path.home() / ".local/share/coop-web-editor/editor.log").read_bytes()
        print(config_path)
        break
else:
    raise SystemExit("managed editor did not disable workspace trust")
''', expected_config], check=True, capture_output=True, timeout=30)
        guest_config = inspected.stdout.decode().strip()
        # Both invocations remain in the foreground. Ctrl+C in the attached
        # terminal revokes the shared session and releases the original owner.
        with editor(launch) as (attached, attached_url, path):
            assert attached_url == url and attached.poll() is None
            assert path.read_text() == password
            renewed = login(url, password)
            assert request(url, "/healthz", headers={"Cookie": renewed})[0] == 200
            attached.send_signal(signal.SIGINT)
            assert attached.wait(timeout=20) == 0
        child.wait(timeout=15)
        assert child.returncode == 0
        try:
            request(url)
        except OSError:
            pass
        else:
            raise AssertionError("SSH forward survived attached terminal Ctrl+C")
        assert not path.exists(), "password file survived editor stop"
        subprocess.run(command + ["exec", name, "--", "test", "!", "-e", guest_config],
                       check=True, capture_output=True, timeout=30)
    subprocess.run(command + ["stop", name], check=True, capture_output=True, timeout=60)
    subprocess.run(command + ["start", name, "--no-agents"], check=True, capture_output=True, timeout=120)
    with editor(launch) as (child, new_url, path):
        assert new_url != url
        assert request(new_url, "/", headers={"Cookie": cookie})[0] == 302
        assert path.read_text() != password
        cookie = login(new_url, path.read_text())
        assert request(new_url, "/healthz", headers={"Cookie": cookie})[0] == 200
        subprocess.run(command + ["stop", name], check=True, capture_output=True, timeout=60)
        assert child.wait(timeout=15) == 0
    subprocess.run(command + ["start", name, "--no-agents"], check=True, capture_output=True, timeout=120)
    # Abrupt owner death closes stdin; the cooperative guest stops its service
    # and exits SSH. A retry cleans stale private state and starts afresh.
    with editor(launch) as (child, dead_url, path):
        child.kill()
        child.wait(timeout=5)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            try:
                request(dead_url, "/healthz")
            except OSError:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("SSH forward survived owner death")
    with editor(launch) as (child, retry_url, path):
        assert retry_url != dead_url
        cookie = login(retry_url, path.read_text())
        assert request(retry_url, "/?folder=/workspace", headers={"Cookie": cookie})[0] == 200
    print("PASS VM web editor: port collision, owner death/retry, native password auth, private config, repeat launch, stop, restart, session isolation")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary")
    parser.add_argument("--name")
    parser.add_argument("--config")
    args = parser.parse_args()
    if args.binary and args.name:
        vm(args.binary, args.name, args.config)
    else:
        parser.error("supply --binary and --name")
