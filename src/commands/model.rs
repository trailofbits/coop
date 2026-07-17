//! `coop model` — switch a VM between cloud and local model backends.
//!
//! The selection is persisted per-VM ([`crate::model_state`]) and
//! materialized as guest config files (Claude `settings.json` env block,
//! Codex `config.toml` provider block) so every launch path picks it up.
//! Switching is a cheap SSH file write against the running VM — no
//! rebuild — and the persisted state is re-applied on the next start.

use std::io::Write;

use anyhow::{Result, bail};

use crate::backend::VmBackend as _;
use crate::config::{LocalModel, Secret};
use crate::model_state::{ModelMode, ModelState};
use crate::{backend, config, network, prompt};

pub(crate) fn cmd_model(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    action: Option<ModelMode>,
) -> Result<()> {
    let inst = cfg.resolve_instance(name)?;
    match action {
        None => render_status(be, cfg, &inst),
        Some(ModelMode::Local) => set_local(be, cfg, &inst),
        Some(ModelMode::Remote) => set_remote(be, cfg, &inst),
    }
}

/// Print the per-tool mode and the endpoint each tool resolves to.
fn render_status(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
) -> Result<()> {
    let state = ModelState::load_or_default(inst)?;
    let guest_host = be.guest_host_address(&cfg.network);
    let out = &mut std::io::stdout();
    writeln!(out, "Instance: {}", inst.name)?;
    writeln!(out, "Mode:     {}", state.mode.as_str())?;
    write_tool_line(
        out,
        "Claude",
        state.mode,
        state.resolved_claude(&cfg.claude),
        &guest_host,
    )?;
    write_tool_line(
        out,
        "Codex",
        state.mode,
        state.resolved_codex(&cfg.codex),
        &guest_host,
    )?;
    Ok(())
}

fn write_tool_line(
    out: &mut impl Write,
    label: &str,
    mode: ModelMode,
    endpoint: Option<&LocalModel>,
    guest_host: &str,
) -> Result<()> {
    match (mode, endpoint) {
        (ModelMode::Local, Some(ep)) => {
            let url = network::rewrite_host_url(ep.host_url(), guest_host)?;
            writeln!(out, "{label:<9} local — {} @ {}", ep.model(), url)?;
        }
        (ModelMode::Local, None) => {
            writeln!(out, "{label:<9} cloud (no local endpoint configured)")?;
        }
        (ModelMode::Remote, _) => writeln!(out, "{label:<9} cloud")?,
    }
    Ok(())
}

fn set_local(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
) -> Result<()> {
    let mut state = ModelState::load_or_default(inst)?;
    state.mode = ModelMode::Local;

    // Offer to configure each tool that doesn't already resolve an
    // endpoint. This covers both the fresh "nothing configured yet" case
    // and the "one tool is local, the other is still on cloud" case —
    // declining leaves that tool on cloud. Non-interactive stdin declines
    // automatically.
    let (prompt_claude, prompt_codex) = tools_needing_prompt(&state, &cfg.claude, &cfg.codex);
    if prompt_claude && let Some(ep) = prompt_endpoint("Claude")? {
        state.claude_endpoint = Some(ep);
    }
    if prompt_codex && let Some(ep) = prompt_endpoint("Codex")? {
        state.codex_endpoint = Some(ep);
    }

    if state.resolved_claude(&cfg.claude).is_none() && state.resolved_codex(&cfg.codex).is_none() {
        bail!(
            "No local model endpoint configured for '{}'.\n\
             Set [claude.local_model] or [codex.local_model] in config.toml, \
             or run `coop model {} local` in an interactive terminal to enter one.",
            inst.name,
            inst.name,
        );
    }

    // Remember that coop now owns Codex's config.toml model keys, so a
    // later switch to remote rewrites a clean file even if the configured
    // endpoint is later removed (Claude's settings.json is unconditional).
    if state.resolved_codex(&cfg.codex).is_some() {
        state.codex_materialized = true;
    }

    state.save(inst)?;
    let applied = apply_to_running(be, cfg, inst)?;
    report_switch(inst, cfg, &state, applied)
}

