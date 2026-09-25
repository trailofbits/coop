//! A VM stores an entry key, never a resolved PAT or a duplicate secret.
use std::fs;
use std::io::ErrorKind;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{CoopConfig, Instance};
use crate::github_repo::RepoSlug;
use crate::guest_env_state::GuestEnvState;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub repo: RepoSlug,
}

impl Assignment {
    pub fn load(inst: &Instance) -> Result<Option<Self>> {
        let path = inst.dir.join("github_pat.json");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => bail!(
                "github_pat.json must be a regular file; use coop github unassign-pat --vm <name> to remove it"
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("Cannot inspect github_pat.json"),
        }
        let bytes = fs::read(path).context("Cannot read github_pat.json")?;
        serde_json::from_slice::<Self>(&bytes).map(Some).context(
            "Invalid github_pat.json; use coop github unassign-pat --vm <name> to remove it",
        )
    }

    pub fn save(&self, cfg: &CoopConfig, inst: &Instance) -> Result<()> {
        self.validate(cfg)?;
        crate::fs_util::atomic_write_with_mode(
            &inst.dir.join("github_pat.json"),
            &serde_json::to_string(self)?,
            0o600,
        )
    }

    pub fn remove(inst: &Instance) -> Result<()> {
        match fs::remove_file(inst.dir.join("github_pat.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("Cannot remove github_pat.json"),
        }
    }

    pub fn validate(&self, cfg: &CoopConfig) -> Result<()> {
        if !self.available(cfg) {
            bail!(
                "Assigned PAT entry '{}' is unavailable; restore it with coop github setup-pat --repo {}, or use coop github unassign-pat --vm <name>",
                self.repo,
                self.repo
            );
        }
        Ok(())
    }

    pub fn available(&self, cfg: &CoopConfig) -> bool {
        cfg.github
            .as_ref()
            .and_then(|auth| auth.pat_entry(&self.repo))
            .is_some()
    }
}

/// Opt-out precedes even reading the sidecar. A bad reference never falls back.
pub fn active(cfg: &CoopConfig, inst: &Instance) -> Result<Option<Assignment>> {
    if cfg.github_disabled {
        return Ok(None);
    }
    let assignment = Assignment::load(inst)?;
    if let Some(assignment) = &assignment {
        assignment.validate(cfg)?;
        reject_overrides(cfg.guest_env.keys().map(AsRef::as_ref))?;
        let grok_mcp_hosts = cfg.grok.stdio_env_host_names();
        reject_overrides(
            cfg.claude
                .env_forward
                .iter()
                .chain(&cfg.codex.env_forward)
                .chain(&cfg.grok.env_forward)
                .map(AsRef::as_ref)
                .chain(grok_mcp_hosts.iter().map(AsRef::as_ref)),
        )?;
        if let Some(state) = GuestEnvState::try_load(inst)? {
            reject_overrides(state.entries.keys().map(AsRef::as_ref))?;
        }
    }
    Ok(assignment)
}

pub fn reject_overrides<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    for name in names {
        if matches!(name, "GITHUB_TOKEN" | "GH_TOKEN") {
            bail!(
                "VM PAT assignment conflicts with managed {name}; remove it from guest_env, env_forward, a Grok stdio MCP env mapping, and persisted guest_env.json (including --env/containerEnv), or unassign the PAT"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "tests")]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use crate::backend::{Hostname, SshTarget, SshUser};
    use crate::config::{ConfigPath, CoopConfig, Instance, InstanceName};
    use crate::github_assignment::{Assignment, active, reject_overrides};
    use crate::github_repo::RepoSlug;
    use crate::guest_env_state::{EnvVarName, GuestEnvState};

    fn fixture() -> (tempfile::TempDir, CoopConfig, Instance) {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg: CoopConfig = toml::from_str(
            r#"
            [github]
            mode = "pat"
            [github.pat."org/assigned"]
            token = "github_pat_assigned"
            [github.pat."org/workspace"]
            token = "github_pat_default"
        "#,
        )
        .unwrap();
        cfg.data_dir = ConfigPath::new(tmp.path());
        let inst = cfg
            .allocate_instance(
                Some(&InstanceName::new("projects").unwrap()),
                &crate::config::ImageName::new("default").unwrap(),
                None,
            )
            .unwrap();
        (tmp, cfg, inst)
    }

    fn assign(cfg: &CoopConfig, inst: &Instance) {
        Assignment {
            repo: RepoSlug::new("org/assigned").unwrap(),
        }
        .save(cfg, inst)
        .unwrap();
    }

    fn session_token(
        cfg: &CoopConfig,
        inst: &Instance,
        repo: Option<&RepoSlug>,
    ) -> anyhow::Result<Option<String>> {
        let target = SshTarget {
            host: Hostname::new("127.0.0.1").unwrap(),
            port: std::num::NonZeroU16::new(22).unwrap(),
            user: SshUser::new("ubuntu").unwrap(),
            key_path: inst.dir.join("unused"),
        };
        let session = crate::commands::prepare_session_from_target(cfg, Some(inst), target, repo)?;
        Ok(session.env.as_envs().get("GITHUB_TOKEN").cloned())
    }

    #[test]
    fn assignment_selection_rotation_unassignment_and_vm_independence() {
        let (_tmp, mut cfg, inst) = fixture();
        let repo = RepoSlug::new("org/workspace").unwrap();
        let other = cfg
            .allocate_instance(
                Some(&InstanceName::new("other").unwrap()),
                &crate::config::ImageName::new("default").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(
            session_token(&cfg, &inst, Some(&repo)).unwrap().as_deref(),
            Some("github_pat_default")
        );
        assign(&cfg, &inst);
        for context in [None, Some(&repo)] {
            assert_eq!(
                session_token(&cfg, &inst, context).unwrap().as_deref(),
                Some("github_pat_assigned")
            );
        }
        assert_eq!(
            session_token(&cfg, &other, Some(&repo)).unwrap().as_deref(),
            Some("github_pat_default")
        );
        let crate::config::GitHubAuth::Pat(auth) = cfg.github.as_mut().unwrap() else {
            panic!()
        };
        auth.entries
            .get_mut(&RepoSlug::new("org/assigned").unwrap())
            .unwrap()
            .token = crate::config::Secret::new("github_pat_rotated".into());
        assert_eq!(
            session_token(&cfg, &inst, None).unwrap().as_deref(),
            Some("github_pat_rotated")
        );
        let saved = std::fs::read_to_string(inst.dir.join("github_pat.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap(),
            serde_json::json!({"repo": "org/assigned"})
        );
        assert_eq!(
            std::fs::metadata(inst.dir.join("github_pat.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        Assignment::remove(&inst).unwrap();
        Assignment::remove(&inst).unwrap();
        assert!(Assignment::load(&inst).unwrap().is_none());
        assert_eq!(
            session_token(&cfg, &inst, Some(&repo)).unwrap().as_deref(),
            Some("github_pat_default")
        );
        assert!(
            cfg.github
                .as_ref()
                .unwrap()
                .pat_entry(&RepoSlug::new("org/assigned").unwrap())
                .is_some()
        );
    }

    #[test]
    fn assignment_missing_failed_and_malformed_fail_closed_except_opt_out() {
        let (_tmp, mut cfg, inst) = fixture();
        let bad = Assignment {
            repo: RepoSlug::new("org/missing").unwrap(),
        };
        assert!(bad.save(&cfg, &inst).is_err());
        assert!(Assignment::load(&inst).unwrap().is_none());
        assign(&cfg, &inst);
        let crate::config::GitHubAuth::Pat(auth) = cfg.github.as_mut().unwrap() else {
            panic!()
        };
        auth.entries.remove(&RepoSlug::new("org/assigned").unwrap());
        assert!(
            session_token(&cfg, &inst, None)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
        cfg.github_disabled = true;
        cfg.github = Some(crate::config::GitHubAuth::Off);
        assert!(session_token(&cfg, &inst, None).unwrap().is_none());
        for state in [
            "null",
            "{}",
            "{",
            r#"{"repo":"bad"}"#,
            r#"{"repo":"org/assigned","token":"unexpected"}"#,
        ] {
            std::fs::write(inst.dir.join("github_pat.json"), state).unwrap();
            assert!(active(&cfg, &inst).unwrap().is_none());
            cfg.github_disabled = false;
            assert!(active(&cfg, &inst).is_err(), "{state}");
            cfg.github_disabled = true;
        }
        let (_tmp2, mut cfg2, inst2) = fixture();
        assign(&cfg2, &inst2);
        let crate::config::GitHubAuth::Pat(auth) = cfg2.github.as_mut().unwrap() else {
            panic!()
        };
        auth.entries
            .get_mut(&RepoSlug::new("org/assigned").unwrap())
            .unwrap()
            .token = crate::config::Secret::new("cmd:exit 42".into());
        assert!(
            session_token(
                &cfg2,
                &inst2,
                Some(&RepoSlug::new("org/workspace").unwrap())
            )
            .is_err()
        );
    }

    #[test]
    fn assignment_state_io_errors_are_not_absence() {
        let (_tmp, cfg, inst) = fixture();
        std::fs::create_dir(inst.dir.join("github_pat.json")).unwrap();
        assert!(Assignment::load(&inst).is_err());
        assert!(Assignment::remove(&inst).is_err());
        assert!(active(&cfg, &inst).is_err());
        std::fs::remove_dir(inst.dir.join("github_pat.json")).unwrap();
        std::os::unix::fs::symlink("missing-target", inst.dir.join("github_pat.json")).unwrap();
        assert!(active(&cfg, &inst).is_err());
        Assignment::remove(&inst).unwrap();
        assert!(Assignment::load(&inst).unwrap().is_none());

        let target = inst.dir.join("valid-assignment.json");
        std::fs::write(&target, r#"{"repo":"org/assigned"}"#).unwrap();
        std::os::unix::fs::symlink(&target, inst.dir.join("github_pat.json")).unwrap();
        assert!(Assignment::load(&inst).is_err());
        Assignment::remove(&inst).unwrap();
        assert!(
            target.is_file(),
            "removal must unlink the association, not its target"
        );

        let regular_file = inst.dir.join("not-a-directory");
        std::fs::write(&regular_file, "file").unwrap();
        let mut invalid_parent = inst.clone();
        invalid_parent.dir = regular_file.join("child");
        assert!(Assignment::load(&invalid_parent).is_err());
    }

    #[test]
    fn assignment_rejects_each_managed_override_source() {
        for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
            for source in 0..4 {
                let (_tmp, mut cfg, inst) = fixture();
                assign(&cfg, &inst);
                let key = EnvVarName::new(name).unwrap();
                match source {
                    0 => {
                        cfg.guest_env.insert(key, "not-printed".into());
                    }
                    1 => cfg.claude.env_forward.push(key),
                    2 => cfg.codex.env_forward.push(key),
                    _ => {
                        let mut state = GuestEnvState::default();
                        state.entries.insert(key, "not-printed".into());
                        state.save(&inst).unwrap();
                    }
                }
                let err = session_token(&cfg, &inst, None).unwrap_err().to_string();
                assert!(err.contains(name), "{err}");
                assert!(!err.contains("not-printed"));
                Assignment::remove(&inst).unwrap();
                assert!(active(&cfg, &inst).unwrap().is_none());
            }
        }
        reject_overrides(["GH_TOKEN_OTHER", "OTHER_GITHUB_TOKEN", "NORMAL"]).unwrap();
    }

    #[test]
    fn assignment_rejects_grok_token_forwarding() {
        for name in ["GITHUB_TOKEN", "GH_TOKEN"] {
            for source in 0..2 {
                let (_tmp, mut cfg, inst) = fixture();
                assign(&cfg, &inst);
                let key = EnvVarName::new(name).unwrap();
                if source == 0 {
                    cfg.grok.env_forward.push(key);
                } else {
                    let mut env = std::collections::BTreeMap::new();
                    env.insert(EnvVarName::new("TOKEN").unwrap(), key);
                    cfg.grok.mcp_servers.insert(
                        "tool".into(),
                        crate::config::McpServerDef::Stdio {
                            command: "npx".into(),
                            args: vec![],
                            env,
                        },
                    );
                }
                let err = session_token(&cfg, &inst, None).unwrap_err().to_string();
                assert!(err.contains(name), "{err}");
                assert!(!err.contains("not-printed"));
            }
        }
    }
}
