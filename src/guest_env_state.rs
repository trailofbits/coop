//! Per-instance snapshot of `--env` overrides applied at `coop start`.
//!
//! Mirrors [`crate::port_forward::ForwardsState`]: `--env KEY=VALUE` lives
//! only in the `coop start` process's memory, so subsequent invocations
//! like `coop shell` reload `config.toml` and never see those values.
//! Persisting the CLI-provided entries lets every later invocation
//! against the same instance forward them via SSH `SendEnv`.
//!
//! The snapshot stores **only** the CLI-provided `--env` set (not the
//! whole merged `guest_env` map). `[guest_env]` from `config.toml` and
//! devcontainer-derived entries are re-read from disk on every command,
//! so persisting them would freeze edits the user made between `start`
//! and `shell`. Restart can extend or override the snapshot — passing
//! `--env KEY=newvalue` on `coop start <stopped>` wins over the saved
//! value for that key and replaces it in the saved set.
//!
//! Persistence layout: one JSON file at `<inst.dir>/guest_env.json`.
//! Empty snapshots are not written; an empty file would be ambiguous
//! with "no snapshot," and the missing-file branch already means
//! "nothing extra to overlay."
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Instance;

/// Persisted CLI `--env KEY=VALUE` snapshot, applied on every later
/// `coop` invocation that targets the same instance.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GuestEnvState {
    /// Entries to overlay onto the resolved env-forward set. `BTreeMap`
    /// gives deterministic iteration so `serde_json` output is stable
    /// across runs.
    #[serde(default)]
    pub entries: BTreeMap<String, String>,
}

impl GuestEnvState {
    pub fn save(&self, inst: &Instance) -> Result<()> {
        let path = inst.guest_env_state_path();
        if self.entries.is_empty() {
            // Don't leave a stale or empty snapshot behind — the
            // missing-file branch already encodes "nothing to overlay."
            if path.exists()
                && let Err(e) = fs::remove_file(&path)
            {
                tracing::debug!(
                    "Failed to remove empty guest_env state {} (non-fatal): {e}",
                    path.display()
                );
            }
            return Ok(());
        }
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize guest_env state")?;
        crate::fs_util::atomic_write_json(&path, &json)
            .context("Failed to write guest_env.json")?;
        tracing::debug!("Wrote guest_env state to {}", path.display());
        Ok(())
    }

    pub fn try_load(inst: &Instance) -> Result<Option<Self>> {
        let path = inst.guest_env_state_path();
        match fs::read_to_string(&path) {
            Ok(content) => {
                let state =
                    serde_json::from_str(&content).context("Failed to parse guest_env.json")?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("Failed to read {}", path.display())))
            }
        }
    }
}

/// Parse `--env KEY=VALUE` argument list into a `BTreeMap`.
///
/// Rejects entries missing `=` and entries with an empty key. Empty
/// values are allowed (e.g. `--env CLEAR=`).
///
/// Returned map preserves the "last write wins" precedence of the
/// argument order: later duplicates of the same key overwrite earlier
/// ones, matching `merge_cli_guest_env`'s in-memory behaviour.
pub fn parse_cli_env_args(args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in args {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("--env expects KEY=VALUE, got '{entry}' (missing '=')"))?;
        if key.is_empty() {
            bail!("--env KEY must not be empty (got '{entry}')");
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{Instance, InstanceIndex, InstanceName};

    fn fake_instance(dir: PathBuf) -> Instance {
        Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0),
            dir,
            image: "test.img".to_string(),
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        let mut state = GuestEnvState::default();
        state.entries.insert("FOO".to_string(), "1".to_string());
        state.entries.insert("BAR".to_string(), "2".to_string());

        state.save(&inst).unwrap();
        let loaded = GuestEnvState::try_load(&inst).unwrap().unwrap();
        assert_eq!(loaded.entries.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(loaded.entries.get("BAR").map(String::as_str), Some("2"));
    }

    #[test]
    fn try_load_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        assert!(GuestEnvState::try_load(&inst).unwrap().is_none());
    }

    #[test]
    fn save_empty_removes_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        let mut state = GuestEnvState::default();
        state.entries.insert("KEEP".to_string(), "x".to_string());
        state.save(&inst).unwrap();
        assert!(inst.guest_env_state_path().exists());

        GuestEnvState::default().save(&inst).unwrap();
        assert!(!inst.guest_env_state_path().exists());
    }

    #[test]
    fn parse_cli_env_args_basic() {
        let parsed = parse_cli_env_args(&["FOO=1".into(), "BAR=baz".into()]).unwrap();
        assert_eq!(parsed.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(parsed.get("BAR").map(String::as_str), Some("baz"));
    }

    #[test]
    fn parse_cli_env_args_allows_empty_value() {
        let parsed = parse_cli_env_args(&["EMPTY=".into()]).unwrap();
        assert_eq!(parsed.get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn parse_cli_env_args_rejects_missing_equals() {
        let err = parse_cli_env_args(&["BAD".into()]).unwrap_err();
        assert!(format!("{err:#}").contains("missing '='"));
    }

    #[test]
    fn parse_cli_env_args_rejects_empty_key() {
        let err = parse_cli_env_args(&["=value".into()]).unwrap_err();
        assert!(format!("{err:#}").contains("KEY must not be empty"));
    }

    #[test]
    fn parse_cli_env_args_last_duplicate_wins() {
        let parsed = parse_cli_env_args(&["K=v1".into(), "K=v2".into()]).unwrap();
        assert_eq!(parsed.get("K").map(String::as_str), Some("v2"));
    }

    #[test]
    fn parse_cli_env_args_value_may_contain_equals() {
        let parsed = parse_cli_env_args(&["URL=https://x?a=b&c=d".into()]).unwrap();
        assert_eq!(
            parsed.get("URL").map(String::as_str),
            Some("https://x?a=b&c=d"),
        );
    }
}
