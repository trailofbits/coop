use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, Metadata};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::cmd::Cmd;

const MAX_SYMLINKS: usize = 40;

/// Policy for one family of trusted host executables.
///
/// `trust_anchor` is the highest directory whose ownership this policy proves.
/// Production policies anchor at `/`; tests may anchor a private fixture tree.
pub(crate) struct TrustedToolPolicy {
    trust_anchor: PathBuf,
    allowed_roots: Vec<PathBuf>,
    expected_uid: u32,
}

impl TrustedToolPolicy {
    #[cfg_attr(
        not(any(target_os = "linux", test)),
        expect(
            dead_code,
            reason = "the first production policy is Linux-only; later families can reuse this constructor"
        )
    )]
    pub(crate) fn new(
        trust_anchor: impl Into<PathBuf>,
        allowed_roots: impl IntoIterator<Item = PathBuf>,
        expected_uid: u32,
    ) -> Self {
        Self {
            trust_anchor: trust_anchor.into(),
            allowed_roots: allowed_roots.into_iter().collect(),
            expected_uid,
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures and assertions")]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum TestTool {
        Sudo,
        Ip,
    }

    impl fmt::Display for TestTool {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                Self::Sudo => "sudo",
                Self::Ip => "ip",
            })
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        anchor: PathBuf,
        bin: PathBuf,
        policy: TrustedToolPolicy,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let anchor = temp.path().join("root");
            let bin = anchor.join("usr/bin");
            fs::create_dir_all(&bin).unwrap();
            let uid = fs::metadata(&anchor).unwrap().uid();
            let policy = TrustedToolPolicy::new(&anchor, [bin.clone()], uid);
            Self {
                _temp: temp,
                anchor,
                bin,
                policy,
            }
        }

        fn executable(&self, name: &str, body: &str) -> PathBuf {
            let path = self.bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    #[test]
    fn resolves_only_an_absolute_allowed_executable() {
        let fixture = Fixture::new();
        let candidate = fixture.executable("ip", "#!/bin/sh\nexit 0\n");
        let resolved = resolve_host_tool(
            TestTool::Ip,
            std::slice::from_ref(&candidate),
            &fixture.policy,
        )
        .unwrap();
        assert_eq!(resolved.launch_path(), candidate);
        assert_eq!(resolved.canonical_target(), candidate);

        let relative =
            resolve_host_tool(TestTool::Ip, &[PathBuf::from("ip")], &fixture.policy).unwrap_err();
        assert!(relative.to_string().contains("candidate is not absolute"));
    }

    #[test]
    fn missing_candidate_reports_tool_and_reason_without_path_fallback() {
        let fixture = Fixture::new();
        let missing = fixture.bin.join("ip");
        let second = fixture.bin.join("ip-second");
        let error = resolve_host_tool(
            TestTool::Ip,
            &[missing.clone(), second.clone()],
            &fixture.policy,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with(&format!(
            "could not resolve trusted host tool 'ip': {} (not found:",
            missing.display()
        )));
        assert!(message.contains(&format!("); {} (not found:", second.display())));
    }

    #[test]
    fn hostile_path_is_not_considered_or_executed() {
        const CHILD: &str = "COOP_HOST_TOOL_PATH_CHILD";
        const ANCHOR: &str = "COOP_HOST_TOOL_PATH_ANCHOR";
        if std::env::var_os(CHILD).is_some() {
            let anchor = PathBuf::from(std::env::var_os(ANCHOR).unwrap());
            let bin = anchor.join("usr/bin");
            let policy = TrustedToolPolicy::new(
                &anchor,
                [bin.clone()],
                fs::metadata(&anchor).unwrap().uid(),
            );
            let error = resolve_host_tool(TestTool::Ip, &[bin.join("ip")], &policy).unwrap_err();
            assert!(error.to_string().contains("not found"));
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().join("root");
        fs::create_dir_all(anchor.join("usr/bin")).unwrap();
        let hostile = temp.path().join("hostile");
        fs::create_dir(&hostile).unwrap();
        let marker = temp.path().join("executed");
        let hostile_ip = hostile.join("ip");
        fs::write(
            &hostile_ip,
            format!("#!/bin/sh\n: > '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&hostile_ip, fs::Permissions::from_mode(0o755)).unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("host_tool::tests::hostile_path_is_not_considered_or_executed")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env(ANCHOR, &anchor)
            .env("PATH", &hostile)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!marker.exists());
    }

    #[test]
    fn rejects_group_or_other_writable_executable() {
        let fixture = Fixture::new();
        let candidate = fixture.executable("ip", "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o775)).unwrap();
        let error = resolve_host_tool(TestTool::Ip, &[candidate], &fixture.policy).unwrap_err();
        assert!(error.to_string().contains("writable by group or other"));
    }

    #[test]
    fn rejects_wrong_owner_non_regular_and_non_executable_targets() {
        let fixture = Fixture::new();
        let candidate = fixture.executable("ip", "#!/bin/sh\nexit 0\n");
        let uid = fs::metadata(&candidate).unwrap().uid();
        let wrong_owner_policy = TrustedToolPolicy::new(
            &fixture.anchor,
            [fixture.bin.clone()],
            uid.saturating_add(1),
        );
        let wrong_owner = resolve_host_tool(
            TestTool::Ip,
            std::slice::from_ref(&candidate),
            &wrong_owner_policy,
        )
        .unwrap_err();
        assert!(wrong_owner.to_string().contains("trust anchor has uid"));

        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644)).unwrap();
        let non_executable =
            resolve_host_tool(TestTool::Ip, &[candidate], &fixture.policy).unwrap_err();
        assert!(non_executable.to_string().contains("not executable"));

        let directory = fixture.bin.join("directory");
        fs::create_dir(&directory).unwrap();
        let non_regular =
            resolve_host_tool(TestTool::Ip, &[directory], &fixture.policy).unwrap_err();
        assert!(non_regular.to_string().contains("not a regular file"));
    }

    #[test]
    fn accepts_merged_usr_symlink_when_every_hop_is_trusted() {
        let fixture = Fixture::new();
        let sbin_target = fixture.anchor.join("usr/sbin");
        fs::create_dir_all(&sbin_target).unwrap();
        let target = sbin_target.join("ip");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("usr/sbin", fixture.anchor.join("sbin")).unwrap();
        let policy = TrustedToolPolicy::new(
            &fixture.anchor,
            [fixture.anchor.join("sbin")],
            fs::metadata(&fixture.anchor).unwrap().uid(),
        );
        let candidate = fixture.anchor.join("sbin/ip");
        let resolved =
            resolve_host_tool(TestTool::Ip, std::slice::from_ref(&candidate), &policy).unwrap();
        assert_eq!(resolved.launch_path(), candidate);
        assert_eq!(resolved.canonical_target(), target);
    }

    #[test]
    fn rejects_symlink_escape_and_writable_controlling_directory() {
        let fixture = Fixture::new();
        let escape = fixture.bin.join("escape");
        symlink("/bin/sh", &escape).unwrap();
        let error = resolve_host_tool(TestTool::Ip, &[escape], &fixture.policy).unwrap_err();
        assert!(error.to_string().contains("escapes trust anchor"));

        let target = fixture.executable("real-ip", "#!/bin/sh\nexit 0\n");
        let writable = fixture.anchor.join("writable");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let candidate = writable.join("ip");
        symlink(&target, &candidate).unwrap();
        let policy = TrustedToolPolicy::new(
            &fixture.anchor,
            [fixture.bin.clone()],
            fs::metadata(&fixture.anchor).unwrap().uid(),
        );
        let error = resolve_host_tool(TestTool::Ip, &[candidate], &policy).unwrap_err();
        assert!(error.to_string().contains("path directory is writable"));
    }

    #[test]
    fn explicit_path_accepts_a_trusted_target_outside_builtin_roots() {
        let fixture = Fixture::new();
        let store = fixture.anchor.join("nix/store/package/bin");
        fs::create_dir_all(&store).unwrap();
        let target = store.join("ip");
        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let candidate = fixture.bin.join("ip");
        symlink(&target, &candidate).unwrap();

        let builtin = resolve_host_tool(
            TestTool::Ip,
            std::slice::from_ref(&candidate),
            &fixture.policy,
        )
        .unwrap_err();
        assert!(builtin.to_string().contains("outside the allowed roots"));

        let explicit = resolve_exact_host_tool(TestTool::Ip, &candidate, &fixture.policy).unwrap();
        assert_eq!(explicit.launch_path(), candidate);
        assert_eq!(explicit.canonical_target(), target);
    }

    #[test]
    fn symlink_parent_components_follow_kernel_resolution_order() {
        let fixture = Fixture::new();
        let a = fixture.anchor.join("a");
        let outside = fixture.anchor.join("outside");
        let deep = outside.join("deep");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&deep).unwrap();

        // Lexically collapsing `link/..` would inspect a/tool. The kernel
        // follows link first, then applies `..`, and therefore executes
        // outside/tool instead.
        let decoy = a.join("tool");
        fs::write(&decoy, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).unwrap();
        let actual = outside.join("tool");
        fs::write(&actual, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&actual, fs::Permissions::from_mode(0o777)).unwrap();
        symlink("../outside/deep", a.join("link")).unwrap();
        let candidate = fixture.bin.join("ip");
        symlink("../../a/link/../tool", &candidate).unwrap();

        let error = resolve_exact_host_tool(TestTool::Ip, &candidate, &fixture.policy).unwrap_err();
        assert!(
            error.to_string().contains("canonical target is writable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn symlink_parent_component_cannot_climb_above_trust_anchor() {
        let fixture = Fixture::new();
        let candidate = fixture.bin.join("ip");
        symlink("../../../bin/sh", &candidate).unwrap();
        let error = resolve_exact_host_tool(TestTool::Ip, &candidate, &fixture.policy).unwrap_err();
        assert!(error.to_string().contains("escapes trust anchor"));
    }

    #[test]
    fn exact_path_api_rejects_parent_components() {
        let fixture = Fixture::new();
        let candidate = fixture.bin.join("../bin/ip");
        let error = resolve_exact_host_tool(TestTool::Ip, &candidate, &fixture.policy).unwrap_err();
        assert!(error.to_string().contains("parent-directory component"));
    }

    #[test]
    fn rejects_unreadable_candidates_and_non_directory_allowed_roots() {
        let fixture = Fixture::new();
        let locked = fixture.anchor.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable =
            resolve_host_tool(TestTool::Ip, &[locked.join("ip")], &fixture.policy).unwrap_err();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(unreadable.to_string().contains("cannot inspect candidate"));

        let candidate = fixture.executable("ip", "#!/bin/sh\nexit 0\n");
        let root_file = fixture.anchor.join("allowed-root-file");
        fs::write(&root_file, "not a directory").unwrap();
        fs::set_permissions(&root_file, fs::Permissions::from_mode(0o755)).unwrap();
        let policy = TrustedToolPolicy::new(
            &fixture.anchor,
            [root_file],
            fs::metadata(&fixture.anchor).unwrap().uid(),
        );
        let error = resolve_host_tool(TestTool::Ip, &[candidate], &policy).unwrap_err();
        assert!(error.to_string().contains("is not a directory"));
    }

    #[test]
    fn symlink_limit_accepts_the_boundary_and_rejects_one_more() {
        fn make_chain(fixture: &Fixture, prefix: &str, count: usize) -> PathBuf {
            let target = fixture.executable(&format!("{prefix}-target"), "#!/bin/sh\nexit 0\n");
            for index in (0..count).rev() {
                let link = fixture.bin.join(format!("{prefix}-{index}"));
                let next = if index + 1 == count {
                    target.clone()
                } else {
                    fixture.bin.join(format!("{prefix}-{}", index + 1))
                };
                symlink(next, link).unwrap();
            }
            fixture.bin.join(format!("{prefix}-0"))
        }

        let fixture = Fixture::new();
        let accepted = make_chain(&fixture, "accepted", MAX_SYMLINKS);
        resolve_host_tool(TestTool::Ip, &[accepted], &fixture.policy).unwrap();

        let rejected = make_chain(&fixture, "rejected", MAX_SYMLINKS + 1);
        let error = resolve_host_tool(TestTool::Ip, &[rejected], &fixture.policy).unwrap_err();
        assert!(error.to_string().contains("more than 40 symbolic links"));
    }

    #[test]
    fn absolute_normalization_rejects_traversal_above_root() {
        assert_eq!(
            normalize_absolute(Path::new("/usr/../bin")).unwrap(),
            Path::new("/bin")
        );
        assert!(normalize_absolute(Path::new("/..")).is_err());
    }

    #[test]
    fn direct_launch_uses_selected_path_clean_environment_and_root_cwd() {
        let fixture = Fixture::new();
        let candidate = fixture.executable(
            "ip",
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n%s\\n' \"$PWD\" \"$LANG\" \"$LC_ALL\" \"${HOME-unset}\"\n",
        );
        let resolved = resolve_host_tool(
            TestTool::Ip,
            std::slice::from_ref(&candidate),
            &fixture.policy,
        )
        .unwrap();
        let context = TrustedLaunchContext::system();
        let built = context.command(&resolved).build();
        assert_eq!(built.get_program(), candidate.as_os_str());
        assert_eq!(built.get_current_dir(), Some(Path::new("/")));
        assert_eq!(
            built
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
                .collect::<Vec<_>>(),
            [
                (OsString::from("LANG"), Some(OsString::from("C"))),
                (OsString::from("LC_ALL"), Some(OsString::from("C"))),
            ]
        );
        let output = context.command(&resolved).capture().unwrap();
        assert_eq!(output, "/\nC\nC\nunset\n");
    }

    #[test]
    fn elevated_launch_uses_resolved_sudo_and_selected_target() {
        let fixture = Fixture::new();
        let sudo_path = fixture.executable("sudo", "#!/bin/sh\nexit 0\n");
        let ip_path = fixture.executable("ip", "#!/bin/sh\nexit 0\n");
        let sudo = resolve_host_tool(
            TestTool::Sudo,
            std::slice::from_ref(&sudo_path),
            &fixture.policy,
        )
        .unwrap();
        let ip = resolve_host_tool(
            TestTool::Ip,
            std::slice::from_ref(&ip_path),
            &fixture.policy,
        )
        .unwrap();
        let command = TrustedLaunchContext::system().elevated(&sudo, &ip).build();
        assert_eq!(command.get_program(), sudo_path.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("--"), ip_path.as_os_str()]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    }
}

/// An executable path that has passed a family-specific trust policy.
///
/// The fields and constructor are private so a raw path cannot masquerade as a
/// resolved identity at a launch boundary. The selected path is retained for
/// launch because alternatives and multi-call binaries may depend on argv[0];
/// `canonical_target` records the object that was validated.
#[derive(Debug)]
pub(crate) struct ResolvedHostTool<I> {
    identity: I,
    launch_path: PathBuf,
    canonical_target: PathBuf,
}

impl<I> ResolvedHostTool<I> {
    #[cfg(test)]
    pub(crate) fn launch_path(&self) -> &Path {
        &self.launch_path
    }

    #[cfg(test)]
    pub(crate) fn canonical_target(&self) -> &Path {
        &self.canonical_target
    }
}

/// Failure to establish a trusted executable identity from an explicit list.
#[derive(Debug, Error)]
#[error("could not resolve trusted host tool '{tool}': {attempts}")]
pub(crate) struct ResolveHostToolError {
    tool: String,
    attempts: AttemptFailures,
}

#[derive(Debug)]
struct AttemptFailures(Vec<CandidateFailure>);

impl fmt::Display for AttemptFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("policy supplied no candidates");
        }
        for (index, failure) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str("; ")?;
            }
            write!(f, "{} ({})", failure.path.display(), failure.reason)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct CandidateFailure {
    path: PathBuf,
    reason: String,
}

