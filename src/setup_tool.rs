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
const APT_CONFIG_CAPTURE_LIMIT: usize = 16 * 1024 * 1024;
const APT_CONFIG_ENTRY_LIMIT: usize = 4096;
const APT_CONFIG_READ_DEADLINE: Duration = Duration::from_secs(30);

fn apt_block_comment_end(input: &[u8]) -> Option<usize> {
    // The opener's `*` cannot also start the closing `*/`.
    let mut offset = 3;
    while offset < input.len() {
        if input[offset - 1] == b'*' && input[offset] == b'/' {
            return Some(offset + 1);
        }
        offset += 1;
    }
    None
}

fn apt_first_newline(input: &[u8]) -> Option<usize> {
    let mut offset = 0;
    while offset < input.len() {
        if input[offset] == b'\n' {
            return Some(offset);
        }
        offset += 1;
    }
    None
}

fn apt_quoted_advance(input: &[u8]) -> usize {
    usize::from(input.starts_with(b"\\") && input.len() > 1) + 1
}

fn apt_directive_has_separator(input: &[u8]) -> bool {
    let mut offset = 0;
    let mut separated = false;
    while offset < input.len() {
        let tail = &input[offset..];
        if tail[0].is_ascii_whitespace() {
            separated = true;
            offset += 1;
        } else if tail.starts_with(b"/*") {
            let Some(end) = apt_block_comment_end(tail) else {
                return false;
            };
            separated |= apt_first_newline(&tail[..end]).is_some();
            offset += end;
        } else if tail.starts_with(b"//") {
            let Some(end) = apt_first_newline(tail) else {
                return false;
            };
            separated = true;
            offset += end + 1;
        } else {
            break;
        }
    }
    separated
}

fn apt_directive_body(input: &[u8]) -> Result<(Vec<u8>, usize), SetupToolError> {
    let mut cursor = 0;
    let mut value = Vec::new();
    let mut quoted = false;
    while cursor < input.len() {
        let tail = &input[cursor..];
        if quoted && apt_quoted_advance(tail) == 2 {
            value.extend_from_slice(&tail[..2]);
            cursor += 2;
            continue;
        }
        if !quoted && tail.starts_with(b"/*") {
            let end =
                apt_block_comment_end(tail).ok_or_else(|| SetupToolError::AptConfiguration {
                    reason: "unterminated apt directive comment".into(),
                })?;
            cursor += end;
            value.push(b' ');
            continue;
        }
        if !quoted && tail.starts_with(b"//") {
            cursor += apt_first_newline(tail).map_or(tail.len(), |end| end + 1);
            value.push(b' ');
            continue;
        }
        if tail[0] == b'"' {
            quoted = !quoted;
        } else if !quoted && tail[0] == b';' {
            return Ok((value, cursor + 1));
        }
        value.push(tail[0]);
        cursor += 1;
    }
    Err(SetupToolError::AptConfiguration {
        reason: "unterminated apt directive".into(),
    })
}

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
        "System apt configuration could not be validated: {reason}; repair its files or permissions and retry setup"
    )]
    AptConfiguration { reason: String },
}

pub(crate) fn validate_apt_configuration() -> Result<(), SetupToolError> {
    // A private root-owned apt fragment may require an elevated read.
    SetupTool::Cat.resolve()?;
    let policy = TrustedToolPolicy::new("/", [], 0);
    validate_apt_configuration_at(Path::new("/etc/apt"), &policy)
}

fn read_apt_configuration(path: &Path) -> Result<Vec<u8>, SetupToolError> {
    match fs::read(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let mut command = apt_cat_command(path)?.build();
            let output = crate::process_output::run_bounded(
                &mut command,
                crate::process_output::CapturePolicy {
                    stdout_max: APT_CONFIG_CAPTURE_LIMIT,
                    stderr_max: 4096,
                    completion: crate::process_output::CompletionPolicy::Deadline(
                        APT_CONFIG_READ_DEADLINE,
                    ),
                },
            )
            .map_err(|error| SetupToolError::AptConfiguration {
                reason: format!("elevated configuration read failed: {error}"),
            })?;
            if !output.status.success() {
                return Err(SetupToolError::AptConfiguration {
                    reason: format!("elevated configuration read exited with {}", output.status),
                });
            }
            Ok(output.stdout)
        }
        Err(error) => Err(SetupToolError::AptConfiguration {
            reason: error.to_string(),
        }),
    }
}