fn set_remote(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
) -> Result<()> {
    let mut state = ModelState::load_or_default(inst)?;
    // Keep any saved endpoints so a later `local` doesn't re-prompt; only
    // the mode flips, which drops the materialized guest config.
    state.mode = ModelMode::Remote;
    state.save(inst)?;
    let applied = apply_to_running(be, cfg, inst)?;
    report_switch(inst, cfg, &state, applied)
}

/// Tools that still need an interactive endpoint prompt when switching to
/// local — those that don't already resolve one (from
/// `[claude.local_model]`/`[codex.local_model]` in config.toml or a saved
/// prompt answer). Returned as `(claude, codex)`. The two are independent,
/// so configuring one tool now and adding the other later both work.
fn tools_needing_prompt(
    state: &ModelState,
    claude: &config::ClaudeConfig,
    codex: &config::CodexConfig,
) -> (bool, bool) {
    (
        state.resolved_claude(claude).is_none(),
        state.resolved_codex(codex).is_none(),
    )
}

/// Tell the user what changed and what (if anything) they need to do.
///
/// Reports each tool independently, because a switch to local only routes
/// the tools that resolve an endpoint — any tool left on cloud gets an
/// explicit warning so "now uses a local model" never hides a half-applied
/// switch. Switching never requires a VM restart (the guest config is
/// rewritten live over SSH), but an already-running agent reads its config
/// at launch, so the only follow-up is to relaunch `claude`/`codex`.
fn report_switch(
    inst: &config::Instance,
    cfg: &config::CoopConfig,
    state: &ModelState,
    applied: bool,
) -> Result<()> {
    let local = state.mode == ModelMode::Local;
    let claude_local = local && state.resolved_claude(&cfg.claude).is_some();
    let codex_local = local && state.resolved_codex(&cfg.codex).is_some();
    let out = &mut std::io::stdout();
    for line in switch_report_lines(
        inst.name.as_str(),
        state.mode,
        claude_local,
        codex_local,
        applied,
    ) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Build the lines printed after a model switch: a per-tool summary and,
/// in local mode, a warning for any tool left on cloud. Pure so it can be
/// unit-tested without a backend, TTY, or stdout. In local mode at least
/// one tool is always local ([`set_local`] bails otherwise).
fn switch_report_lines(
    inst_name: &str,
    mode: ModelMode,
    claude_local: bool,
    codex_local: bool,
    applied: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    match mode {
        ModelMode::Remote => lines.push(format!(
            "'{inst_name}' now uses cloud models for Claude and Codex."
        )),
        ModelMode::Local => {
            let tools = [
                ("Claude", "claude.local_model", claude_local),
                ("Codex", "codex.local_model", codex_local),
            ];
            let local: Vec<&str> = tools
                .iter()
                .filter_map(|&(label, _, is_local)| is_local.then_some(label))
                .collect();
            lines.push(format!(
                "'{inst_name}' now uses a local model for {}.",
                local.join(" and ")
            ));
            for &(label, key, is_local) in &tools {
                if !is_local {
                    lines.push(format!(
                        "  warning: {label} stays on cloud — no local endpoint configured; \
                         set [{key}] in config.toml or re-run `coop model {inst_name} local` \
                         in an interactive terminal."
                    ));
                }
            }
        }
    }
    lines.push(if applied {
        format!(
            "Applied to the running VM — no restart needed; relaunch claude/codex \
             (e.g. `coop claude {inst_name}`) to pick it up."
        )
    } else {
        "Saved — applies on next start.".to_string()
    });
    lines
}

/// Re-materialize guest config for the current model state. Returns `true`
/// when the VM was running and the change was applied live, `false` when
/// the VM is stopped and the persisted state will apply on next start.
fn apply_to_running(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
) -> Result<bool> {
    let Some(running) = be.as_running(cfg, inst.clone())? else {
        return Ok(false);
    };
    let repo = backend::detect_instance_repo(running.instance());
    let (inst, target) = running.into_parts();
    let session = super::prepare_session_from_target(cfg, Some(&inst), target, repo.as_ref())?;
    let guest_host = be.guest_host_address(&cfg.network);
    let proxy_bind_ip = be.proxy_bind_ip(&cfg.network);
    backend::bootstrap_agents(
        &session,
        cfg,
        &inst,
        backend::BootMode::Restart,
        &guest_host,
        proxy_bind_ip,
    )?;
    Ok(true)
}

/// Interactively collect a local endpoint for `tool`. Returns `None` when
/// the user declines or stdin is not a TTY.
fn prompt_endpoint(tool: &str) -> Result<Option<LocalModel>> {
    if !prompt::confirm(&format!("Configure a local model for {tool}?"))? {
        return Ok(None);
    }
    let Some(host_url) =
        prompt::read_line(&format!("{tool} host URL (e.g. http://localhost:11434)"))?
    else {
        bail!("{tool} host URL is required");
    };
    let url = url::Url::parse(&host_url)
        .map_err(|e| anyhow::anyhow!("invalid {tool} host URL '{host_url}': {e}"))?;
    let Some(model) = prompt::read_line(&format!("{tool} model name"))? else {
        bail!("{tool} model name is required");
    };
    let token = prompt::read_line(&format!(
        "{tool} auth token (optional — press enter to skip)"
    ))?;
    let endpoint = LocalModel::new(url, model, token.map(Secret::new))?;
    Ok(Some(endpoint))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::config::{ClaudeConfig, CodexConfig};

    fn endpoint(url: &str) -> LocalModel {
        LocalModel::new(url::Url::parse(url).unwrap(), "m".to_string(), None).unwrap()
    }

    fn claude_with_local() -> ClaudeConfig {
        ClaudeConfig {
            local_model: Some(endpoint("http://localhost:1/")),
            ..Default::default()
        }
    }

    // ── gating: which tools get an interactive prompt ──────────

    #[test]
    fn prompts_both_tools_when_nothing_configured() {
        let state = ModelState::default();
        assert_eq!(
            tools_needing_prompt(&state, &ClaudeConfig::default(), &CodexConfig::default()),
            (true, true)
        );
    }

    #[test]
    fn prompts_only_claude_when_codex_already_configured() {
        // The reported scenario: Codex was set up first (saved endpoint),
        // so switching to local must still offer to configure Claude.
        let state = ModelState {
            codex_endpoint: Some(endpoint("http://localhost:2/")),
            ..Default::default()
        };
        assert_eq!(
            tools_needing_prompt(&state, &ClaudeConfig::default(), &CodexConfig::default()),
            (true, false)
        );
    }

    #[test]
    fn prompts_neither_tool_when_both_resolve() {
        let codex = CodexConfig {
            local_model: Some(endpoint("http://localhost:3/")),
            ..Default::default()
        };
        assert_eq!(
            tools_needing_prompt(&ModelState::default(), &claude_with_local(), &codex),
            (false, false)
        );
    }

    // ── per-tool switch reporting ──────────────────────────────

    #[test]
    fn local_report_lists_both_tools_and_warns_about_none() {
        let lines = switch_report_lines("dev", ModelMode::Local, true, true, true);
        assert!(lines[0].contains("local model for Claude and Codex"));
        assert!(!lines.iter().any(|l| l.contains("stays on cloud")));
        assert!(lines.last().unwrap().contains("relaunch"));
    }

    #[test]
    fn local_report_warns_about_the_tool_left_on_cloud() {
        let lines = switch_report_lines("dev", ModelMode::Local, true, false, true);
        assert!(lines[0].contains("local model for Claude."));
        let warning = lines.iter().find(|l| l.contains("stays on cloud")).unwrap();
        assert!(warning.contains("Codex"));
        assert!(warning.contains("codex.local_model"));
        // The tool that did switch must not be warned about.
        assert!(!warning.contains("Claude"));
    }

    #[test]
    fn remote_report_has_no_cloud_warnings() {
        let lines = switch_report_lines("dev", ModelMode::Remote, false, false, false);
        assert!(lines[0].contains("cloud models"));
        assert!(!lines.iter().any(|l| l.contains("stays on cloud")));
        assert!(lines.last().unwrap().contains("next start"));
    }
}
