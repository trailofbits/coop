//! Persisted, resumable creation commands executed only inside the guest.

use std::fmt;
use std::process::{Child, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::backend::SshSession;
use crate::config::{CoopConfig, Instance};
use crate::remote_command::RemoteCommand;

/// A NUL-free shell command with redacted debug output.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct CreationCommand(String);

impl TryFrom<String> for CreationCommand {
    type Error = anyhow::Error;

    fn try_from(command: String) -> Result<Self> {
        ensure!(!command.contains('\0'), "Creation command contains NUL");
        Ok(Self(command))
    }
}

impl From<CreationCommand> for String {
    fn from(command: CreationCommand) -> Self {
        command.0
    }
}

impl fmt::Debug for CreationCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<creation command>")
    }
}

impl CreationCommand {
    fn shell(command: &str) -> Result<Self> {
        Self::try_from(command.to_owned())
    }

    fn render(&self) -> String {
        RemoteCommand::new()
            .literal("exec sh -c ")
            .arg(&self.0)
            .into_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Completion {
    Pending,
    Succeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Preparation {
    Waiting,
    Ready,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Stage {
    command: CreationCommand,
    completion: Completion,
}

impl Stage {
    fn new(command: CreationCommand) -> Self {
        Self {
            command,
            completion: Completion::Pending,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreationState {
    preparation: Preparation,
    global: Option<Stage>,
    post_start: Option<String>,
}

impl fmt::Debug for CreationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreationState")
            .field("preparation", &self.preparation)
            .field("global", &self.global)
            .field("has_post_start", &self.post_start.is_some())
            .finish()
    }
}

pub(crate) enum CreationProgress {
    Unchanged,
    Finished(Option<String>),
}

impl CreationProgress {
    pub(crate) fn post_start(
        self,
        override_command: Option<&str>,
        fallback: Option<&str>,
    ) -> Option<String> {
        if let Some(command) = override_command {
            return Some(command.to_owned());
        }
        match self {
            Self::Unchanged => fallback.map(str::to_owned),
            Self::Finished(command) => command,
        }
    }
}

impl CreationState {
    pub(crate) fn select(cfg: &CoopConfig, post_start: Option<&str>) -> Result<Self> {
        Ok(Self {
            preparation: Preparation::Waiting,
            global: cfg
                .post_create
                .as_deref()
                .map(CreationCommand::shell)
                .transpose()?
                .map(Stage::new),
            post_start: post_start.or(cfg.post_start.as_deref()).map(str::to_owned),
        })
    }

    pub(crate) fn save(&self, inst: &Instance) -> Result<()> {
        crate::fs_util::atomic_write_json(
            &inst.dir.join("creation.json"),
            &serde_json::to_string_pretty(self)?,
        )
        .context("Cannot save creation-hook progress")
    }

    pub(crate) fn load(inst: &Instance) -> Result<Option<Self>> {
        crate::fs_util::read_optional_private(&inst.dir.join("creation.json"))
            .context("Cannot read creation-hook progress")?
            .map(|contents| {
                serde_json::from_str(&contents)
                    .context("Invalid creation.json; restore its saved recipe before retrying")
            })
            .transpose()
    }

    fn pending(&self) -> bool {
        self.global
            .as_ref()
            .is_some_and(|stage| stage.completion == Completion::Pending)
    }

    fn reset(&mut self) {
        self.preparation = Preparation::Waiting;
        if let Some(stage) = &mut self.global {
            stage.completion = Completion::Pending;
        }
    }
}

/// Serialize boot prerequisites and running creation retries before backend locks.
pub(crate) fn lock_provisioning(inst: &Instance) -> Result<crate::fs_util::FileLock> {
    let target = inst
        .dir
        .with_file_name(format!(".{}.provisioning", inst.name));
    crate::fs_util::lock_sibling_bounded(&target, Duration::from_secs(30))
}

pub(crate) fn set_preparation(inst: &Instance, preparation: Preparation) -> Result<()> {
    if let Some(mut state) = CreationState::load(inst)? {
        state.preparation = preparation;
        state.save(inst)?;
    }
    Ok(())
}

fn ensure_prepared(state: &CreationState) -> Result<()> {
    ensure!(
        state.preparation == Preparation::Ready,
        "Boot prerequisites are unfinished; stop the VM and run `coop start` to retry provisioning"
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Creation setup for '{instance}' is unfinished. The VM is retained; \
    use `coop shell {instance}` or `coop exec {instance}` to debug, \
    then retry `coop up` or stop it and run `coop start {instance}`. {source}"
)]
pub(crate) struct CreationIncomplete {
    instance: String,
    #[source]
    source: anyhow::Error,
}

/// Reset progress before disk replacement, preserving completion on an intact-disk failure.
pub(crate) fn restore_disk(
    inst: &Instance,
    disk: &std::path::Path,
    restore: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let Some(original) = CreationState::load(inst)? else {
        return restore();
    };
    let directory = crate::fs_util::PrivateDir::open_existing(
        disk.parent().context("Instance disk has no parent")?,
    )?;
    let name = disk.file_name().context("Instance disk has no name")?;
    let pinned = directory
        .pin_existing_regular(name)
        .context("Cannot pin instance disk before restore")?;
    let mut reset = original.clone();
    reset.reset();
    reset.save(inst)?;
    let Err(error) = restore() else {
        return Ok(());
    };
    let unchanged = match pinned
        .as_ref()
        .map(|file| directory.names_file(name, file))
        .transpose()
    {
        Ok(unchanged) => unchanged.unwrap_or(false),
        Err(probe) => {
            return Err(error).context(format!(
                "Cannot verify instance disk; creation hooks remain pending: {probe:#}"
            ));
        }
    };
    if unchanged && let Err(rollback) = original.save(inst) {
        return Err(error).context(format!(
            "Disk is unchanged, but cannot restore creation-hook progress: {rollback:#}"
        ));
    }
    Err(error)
}

pub(crate) fn pending(inst: &Instance) -> Result<bool> {
    Ok(CreationState::load(inst)?.is_some_and(|state| state.pending()))
}

pub(crate) fn ensure_complete(inst: &Instance) -> Result<()> {
    ensure!(
        !pending(inst)?,
        "Creation setup is unfinished for '{}'; retry `coop up` before launching an agent. \
        `coop shell {}` and `coop exec {}` remain available.",
        inst.name,
        inst.name,
        inst.name
    );
    Ok(())
}

impl CreationIncomplete {
    pub(crate) fn new(inst: &Instance, source: anyhow::Error) -> Self {
        Self {
            instance: inst.name.to_string(),
            source,
        }
    }
}

pub(crate) fn run_pending(inst: &Instance, session: &SshSession) -> Result<CreationProgress> {
    let _lock = crate::backend::lock_instance_operation(inst)?;
    let Some(mut state) = CreationState::load(inst)? else {
        return Ok(CreationProgress::Unchanged);
    };
    let Some(mut stage) = state.global.take() else {
        return Ok(CreationProgress::Unchanged);
    };
    if stage.completion == Completion::Succeeded {
        return Ok(CreationProgress::Unchanged);
    }
    ensure_prepared(&state)?;
    let directory = crate::guest::GuestUser::new(session.target.user.as_ref())?.home();
    tracing::info!("Running global post_create");
    execute(session, &stage.command, directory.as_ref()).context("global post_create failed")?;
    stage.completion = Completion::Succeeded;
    state.global = Some(stage);
    state.save(inst)?;
    Ok(CreationProgress::Finished(state.post_start))
}

fn script(command: &CreationCommand, directory: &str) -> String {
    let body = RemoteCommand::new()
        .literal("cd -- ")
        .arg(directory)
        .literal("\n")
        .literal(command.render())
        .into_string();
    format!(
        "#!/bin/bash\nset -euo pipefail\nif [[ ${{1:-}} == --command ]]; then\n{body}\nfi\n{}",
        include_str!("../scripts/guest/creation-hook.sh")
    )
}

fn execute(session: &SshSession, command: &CreationCommand, directory: &str) -> Result<()> {
    let staging = tempfile::Builder::new().prefix("coop-create-").tempdir()?;
    let name = staging
        .path()
        .file_name()
        .context("Missing hook staging name")?
        .to_str()
        .context("Invalid hook staging name")?;
    let remote = format!("/tmp/{name}");
    let result = (|| {
        session.target.exec_with_stdin(
            RemoteCommand::new()
                .literal("umask 077; mkdir -- ")
                .arg(&remote)
                .literal(" && cat > ")
                .arg(&remote)
                .literal("/hook.sh"),
            script(command, directory).as_bytes(),
        )?;
        execute_staged(session, &remote)
    })();
    if let Err(error) = session.target.exec(
        RemoteCommand::new()
            .literal("test ! -e ")
            .arg(&remote)
            .literal(" || rm -r -- ")
            .arg(&remote),
    ) {
        tracing::warn!("Cannot remove guest creation-hook staging directory: {error}");
    }
    result
}

fn execute_staged(session: &SshSession, remote: &str) -> Result<()> {
    let command = RemoteCommand::new()
        .literal("bash ")
        .arg(remote)
        .literal("/hook.sh");
    let mut child = session
        .command(&["-tt".into()], &command.into_string())?
        .stdin(Stdio::null())
        .spawn()
        .context("Cannot launch creation-hook SSH session")?;
    let result = wait_for_hook(&mut child);
    if result.is_err() {
        let _ = child.kill();
        child
            .wait()
            .context("Cannot reap creation-hook SSH process")?;
    }
    result
}

fn wait_for_hook(child: &mut Child) -> Result<()> {
    loop {
        crate::signal::check_shutdown()?;
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "Guest creation command exited with {status}"
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures")]
mod tests {
    use crate::config::{CoopConfig, ImageName, Instance, InstanceIndex, InstanceName};
    use crate::creation_hooks::{
        Completion, CreationCommand, CreationProgress, CreationState, Preparation, ensure_complete,
        ensure_prepared, execute_staged, pending, restore_disk, script, set_preparation,
        wait_for_hook,
    };
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::process::Command;

    fn instance(dir: &Path) -> Instance {
        Instance {
            dir: dir.to_owned(),
            name: InstanceName::new("hooks").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            image: ImageName::new("default").unwrap(),
        }
    }

    #[test]
    fn startup_hook_selection_preserves_retry_recipe_and_cli_precedence() {
        assert_eq!(
            CreationProgress::Finished(Some("saved".into())).post_start(None, Some("global")),
            Some("saved".into())
        );
        assert_eq!(
            CreationProgress::Finished(None).post_start(None, Some("global")),
            None
        );
        assert_eq!(
            CreationProgress::Unchanged.post_start(None, Some("global")),
            Some("global".into())
        );
        assert_eq!(
            CreationProgress::Finished(Some("saved".into()))
                .post_start(Some("cli"), Some("global")),
            Some("cli".into())
        );
        assert_eq!(CreationProgress::Unchanged.post_start(None, None), None);
        let cfg = CoopConfig {
            post_start: Some("global".into()),
            ..CoopConfig::default()
        };
        assert_eq!(
            CreationState::select(&cfg, Some("cli"))
                .unwrap()
                .post_start
                .as_deref(),
            Some("cli")
        );
        assert_eq!(
            CreationState::select(&cfg, None)
                .unwrap()
                .post_start
                .as_deref(),
            Some("global")
        );
    }

    #[test]
    fn shell_commands_round_trip_without_exposing_their_contents() {
        let command = CreationCommand::shell("echo a && echo b").unwrap();
        assert_eq!(command.0, "echo a && echo b");
        let encoded = serde_json::to_string(&command).unwrap();
        let decoded: CreationCommand = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.0, command.0);
        assert_eq!(format!("{command:?}"), "<creation command>");
    }

    #[test]
    fn malformed_commands_are_rejected() {
        for value in [
            serde_json::json!("bad\0command"),
            serde_json::json!([]),
            serde_json::json!(["sh", "-c", "true"]),
            serde_json::json!({"command": "true"}),
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
        ] {
            assert!(
                serde_json::from_value::<CreationCommand>(value.clone()).is_err(),
                "{value}"
            );
        }
        assert!(CreationCommand::shell("bad\0command").is_err());
        assert!(CreationCommand::shell("").is_ok());
    }

    proptest::proptest! {
        #[test]
        fn shell_serialization_preserves_command(command in "[^\\x00]{0,120}") {
            let value = CreationCommand::shell(&command).unwrap();
            let decoded: CreationCommand = serde_json::from_value(
                serde_json::to_value(value).unwrap()
            ).unwrap();
            proptest::prop_assert_eq!(decoded.0, command);
        }
    }

    #[test]
    fn selected_recipe_is_private_and_independent_of_later_config() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        let mut cfg = CoopConfig {
            post_create: Some("original secret".into()),
            ..CoopConfig::default()
        };
        CreationState::select(&cfg, None)
            .unwrap()
            .save(&inst)
            .unwrap();
        cfg.post_create = Some("changed".into());
        let state = CreationState::load(&inst).unwrap().unwrap();
        assert_eq!(state.global.as_ref().unwrap().command.0, "original secret");
        assert!(state.pending());
        let debug = format!("{state:?}");
        assert!(debug.contains("CreationState"));
        assert!(!debug.contains("original secret"));
        assert_eq!(
            fs::metadata(root.path().join("creation.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(ensure_complete(&inst).is_err());
    }

    fn completed_restore_fixture(root: &Path) -> (Instance, std::path::PathBuf) {
        let inst = instance(root);
        let cfg = CoopConfig {
            post_create: Some("saved setup".into()),
            post_start: Some("saved startup".into()),
            ..CoopConfig::default()
        };
        let mut state = CreationState::select(&cfg, None).unwrap();
        state.preparation = Preparation::Ready;
        state.global.as_mut().unwrap().completion = Completion::Succeeded;
        state.save(&inst).unwrap();
        let disk = root.join("disk");
        fs::write(&disk, "old disk").unwrap();
        (inst, disk)
    }

    #[test]
    fn failed_restore_preserves_completed_progress_on_original_disk() {
        let root = tempfile::tempdir().unwrap();
        let (inst, disk) = completed_restore_fixture(root.path());
        let original = fs::read(root.path().join("creation.json")).unwrap();
        let error = crate::creation_hooks::restore_disk(&inst, &disk, || {
            assert!(pending(&inst).unwrap());
            anyhow::bail!("copy failed before replacement")
        })
        .unwrap_err();
        assert!(error.to_string().contains("copy failed before replacement"));
        assert_eq!(fs::read(&disk).unwrap(), b"old disk");
        assert_eq!(
            fs::read(root.path().join("creation.json")).unwrap(),
            original
        );
        assert!(!pending(&inst).unwrap());
    }

    #[test]
    fn destructive_restore_failures_keep_progress_pending() {
        for change in ["removed", "replaced", "symlink", "missing initially"] {
            let root = tempfile::tempdir().unwrap();
            let (inst, disk) = completed_restore_fixture(root.path());
            if change == "missing initially" {
                fs::remove_file(&disk).unwrap();
            }
            let error = crate::creation_hooks::restore_disk(&inst, &disk, || {
                if change != "missing initially" {
                    fs::remove_file(&disk)?;
                }
                if change == "replaced" {
                    fs::write(&disk, "replacement disk")?;
                } else if change == "symlink" {
                    symlink(root.path().join("creation.json"), &disk)?;
                }
                anyhow::bail!("replacement did not finish")
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "replacement did not finish");
            let state = CreationState::load(&inst).unwrap().unwrap();
            assert!(state.pending(), "{change}");
            assert_eq!(state.preparation, Preparation::Waiting, "{change}");
            assert_eq!(state.post_start.as_deref(), Some("saved startup"));
        }
    }

    #[test]
    fn successful_restore_keeps_progress_pending() {
        let root = tempfile::tempdir().unwrap();
        let (inst, disk) = completed_restore_fixture(root.path());
        crate::creation_hooks::restore_disk(&inst, &disk, || {
            fs::remove_file(&disk)?;
            fs::write(&disk, "new disk")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&disk).unwrap(), b"new disk");
        assert!(pending(&inst).unwrap());
    }

    #[test]
    fn unsafe_disk_or_corrupt_progress_prevents_restore() {
        for failure in ["symlink", "hardlink", "corrupt recipe"] {
            let root = tempfile::tempdir().unwrap();
            let (inst, disk) = completed_restore_fixture(root.path());
            let recipe = root.path().join("creation.json");
            let before = fs::read(&recipe).unwrap();
            if failure == "symlink" {
                fs::remove_file(&disk).unwrap();
                symlink(&recipe, &disk).unwrap();
            } else if failure == "hardlink" {
                fs::hard_link(&disk, root.path().join("alias")).unwrap();
            } else {
                fs::write(&recipe, "invalid").unwrap();
            }
            let called = std::cell::Cell::new(false);
            assert!(
                crate::creation_hooks::restore_disk(&inst, &disk, || {
                    called.set(true);
                    Ok(())
                })
                .is_err()
            );
            assert!(!called.get(), "{failure}");
            if failure != "corrupt recipe" {
                assert_eq!(fs::read(&recipe).unwrap(), before);
            }
        }
    }

    #[test]
    fn unverifiable_disk_keeps_progress_pending_and_reports_probe_error() {
        // Permission failures cannot be reproduced by an effective root user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (inst, disk) = completed_restore_fixture(root.path());
        let error = crate::creation_hooks::restore_disk(&inst, &disk, || {
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o000))?;
            anyhow::bail!("copy failed with unverifiable disk")
        });
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let error = format!("{:#}", error.unwrap_err());
        assert!(error.contains("Cannot verify instance disk"), "{error}");
        assert!(
            error.contains("copy failed with unverifiable disk"),
            "{error}"
        );
        assert!(pending(&inst).unwrap());
        assert_eq!(fs::read(&disk).unwrap(), b"old disk");
    }

    #[test]
    fn progress_write_failure_prevents_disk_restore() {
        if let Some(root) = std::env::var_os("COOP_RESTORE_WRITE_FAILURE") {
            let root = std::path::Path::new(&root);
            let (inst, disk) = completed_restore_fixture(root);
            let original = fs::read(root.join("creation.json")).unwrap();
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: only this isolated test child changes its signal handler and file limit.
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit), 0);
            }
            let called = std::cell::Cell::new(false);
            let error = crate::creation_hooks::restore_disk(&inst, &disk, || {
                called.set(true);
                Ok(())
            })
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Cannot save creation-hook progress")
            );
            assert!(!called.get());
            assert_eq!(fs::read(root.join("creation.json")).unwrap(), original);
            assert!(!pending(&inst).unwrap());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "creation_hooks::tests::progress_write_failure_prevents_disk_restore",
            ])
            .env("COOP_RESTORE_WRITE_FAILURE", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn rollback_failure_reports_both_errors() {
        let root = tempfile::tempdir().unwrap();
        let (inst, disk) = completed_restore_fixture(root.path());
        let recipe = root.path().join("creation.json");
        let error = crate::creation_hooks::restore_disk(&inst, &disk, || {
            fs::rename(&recipe, root.path().join("pending.json"))?;
            fs::create_dir(&recipe)?;
            anyhow::bail!("disk copy failed")
        })
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("cannot restore creation-hook progress"),
            "{error}"
        );
        assert!(error.contains("disk copy failed"), "{error}");
        assert_eq!(fs::read(&disk).unwrap(), b"old disk");
        assert!(CreationState::load(&inst).is_err());
    }

    #[test]
    fn legacy_restore_has_no_creation_progress_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        let called = std::cell::Cell::new(false);
        let error =
            crate::creation_hooks::restore_disk(&inst, &root.path().join("missing"), || {
                called.set(true);
                anyhow::bail!("legacy restore failed")
            })
            .unwrap_err();
        assert!(called.get());
        assert_eq!(error.to_string(), "legacy restore failed");
        assert!(!root.path().join("creation.json").exists());
    }

    #[test]
    fn completed_hook_is_skipped_and_restore_resets_progress() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        let cfg = CoopConfig {
            post_create: Some("global".into()),
            ..CoopConfig::default()
        };
        let state = CreationState::select(&cfg, None).unwrap();
        assert!(ensure_prepared(&state).is_err());
        state.save(&inst).unwrap();
        set_preparation(&inst, Preparation::Ready).unwrap();
        let mut state = CreationState::load(&inst).unwrap().unwrap();
        assert!(ensure_prepared(&state).is_ok());
        state.global.as_mut().unwrap().completion = Completion::Succeeded;
        state.save(&inst).unwrap();
        let state = CreationState::load(&inst).unwrap().unwrap();
        assert_eq!(
            state.global.as_ref().unwrap().completion,
            Completion::Succeeded
        );
        assert!(!pending(&inst).unwrap());
        assert!(ensure_complete(&inst).is_ok());
        restore_disk(&inst, &root.path().join("missing-disk"), || Ok(())).unwrap();
        let state = CreationState::load(&inst).unwrap().unwrap();
        assert!(ensure_prepared(&state).is_err());
        assert_eq!(
            state.global.as_ref().unwrap().completion,
            Completion::Pending
        );
    }

    #[test]
    fn legacy_and_empty_recipes_are_ready_but_corruption_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        assert!(ensure_complete(&inst).is_ok());
        restore_disk(&inst, &root.path().join("missing-disk"), || Ok(())).unwrap();
        assert!(!root.path().join("creation.json").exists());
        set_preparation(&inst, Preparation::Ready).unwrap();
        assert!(!root.path().join("creation.json").exists());
        CreationState::select(&CoopConfig::default(), None)
            .unwrap()
            .save(&inst)
            .unwrap();
        assert!(!pending(&inst).unwrap());
        fs::write(root.path().join("creation.json"), "invalid").unwrap();
        assert!(pending(&inst).is_err());
        assert!(ensure_complete(&inst).is_err());
        assert!(restore_disk(&inst, &root.path().join("missing-disk"), || Ok(())).is_err());
    }

