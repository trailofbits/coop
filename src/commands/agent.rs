//! `coop agent update` — refresh the coding-agent binaries inside a running VM.
//!
//! Both agents are installed "latest at build time" during `coop setup`, so
//! they go stale in long-running VMs and in VMs created from an old image.
//! This command updates them in place against a *running* instance, without
//! rebuilding the golden image (`coop setup --rebuild` remains the path for
//! refreshing the image itself).
//!
//! The two agents differ in how they update, and the difference is encoded in
//! [`UpdateStrategy`] so no caller can run the wrong one:
//!
//! - **Codex** is a root-owned package exposed through `/usr/local/bin` and has
//!   no background updater. coop re-runs its own installer
//!   ([`guest::SCRIPT_CODEX`]) as root with `COOP_FORCE_INSTALL=1` to install
//!   the current release and atomically switch its entrypoints.
//! - **Claude Code** lives in the guest user's `~/.local/bin` and already
//!   auto-updates in the background. `coop agent update --claude` just runs
//!   `claude update` synchronously as the guest user — a convenience, not a
//!   fix.

use std::io::Write as _;

use anyhow::{Context, Result, bail};
use semver::Version;

use crate::backend::{self, SshSession};
use crate::paths::GuestPath;
use crate::remote_command::RemoteCommand;
use crate::{config, guest, prompt, update};

use super::{prepare_session_from_target, resolve_running};

/// Options for `coop agent update`, parsed from the CLI flags.
pub(crate) struct AgentUpdateOpts {
    pub selection: AgentSelection,
    pub check: bool,
    pub yes: bool,
}

/// The `openai/codex` release feed coop compares the guest binary against.
const CODEX_REPO: &str = "openai/codex";

// ── Domain types ──────────────────────────────────────────────

/// A coding agent coop can update inside the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// Human-facing label used in prompts, reports, and errors.
    fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// How this agent's binary is refreshed inside the guest. The
    /// root-vs-user asymmetry lives here so a caller can't run Claude's
    /// self-update as root or Codex's reinstall without sudo.
    fn strategy(self) -> UpdateStrategy {
        match self {
            Self::Claude => UpdateStrategy::SelfUpdate,
            Self::Codex => UpdateStrategy::ReinstallAsRoot {
                script: guest::SCRIPT_CODEX,
            },
        }
    }
}

/// How an agent's binary is refreshed in the guest.
enum UpdateStrategy {
    /// Re-run coop's own installer as root, forcing overwrite of the
    /// root-owned binary. Carries the embedded installer script.
    ReinstallAsRoot { script: &'static str },
    /// Invoke the agent's own updater as the guest user (no sudo).
    SelfUpdate,
}

/// Which agents a single `coop agent update` invocation targets. No variant
/// can represent "update nothing", so [`agents`](Self::agents) is always
/// non-empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentSelection {
    Claude,
    Codex,
    Both,
}

impl AgentSelection {
    /// Map the two CLI booleans to a selection. Selection is additive: no
    /// flag, or both flags, means both agents.
    pub(crate) fn from_flags(claude: bool, codex: bool) -> Self {
        match (claude, codex) {
            (true, false) => Self::Claude,
            (false, true) => Self::Codex,
            _ => Self::Both,
        }
    }

    fn agents(self) -> &'static [Agent] {
        match self {
            Self::Claude => &[Agent::Claude],
            Self::Codex => &[Agent::Codex],
            Self::Both => &[Agent::Claude, Agent::Codex],
        }
    }
}

/// A parsed agent version. The constructor extracts the first semver-looking
/// token from arbitrary `--version` / `tag_name` output, so callers compare
/// with `<` rather than string-diffing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AgentVersion(Version);

impl AgentVersion {
    /// Extract a semver version from free-form text. Handles bare `1.2.3`,
    /// `v1.2.3`, tool-prefixed `codex-cli 0.5.0`, and dash-separated tags
    /// like `rust-v0.42.0`. Returns `Err` when no token parses.
    ///
    /// The *first* semver-parseable whitespace token wins, which matches the
    /// `--version` output of both agents today (the version leads or is the
    /// second token). A future format that emitted another semver-shaped
    /// token first would mis-pick; the unit tests pin the current shapes.
    fn parse(raw: &str) -> Result<Self> {
        raw.split_whitespace()
            .find_map(Self::from_token)
            .map(Self)
            .with_context(|| format!("no semver version found in {raw:?}"))
    }

