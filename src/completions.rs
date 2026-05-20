//! Shell completion support.
//!
//! Two pieces:
//!
//! * Static generation via `coop completions <shell>` — emits a completion
//!   script the user can source from their shell rc.
//! * Dynamic completion via `clap_complete::CompleteEnv` — when the binary
//!   is invoked with `COMPLETE=<shell>`, it computes candidates at runtime
//!   so things like instance names and image names get real values.
//!
//! Each `complete_*` function must be infallible and fast: shells call into
//! the binary on every TAB. We swallow all errors and return an empty list
//! rather than printing diagnostics or panicking.

use std::io;

use clap::CommandFactory as _;
use clap_complete::engine::CompletionCandidate;
use clap_complete::{Shell, generate};

use crate::config::CoopConfig;
use crate::guest::BUILTIN_PROFILES;

/// Write a static completion script for `shell` to stdout.
pub fn emit_static(shell: Shell) {
    let mut cmd = crate::Cli::command();
    let name = cmd.get_name().to_string();
    generate(shell, &mut cmd, name, &mut io::stdout());
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
    let Ok(cfg) = CoopConfig::load(&CoopConfig::default_path()) else {
        return Vec::new();
    };
    let Ok(instances) = cfg.list_instances() else {
        return Vec::new();
    };
    instances
        .into_iter()
        .map(|i| CompletionCandidate::new(i.name.as_str()))
        .collect()
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
