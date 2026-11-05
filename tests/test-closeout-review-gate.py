#!/usr/bin/env python3
"""Exercise draft and ready PR review markers in the Claude compatibility hook."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


HOOK = Path(__file__).resolve().parents[1] / ".claude/hooks/closeout-review-gate.sh"


class CloseoutReviewGateTests(unittest.TestCase):
    def setUp(self):
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.repo = Path(self.tempdir.name)
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "config", "user.email", "test@example.invalid"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(self.repo), "config", "user.name", "Test"],
            check=True,
        )
        (self.repo / "file").write_text("first\n")
        subprocess.run(["git", "-C", str(self.repo), "add", "file"], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "commit", "-qm", "First"], check=True
        )

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), *args], text=True
        ).strip()

    def marker(self, verdict):
        marker = self.repo / ".git" / "closeout-review" / self.git("branch", "--show-current")
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(f"{self.git('rev-parse', 'HEAD')} {verdict}\n")

    def gate(self, command):
        env = os.environ.copy()
        fake_bin = self.repo / "bin"
        fake_bin.mkdir(exist_ok=True)
        fake_gh = fake_bin / "gh"
        fake_gh.write_text("#!/bin/sh\nprintf called > \"$GH_CALL_LOG\"\n")
        fake_gh.chmod(0o755)
        env["PATH"] = f"{fake_bin}:{env['PATH']}"
        env["GH_CALL_LOG"] = str(self.repo / "gh-called")
        return subprocess.run(
            ["bash", str(HOOK)],
            input=json.dumps({"cwd": str(self.repo), "tool_input": {"command": command}}),
            text=True,
            capture_output=True,
            check=False,
            env=env,
        ).returncode

    def test_draft_requires_current_review_but_not_clean_verdict(self):
        self.assertNotEqual(self.gate("gh pr create --draft"), 0)
        self.marker("blocked")
        self.assertEqual(self.gate("gh pr create --draft"), 0)
        self.assertNotEqual(self.gate("gh pr create"), 0)
        (self.repo / "file").write_text("second\n")
        subprocess.run(["git", "-C", str(self.repo), "commit", "-qam", "Second"], check=True)
        self.assertNotEqual(self.gate("gh pr create --draft"), 0)

    def test_ready_requires_clean_marker_and_quoted_draft_does_not_bypass(self):
        self.marker("blocked")
        self.assertNotEqual(self.gate("gh pr create --title 'text --draft here'"), 0)
        self.assertNotEqual(self.gate("gh pr create --title '--draft'"), 0)
        self.assertNotEqual(self.gate("gh pr create --title --draft --body ''"), 0)
        self.assertNotEqual(self.gate("gh pr create --title x --draft"), 0)
        self.assertNotEqual(self.gate("gh pr create # --draft"), 0)
        self.assertNotEqual(self.gate("gh pr create && echo --draft"), 0)
        self.assertNotEqual(self.gate("gh pr create --draft && gh pr ready"), 0)
        self.marker("clean")
        self.assertEqual(self.gate("gh pr create"), 0)
        self.marker("blocked")
        self.assertNotEqual(self.gate("gh pr create"), 0)

    def test_gate_does_not_execute_pr_command(self):
        self.assertNotEqual(self.gate("gh pr create --draft"), 0)
        self.assertFalse((self.repo / "gh-called").exists())


if __name__ == "__main__":
    unittest.main()