    /// Try to read a version out of one whitespace-delimited token, first as
    /// the whole token (minus a leading `v`), then as the suffix after the
    /// last `v` (for tags such as `rust-v0.42.0`).
    fn from_token(token: &str) -> Option<Version> {
        let stripped = token.strip_prefix('v').unwrap_or(token);
        if let Ok(v) = Version::parse(stripped) {
            return Some(v);
        }
        let after_v = token.rsplit_once('v').map(|(_, rest)| rest)?;
        Version::parse(after_v).ok()
    }
}

impl std::fmt::Display for AgentVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Result of updating one agent.
enum UpdateOutcome {
    Updated {
        from: Option<AgentVersion>,
        to: AgentVersion,
    },
    AlreadyCurrent {
        version: AgentVersion,
    },
}

/// Version-comparison result for `--check`, never a sentinel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckStatus {
    UpToDate,
    UpdateAvailable,
    /// Claude Code updates itself in the background — coop does not track a
    /// "latest" for it.
    AutoUpdates,
    /// Installed or latest version could not be determined.
    Unknown,
}

/// One row of the `--check` report.
struct CheckRow {
    agent: Agent,
    installed: Option<AgentVersion>,
    latest: Option<AgentVersion>,
    status: CheckStatus,
}

// ── Command entry point ───────────────────────────────────────

pub(crate) fn cmd_agent_update(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    opts: &AgentUpdateOpts,
) -> Result<()> {
    // Resolve the running instance once (a stopped/missing one errors here
    // with the shared "not running" guidance), then build the SSH session
    // from it — this mirrors `open_ssh_session` but keeps the instance name
    // for the confirmation prompt and messages without a second lookup.
    let running = resolve_running(be, cfg, name)?;
    let inst_name = running.instance().name.to_string();
    let repo = backend::detect_instance_repo(running.instance());
    let (inst, target) = running.into_parts();
    let session = prepare_session_from_target(cfg, Some(&inst), target, repo.as_ref())?;

    if opts.check {
        return run_check(&session, opts.selection);
    }

    if !opts.yes
        && !prompt::confirm(&format!(
            "Update {} in '{inst_name}' to the latest version?",
            selection_phrase(opts.selection),
        ))?
    {
        tracing::info!("Update cancelled");
        return Ok(());
    }

    run_updates(&session, opts.selection)
}

/// Comma/and-joined agent names for the confirmation prompt.
fn selection_phrase(selection: AgentSelection) -> String {
    let labels: Vec<&str> = selection.agents().iter().map(|a| a.display()).collect();
    labels.join(" and ")
}

// ── Update path ───────────────────────────────────────────────

/// Update every selected agent, printing each result. Continues past a
/// per-agent failure and returns an error only after all have run, so a
/// `Both` update reports both outcomes even when one fails.
fn run_updates(session: &SshSession, selection: AgentSelection) -> Result<()> {
    let out = &mut std::io::stdout();
    let mut failed = false;
    for &agent in selection.agents() {
        match update_one(session, agent) {
            Ok(outcome) => {
                writeln!(out, "{}", outcome_line(agent, &outcome))?;
                if agent == Agent::Claude {
                    writeln!(
                        out,
                        "  note: Claude Code also auto-updates in the background."
                    )?;
                }
            }
            Err(e) => {
                failed = true;
                writeln!(out, "{}: update failed — {e:#}", agent.display())?;
            }
        }
    }
    if failed {
        bail!("one or more agents failed to update");
    }
    Ok(())
}

/// Update a single agent and verify the result by re-reading its version.
fn update_one(session: &SshSession, agent: Agent) -> Result<UpdateOutcome> {
    let before = capture_version(session, agent);
    match agent.strategy() {
        UpdateStrategy::ReinstallAsRoot { script } => reinstall_as_root(session, script)
            .with_context(|| format!("failed to reinstall {}", agent.display()))?,
        UpdateStrategy::SelfUpdate => {
            self_update(session, agent)
                .with_context(|| format!("failed to update {}", agent.display()))?;
        }
    }
    // A readable version after the update doubles as the executable check:
    // `capture_version` runs `<bin> --version` over SSH, which fails if the
    // binary is missing or not runnable.
    let after = capture_version(session, agent).with_context(|| {
        format!(
            "could not read {} version after update — the binary may be missing or broken",
            agent.display(),
        )
    })?;
    Ok(if before.as_ref() == Some(&after) {
        UpdateOutcome::AlreadyCurrent { version: after }
    } else {
        UpdateOutcome::Updated {
            from: before,
            to: after,
        }
    })
}

