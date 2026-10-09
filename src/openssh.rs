use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::backend::SshTarget;
use crate::config::{AbsoluteHostToolPath, SshHostToolsConfig};
use crate::host_tool::{
    ResolvedHostTool, TrustedLaunchContext, TrustedToolPolicy, resolve_exact_host_tool,
    resolve_host_tool,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenSshTool {
    Ssh,
    Scp,
    Rsync,
}

impl OpenSshTool {
    fn name(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Scp => "scp",
            Self::Rsync => "rsync",
        }
    }

    fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Ssh => &["/usr/bin/ssh", "/bin/ssh"],
            Self::Scp => &["/usr/bin/scp", "/bin/scp"],
            Self::Rsync => &["/usr/bin/rsync", "/bin/rsync"],
        }
    }

    fn configured(self, config: &SshHostToolsConfig) -> Option<&AbsoluteHostToolPath> {
        match self {
            Self::Ssh => config.ssh.as_ref(),
            Self::Scp => config.scp.as_ref(),
            Self::Rsync => config.rsync.as_ref(),
        }
    }
}

impl fmt::Display for OpenSshTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Trusted ownership policy for unprivileged transport clients.
///
/// Linux and macOS both prefer root-owned system clients. Exact paths may also
/// be owned by the invoking user so installations such as a private Nix
/// profile can be selected deliberately. Group/other-writable path components
/// remain forbidden in either case.
pub(crate) fn configured_tool_policy() -> TrustedToolPolicy {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    #[cfg(target_os = "macos")]
    let roots = ["/usr/bin", "/bin"];
    #[cfg(not(target_os = "macos"))]
    let roots = ["/usr/bin", "/bin"];
    TrustedToolPolicy::new_with_owners("/", roots.map(PathBuf::from), [0, euid])
}

pub(crate) fn resolve(
    tool: OpenSshTool,
    config: &SshHostToolsConfig,
) -> Result<ResolvedHostTool<OpenSshTool>> {
    let policy = configured_tool_policy();
    if let Some(path) = tool.configured(config) {
        return resolve_exact_host_tool(tool, path.as_path(), &policy).with_context(|| {
            format!(
                "configured ssh.host_tools.{} path '{}' is not trusted; built-in fallback was not attempted",
                tool.name(),
                path.as_path().display()
            )
        });
    }
    let candidates = tool
        .candidates()
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    resolve_host_tool(tool, &candidates, &policy).map_err(Into::into)
}

/// Report whether a built-in transport client is absent while preserving
/// configuration and trust failures as errors.
pub(crate) fn prerequisite_available(
    tool: OpenSshTool,
    config: &SshHostToolsConfig,
) -> Result<bool> {
    if tool.configured(config).is_some() {
        resolve(tool, config)?;
        return Ok(true);
    }

    let candidates = tool
        .candidates()
        .iter()
        .map(|candidate| (*candidate, fs::symlink_metadata(candidate).map(|_| ())));
    if any_candidate_exists(candidates)? {
        resolve(tool, config)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn any_candidate_exists<'a>(
    candidates: impl IntoIterator<Item = (&'a str, std::io::Result<()>)>,
) -> Result<bool> {
    let mut candidate_exists = false;
    for (candidate, result) in candidates {
        match result {
            Ok(()) => candidate_exists = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to inspect built-in transport candidate '{candidate}'")
                });
            }
        }
    }
    Ok(candidate_exists)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Forwarding {
    Disabled,
    Explicit,
}

pub(crate) fn literal_path(path: &Path, description: &str) -> Result<String> {
    let path = path
        .to_str()
        .with_context(|| format!("{description} is not valid UTF-8 for OpenSSH"))?;
    if path.contains('$') || path.chars().any(char::is_control) {
        bail!(
            "{description} '{}' contains bytes OpenSSH may reinterpret",
            Path::new(path).display()
        );
    }
    Ok(path.replace('%', "%%"))
}