pub(crate) fn resolve_host_tool<I: Copy + fmt::Display>(
    identity: I,
    candidates: &[PathBuf],
    policy: &TrustedToolPolicy,
) -> Result<ResolvedHostTool<I>, ResolveHostToolError> {
    let mut failures = Vec::new();
    for candidate in candidates {
        if !candidate.is_absolute() {
            return Err(ResolveHostToolError {
                tool: identity.to_string(),
                attempts: AttemptFailures(vec![CandidateFailure {
                    path: candidate.clone(),
                    reason: "candidate is not absolute".to_string(),
                }]),
            });
        }
        match fs::symlink_metadata(candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                failures.push(CandidateFailure {
                    path: candidate.clone(),
                    reason: format!("not found: {error}"),
                });
                continue;
            }
            Err(error) => {
                return Err(ResolveHostToolError {
                    tool: identity.to_string(),
                    attempts: AttemptFailures(vec![CandidateFailure {
                        path: candidate.clone(),
                        reason: format!("cannot inspect candidate: {error}"),
                    }]),
                });
            }
            Ok(_) => {}
        }
        match validate_builtin_candidate(candidate, policy) {
            Ok(canonical_target) => {
                return Ok(ResolvedHostTool {
                    identity,
                    launch_path: candidate.clone(),
                    canonical_target,
                });
            }
            Err(reason) => {
                return Err(ResolveHostToolError {
                    tool: identity.to_string(),
                    attempts: AttemptFailures(vec![CandidateFailure {
                        path: candidate.clone(),
                        reason,
                    }]),
                });
            }
        }
    }
    Err(ResolveHostToolError {
        tool: identity.to_string(),
        attempts: AttemptFailures(failures),
    })
}