    #[test]
    fn unreadable_recipe_is_not_treated_as_a_legacy_instance() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        fs::create_dir(root.path().join("creation.json")).unwrap();
        assert!(CreationState::load(&inst).is_err());
        assert!(ensure_complete(&inst).is_err());
        assert!(restore_disk(&inst, &root.path().join("missing-disk"), || Ok(())).is_err());
    }

    #[test]
    fn recipe_reads_and_replacements_tighten_private_storage() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(&root.path().join("instance"));
        let cfg = CoopConfig {
            post_create: Some("saved command".into()),
            ..CoopConfig::default()
        };
        let state = CreationState::select(&cfg, None).unwrap();
        state.save(&inst).unwrap();
        let recipe = inst.dir.join("creation.json");
        for read in [true, false] {
            fs::set_permissions(&inst.dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&recipe, fs::Permissions::from_mode(0o644)).unwrap();
            if read {
                let saved = CreationState::load(&inst).unwrap().unwrap();
                assert_eq!(saved.global.unwrap().command.0, "saved command");
            } else {
                state.save(&inst).unwrap();
            }
            assert_eq!(
                fs::metadata(&inst.dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&recipe).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn recipe_links_cannot_bypass_completion_or_overwrite_other_files() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(&root.path().join("instance"));
        let state = CreationState::select(&CoopConfig::default(), None).unwrap();
        state.save(&inst).unwrap();
        let recipe = inst.dir.join("creation.json");
        let target = root.path().join("outside.json");
        let contents = fs::read_to_string(&recipe).unwrap();
        fs::write(&target, &contents).unwrap();
        for hard_link in [false, true] {
            fs::remove_file(&recipe).unwrap();
            if hard_link {
                fs::hard_link(&target, &recipe).unwrap();
            } else {
                symlink(&target, &recipe).unwrap();
            }
            assert!(CreationState::load(&inst).is_err());
            assert!(ensure_complete(&inst).is_err());
            assert!(state.save(&inst).is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), contents);
        }
        fs::remove_file(&recipe).unwrap();
        symlink(root.path().join("missing"), &recipe).unwrap();
        assert!(CreationState::load(&inst).is_err());
        assert!(state.save(&inst).is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn recipes_reject_symlinked_and_writable_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("shared");
        let inst = instance(&parent.join("instance"));
        let state = CreationState::select(&CoopConfig::default(), None).unwrap();
        state.save(&inst).unwrap();
        let alias = root.path().join("alias");
        symlink(&parent, &alias).unwrap();
        let linked = instance(&alias.join("instance"));
        assert!(CreationState::load(&linked).is_err());
        assert!(state.save(&linked).is_err());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(CreationState::load(&inst).is_err());
        assert!(state.save(&inst).is_err());
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[test]
    fn missing_instance_recipe_does_not_create_storage() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(&root.path().join("missing/instance"));
        assert!(CreationState::load(&inst).unwrap().is_none());
        assert!(!root.path().join("missing").exists());
    }

    fn run_creation_transport_fixture(root: &Path) {
        let mut env = crate::backend::EnvForward::default();
        env.set(
            "PATH",
            format!("{}/guest-bin:/usr/bin:/bin", root.display()),
        )
        .unwrap();
        env.set("LD_LIBRARY_PATH", "/creation-guest-libraries")
            .unwrap();
        env.set("SECRET", "creation-guest-only").unwrap();
        env.set("COOP_SSH_ENV_0", "creation-alias").unwrap();
        let session = crate::backend::SshSession {
            target: crate::backend::SshTarget {
                host: crate::backend::Hostname::new("127.0.0.1").unwrap(),
                port: std::num::NonZeroU16::MIN,
                user: crate::backend::SshUser::new("ubuntu").unwrap(),
                key_path: root.join("unused-key"),
                host_tools: crate::config::SshHostToolsConfig::with_exact_paths(
                    Some(&root.join("host-bin/ssh")),
                    None,
                    None,
                )
                .unwrap(),
            },
            env,
        };
        execute_staged(&session, root.to_str().unwrap()).unwrap();
    }

    fn prepare_creation_transport_fixture(root: &Path) {
        fs::create_dir(root.join("host-bin")).unwrap();
        fs::create_dir(root.join("guest-bin")).unwrap();
        let ssh = root.join("host-bin/ssh");
        fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
set -eu
test "${{LD_LIBRARY_PATH-unset}}" = unset
test "${{SECRET-unset}}" = unset
test "${{COOP_CREATION_FIXTURE-unset}}" = unset
printf host > {0}/host-launched
for arg do remote=$arg; done
SHELL=/bin/bash /bin/sh -c "$remote"
"#,
                crate::shell::shell_escape(&root.to_string_lossy())
            ),
        )
        .unwrap();
        fs::set_permissions(ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let injected = root.join("guest-bin/ssh");
        fs::write(
            &injected,
            "#!/bin/sh\nprintf injected > \"$COOP_CREATION_FIXTURE/host-injected\"\n",
        )
        .unwrap();
        fs::set_permissions(injected, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            root.join("hook.sh"),
            format!(
                r#"#!/bin/bash
set -euo pipefail
test "$PATH" = {1}
test "$LD_LIBRARY_PATH" = /creation-guest-libraries
test "$SECRET" = creation-guest-only
test "$COOP_SSH_ENV_0" = creation-alias
printf guest > {0}/guest-restored
"#,
                crate::shell::shell_escape(&root.to_string_lossy()),
                crate::shell::shell_escape(&format!("{}/guest-bin:/usr/bin:/bin", root.display())),
            ),
        )
        .unwrap();
    }

    #[test]
    fn creation_transport_keeps_guest_environment_off_host() {
        if let Some(root) = std::env::var_os("COOP_CREATION_FIXTURE") {
            run_creation_transport_fixture(Path::new(&root));
            return;
        }
        let root = crate::host_tool::trusted_test_tempdir();
        prepare_creation_transport_fixture(root.path());
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "creation_hooks::tests::creation_transport_keeps_guest_environment_off_host",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", root.path().join("host-bin"))
            .env("COOP_CREATION_FIXTURE", root.path())
            .output()
            .unwrap();
        assert!(!root.path().join("host-injected").exists());
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(root.path().join("host-launched")).unwrap(),
            b"host"
        );
        assert_eq!(
            fs::read(root.path().join("guest-restored")).unwrap(),
            b"guest"
        );
    }

    #[test]
    fn global_shell_command_preserves_quoting_and_selected_directory() {
        let root = tempfile::tempdir().unwrap();
        let command =
            CreationCommand::shell(r#"printf '%s' "literal \$(touch injected); ' space" > result"#)
                .unwrap();
        let path = root.path().join("hook.sh");
        fs::write(&path, script(&command, root.path().to_str().unwrap())).unwrap();
        let output = Command::new("bash")
            .arg(path)
            .arg("--command")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(root.path().join("result")).unwrap(),
            "literal $(touch injected); ' space"
        );
        assert!(!root.path().join("injected").exists());
    }

    #[test]
    fn removed_project_and_argv_recipes_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        let cfg = CoopConfig {
            post_create: Some("true".into()),
            ..CoopConfig::default()
        };
        let recipe = serde_json::to_value(CreationState::select(&cfg, None).unwrap()).unwrap();
        for project in [serde_json::Value::Null, recipe["global"].clone()] {
            let mut invalid = recipe.clone();
            invalid["project"] = project;
            fs::write(root.path().join("creation.json"), invalid.to_string()).unwrap();
            assert!(CreationState::load(&inst).is_err());
            assert!(ensure_complete(&inst).is_err());
        }
        let mut invalid = recipe;
        invalid["global"]["command"] = serde_json::json!(["sh", "-c", "true"]);
        fs::write(root.path().join("creation.json"), invalid.to_string()).unwrap();
        assert!(CreationState::load(&inst).is_err());
        assert!(ensure_complete(&inst).is_err());
    }

    #[test]
    fn missing_hook_directory_fails_without_entering_supervisor() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("hook.sh");
        let command = CreationCommand::shell("touch executed").unwrap();
        fs::write(
            &path,
            script(&command, root.path().join("missing").to_str().unwrap()),
        )
        .unwrap();
        let output = Command::new("bash")
            .arg(path)
            .arg("--command")
            .env("HOME", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.path().join(".coop-creation.lock").exists());
    }

    #[test]
    fn hook_wait_reports_exit_failure_and_reaps_success() {
        for code in [0, 7] {
            let mut child = Command::new("sh")
                .args(["-c", &format!("exit {code}")])
                .spawn()
                .unwrap();
            assert_eq!(wait_for_hook(&mut child).is_ok(), code == 0);
            assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(code));
        }
    }
}