/// Re-run an embedded installer script as root with the force flag set,
/// piping the script over stdin so it never lands on argv.
fn reinstall_as_root(session: &SshSession, script: &str) -> Result<()> {
    session.target.exec_with_stdin(
        RemoteCommand::new().literal("sudo env COOP_FORCE_INSTALL=1 bash -s"),
        script.as_bytes().to_vec(),
    )
}

/// Run an agent's own updater as the guest user (no sudo).
fn self_update(session: &SshSession, agent: Agent) -> Result<()> {
    let bin = agent_binary(session, agent)?;
    session
        .target
        .exec(RemoteCommand::new().arg(bin).literal(" update"))
}

// ── Check path ────────────────────────────────────────────────

/// Report installed-vs-latest versions for the selected agents. Mutates
/// nothing; degrades to `Unknown` when a version can't be determined.
fn run_check(session: &SshSession, selection: AgentSelection) -> Result<()> {
    let rows: Vec<CheckRow> = selection
        .agents()
        .iter()
        .map(|&agent| check_row(session, agent))
        .collect();
    let out = &mut std::io::stdout();
    for line in check_report(&rows) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Gather one agent's installed/latest versions and classify them.
fn check_row(session: &SshSession, agent: Agent) -> CheckRow {
    let installed = capture_version(session, agent);
    let latest = match agent {
        Agent::Claude => None,
        Agent::Codex => codex_latest(),
    };
    let status = check_status(agent, installed.as_ref(), latest.as_ref());
    CheckRow {
        agent,
        installed,
        latest,
        status,
    }
}

/// Best-effort lookup of the newest Codex release tag. Network failures
/// degrade to `None` (reported as `Unknown`) rather than aborting the check.
fn codex_latest() -> Option<AgentVersion> {
    match update::latest_release_tag(CODEX_REPO) {
        Ok(tag) => AgentVersion::parse(&tag).ok(),
        Err(e) => {
            tracing::debug!("Failed to look up latest Codex release: {e:#}");
            None
        }
    }
}

/// Classify an agent's installed version against the latest known one.
fn check_status(
    agent: Agent,
    installed: Option<&AgentVersion>,
    latest: Option<&AgentVersion>,
) -> CheckStatus {
    match agent {
        Agent::Claude => CheckStatus::AutoUpdates,
        Agent::Codex => match (installed, latest) {
            (Some(i), Some(l)) if i < l => CheckStatus::UpdateAvailable,
            (Some(_), Some(_)) => CheckStatus::UpToDate,
            _ => CheckStatus::Unknown,
        },
    }
}

// ── Pure formatting ───────────────────────────────────────────

/// One line describing an update outcome.
fn outcome_line(agent: Agent, outcome: &UpdateOutcome) -> String {
    let label = agent.display();
    match outcome {
        UpdateOutcome::Updated {
            from: Some(from),
            to,
        } => {
            format!("{label}: updated {from} → {to}")
        }
        UpdateOutcome::Updated { from: None, to } => format!("{label}: updated to {to}"),
        UpdateOutcome::AlreadyCurrent { version } => {
            format!("{label}: already at the latest version ({version})")
        }
    }
}

/// Build the `--check` report lines: one aligned row per agent.
fn check_report(rows: &[CheckRow]) -> Vec<String> {
    rows.iter().map(check_line).collect()
}

fn check_line(row: &CheckRow) -> String {
    let installed = row
        .installed
        .as_ref()
        .map_or_else(|| "?".to_string(), AgentVersion::to_string);
    let (version_col, note) = match row.status {
        CheckStatus::UpToDate => (installed, "up to date".to_string()),
        CheckStatus::UpdateAvailable => {
            let latest = row
                .latest
                .as_ref()
                .map_or_else(|| "?".to_string(), AgentVersion::to_string);
            (
                format!("{installed} → {latest}"),
                "update available — run: coop agent update --codex".to_string(),
            )
        }
        CheckStatus::AutoUpdates => (
            installed,
            "up to date (auto-updates in background)".to_string(),
        ),
        CheckStatus::Unknown => (installed, "could not determine latest version".to_string()),
    };
    let label = row.agent.display();
    format!("{label:<12} {version_col:<16} {note}")
}

// ── Guest binary resolution + version capture (IO) ────────────

/// Absolute guest path of an agent's binary. Claude lives under the guest
/// user's home; Codex is system-wide.
fn agent_binary(session: &SshSession, agent: Agent) -> Result<GuestPath> {
    Ok(match agent {
        Agent::Claude => guest::GuestUser::new(session.target.user.as_ref())?.claude_bin(),
        Agent::Codex => guest::codex_bin(),
    })
}

/// Read an agent's installed version over SSH, or `None` if the binary is
/// absent or its output doesn't parse. The path is derived from a validated
/// guest user, so it carries no shell metacharacters.
fn capture_version(session: &SshSession, agent: Agent) -> Option<AgentVersion> {
    let bin = agent_binary(session, agent).ok()?;
    let raw = session.target.capture(&format!("{bin} --version")).ok()?;
    AgentVersion::parse(&raw).ok()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    fn ver(s: &str) -> AgentVersion {
        AgentVersion::parse(s).unwrap()
    }

    // ── selection ──────────────────────────────────────────────

    #[test]
    fn from_flags_maps_all_four_combinations() {
        assert_eq!(
            AgentSelection::from_flags(true, false),
            AgentSelection::Claude
        );
        assert_eq!(
            AgentSelection::from_flags(false, true),
            AgentSelection::Codex
        );
        assert_eq!(
            AgentSelection::from_flags(false, false),
            AgentSelection::Both
        );
        assert_eq!(AgentSelection::from_flags(true, true), AgentSelection::Both);
    }

    #[test]
    fn agents_is_never_empty_and_both_lists_two() {
        assert_eq!(AgentSelection::Claude.agents(), &[Agent::Claude]);
        assert_eq!(AgentSelection::Codex.agents(), &[Agent::Codex]);
        assert_eq!(
            AgentSelection::Both.agents(),
            &[Agent::Claude, Agent::Codex]
        );
    }

    #[test]
    fn selection_phrase_joins_with_and() {
        assert_eq!(selection_phrase(AgentSelection::Claude), "Claude Code");
        assert_eq!(selection_phrase(AgentSelection::Codex), "Codex");
        assert_eq!(
            selection_phrase(AgentSelection::Both),
            "Claude Code and Codex"
        );
    }

    // ── version parsing ────────────────────────────────────────

    #[test]
    fn parse_handles_bare_and_v_prefixed() {
        assert_eq!(ver("1.2.3"), ver("v1.2.3"));
        assert_eq!(ver("0.5.0").to_string(), "0.5.0");
    }

    #[test]
    fn parse_extracts_version_from_tool_prefixed_output() {
        assert_eq!(ver("codex-cli 0.5.0"), ver("0.5.0"));
        assert_eq!(ver("claude 1.2.3 (Claude Code)"), ver("1.2.3"));
    }

    #[test]
    fn parse_extracts_version_from_dashed_tag() {
        assert_eq!(ver("rust-v0.42.0"), ver("0.42.0"));
    }

    #[test]
    fn parse_preserves_prerelease() {
        let v = ver("1.0.0-rc.1");
        assert_eq!(v.to_string(), "1.0.0-rc.1");
        assert!(ver("1.0.0") > v, "release must sort above its prerelease");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(AgentVersion::parse("no version here").is_err());
        assert!(AgentVersion::parse("").is_err());
        assert!(AgentVersion::parse("v1").is_err());
    }

    #[test]
    fn version_ordering_is_semver_not_lexical() {
        assert!(ver("0.10.0") > ver("0.9.0"));
        assert!(ver("1.0.0") > ver("0.42.0"));
    }

    // ── check classification ───────────────────────────────────

    #[test]
    fn claude_always_reports_auto_updates() {
        assert_eq!(
            check_status(Agent::Claude, Some(&ver("1.2.3")), None),
            CheckStatus::AutoUpdates
        );
    }

    #[test]
    fn codex_update_available_when_installed_is_older() {
        assert_eq!(
            check_status(Agent::Codex, Some(&ver("0.4.1")), Some(&ver("0.5.0"))),
            CheckStatus::UpdateAvailable
        );
    }

    #[test]
    fn codex_up_to_date_when_equal_or_newer() {
        assert_eq!(
            check_status(Agent::Codex, Some(&ver("0.5.0")), Some(&ver("0.5.0"))),
            CheckStatus::UpToDate
        );
        assert_eq!(
            check_status(Agent::Codex, Some(&ver("0.6.0")), Some(&ver("0.5.0"))),
            CheckStatus::UpToDate
        );
    }

    #[test]
    fn codex_unknown_when_either_version_missing() {
        assert_eq!(
            check_status(Agent::Codex, None, Some(&ver("0.5.0"))),
            CheckStatus::Unknown
        );
        assert_eq!(
            check_status(Agent::Codex, Some(&ver("0.5.0")), None),
            CheckStatus::Unknown
        );
    }

    // ── report lines ───────────────────────────────────────────

    #[test]
    fn check_line_update_available_shows_arrow_and_command() {
        let row = CheckRow {
            agent: Agent::Codex,
            installed: Some(ver("0.4.1")),
            latest: Some(ver("0.5.0")),
            status: CheckStatus::UpdateAvailable,
        };
        let line = check_line(&row);
        assert!(line.contains("0.4.1 → 0.5.0"), "{line}");
        assert!(line.contains("coop agent update --codex"), "{line}");
    }

    #[test]
    fn check_line_up_to_date_shows_installed_only() {
        let row = CheckRow {
            agent: Agent::Codex,
            installed: Some(ver("0.5.0")),
            latest: Some(ver("0.5.0")),
            status: CheckStatus::UpToDate,
        };
        let line = check_line(&row);
        assert!(line.contains("0.5.0"), "{line}");
        assert!(line.contains("up to date"), "{line}");
        assert!(!line.contains("→"), "{line}");
    }

    #[test]
    fn check_line_auto_updates_notes_background() {
        let row = CheckRow {
            agent: Agent::Claude,
            installed: Some(ver("1.2.3")),
            latest: None,
            status: CheckStatus::AutoUpdates,
        };
        let line = check_line(&row);
        assert!(line.contains("Claude Code"), "{line}");
        assert!(line.contains("1.2.3"), "{line}");
        assert!(line.contains("auto-updates in background"), "{line}");
    }

    #[test]
    fn check_line_unknown_shows_placeholder() {
        let row = CheckRow {
            agent: Agent::Codex,
            installed: None,
            latest: None,
            status: CheckStatus::Unknown,
        };
        let line = check_line(&row);
        assert!(line.contains('?'), "{line}");
        assert!(line.contains("could not determine"), "{line}");
    }

    #[test]
    fn check_report_has_one_line_per_agent() {
        let rows = vec![
            CheckRow {
                agent: Agent::Claude,
                installed: Some(ver("1.2.3")),
                latest: None,
                status: CheckStatus::AutoUpdates,
            },
            CheckRow {
                agent: Agent::Codex,
                installed: Some(ver("0.4.1")),
                latest: Some(ver("0.5.0")),
                status: CheckStatus::UpdateAvailable,
            },
        ];
        assert_eq!(check_report(&rows).len(), 2);
    }

    // ── outcome lines ──────────────────────────────────────────

    #[test]
    fn outcome_line_updated_from_known_version() {
        let outcome = UpdateOutcome::Updated {
            from: Some(ver("0.4.1")),
            to: ver("0.5.0"),
        };
        let line = outcome_line(Agent::Codex, &outcome);
        assert!(line.contains("Codex"), "{line}");
        assert!(line.contains("0.4.1 → 0.5.0"), "{line}");
    }

    #[test]
    fn outcome_line_updated_from_unknown_version() {
        let outcome = UpdateOutcome::Updated {
            from: None,
            to: ver("0.5.0"),
        };
        let line = outcome_line(Agent::Codex, &outcome);
        assert!(line.contains("updated to 0.5.0"), "{line}");
    }

    #[test]
    fn outcome_line_already_current() {
        let outcome = UpdateOutcome::AlreadyCurrent {
            version: ver("0.5.0"),
        };
        let line = outcome_line(Agent::Codex, &outcome);
        assert!(line.contains("already at the latest"), "{line}");
        assert!(line.contains("0.5.0"), "{line}");
    }

    #[test]
    fn strategy_matches_agent_asymmetry() {
        assert!(matches!(
            Agent::Codex.strategy(),
            UpdateStrategy::ReinstallAsRoot { .. }
        ));
        assert!(matches!(
            Agent::Claude.strategy(),
            UpdateStrategy::SelfUpdate
        ));
    }
}
