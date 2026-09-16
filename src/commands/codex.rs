//! Guest `ChatGPT` keyring installation and interactive unlock.
use anyhow::{Result, bail};

use crate::backend::{self, PlatformBackend};
use crate::commands::open_ssh_session;
use crate::config::{CoopConfig, InstanceName};
use crate::guest;
use crate::model_state::ModelState;
use crate::remote_command::RemoteCommand;
use crate::ssh;

pub(crate) fn cmd_codex_unlock(
    be: &PlatformBackend,
    cfg: &CoopConfig,
    name: Option<&InstanceName>,
) -> Result<()> {
    if !cfg.codex.auth.uses_chatgpt_account() {
        bail!("`coop codex-unlock` requires [codex] auth = \"chatgpt\"");
    }
    let inst = cfg.resolve_instance(name)?;
    let model_state = ModelState::load_or_default(&inst)?;
    backend::ensure_codex_remote_auth_consistent(cfg, &inst, &model_state)?;
    let session = open_ssh_session(be, cfg, name)?;
    backend::ensure_codex_keyring_configured(&session.target)?;
    if !session.target.exec_ok(RemoteCommand::new().literal(
        "test -x /usr/local/bin/codex-keyring \
         && test -x /usr/local/libexec/coop-codex-keyring-pam \
         && test -f /etc/tmpfiles.d/coop-codex-keyring.conf \
         && test -f /var/lib/coop/codex-session-v1 \
         && test -f /var/lib/coop/codex-keyring-install-boot",
    )) {
        // All installer bytes are embedded trusted source. SSH user is a
        // validated newtype and crosses the shell boundary via arg().
        let script = format!(
            "{}\n{}\n{}",
            guest::SCRIPT_CODEX_KEYRING_MIGRATE,
            guest::SCRIPT_CODEX_KEYRING,
            guest::SCRIPT_CODEX_ACCOUNT,
        );
        tracing::info!("Installing shared guest keyring support; a VM restart is required");
        session.target.exec_with_stdin(
            RemoteCommand::new()
                .literal("sudo env ")
                .arg(format!("GUEST_USER={}", session.target.user))
                .literal(" bash -s"),
            script.into_bytes(),
        )?;
        bail!(
            "Guest keyring support installed. Stop and start this VM, then rerun `coop codex-unlock`."
        );
    }
    ssh::run_interactive_checked(&session, &["/usr/local/bin/codex-keyring".into()])
}
