//! Host executables used by Firecracker setup. Preflight checks the complete
//! coop-selected phase family before effects; each launch revalidates its selected path
//! against the policy. Pathname validation does not pin the executed inode.
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use thiserror::Error;

use crate::cmd::Cmd;
use crate::host_tool::{
    ResolveHostToolError, ResolvedHostTool, TrustedLaunchContext, TrustedToolPolicy,
    resolve_host_tool,
};

const SYSTEM_HELPER_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const ROOT_TIMEOUT_GRACE: &str = "--kill-after=2s";

#[derive(Debug, Error)]
pub(crate) enum SetupToolError {
    #[error(
        "Firecracker setup tool '{tool}' is absent from supported system directories; install its system package and retry"
    )]
    Absent { tool: &'static str },
    #[error(
        "Firecracker setup tool '{tool}' is present but unusable: {reason}; repair its system package and retry"
    )]
    Unusable { tool: &'static str, reason: String },
    #[error(
        "Firecracker setup tool '{tool}' has a rejected identity: {source}; repair its ownership or installation and retry"
    )]
    Rejected {
        tool: &'static str,
        #[source]
        source: ResolveHostToolError,
    },
    #[error("Firecracker setup tool '{tool}' candidate is not an absolute path: {path}")]
    InvalidCandidate { tool: &'static str, path: PathBuf },
    #[error(
        "System apt configuration is not root-controlled: {reason}; repair it before retrying setup"
    )]
    AptConfiguration { reason: String },
}

pub(crate) fn validate_apt_configuration() -> Result<(), SetupToolError> {
    let policy = TrustedToolPolicy::new("/", [], 0);
    validate_apt_configuration_at(Path::new("/etc/apt"), &policy)
}

fn validate_apt_configuration_at(
    root: &Path,
    policy: &TrustedToolPolicy,
) -> Result<(), SetupToolError> {
    let mut pending = vec![(root.to_path_buf(), false)];
    let mut visited = HashSet::new();
    let mut entries = 0usize;
    while let Some((path, scan_config)) = pending.pop() {
        entries += 1;
        if entries > 4096 {
            return Err(SetupToolError::AptConfiguration {
                reason: "configuration tree exceeds 4096 entries".into(),
            });
        }
        let (canonical, metadata) = crate::host_tool::validate_trusted_path(&path, policy)
            .map_err(|reason| SetupToolError::AptConfiguration { reason })?;
        if !visited.insert((canonical.clone(), scan_config)) {
            continue;
        }
        if metadata.is_dir() {
            let children =
                fs::read_dir(&canonical).map_err(|error| SetupToolError::AptConfiguration {
                    reason: error.to_string(),
                })?;
            for child in children {
                let child = child.map_err(|error| SetupToolError::AptConfiguration {
                    reason: error.to_string(),
                })?;
                let child_path = child.path();
                let child_is_config = scan_config
                    || child_path == root.join("apt.conf")
                    || child_path.starts_with(root.join("apt.conf.d"));
                pending.push((child_path, child_is_config));
            }
        } else if !metadata.is_file() {
            return Err(SetupToolError::AptConfiguration {
                reason: "configuration contains a special file".into(),
            });
        } else if scan_config {
            // apt.conf's #include can reach outside /etc/apt. Apt runs with
            // cwd /, so relative include paths are anchored there too.
            let contents =
                fs::read(&canonical).map_err(|error| SetupToolError::AptConfiguration {
                    reason: error.to_string(),
                })?;
            for line in contents.split(|byte| *byte == b'\n') {
                let mut remaining = line;
                while let Some(start) = remaining.windows(8).position(|part| part == b"#include") {
                    let tail = &remaining[start + 8..];
                    let end = tail.iter().position(|byte| *byte == b';').ok_or_else(|| {
                        SetupToolError::AptConfiguration {
                            reason: "unterminated apt include directive".into(),
                        }
                    })?;
                    let value = std::str::from_utf8(&tail[..end])
                        .map_err(|_| SetupToolError::AptConfiguration {
                            reason: "non-UTF-8 apt include path".into(),
                        })?
                        .trim();
                    let value = value
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .unwrap_or(value);
                    if value.is_empty() || value.contains(['"', '\\', '\n']) {
                        return Err(SetupToolError::AptConfiguration {
                            reason: "unsupported apt include path".into(),
                        });
                    }
                    let included = Path::new(value);
                    pending.push((
                        if included.is_absolute() {
                            included.to_path_buf()
                        } else {
                            Path::new("/").join(included)
                        },
                        true,
                    ));
                    remaining = &tail[end + 1..];
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetupTool {
    Sudo,
    Curl,
    Tar,
    Gzip,
    Chmod,
    Rm,
    Setfacl,
    Usermod,
    AptGet,
    Unsquashfs,
    SshKeygen,
    Mkdir,
    Tee,
    Chroot,
    #[cfg_attr(
        target_os = "macos",
        expect(dead_code, reason = "Firecracker-only cleanup")
    )]
    Rmdir,
    MkfsExt4,
    #[cfg_attr(
        target_os = "macos",
        expect(dead_code, reason = "Firecracker-only disk helper")
    )]
    E2fsck,
    #[cfg_attr(
        target_os = "macos",
        expect(dead_code, reason = "Firecracker-only disk helper")
    )]
    Resize2fs,
    Id,
    Timeout,
}

