#!/usr/bin/env python3
"""Host-only regression tests for the Docker Linux test runner."""

import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parent.parent
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("linux_unit", ROOT / "tests/run-linux-unit.py")
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)
INVOKE_RUNNER = '''
import importlib.util, os, pathlib, signal, sys
path = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("linux_unit", path)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
runner.host_platform.system = lambda: "Darwin"
runner.host_platform.machine = lambda: os.uname().machine
signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
sys.exit(runner.run(sys.argv[2:]))
'''


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "tests").mkdir()
        for name in ("run-linux-unit.py", "linux-unit.Dockerfile"):
            (self.root / "tests" / name).write_bytes((ROOT / "tests" / name).read_bytes())
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.94.0"\n')
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        (self.root / "Cargo.toml").write_text('[package]\nname = "sample"\n')
        (self.root / "changed.rs").write_text("original\n")
        (self.root / "link").symlink_to("changed.rs")
        (self.root / "executable.sh").write_text("#!/bin/sh\n")
        (self.root / "executable.sh").chmod(0o755)
        (self.root / ".env").write_text("PRIVATE\n")
        (self.root / "credentials.json").write_text("PRIVATE\n")
        subprocess.run(["git", "-C", str(self.root), "add", "Cargo.toml", "changed.rs",
                        "link", "executable.sh"], check=True)
        (self.root / "changed.rs").write_text("working tree edit\n")
        os.utime(self.root / "changed.rs", (1_600_000_000, 1_600_000_000))
        (self.root / "target").mkdir()
        (self.root / "target/local").write_text("PRIVATE\n")
        (self.root / "ignored-secret").write_text("PRIVATE\n")
        (self.root / ".claude").mkdir()
        (self.root / ".claude/settings.local.json").write_text("PRIVATE\n")
        (self.root / ".gitignore").write_text(
            "ignored-secret\n.claude/settings.local.json\n.env\ncredentials.json\n")
        self.log = self.root / "docker-log"
        selected, _ = RUNNER.selected_platform("Darwin", RUNNER.host_machine())
        (self.root / "expected-arch").write_text(
            "aarch64" if selected == "linux/arm64" else "x86_64")
        bindir = self.root / "bin"
        bindir.mkdir()
        fake = bindir / "docker"
        fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).resolve().parent.parent
