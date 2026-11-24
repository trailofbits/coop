#!/usr/bin/env python3
"""Native code-server auth and direct SSH lifecycle checks in a disposable VM."""

import argparse
import contextlib
import http.client
import os
from pathlib import Path
import queue
import re
import secrets
import select
import signal
import socket
import subprocess
import threading
import time
from urllib.parse import parse_qs, urlsplit


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
def editor(command, launch_probe=None):
    child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
    try:
        lines = reader(child.stderr)
        diagnostics = []
        url = password_path = None
        deadline = time.monotonic() + 2120
        launch_waiting = False
        launch_probes = 0
        while time.monotonic() < deadline:
            try:
                line = lines.get(timeout=min(30, max(0.1, deadline - time.monotonic())))
            except queue.Empty:
                if launch_waiting and launch_probe is not None and launch_probes < 5:
                    launch_probe()
                    launch_probes += 1
                continue
            if line is None:
                raise AssertionError(f"editor exited before ready: {child.wait()}\n{''.join(diagnostics)}")
            diagnostics.append(line)
            diagnostics = diagnostics[-10:]
            clean = re.sub(r"\x1b\[[0-9;]*m", "", line)
            if "Starting code-server through guest systemd" in clean:
                launch_waiting = True
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


def inspect_guest_editor(command, name, url, project):
    # Inspect the real guest process, not just the launch-script text.
    expected_config = f"/tmp/coop-we-{urlsplit(url).hostname.split('.')[0][:24]}/config.yaml" if url else ""
    inspected = subprocess.run(command + ["exec", name, "--", "python3", "-c", '''
from pathlib import Path
import subprocess
import sys
for path in Path("/proc").glob("[0-9]*/cmdline"):
    try:
        args = path.read_bytes().split(b"\\0")
    except OSError:
        continue
    if b"--disable-workspace-trust" in args and b"--service-type=exec" not in args and (not sys.argv[1] or sys.argv[1].encode() in args) and any(b"coop-web-editor/code-server-" in arg for arg in args):
        assert args[args.index(b"--auth") + 1] == b"password"
        assert args[-2] == sys.argv[2].encode()
        config_path = Path(args[args.index(b"--config") + 1].decode())
        config = config_path.read_text()
        assert "auth: password" in config and "password:" in config
        assert config_path.stat().st_mode & 0o777 == 0o600
        secret = config.split('password: "')[1].split('"')[0].encode()
        assert len(secret) == 64 and all(secret not in arg for arg in args)
        assert secret not in (Path.home() / ".local/share/coop-web-editor/editor.log").read_bytes()
        unit = config_path.parent.name + ".service"
        result = subprocess.run(["systemctl", "show", unit, "--property=ActiveState,ControlGroup"],
                                check=True, capture_output=True, text=True, timeout=5)
        state = dict(line.split("=", 1) for line in result.stdout.splitlines())
        assert state["ActiveState"] == "active" and state["ControlGroup"], state
        print(config_path)
        print(state["ControlGroup"])
        break
else:
    raise SystemExit("managed editor did not disable workspace trust")
''', expected_config, project], check=True, capture_output=True, timeout=30)
    return inspected.stdout.decode().splitlines()


