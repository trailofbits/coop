#!/usr/bin/env python3
"""Run tracked working-tree sources under native Linux Docker."""

import argparse
import os
from pathlib import Path
import platform as host_platform
import re
from shutil import which
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


ROOT = Path(__file__).resolve().parent.parent
DOCKERFILE = ROOT / "tests/linux-unit.Dockerfile"
PRIVATE_PARTS = {".git", "target", ".ssh", ".aws", ".config", ".docker",
                 ".kube", ".netrc", ".npmrc", ".pypirc", "auth.json", "secrets"}
PRIVATE_PREFIXES = (".env", "credentials", "id_")
PRIVATE_SUFFIXES = (".pem", ".key", ".p12", ".pfx")
DOCKER_ENV_KEYS = ("PATH", "HOME", "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG",
                   "DOCKER_CERT_PATH", "DOCKER_TLS_VERIFY")


def limited_env(keys):
    return {key: os.environ[key] for key in keys if key in os.environ}


def host_machine():
    machine = host_platform.machine()
    if host_platform.system() == "Darwin" and machine == "x86_64":
        # Rosetta reports an x86_64 process even on Apple Silicon hardware.
        translated = subprocess.run(["/usr/sbin/sysctl", "-n", "sysctl.proc_translated"],
                                    capture_output=True, text=True, check=False,
                                    env=limited_env(("PATH", "HOME")))
        if translated.returncode == 0 and translated.stdout.strip() == "1":
            return "arm64"
    return machine


def selected_platform(system, machine, override=None):
    architectures = {"arm64": "linux/arm64", "aarch64": "linux/arm64",
                     "x86_64": "linux/amd64", "amd64": "linux/amd64"}
    if system != "Darwin":
        raise ValueError("this runner is for macOS; on Linux use cargo test --workspace")
    if machine not in architectures:
        raise ValueError(f"unsupported host architecture: {system} {machine}")
    native = architectures[machine]
    if override is not None and override not in ("linux/arm64", "linux/amd64"):
        raise ValueError("--platform must be linux/arm64 or linux/amd64")
    return override or native, native


def toolchain_version(root):
    text = (root / "rust-toolchain.toml").read_text()
    match = re.search(r'^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$', text, re.M)
    if not match:
        raise ValueError("rust-toolchain.toml must pin a numeric Rust version")
    return match.group(1)


def source_paths(root):
    result = subprocess.run(["git", "ls-files", "--cached", "-z", "--full-name"],
                            cwd=root, check=True, capture_output=True,
                            env=limited_env(("PATH", "HOME")))
    ignored = subprocess.run(["git", "check-ignore", "--no-index", "-z", "--stdin"],
                             input=result.stdout, cwd=root, capture_output=True,
                             env=limited_env(("PATH", "HOME")))
    if ignored.returncode not in (0, 1):
        raise subprocess.CalledProcessError(ignored.returncode, ignored.args, ignored.stdout,
                                            ignored.stderr)
    ignored_paths = set(ignored.stdout.split(b"\0"))
    paths = []
    excluded = []
    for raw in result.stdout.split(b"\0"):
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        parts = relative.parts
        if relative.is_absolute() or ".." in parts:
            raise ValueError(f"tracked path escapes the repository: {relative}")
        path = root
        for part in parts[:-1]:
            path = path / part
            if path.is_symlink():
                raise ValueError(f"tracked path has symlink parent: {relative}")
        full = root / relative
        if not full.is_file() and not full.is_symlink():
            if not full.exists():  # A tracked deletion is part of the current tree.
                continue
            raise ValueError(f"tracked path is not a file or symlink: {relative}")
        if (raw in ignored_paths or
                any(part in PRIVATE_PARTS or part.startswith(PRIVATE_PREFIXES)
                    or part.endswith(PRIVATE_SUFFIXES) for part in parts)):
            excluded.append(relative)
            continue
        paths.append(relative)
    if excluded:
        names = ", ".join(str(path) for path in excluded)
        raise ValueError(
            f"refusing to omit tracked private or ignored paths: {names}; "
            "remove them from the Git index or rename them")
    return paths


def source_archive(root, destination):
    paths = source_paths(root)
    if not paths:
        raise ValueError("no tracked source files found")
    with tarfile.open(destination, "w") as archive:
        for relative in paths:
            full = root / relative
            info = archive.gettarinfo(full, arcname=str(relative))
            # Reused target volumes must see each source as newer than the last build.
            info.mtime = time.time()
            if info.isfile():
                with full.open("rb") as source:
                    archive.addfile(info, source)
            else:
                archive.addfile(info)
    return paths


def docker(*args, input_bytes=None, check=True, **kwargs):
    return subprocess.run(["docker", *args], input=input_bytes, check=check,
                          env=limited_env(DOCKER_ENV_KEYS), **kwargs)


def docker_test(*args):
    # Stop the CLI promptly on interruption so container removal can stop its test process.
    with subprocess.Popen(["docker", *args], env=limited_env(DOCKER_ENV_KEYS)) as process:
        try:
            return process.wait()
        except BaseException:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            raise


