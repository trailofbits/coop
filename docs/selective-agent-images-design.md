# Selective coding agents in golden images — design for #683

Status: implementation design for #683, based on `main` and PR #731 as of
2026-11-26. The feature branch implements this design; validation outcomes
belong in its draft pull request.

## Goal and user contract

An image contains a nonempty, explicit set drawn from Claude Code, Codex, and
Grok Build. The default remains all three. A user can build a Codex-only image
without downloading or installing Claude or Grok, and use normal Codex CLI,
Code Mode host, plugins, account auth, and credential proxy in VMs created from
that image. The selection is a promise about what coop provisioned, not a scan
for arbitrary software subsequently installed by a guest user.

Recommended CLI, following PR #731's repeatable selector:

```text
coop setup --image codex-dev --agent codex
coop setup --image polyglot --agent claude --agent codex
coop setup --image codex-dev --agent codex --rebuild
coop up . --image codex-dev
coop agent status my-vm                   # only installed agents
coop agent update my-vm --agent codex    # explicit target
```

`--agent` is repeatable, deduplicated, and order independent. Omission on a new
image means all three. Omission on an existing image preserves its recorded
selection. An explicit selection differing from an existing complete image
requires `--rebuild`; on the normal path, fail before host setup or image
effects with guidance to rebuild or choose a new name. Recheck under the image
lock before any image effect so a concurrent replacement cannot bypass this.
If an existing disk's metadata is missing or unreadable, selection-preserving
setup cannot determine its contract and must refuse implicit rebuilding;
`--rebuild` is the explicit recovery path and warns that it discards the old
image. This tightens the current orphan-image auto-rebuild behavior only for
that damaged state. A readable committed checkpoint can still be implicitly
rebuilt by current staleness behavior for unrelated recipe changes; preventing
that needs an image-kind contract and is a separate logical change. The guard
here prevents an *explicit selection change* from silently replacing it.
`--rebuild` without a
selector also preserves the existing selection. This is deliberately different
from `--no-agents`, which only skips boot-time configuration and credential
injection, leaving the image contents intact.

No `up --agent` flag in this slice. `up --profile rust` keeps its current
profile-derived image name and, if that image exists, reuses its selection;
otherwise it builds an all-agent image. To get a predictable Codex-only image,
use the named-image commands above. Adding `up --agent` would need a distinct
derived name incorporating the agent set, and could be assessed separately.
When `up --profile` reuses an existing image, print the image name and agent
set before VM creation so that a previously built subset is visible.
Current `main` removed `coop quickstart`, so #683's quickstart criterion is
obsolete; getting-started guidance should use `setup`, `up`, then an available
agent command.

### Visible behavior

| Situation | Result and recovery |
| --- | --- |
| `coop setup` on a new image | Builds all three agents, as today. |
| `coop setup --image codex-dev` on existing Codex-only image | Keeps Codex-only selection, including when a recipe change rebuilds it. |
| Explicit different set on existing image without `--rebuild` | Fails before side effects; name `--rebuild` and a new image name as alternatives. |
| `coop images` / `--json` | Show `agents: claude,codex,grok` (human) or a canonical `agents` array (JSON). Corrupt/missing metadata remains `unknown`/`null`, not an invented capability. |
| `coop status` / `--json` | Show the VM's snapshotted agent set, so a user can choose a valid launch or diagnose an older disk after its source image is rebuilt. |
| `coop claude` on a Codex-only VM | Fails before session credential resolution with `Image '<image>' for VM '<vm>' does not include Claude Code; build another image with --agent claude and create or restore a VM from it`. Does not auto-install. |
| `coop agent status` or `update` with no `--agent` | Targets exactly the VM's installed set. Explicit unavailable targets fail before any update, including when another requested agent is present. |
| `coop model` / `model local` | Report or prompt only for installed Claude/Codex. Grok still has no local-model integration. If the image contains only Grok, `model local` fails before saving state with actionable guidance; `model remote` and status remain usable. |
| `coop proxy setup --vm V` for absent provider | Fails before prompting, validating, or storing a credential; suggests a VM whose image includes that agent or a global default for other VMs. Global `proxy setup` remains valid. |
| `coop proxy status --vm V` | Labels absent providers `unavailable in this image`, with any configured default/override shown as inactive, without resolving or printing a secret. Global status still lists both defaults. |
| Image includes an agent but its CLI is damaged or manually removed | Capability remains recorded; launch reports a broken installation with rebuild/restore guidance, while `agent status` can show the missing or unknown version. Do not treat a failed binary probe as an omitted agent. |
| `coop restore` / `restore --reprovision` to another image | After successful disk replacement, VM lineage and agent set change together; the next start uses the restored set. A failed replacement keeps recoverable partial state and retry guidance. |

