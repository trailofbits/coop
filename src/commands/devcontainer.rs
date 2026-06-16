//! devcontainer.json discovery, translation, and the `coop devcontainer` subcommands.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    DevcontainerCheckStage, DevcontainerCommands, config, devcontainer, devcontainer_oci,
    git_repo_devcontainer, guest, prompt,
};

/// Whether and how a `devcontainer.json` should be resolved.
///
/// Models the mutually exclusive `--devcontainer PATH` and `--no-devcontainer`
/// flags as a single value, so "explicit path *and* disabled" cannot be
/// represented.
pub(crate) enum DevcontainerInput {
    /// `--devcontainer PATH`: use this exact file, skipping discovery and the
    /// prompt.
    Explicit(PathBuf),
    /// `--no-devcontainer`: skip discovery entirely.
    Disabled,
    /// Default: discover a `devcontainer.json`, then prompt before applying.
    Discover,
}

impl DevcontainerInput {
    /// Build from the parsed `--devcontainer` / `--no-devcontainer` flags.
    ///
    /// clap enforces their mutual exclusion at parse time; should both ever
    /// arrive, `--no-devcontainer` wins (matching the opt-out's precedence).
    pub(crate) fn from_flags(path: Option<PathBuf>, no_devcontainer: bool) -> Self {
        match (path, no_devcontainer) {
            (_, true) => Self::Disabled,
            (Some(p), false) => Self::Explicit(p),
            (None, false) => Self::Discover,
        }
    }
}

/// CLI surface controlling devcontainer.json discovery and apply.
///
/// `input` selects an explicit file, opts out entirely, or requests
/// discovery. `dry_run` prints the report and exits before any side effects.
pub(crate) struct DevcontainerOpts<'a> {
    pub(crate) input: &'a DevcontainerInput,
    pub(crate) dry_run: bool,
    pub(crate) workspace: Option<&'a Path>,
    pub(crate) mounts: &'a [config::Mount],
    pub(crate) git_repo: Option<&'a str>,
    pub(crate) github_auth: Option<&'a config::GitHubAuth>,
    pub(crate) preference_path: Option<&'a Path>,
}

enum DevcontainerSource {
    Path(PathBuf),
    Contents {
        display_path: PathBuf,
        contents: String,
    },
}

fn discovered_local_devcontainer(opts: &DevcontainerOpts<'_>, source: &DevcontainerSource) -> bool {
    matches!(opts.input, DevcontainerInput::Discover)
        && matches!(source, DevcontainerSource::Path(_))
}

fn maybe_skip_stored_devcontainer_opt_out(
    opts: &DevcontainerOpts<'_>,
    source: &DevcontainerSource,
    display_path: &Path,
) -> Result<bool> {
    if !discovered_local_devcontainer(opts, source) {
        return Ok(false);
    }
    let (Some(preference_path), Some(project)) = (opts.preference_path, opts.workspace) else {
        return Ok(false);
    };
    let preferences = devcontainer::DevcontainerPreferences::load(preference_path)?;
    let Some(project_key) = preferences.ignored_project(project)? else {
        return Ok(false);
    };

    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "Skipping {} because a stored devcontainer opt-out is set for project {}.\n\
         Run `coop devcontainer clear {}` to re-enable discovery, or pass --devcontainer {} to apply this file once.",
        display_path.display(),
        project_key,
        project_key,
        display_path.display(),
    )
    .context("Failed to write devcontainer opt-out message")?;
    Ok(true)
}

fn maybe_record_devcontainer_opt_out(opts: &DevcontainerOpts<'_>, project: &Path) -> Result<()> {
    let Some(preference_path) = opts.preference_path else {
        return Ok(());
    };
    if !prompt::confirm(&format!(
        "Always ignore devcontainer.json for project {}?",
        project.display()
    ))? {
        return Ok(());
    }

    let mut preferences = devcontainer::DevcontainerPreferences::load(preference_path)?;
    let project_key = preferences.set_ignored(project)?;
    preferences.save(preference_path)?;
    tracing::info!("Recorded persistent devcontainer opt-out for project {project_key}");
    Ok(())
}