/// Resolve one exact path explicitly selected by trusted host configuration.
///
/// The explicit selection replaces the built-in candidate set, so FHS target
/// containment is not relevant. Full path-chain ownership and mode checks
/// remain mandatory; this API must not be used for ambient discovery.
pub(crate) fn resolve_exact_host_tool<I: Copy + fmt::Display>(
    identity: I,
    candidate: &Path,
    policy: &TrustedToolPolicy,
) -> Result<ResolvedHostTool<I>, ResolveHostToolError> {
    let failure = |reason| ResolveHostToolError {
        tool: identity.to_string(),
        attempts: AttemptFailures(vec![CandidateFailure {
            path: candidate.to_path_buf(),
            reason,
        }]),
    };
    if !candidate.is_absolute() {
        return Err(failure("candidate is not absolute".to_string()));
    }
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(failure(
            "configured path contains a parent-directory component".to_string(),
        ));
    }
    fs::symlink_metadata(candidate)
        .map_err(|error| failure(format!("cannot inspect configured path: {error}")))?;
    let canonical_target =
        validate_candidate(candidate, policy, TargetContainment::Explicit).map_err(failure)?;
    Ok(ResolvedHostTool {
        identity,
        launch_path: candidate.to_path_buf(),
        canonical_target,
    })
}