All rejections should name the selected image and VM and offer a command path.
Never claim an arbitrary hand-installed binary is absent; say the image does not
include coop-managed support for it. `coop shell` and `coop exec` remain usable
even when an agent is omitted. Explicit `post_start` and guest files remain
user-controlled and are not interpreted as agent installation requests.

## Storage and authority

Define one shared `AgentId` enum (`claude`, `codex`, `grok`) and an `AgentSet`
smart constructor that sorts/deduplicates and rejects empty sets. Do not reuse
PR #731's `AgentSelection::new(Vec::new())` as the persisted type: empty there
means "use the default", whereas an empty image capability is invalid. The CLI
parser keeps `Option<AgentSet>` for setup and a possibly empty selector for
maintenance. Move PR #731's agent identifier to a small shared module so
`setup`, `guest`, `backend`, and `commands` do not define parallel enums.

Add `agents` to `TemplateConfig`, serialized as a canonical array, with a
deserialization default of all three for legacy metadata. Reject an explicitly
empty or unknown array. Write the same value into Firecracker's
`/etc/coop-template` marker and Lima's host metadata. The source image's
metadata is authoritative when creating a VM; a missing/unreadable file must
fail image-consuming operations, rather than silently assuming all agents.
Legacy *valid* metadata without `agents` means all three.

Add an optional **full source `TemplateConfig` snapshot** to `InstanceMeta`
and `Instance`, rather than a standalone agent field. This is a compact image
install contract: agent set, guest user, baked plugin lists, profile inputs,
hashes, and backend capabilities all describe the same copied disk. Use it for
runtime agent policy, SSH guest user, first-boot plugin deltas, existing-disk
control validation, and `commit` metadata. This removes runtime dependence on
the mutable source image directory. A new instance can restart after its
source image is rebuilt or deleted without acquiring a different contract.
An old `instance.json` without a snapshot keeps the legacy lookup behavior,
except its agent set is conservatively all three; old images all installed
three. Do not eagerly write a current image snapshot onto an old disk, since
the image may already have been rebuilt. If an old instance's source metadata
is missing, retain existing recovery guidance rather than inventing guest
user or baked plugin state.

New instance allocation has two image-lock windows today: early validation,
then a second lock in `start_instance` around the actual disk copy. The second
lock is authoritative. Reload and validate image metadata there, persist the
snapshot matching that disk before copying it, **replace the in-memory
`Instance` with the refreshed value**, and rerun selection-dependent preflight
against the locked image. `start_instance` currently takes `&Instance`, so
its caller/signature must change; `BootPayload::prepare`, backend copy, SSH
target construction, and subsequent bootstrap must all receive the refreshed
value, and `allocate_and_start` must return it. A concurrent rebuild between the windows must not leave a VM whose
recorded contract differs from its disk. Hold the lock through the backend
copy; retain failed-start cleanup if final validation or copying fails.
`commit` clones the instance's source snapshot with a fresh
creation timestamp and backend-adjusted capabilities. A manually installed
agent is outside coop's capability promise; commit does not guess from
binaries. For legacy instances, keep the existing source-metadata fallback.
Restore/reprovision and new VM creation are how a user adopts a changed set.