impl SetupTool {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sudo => "sudo",
            Self::Curl => "curl",
            Self::Tar => "tar",
            Self::Gzip => "gzip",
            Self::Chmod => "chmod",
            Self::Rm => "rm",
            Self::Setfacl => "setfacl",
            Self::Usermod => "usermod",
            Self::AptGet => "apt-get",
            Self::Unsquashfs => "unsquashfs",
            Self::SshKeygen => "ssh-keygen",
            Self::Mkdir => "mkdir",
            Self::Tee => "tee",
            Self::Chroot => "chroot",
            Self::Rmdir => "rmdir",
            Self::MkfsExt4 => "mkfs.ext4",
            Self::E2fsck => "e2fsck",
            Self::Resize2fs => "resize2fs",
            Self::Id => "id",
            Self::Timeout => "timeout",
        }
    }

    fn candidates(self) -> [PathBuf; 6] {
        let name = self.name();
        [
            "/usr/bin",
            "/usr/sbin",
            "/bin",
            "/sbin",
            "/usr/local/bin",
            "/usr/local/sbin",
        ]
        .map(|dir| PathBuf::from(dir).join(name))
    }

    fn policy(self) -> TrustedToolPolicy {
        self.policy_at(Path::new("/"), 0)
    }

    fn policy_at(self, anchor: &Path, uid: u32) -> TrustedToolPolicy {
        let policy = TrustedToolPolicy::new(
            anchor,
            [
                "usr/bin",
                "usr/sbin",
                "bin",
                "sbin",
                "usr/local/bin",
                "usr/local/sbin",
            ]
            .map(|relative| anchor.join(relative)),
            uid,
        );
        if self == Self::Sudo {
            policy.with_allowed_target_beneath_anchor("usr/lib/cargo/bin/sudo")
        } else {
            policy
        }
    }

    pub(crate) fn resolve(self) -> Result<ResolvedHostTool<Self>, SetupToolError> {
        self.resolve_required_in(&self.candidates(), &self.policy())
    }

    fn resolve_required_in(
        self,
        candidates: &[PathBuf],
        policy: &TrustedToolPolicy,
    ) -> Result<ResolvedHostTool<Self>, SetupToolError> {
        self.resolve_in(candidates, policy)?
            .ok_or(SetupToolError::Absent { tool: self.name() })
    }

    /// Absence permits the package-install phase. A present candidate
    /// rejected by the trust policy is never treated as an installable absence.
    pub(crate) fn resolve_if_present(
        self,
    ) -> Result<Option<ResolvedHostTool<Self>>, SetupToolError> {
        self.resolve_in(&self.candidates(), &self.policy())
    }

    fn resolve_in(
        self,
        candidates: &[PathBuf],
        policy: &TrustedToolPolicy,
    ) -> Result<Option<ResolvedHostTool<Self>>, SetupToolError> {
        if let Some(path) = candidates.iter().find(|path| !path.is_absolute()) {
            return Err(SetupToolError::InvalidCandidate {
                tool: self.name(),
                path: path.clone(),
            });
        }
        let mut first = None;
        for candidate in candidates {
            match fs::symlink_metadata(candidate) {
                Ok(_) => {
                    first = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(SetupToolError::Unusable {
                        tool: self.name(),
                        reason: error.to_string(),
                    });
                }
            }
        }
        let Some(first) = first else { return Ok(None) };
        let metadata = fs::metadata(first).map_err(|error| SetupToolError::Unusable {
            tool: self.name(),
            reason: error.to_string(),
        })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(SetupToolError::Unusable {
                tool: self.name(),
                reason: "selected path is not a regular executable file".into(),
            });
        }
        resolve_host_tool(self, candidates, policy)
            .map(Some)
            .map_err(|source| SetupToolError::Rejected {
                tool: self.name(),
                source,
            })
    }

    pub(crate) fn command(self) -> Result<Cmd> {
        let tool = self.resolve()?;
        let mut command = TrustedLaunchContext::system()
            .command(&tool)
            // GNU tar's decompressor and package tools require helper lookup.
            // These fixed system directories do not come from the caller.
            .env("PATH", SYSTEM_HELPER_PATH);
        if self == Self::Curl {
            // curl reads ~/.curlrc even if HOME is absent on some builds.
            // -q must be its first argument to disable that implicit input.
            command = command.arg("-q").arg("--silent").arg("--show-error");
            // HTTPS proxy routing is a supported host setting. No certificate
            // override, curl config, HOME, XDG, or loader variable is inherited.
            for name in ["HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy"] {
                if let Some(value) = std::env::var_os(name) {
                    command = command.env(name, value);
                }
            }
        }
        Ok(command)
    }

    pub(crate) fn elevated(self) -> Result<Cmd> {
        self.elevated_with_deadline(crate::setup::SETUP_TOOL_DEADLINE)
    }

    pub(crate) fn elevated_with_deadline(self, deadline: Duration) -> Result<Cmd> {
        let sudo = Self::Sudo.resolve()?;
        let timeout = Self::Timeout.resolve()?;
        let tool = self.resolve()?;
        Ok(Self::root_timeout_command(&sudo, &timeout, deadline).arg(tool.launch_path()))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn sudo_command(deadline: Duration) -> Result<Cmd> {
        let sudo = Self::Sudo.resolve()?;
        let timeout = Self::Timeout.resolve()?;
        Ok(Self::root_timeout_command(&sudo, &timeout, deadline))
    }

    fn root_timeout_command(
        sudo: &ResolvedHostTool<Self>,
        timeout: &ResolvedHostTool<Self>,
        deadline: Duration,
    ) -> Cmd {
        let root_deadline = deadline.saturating_add(Duration::from_secs(30));
        TrustedLaunchContext::system()
            .command(sudo)
            .env("PATH", SYSTEM_HELPER_PATH)
            .arg("--")
            .arg(timeout.launch_path())
            .arg(ROOT_TIMEOUT_GRACE)
            .arg(format!("{}s", root_deadline.as_secs()))
    }

    pub(crate) fn apt_install_command() -> Result<Cmd> {
        validate_apt_configuration()?;
        let sudo = Self::Sudo.resolve()?;
        let timeout = Self::Timeout.resolve()?;
        let apt = Self::AptGet.resolve()?;
        Ok(Self::apt_command_from(&sudo, &timeout, &apt))
    }

    fn apt_command_from(
        sudo: &ResolvedHostTool<Self>,
        timeout: &ResolvedHostTool<Self>,
        apt: &ResolvedHostTool<Self>,
    ) -> Cmd {
        // The input environment is empty. Preserve only this explicit value
        // through sudo so debconf cannot block an unattended package install.
        TrustedLaunchContext::system()
            .command(sudo)
            .env("PATH", SYSTEM_HELPER_PATH)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .arg("--preserve-env=DEBIAN_FRONTEND")
            .arg("--")
            .arg(timeout.launch_path())
            .arg(ROOT_TIMEOUT_GRACE)
            .arg(format!(
                "{}s",
                (crate::setup::SETUP_TOOL_DEADLINE + Duration::from_secs(30)).as_secs()
            ))
            .arg(apt.launch_path())
    }
}

