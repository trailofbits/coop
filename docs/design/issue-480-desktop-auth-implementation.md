# Issue #480: desktop guest authentication implementation guide

Status: implementation branch for validation, based on PR #482. Desktop UI,
real-account OAuth, and both VM lifecycle gates remain required before release.
Updated: 2026-09-15. Evidence baseline: Ubuntu 24.04, systemd 255,
GNOME Keyring 46.1, native Codex 0.154.0.

This is the implementation direction for #480. The
[research record](issue-480-desktop-auth-research.md) contains reproductions,
upstream sources, prototype commands, and the adversarial review findings.
Use this guide for decisions and that record for supporting evidence.

## Outcome and scope

A user unlocks their guest keyring, connects the Codex desktop app over SSH,
closes the unlock terminal, and can reconnect without unlocking again. A VM
restart requires another unlock. Both desktop and terminal use the same guest
credentials while retaining separate Codex app-servers.

Implement ChatGPT guest authentication for both Linux/Firecracker and
macOS/Lima. Both backends run Linux guests. API-key/proxy forwarding is a
separate design; ordinary desktop SSH bypasses coop's session secret injection.

## Architecture and ownership

| Component | Owner and contract |
| --- | --- |
| User D-Bus | systemd user manager; stable `/run/user/<uid>/bus`. |
| Secret Service | One packaged GNOME Keyring service/socket per guest UID; one persistent encrypted login store. |
| Lifetime after logout | Enable user lingering and explicit headless service startup. |
| Interactive unlock | Dedicated guest PAM helper; password exists only during the operation. |
| Desktop app-server and updater | Codex's native daemon interfaces own their lifecycle. |
| Readiness and recovery | coop's unlock operation checks authentication and performs native recovery while it runs. |
| Terminal app-server | Remains independent, preserving #481's explicit configuration override. |

Use the ordinary SSH login environment supplied by PAM/systemd. Never read a
terminal process's environment or introduce a separate desktop bus. Configure
the packaged keyring service for `default.target`; Ubuntu's graphical install
target alone is insufficient on a headless guest.

Replace the terminal wrapper's private keyring sessions with access to this
service. Preserve its explicit launch argument:

```text
-c 'cli_auth_credentials_store="keyring"'
```

Do not create multiple keyring daemons writing the same files: this reproduced
stale reads and resurrection of deleted credentials after an unrelated write.
Do not introduce separate desktop credentials, a custom OAuth broker, private
GNOME unlock protocol code, or a competing app-server PID manager.

## Unlock operation

The implemented command is `coop codex-unlock <vm>`. The existing
`coop codex unlock` spelling continues to select a VM named `unlock`.

Recovery uses native `daemon stop` to retire cached server authentication.
Coop does not call `start`, `restart`, or `bootstrap`: the desktop owns its next
bootstrap and updater. This avoids introducing failed-start children that
cannot be rolled back conditionally through native 0.154.0 interfaces. A pidfd
observes termination of the server present before stop; coop never signals a
PID or edits native process records. Concurrent native bootstrap still requires
an acceptance test with the actual desktop.

1. Check managed ChatGPT mode and the existing `~/.codex` keyring policy.
   Preserve guards against plaintext fallback and unmanaged `CODEX_HOME`.
2. Acquire a bounded per-user operation lock. Start or verify the systemd user
   bus and keyring service. Derive paths from the guest user's runtime directory.
3. Classify the existing store **before** prompting, probing writes, or taking
   an already-unlocked shortcut. Apply the adoption rules below.
4. For genuinely new storage, prompt without echo for a nonempty password and
   confirmation. For a locked existing encrypted collection, prompt to unlock.
5. Use a dedicated PAM service and `pam_gnome_keyring.so` to create/unlock the
   running daemon through its control socket. Omit `auto_start`: PAM does not
   own daemon lifetime. Do not use standalone `gnome-keyring-daemon --unlock`
   against the running service; it can create competing processes.
6. Resolve `ReadAlias("default")`; verify it names the intended persistent login
   collection and that the collection is unlocked. Do not redirect an unrelated
   default or accept a session-only collection.
7. Write a uniquely named disposable non-secret item, read it back, and delete
   it. Report cleanup failures. An exit-zero or PAM-success result alone is
   insufficient.
8. Perform required native server recovery, then record the successfully
   recovered service generation. Recheck the generation before reporting ready.

Pass the password only over a guest TTY/private pipe, never argv, environment,
files, or logs. Implement cancellation, bounded noninteractive operations, and
secret-memory cleanup. The existing test PAM client is not a production helper.

The prototype PAM stack collects the token with
`pam_exec.so expose_authtok /usr/bin/true`, followed by the GNOME module.
Verify the chosen production stack and package dependencies. Ubuntu's PAM
package runs `pam-auth-update` and adds a global password-management hook by
default; provisioning must explicitly account for that side effect.

## Storage adoption and encryption