Disk replacement needs a durable transition gate, not only an atomic metadata
write. After a VM is confirmed stopped and checked proxy teardown succeeds,
but before restore or reprovision swaps a disk, write a private
`restore-intent.json` containing the target image identity, complete target
contract, and a phase (`disk_swap_pending` or `provisioning_pending`). A
central `ensure_contract_ready` gate must run before every guest-consuming
operation: start and network restart, SSH target/session
preparation (`shell`, `exec`, editor, agent launch, agent update/status), model
application, commit, and other paths that assume the disk contract. Put it at
shared target/session entry points as defense in depth and at commands before
guest side effects. Read-only list, status, and proxy status report pending
recovery. Stop/destroy and a new restore remain available. Also allow
**host-only recovery edits** while the VM is stopped: `model remote` saves
remote mode without applying it, `proxy setup --vm` checks the intent's target
agent set before prompting/storing a credential, and network mode changes
without `--restart` remain available. These edits must not prepare a guest
session or clear the intent. Other model changes that would need the guest
remain gated. This prevents a partial restore from trapping the user when its
target image needs a different model or proxy setting.
The gate applies even if the VM has no `creation.json` hook state. Under the
image publication and instance operation locks, run checked teardown of
**both** proxy providers for the stopped VM before writing the intent. If
either exit is uncertain, leave the old disk and contract intact and report
the cleanup failure. Then write the intent, replace the disk, and write
the new image name plus snapshot in one `instance.json` update. For disk-only
restore, remove the intent last. For `--reprovision`, advance the intent to
`provisioning_pending` and retain it until workspace sync, agent bootstrap,
plugins, and creation hooks have completed. Only the same restore workflow,
holding its provisioning token, may boot or prepare a session while this
phase is pending; ordinary start and launches remain blocked. Clear the
intent only after full provisioning succeeds. If a failure proves the old
disk is unchanged, restore the old metadata and clear intent. If the disk
changed or its identity is uncertain, keep the intent and instruct the user
to retry restore (to the same or another valid image). A crash after metadata
publication but before intent removal still blocks start; retrying restore
is safe. The intent is never silently cleared based only on matching metadata,
since that does not prove the disk replacement completed. A reprovision
failure leaves the new disk and pending phase for an explicit
`restore --reprovision` retry; it cannot be mistaken for a ready VM even when
no `creation.json` exists.

For profile-derived images, the effective setup selection must be resolved
from that image's metadata when no selector was supplied, even though the
profile list was supplied by `up --profile`. This rule must be common to both
backends. Lima currently does not preserve omitted profiles on plain `setup`
the way Firecracker does; resolve effective image inputs once in shared code
and use them in both recipe generation and staleness checks, so omission
preserves profiles and agents consistently. Preserve the current platform
limits on `--extra-packages` and `--post-install`.

Hash the normalized set and only the selected installer scripts and selected
support packages into each backend recipe. Compare the persisted set explicitly
as well as the recipe hash; do not rely on a hash collision or on a stale
metadata field. Plain setup on a legacy image defaults to all three. A recipe
code change may rebuild that image, but must not narrow it. Publish disk and
metadata under the existing image lock and completion marker. No selected
agent is introduced merely because global config mentions it.

## Build contract on both backends

`guest.rs` should provide a common, selected-agent build plan: base tools,
Codex-specific support packages, selected installer snippets, expected binary
paths, guest PATH entries, shortcuts/symlinks, and plugin applicability.
Firecracker's `guest-config.sh` and Lima's `compose_lima_guest_config` currently
create `claude`, `grok`, and `*-yolo` entrypoints unconditionally. Make those
agent-specific artifacts conditional too: a Codex-only image should not have
broken Claude/Grok symlinks or launch shortcuts. Keep shared `~/.local/bin`
PATH setup needed by Codex, and include `~/.grok/bin` only for Grok. A custom
profile or post-install script can still add arbitrary tools; capability
metadata describes coop-managed support and the host command policy, not an
exhaustive filesystem inventory. Firecracker's `compose_recipe` and Lima's
`compose_provision_script` consume that plan in their existing ordering:
packages, profile scripts, guest user setup, then selected installers. Keep
Docker and GitHub CLI base packages independent of the agent set. Keep
`codex.sh` and `codex-account.sh` together when Codex is selected; the
`codex-account` wrapper and `dbus-user-session`, `gnome-keyring`, and
`libsecret-tools` are needed even when current `[codex].auth` is `api_key`,
because the config can change without rebuilding. Verify Docker/gh for every
image, Claude and Grok binaries when selected, and the Codex CLI, Code Mode
host, wrapper, and keyring utilities when Codex is selected. Do not require
missing agents' binary paths. Before removing a package from a non-Codex
image, verify its use by the base OS, GitHub auth, other agents, profiles, and
both backends; the three named keyring packages are candidates, not assumed
exclusive solely from their current comments.

In Lima, filter builder marketplace/plugin lists by the set before both
installation and staleness comparison. In Firecracker, the existing build
does not bake plugins; first boot installs deltas only for selected agents.
Built-in `c` and `rust` profiles carry Claude plugins, so a Codex-only image
still installs their language toolchains but reports `Skipping Claude plugin
<name>: Claude is not in image '<image>'` once at setup. Current Firecracker
first-boot delta logic also omits profile-declared Claude plugins even on an
all-agent image, whereas Lima bakes them. That pre-existing mismatch needs a
separate fix or an explicitly scoped change; do not present a skip as though
Firecracker would otherwise have installed the plugin. The same rule applies
to global plugin/marketplace/MCP/config values for absent agents: keep them in
shared config for other images, skip their work for this image, and do not
resolve agent-specific source paths or credentials. An explicit per-image
request for an absent agent, if such a field is added later, should error;
currently the only explicit per-image agent request is `setup --agent`.

