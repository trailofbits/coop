//! Per-instance snapshot of start-time guest env overrides.
//!
//! Mirrors [`crate::port_forward::ForwardsState`]. Current versions save CLI
//! `--env` entries so later shell and exec invocations can reapply them. A
//! snapshot created before devcontainer support was removed can also contain
//! translated `containerEnv` entries; those remain readable so existing
//! instances keep their established environment until they are recreated.
//!
//! `[guest_env]` from `config.toml` is deliberately *not* in the snapshot:
//! it is re-read on every invocation, so persisting it would freeze edits
//! the user made between `start` and `shell`.
//!
//! Restart can extend or override the snapshot with new `--env` values.
//!
//! Persistence layout: one JSON file at `<inst.dir>/guest_env.json`.
//! Empty snapshots are not written; an empty file would be ambiguous
//! with "no snapshot," and the missing-file branch already means
//! "nothing extra to overlay."
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Instance;

/// Validated POSIX-style environment variable name.
///
/// Construction guarantees the name matches `[a-zA-Z_][a-zA-Z0-9_]*`, so
/// downstream code (SSH `SendEnv`, JSON snapshots, shell env exports)
/// can use it without re-checking. CLI parsing and snapshot deserialization
/// validate this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EnvVarName(String);

impl EnvVarName {
    /// Parse and validate. Empty input is rejected; the first character
    /// must be a letter or `_`; subsequent characters may also be digits.
    pub fn new(s: &str) -> Result<Self> {
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            bail!("env var name must not be empty");
        };
        if !(first.is_ascii_alphabetic() || first == '_') {
            bail!("env var name '{s}' must start with a letter or '_' (got '{first}')");
        }
        for c in chars {
            if !(c.is_ascii_alphanumeric() || c == '_') {
                bail!(
                    "env var name '{s}' contains invalid character '{c}' \
                     (allowed: a-z, A-Z, 0-9, '_')"
                );
            }
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvVarName {
    #[mutants::skip] // equivalent: trivial forwarder; a test would duplicate the as_str() coverage above
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for EnvVarName {
    #[mutants::skip] // equivalent: trivial forwarder; a test would duplicate the as_str() coverage above
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for EnvVarName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl FromStr for EnvVarName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl Serialize for EnvVarName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EnvVarName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

/// Persisted start-time guest environment, applied on later invocations.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GuestEnvState {
    /// Entries to overlay onto the resolved env-forward set. `BTreeMap`
    /// gives deterministic iteration so `serde_json` output is stable
    /// across runs.
    #[serde(default)]
    pub entries: BTreeMap<EnvVarName, String>,
}

impl GuestEnvState {
    pub fn save(&self, inst: &Instance) -> Result<()> {
        let path = inst.guest_env_state_path();
        if self.entries.is_empty() {
            // Don't leave a stale or empty snapshot behind — the
            // missing-file branch already encodes "nothing to overlay."
            if let Err(e) = crate::fs_util::remove_private_if_exists(&path) {
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
        match crate::fs_util::read_optional_private(&path) {
            Ok(Some(content)) => {
                let state =
                    serde_json::from_str(&content).context("Failed to parse guest_env.json")?;
                Ok(Some(state))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e.context(format!("Failed to read {}", path.display()))),
        }
    }
}

/// Parse a single `--env KEY=VALUE` entry. Suitable for clap
/// `value_parser`, so invalid keys fail before any VM lifecycle work.
///
/// Rejects entries missing `=` and entries whose key fails
/// [`EnvVarName`] validation. Empty values are allowed (e.g.
/// `--env CLEAR=`).
pub fn parse_cli_env_arg(entry: &str) -> Result<(EnvVarName, String)> {
    let (key, value) = entry
        .split_once('=')
        .with_context(|| format!("--env expects KEY=VALUE, got '{entry}' (missing '=')"))?;
    let name =
        EnvVarName::new(key).with_context(|| format!("--env KEY is invalid (got '{entry}')"))?;
    Ok((name, value.to_string()))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::config::{ImageName, Instance, InstanceIndex, InstanceName};

    fn env(s: &str) -> EnvVarName {
        EnvVarName::new(s).unwrap()
    }

    fn fake_instance(dir: PathBuf) -> Instance {
        Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir,
            image: ImageName::new("test.img").unwrap(),
        }
    }

    // ── EnvVarName ───────────────────────────────────────────

    #[test]
    fn env_var_name_rejects_invalid_forms() {
        for s in ["", "1FOO", "FOO BAR", "FOO=BAR", "FOO-BAR", "FOO.BAR", "ä"] {
            assert!(EnvVarName::new(s).is_err(), "expected '{s}' to be invalid");
        }
    }

    #[test]
    fn env_var_name_serde_round_trips_through_json_map_key() {
        let mut map: BTreeMap<EnvVarName, String> = BTreeMap::new();
        map.insert(env("FOO"), "1".to_string());
        map.insert(env("_BAR"), "2".to_string());
        let json = serde_json::to_string(&map).unwrap();
        let back: BTreeMap<EnvVarName, String> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);
    }

    #[test]
    fn env_var_name_serde_rejects_invalid_key() {
        let json = r#"{"1FOO":"v"}"#;
        assert!(serde_json::from_str::<BTreeMap<EnvVarName, String>>(json).is_err());
    }

    // ── GuestEnvState ────────────────────────────────────────

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        let mut state = GuestEnvState::default();
        state.entries.insert(env("FOO"), "1".to_string());
        state.entries.insert(env("BAR"), "2".to_string());

        state.save(&inst).unwrap();
        let loaded = GuestEnvState::try_load(&inst).unwrap().unwrap();
        assert_eq!(
            loaded.entries.get(&env("FOO")).map(String::as_str),
            Some("1")
        );
        assert_eq!(
            loaded.entries.get(&env("BAR")).map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn try_load_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        assert!(GuestEnvState::try_load(&inst).unwrap().is_none());
    }

    #[test]
    fn try_load_surfaces_non_not_found_read_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        // Put a directory where the JSON file is expected: reading it fails
        // with a non-NotFound error kind, which must surface as Err rather
        // than being swallowed as Ok(None).
        std::fs::create_dir_all(inst.guest_env_state_path()).unwrap();
        assert!(GuestEnvState::try_load(&inst).is_err());
    }

    #[test]
    fn save_empty_removes_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = fake_instance(tmp.path().to_path_buf());
        let mut state = GuestEnvState::default();
        state.entries.insert(env("KEEP"), "x".to_string());
        state.save(&inst).unwrap();
        assert!(inst.guest_env_state_path().exists());

        GuestEnvState::default().save(&inst).unwrap();
        assert!(!inst.guest_env_state_path().exists());
    }

    // ── parse_cli_env_arg ────────────────────────────────────

    #[test]
    fn parse_cli_env_arg_basic() {
        let (k, v) = parse_cli_env_arg("FOO=1").unwrap();
        assert_eq!(k, env("FOO"));
        assert_eq!(v, "1");
    }

    #[test]
    fn parse_cli_env_arg_allows_empty_value() {
        let (k, v) = parse_cli_env_arg("EMPTY=").unwrap();
        assert_eq!(k, env("EMPTY"));
        assert_eq!(v, "");
    }

    #[test]
    fn parse_cli_env_arg_rejects_missing_equals() {
        let err = parse_cli_env_arg("BAD").unwrap_err();
        assert!(format!("{err:#}").contains("missing '='"));
    }

    #[test]
    fn parse_cli_env_arg_value_may_contain_equals() {
        let (k, v) = parse_cli_env_arg("URL=https://x?a=b&c=d").unwrap();
        assert_eq!(k, env("URL"));
        assert_eq!(v, "https://x?a=b&c=d");
    }

    // ── property tests ───────────────────────────────────────

    /// Keys drawn from a deliberately small space to exercise varied map
    /// sizes and repeated generated values.
    fn small_env_map() -> impl Strategy<Value = BTreeMap<EnvVarName, String>> {
        prop::collection::btree_map(
            "[A-E]".prop_map(|s| EnvVarName::new(&s).unwrap()),
            any::<String>(),
            0..6,
        )
    }

    proptest! {
        /// Every string matching the documented `[a-zA-Z_][a-zA-Z0-9_]*`
        /// grammar parses, and the parsed name preserves the input verbatim.
        #[test]
        fn env_var_name_accepts_all_valid_forms(s in "[a-zA-Z_][a-zA-Z0-9_]*") {
            let name = EnvVarName::new(&s).unwrap();
            prop_assert_eq!(name.as_str(), s.as_str());
        }

        /// A `GuestEnvState` round-trips through disk unchanged: save → load
        /// yields the same entries, and a second save → load is identical to
        /// the first (deterministic `BTreeMap` ordering). An empty map is not
        /// persisted (see `save`), so loading then yields `None`, which decodes
        /// back to the empty snapshot it represents.
        #[test]
        fn guest_env_state_round_trips_through_disk(entries in small_env_map()) {
            let tmp = tempfile::tempdir().unwrap();
            let inst = fake_instance(tmp.path().to_path_buf());
            let state = GuestEnvState { entries: entries.clone() };

            state.save(&inst).unwrap();
            let loaded = GuestEnvState::try_load(&inst).unwrap().unwrap_or_default();
            prop_assert_eq!(&loaded.entries, &entries);

            loaded.save(&inst).unwrap();
            let reloaded = GuestEnvState::try_load(&inst).unwrap().unwrap_or_default();
            prop_assert_eq!(reloaded.entries, entries);
        }
    }
}