fn apt_cat_command(path: &Path) -> Result<Cmd, SetupToolError> {
    SetupTool::Cat
        .elevated_with_deadline(APT_CONFIG_READ_DEADLINE)
        .map(|command| command.arg(path.as_os_str()))
        .map_err(|error| SetupToolError::AptConfiguration {
            reason: error.to_string(),
        })
}

fn validate_apt_configuration_at(
    root: &Path,
    policy: &TrustedToolPolicy,
) -> Result<(), SetupToolError> {
    let (root, _) = crate::host_tool::validate_trusted_path(root, policy)
        .map_err(|reason| SetupToolError::AptConfiguration { reason })?;
    let mut pending = vec![(root.clone(), false)];
    let mut visited = HashSet::new();
    let mut entries = 0usize;
    while let Some((path, scan_config)) = pending.pop() {
        record_apt_configuration_entry(&mut entries)?;
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
            let contents = read_apt_configuration(&canonical)?;
            for included in apt_include_paths(&contents)? {
                pending.push((included, true));
            }
        }
    }
    Ok(())
}

fn record_apt_configuration_entry(entries: &mut usize) -> Result<(), SetupToolError> {
    *entries += 1;
    if *entries > APT_CONFIG_ENTRY_LIMIT {
        return Err(SetupToolError::AptConfiguration {
            reason: format!("configuration tree exceeds {APT_CONFIG_ENTRY_LIMIT} entries"),
        });
    }
    Ok(())
}

fn apt_include_paths(contents: &[u8]) -> Result<Vec<PathBuf>, SetupToolError> {
    let mut includes = Vec::new();
    let mut offset = 0;
    let mut block_comment = false;
    let mut quoted = false;
    while offset < contents.len() {
        let tail = &contents[offset..];
        if block_comment {
            if tail.starts_with(b"*/") {
                block_comment = false;
                offset += 2;
            } else {
                offset += 1;
            }
            continue;
        }
        if quoted {
            if tail.starts_with(b"\"") {
                quoted = false;
            }
            offset += apt_quoted_advance(tail);
            continue;
        }
        if tail.starts_with(b"//") {
            offset += apt_first_newline(tail).unwrap_or(tail.len());
            if offset == contents.len() {
                break;
            }
            offset += 1;
            continue;
        }
        if tail.starts_with(b"/*") {
            block_comment = true;
            offset += 2;
            continue;
        }
        if tail.starts_with(b"\"") {
            quoted = true;
            offset += 1;
            continue;
        }
        let previous_offset = offset;
        for (directive, is_include) in [(b"#clear".as_slice(), false), (b"#include", true)] {
            if !tail.starts_with(directive) {
                continue;
            }
            if !apt_directive_has_separator(&tail[directive.len()..]) {
                if let Ok((_, consumed)) = apt_directive_body(&tail[directive.len()..]) {
                    offset += directive.len() + consumed;
                    break;
                }
                continue;
            }
            let (body, consumed) = apt_directive_body(&tail[directive.len()..])?;
            if is_include {
                includes.push(apt_include_path(&body)?);
            }
            offset += directive.len() + consumed;
            break;
        }
        if offset != previous_offset {
            continue;
        }
        if tail.starts_with(b"#") {
            offset += apt_first_newline(tail).unwrap_or(tail.len());
            if offset == contents.len() {
                break;
            }
            offset += 1;
        } else {
            offset += 1;
        }
    }
    Ok(includes)
}