An unlocked, writable login collection may store plaintext. Never infer
encryption from its name, permissions, `Locked` property, or successful writes.

| Existing state | Required action |
| --- | --- |
| No store and no conflicting live collection | Allow first-use creation with a confirmed nonempty password; verify format afterward. |
| Supported encrypted-format candidate | Require the fresh daemon to load the intended collection, then unlock through PAM. |
| Plaintext or unsupported format | Refuse before PAM or writes; explain explicit migration and reauthentication. |
| Existing file that daemon cannot load | Refuse as invalid storage; do not treat it as first use or overwrite it. |
| Encrypted candidate that cannot unlock | Preserve it; report unlock failure without asserting an incorrect password. |

GNOME 46.1's binary prefix and version fields identify an encrypted-format
candidate, not its integrity. A valid prefix can survive truncation or
ciphertext damage. The fresh daemon's parser and successful unlock provide
additional evidence. A wrong password and damaged ciphertext can produce the
same unlock failure.

Initial adoption requires a fresh service generation; cached state in an
already-unlocked daemon cannot validate a changed backing file. The implementation accepts GNOME Keyring 46.1 and refuses other versions until
their format/adoption behavior is tested. Do not
claim that a header check is a general integrity validator. Do not silently
convert, delete, or replace existing plaintext/unknown stores.

## Recovery, concurrency, and cleanup

The first implementation uses **user-triggered recovery**. Systemd keeps the
keyring alive. No coop process remains after unlock to retire a native server
automatically when the keyring fails. The recovery instruction is to rerun
unlock and reconnect the desktop. Running clients may retain cached credentials
until they restart; locking the keyring cannot erase those copies.

Record only non-secret runtime state: boot ID, D-Bus server ID, and the
keyring's unique bus owner. Compare this generation with the last successfully
recovered generation. A missing/changed record, or a locked-to-unlocked
transition, requires native server recovery even if another client already
unlocked the replacement service. Publish success only after all probes and
recovery pass. Failure must leave the next invocation eligible to retry.

Repeated unlock can avoid server churn only when storage, collection readiness,
and generation checks pass. Serialize coop operations, but do not assume that
this lock also serializes desktop or updater operations. Use native lifecycle
commands for their own locks and ownership checks.

Native 0.154.0 imposes these constraints:

- `start` reuses a responsive socket without checking its keyring environment.
- `bootstrap` replaces the server and starts/replaces an updater; it is not an
  idempotent attachment operation. `stop` does not also stop the updater.
- Server readiness does not establish keyring readiness. Failed readiness can
  leave detached processes requiring cleanup; an outer timeout is insufficient.
- A coop lock cannot prevent desktop-owned bootstrap/restart. Verify the real
  command sequence and updater races before claiming reliable recovery.

For intentional keyring replacement, retire the associated desktop server first.
For unexpected crashes, apply the next-unlock recovery contract. Preserve the
original failure when rollback also fails. Bound lock waits, D-Bus calls, native
startup, and rollback independently. Clean only resources owned by the failed
operation; cancellation must not tear down a shared healthy service. Validate
real process termination, not just removal of sockets or PID records.

Keep distinct errors for missing/uninitialized collection, locked collection,
unavailable service, invalid default alias, plaintext/unsupported or invalid
storage, unlock failure, probe-write failure, probe-cleanup failure, busy
startup, server ownership conflict, and server readiness timeout.

## Implementation order and repository touchpoints

1. **Guest service and helper:** package dependencies and embedded scripts in
   `src/guest.rs`, shared guest provisioning, `src/lima.rs`, and `scripts/guest/`.
   Add the PAM helper and service setup with isolated lifecycle tests.
2. **Storage/readiness contract:** implement typed failure states, adoption,
   collection checks, bounded operations, and discriminating tests.
3. **Terminal migration:** update `scripts/guest/codex-account.sh` to use the
   singleton while preserving #481 behavior, nesting, and secret-store policy.
4. **Host command and native recovery:** add compatible parsing/dispatch in
   `src/lib.rs` and shared command code; preserve host/guest trust boundaries.
5. **Existing guests:** support in-place installation, then require a VM restart
   before singleton activation to retire old private daemons. Preserve valid
   encrypted credentials and Codex home; do not silently select between stale
   conflicting credential histories.
6. **User docs and gates:** update command/configuration references, examples,
   guest dependency checks, and relevant mutation scope together.

Before enabling the feature, prove the actual desktop sequence through native
interfaces: **unlock → connect → close terminal → reconnect → crash server →
reconnect**. Do not modify private desktop databases or replace native binaries
to force attachment. Resolve native lifecycle blockers before expanding scope.

## Discovery and user workflow

1. Start the VM and generate its alias with `coop ssh-config <vm>`.
2. Verify `ssh coop-<name>` works and the remote login shell finds `codex`.
3. Run the implemented unlock operation.
4. Enable the SSH host in desktop Settings → Connections and select the guest
   project, normally `/workspace`.