pub(crate) fn config_path(path: &Path, description: &str) -> Result<String> {
    let path = literal_path(path, description)?;
    if path.bytes().any(|byte| byte.is_ascii_whitespace()) || path.contains(['\\', '"']) {
        Ok(format!(
            "\"{}\"",
            path.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    } else {
        Ok(path)
    }
}

/// Options shared by direct SSH, SCP, and rsync's SSH transport.
///
/// `ConnectTimeout` bounds connection and banner exchange. `BatchMode`
/// refuses password and passphrase prompts, and `ServerAlive*` bounds an
/// established session whose guest sshd stops responding without affecting a
/// healthy long-running remote command.
pub(crate) fn authority_options(target: &SshTarget, forwarding: Forwarding) -> Result<Vec<String>> {
    Ok(vec![
        "-F".into(),
        "none".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "GlobalKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "UpdateHostKeys=no".into(),
        "-o".into(),
        "VerifyHostKeyDNS=no".into(),
        "-o".into(),
        "ForwardAgent=no".into(),
        "-o".into(),
        "ForwardX11=no".into(),
        "-o".into(),
        "ForwardX11Trusted=no".into(),
        "-o".into(),
        "IdentityAgent=none".into(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-o".into(),
        "CertificateFile=none".into(),
        "-o".into(),
        "AddKeysToAgent=no".into(),
        "-o".into(),
        "PreferredAuthentications=publickey".into(),
        "-o".into(),
        "PasswordAuthentication=no".into(),
        "-o".into(),
        "KbdInteractiveAuthentication=no".into(),
        "-o".into(),
        "GSSAPIAuthentication=no".into(),
        "-o".into(),
        "HostbasedAuthentication=no".into(),
        "-o".into(),
        "CanonicalizeHostname=no".into(),
        "-o".into(),
        "PermitLocalCommand=no".into(),
        "-o".into(),
        "ProxyCommand=none".into(),
        "-o".into(),
        "ProxyJump=none".into(),
        "-o".into(),
        "PKCS11Provider=none".into(),
        "-o".into(),
        "SecurityKeyProvider=none".into(),
        "-o".into(),
        "KnownHostsCommand=none".into(),
        "-o".into(),
        format!(
            "ClearAllForwardings={}",
            if forwarding == Forwarding::Explicit {
                "no"
            } else {
                "yes"
            }
        ),
        "-o".into(),
        "Tunnel=no".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        format!(
            "IdentityFile={}",
            config_path(&target.key_path, "managed SSH identity path")?
        ),
    ])
}

fn command(tool: &ResolvedHostTool<OpenSshTool>) -> Command {
    TrustedLaunchContext::system().command(tool).build()
}

pub(crate) fn ssh_command(target: &SshTarget, forwarding: Forwarding) -> Result<Command> {
    let tool = resolve(OpenSshTool::Ssh, &target.host_tools)?;
    let mut command = command(&tool);
    command.args(authority_options(target, forwarding)?);
    command.args(["-p", &target.port.to_string()]);
    Ok(command)
}

pub(crate) fn scp_command(target: &SshTarget) -> Result<Command> {
    let scp = resolve(OpenSshTool::Scp, &target.host_tools)?;
    // scp launches ssh as a child. Pin that nested executable explicitly so
    // it cannot bypass the same trusted-path policy applied to direct SSH.
    let ssh = resolve(OpenSshTool::Ssh, &target.host_tools)?;
    let mut command = command(&scp);
    command.arg("-q");
    command.args(authority_options(target, Forwarding::Disabled)?);
    command.arg("-S").arg(ssh.launch_path());
    command.args(["-P", &target.port.to_string()]);
    Ok(command)
}

pub(crate) fn rsync_command(target: &SshTarget) -> Result<Command> {
    let tool = resolve(OpenSshTool::Rsync, &target.host_tools)?;
    Ok(command(&tool))
}

fn rsync_word(value: &OsStr, description: &str) -> Result<String> {
    let value = value
        .to_str()
        .with_context(|| format!("{description} is not valid UTF-8 for rsync's -e parser"))?;
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'/' | b'_' | b'-' | b'.' | b':' | b'=' | b'@' | b'+' | b'%'
                )
        })
    {
        bail!(
            "{description} '{}' cannot be represented unambiguously in rsync's -e command",
            Path::new(value).display()
        );
    }
    Ok(value.to_owned())
}