with (root / "docker-log").open("a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[1] == "info":
    (root / "docker-env").write_text(json.dumps(sorted(os.environ)))
if sys.argv[1] == "build":
    sys.stdin.buffer.read()
if sys.argv[1] == "exec" and "tar" in sys.argv:
    with (root / "staged.tar").open("wb") as out:
        out.write(sys.stdin.buffer.read())
if sys.argv[1] == "exec" and "uname" in sys.argv:
    arch_file = root / ("fake-arch" if (root / "fake-arch").exists() else "expected-arch")
    arch = arch_file.read_text()
    print(arch)
if sys.argv[1] == "exec" and "rustc" in sys.argv:
    print((root / "fake-rustc").read_text() if (root / "fake-rustc").exists() else "rustc 1.94.0 (test)")
if sys.argv[1] == "info" and (root / "fake-down").exists():
    sys.exit(1)
if sys.argv[1] == "create" and (root / "fake-create-fail").exists():
    sys.exit(7)
if sys.argv[1] == "rm" and (root / "fake-rm-fail").exists():
    sys.exit(7)
if sys.argv[1] == "exec" and "flock" in sys.argv and (root / "fake-cargo-fail").exists():
    sys.exit(42)
if sys.argv[1] == "exec" and "flock" in sys.argv and (root / "fake-cargo-sleep").exists():
    import time
    time.sleep(30)
''')
        fake.chmod(0o755)
        self.env = {**os.environ, "PATH": str(bindir) + os.pathsep + os.environ["PATH"]}

    def invoke(self, *args, env=None):
        return subprocess.run([sys.executable, "-B", "-c", INVOKE_RUNNER,
                               str(self.root / "tests/run-linux-unit.py"), *args],
                              env=env or self.env, capture_output=True, text=True, timeout=30)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_architecture_and_platform_validation(self):
        self.assertEqual(RUNNER.selected_platform("Darwin", "arm64"),
                         ("linux/arm64", "linux/arm64"))
        self.assertEqual(RUNNER.selected_platform("Darwin", "x86_64"),
                         ("linux/amd64", "linux/amd64"))
        with self.assertRaisesRegex(ValueError, "on Linux use cargo test --workspace"):
            RUNNER.selected_platform("Linux", "aarch64")
        for bad in ("linux/386", "linux/amd64;true", "arm64", ""):
            with self.assertRaisesRegex(ValueError, "--platform must"):
                RUNNER.selected_platform("Darwin", "arm64", bad)
        with (mock.patch.object(RUNNER.host_platform, "system", return_value="Darwin"),
              mock.patch.object(RUNNER.host_platform, "machine", return_value="x86_64"),
              mock.patch.object(RUNNER.subprocess, "run",
                                return_value=subprocess.CompletedProcess([], 0, "1\n", ""))):
            self.assertEqual(RUNNER.host_machine(), "arm64")

    def test_source_manifest_uses_working_tree_without_private_files(self):
        archive = self.root / "source.tar"
        RUNNER.source_archive(self.root, archive)
        with tarfile.open(archive) as source:
            names = set(source.getnames())
            self.assertIn("changed.rs", names)
            self.assertEqual(source.extractfile("changed.rs").read(), b"working tree edit\n")
            self.assertGreater(source.getmember("changed.rs").mtime, 1_600_000_000)
            self.assertTrue(source.getmember("link").issym())
            self.assertEqual(source.getmember("executable.sh").mode & 0o111, 0o111)
            self.assertFalse(any(name.startswith(".git") or name.startswith("target") or
                                 "credential" in name or name == ".env" or
                                 name == "ignored-secret" or name.startswith(".claude/")
                                 for name in names))
        self.assertIn(Path("src/secret_store.rs"), RUNNER.source_paths(ROOT))

    def test_tracked_exclusions_fail_loudly(self):
        tracked = self.root / "linux_regression.rs"
        tracked.write_text("tracked source\n")
        subprocess.run(["git", "-C", str(self.root), "add", "linux_regression.rs"], check=True)
        with (self.root / ".gitignore").open("a") as ignore:
            ignore.write("linux_regression.rs\n")
        with self.assertRaisesRegex(ValueError, "refusing to omit tracked.*linux_regression"):
            RUNNER.source_paths(self.root)
        subprocess.run(["git", "-C", str(self.root), "rm", "--cached", "linux_regression.rs"],
                       check=True, stdout=subprocess.DEVNULL)
        subprocess.run(["git", "-C", str(self.root), "add", "-f", ".env"], check=True)
        with self.assertRaisesRegex(ValueError, r"refusing to omit tracked.*\.env"):
            RUNNER.source_paths(self.root)

    def test_forwarding_status_and_cleanup(self):
        result = self.invoke("-p", "coop; touch /tmp/injected", "--test", "firecracker_socket")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Container uname -m:", result.stdout)
        calls = self.calls()
        cargo = next(call for call in calls if call[0] == "exec" and "flock" in call)
        self.assertEqual(cargo[-5:], ["sh", "-p", "coop; touch /tmp/injected",
                                       "--test", "firecracker_socket"])
        self.assertIn('cargo clean --workspace --quiet && exec cargo test "$@"', cargo)
        self.assertIn("/workspace/target/.coop-linux-unit.lock", cargo)
        self.assertEqual(calls[-1][:2], ["rm", "--force"])
        create = next(call for call in calls if call[0] == "create")
        self.assertIn("--volume", create)
        for forbidden in ("--privileged", "--network", "--device", "--env"):
            self.assertNotIn(forbidden, create)
        with tarfile.open(self.root / "staged.tar") as source:
            self.assertEqual(source.extractfile("changed.rs").read(), b"working tree edit\n")
        self.log.unlink()
        (self.root / "fake-cargo-fail").touch()
        failed = self.invoke()
        (self.root / "fake-cargo-fail").unlink()
        self.assertEqual(failed.returncode, 42)
        self.assertEqual(self.calls()[-1][:2], ["rm", "--force"])
        self.log.unlink()
        (self.root / "fake-create-fail").touch()
        create_failed = self.invoke()
        (self.root / "fake-create-fail").unlink()
        self.assertNotEqual(create_failed.returncode, 0)
        self.assertEqual(self.calls()[-1][:2], ["rm", "--force"])
        self.log.unlink()
        (self.root / "fake-arch").write_text("unexpected")
        wrong_arch = self.invoke()
        self.assertIn("container architecture is unexpected", wrong_arch.stderr)
        self.assertEqual(self.calls()[-1][:2], ["rm", "--force"])
        (self.root / "fake-arch").unlink()
        self.log.unlink()
        (self.root / "fake-rustc").write_text("rustc 1.93.0 (test)")
        wrong_rustc = self.invoke()
        self.assertIn("container Rust is rustc 1.93.0", wrong_rustc.stderr)
        self.assertEqual(self.calls()[-1][:2], ["rm", "--force"])

    def test_cleanup_failure_is_reported(self):
        (self.root / "fake-rm-fail").touch()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not remove container", result.stderr)
        self.assertIn("docker rm --force", result.stderr)
        (self.root / "fake-cargo-fail").touch()
        failed = self.invoke()
        self.assertEqual(failed.returncode, 42)
        self.assertIn("could not remove container", failed.stderr)

    def test_default_workspace_and_cargo_harness_arguments(self):
        result = self.invoke("--", "--nocapture")
        self.assertEqual(result.returncode, 0, result.stderr)
        cargo = next(call for call in self.calls() if call[0] == "exec" and "flock" in call)
        self.assertEqual(cargo[-4:], ["sh", "--workspace", "--", "--nocapture"])

    def test_docker_errors(self):
        empty = self.root / "empty-bin"
        empty.mkdir()
        absent = self.invoke(env={**self.env, "PATH": str(empty)})
        self.assertIn("Docker CLI is missing", absent.stderr)
        self.assertNotEqual(absent.returncode, 0)
        (self.root / "fake-down").touch()
        down = self.invoke()
        self.assertIn("daemon is unreachable", down.stderr)
        self.assertNotEqual(down.returncode, 0)

    def test_host_secrets_are_not_passed_to_docker_cli(self):
        result = self.invoke(env={**self.env, "OPENAI_API_KEY": "example-secret"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("OPENAI_API_KEY", json.loads((self.root / "docker-env").read_text()))

    def test_interrupt_cleans_container(self):
        (self.root / "fake-cargo-sleep").touch()
        process = subprocess.Popen(
            [sys.executable, "-B", "-c", INVOKE_RUNNER,
             str(self.root / "tests/run-linux-unit.py")],
            env=self.env,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            for _ in range(100):
                if self.log.exists() and any(call[0] == "exec" and "flock" in call
                                             for call in self.calls()):
                    break
                time.sleep(0.02)
            else:
                self.fail("runner never reached cargo")
            process.send_signal(signal.SIGTERM)
            self.assertNotEqual(process.wait(timeout=10), 0)
            self.assertEqual(self.calls()[-1][:2], ["rm", "--force"])
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            process.stderr.close()

    def test_cache_names_and_toolchain_sync(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        create = next(call for call in self.calls() if call[0] == "create")
        cargo = next(call for call in self.calls() if call[0] == "exec" and "flock" in call)
        self.assertEqual(cargo[-2:], ["sh", "--workspace"])
        version = RUNNER.toolchain_version(self.root)
        chosen, _ = RUNNER.selected_platform("Darwin", RUNNER.host_machine())
        arch = chosen.split("/")[1]
        mounts = [create[i + 1] for i, item in enumerate(create[:-1]) if item == "--volume"]
        self.assertEqual(len(mounts), 3)
        self.assertTrue(all(f"-{version}-{arch}:" in mount for mount in mounts))
        dockerfile = (ROOT / "tests/linux-unit.Dockerfile").read_text()
        self.assertIn(f"FROM rust:{version}-bookworm@sha256:", dockerfile)
        self.assertIn(f"ENV RUSTUP_TOOLCHAIN={version}", dockerfile)
        self.assertIn("ENV COOP_LINUX_UNIT_TEST_CONTAINER=1", dockerfile)

    def test_help_names_entry_point_and_focused_arguments(self):
        help_text = self.invoke("--help")
        self.assertEqual(help_text.returncode, 0)
        self.assertIn("./tests/run-linux-unit.sh", help_text.stdout)
        self.assertIn("[CARGO TEST ARGS...]", help_text.stdout)
        self.assertIn("-p coop --test firecracker_socket", help_text.stdout)

    def test_ci_package_contract(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        dockerfile = (ROOT / "tests/linux-unit.Dockerfile").read_text()
        packages = set()
        for match in re.finditer(r"apt-get install -y ([^\n]+)", ci.replace("\\\n", " ")):
            packages.update(match.group(1).split())
        self.assertTrue(packages)
        installed = re.search(r"apt-get install -y --no-install-recommends (.*?) && rm",
                              dockerfile, re.S)
        self.assertIsNotNone(installed)
        docker_packages = set(installed.group(1).replace("\\", "").split())
        self.assertEqual(docker_packages, packages | {"sudo", "python3"})


if __name__ == "__main__":
    unittest.main()