fn validate_builtin_candidate(
    candidate: &Path,
    policy: &TrustedToolPolicy,
) -> Result<PathBuf, String> {
    validate_candidate(candidate, policy, TargetContainment::BuiltInRoots)
}

#[derive(Clone, Copy)]
enum TargetContainment {
    BuiltInRoots,
    Explicit,
}

fn validate_candidate(
    candidate: &Path,
    policy: &TrustedToolPolicy,
    containment: TargetContainment,
) -> Result<PathBuf, String> {
    if !candidate.is_absolute() {
        return Err("candidate is not absolute".to_string());
    }
    let (canonical_target, metadata) = resolve_and_validate(candidate, policy)?;
    if !metadata.file_type().is_file() {
        return Err("canonical target is not a regular file".to_string());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err("canonical target is not executable".to_string());
    }
    validate_owner_and_mode(&metadata, policy.expected_uid, "canonical target")?;

    if matches!(containment, TargetContainment::Explicit) {
        return Ok(canonical_target);
    }

    let mut allowed = false;
    let mut root_failures = Vec::new();
    for root in &policy.allowed_roots {
        match resolve_and_validate(root, policy) {
            Ok((canonical_root, root_metadata)) if root_metadata.is_dir() => {
                if canonical_target.starts_with(&canonical_root) {
                    allowed = true;
                    break;
                }
            }
            Ok(_) => root_failures.push(format!("{} is not a directory", root.display())),
            Err(reason) => root_failures.push(format!("{}: {reason}", root.display())),
        }
    }
    if !allowed {
        let detail = if root_failures.is_empty() {
            String::new()
        } else {
            format!("; unusable policy roots: {}", root_failures.join(", "))
        };
        return Err(format!(
            "canonical target {} is outside the allowed roots{detail}",
            canonical_target.display()
        ));
    }
    Ok(canonical_target)
}