## Boot, credentials, network, and live changes

Derive one `InstanceAgentPolicy` from the instance set and current model mode.
Pass it to existing shared bootstrap, model, proxy, session, and isolated
preflight code. This can replace repeated Claude/Codex/Grok loops and two
unconditional proxy lookups; it should remain data, not a second backend
dispatch. On every boot and live `model` reconfiguration:

1. Stop stale proxy processes/tunnels/tokens for absent providers before
   preparing any session. Keep stored global defaults and per-VM overrides;
   `proxy status` marks them inactive so restoring a suitable image can reuse
   them. Existing `stop_provider` only sends SIGTERM and unlinks state, so it
   cannot prove exit. Add a bounded, checked teardown for the transition:
   retain the process identity and PID files until both the proxy and tunnel
   are confirmed gone, remove the token, and fail closed if exit is uncertain.
   Verify the stored process identity before signalling to avoid PID reuse;
   use Linux pidfd to pin the process through signalling. macOS has no pidfd
   equivalent here, so its start-time check retains a narrow PID reuse race.
   Use checked, bounded cleanup in **all** new stop/destroy paths too. If teardown is
   uncertain, retain the instance directory and process records, permit VM
   shutdown for recovery, and return a cleanup error so retry can finish;
   do not unlink the only identity record or claim complete cleanup.
   Previously orphaned processes whose old PID files were already removed
   cannot be proven absent by the new protocol. Document that legacy limit
   and give host-process inspection/termination guidance; do not represent a
   missing PID file as proof that no older proxy exists. Existing PID-only
   files lack a recorded start time. For one of those, clear a stale file
   automatically only when its PID is confirmed absent; if that PID is live,
   do not signal a potentially reused process or delete the record. Return
   the PID and record path with guidance to inspect the host process identity,
   terminate it only if confirmed to be coop's proxy/tunnel, and retry the
   operation. A live PID-only record remains a cleanup uncertainty until the
   user resolves it. Route **every** new proxy teardown site through this
   protocol: normal stop/destroy, `allocate_and_start` failed-start cleanup,
   proxy-launch rollback after token/tunnel failure, and stale replacement at
   the beginning of `spawn_proxy`. When a fresh child handle is still owned,
   kill and reap it directly. If cleanup cannot be proved, retain the
   instance/PID records instead of destroying the directory and return
   explicit retry guidance. No failure path may erase the only record of a
   possibly live credential holder.
   A disk-only restore must complete checked teardown before it reports
   success; no later boot is required to make the guarantee true. Live model
   reconfiguration must coordinate its short proxy start/stop transitions
   with stop/restore through a per-instance generation and a short proxy
   transition lock. Resolve a potentially interactive `cmd:` credential
   outside that lock; after acquiring it, recheck the generation before a
   bounded spawn or teardown. Stop/restore marks the generation cancelled,
   then takes that transition lock for checked teardown. A later model apply
   cannot resurrect a proxy after cleanup. All boot and live proxy starts
   use the same transition protocol; a special unchecked caller would reopen
   the race. Keep one lock order: lifecycle operation lock may precede proxy
   transition lock; proxy transition code never acquires the lifecycle lock.
   The generation/cancellation record is written atomically under the
   lifecycle lock and read under the proxy lock without reacquiring it. If
   model apply gets the proxy lock first, stop waits and then tears down what
   it started; if stop cancels first, model apply sees cancellation and skips
   the spawn.
2. Resolve proxy upstreams only for selected providers in remote mode. Never
   execute a `cmd:` resolver for an absent provider. A selected provider's
   proxy still fails closed; do not fall back to its raw key.
3. Build credential forwarding from the same set. Do not resolve or forward
   an absent agent's built-in API key, its section-specific `env_forward`
   names, or its MCP environment mappings. Preserve explicit `guest_env` and
   saved `--env` entries even if their names resemble an absent agent's keys:
   users may deliberately pass `OPENAI_API_KEY` to another tool in a
   Claude-only VM. Document that this is an explicit guest credential path.
   Apply the existing proxy and isolated-network suppression to those entries
   when active for the VM; an absent provider's global proxy default is not
   an active proxy for that VM. Preserve the historical `CODEX_ACCESS_TOKEN`
   rule for saved overrides when Codex is present. Preserve arbitrary shared
   env vars and GitHub token behavior. Never log values.
