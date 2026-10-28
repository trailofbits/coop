#!/usr/bin/env python3
"""Check HEAD-built Homebrew guards; invoked by integration-update.sh."""

import json
import os
from pathlib import Path
import pty
import shutil
import subprocess
import sys
import tempfile


def check_guards(ordinary, homebrew):
    with tempfile.TemporaryDirectory(prefix="coop-homebrew-") as directory:
        root = Path(directory)
        # Block privilege escalation if the uninstall guard regresses.
        (root / "sudo").symlink_to("/usr/bin/false")
        # Exclude CI, COOP_NO_UPDATE_CHECK, COMPLETE, and host credentials.
        env = {
            "PATH": f"{root}:{os.environ['PATH']}",
            "HOME": str(root),
            "XDG_STATE_HOME": str(root / "state"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "COOP_UPDATE_TEST_API_BASE_URL": os.environ["COOP_UPDATE_TEST_API_BASE_URL"],
        }
        config = root / ".coop" / "config.toml"
        config.parent.mkdir()
        # Linux network cleanup resolves sudo independently of PATH.
        config.write_text(
            '[network.host_tools]\nsudo = "/usr/bin/false"\n'
            '[updates]\nmode = "notify"\ncheck_interval_hours = 0\n'
        )
        sentinel = config.parent / "sentinel"
        sentinel.write_text("preserve user data\n")
        state_root = (
            root / "Library" / "Application Support"
            if sys.platform == "darwin" else Path(env["XDG_STATE_HOME"])
        )
        state = state_root / "coop" / "update-check.json"
        state.parent.mkdir(parents=True)
        initial = b'{"last_checked_at": 0, "latest_known_version": "v9999.0.0"}\n'
        binary = root / "coop"

        controller_fd, terminal_fd = pty.openpty()
        with os.fdopen(controller_fd, "rb"), os.fdopen(terminal_fd, "rb") as terminal:
            assert terminal.isatty(), "fixture requires interactive stdin"

            def run(source, *args):
                # Relocate every invocation so a broken uninstall guard can
                # only remove a disposable copy, never the build artifact.
                shutil.copy2(source, binary)
                return subprocess.run(
                    [str(binary), "--config", str(config), *args],
                    stdin=terminal, capture_output=True, text=True,
                    timeout=15, cwd=root, env=env,
                )

            for source in (ordinary, homebrew):
                state.write_bytes(initial)
                result = run(source, "profiles", "list")
                assert result.returncode == 0, result.stderr
                if source == homebrew:
                    assert "A newer coop" not in result.stderr, result.stderr
                    assert state.read_bytes() == initial, "Homebrew scheduled a check"
                else:
                    assert "A newer coop (9999.0.0)" in result.stderr, result.stderr
                    # The timestamp is written before spawning the thread;
                    # no wait for the HTTP request is needed.
                    assert json.loads(state.read_bytes())["last_checked_at"] > 0, (
                        "ordinary build did not schedule a check"
                    )

            preserved = {path: path.read_bytes() for path in (binary, config, sentinel, state)}
            for args, hint in (
                (("update", "--check"), "brew upgrade coop"),
                (("uninstall", "--yes", "--purge"), "brew uninstall coop"),
            ):
                result = run(homebrew, *args)
                assert result.returncode != 0, f"{args} unexpectedly succeeded"
                assert "Homebrew-managed" in result.stderr, result.stderr
                assert hint in result.stderr, result.stderr
                for path, content in preserved.items():
                    assert path.is_file() and path.read_bytes() == content, (
                        f"{args} changed or removed {path}"
                    )


if __name__ == "__main__":
    check_guards(*(Path(arg).resolve() for arg in sys.argv[1:]))