/// Resolve symlinks one hop at a time while validating every directory that
/// controls a hop. Parent components in symlink targets are processed in
/// kernel order, after preceding components and their symlinks have resolved;
/// lexical normalization here could validate a different object than exec.
fn resolve_and_validate(
    path: &Path,
    policy: &TrustedToolPolicy,
) -> Result<(PathBuf, Metadata), String> {
    let anchor = normalize_absolute(&policy.trust_anchor)?;
    let requested = normalize_absolute(path)?;
    let relative = requested.strip_prefix(&anchor).map_err(|_| {
        format!(
            "path escapes trust anchor {}",
            policy.trust_anchor.display()
        )
    })?;

    let anchor_metadata = fs::symlink_metadata(&anchor)
        .map_err(|error| format!("cannot inspect trust anchor: {error}"))?;
    if !anchor_metadata.is_dir() {
        return Err("trust anchor is not a directory".to_string());
    }
    validate_owner_and_mode(&anchor_metadata, policy.expected_uid, "trust anchor")?;

    let mut current = anchor.clone();
    let mut pending = normal_components(relative)?;
    let mut followed = 0;
    loop {
        let Some(component) = pending.pop_front() else {
            return Ok((current, anchor_metadata));
        };
        if matches!(component, PathStep::Parent) {
            if current == anchor {
                return Err(format!(
                    "symbolic link target escapes trust anchor {}",
                    policy.trust_anchor.display()
                ));
            }
            current.pop();
            continue;
        }
        let PathStep::Normal(component) = component else {
            unreachable!("parent path step handled above")
        };
        let next = current.join(&component);
        let metadata = fs::symlink_metadata(&next)
            .map_err(|error| format!("cannot inspect path component: {error}"))?;

        if metadata.file_type().is_symlink() {
            followed += 1;
            if followed > MAX_SYMLINKS {
                return Err(format!("more than {MAX_SYMLINKS} symbolic links"));
            }
            if metadata.uid() != policy.expected_uid {
                return Err(format!(
                    "symbolic link has uid {}, expected {}",
                    metadata.uid(),
                    policy.expected_uid
                ));
            }
            let target = fs::read_link(&next)
                .map_err(|error| format!("cannot read symbolic link: {error}"))?;
            let mut redirected = if target.is_absolute() {
                let target_relative = target.strip_prefix(&anchor).map_err(|_| {
                    format!(
                        "symbolic link target {} escapes trust anchor",
                        target.display()
                    )
                })?;
                current.clone_from(&anchor);
                symlink_target_components(target_relative)?
            } else {
                symlink_target_components(&target)?
            };
            redirected.append(&mut pending);
            pending = redirected;
            continue;
        }

        current = next;
        if !pending.is_empty() {
            if !metadata.is_dir() {
                return Err("non-directory path component".to_string());
            }
            validate_owner_and_mode(&metadata, policy.expected_uid, "path directory")?;
        }
        if pending.is_empty() {
            return Ok((current, metadata));
        }
    }
}