fn apt_include_path(value_bytes: &[u8]) -> Result<PathBuf, SetupToolError> {
    let value = std::str::from_utf8(value_bytes)
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
    if included
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(SetupToolError::AptConfiguration {
            reason: "apt include path contains a parent component".into(),
        });
    }
    Ok(if included.is_absolute() {
        included.to_path_buf()
    } else {
        Path::new("/").join(included)
    })
}

#[cfg(kani)]
mod proofs {
    /// A quoted escape always advances, and never moves past the remaining
    /// input when a backslash is the final byte.
    #[kani::proof]
    fn apt_quoted_advance_is_in_bounds() {
        let bytes: [u8; 4] = kani::any();
        let length: usize = kani::any();
        kani::assume((1..=bytes.len()).contains(&length));
        let step = super::apt_quoted_advance(&bytes[..length]);
        assert!(step >= 1 && step <= length);
        assert_eq!(step == 2, bytes[0] == b'\\' && length > 1);
    }

    /// For a `/*` opener, the first non-overlapping `*/` pair is returned.
    #[kani::proof]
    #[kani::unwind(12)]
    fn apt_block_comment_end_is_first_and_in_bounds() {
        let bytes: [u8; 8] = kani::any();
        let length: usize = kani::any();
        kani::assume((2..=bytes.len()).contains(&length));
        kani::assume(bytes[0] == b'/' && bytes[1] == b'*');
        let input = &bytes[..length];
        match super::apt_block_comment_end(input) {
            Some(end) => {
                assert!(end >= 4 && end <= input.len());
                assert_eq!(&input[end - 2..end], b"*/");
                for index in 3..end - 1 {
                    assert!(input[index - 1] != b'*' || input[index] != b'/');
                }
            }
            None => {
                for index in 3..input.len() {
                    assert!(input[index - 1] != b'*' || input[index] != b'/');
                }
            }
        }
    }

    /// Newline detection returns the first in-bounds newline, or None only
    /// when the bounded input contains no newline.
    #[kani::proof]
    #[kani::unwind(12)]
    fn apt_first_newline_is_exact() {
        let bytes: [u8; 8] = kani::any();
        let length: usize = kani::any();
        kani::assume(length <= bytes.len());
        let input = &bytes[..length];
        match super::apt_first_newline(input) {
            Some(found) => {
                assert!(found < input.len());
                assert_eq!(input[found], b'\n');
                for index in 0..found {
                    assert_ne!(input[index], b'\n');
                }
            }
            None => {
                for byte in input {
                    assert_ne!(*byte, b'\n');
                }
            }
        }
    }