pub(crate) fn rsync_ssh_command(target: &SshTarget) -> Result<String> {
    let ssh = resolve(OpenSshTool::Ssh, &target.host_tools)?;
    let mut words = Vec::new();
    words.push(rsync_word(
        ssh.launch_path().as_os_str(),
        "resolved ssh path",
    )?);
    for option in authority_options(target, Forwarding::Disabled)? {
        words.push(rsync_word(OsStr::new(&option), "OpenSSH transport option")?);
    }
    words.push("-p".to_string());
    words.push(target.port.to_string());
    Ok(words.join(" "))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures")]
mod tests {
    use std::fs;
    use std::num::NonZeroU16;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn target(config: SshHostToolsConfig) -> SshTarget {
        SshTarget {
            host: crate::backend::Hostname::new("127.0.0.1").unwrap(),
            port: NonZeroU16::new(2222).unwrap(),
            user: crate::backend::SshUser::new("ubuntu").unwrap(),
            key_path: PathBuf::from("/tmp/coop-test-key"),
            host_tools: config,
        }
    }

    #[test]
    fn tool_identities_have_stable_diagnostics() {
        for (tool, expected) in [
            (OpenSshTool::Ssh, "ssh"),
            (OpenSshTool::Scp, "scp"),
            (OpenSshTool::Rsync, "rsync"),
        ] {
            assert_eq!(tool.name(), expected);
            assert_eq!(tool.to_string(), expected);
        }
    }

    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn assert_scp_uses_ssh(command: &Command, ssh: &Path) {
        let args = command.get_args().collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == [OsStr::new("-S"), ssh.as_os_str()])
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the complete isolated SSH argv is one contract"
    )]
    fn exact_tools_build_absolute_isolated_commands() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let ssh_path = executable(fixture.path(), "ssh");
        let scp = executable(fixture.path(), "scp");
        let rsync = executable(fixture.path(), "rsync");
        let config =
            SshHostToolsConfig::with_exact_paths(Some(&ssh_path), Some(&scp), Some(&rsync))
                .unwrap();
        let target = target(config);

        for (expected, command) in [
            (
                &ssh_path,
                ssh_command(&target, Forwarding::Disabled).unwrap(),
            ),
            (&scp, scp_command(&target).unwrap()),
            (&rsync, rsync_command(&target).unwrap()),
        ] {
            assert_eq!(command.get_program(), expected.as_os_str());
            assert_eq!(command.get_current_dir(), Some(Path::new("/")));
            let env = command.get_envs().collect::<Vec<_>>();
            assert_eq!(env.len(), 2);
            assert!(env.contains(&(OsStr::new("LANG"), Some(OsStr::new("C")))));
            assert!(env.contains(&(OsStr::new("LC_ALL"), Some(OsStr::new("C")))));
        }

        let ssh = ssh_command(&target, Forwarding::Disabled).unwrap();
        let ssh_args = ssh
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ssh_args,
            [
                "-F",
                "none",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "UpdateHostKeys=no",
                "-o",
                "VerifyHostKeyDNS=no",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-o",
                "ForwardX11Trusted=no",
                "-o",
                "IdentityAgent=none",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "CertificateFile=none",
                "-o",
                "AddKeysToAgent=no",
                "-o",
                "PreferredAuthentications=publickey",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "GSSAPIAuthentication=no",
                "-o",
                "HostbasedAuthentication=no",
                "-o",
                "CanonicalizeHostname=no",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ProxyCommand=none",
                "-o",
                "ProxyJump=none",
                "-o",
                "PKCS11Provider=none",
                "-o",
                "SecurityKeyProvider=none",
                "-o",
                "KnownHostsCommand=none",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "Tunnel=no",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "IdentityFile=/tmp/coop-test-key",
                "-p",
                "2222",
            ]
        );

        assert_scp_uses_ssh(&scp_command(&target).unwrap(), &ssh_path);
    }

    #[test]
    fn forward_mode_preserves_only_call_site_forwards() {
        let target = target(SshHostToolsConfig::default());
        let session = ssh_command(&target, Forwarding::Disabled).unwrap();
        let forward = ssh_command(&target, Forwarding::Explicit).unwrap();
        assert!(
            session
                .get_args()
                .any(|arg| arg == "ClearAllForwardings=yes")
        );
        assert!(
            forward
                .get_args()
                .any(|arg| arg == "ClearAllForwardings=no")
        );
        assert!(
            !forward
                .get_args()
                .any(|arg| { matches!(arg.to_str(), Some("-L" | "-R" | "-D")) })
        );
    }

    #[test]
    fn hostile_path_is_ignored_by_builtin_resolution() {
        let target = target(SshHostToolsConfig::default());
        let command = ssh_command(&target, Forwarding::Disabled).unwrap();
        assert!(Path::new(command.get_program()).is_absolute());
        assert_ne!(command.get_program(), OsStr::new("ssh"));
    }

    #[test]
    fn configured_failure_never_falls_back() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let missing = fixture.path().join("missing-ssh");
        let config = SshHostToolsConfig::with_exact_paths(Some(&missing), None, None).unwrap();
        let error = ssh_command(&target(config), Forwarding::Disabled).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("built-in fallback was not attempted"));
        assert!(message.contains("missing-ssh"));
    }

    #[test]
    fn scp_rejects_an_untrusted_nested_ssh_before_launch() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let marker = fixture.path().join("scp-launched");
        let scp = fixture.path().join("scp");
        fs::write(
            &scp,
            format!("#!/bin/sh\ntouch {}\n", marker.to_string_lossy()),
        )
        .unwrap();
        fs::set_permissions(&scp, fs::Permissions::from_mode(0o700)).unwrap();
        let missing_ssh = fixture.path().join("missing-ssh");
        let config =
            SshHostToolsConfig::with_exact_paths(Some(&missing_ssh), Some(&scp), None).unwrap();

        let error = scp_command(&target(config)).unwrap_err();

        assert!(format!("{error:#}").contains("missing-ssh"));
        assert!(!marker.exists());
    }

    #[test]
    fn prerequisite_check_distinguishes_configured_success_from_failure() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let ssh = executable(fixture.path(), "ssh");
        let valid = SshHostToolsConfig::with_exact_paths(Some(&ssh), None, None).unwrap();
        assert!(prerequisite_available(OpenSshTool::Ssh, &valid).unwrap());

        let missing = fixture.path().join("missing-ssh");
        let invalid = SshHostToolsConfig::with_exact_paths(Some(&missing), None, None).unwrap();
        let error = prerequisite_available(OpenSshTool::Ssh, &invalid)
            .unwrap_err()
            .to_string();
        assert!(error.contains("built-in fallback was not attempted"));
    }

    #[test]
    fn candidate_presence_distinguishes_missing_present_and_inspection_failure() {
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(
            !any_candidate_exists([("/one", Err(missing())), ("/two", Err(missing()))]).unwrap()
        );
        assert!(any_candidate_exists([("/one", Err(missing())), ("/two", Ok(()))]).unwrap());

        let error = any_candidate_exists([
            ("/one", Err(missing())),
            (
                "/two",
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            ),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("Failed to inspect built-in transport candidate '/two'"));
    }

    #[test]
    fn rsync_transport_uses_resolved_ssh_and_rejects_ambiguous_exact_path() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let safe = executable(fixture.path(), "safe-ssh");
        let safe_config = SshHostToolsConfig::with_exact_paths(Some(&safe), None, None).unwrap();
        let transport = rsync_ssh_command(&target(safe_config)).unwrap();
        assert!(transport.starts_with(safe.to_str().unwrap()));
        assert!(transport.contains(" -F none "));

        let spaced_dir = fixture.path().join("with space");
        fs::create_dir(&spaced_dir).unwrap();
        let ambiguous = executable(&spaced_dir, "ssh");
        let ambiguous_config =
            SshHostToolsConfig::with_exact_paths(Some(&ambiguous), None, None).unwrap();
        assert!(rsync_ssh_command(&target(ambiguous_config)).is_err());
    }

    #[test]
    fn installed_rsync_parser_preserves_the_transport_argv() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let capture = fixture.path().join("ssh-argv");
        let ssh = fixture.path().join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 1\n",
                crate::shell::shell_escape(&capture.to_string_lossy())
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let config = SshHostToolsConfig::with_exact_paths(Some(&ssh), None, None).unwrap();
        let target = target(config);
        let transport = rsync_ssh_command(&target).unwrap();
        let destination = fixture.path().join("destination");
        fs::create_dir(&destination).unwrap();

        let status = rsync_command(&target)
            .unwrap()
            .args([
                "-e",
                &transport,
                "ubuntu@127.0.0.1:/missing",
                destination.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        let argv = fs::read_to_string(capture).unwrap();
        let args = argv.lines().collect::<Vec<_>>();
        assert_eq!(args.first(), Some(&"-F"));
        assert_eq!(args.get(1), Some(&"none"));
        assert!(args.contains(&"IdentityFile=/tmp/coop-test-key"));
        assert!(
            args.contains(&"ubuntu@127.0.0.1")
                || (args.windows(2).any(|pair| pair == ["-l", "ubuntu"])
                    && args.contains(&"127.0.0.1")),
            "argv = {args:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair == ["rsync", "--server"]),
            "argv = {args:?}"
        );
    }

    #[test]
    fn production_transport_sources_have_no_bare_launches() {
        for (name, source) in [
            ("backend", include_str!("backend.rs")),
            ("workspace", include_str!("workspace.rs")),
            ("port_forward", include_str!("port_forward.rs")),
            ("proxy", include_str!("proxy.rs")),
            ("ssh", include_str!("ssh.rs")),
            ("lima", include_str!("lima.rs")),
        ] {
            for forbidden in [
                "Command::new(\"ssh\")",
                "Command::new(\"scp\")",
                "Command::new(\"rsync\")",
                "Cmd::new(\"ssh\")",
                "Cmd::new(\"scp\")",
                "Cmd::new(\"rsync\")",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{name} contains a bare managed transport launch: {forbidden}"
                );
            }
        }
    }

    #[test]
    fn rsync_words_reject_ambiguous_and_non_utf8_values() {
        assert!(rsync_word(OsStr::new("/safe/path"), "path").is_ok());
        assert!(rsync_word(OsStr::new("/path with space"), "path").is_err());
        assert!(rsync_word(OsStr::new("-oProxyCommand=sh -c true"), "option").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            assert!(rsync_word(OsStr::from_bytes(b"/bad/\xff"), "path").is_err());
        }
    }

    #[test]
    fn openssh_paths_escape_tokens_quote_spaces_and_reject_environment_expansion() {
        assert_eq!(
            literal_path(Path::new("/tmp/control-%h-%p"), "control path").unwrap(),
            "/tmp/control-%%h-%%p"
        );
        assert_eq!(
            config_path(Path::new("/tmp/key with space"), "identity path").unwrap(),
            "\"/tmp/key with space\""
        );
        assert!(config_path(Path::new("/tmp/${HOME}/key"), "identity path").is_err());

        let mut target = target(SshHostToolsConfig::default());
        target.key_path = PathBuf::from("/tmp/key-%h");
        let command = ssh_command(&target, Forwarding::Disabled).unwrap();
        assert!(
            command
                .get_args()
                .any(|arg| arg == "IdentityFile=/tmp/key-%%h")
        );
        let transport = rsync_ssh_command(&target).unwrap();
        assert!(transport.contains("IdentityFile=/tmp/key-%%h"));
    }
}
