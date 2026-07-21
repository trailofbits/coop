# `--json` machine-readable output — design & implementation guide

Tracking issue: [#385](https://github.com/trailofbits/coop/issues/385).

This document is an implementation guide. It specifies a `--json` output mode
for coop's read/query commands, the shared infrastructure that keeps human and
JSON output from drifting, and the exact view models (typed against coop's
existing newtypes) for every command in scope.

## Goal

coop has no machine-readable output. Every command prints human text; automation
must scrape fixed-width tables or free-form strings. Add an opt-in `--json` flag
so command output can be parsed reliably, starting with the highest-value read
commands. The human output stays the default and is unchanged.

`serde_json` (with `preserve_order`) is already a dependency; `serde::Serialize`
is derived widely across `config.rs`. No new dependency is needed.

## Design principles

These are the rules that keep this from becoming a maintenance burden. Follow
them for every command you add to the JSON surface.

### 1. Collect once, render twice (single source of truth)

The trap is `if json { build_json() } else { build_table() }` with two hand-built
formatters that drift. Instead: the command builds **one** `Serialize` view
model, and `--json` switches only the final render step. The human formatter and
the JSON serializer read the *same* value, so they cannot disagree about the
data — only about presentation.

```rust
let view = collect(...)?;                 // one value, both paths read it
if json {
    render_json(&view)?;                  // serde_json → stdout
} else {
    render_human(&view)?;                 // existing formatter, now reads `view`
}
```

### 2. Carry newtypes, don't downgrade to `String`

coop already parses untrusted input into strong newtypes at the CLI boundary
(`InstanceName`, `ImageName`, `RepoSlug`, `GuestUser`, …). Every one of these
**already implements `Serialize` and emits a bare JSON string**:

- `InstanceName` — `config.rs:1611`, serializes `self.0`
- `ImageName` — `config.rs:1683`
- `RepoSlug` — `github_repo.rs:96`, `serialize_str(&self.0)`
- `GuestUser` — `guest.rs:42`, `#[serde(into = "String")]`

So a view field typed as the newtype produces **byte-identical JSON** to a
`String` field — downgrading to `String` (via `.to_string()` / `.as_str()`) buys
nothing and throws away the type safety the boundary already paid for. View
models carry the newtype, **borrowed** from the data already in hand (`serde`
implements `Serialize` for `&T`), so there is no clone and no downgrade.

Only use `String` where the domain genuinely has no newtype (see per-command
notes: profile names, `Report` free-text, timestamps, `ResourceUsage`
primitives).

### 3. Enums for every closed set, never a free string

State, backend kind, report status/source, PAT storage, probe result — each is a
fixed set of values. Model each as an enum with `#[serde(rename_all = "…")]` so
serialization is **total** (no typo'd literal can escape) and the compiler forces
exhaustive handling. Where the human path already has a `label()` method that
emits the human text, keep it and derive the JSON token from the same enum — that
is what stops the two representations drifting.

### 4. `Option<T>` for genuine absence, not sentinel strings

Where the human table prints `"unknown"` / `"none"` / omits a line, the JSON
models it as `null` / `[]`. `usage`, `created`, `storage`, `probe`, `valid` are
all `Option`.

### 5. JSON goes to stdout; tracing/human decoration stays on stderr

Per `CLAUDE.md`, tracing already goes to stderr, so `coop … --json | jq` stays
clean. **Two commands (`devcontainer check`, `up/start --dry-run`) currently
write their report to stderr** via `eprintln!` / `Report::render()`. For those,
`--json` must move the payload to **stdout** — the flag selects
`(stderr, human)` vs `(stdout, json)`, not just the format. The human path stays
on stderr, unchanged.

## The flag: per-command `--json`, not global

Add `#[arg(long)] json: bool` to each subcommand that supports it — **not** a
global flag on `Cli`. A global flag would appear on `coop up`, `coop shell`,
etc., where it does nothing, forcing a runtime rejection and lying in `--help`.
Per-command keeps clap's help honest and matches the repo's "no speculative
features" rule.

A bare `bool` is correct while there are two formats. If a third format ever
lands (`--yaml`), promote to `#[derive(ValueEnum)] enum OutputFormat { Human,
Json }` then — a mechanical change.

## Shared infrastructure

Put the cross-cutting pieces in a new module `src/commands/json.rs` (declare
`mod json;` in `src/commands/mod.rs`). Command-specific view structs live next to
their command handler.

### Shared enums

```rust
use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InstanceState { Running, Stopped }

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BackendKind { Firecracker, Lima }
```

Provide the projection from the live backend enum (match on `PlatformBackend`'s
variants):

```rust
impl BackendKind {
    pub(crate) fn of(be: &backend::PlatformBackend) -> Self { /* match variants */ }
}
```

### The JSON writer

One helper, used by every command. It is the *only* place JSON hits stdout:

```rust
/// Serialize `value` as pretty JSON to stdout, with a trailing newline.
pub(crate) fn render_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let mut out = std::io::stdout();
    serde_json::to_writer_pretty(&mut out, value)
        .map_err(|e| anyhow::anyhow!("Failed to write JSON: {e}"))?;
    writeln!(out).map_err(|e| anyhow::anyhow!("Failed to write JSON: {e}"))?;
    Ok(())
}
```

## Command classification

| Command | `--json`? | Priority | Reason |
|---|---|---|---|
| `status` | yes | **P0** | Instance state — highest automation value. |
| `list` / `ls` | yes | **P0** | Trivial subset of status; nearly free once the enum exists. |
| `images` | yes | **P1** | Machine-readable image inventory; already structured. |
| `devcontainer check` | yes | **P1** | `Report` already structured; CI branches on translation. |
| `github status` | yes | **P1** | PAT entries + validation state; secrets excluded. |
| `up --dry-run` / `start --dry-run` | yes | **P2** | Resolved plan/translation. Heaviest shape. |
| `profiles list` | yes | **P2** | Profile inventory; low automation demand. |
| `model` (no action) | status-only | **P3** | Only the readout; `local`/`remote` mutate and prompt. |
| `devcontainer status` | yes | **P3** | Lists opt-outs; minor query. |
| `validate` | maybe | **P3** | Its value *is* the human warnings; wrap only on demand. |
| `logs` | no | — | A stream; NDJSON adds ~nothing over raw text. |
| `profiles show`, `github show` | no | — | Single-item detail dumps; wrap only if a consumer appears. |
| `up`/`start`/`setup`/`quickstart`/`resize`/`commit`/`restore`/`stop`/`destroy`/`push`/`pull` | no | — | Mutations. Output is side effect + tracing (stderr). |
| `shell`/`claude`/`claude-agents`/`codex`/`exec`/`editor` | no | — | Interactive / passthrough; stdout is the guest's. |
| `ssh-config` | no | — | Emits SSH config text, not data. |
| `model local`/`remote`, `github setup-pat`/`rotate-pat`/`forget-pat`, `devcontainer ignore`/`clear` | no | — | Mutations; several prompt on a TTY. |
| `init`/`update`/`uninstall`/`completions` | no | — | Installer/meta actions. |

**Rule for the "no" set:** everything ruled out is a mutation (its output is a
side effect + tracing), an interactive passthrough (stdout belongs to the guest
process), or a text emitter. A structured *result envelope* for mutations is a
separate feature — do not conflate it with this issue.

---

## Per-command specifications

### P0 — `status`

- **Handler:** `cmd_status`, `src/commands/lifecycle.rs:1612`.
- **Clap:** add `json: bool` to the `Status` variant (`src/lib.rs:394`); thread it
  through the `Commands::Status { name, json }` arm (`src/lib.rs:1117`) into
  `cmd_status`.
- **Current output:** list mode = fixed-width table (name / state / image /
  backend + usage summary); single-instance mode = rich multi-line `String` from
  `Backend::status`.

**View model** (in `src/commands/json.rs` or beside the handler):

```rust
#[derive(Serialize)]
pub(crate) struct Usage {
    pub load_1m: f64,
    pub mem_used_mib: u64,
    pub mem_total_mib: u64,
    pub disk_used_mib: u64,
    pub disk_total_mib: u64,
}

#[derive(Serialize)]
pub(crate) struct InstanceStatus<'a> {
    pub name: &'a config::InstanceName,
    pub state: InstanceState,
    pub image: &'a config::ImageName,
    pub backend: BackendKind,
    pub usage: Option<Usage>,   // None ⇔ stopped, or query failed on a running guest
}
```

Build `Usage` from `backend::ResourceUsage` (add `#[derive(Serialize)]` to
`ResourceUsage` at `src/backend.rs:620`, or map field-by-field — either is fine;
the fields are public primitives). Emit the **raw MiB / load** values, not
derived percentages — consumers compute their own ratios, and this matches what
the type holds. The issue's `cpu_pct`/`mem_mib` example was illustrative.

- **Behavior:** bare `--json` emits a JSON **array**; `--name X --json` emits a
  single **object**, mirroring the human modes.
- **Scope decision — rich single-instance fields stay human-only.** The rich
  single-instance report (`Backend::status` in `vm.rs`/`lima.rs`) carries
  backend-*specific* fields (Firecracker: PID, guest IP; Lima: arch, disk, SSH
  port). Unifying those two differing shapes into one serializable struct is real
  cross-platform work and is **out of scope**. JSON single-instance emits the
  common `InstanceStatus` shape. The common fields (state/image/backend/usage)
  are computed from the same sources in both paths, so they never disagree; the
  human path just adds decoration. If rich JSON is wanted later, change
  `Backend::status` to return a struct — a separate change with its own
  cross-platform test cost. **Do not touch the `Backend` trait for this issue.**

**JSON example:**

```json
[
  { "name": "my-project", "state": "running", "image": "default",
    "backend": "firecracker",
    "usage": { "load_1m": 0.12, "mem_used_mib": 512, "mem_total_mib": 2048,
               "disk_used_mib": 8192, "disk_total_mib": 20480 } },
  { "name": "other", "state": "stopped", "image": "default",
    "backend": "firecracker", "usage": null }
]
```

### P0 — `list` / `ls`

- **Handler:** `cmd_list`, `src/commands/lifecycle.rs:1589`.
- **Clap:** add `json: bool` to the `List` variant (`src/lib.rs:392`, currently a
  unit variant — becomes `List { json: bool }`); update the match arm at
  `src/lib.rs:1116`.

```rust
#[derive(Serialize)]
pub(crate) struct InstanceSummary<'a> {
    pub name: &'a config::InstanceName,
    pub state: InstanceState,
}
```

Turning the unit variant `List` into `List { json: bool }` breaks two existing
clap tests that assert `matches!(cli.command, Commands::List)` (`src/lib.rs:1670`
and `:1676`) — update them to `Commands::List { .. }`.

Deliberately **smaller** than `InstanceStatus`: `cmd_list` only calls
`is_running` and does **not** run the per-instance usage SSH query that `status`
runs. Keep `list` cheap — do not add usage here. Consumers who want usage call
`status --json`. Reusing `InstanceState` keeps the two consistent where they
overlap.

```json
[ { "name": "my-project", "state": "running" },
  { "name": "other", "state": "stopped" } ]
```

### P1 — `images`

- **Handler:** `cmd_images`, `src/commands/profiles.rs:166`.
- **Clap:** add `json: bool` to the `Images` variant (`src/lib.rs:510`).

```rust
#[derive(Serialize)]
pub(crate) struct ImageInfo<'a> {
    pub name: &'a config::ImageName,
    pub profiles: Vec<String>,     // String: profile names are plain String in the domain
    pub created: Option<&'a str>,  // None where the human path prints "unknown"
    pub size_bytes: u64,           // raw bytes; human path keeps format_dir_size ("8.0 GiB")
}
```

The human path collapses three cases to strings (`"none"` / `"unknown"`); JSON
models them honestly: `profiles: []` (not `"none"`), `created: null` (not
`"unknown"`), and raw `size_bytes` instead of a formatted GiB string. `profiles`
stays `Vec<String>` because profile names have no newtype (`config.rs:678`:
`profiles: HashMap<String, CustomProfile>`).

```json
[ { "name": "default", "profiles": ["python", "node"],
    "created": "2026-06-01T12:00:00Z", "size_bytes": 8589934592 } ]
```

### P1 — `devcontainer check`

- **Handler:** report rendered at `src/commands/devcontainer.rs:230` (and the
  both-stage path at `:309`); subcommand at `src/lib.rs:699`.
- **Clap:** add `json: bool` to the `Check` variant.
- **stderr → stdout:** the human report currently goes to **stderr**
  (`eprintln!`, behind `#[expect(clippy::print_stderr)]`). With `--json`, write
  to **stdout** via `render_json`. Human path unchanged.

The `Report` types (`src/devcontainer.rs:349–427`) are already structured — add
`Serialize` derives. Their existing `label()` methods already return exactly the
strings `rename_all = "lowercase"` produces (`"applied"`, `"cli"`, …), so human
and JSON labels cannot diverge:

```rust
#[derive(Serialize, /* existing derives */)]
#[serde(rename_all = "lowercase")]
pub enum ReportStatus { Applied, Overridden, Unsupported, Invalid }

#[derive(Serialize, /* existing derives */)]
#[serde(rename_all = "lowercase")]
pub enum ReportSource { Cli, Devcontainer }

#[derive(Serialize, /* existing derives */)]
pub struct ReportEntry { pub key: String, pub status: ReportStatus,
                         pub source: ReportSource, pub value: String, pub note: String }

#[derive(Serialize, /* existing derives */)]
pub struct Report { pub entries: Vec<ReportEntry>, pub source_path: Option<PathBuf>,
                    pub ignored_paths: Vec<PathBuf> }
```

`key` / `value` / `note` stay `String` — they are free-form (a dotted
devcontainer path, a rendered value, a human note), not a closed set. Keep the
`label()` methods for the human table; the derive is purely additive.

- **`--stage both`** emits `{ "setup": <Report>, "start": <Report> }`; a single
  stage emits one `Report`. `PathBuf` serializes as a string via serde.

### P1 — `github status`

- **Handler:** `render_status`, `src/github_pat.rs:115` (already split from
  `run_status` for testability); subcommand `GithubAction::Status`,
  `src/lib.rs:682`.
- **Clap:** add `json: bool` alongside the existing `probe: bool`.
- **Never emit the token value.** Only repo, storage backend, and validation
  state.

Reuse the existing `secret_store::Backend` enum (`src/secret_store.rs:37`) for
storage — add a `Serialize` derive with a **stable machine token**
(`rename_all = "snake_case"`, distinct from its human `label()`). The enum is
`#[cfg]`-gated per OS; that is fine, you only serialize variants that exist on
the host. Turn the three `probe_status_label` strings into an enum (the
gap this closes), and have the human `label()` derive from it so the two do not
drift:

```rust
// secret_store.rs — additive derive on the existing enum:
#[derive(Serialize, /* existing */)]
#[serde(rename_all = "snake_case")]
pub enum Backend { /* MacosKeychain | LinuxSecretService | OnePassword | File */ }
// → "macos_keychain" | "linux_secret_service" | "one_password" | "file"

// github_pat.rs — replace the &'static str returned by probe_status_label:
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProbeStatus { Ok, UnexpectedFormat, ResolveFailed }

impl ProbeStatus {
    fn label(self) -> &'static str {         // human strings, unchanged
        match self {
            Self::Ok => "resolves, format ok",
            Self::UnexpectedFormat => "resolves but unexpected format",
            Self::ResolveFailed => "FAILED to resolve",
        }
    }
}
```

**View model:**

```rust
#[derive(Serialize)]
pub(crate) struct GithubStatus<'a> {
    pub mode: GithubMode,               // off | pat | other; see note
    pub entries: Vec<PatEntry<'a>>,     // empty unless pat mode
    pub skip: Vec<&'a github_repo::RepoSlug>,
}

#[derive(Serialize)]
pub(crate) struct PatEntry<'a> {
    pub repo: &'a github_repo::RepoSlug,
    pub storage: Option<secret_store::Backend>, // None where human prints "unknown"
    pub probe: Option<ProbeStatus>,             // None unless --probe
}
```

`storage` is `Option` because `CmdToken::parse` returns `Option` — `None` is the
"unknown" (unparseable `cmd:`) case. `probe` is `Option` because it is only
resolved under `--probe`.

- **`mode`:** the human path calls `GitHubAuth::mode_name()` and distinguishes
  `off` (no `github` config) / `pat` / other modes. Model `GithubMode` as an enum
  mirroring `GitHubAuth`'s discriminants plus `Off`. **Verify the exact variant
  set** against `GitHubAuth` / `mode_name()` before finalizing; it is a closed
  set, so it should be an enum, not a free string. Do **not** reuse the existing
  `impl Serialize for GitHubAuth` (`config.rs:1100`) — that serializes the full
  auth config, which may include secret material.

```json
{ "mode": "pat",
  "entries": [ { "repo": "trailofbits/coop", "storage": "macos_keychain",
                 "probe": "ok" } ],
  "skip": [] }
```

### P2 — `up --dry-run` / `start --dry-run`

- **Handlers:** dry-run early-returns in the `Up` and `Start` arms
  (`src/lib.rs:970` and `:1032`). Today the dry-run runs `resolve_devcontainer`
  (which renders the `Report` to stderr) and returns; there is no collected plan
  object.
- **Clap:** `--dry-run` already exists; add `json: bool` to `Up` and `Start`.
- This is the **heaviest** shape — the plan's pieces (translation report,
  resolved profiles, VM overrides, guest user) are computed but never assembled
  into one struct. That assembly is the bulk of the work.

```rust
#[derive(Serialize)]
pub(crate) struct DryRunPlan {
    pub report: Option<devcontainer::Report>,   // reuse P1 Serialize derives
    pub profiles: Vec<String>,                   // resolved profile names
    pub guest_user: guest::GuestUser,            // newtype, serializes as string
    pub vm: VmOverrides,
}

#[derive(Serialize)]
pub(crate) struct VmOverrides {
    pub vcpus: Option<u32>,
    pub mem_mib: Option<u32>,
    pub disk: Option<String>,   // or the DiskSize newtype if it implements Serialize
}
```

Same stderr → stdout move as `devcontainer check`. Do this **after** P1 so the
`Report` derives already exist.

### P2 — `profiles list`

- **Handler:** `cmd_profiles`, `src/commands/profiles.rs:10`; subcommand
  `ProfilesAction::List`, `src/lib.rs:658`.

```rust
#[derive(Serialize)]
pub(crate) struct ProfilesList {
    pub builtin: Vec<ProfileEntry>,
    pub custom: Vec<ProfileEntry>,
}

#[derive(Serialize)]
pub(crate) struct ProfileEntry {
    pub name: String,        // no ProfileName newtype exists
    pub summary: String,     // from builtin_summary / custom detail
}
```

### P3 — `model` (readout only), `devcontainer status`, `validate`

Lower demand; wrap only when reached. Shapes:

- **`model`** (no action, `render_status` at `src/commands/model.rs:33`): only
  the `None` action gets `--json` — `local`/`remote` mutate and prompt on a TTY.
  Carry `&InstanceName` and `&GuestUser`; `mode` reuses/mirrors the `ModelMode`
  enum (already `ValueEnum`); per-tool endpoints as `{ claude: …, codex: … }`.
- **`devcontainer status`** (`src/lib.rs:712`): `[{ "project": …, "ignored_path":
  … }]` — paths serialize as strings.
- **`validate`** (`src/lib.rs:1184`): only if a consumer appears; its value is the
  human warning text.

---

## Testing

Follow the testing and mutation-testing rules in [`testing.md`](testing.md).

### Unit tests — assert the serialized shape

For each view model, add a test that serializes a known value and asserts the
JSON (via `serde_json::to_value` and comparison, or `to_string`). This pins the
contract — field names, enum tokens, `null` vs omitted, array vs object. Test the
edges:

- `status`: running-with-usage, running-with-`usage: null` (query failed),
  stopped; array (list) vs object (single `--name`).
- `github status`: `mode: off`, `pat` with entries, unparseable `storage: null`,
  `probe` present vs absent.
- `images`: `profiles: []`, `created: null`.
- enum round-trips: each `rename_all` token is what you expect
  (`"firecracker"`, `"macos_keychain"`, `"unexpected_format"`, …).

Also test the pure projections you add — e.g. `BackendKind::of`,
`From<probe result> for ProbeStatus`, `GithubMode` from `GitHubAuth`.

### Mutation testing — keep `.cargo/mutants.toml` in sync **in the same PR**

[`testing.md`](testing.md) is emphatic about this (see the `coop model` #352
regression). When you add these functions:

- **`render_json`** and any handler branch that writes JSON to stdout are **IO** —
  add an `exclude_re` entry (they only write stdout; a `--lib` test cannot observe
  them). Anchor patterns with `\b`.
- **View builders and pure projections** (`BackendKind::of`, `ProbeStatus`
  mapping, the struct-assembly helpers, `GithubMode` derivation) are **pure
  logic** — leave them **in** scope and cover them with the shape tests above. A
  survivor there is a real gap.
- `#[serde(...)]` derives are not meaningfully mutated; the shape tests are what
  guard them.

Verify with `cargo mutants -f <touched files> -- --lib` (full-file sweep, not
just `--in-diff`), per [`testing.md`](testing.md).

### Integration test

Consider one JSON assertion in `tests/integration.sh` for `status --json` piped
through `jq` (e.g. `coop status --json | jq -e '.[0].state'`), exercising the
real stdout/stderr split on both backends.

## Suggested phasing

1. **P0 (this issue #385):** `status` + `list/ls` together — they share
   `InstanceState` and get the shared `json` module + `render_json` helper in
   place. Barely more than `status` alone.
2. **P1:** `images`, `devcontainer check`, `github status` — mostly
   derive-and-branch on already-structured data; `check` also does the
   stderr → stdout move.
3. **P2:** `up/start --dry-run`, `profiles list` — real assembly / lower demand.
4. **P3 (on demand only):** `model` readout, `devcontainer status`, `validate`.

## Per-command implementation checklist

For each command you add:

- [ ] Add `json: bool` to the clap variant in `src/lib.rs`; thread it through the
      match arm into the handler.
- [ ] Define the view model with newtypes (borrowed) and enums; `String` only
      where the domain has no newtype.
- [ ] Extract a pure builder from the handler's IO so the assembly is unit-testable.
- [ ] Branch the handler: `render_json(&view)?` vs the existing human formatter.
- [ ] For `check` / `--dry-run`: move the JSON payload to stdout (human stays on
      stderr).
- [ ] Add shape tests (edges: `null`, `[]`, array vs object, enum tokens).
- [ ] Update `.cargo/mutants.toml` (exclude IO/stdout writers; keep pure builders
      in scope) **in the same PR**.
- [ ] Run `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`,
      `cargo test`, and `cargo mutants -f <touched> -- --lib`.