/// Discover, prompt, and translate a `devcontainer.json` for the given
/// lifecycle `stage`.
///
/// Returns `None` when the user opts out, no file is found, or stdin is
/// not a TTY with no explicit flag (in which case an error is returned
/// instead — the issue forbids silently choosing). The report is always
/// emitted to stderr when a file is loaded, so the user sees every key
/// that did and didn't take effect.
#[expect(
    clippy::print_stderr,
    reason = "devcontainer report is intentional user-facing CLI output"
)]
pub(crate) fn resolve_devcontainer(
    opts: &DevcontainerOpts<'_>,
    inputs: &devcontainer::TranslatorInputs,
    stage: devcontainer::Stage,
) -> Result<Option<devcontainer::Translation>> {
    use std::io::IsTerminal as _;

    if matches!(opts.input, DevcontainerInput::Disabled) {
        return Ok(None);
    }

    let (source, losers) = if let DevcontainerInput::Explicit(p) = opts.input {
        (DevcontainerSource::Path(p.clone()), Vec::new())
    } else {
        let found = devcontainer::discover(opts.workspace, opts.mounts);
        if let Some((winner, losers)) = devcontainer::pick_winner(found) {
            (DevcontainerSource::Path(winner.path), losers)
        } else if let Some(repo_url) = opts.git_repo
            && let Some(remote) = git_repo_devcontainer::discover(repo_url, opts.github_auth)?
        {
            (
                DevcontainerSource::Contents {
                    display_path: remote.display_path,
                    contents: remote.contents,
                },
                Vec::new(),
            )
        } else {
            return Ok(None);
        }
    };
    let display_path = match &source {
        DevcontainerSource::Path(path) => path,
        DevcontainerSource::Contents { display_path, .. } => display_path,
    };

    if maybe_skip_stored_devcontainer_opt_out(opts, &source, display_path)? {
        return Ok(None);
    }

    // When discovery (not an explicit flag) found the file, defer to the
    // user. CI/scripted callers must pass --devcontainer or --no-devcontainer.
    if matches!(opts.input, DevcontainerInput::Discover) && !opts.dry_run {
        if !std::io::stdin().is_terminal() {
            match &source {
                DevcontainerSource::Path(_) => bail!(
                    "Found {} but stdin is not a TTY.\n\
                     Pass --devcontainer {} to apply it, or --no-devcontainer to ignore.\n\
                     coop reads a subset of devcontainer.json — see docs/devcontainer.md for the supported keys.",
                    display_path.display(),
                    display_path.display()
                ),
                DevcontainerSource::Contents { .. } => bail!(
                    "Found {} but stdin is not a TTY.\n\
                     Run interactively to confirm the remote file, pass --no-devcontainer to ignore it, \
                     or pass --devcontainer <local-path> to apply an explicit file.\n\
                     coop reads a subset of devcontainer.json — see docs/devcontainer.md for the supported keys.",
                    display_path.display()
                ),
            }
        }
        let answer = prompt::confirm_default_yes(&format!(
            "Use devcontainer.json at {}?",
            display_path.display()
        ))?;
        if !answer {
            match &source {
                DevcontainerSource::Path(_) => {
                    tracing::info!(
                        "Skipping {}. Re-run with --devcontainer {} to apply it later.",
                        display_path.display(),
                        display_path.display()
                    );
                    if let Some(project) = opts.workspace {
                        maybe_record_devcontainer_opt_out(opts, project)?;
                    }
                }
                DevcontainerSource::Contents { .. } => tracing::info!(
                    "Skipping {}. Re-run interactively to apply it later.",
                    display_path.display()
                ),
            }
            return Ok(None);
        }
    }

    let source_is_remote = matches!(source, DevcontainerSource::Contents { .. });
    let parsed = match source {
        DevcontainerSource::Path(path) => devcontainer::ParsedDevcontainer::load(&path)?,
        DevcontainerSource::Contents {
            display_path,
            contents,
        } => devcontainer::ParsedDevcontainer::from_str(display_path, &contents)?,
    };
    let mut translation = devcontainer::translate(&parsed, inputs, stage);
    if source_is_remote && let Some(applied) = &mut translation.applied {
        applied.source = devcontainer::AppliedDevcontainerSource::RemoteContents;
    }
    translation.report.ignored_paths = losers;
    resolve_oci_feature_requests(&mut translation);

    eprintln!("{}", translation.report.render());

    Ok(Some(translation))
}

