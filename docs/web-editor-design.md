# Web editor design

`coop web-editor` runs code-server inside an existing VM and prints a URL for
editing `/workspace`. Installation happens on first use; guest settings and extensions
persist between sessions.

The host reuses its trusted SSH launcher to forward a loopback port to the guest's
Unix socket. Code-server provides password login. The CLI owns the foreground
session, coordinates repeat invocations through private IPC, and stops the
editor on Ctrl+C. Guest systemd control groups manage editor descendants.

The `Update code-server` workflow checks stable upstream releases daily and on
manual dispatch. It downloads and hashes both Linux architecture archives, then
proposes a PR changing only the version and checksum pins in
`scripts/guest/web-editor.sh`. Updates require human review; the checker never
installs a release or closes previous update PRs.

See [usage](commands.md#web-editor), [trust boundaries](trust-model.md#browser-editor-transport),
and [integration tests](testing.md#integration-tests).