impl fmt::Display for SetupTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixture and assertions")]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::Path;
    use std::process::Command;

    use super::*;

    #[test]
    fn resolution_distinguishes_absent_unusable_and_rejected() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let root = fixture.path();
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let policy = TrustedToolPolicy::new_with_owners("/", [root.to_path_buf()], [0, uid]);
        let candidate = root.join("curl");
        assert!(matches!(
            SetupTool::Curl.resolve_required_in(std::slice::from_ref(&candidate), &policy),
            Err(SetupToolError::Absent { .. })
        ));
        for relative in ["curl", "./curl"] {
            assert!(matches!(
                SetupTool::Curl.resolve_required_in(&[PathBuf::from(relative)], &policy),
                Err(SetupToolError::InvalidCandidate { .. })
            ));
        }
        assert!(
            SetupTool::Curl
                .resolve_in(std::slice::from_ref(&candidate), &policy)
                .unwrap()
                .is_none()
        );

        fs::write(&candidate, "#!/bin/sh\nprintf trusted-witness\n").unwrap();
        let error = SetupTool::Curl
            .resolve_in(std::slice::from_ref(&candidate), &policy)
            .unwrap_err();
        assert!(matches!(error, SetupToolError::Unusable { .. }));

        let mut permissions = fs::metadata(&candidate).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&candidate, permissions).unwrap();
        let resolved = SetupTool::Curl
            .resolve_in(std::slice::from_ref(&candidate), &policy)
            .unwrap()
            .unwrap();
        let output = TrustedLaunchContext::system()
            .command(&resolved)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"trusted-witness");

        let mut permissions = fs::metadata(&candidate).unwrap().permissions();
        permissions.set_mode(0o757);
        fs::set_permissions(&candidate, permissions).unwrap();
        let error = SetupTool::Curl
            .resolve_in(&[candidate], &policy)
            .unwrap_err();
        assert!(matches!(error, SetupToolError::Rejected { .. }));

        let blocked = root.join("blocked");
        fs::create_dir(&blocked).unwrap();
        let mut permissions = fs::metadata(&blocked).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&blocked, permissions).unwrap();
        let error = SetupTool::Curl
            .resolve_in(&[blocked.join("curl")], &policy)
            .unwrap_err();
        assert!(matches!(error, SetupToolError::Unusable { .. }));
        let mut permissions = fs::metadata(&blocked).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&blocked, permissions).unwrap();
    }

    #[test]
    fn apt_configuration_requires_trusted_files_and_symlink_targets() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let root = fixture.path().join("apt");
        let parts = root.join("apt.conf.d");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parts).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&parts, fs::Permissions::from_mode(0o755)).unwrap();
        let config = parts.join("policy");
        fs::write(&config, "").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let policy = TrustedToolPolicy::new(fixture.path(), [], uid);
        validate_apt_configuration_at(&root, &policy).unwrap();

        fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            validate_apt_configuration_at(&root, &policy),
            Err(SetupToolError::AptConfiguration { .. })
        ));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

        let writable = fixture.path().join("writable");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let target = writable.join("external");
        fs::write(&target, "").unwrap();
        symlink(&target, parts.join("redirect")).unwrap();
        assert!(matches!(
            validate_apt_configuration_at(&root, &policy),
            Err(SetupToolError::AptConfiguration { .. })
        ));
    }

    #[test]
    fn apt_include_outside_tree_must_be_trusted() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let root = fixture.path().join("apt");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("apt.conf");
        let external = fixture.path().join("external.conf");
        fs::write(&external, "").unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&config, format!("#include \"{}\";", external.display())).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let policy = TrustedToolPolicy::new(fixture.path(), [], uid);
        validate_apt_configuration_at(&root, &policy).unwrap();

        let writable = fixture.path().join("writable");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let untrusted = writable.join("hook.conf");
        fs::write(&untrusted, "DPkg::Pre-Invoke { \"true\"; };").unwrap();
        fs::write(&config, format!("#include \"{}\";", untrusted.display())).unwrap();
        assert!(matches!(
            validate_apt_configuration_at(&root, &policy),
            Err(SetupToolError::AptConfiguration { .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_apt_configuration_passes_the_supported_host_policy() {
        if SetupTool::AptGet.resolve_if_present().unwrap().is_some() {
            validate_apt_configuration().unwrap();
        }
    }

    #[test]
    fn only_sudo_accepts_the_documented_non_fhs_target() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let anchor = fixture.path();
        let bin = anchor.join("usr/bin");
        let target_dir = anchor.join("usr/lib/cargo/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        for directory in [
            anchor.join("usr"),
            bin.clone(),
            anchor.join("usr/lib"),
            anchor.join("usr/lib/cargo"),
            target_dir.clone(),
        ] {
            let mut permissions = fs::metadata(&directory).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&directory, permissions).unwrap();
        }
        let target = target_dir.join("sudo");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&target, permissions).unwrap();
        let sudo = bin.join("sudo");
        let curl = bin.join("curl");
        symlink(&target, &sudo).unwrap();
        symlink(&target, &curl).unwrap();
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let sudo_result = SetupTool::Sudo.resolve_required_in(
            std::slice::from_ref(&sudo),
            &SetupTool::Sudo.policy_at(anchor, uid),
        );
        assert!(sudo_result.is_ok(), "{sudo_result:?}");
        assert!(matches!(
            SetupTool::Curl.resolve_required_in(
                std::slice::from_ref(&curl),
                &SetupTool::Curl.policy_at(anchor, uid)
            ),
            Err(SetupToolError::Rejected { .. })
        ));
    }

    #[test]
    fn role_name_is_visible_in_resolution_diagnostics() {
        assert_eq!(SetupTool::Curl.to_string(), "curl");
        assert_eq!(SetupTool::Sudo.to_string(), "sudo");
    }

    #[test]
    fn supported_curl_candidate_is_present_and_runs() {
        let resolved = SetupTool::Curl.resolve_if_present().unwrap().unwrap();
        assert!(resolved.launch_path().is_absolute());
        let output = SetupTool::Curl
            .command()
            .unwrap()
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.starts_with(b"curl "));
    }

    #[test]
    fn apt_launch_preserves_only_noninteractive_frontend_through_resolved_sudo() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let policy =
            TrustedToolPolicy::new_with_owners("/", [fixture.path().to_path_buf()], [0, uid]);
        let sudo_path = fixture.path().join("sudo");
        let apt_path = fixture.path().join("apt-get");
        let timeout_path = fixture.path().join("timeout");
        for path in [&sudo_path, &apt_path, &timeout_path] {
            fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        let sudo =
            resolve_host_tool(SetupTool::Sudo, std::slice::from_ref(&sudo_path), &policy).unwrap();
        let apt =
            resolve_host_tool(SetupTool::AptGet, std::slice::from_ref(&apt_path), &policy).unwrap();
        let timeout = resolve_host_tool(
            SetupTool::Timeout,
            std::slice::from_ref(&timeout_path),
            &policy,
        )
        .unwrap();
        let built = SetupTool::apt_command_from(&sudo, &timeout, &apt).build();
        assert_eq!(built.get_program(), sudo_path.as_os_str());
        assert_eq!(built.get_current_dir(), Some(Path::new("/")));
        let args = built.get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "--preserve-env=DEBIAN_FRONTEND",
                "--",
                timeout_path.to_str().unwrap(),
                ROOT_TIMEOUT_GRACE,
                "21630s",
                apt_path.to_str().unwrap()
            ]
        );
        let env = built.get_envs().collect::<Vec<_>>();
        assert!(env.iter().any(|(name, value)| *name == "DEBIAN_FRONTEND"
            && value.is_some_and(|value| value == "noninteractive")));
        assert!(
            !env.iter()
                .any(|(name, _)| *name == "HOME" || *name == "APT_CONFIG")
        );
    }

    #[test]
    fn curl_launch_ignores_hostile_path_home_and_cwd() {
        const CHILD: &str = "COOP_TEST_SETUP_CURL_CHILD";
        const SOURCE: &str = "COOP_TEST_SETUP_CURL_SOURCE";
        const MARKER: &str = "COOP_TEST_SETUP_CURL_MARKER";
        const CONFIG_MARKER: &str = "COOP_TEST_SETUP_CURL_CONFIG_MARKER";
        if std::env::var_os(CHILD).is_some() {
            let source = std::env::var(SOURCE).unwrap();
            let marker = std::env::var(MARKER).unwrap();
            let config_marker = std::env::var(CONFIG_MARKER).unwrap();
            let command = SetupTool::Curl.command().unwrap();
            let built = command.build();
            let result = command.arg(format!("file://{source}")).output().unwrap();
            assert!(result.status.success());
            assert_eq!(result.stdout, b"setup-tool-witness");
            assert!(!Path::new(&marker).exists());
            assert!(!Path::new(&config_marker).exists());
            let argv_destination = Path::new(&source).with_file_name("argv-output$(printf X)");
            let output = SetupTool::Curl
                .command()
                .unwrap()
                .arg("-o")
                .arg(&argv_destination)
                .arg(format!("file://{source}"))
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(fs::read(&argv_destination).unwrap(), b"setup-tool-witness");
            assert!(!Path::new(&source).with_file_name("argv-outputX").exists());
            assert!(!Path::new(&marker).exists());
            assert!(built.get_program().to_string_lossy().starts_with('/'));
            assert_eq!(built.get_current_dir(), Some(Path::new("/")));
            let vars = built.get_envs().collect::<Vec<_>>();
            assert!(vars.iter().any(|(key, value)| *key == "PATH"
                && value.is_some_and(|value| value == "/usr/sbin:/usr/bin:/sbin:/bin")));
            assert!(
                !vars
                    .iter()
                    .any(|(key, _)| *key == "HOME" || *key == "XDG_CONFIG_HOME")
            );
            // Even if a future caller deliberately supplies HOME, curl's
            // first-argument -q must keep the default config inert.
            let home = Path::new(&source).parent().unwrap();
            let result = SetupTool::Curl
                .command()
                .unwrap()
                .env("HOME", home)
                .arg(format!("file://{source}"))
                .output()
                .unwrap();
            assert!(result.status.success());
            assert_eq!(result.stdout, b"setup-tool-witness");
            assert!(!Path::new(&config_marker).exists());
            return;
        }

        let fixture = crate::host_tool::trusted_test_tempdir();
        let source = fixture.path().join("source");
        let marker = fixture.path().join("marker");
        let config_marker = fixture.path().join("curlrc-marker");
        fs::write(&source, "setup-tool-witness").unwrap();
        let lookalike = fixture.path().join("curl");
        fs::write(
            &lookalike,
            format!(
                "#!/bin/sh\n/usr/bin/touch '{}'\nexit 77\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&lookalike).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lookalike, permissions).unwrap();
        fs::write(
            fixture.path().join(".curlrc"),
            format!("output = \"{}\"\n", config_marker.display()),
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "setup_tool::tests::curl_launch_ignores_hostile_path_home_and_cwd",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(SOURCE, &source)
            .env(MARKER, &marker)
            .env(CONFIG_MARKER, &config_marker)
            .env("PATH", fixture.path())
            .env("HOME", fixture.path())
            .env("XDG_CONFIG_HOME", fixture.path())
            .current_dir(fixture.path())
            .output()
            .unwrap();
        assert!(!marker.exists(), "hostile PATH executable ran");
        assert!(!config_marker.exists(), "curl read the hostile config");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