def run(argv=None):
    parser = argparse.ArgumentParser(
        prog="./tests/run-linux-unit.sh",
        usage="%(prog)s [--platform {linux/arm64,linux/amd64}] [CARGO TEST ARGS...]",
        description="Build and run Linux Rust tests on tracked working-tree sources in Docker.",
        epilog=("Optional macOS pre-push check for Linux-specific changes. On native Linux, use "
                "cargo test --workspace.\n"
                "Prerequisites: macOS arm64 or x86_64, Python 3, Git, and a running "
                "Docker-compatible daemon.\n"
                "Default: cargo test --workspace. Apple Silicon uses native linux/arm64; Intel "
                "uses native linux/amd64. --platform may use emulation.\n"
                "Examples:\n  ./tests/run-linux-unit.sh\n"
                "  ./tests/run-linux-unit.sh -p coop --test firecracker_socket\n"
                "Early coverage: Linux cfg, /proc, processes, signals, Unix sockets, modes, "
                "symlinks, permissions, and Firecracker host-side logic available in a container.\n"
                "Limitations: no KVM/Firecracker boot, TAP/bridge/iptables/forwarding/routing, "
                "systemd, host sudoers, macOS/Lima, foreign-architecture runtime proof, or full "
                "lifecycle/guest-visible/install/update/release coverage.\n"
                "This does not replace either integration suite: ./tests/run-integration.sh "
                "and ./tests/run-integration.sh --remote user@host."),
        formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--platform", help="linux/arm64 or linux/amd64 (foreign architecture uses emulation)")
    args, cargo_args = parser.parse_known_args(argv)
    if not cargo_args or cargo_args[0] == "--":
        cargo_args.insert(0, "--workspace")
    try:
        chosen, native = selected_platform(host_platform.system(), host_machine(), args.platform)
        version = toolchain_version(ROOT)
        dockerfile = DOCKERFILE.read_text()
        if not re.search(rf"^FROM rust:{re.escape(version)}-bookworm@sha256:[0-9a-f]{{64}}$",
                         dockerfile, re.M):
            raise ValueError("Dockerfile Rust tag or digest differs from rust-toolchain.toml")
        if f"ENV RUSTUP_TOOLCHAIN={version}" not in dockerfile:
            raise ValueError("Dockerfile Rust override differs from rust-toolchain.toml")
        if not which("docker"):
            raise ValueError(
                "Docker CLI is missing; install Docker Desktop, Colima, or another "
                "Docker-compatible runtime")
        if docker("info", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                  check=False).returncode:
            raise ValueError(
                "Docker daemon is unreachable; start a Docker-compatible daemon and retry")
        arch = chosen.split("/")[1]
        image = f"coop-linux-unit:{version}-{arch}"
        name = f"coop-linux-unit-{uuid.uuid4().hex}"
        print(f"Linux test container: {chosen}; Rust {version}", flush=True)
        if chosen != native:
            print(f"WARNING: {chosen} differs from native {native}; tests may run under emulation", file=sys.stderr)
        print("Early warning only: run both real Lima and Firecracker integration gates when applicable.", flush=True)
        with tempfile.TemporaryDirectory(prefix="coop-linux-unit-") as temporary:
            archive_path = Path(temporary) / "sources.tar"
            source_archive(ROOT, archive_path)
            docker("build", "--platform", chosen, "--tag", image, "-",
                   input_bytes=dockerfile.encode())
            volumes = [f"coop-linux-unit-registry-{version}-{arch}:/usr/local/cargo/registry",
                       f"coop-linux-unit-git-{version}-{arch}:/usr/local/cargo/git",
                       f"coop-linux-unit-target-{version}-{arch}:/workspace/target"]
            test_status = None
            try:
                command = ["create", "--platform", chosen, "--name", name]
                for volume in volumes:
                    command.extend(["--volume", volume])
                command.extend([image, "sleep", "infinity"])
                docker(*command, stdout=subprocess.DEVNULL)
                docker("start", name, stdout=subprocess.DEVNULL)
                actual_arch = docker("exec", name, "uname", "-m", capture_output=True,
                                     text=True).stdout.strip()
                expected_arch = {"arm64": "aarch64", "amd64": "x86_64"}[arch]
                if actual_arch != expected_arch:
                    raise ValueError(f"container architecture is {actual_arch}, expected {expected_arch}")
                print(f"Container uname -m: {actual_arch}", flush=True)
                rustc = docker("exec", name, "rustc", "--version", capture_output=True,
                               text=True).stdout.strip()
                if not rustc.startswith(f"rustc {version} "):
                    raise ValueError(f"container Rust is {rustc}, expected {version}")
                with archive_path.open("rb") as source:
                    subprocess.run(["docker", "exec", "-i", "--user", "coop", name,
                                    "tar", "-x", "-C", "/workspace"], stdin=source, check=True,
                                   env=limited_env(DOCKER_ENV_KEYS))
                # A volume-wide lock keeps clean/build/test together across
                # concurrent invocations, including from separate worktrees.
                # Cleaning workspace members prevents stale mtime fingerprints
                # from reusing binaries built from a different source snapshot.
                shell = 'cargo clean --workspace --quiet && exec cargo test "$@"'
                test_status = docker_test("exec", "--user", "coop", "--workdir", "/workspace", name,
                                          "flock", "-x", "/workspace/target/.coop-linux-unit.lock",
                                          "/bin/sh", "-c", shell, "sh", *cargo_args)
            finally:
                try:
                    cleanup = docker("rm", "--force", name, check=False,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                    cleanup_error = cleanup.returncode != 0
                except OSError:
                    cleanup_error = True
                if cleanup_error:
                    message = f"could not remove container {name}; run docker rm --force {name}"
                    if test_status == 0:
                        raise ValueError(message)
                    print(f"run-linux-unit: {message}", file=sys.stderr)
            return test_status
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"run-linux-unit: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        sys.exit(run())
    except KeyboardInterrupt:
        sys.exit(130)
