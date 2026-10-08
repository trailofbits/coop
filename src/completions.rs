//! Shell completion support.
//!
//! Two pieces:
//!
//! * Script generation via `coop completions <shell>` — emits a dynamic
//!   completion script for each supported shell.
//! * Dynamic completion via `clap_complete::CompleteEnv` — when the binary
//!   is invoked with `COMPLETE=<shell>`, it computes candidates at runtime
//!   so things like instance names and image names get real values.
//!
//! Each `complete_*` function must be infallible and fast: shells call into
//! the binary on every TAB. We swallow all errors and return an empty list
//! rather than printing diagnostics or panicking.

use std::io::{self, Write as _};

use clap::CommandFactory as _;
use clap_complete::Shell;
use clap_complete::engine::CompletionCandidate;
use clap_complete::env::Shells;

use crate::config::{CoopConfig, Instance};
use crate::guest::BUILTIN_PROFILES;

/// Write a completion script for `shell` to stdout.
pub fn emit(shell: Shell) -> io::Result<()> {
    let cmd = crate::Cli::command();
    let name = cmd.get_name();
    let shells = Shells::builtins();
    let shell_name = shell.to_string();
    let completer = shells
        .completer(&shell_name)
        .ok_or_else(|| io::Error::other(format!("Unsupported completion shell: {shell_name}")))?;
    let mut stdout = io::stdout().lock();
    // Saved completion files must find the installed binary on PATH,
    // rather than pinning the path used to generate the file.
    completer.write_registration("COMPLETE", name, name, name, &mut stdout)?;
    if shell == Shell::Zsh {
        // compinit autoloads this file as _coop on the first TAB. Registration
        // alone would only make the second TAB work; dispatch the first too.
        writeln!(
            stdout,
            "\nif [[ $funcstack[1] == _{name} ]]; then\n    _clap_dynamic_completer_{name} \"$@\"\nfi"
        )?;
    }
    Ok(())
}

/// Candidate list for instance-name arguments.
///
/// Reads the default config (ignoring `--config` — completion fires before
/// argument parsing) and returns the names of any registered instances.
///
/// `CoopConfig::list_instances` emits `tracing::warn!` for corrupted instance
/// dirs. Today the completion path runs before `init_tracing`, so the global
/// subscriber is the no-op default and nothing leaks into the user's shell.
/// If tracing is ever wired up earlier, those warnings would surface here.
#[must_use]
pub fn instance_candidates() -> Vec<CompletionCandidate> {
    instance_candidates_for(InstanceFilter::All)
}

#[derive(Clone, Copy)]
enum InstanceFilter {
    All,
    Running,
    Stopped,
}

fn instance_candidates_for(filter: InstanceFilter) -> Vec<CompletionCandidate> {
    let Ok(cfg) = CoopConfig::load(&CoopConfig::default_path()) else {
        return Vec::new();
    };
    let Ok(instances) = cfg.list_instances() else {
        return Vec::new();
    };
    #[cfg(target_os = "macos")]
    let state_names = if matches!(filter, InstanceFilter::All) {
        None
    } else {
        match crate::lima::completion_instance_names() {
            Ok(names) => Some(names),
            Err(_) => return Vec::new(),
        }
    };
    #[cfg(target_os = "macos")]
    let matches_filter = |instance: &Instance| match filter {
        InstanceFilter::All => true,
        InstanceFilter::Running => state_names
            .as_ref()
            .is_some_and(|names| names.running.contains(&instance.name)),
        InstanceFilter::Stopped => state_names
            .as_ref()
            .is_some_and(|names| names.stopped.contains(&instance.name)),
    };
    #[cfg(not(target_os = "macos"))]
    let matches_filter = |instance: &Instance| match filter {
        InstanceFilter::All => true,
        InstanceFilter::Running => matches!(instance.probe_running(), Ok(true)),
        InstanceFilter::Stopped => matches!(instance.probe_running(), Ok(false)),
    };
    instances
        .into_iter()
        .filter(matches_filter)
        .map(|i| CompletionCandidate::new(i.name.as_str()))
        .collect()
}

/// Candidate list for commands that need a running instance.
#[must_use]
pub fn running_instance_candidates() -> Vec<CompletionCandidate> {
    instance_candidates_for(InstanceFilter::Running)
}

/// Candidate list for `start`, which accepts only stopped instances.
#[must_use]
pub fn stopped_instance_candidates() -> Vec<CompletionCandidate> {
    instance_candidates_for(InstanceFilter::Stopped)
}

/// Candidate list for `--image` arguments.
#[must_use]
pub fn image_candidates() -> Vec<CompletionCandidate> {
    let Ok(cfg) = CoopConfig::load(&CoopConfig::default_path()) else {
        return Vec::new();
    };
    let Ok(images) = cfg.list_images() else {
        return Vec::new();
    };
    images
        .into_iter()
        .map(|i| CompletionCandidate::new(i.name.as_str()))
        .collect()
}

/// Candidate list for `--profile` and `profiles show <name>` arguments.
///
/// Combines compile-time builtin profiles with any custom profiles defined
/// in the user's config.
#[must_use]
pub fn profile_candidates() -> Vec<CompletionCandidate> {
    let builtins = BUILTIN_PROFILES
        .iter()
        .map(|p| CompletionCandidate::new(p.name));
    let custom = CoopConfig::load(&CoopConfig::default_path())
        .ok()
        .into_iter()
        .flat_map(|cfg| cfg.profiles.into_keys().map(CompletionCandidate::new));
    builtins.chain(custom).collect()
}