fn resolve_oci_feature_requests(translation: &mut devcontainer::Translation) {
    if translation.oci_feature_requests.is_empty() {
        return;
    }
    for (request, resolved) in
        translation
            .oci_feature_requests
            .iter()
            .zip(devcontainer_oci::resolve_features(
                &translation.oci_feature_requests,
            ))
    {
        let key = format!("features.{}", request.raw_id);
        match resolved {
            Ok(feature) => {
                translation.report.push(
                    key,
                    devcontainer::ReportStatus::Applied,
                    devcontainer::ReportSource::Devcontainer,
                    feature.installed.digest.to_string(),
                    format!(
                        "OCI feature '{}' install.sh sha256 {} will run during setup",
                        feature.installed.id, feature.installed.install_script_hash
                    ),
                );
                translation.oci_features.push(feature);
            }
            Err(e) => translation.report.push(
                key,
                devcontainer::ReportStatus::Invalid,
                devcontainer::ReportSource::Devcontainer,
                request.raw_id.clone(),
                format!("failed to resolve OCI feature: {e:#}"),
            ),
        }
    }
}

#[expect(
    clippy::print_stderr,
    reason = "devcontainer check report is intentional user-facing CLI output"
)]
pub(crate) fn cmd_devcontainer_check(command: &DevcontainerCommands) -> Result<()> {
    match command {
        DevcontainerCommands::Check { path, stage } => {
            let dc_input = DevcontainerInput::Explicit(path.clone());
            let opts = DevcontainerOpts {
                input: &dc_input,
                dry_run: true,
                workspace: None,
                mounts: &[],
                git_repo: None,
                github_auth: None,
                preference_path: None,
            };
            let setup_inputs = devcontainer::TranslatorInputs::default();

            match stage {
                DevcontainerCheckStage::Setup => {
                    resolve_devcontainer(&opts, &setup_inputs, devcontainer::Stage::Setup)?;
                }
                DevcontainerCheckStage::Start => {
                    let start_inputs = devcontainer::TranslatorInputs {
                        persisted_guest_user: Some(guest::GuestUser::default()),
                        ..devcontainer::TranslatorInputs::default()
                    };
                    resolve_devcontainer(&opts, &start_inputs, devcontainer::Stage::Start)?;
                }
                DevcontainerCheckStage::Both => {
                    eprintln!("setup-stage translation:");
                    let setup_translation =
                        resolve_devcontainer(&opts, &setup_inputs, devcontainer::Stage::Setup)?;
                    let assumed_guest_user =
                        devcontainer_check_assumed_guest_user(setup_translation.as_ref());
                    let start_inputs = devcontainer::TranslatorInputs {
                        persisted_guest_user: Some(assumed_guest_user),
                        ..devcontainer::TranslatorInputs::default()
                    };
                    eprintln!();
                    eprintln!("start-stage translation:");
                    resolve_devcontainer(&opts, &start_inputs, devcontainer::Stage::Start)?;
                }
            }
            Ok(())
        }
        DevcontainerCommands::Ignore { .. }
        | DevcontainerCommands::Status { .. }
        | DevcontainerCommands::Clear { .. } => {
            unreachable!("devcontainer preference commands require config")
        }
    }
}