def assert_guest_editor_stopped(command, name, guest_config, cgroup):
    unit = Path(guest_config).parent.name + ".service"
    result = subprocess.run(command + ["exec", name, "--", "python3", "-c", '''
from pathlib import Path
import subprocess
import sys
import time

unit, cgroup = sys.argv[1:]
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    result = subprocess.run(["systemctl", "show", unit, "--property=LoadState,ActiveState"],
                            check=True, capture_output=True, text=True, timeout=5)
    state = dict(line.split("=", 1) for line in result.stdout.splitlines())
    pids = []
    # /proc covers nested cgroups on both unified and legacy guest hierarchies.
    for path in Path("/proc").glob("[0-9]*/cgroup"):
        try:
            groups = [line.split(":", 2)[2] for line in path.read_text().splitlines()]
        except (FileNotFoundError, ProcessLookupError):
            continue
        if any(group == cgroup or group.startswith(cgroup + "/") for group in groups):
            pids.append(path.parent.name)
    # --collect may have unloaded the transient unit already.
    if state["LoadState"] in ("loaded", "not-found") and state["ActiveState"] == "inactive" and not pids:
        break
    time.sleep(0.1)
else:
    raise AssertionError(f"guest editor {unit} survived cleanup: {state}, remaining PIDs: {pids}")
''', unit, cgroup], capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, result.stderr


def owner_death_before_ready(command, name, launch, project):
    # Suppress readiness while the real code-server and its sockets are live.
    # The guest script resets PATH, so intercept its first trusted curl location.
    fixture = f"/tmp/coop-editor-curl-{secrets.token_hex(12)}"
    curl_wrapper = r'''
set -euo pipefail
wrapper=/usr/local/bin/curl
backup="$1.original"
staged="$1.new"
if [[ $2 == setup ]]; then
    [[ ! -e $backup && ! -L $backup ]]
    [[ ! -e $wrapper && ! -L $wrapper ]] || sudo -n mv -- "$wrapper" "$backup"
    cat > "$staged" <<'EOF'
#!/bin/sh
for arg do
    [ "$arg" != http://localhost/healthz ] || exit 22
done
exec /usr/bin/curl "$@"
EOF
    sudo -n install -o root -g root -m 755 -- "$staged" "$wrapper"
    rm -f -- "$staged"
else
    sudo -n rm -f -- "$wrapper"
    [[ ! -e $backup && ! -L $backup ]] || sudo -n mv -- "$backup" "$wrapper"
    rm -f -- "$staged"
fi
'''
    child = None
    try:
        subprocess.run(command + ["exec", name, "--", "bash", "-c", curl_wrapper, "bash", fixture, "setup"],
                       check=True, capture_output=True, timeout=30)
        child = subprocess.Popen(launch, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + 60
        while True:
            assert child.poll() is None, child.communicate()[1]
            try:
                guest_config, cgroup = inspect_guest_editor(command, name, None, project)
                subprocess.run(command + ["exec", name, "--", "/usr/bin/curl", "-q", "--silent", "--fail",
                               "--max-time", "1", "--unix-socket", str(Path(guest_config).parent / "editor.sock"),
                               "http://localhost/healthz"], check=True, capture_output=True, timeout=5)
                break
            except subprocess.CalledProcessError as error:
                assert time.monotonic() < deadline, error.stderr
                time.sleep(0.1)
        child.kill()
        stdout, stderr = child.communicate(timeout=10)
        assert "Open: " not in stdout + stderr, "editor became ready before owner death"
        assert_guest_editor_stopped(command, name, guest_config, cgroup)
        subprocess.run(command + ["exec", name, "--", "timeout", "5", "bash", "-c",
                       'while [[ -e $1 ]]; do sleep 0.1; done', "bash", str(Path(guest_config).parent)],
                       check=True, capture_output=True, timeout=10)
    finally:
        if child is not None and child.poll() is None:
            child.kill()
            child.wait(timeout=5)
        subprocess.run(command + ["exec", name, "--", "bash", "-c", curl_wrapper, "bash", fixture, "restore"],
                       check=True, capture_output=True, timeout=30)


def download_failure(command, name, launch):
    fixture = f"/tmp/coop-editor-download-{secrets.token_hex(12)}"
    setup = r'''
set -euo pipefail
fixture=$1
base="$HOME/.local/share/coop-web-editor"
mkdir -m 700 -- "$fixture"
mkdir -- "$fixture/installs"
curl_config="${CURL_HOME:-$HOME}/.curlrc"
if [[ -z ${CURL_HOME:-} && -n ${XDG_CONFIG_HOME:-} ]]; then
    curl_config="$XDG_CONFIG_HOME/curlrc"
fi
mkdir -p -- "${curl_config%/*}"
if [[ -e "$curl_config" || -L "$curl_config" ]]; then
    mv -- "$curl_config" "$fixture/curlrc.original"
fi
for install in "$base"/code-server-*; do
    [[ -e "$install" ]] || continue
    mv -- "$install" "$fixture/installs/"
done
touch "$fixture/config-installed"
# .invalid is reserved; an explicit empty noproxy overrides NO_PROXY.
printf 'proxy = "http://coop-editor-download.invalid.:9"\nnoproxy = ""\n' > "$curl_config"
'''
    restore = r'''
set -euo pipefail
fixture=$1
base="$HOME/.local/share/coop-web-editor"
curl_config="${CURL_HOME:-$HOME}/.curlrc"
if [[ -z ${CURL_HOME:-} && -n ${XDG_CONFIG_HOME:-} ]]; then
    curl_config="$XDG_CONFIG_HOME/curlrc"
fi
if [[ -e "$fixture/config-installed" ]]; then
    rm -f -- "$curl_config"
fi
if [[ -e "$fixture/curlrc.original" || -L "$fixture/curlrc.original" ]]; then
    mv -- "$fixture/curlrc.original" "$curl_config"
fi
for install in "$fixture"/installs/*; do
    [[ -e "$install" ]] || continue
    mkdir -p -- "$base"
    mv -- "$install" "$base/"
done
rm -rf -- "$fixture"
'''
    try:
        subprocess.run(command + ["exec", name, "--", "bash", "-c", setup, "bash", fixture],
                       check=True, capture_output=True, timeout=30)
        failed = subprocess.run(launch, capture_output=True, text=True, timeout=45)
        assert failed.returncode != 0
        assert "Guest code-server download failed DNS resolution" in failed.stderr, failed.stderr
        assert "editor.log" not in failed.stderr and "Open: " not in failed.stderr
        assert "curl:" not in failed.stdout + failed.stderr
    finally:
        subprocess.run(command + ["exec", name, "--", "bash", "-c", restore, "bash", fixture],
                       check=True, capture_output=True, timeout=30)


def bounded_guest_probe(command, name, probe):
    # `coop shell` streams output; `coop exec` buffers guest output without a
    # limit. Read at most one small status line from the untrusted VM.
    try:
        child = subprocess.Popen(command + ["shell", name, "--", "python3", "-c", probe],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, start_new_session=True)
    except OSError:
        return None
    output = bytearray()
    deadline = time.monotonic() + 15
    completed = False
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([child.stdout], [], [], max(0, deadline - time.monotonic()))
            if not readable:
                break
            chunk = os.read(child.stdout.fileno(), 257 - len(output))
            if not chunk:
                remaining = max(0, deadline - time.monotonic())
                status = child.wait(timeout=remaining)
                completed = True
                return status, bytes(output)
            output.extend(chunk)
            if len(output) > 256:
                break
    except (OSError, subprocess.TimeoutExpired):
        pass
    finally:
        if not completed:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
        child.stdout.close()
    return None


def failure_snapshot(command, name):
    # Only fixed labels and bounded counts reach the public CI log. Guest logs
    # can include agent output or secrets, so do not print journal text.
    probe = r'''
import subprocess

def run(args):
    try:
        return subprocess.run(args, capture_output=True, text=True, timeout=4)
    except (OSError, subprocess.TimeoutExpired):
        return None

service = run(["systemctl", "show", "ssh.service", "--property=ActiveState,Result"])
fields = dict(line.split("=", 1) for line in service.stdout.splitlines() if "=" in line) if service and service.returncode == 0 else {}
active = fields.get("ActiveState")
result = fields.get("Result")
active = active if active in ("active", "inactive", "failed") else "unknown"
result = result if result in ("success", "exit-code", "signal", "timeout") else "unknown"

def count(args):
    found = run(["sudo", "-n", "journalctl", "-q", "-b", "--since=-3min",
                 "--no-pager", "--output=cat", "-n", "100", *args])
    if found and found.returncode in (0, 1) and not found.stderr:
        return str(min(len(found.stdout.splitlines()), 999))
    return "unknown"

oom = count(["-k", "-g", "Out of memory|Killed process"])
ssh_warnings = count(["-u", "ssh.service", "-p", "warning"])
print(f"sshd_active={active} sshd_result={result} kernel_oom_lines={oom} sshd_warning_lines={ssh_warnings}")
'''
    result = bounded_guest_probe(command, name, probe)
    if result is not None:
        code, output = result
        line = output.decode("ascii", errors="ignore").strip()
        if code == 0 and re.fullmatch(r"sshd_active=(active|inactive|failed|unknown) "
                        r"sshd_result=(success|exit-code|signal|timeout|unknown) "
                        r"kernel_oom_lines=(unknown|[0-9]{1,3}) "
                        r"sshd_warning_lines=(unknown|[0-9]{1,3})", line):
            print(f"EDITOR_FAILURE_SNAPSHOT guest_ssh=reachable {line}", flush=True)
            return
    print("EDITOR_FAILURE_SNAPSHOT guest_ssh=unavailable", flush=True)


def launch_state_probe(command, name):
    # Only allowlisted systemd state and marker presence reach the CI log.
    probe = r'''
from pathlib import Path
import subprocess

roots = [p for p in Path("/tmp").glob("coop-we-*")
         if p.is_dir() and (p / "config.yaml").exists()]
if len(roots) != 1:
    print("dir=" + ("none" if not roots else "multiple") +
          " marker=unknown loop=unknown reported=unknown load=unknown active=unknown"
          " shell=unknown state=unknown wait=unknown children=unknown"
          " launch=unknown curl=unknown timeout=unknown other=unknown")
else:
    root = roots[0]
    try:
        shown = subprocess.run(["systemctl", "show", root.name + ".service",
                                "--property=LoadState,ActiveState"],
                               capture_output=True, text=True, timeout=4)
        fields = dict(line.split("=", 1) for line in shown.stdout.splitlines()
                      if "=" in line) if shown.returncode == 0 else {}
    except (OSError, subprocess.TimeoutExpired):
        fields = {}
    load = fields.get("LoadState")
    active = fields.get("ActiveState")
    load = load if load in ("loaded", "not-found", "masked", "error") else "unknown"
    active = active if active in ("active", "activating", "inactive", "failed", "deactivating") else "unknown"
    marker = "present" if (root / "launched").is_file() else "missing"
    loop = "present" if (root / "launch-loop").is_file() else "missing"
    reported = "present" if (root / "launch-reported").is_file() else "missing"
    needle = f"session_socket={root}/session.sock".encode()
    shells = []
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            with (proc / "cmdline").open("rb") as command_line:
                args = command_line.read(4096).split(b"\0")
            if args and args[0].endswith(b"bash") and needle in b"\0".join(args):
                shells.append(proc)
                if len(shells) > 1:
                    break
        except OSError:
            pass
    shell = "one" if len(shells) == 1 else ("none" if not shells else "multiple")
    state = wait = children = launch = curl = timeout = other = "unknown"
    if shell == "one":
        try:
            state = (shells[0] / "stat").read_text().rsplit(")", 1)[1].split()[0]
            state = state if state in ("R", "S", "D", "T", "Z", "I") else "unknown"
            wchan = (shells[0] / "wchan").read_text().strip()
            wait = next((label for text, label in (("pipe", "pipe"), ("poll", "poll"),
                         ("select", "select"), ("futex", "futex"), ("wait", "wait"))
                         if text in wchan), "other")
        except (OSError, IndexError):
            state = wait = "unknown"
        try:
            with (shells[0] / "task" / shells[0].name / "children").open("rb") as child_file:
                child_ids = child_file.read(513)
            if len(child_ids) <= 512:
                ids = child_ids.split()
                if len(ids) <= 8 and all(pid.isdigit() for pid in ids):
                    kinds = []
                    for pid in ids:
                        with (Path("/proc") / pid.decode("ascii") / "comm").open("rb") as child_file:
                            kinds.append(child_file.read(32).strip())
                    children = str(len(ids)) if len(ids) < 3 else "3plus"
                    launch = "yes" if any(k in (b"setsid", b"sudo", b"systemd-run") for k in kinds) else "no"
                    curl = "yes" if b"curl" in kinds else "no"
                    timeout = "yes" if b"timeout" in kinds else "no"
                    other = "yes" if any(k not in (b"setsid", b"sudo", b"systemd-run",
                                                   b"curl", b"timeout") for k in kinds) else "no"
        except OSError:
            pass
    print(f"dir=one marker={marker} loop={loop} reported={reported} load={load} active={active} "
          f"shell={shell} state={state} wait={wait} children={children} "
          f"launch={launch} curl={curl} timeout={timeout} other={other}")
'''
    result = bounded_guest_probe(command, name, probe)
    if result is not None:
        code, output = result
        line = output.decode("ascii", errors="ignore").strip()
        if code == 0 and re.fullmatch(
            r"dir=(none|one|multiple) marker=(unknown|missing|present) "
            r"loop=(unknown|missing|present) reported=(unknown|missing|present) "
            r"load=(unknown|loaded|not-found|masked|error) "
            r"active=(unknown|active|activating|inactive|failed|deactivating) "
            r"shell=(unknown|none|one|multiple) state=(unknown|R|S|D|T|Z|I) "
            r"wait=(unknown|pipe|poll|select|futex|wait|other) "
            r"children=(unknown|0|1|2|3plus) "
            r"launch=(unknown|yes|no) curl=(unknown|yes|no) "
            r"timeout=(unknown|yes|no) other=(unknown|yes|no)", line
        ):
            print(f"EDITOR_LAUNCH_PROBE guest_ssh=reachable {line}", flush=True)
            return
    print("EDITOR_LAUNCH_PROBE guest_ssh=unavailable", flush=True)


def restart_vm(command, name):
    # Lima allows up to 300 seconds for SSH readiness on a cold restart.
    # Leave a small margin for coop's cleanup and report only fixed categories.
    try:
        subprocess.run(command + ["start", name, "--no-agents"], check=True,
                       capture_output=True, timeout=330)
    except subprocess.CalledProcessError as error:
        stderr = error.stderr.decode("utf-8", errors="replace") if error.stderr else ""
        if "Timeout waiting for Lima to assign SSH port" in stderr:
            reason = "ssh_port_timeout"
        elif "Timeout waiting for SSH auth on port" in stderr:
            reason = "ssh_auth_timeout"
        elif "limactl start failed" in stderr:
            reason = "limactl_start_failed"
        else:
            reason = "other_failure"
        print(f"EDITOR_RESTART_FAILURE reason={reason}", flush=True)
        raise
    except subprocess.TimeoutExpired:
        print("EDITOR_RESTART_FAILURE reason=host_command_timeout", flush=True)
        raise


def vm(binary, name, config):
    command = [binary] + (["--config", config] if config else [])
    default_launch = command + ["editor", name, "--web"]
    project = "/workspace/editor app #? & %h ${USER} ü"
    other_project = "/workspace/editor second"
    subprocess.run(command + ["exec", name, "--", "mkdir", "-p", "--", project, other_project],
                   check=True, capture_output=True, timeout=30)
    launch = default_launch + ["--project", project]
    # A busy host port must fail before publishing an editor URL.
    with socket.socket() as busy:
        busy.bind(("127.0.0.1", 0))
        busy.listen()
        failed = subprocess.run(launch + ["--port", str(busy.getsockname()[1])],
                                capture_output=True, text=True, timeout=30)
        assert failed.returncode != 0 and "Open: " not in failed.stderr
        assert "Cannot bind editor SSH forward" in failed.stderr
        assert "editor.log" not in failed.stderr
    download_failure(command, name, launch)
    with editor(launch, launch_probe=lambda: launch_state_probe(command, name)) as (child, url, path):
        assert parse_qs(urlsplit(url).query) == {"folder": [project]}
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
        assert request(url, f"/?{urlsplit(url).query}", headers={"Cookie": cookie})[0] == 200
        guest_config, cgroup = inspect_guest_editor(command, name, url, project)
        # Reject invalid projects on attach without advertising a URL or
        # revoking the owner's existing session. '=' passes --help as a value.
        for invalid_project, message in [
            (f"{project}/missing-{secrets.token_hex(12)}", "Guest project directory is missing"),
            ("--help", "--project must be an absolute guest path"),
        ]:
            failed = subprocess.run(default_launch + [f"--project={invalid_project}"],
                                    capture_output=True, text=True, timeout=30)
            assert failed.returncode != 0
            assert message in failed.stderr, failed.stderr
            assert "Open: " not in failed.stdout + failed.stderr
            assert child.poll() is None and path.read_text() == password
            assert request(url, "/healthz", headers={"Cookie": cookie})[0] == 200
        # Both invocations remain in the foreground. Ctrl+C in the attached
        # terminal revokes the shared session and releases the original owner.
        with editor(default_launch + ["--project", other_project]) as (attached, attached_url, path):
            assert urlsplit(attached_url).netloc == urlsplit(url).netloc and attached.poll() is None
            assert parse_qs(urlsplit(attached_url).query) == {"folder": [other_project]}
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
        assert_guest_editor_stopped(command, name, guest_config, cgroup)
    subprocess.run(command + ["stop", name], check=True, capture_output=True, timeout=60)
    restart_vm(command, name)
    with editor(launch) as (child, new_url, path):
        assert new_url != url
        assert request(new_url, "/", headers={"Cookie": cookie})[0] == 302
        assert path.read_text() != password
        cookie = login(new_url, path.read_text())
        assert request(new_url, "/healthz", headers={"Cookie": cookie})[0] == 200
        subprocess.run(command + ["stop", name], check=True, capture_output=True, timeout=60)
        assert child.wait(timeout=15) == 0
    restart_vm(command, name)
    owner_death_before_ready(command, name, launch, project)
    # Abrupt owner death closes stdin; the cooperative guest stops its service
    # and exits SSH. A retry cleans stale private state and starts afresh.
    with editor(launch) as (child, dead_url, path):
        dead_config, dead_cgroup = inspect_guest_editor(command, name, dead_url, project)
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
        assert_guest_editor_stopped(command, name, dead_config, dead_cgroup)
    with editor(default_launch) as (child, retry_url, path):
        assert parse_qs(urlsplit(retry_url).query) == {"folder": ["/workspace"]}
        assert retry_url != dead_url
        cookie = login(retry_url, path.read_text())
        assert request(retry_url, "/?folder=/workspace", headers={"Cookie": cookie})[0] == 200
    print("PASS VM browser editor: custom/default projects, missing/option-like project rejection on attach, download failure diagnostics, port collision, owner death before/after readiness and retry, native password auth, private config, repeat launch, stop, restart, session isolation")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary")
    parser.add_argument("--name")
    parser.add_argument("--config")
    args = parser.parse_args()
    if args.binary and args.name:
        try:
            vm(args.binary, args.name, args.config)
        except Exception:
            command = [args.binary] + (["--config", args.config] if args.config else [])
            failure_snapshot(command, args.name)
            raise
    else:
        parser.error("supply --binary and --name")
