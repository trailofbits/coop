---
description: Run coop's integration test suite — the full VM lifecycle — locally (Lima) and/or on a remote Linux host (Firecracker).
argument-hint: [--remote user@host] [--full] [--profile LIST] [--name NAME]
allowed-tools: Bash(./tests/run-integration.sh:*), Bash(git rev-parse:*), Bash(uname:*)
---

## What this does

Wraps `tests/run-integration.sh` — the runner that builds `coop`, deploys it
(cross-compiling for a remote host), and runs `tests/integration.sh`, which
exercises the **full VM lifecycle**: setup → start → status → shell → guest
environment → docker → stop → destroy.

coop supports two backends, and CLAUDE.md requires the suite to pass on **both**
before a commit:

- **Local (macOS/Lima)** — builds a native release binary and runs against Lima.
- **Remote (Linux/Firecracker)** — detects the remote arch, cross-compiles
  (`x86_64-unknown-linux-musl`), copies the binary over, and runs there.

## Argument

`$ARGUMENTS` is forwarded to the runner. Common forms:

- (empty) — run locally against Lima (macOS host).
- `--remote user@host` — cross-compile and run on a Linux/Firecracker host.
- `--full` — extended tests (workspace sync, multi-instance).
- `--profile python,node` — install these guest profiles during the run.
- `--name my-test` — instance-name prefix (default `test-<pid>`).

Flags combine, e.g. `--remote user@host --full`.

## Preconditions

- In the coop repo: !`git rev-parse --show-toplevel 2>/dev/null || echo "MISSING — not a git repo"`
- Local host: `./tests/run-integration.sh` (no `--remote`) requires macOS + Lima. On a Linux host, use `--remote` to reach a Firecracker box, or run the suite there directly.

If the caller asked for "both platforms," you need a macOS host for the local
Lima run and a `--remote` Linux host for the Firecracker run — a single machine
only covers one. Surface that if only one is available.

## Your task

1. Determine which run(s) the arguments call for. If the user said "both
   platforms" and gave a `--remote` host, run the remote one and remind them to
   run the local Lima pass on their Mac (or vice-versa).
2. Redirect output to a file rather than piping to grep/head — the run is
   expensive (a `PreToolUse` hook enforces this). e.g.:
   ```bash
   ./tests/run-integration.sh $ARGUMENTS > "${TMPDIR:-/tmp}/coop-integration.out" 2>&1
   ```
3. Read the output file, report pass/fail per phase, and quote the failing
   phase's output on any failure. Don't declare success unless the suite exits 0.
4. Clean up the output file when done.

Narrate each step. This drives real VMs and takes minutes; use a generous
timeout and don't busy-poll.
