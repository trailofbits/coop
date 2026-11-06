# Web editor design

`coop editor --web` runs code-server inside an existing VM and prints a URL for
editing the requested `--project` (default: `/workspace`). Installation happens on first use; guest settings and extensions
persist between sessions.

The host reuses its trusted SSH launcher to forward a loopback port to the guest's
Unix socket. Code-server provides password login. The CLI owns the foreground
session, coordinates repeat invocations through private IPC, and stops the
editor on Ctrl+C. Guest systemd control groups manage editor descendants.

The project is passed as a quoted guest argument and encoded as a single URL
query parameter. Repeat invocations reuse the session while opening their own
requested project. `--web` conflicts with `--editor` and `--clean`; `--port`
requires `--web`.

The guest tracks startup in a stage variable and sends short stdout lines:
`preparing`, `installing`, `downloaded BYTES`, `starting`, `launched`, `ready`,
or `failed STAGE REASON`. The host remembers the last stage, accepts only known
tokens and numeric progress, bounds lines to 256 bytes and 1,024 total messages,
and rate-limits progress display. It reads statuses before reporting SSH exit;
stderr stays discarded.
Downloads allow up to 30 minutes within a 35-minute startup deadline, with
byte progress reported every two seconds.
The guest acknowledges launch only after opening `editor.log`, so download and
systemd failures do not direct users to a log that was never created. A closed
transport without a failure status reports the last observed stage, rather
than guessing a guest failure. SSH connection and forward-setup failures before
the first status cannot be distinguished from stdout alone.

The `Update code-server` workflow checks stable upstream releases daily and on
manual dispatch. It downloads and hashes both Linux architecture archives, then
proposes a PR changing only the version and checksum pins in
`scripts/guest/web-editor.sh`. Updates require human review; the checker never
installs a release or closes previous update PRs.

See [usage](commands.md#browser-mode), [trust boundaries](trust-model.md#browser-editor-transport),
and [integration tests](testing.md#integration-tests).