    /// A newline within a block comment separates `#include` from its body;
    /// a line comment separates it only when terminated by a newline.
    #[kani::proof]
    #[kani::unwind(12)]
    fn apt_directive_separator_handles_comments() {
        let comment_byte: u8 = kani::any();
        kani::assume(!comment_byte.is_ascii_whitespace());
        let newline: bool = kani::any();
        let trailing_space: bool = kani::any();
        let block = [
            b'/',
            b'*',
            if newline { b'\n' } else { comment_byte },
            b'*',
            b'/',
            if trailing_space { b' ' } else { b'x' },
        ];
        assert_eq!(
            super::apt_directive_has_separator(&block),
            newline || trailing_space
        );
        let line = [
            b'/',
            b'/',
            comment_byte,
            if newline { b'\n' } else { b'x' },
            b'x',
        ];
        assert_eq!(super::apt_directive_has_separator(&line), newline);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetupTool {
    Sudo,
    Curl,
    Tar,
    Gzip,
    Cat,
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
        all(target_os = "macos", not(test)),
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
            Self::Cat => "cat",
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
        match self {
            Self::Sudo => policy.with_allowed_target_beneath_anchor("usr/lib/cargo/bin/sudo"),
            // Ubuntu's coreutils layout can use these exact targets behind
            // the conventional /usr/bin links. The resolver still checks every
            // traversed component for root ownership and writability.
            Self::Cat
            | Self::Chmod
            | Self::Rm
            | Self::Mkdir
            | Self::Tee
            | Self::Chroot
            | Self::Rmdir
            | Self::Id
            | Self::Timeout => policy.with_allowed_target_beneath_anchor(&format!(
                "usr/lib/cargo/bin/coreutils/{}",
                self.name()
            )),
            _ => policy,
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
    fn apt_configuration_limits_preserve_the_exact_boundaries() {
        assert_eq!(APT_CONFIG_CAPTURE_LIMIT, 16 * 1024 * 1024);

        let mut entries = APT_CONFIG_ENTRY_LIMIT - 1;
        assert!(record_apt_configuration_entry(&mut entries).is_ok());
        assert_eq!(entries, APT_CONFIG_ENTRY_LIMIT);

        let result = record_apt_configuration_entry(&mut entries);
        assert_eq!(entries, APT_CONFIG_ENTRY_LIMIT + 1);
        match result {
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("configuration tree exceeds 4096 entries")
            ),
            Ok(()) => assert!(
                entries <= APT_CONFIG_ENTRY_LIMIT,
                "entry beyond the limit should fail"
            ),
        }
    }

    #[test]
    fn apt_byte_scans_preserve_short_input_boundaries() {
        assert_eq!(apt_block_comment_end(b"/*"), None);
        assert_eq!(apt_block_comment_end(b"/*/"), None);
        assert_eq!(apt_block_comment_end(b"/*x*"), None);
        assert_eq!(apt_block_comment_end(b"/*x*/"), Some(5));
        assert_eq!(apt_block_comment_end(b"/*/*/"), Some(5));
        assert_eq!(apt_first_newline(b""), None);
        assert_eq!(apt_first_newline(b"x"), None);
        assert_eq!(apt_first_newline(b"\n"), Some(0));
        assert_eq!(apt_first_newline(b"x\n\n"), Some(1));
        assert_eq!(apt_quoted_advance(b"\\"), 1);
        assert_eq!(apt_quoted_advance(b"\\x"), 2);
    }

    #[test]
    fn apt_directive_parser_preserves_boundaries_and_bytes() {
        assert!(!apt_directive_has_separator(b""));
        assert!(apt_directive_has_separator(b" "));
        assert!(!apt_directive_has_separator(b"//x\n/*"));
        assert!(matches!(
            apt_directive_body(b""),
            Err(SetupToolError::AptConfiguration { .. })
        ));
        assert_eq!(
            apt_directive_body(b"\"\\x\";").unwrap(),
            (b"\"\\x\"".to_vec(), 5)
        );
        assert_eq!(
            apt_directive_body(b"//x\n/a;").unwrap(),
            (b" /a".to_vec(), 7)
        );
    }

    #[test]
    fn apt_include_scanner_handles_short_inputs() {
        assert!(apt_include_paths(b"").unwrap().is_empty());
        assert!(apt_include_paths(b"/**/").unwrap().is_empty());
        assert!(apt_include_paths(b"\"\\").unwrap().is_empty());
    }

    #[test]
    fn apt_include_path_rejects_empty_and_accepts_absolute() {
        assert!(apt_include_path(b"").is_err());
        assert_eq!(apt_include_path(b"/a").unwrap(), PathBuf::from("/a"));
    }

    #[test]
    fn apt_include_scanner_resumes_only_after_closed_comments() {
        assert_eq!(
            apt_include_paths(b"// \"\n#include \"/a\";").unwrap(),
            vec![PathBuf::from("/a")]
        );
        assert_eq!(
            apt_include_paths(b"# #include \"/ignored\";\n#include \"/a\";").unwrap(),
            vec![PathBuf::from("/a")]
        );
        assert_eq!(
            apt_include_paths(b"#include/*/*/ \"/a\";").unwrap(),
            vec![PathBuf::from("/a")]
        );
        assert!(
            apt_include_paths(b"/*/#include \"/a\";")
                .unwrap()
                .is_empty()
        );
    }

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
    fn apt_include_parent_component_cannot_hide_a_symlink_hop() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let root = fixture.path().join("apt");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let trusted = fixture.path().join("trusted");
        fs::create_dir(&trusted).unwrap();
        fs::set_permissions(&trusted, fs::Permissions::from_mode(0o755)).unwrap();
        let external = fixture.path().join("external");
        fs::create_dir(&external).unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o777)).unwrap();
        let nested = external.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(
            external.join("hook.conf"),
            "DPkg::Pre-Invoke { \"true\"; };",
        )
        .unwrap();
        let decoy = trusted.join("hook.conf");
        fs::write(&decoy, "").unwrap();
        fs::set_permissions(&decoy, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&nested, trusted.join("link")).unwrap();
        let config = root.join("apt.conf");
        fs::write(
            &config,
            format!("#include \"{}/link/../hook.conf\";", trusted.display()),
        )
        .unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let policy = TrustedToolPolicy::new(fixture.path(), [], uid);
        let error = validate_apt_configuration_at(&root, &policy).unwrap_err();
        assert!(error.to_string().contains("parent component"), "{error}");
    }

    #[test]
    fn apt_root_symlink_still_scans_its_configuration_files() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let root = fixture.path().join("apt");
        let real_root = fixture.path().join("real-apt");
        fs::create_dir(&real_root).unwrap();
        fs::set_permissions(&real_root, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&real_root, &root).unwrap();
        let config = real_root.join("apt.conf");
        let trusted = fixture.path().join("trusted.conf");
        fs::write(&trusted, "").unwrap();
        fs::set_permissions(&trusted, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&config, format!("#include \"{}\";", trusted.display())).unwrap();
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
        let error = validate_apt_configuration_at(&root, &policy)
            .unwrap_err()
            .to_string();
        assert!(error.contains("writable"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_root_owned_apt_fragment_is_read_through_trusted_cat() {
        if !std::process::Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/true"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        let fixture = crate::host_tool::trusted_test_tempdir();
        let root = fixture.path().join("apt");
        let parts = root.join("apt.conf.d");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parts).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&parts, fs::Permissions::from_mode(0o755)).unwrap();
        let included = fixture.path().join("included.conf");
        fs::write(&included, "").unwrap();
        fs::set_permissions(&included, fs::Permissions::from_mode(0o600)).unwrap();
        let config = parts.join("private");
        fs::write(&config, format!("#include \"{}\";", included.display())).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/sudo")
                .args(["-n", "/usr/bin/chown", "0:0"])
                .arg(&config)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            fs::read(&config).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let uid = unsafe { libc::geteuid() }; // SAFETY: geteuid has no preconditions.
        let policy = TrustedToolPolicy::new_with_owners(fixture.path(), [], [0, uid]);
        validate_apt_configuration_at(&root, &policy).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_apt_read_bounds_its_privileged_child() {
        let config = Path::new("/etc/apt/apt.conf");
        let command = apt_cat_command(config).unwrap().build();
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args[0], "--");
        assert_eq!(args[2], ROOT_TIMEOUT_GRACE);
        assert_eq!(args[3], "60s");
        assert_eq!(args[5], config.to_str().unwrap());
        assert!(args[1].starts_with('/'));
        assert!(args[4].starts_with('/'));
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

        let missing = fixture.path().join("missing.conf");
        fs::write(
            &config,
            format!(
                "// #include \"{}\";\n/* #include \"{}\"; */\n# comment #include \"{}\";\n#include\"{}\";\n#include/*x*/\"{}\";\nAPT::Note \"#include {} ;\";\n#clear /* ; #include \"{}\"; */ Acquire;\n#clear \"Acquire; #include {} ;\";\n#include/*x*/ \"{}\";\n#include /*x*/ \"{}\";\n#include//x\n \"{}\";",
                missing.display(),
                missing.display(),
                missing.display(),
                missing.display(),
                missing.display(),
                missing.display(),
                missing.display(),
                missing.display(),
                external.display(),
                external.display(),
                external.display()
            ),
        )
        .unwrap();
        validate_apt_configuration_at(&root, &policy).unwrap();

        let writable = fixture.path().join("writable");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let untrusted = writable.join("hook.conf");
        fs::write(&untrusted, "DPkg::Pre-Invoke { \"true\"; };").unwrap();
        for source in [
            format!("#clear Acquire; #include \"{}\";", untrusted.display()),
            format!("#clear/*x*/ Acquire; #include \"{}\";", untrusted.display()),
            format!("#clear//x\n Acquire; #include \"{}\";", untrusted.display()),
            format!("#include/*x*/ \"{}\";", untrusted.display()),
            format!("#include/*\n*/\"{}\";", untrusted.display()),
            format!("#include \"{}\";", untrusted.display()),
            format!("#include\"ignored\"; #include \"{}\";", untrusted.display()),
            format!(
                "#include/*x*/\"ignored\"; #include \"{}\";",
                untrusted.display()
            ),
            format!("#clear\"A\"; #include \"{}\";", untrusted.display()),
        ] {
            fs::write(&config, source).unwrap();
            let error = validate_apt_configuration_at(&root, &policy)
                .unwrap_err()
                .to_string();
            assert!(error.contains("writable"), "{error}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_apt_configuration_passes_the_supported_host_policy() {
        if SetupTool::AptGet.resolve_if_present().unwrap().is_some() {
            validate_apt_configuration().unwrap();
        }
    }

    #[test]
    fn non_fhs_targets_are_limited_to_their_setup_roles() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let anchor = fixture.path();
        let bin = anchor.join("usr/bin");
        let cargo_bin = anchor.join("usr/lib/cargo/bin");
        let coreutils_bin = cargo_bin.join("coreutils");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&coreutils_bin).unwrap();
        for directory in [
            anchor.join("usr"),
            bin.clone(),
            anchor.join("usr/lib"),
            anchor.join("usr/lib/cargo"),
            cargo_bin.clone(),
            coreutils_bin.clone(),
        ] {
            let mut permissions = fs::metadata(&directory).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&directory, permissions).unwrap();
        }
        let target = cargo_bin.join("sudo");
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
        for tool in [
            SetupTool::Cat,
            SetupTool::Chmod,
            SetupTool::Rm,
            SetupTool::Mkdir,
            SetupTool::Tee,
            SetupTool::Chroot,
            SetupTool::Rmdir,
            SetupTool::Id,
            SetupTool::Timeout,
        ] {
            let target = coreutils_bin.join(tool.name());
            fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
            let candidate = bin.join(tool.name());
            symlink(&target, &candidate).unwrap();
            let resolved = tool
                .resolve_required_in(
                    std::slice::from_ref(&candidate),
                    &tool.policy_at(anchor, uid),
                )
                .unwrap();
            assert_eq!(resolved.canonical_target(), target);
            assert_eq!(resolved.launch_path(), candidate);
        }
        let substituted = bin.join("cat");
        fs::remove_file(&substituted).unwrap();
        symlink(coreutils_bin.join("rm"), &substituted).unwrap();
        assert!(matches!(
            SetupTool::Cat.resolve_required_in(
                std::slice::from_ref(&substituted),
                &SetupTool::Cat.policy_at(anchor, uid)
            ),
            Err(SetupToolError::Rejected { .. })
        ));
        assert!(matches!(
            SetupTool::Curl.resolve_required_in(
                std::slice::from_ref(&curl),
                &SetupTool::Curl.policy_at(anchor, uid)
            ),
            Err(SetupToolError::Rejected { .. })
        ));
        let wrong_target = coreutils_bin.join("tar");
        fs::write(&wrong_target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&wrong_target, fs::Permissions::from_mode(0o755)).unwrap();
        let tar = bin.join("tar");
        symlink(&wrong_target, &tar).unwrap();
        assert!(matches!(
            SetupTool::Tar.resolve_required_in(
                std::slice::from_ref(&tar),
                &SetupTool::Tar.policy_at(anchor, uid)
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