fn validate_owner_and_mode(
    metadata: &Metadata,
    expected_uid: u32,
    what: &str,
) -> Result<(), String> {
    if metadata.uid() != expected_uid {
        return Err(format!(
            "{what} has uid {}, expected {expected_uid}",
            metadata.uid()
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(format!(
            "{what} is writable by group or other users (mode {:04o})",
            mode & 0o7777
        ));
    }
    Ok(())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("path is not absolute".to_string());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if normalized == Path::new("/") {
                    return Err("path traverses above filesystem root".to_string());
                }
                normalized.pop();
            }
            Component::Prefix(_) => return Err("unsupported path prefix".to_string()),
        }
    }
    Ok(normalized)
}

#[derive(Debug)]
enum PathStep {
    Normal(OsString),
    Parent,
}

fn normal_components(path: &Path) -> Result<VecDeque<PathStep>, String> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => Ok(PathStep::Normal(value.to_owned())),
            _ => Err("path contains a non-normal component".to_string()),
        })
        .collect()
}

fn symlink_target_components(path: &Path) -> Result<VecDeque<PathStep>, String> {
    path.components()
        .filter_map(|component| match component {
            Component::CurDir | Component::RootDir => None,
            Component::ParentDir => Some(Ok(PathStep::Parent)),
            Component::Normal(value) => Some(Ok(PathStep::Normal(value.to_owned()))),
            Component::Prefix(_) => Some(Err("unsupported path prefix".to_string())),
        })
        .collect()
}

/// Explicit context supplied to trusted host tools.
pub(crate) struct TrustedLaunchContext {
    working_directory: PathBuf,
}

impl TrustedLaunchContext {
    pub(crate) fn system() -> Self {
        Self {
            working_directory: PathBuf::from("/"),
        }
    }

    pub(crate) fn command<I>(&self, tool: &ResolvedHostTool<I>) -> Cmd {
        let _ = (&tool.identity, &tool.canonical_target);
        self.base_command(tool.launch_path.as_os_str())
    }

    pub(crate) fn elevated<S, I>(
        &self,
        sudo: &ResolvedHostTool<S>,
        target: &ResolvedHostTool<I>,
    ) -> Cmd {
        let _ = (
            &sudo.identity,
            &sudo.canonical_target,
            &target.identity,
            &target.canonical_target,
        );
        self.base_command(sudo.launch_path.as_os_str())
            .arg("--")
            .arg(&target.launch_path)
    }

    fn base_command(&self, program: &OsStr) -> Cmd {
        Cmd::new(program)
            .env_clear()
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .current_dir(&self.working_directory)
    }
}