5. Complete guest Codex login and run a real task.

Concrete SSH aliases match documented discovery; actual UI discovery/refresh
remains untested. Desktop sign-in, SSH authentication, keyring unlock, guest
Codex login, and project selection are separate steps. Test changed Lima ports,
VM recreation, stale saved connections, and removal of coop-owned SSH entries.

## Validation and release gates

| Gate | Evidence/status |
| --- | --- |
| Shared service coherence and #481 isolation | Local real-process tests pass, including an override-removal mutant. |
| PAM creation/unlock, same service PID | Local tests pass; replacing PAM with unconditional success fails the test. |
| Storage adoption cases | Valid, plaintext, unknown, truncated, unsupported-version, and damaged-ciphertext files tested locally without modifying existing files. Production candidate classification and fresh-service adoption are implemented; the parser and successful PAM unlock provide the additional evidence. |
| Logout vs lookup failure | Helper now verifies absence; unavailable-bus regression fails with old behavior restored. |
| Real systemd and SSH logout/reconnect | Passed with a disposable user and packaged services. |
| Keyring crash and user-manager restart | Passed locally; replacement starts locked and unlock restores credentials. This is not a VM reboot test. |
| Native duplicate start, crash, failed-start cleanup | Local prototype passes. Actual desktop/bootstrap/updater races remain untested. |
| Actual desktop terminal-close/reconnect | Required on macOS/Lima; not run. |
| Fresh browser/device-code login, logout/relogin, account switching | Real accounts required; not run. |
| Concurrent CLI/desktop OAuth refresh and logout | Not run. Shared storage does not guarantee atomic cross-process refresh or cache invalidation. |
| VM reboot, stop/destroy/recreate, migration | Required on both backends; not run. |
| Duplicate unlock, cancellation, wrong password, retry, cleanup | Production PAM/systemd/SSH fixture passes; direct probe tests cover read/write and cleanup failures. |

The research baseline had 15 passing prototype tests; it is characterization
evidence, not completed-feature CI coverage. The historical private-bus terminal
test is superseded by the production shared-service terminal test. The user will run the pushed branch
on macOS/Lima and Linux/Firecracker. Apply repository build, formatting, lint,
unit, mutation, integration, and closeout-review requirements to the actual
implementation. Report unrun gates explicitly.

If real concurrent OAuth use exposes a race, reproduce it at Codex's shared
credential-store boundary. Do not assume singleton keyring ownership fixes
application memory caches or refresh transactions. API-key/proxy mode needs
its own credential-delivery, rotation, and route-compatibility investigation.

## Implementation validation record

The shared provisioning path embeds `codex-keyring.py`, compiles the dedicated
unprivileged PAM client, installs its isolated PAM service, removes Ubuntu's
global GNOME password hook, and enables headless startup and lingering. In-place
installation records the installation boot and refuses activation until reboot.

The operation classifies storage before prompts or writes, requires fresh-service
adoption when the recovered generation is missing or changed, checks the default
alias and lock state, and creates/reads/deletes a uniquely named non-secret item.
Only successful native retirement and final generation checks publish readiness.
Explicit retries reset systemd's start-limit failure before a bounded startup.
Passwords stay in the C helper; its locked buffers are cleared, core dumps are
disabled, and signal/timeout handling restores TTY echo.

The production systemd/SSH fixture has passed first-use confirmation, encrypted
storage, repeated unlock without service churn, last-terminal closure and fresh
SSH access, wrong-password refusal, native bootstrap/updater presence, native
server crash/restart, keyring crash recovery, and refusal of plaintext, unknown, unsupported-version,
truncated and damaged-ciphertext stores. It also tests concurrent native
bootstrap/unlock and the terminal override-removal mutant.
These tests use disposable fixture values, not real account credentials.

Release remains blocked on actual desktop attachment/reconnect, concurrent OAuth,
VM reboot and migration on both Lima and Firecracker. This branch is intended
for those validation runs; local Linux service tests do not establish them.

The 28 host-only tests exercise storage and collection decisions, operation
retry/generation handling, migration refusal, and the production disposable-item
probe. Removing probe writes, deletion, value verification, recovery or format
checks makes these tests fail. Replacing PAM authentication with unconditional
success also fails the real-service fixture.

Workspace build, format, clippy, unit tests and TOML formatting have passed.
`cargo deny` reports inherited advisory RUSTSEC-2026-0285 for the unchanged
`rustls 0.23.43` dependency; that is a separate release blocker.

The final cargo-mutants full-file sweep of `src/lib.rs` and `src/guest.rs`
tested 66 mutants: 54 caught, 12 unviable, zero missed and zero timeouts.
The new host command is covered by the existing `cmd_*` IO exclusion;
`.cargo/mutants.toml` needed no change. Final pre-commit hooks passed.
Closeout review found no remaining code blockers; release readiness remains
blocked on the explicit external gates and dependency advisory above.