4. Configure GitHub guest auth once when available, independent of agent
   selection. Bootstrap only selected agent config, plugins, MCP servers, and
   authentication support. `--no-agents` still skips the entire agent pass;
   its warning mentions only relevant configured providers. On a
   `--no-agents` start, checked teardown applies to **all** provider proxies,
   tunnels, and tokens for that VM, including selected agents, because no
   bootstrap will recreate their configuration. A failure to prove teardown
   blocks successful start rather than leaving a stale usable proxy.

Isolated Firecracker networking remains a VM-level policy. On first boot,
check pending remote plugin work only for selected agents. In remote mode,
require at least one selected proxy-capable agent (Claude or Codex) with its
matching configured proxy; an Anthropic-only proxy cannot satisfy a Codex-only
image. A Grok-only isolated VM with agent bootstrap gets clear guidance to
use `--no-agents` for a control-only VM or switch to open networking. Preserve
current `coop grok` launch rejection in isolated mode. On launch, check image
capability before isolated-network and proxy checks so the error identifies
the missing agent first.

Config validation must have a target-aware path. Today the CLI calls
`cfg.validate_and_warn()` before `up`, `setup`, and `start`, while the backend
`boot_preflight` calls global `CoopConfig::validate` before setup/boot (also on
network restart). Both layers must receive the effective image/instance set;
changing only the command handler would leave the boot choke rejecting an
unused agent's stale path. Split common host/VM validation from agent-specific
environmental validation; setup and boot validate only the target set, and
`coop validate` continues to inspect all configured agent sections. Keep
structural parsing for all sections at load time. The
`codex.auth = "chatgpt"` versus `[proxy.openai]` conflict is actionable only
for a VM containing Codex; leave the global `coop validate` diagnostic, but
evaluate the conflict in the target context for setup/boot. Do not weaken
validation for an installed agent. This needs an explicit target argument at
all CLI preflight and backend boot-preflight choke points, not ad hoc skipped
calls. Linux setup currently reaches `build_or_check_template` only after
host-package/Firecracker setup, so resolve and check an explicit selection
conflict earlier, then recheck under the image publication lock. A concurrent
replacement may still cause a late rejection, but must never cause a build
against the wrong selection.

Live `model local` should not save a state that no installed agent can use.
Before restoring a disk, check the VM's existing `model.json` against the
*target* set. If it is local mode but no selected Claude/Codex endpoint
resolves, reject before the swap with `coop model <vm> remote` or endpoint
configuration guidance. Do not silently alter the user's model selection.
State for an omitted agent (old endpoint, materialization flag, or proxy
override) may remain stored for future restore, but no omitted-agent config or
process stays active.

Today live `model local` and `remote` save `model.json` before bootstrap; a
bootstrap failure can leave desired state and guest state different. Make a
live change a recoverable transition: preflight the prospective mode and
target agent set, write desired mode with an `apply_pending` marker **before**
guest/proxy changes, apply the filtered bootstrap, then clear the marker.
Mint a generation for that attempt under the instance operation lock, then
release that lock before any SSH operation or credential resolver; SSH guest
commands can block, and stop/destroy must remain able to recover a hung VM.
Reconfirm that the VM is still running while minting the generation; a stop
between the earlier running check and this step must make the live apply
abort. Clear `apply_pending` only after reacquiring the lifecycle lock
*outside* the proxy transition lock and checking both the generation and VM
state.
Recheck the generation before each short host proxy transition and before
clearing `apply_pending`. If stop/restore cancelled it, leave the marker and
abort without restarting a proxy. A guest write racing with stop cannot mark
the VM ready; the next normal boot reapplies the desired state.
Agent launch checks the marker and fails with restart/retry guidance while it
is set; shell/exec remain available for diagnosis. On a later normal restart,
bootstrap reapplies desired state and clears the marker only on success.
`--no-agents` does not clear it. A stopped-VM model change simply saves the
desired mode for the next boot. Stop/destroy still stop all providers.

## Code simplifications enabled by the capability

- Replace the separate agent enums in `commands/lifecycle.rs` and PR #731's
  `commands/agent.rs` with one shared identifier, plus a distinct nonempty set.
- Replace fixed arrays such as `required_guest_binaries() -> [GuestPath; 10]`
  with plan-derived paths; a single plan drives installer inclusion and
  verification on both backends, reducing recipe/verification drift.