pub(crate) fn cmd_devcontainer(
    cfg: &config::CoopConfig,
    command: &DevcontainerCommands,
) -> Result<()> {
    let preference_path = cfg.devcontainer_preferences_path();
    match command {
        DevcontainerCommands::Check { .. } => cmd_devcontainer_check(command),
        DevcontainerCommands::Ignore { project } => {
            let mut preferences = devcontainer::DevcontainerPreferences::load(&preference_path)?;
            let project_key = preferences.set_ignored(project)?;
            preferences.save(&preference_path)?;
            let mut stdout = std::io::stdout();
            writeln!(
                stdout,
                "Devcontainer discovery disabled for project {project_key}"
            )
            .context("Failed to write devcontainer ignore status")?;
            Ok(())
        }
        DevcontainerCommands::Status { project } => {
            let preferences = devcontainer::DevcontainerPreferences::load(&preference_path)?;
            let mut stdout = std::io::stdout();
            if let Some(project) = project {
                if let Some(project_key) = preferences.ignored_project(project)? {
                    writeln!(
                        stdout,
                        "Devcontainer discovery disabled for project {project_key}"
                    )
                    .context("Failed to write devcontainer status")?;
                } else {
                    let project_key = devcontainer::project_preference_lookup_key(project)?;
                    writeln!(
                        stdout,
                        "Devcontainer discovery enabled for project {project_key}"
                    )
                    .context("Failed to write devcontainer status")?;
                }
            } else {
                let ignored: Vec<_> = preferences.ignored_projects().collect();
                if ignored.is_empty() {
                    writeln!(stdout, "No persistent devcontainer opt-outs recorded.")
                        .context("Failed to write devcontainer status")?;
                } else {
                    writeln!(stdout, "Persistent devcontainer opt-outs:")
                        .context("Failed to write devcontainer status")?;
                    for project in ignored {
                        writeln!(stdout, "  {project}")
                            .context("Failed to write devcontainer status")?;
                    }
                }
            }
            Ok(())
        }
        DevcontainerCommands::Clear { project } => {
            let mut preferences = devcontainer::DevcontainerPreferences::load(&preference_path)?;
            let project_key = devcontainer::project_preference_lookup_key(project)?;
            let removed = preferences.clear(project)?;
            preferences.save(&preference_path)?;
            let mut stdout = std::io::stdout();
            if removed {
                writeln!(
                    stdout,
                    "Cleared devcontainer opt-out for project {project_key}"
                )
                .context("Failed to write devcontainer clear status")?;
            } else {
                writeln!(
                    stdout,
                    "No persistent devcontainer opt-out recorded for project {project_key}"
                )
                .context("Failed to write devcontainer clear status")?;
            }
            Ok(())
        }
    }
}

fn devcontainer_check_assumed_guest_user(
    setup_translation: Option<&devcontainer::Translation>,
) -> guest::GuestUser {
    setup_translation
        .and_then(|t| t.guest_user.clone())
        .unwrap_or_default()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {

    #[test]
    fn devcontainer_input_from_flags_precedence() {
        let path = std::path::PathBuf::from("/tmp/devcontainer.json");
        assert!(matches!(
            super::DevcontainerInput::from_flags(Some(path.clone()), false),
            super::DevcontainerInput::Explicit(p) if p == path
        ));
        assert!(matches!(
            super::DevcontainerInput::from_flags(None, false),
            super::DevcontainerInput::Discover
        ));
        assert!(matches!(
            super::DevcontainerInput::from_flags(None, true),
            super::DevcontainerInput::Disabled
        ));
        // --no-devcontainer wins even if a path is also present.
        assert!(matches!(
            super::DevcontainerInput::from_flags(Some(path), true),
            super::DevcontainerInput::Disabled
        ));
    }

    #[test]
    fn devcontainer_check_both_stage_reuses_setup_guest_user() {
        let translation = super::devcontainer::Translation {
            guest_user: Some(super::guest::GuestUser::new("vscode").unwrap()),
            ..super::devcontainer::Translation::default()
        };
        assert_eq!(
            super::devcontainer_check_assumed_guest_user(Some(&translation)).to_string(),
            "vscode"
        );
    }

    #[test]
    fn devcontainer_check_both_stage_defaults_without_setup_guest_user() {
        assert_eq!(
            super::devcontainer_check_assumed_guest_user(None).to_string(),
            "ubuntu"
        );
    }
}
