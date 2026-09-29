//! Persisted, resumable creation commands executed only inside the guest.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::backend::SshSession;
use crate::config::{CoopConfig, Instance};
use crate::remote_command::RemoteCommand;

/// A guest command whose arguments never pass through a host shell.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct CreationCommand(Vec<String>);

impl TryFrom<Vec<String>> for CreationCommand {
    type Error = anyhow::Error;

    fn try_from(args: Vec<String>) -> Result<Self> {
        ensure!(
            !args.is_empty() && !args[0].is_empty(),
            "Creation command needs an executable"
        );
        ensure!(
            args.iter().all(|arg| !arg.contains('\0')),
            "Creation command contains NUL"
        );
        Ok(Self(args))
    }
}

impl From<CreationCommand> for Vec<String> {
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
    pub(crate) fn shell(command: &str) -> Result<Self> {
        Self::try_from(vec!["sh".into(), "-c".into(), command.into()])
    }

    pub(crate) fn from_devcontainer(value: &serde_json::Value) -> Result<Self> {
        match value {
            serde_json::Value::String(command) => Self::shell(command),
            serde_json::Value::Array(_) => {
                Self::try_from(serde_json::from_value::<Vec<String>>(value.clone())?)
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Object(_) => bail!(
                "postCreateCommand must be a string or a nonempty argv array; objects are unsupported"
            ),
        }
    }

    fn render(&self) -> String {
        let mut command = RemoteCommand::new().literal("exec");
        for arg in &self.0 {
            command = command.literal(" ").arg(arg);
        }
        command.into_string()
    }
}

enum HookKind {
    Global,
    Project,
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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreationState {
    preparation: Preparation,
    global: Option<Stage>,
    project: Option<Stage>,
    post_start: Option<String>,
}

impl fmt::Debug for CreationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreationState")
            .field("preparation", &self.preparation)
            .field("global", &self.global)
            .field("project", &self.project)
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
            project: cfg.project_post_create.clone().map(Stage::new),
            post_start: post_start.or(cfg.post_start.as_deref()).map(str::to_owned),
        })
    }

    pub(crate) fn save(&self, inst: &Instance) -> Result<()> {
        crate::fs_util::atomic_write_with_mode(
            &inst.dir.join("creation.json"),
            &serde_json::to_string_pretty(self)?,
            0o600,
        )
        .context("Cannot save creation-hook progress")
    }

    pub(crate) fn load(inst: &Instance) -> Result<Option<Self>> {
        match fs::read_to_string(inst.dir.join("creation.json")) {
            Ok(contents) => Ok(Some(serde_json::from_str(&contents).context(
                "Invalid creation.json; restore its saved recipe before retrying",
            )?)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("Cannot read creation-hook progress"),
        }
    }

    fn pending(&self) -> bool {
        for stage in [&self.global, &self.project].into_iter().flatten() {
            if stage.completion == Completion::Pending {
                return true;
            }
        }
        false
    }

    fn reset(&mut self) {
        self.preparation = Preparation::Waiting;
        for stage in [&mut self.global, &mut self.project].into_iter().flatten() {
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

pub(crate) fn invalidate(inst: &Instance) -> Result<()> {
    if let Some(mut state) = CreationState::load(inst)? {
        state.reset();
        state.save(inst)?;
    }
    Ok(())
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
    if !state.pending() {
        return Ok(CreationProgress::Unchanged);
    }
    ensure_prepared(&state)?;
    for kind in [HookKind::Global, HookKind::Project] {
        let (stage, label, directory) = match kind {
            HookKind::Global => (
                &mut state.global,
                "global post_create",
                crate::guest::GuestUser::new(session.target.user.as_ref())?
                    .home()
                    .to_string(),
            ),
            HookKind::Project => (
                &mut state.project,
                "project postCreateCommand",
                "/workspace".to_owned(),
            ),
        };
        let Some(stage) = stage else {
            continue;
        };
        if stage.completion == Completion::Succeeded {
            continue;
        }
        tracing::info!("Running {label}");
        execute(session, &stage.command, &directory).with_context(|| format!("{label} failed"))?;
        stage.completion = Completion::Succeeded;
        state.save(inst)?;
    }
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
            script(command, directory).into_bytes(),
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
    let mut child = Command::new("ssh")
        .args(session.ssh_opts())
        .arg("-tt")
        .arg(session.target.addr())
        .arg(command.into_string())
        .envs(session.env.as_envs())
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
        ensure_prepared, invalidate, pending, script, set_preparation, wait_for_hook,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
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
    fn shell_and_argv_forms_preserve_arguments() {
        let shell =
            CreationCommand::from_devcontainer(&serde_json::json!("echo a && echo b")).unwrap();
        assert_eq!(shell.0, ["sh", "-c", "echo a && echo b"]);
        let argv = CreationCommand::from_devcontainer(&serde_json::json!([
            "echo", "a && b", "", "quote's"
        ]))
        .unwrap();
        assert_eq!(argv.0, ["echo", "a && b", "", "quote's"]);
        let encoded = serde_json::to_string(&argv).unwrap();
        let decoded: CreationCommand = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.0, argv.0);
        assert_eq!(format!("{argv:?}"), "<creation command>");
    }

    #[test]
    fn malformed_commands_are_rejected() {
        for value in [
            serde_json::json!([]),
            serde_json::json!([""]),
            serde_json::json!(["echo", 3]),
            serde_json::json!(["echo", "\0"]),
            serde_json::json!({"parallel": "echo x"}),
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
        ] {
            assert!(
                CreationCommand::from_devcontainer(&value).is_err(),
                "{value}"
            );
        }
        assert!(CreationCommand::shell("bad\0command").is_err());
        assert!(CreationCommand::shell("").is_ok());
    }

    proptest::proptest! {
        #[test]
        fn argv_serialization_preserves_arguments(
            executable in "[^\\x00]{1,40}",
            rest in proptest::collection::vec("[^\\x00]{0,80}", 0..8),
        ) {
            let mut args = vec![executable];
            args.extend(rest);
            let value = serde_json::to_value(&args).unwrap();
            let command = CreationCommand::from_devcontainer(&value).unwrap();
            proptest::prop_assert_eq!(&command.0, &args);
            let decoded: CreationCommand = serde_json::from_value(
                serde_json::to_value(command).unwrap()
            ).unwrap();
            proptest::prop_assert_eq!(decoded.0, args);
        }
    }

    #[test]
    fn selected_recipe_is_private_and_independent_of_later_config() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        let mut cfg = CoopConfig {
            post_create: Some("original secret".into()),
            project_post_create: Some(CreationCommand::shell("project").unwrap()),
            ..CoopConfig::default()
        };
        CreationState::select(&cfg, None)
            .unwrap()
            .save(&inst)
            .unwrap();
        cfg.post_create = Some("changed".into());
        let state = CreationState::load(&inst).unwrap().unwrap();
        assert_eq!(
            state.global.as_ref().unwrap().command.0[2],
            "original secret"
        );
        assert_eq!(state.project.as_ref().unwrap().command.0[2], "project");
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

    #[test]
    fn stages_complete_independently_and_restore_resets_both() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        let cfg = CoopConfig {
            post_create: Some("global".into()),
            project_post_create: Some(CreationCommand::shell("project").unwrap()),
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
        assert!(pending(&inst).unwrap());
        let mut state = CreationState::load(&inst).unwrap().unwrap();
        assert_eq!(
            state.global.as_ref().unwrap().completion,
            Completion::Succeeded
        );
        state.project.as_mut().unwrap().completion = Completion::Succeeded;
        state.save(&inst).unwrap();
        assert!(!pending(&inst).unwrap());
        assert!(ensure_complete(&inst).is_ok());
        invalidate(&inst).unwrap();
        let state = CreationState::load(&inst).unwrap().unwrap();
        assert!(ensure_prepared(&state).is_err());
        assert_eq!(
            state.global.as_ref().unwrap().completion,
            Completion::Pending
        );
        assert_eq!(
            state.project.as_ref().unwrap().completion,
            Completion::Pending
        );
    }

    #[test]
    fn legacy_and_empty_recipes_are_ready_but_corruption_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        assert!(ensure_complete(&inst).is_ok());
        invalidate(&inst).unwrap();
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
        assert!(invalidate(&inst).is_err());
    }

    #[test]
    fn unreadable_recipe_is_not_treated_as_a_legacy_instance() {
        let root = tempfile::tempdir().unwrap();
        let inst = instance(root.path());
        fs::create_dir(root.path().join("creation.json")).unwrap();
        assert!(CreationState::load(&inst).is_err());
        assert!(ensure_complete(&inst).is_err());
        assert!(invalidate(&inst).is_err());
    }

    #[test]
    fn rendered_argv_cannot_inject_shell_syntax() {
        let root = tempfile::tempdir().unwrap();
        let args = vec![
            "printf".into(),
            "%s".into(),
            "literal $(touch injected); ' space".into(),
        ];
        let command = CreationCommand::try_from(args).unwrap();
        let path = root.path().join("hook.sh");
        fs::write(&path, script(&command, root.path().to_str().unwrap())).unwrap();
        let output = Command::new("bash")
            .arg(path)
            .arg("--command")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "literal $(touch injected); ' space"
        );
        assert!(!root.path().join("injected").exists());
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