- Replace three unconditional bootstrap calls and provider-specific preflight
  loops with iteration over the set. Keep agent-specific configuration
  functions; a generic trait/framework for the different CLIs would add more
  indirection than it removes.
- Load the full source template snapshot once from `Instance`, rather than
  reopening mutable image metadata for guest user, baked plugin deltas,
  capability, and commit. Legacy instances use a clearly bounded fallback.
- Centralize the provider-to-agent mapping and known credential names in one
  policy. Reuse it in proxy status/setup, isolated preflight, and forwarding.

Avoid combining this behavioral change with unrelated module renames or a
general rewrite of configuration, proxy, or lifecycle code. Stage any pure
type extraction separately if it materially reduces the feature diff.

## Validation and measurement

Unit tests: canonical parsing/serialization and legacy defaults; reject empty
sets; preserve selection on setup and profile-derived setup on both backends;
explicit change requires rebuild; hash changes with selection; each plan's
installer and required-binary paths; profile plugin skip; no absent-agent
`cmd:` resolution; credential suppression from built-ins and `env_forward`,
and explicit `guest_env`/saved `--env` preservation subject to active
proxy/isolated suppression; selected proxy preflight; launch/model/agent
update/status/proxy setup/status behavior; clone/commit/restore lineage and
interrupted publication. Include image rebuild between allocation and copy,
source image deletion after VM creation, disk swap interrupted before metadata
publication, stale proxy teardown failure, and live model apply failure with
its launch gate. Include failed-first-boot cleanup and token/tunnel-launch
rollback that leave a proxy or tunnel uncertain, proving the instance/PID
records remain available for retry. Deliberately remove the capability filter in a
focused test run to confirm the tests fail, or use the mutation-check workflow
for the changed logic modules.

VM integration on Linux/Firecracker and macOS/Lima: build all-agent and
Codex-only images; inspect binaries and support files; inject an unused
installer failure and show Codex-only setup succeeds; boot/restart both;
exercise Codex account auth, Code Mode host and plugins, OpenAI proxy,
Anthropic-only isolated preflight rejection, no Anthropic process/tunnel/token
or prompt, live model changes, explicit absent launch/update/proxy setup,
`c`/`rust` profile handling, commit, disk-only restore, reprovision, and
stale-image replacement while an older VM still exists. Check absent CLI
shortcuts/symlinks as well as absent binaries. Confirm required CI
jobs on the resulting PR, not only visible green checks.

Measure wall-clock build time, installer/download bytes, and allocated disk
blocks (`du`, not just apparent ext4 capacity) for baseline and Codex-only on
both backends, with the same base image and network conditions recorded.
Report the observations and variability; keep the all-agent default until
the requester accepts a measured change. The expected benefit is shorter,
less failure-prone builds and fewer unused credentials/processes. The cost is
that adding an omitted agent needs a new image and new or restored VM; no
existing disk is rewritten automatically. An upgrade with an already-running
legacy PID-only proxy may require one manual host-process check before a
selection change or cleanup can succeed; the VM and its records remain in
place with specific retry guidance. This is preferable to signalling an
unverified reused PID or reporting that a credential-holding process is gone
without proof. No new outbound URL, listener,
egress rule, or guest secret is proposed.

## Implementation order

1. Land or rebase onto PR #731, then extract the shared agent identifier and
   nonempty set. Add legacy serialization tests.
2. Add effective setup inputs and per-image/instance metadata, atomic lineage
   updates, and image/JSON display. Test stale named images and commits.
3. Apply the selected build plan to both recipes, binary verification, and
   baked plugin installation. Cross-compile/lint the affected macOS code.
4. Make shared boot, credential, proxy, model, and command paths consume the
   instance policy. Add targeted failure/recovery tests and docs.
5. Run workspace gates, mutation checks for changed logic, both VM integration
   backends, and the measurements. Record any unavailable gate in a draft PR.

## Source notes

- Issue #683: <https://github.com/trailofbits/coop/issues/683>.
- Draft PR #731: <https://github.com/trailofbits/coop/pull/731>.
- Existing contracts: `src/setup.rs`, `src/lima.rs`, `src/guest.rs`,
  `src/config.rs`, `src/backend.rs`, `src/commands/lifecycle.rs`,
  `src/commands/agent.rs`, `src/commands/model.rs`,
  `src/commands/proxy.rs`, `docs/images-and-profiles.md`,
  `docs/trust-model.md`, and `docs/user-impact.md`.
