//! Systemd's system manager owns the runtime VMM identity and process group.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cmd::Cmd;
use crate::config::Instance;
use crate::firecracker_install::VerifiedFirecracker;
use crate::setup_tool::SetupTool;
use crate::sha256_hash::Sha256Hash;

const START_DEADLINE: Duration = Duration::from_secs(30);
const UNIT_SUFFIX: &str = ".service";
// Version 252 supports JSON unit listing and the transient service options
// used here. The integration matrix exercises newer Ubuntu managers.
const MIN_SYSTEMD_VERSION: u32 = 252;

#[derive(Debug, Deserialize)]
struct ListedUnit {
    unit: String,
    active: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveService {
    unit: String,
    pid: u32,
    control_group: String,
}

impl ActiveService {
    pub(crate) fn unit(&self) -> &str {
        &self.unit
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceProbe {
    None,
    Active(ActiveService),
    Uncertain(String),
}

fn prefix(inst: &Instance) -> Result<String> {
    if !inst.dir.is_absolute() {
        bail!("Firecracker instance directory must be absolute for service identity");
    }
    let absolute = std::path::absolute(&inst.dir)
        .with_context(|| format!("Cannot resolve instance path {}", inst.dir.display()))?;
    let digest = Sha256Hash::of(absolute.as_os_str().as_bytes()).to_string();
    // The complete digest prevents another same-user instance from sharing an
    // action namespace even if its name or data directory changes.
    // SAFETY: geteuid only reads the process credential.
    Ok(format!("coop-vmm-{}-{digest}-", unsafe { libc::geteuid() }))
}

fn valid_unit(unit: &str, prefix: &str) -> bool {
    let Some(generation) = unit
        .strip_prefix(prefix)
        .and_then(|tail| tail.strip_suffix(UNIT_SUFFIX))
    else {
        return false;
    };
    generation.len() == 32
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn new_unit(inst: &Instance) -> Result<String> {
    let mut generation = [0u8; 16];
    getrandom::fill(&mut generation).context("Cannot create Firecracker service generation")?;
    Ok(format!(
        "{}{}{UNIT_SUFFIX}",
        prefix(inst)?,
        hex::encode(generation)
    ))
}

// systemd expands $NAME and ${NAME} in ExecStart arguments even when
// systemd-run receives them as separate argv entries. $$ preserves a literal
// dollar sign, including in non-UTF-8 path components.
fn escape_service_argument(argument: &OsStr) -> OsString {
    let mut escaped = Vec::with_capacity(argument.as_bytes().len());
    for byte in argument.as_bytes() {
        escaped.push(*byte);
        if *byte == b'$' {
            escaped.push(b'$');
        }
    }
    OsString::from_vec(escaped)
}

fn systemctl_query() -> Result<Cmd> {
    let systemctl = SetupTool::Systemctl.resolve()?;
    Ok(SetupTool::Timeout
        .command()?
        .args(["--kill-after=2s", "5s"])
        .arg(systemctl.launch_path()))
}

fn list_units(inst: &Instance) -> Result<Vec<ListedUnit>> {
    let prefix = prefix(inst)?;
    let output = systemctl_query()?
        .args([
            "--system",
            "--no-pager",
            "--plain",
            "--full",
            "--output=json",
            "list-units",
            "--all",
        ])
        .arg(format!("{prefix}*{UNIT_SUFFIX}"))
        .capture()
        .context(
            "A running systemd system manager is required for Firecracker lifecycle operations",
        )?;
    parse_listed_units(&output, &prefix)
}

fn parse_listed_units(output: &str, prefix: &str) -> Result<Vec<ListedUnit>> {
    let listed: Vec<ListedUnit> =
        serde_json::from_str(output).context("Invalid systemd unit listing")?;
    if listed.iter().any(|entry| !valid_unit(&entry.unit, prefix)) {
        bail!("Systemd returned a Firecracker unit outside the instance namespace");
    }
    Ok(listed)
}

pub(crate) fn require_system_manager() -> Result<()> {
    if !Path::new("/run/systemd/system").is_dir() {
        bail!("A running systemd system manager is required to start Firecracker");
    }
    let version = systemctl_query()?
        .args(["--system", "show", "--property=Version", "--value"])
        .capture()
        .context("A running systemd system manager is required to start Firecracker")?;
    require_supported_version(&version)
}

fn require_supported_version(version: &str) -> Result<()> {
    let number = version
        .trim()
        .split_once(|c: char| !c.is_ascii_digit())
        .map_or(version.trim(), |(number, _)| number)
        .parse::<u32>()
        .context("Cannot read the running systemd manager version")?;
    if number < MIN_SYSTEMD_VERSION {
        bail!(
            "Firecracker requires a running systemd system manager version {MIN_SYSTEMD_VERSION} or newer (found {version}); upgrade the host before starting this VM"
        );
    }
    Ok(())
}

pub(crate) fn probe(inst: &Instance) -> Result<ServiceProbe> {
    // Linux hosts without a system manager can still inspect and stop a
    // pre-existing direct-launch VM. New launches require the manager.
    if !Path::new("/run/systemd/system").exists() {
        return Ok(ServiceProbe::None);
    }
    let mut active = None;
    let mut uncertain = None;
    for listed in list_units(inst)? {
        if !matches!(
            listed.active.as_str(),
            "active" | "activating" | "deactivating" | "reloading" | "inactive" | "failed"
        ) {
            bail!(
                "Unknown systemd state for Firecracker service: {}",
                listed.active
            );
        }
        // A guest reboot can finish between list-units and show. Use the
        // later single-unit snapshot rather than treating that normal race as
        // a service identity failure.
        match inspect_state(&listed.unit)? {
            ServiceProbe::Active(observed) => {
                if active.replace(observed).is_some() {
                    bail!("Multiple active Firecracker services exist for this instance");
                }
            }
            ServiceProbe::Uncertain(reason) => uncertain = Some(reason),
            ServiceProbe::None if matches!(listed.active.as_str(), "activating" | "reloading") => {
                uncertain = Some(format!(
                    "Firecracker service '{}' may still be starting",
                    listed.unit
                ));
            }
            ServiceProbe::None => {}
        }
    }
    if let Some(reason) = uncertain {
        return Ok(ServiceProbe::Uncertain(reason));
    }
    Ok(active.map_or(ServiceProbe::None, ServiceProbe::Active))
}

fn parse_active_service(unit: &str, show: &str) -> Result<ActiveService> {
    let field = |key: &str| {
        show.lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(name, value)| (name == key).then_some(value))
    };
    if field("Id") != Some(unit) || field("ActiveState") != Some("active") {
        bail!("Firecracker service {unit} changed state during inspection");
    }
    let pid: u32 = field("MainPID")
        .context("Firecracker service has no MainPID")?
        .parse()
        .context("Invalid Firecracker service MainPID")?;
    if pid == 0 {
        bail!("Firecracker service {unit} has no running main process");
    }
    let control_group = field("ControlGroup")
        .filter(|value| value.starts_with("/system.slice/") && value.ends_with(unit))
        .context("Firecracker service has no matching system cgroup")?;
    Ok(ActiveService {
        unit: unit.to_owned(),
        pid,
        control_group: control_group.to_owned(),
    })
}

pub(crate) fn launch(
    inst: &Instance,
    firecracker: &VerifiedFirecracker,
    config: &Path,
    log: &Path,
) -> Result<ActiveService> {
    require_system_manager()?;
    if !matches!(probe(inst)?, ServiceProbe::None) {
        bail!("Firecracker service for this instance is already active or changing state");
    }
    for previous in list_units(inst)? {
        if previous.active == "failed" {
            if !matches!(inspect_state(&previous.unit)?, ServiceProbe::None) {
                bail!("Previous Firecracker service has uncertain process state");
            }
            SetupTool::Systemctl
                .elevated_with_deadline(START_DEADLINE)?
                .args(["--system", "reset-failed", "--"])
                .arg(&previous.unit)
                .run_bounded(START_DEADLINE)
                .with_context(|| format!("Cannot clear previous service {}", previous.unit))?;
        }
    }
    let unit = new_unit(inst)?;
    // Firecracker creates its vsock socket as root in the user's private
    // instance directory. The invoking user needs write access to connect.
    let launch = SetupTool::SystemdRun
        .elevated_with_deadline(START_DEADLINE + Duration::from_secs(5))?
        .args([
            "--no-ask-password",
            "--service-type=exec",
            "--property=KillMode=control-group",
            "--property=TimeoutStopSec=15s",
            "--property=Restart=no",
            "--property=UMask=0111",
            "--property=StandardOutput=null",
            "--property=StandardError=journal",
            "--working-directory=/",
        ])
        .arg(format!("--unit={unit}"))
        .arg("--")
        .arg(escape_service_argument(firecracker.path().as_os_str()))
        .args(["--no-api", "--config-file"])
        .arg(escape_service_argument(config.as_os_str()))
        .arg("--log-path")
        .arg(escape_service_argument(log.as_os_str()))
        .args(["--level", "Info"]);
    let status =
        crate::process_output::run_status_bounded(&mut launch.build(), START_DEADLINE, None)
            .with_context(|| format!("Firecracker service {unit} launch outcome is uncertain"))?;
    if !status.success() {
        bail!(
            "Systemd did not confirm Firecracker exec for service {unit} (launcher status {status})"
        );
    }
    match inspect_state(&unit)? {
        ServiceProbe::Active(service) => Ok(service),
        ServiceProbe::None => {
            bail!(
                "Firecracker VMM exited after systemd exec in service {unit}; inspect `sudo journalctl -u {unit}`"
            )
        }
        ServiceProbe::Uncertain(reason) => bail!("Firecracker launch state is uncertain: {reason}"),
    }
}

pub(crate) fn stop(service: &ActiveService) -> Result<()> {
    SetupTool::Systemctl
        .elevated_with_deadline(START_DEADLINE)?
        .args(["--system", "stop", "--"])
        .arg(&service.unit)
        .run_bounded(START_DEADLINE)
        .with_context(|| format!("Failed to stop Firecracker service {}", service.unit))?;
    match inspect_state(&service.unit)? {
        ServiceProbe::None => Ok(()),
        ServiceProbe::Active(active) => {
            bail!(
                "Firecracker service {} still runs PID {}",
                active.unit,
                active.pid
            )
        }
        ServiceProbe::Uncertain(reason) => bail!("{reason}"),
    }
}

fn inspect_state(unit: &str) -> Result<ServiceProbe> {
    let show = systemctl_query()?
        .args([
            "--system",
            "--no-pager",
            "show",
            "--property=Id,ActiveState,MainPID,ControlGroup,TasksCurrent",
        ])
        .arg(unit)
        .capture()
        .with_context(|| format!("Cannot inspect Firecracker service {unit}"))?;
    parse_service_state(unit, &show, || service_cgroup_empty(unit))
}

fn service_cgroup_empty(unit: &str) -> Result<bool> {
    service_cgroup_empty_at(Path::new("/sys/fs/cgroup"), unit)
}

fn service_cgroup_empty_at(root: &Path, unit: &str) -> Result<bool> {
    // The service ControlGroup is a cgroup-v2 path. On another hierarchy we
    // cannot infer process death from a missing path under this mount.
    let system_slice = root.join("system.slice");
    if !root.join("cgroup.controllers").exists() || !system_slice.is_dir() {
        return Ok(false);
    }
    let group = system_slice.join(unit);
    match fs::metadata(&group) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).context("Cannot inspect Firecracker service cgroup"),
        Ok(_) => {}
    }
    let events = fs::read_to_string(group.join("cgroup.events"))
        .context("Cannot read Firecracker service cgroup events")?;
    Ok(events.lines().any(|line| line == "populated 0"))
}

fn parse_service_state(
    unit: &str,
    show: &str,
    cgroup_empty: impl FnOnce() -> Result<bool>,
) -> Result<ServiceProbe> {
    if !show.lines().any(|line| line == format!("Id={unit}")) {
        bail!("Systemd returned a different Firecracker service identity");
    }
    let state = show
        .lines()
        .find_map(|line| line.strip_prefix("ActiveState="))
        .context("Systemd did not report Firecracker service state")?;
    match state {
        "active" => parse_active_service(unit, show).map(ServiceProbe::Active),
        "inactive" | "failed" => {
            let main_gone = show.lines().any(|line| line == "MainPID=0");
            let expected_group = format!("ControlGroup=/system.slice/{unit}");
            let group_matches = show
                .lines()
                .any(|line| line == "ControlGroup=" || line == expected_group);
            let tasks_gone = if show.lines().any(|line| line == "TasksCurrent=0") {
                true
            } else if show.lines().any(|line| line == "TasksCurrent=[not set]") {
                cgroup_empty()?
            } else {
                false
            };
            if !main_gone || !group_matches || !tasks_gone {
                return Ok(ServiceProbe::Uncertain(format!(
                    "Firecracker service {unit} is {state} but process death is unconfirmed"
                )));
            }
            Ok(ServiceProbe::None)
        }
        other => Ok(ServiceProbe::Uncertain(format!(
            "Firecracker service {unit} is {other}"
        ))),
    }
}

pub(crate) fn wait_for_exit(inst: &Instance, timeout: Duration) -> Result<bool> {
    let started = std::time::Instant::now();
    loop {
        match probe(inst)? {
            ServiceProbe::None => return Ok(true),
            ServiceProbe::Active(_) => {}
            ServiceProbe::Uncertain(reason) => bail!("{reason}"),
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn contains_pid(service: &ActiveService) -> Result<bool> {
    let path = format!("/proc/{}/cgroup", service.pid);
    let cgroup = SetupTool::Cat
        .elevated_with_deadline(Duration::from_secs(5))?
        .arg(&path)
        .capture()
        .with_context(|| format!("Cannot inspect Firecracker PID {} cgroup", service.pid))?;
    Ok(cgroup.lines().any(|line| {
        line.split_once(':')
            .and_then(|(_, tail)| tail.split_once(':'))
            .is_some_and(|(_, path)| path == service.control_group)
    }))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn running_manager_version_is_checked_before_launch() {
        assert!(require_supported_version("252.0-1debian").is_ok());
        assert!(require_supported_version("255.4-1ubuntu8").is_ok());
        assert!(require_supported_version("256").is_ok());
        assert!(require_supported_version("251").is_err());
        assert!(require_supported_version("").is_err());
        assert!(require_supported_version("unknown").is_err());
    }

    #[test]
    fn instance_namespace_depends_on_absolute_directory_and_uid() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: crate::config::InstanceName::new("service-test").unwrap(),
            index: crate::config::InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: crate::config::ImageName::new("default").unwrap(),
            firecracker_control: crate::config::FirecrackerControlMode::Vsock,
        };
        let expected_hash = Sha256Hash::of(root.path().as_os_str().as_bytes());
        let expected = format!("coop-vmm-{}-{expected_hash}-", unsafe { libc::geteuid() });
        assert_eq!(prefix(&inst).unwrap(), expected);
    }

    #[test]
    fn unit_name_accepts_only_its_instance_and_generation() {
        let prefix = "coop-vmm-1000-0123456789abcdef-";
        assert!(valid_unit(
            "coop-vmm-1000-0123456789abcdef-0123456789abcdef0123456789abcdef.service",
            prefix
        ));
        assert!(!valid_unit(
            "coop-vmm-1000-0123456789abcdef-.service",
            prefix
        ));
        assert!(!valid_unit(
            "coop-vmm-1000-0123456789abcdef-0123456789abcdef0123456789abcdeg.service",
            prefix
        ));
        assert!(!valid_unit(
            "coop-vmm-1000-0123456789abcdef-0123456789abcdef0123456789abcdef.socket",
            prefix
        ));
        assert!(!valid_unit(
            "coop-vmm-1001-0123456789abcdef-0123456789abcdef0123456789abcdef.service",
            prefix
        ));
    }

    #[test]
    fn active_service_requires_pid_and_matching_cgroup() {
        let unit = "coop-vmm-1000-a-0123456789abcdef0123456789abcdef.service";
        let show = format!(
            "Id={unit}\nActiveState=active\nMainPID=123\nControlGroup=/system.slice/{unit}\n"
        );
        let parsed = parse_active_service(unit, &show).unwrap();
        assert_eq!(parsed.pid, 123);
        assert_eq!(parsed.control_group, format!("/system.slice/{unit}"));
        assert!(parse_active_service(unit, &show.replace("MainPID=123", "MainPID=0")).is_err());
        assert!(parse_active_service(unit, &show.replace("active", "failed")).is_err());
        assert!(parse_active_service(unit, &show.replace(unit, "other.service")).is_err());
        assert!(
            parse_active_service(unit, &show.replace("/system.slice/", "/user.slice/")).is_err()
        );
        assert!(
            parse_active_service(
                unit,
                &show.replace(
                    &format!("ControlGroup=/system.slice/{unit}"),
                    "ControlGroup=/system.slice/other.service"
                )
            )
            .is_err()
        );
    }

    #[test]
    fn listing_rejects_foreign_unit_and_keeps_failed_generation() {
        let prefix = "coop-vmm-1000-0123456789abcdef-";
        let unit = format!("{prefix}0123456789abcdef0123456789abcdef.service");
        let listed = format!(r#"[{{"unit":"{unit}","active":"failed"}}]"#);
        let parsed = parse_listed_units(&listed, prefix).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].unit, unit);
        assert_eq!(parsed[0].active, "failed");
        assert!(parse_listed_units("not json", prefix).is_err());
        assert!(
            parse_listed_units(r#"[{"unit":"other.service","active":"active"}]"#, prefix).is_err()
        );
    }

    #[test]
    fn failed_service_requires_empty_process_group_before_retry() {
        let unit = "coop-vmm-1000-a-0123456789abcdef0123456789abcdef.service";
        let failed = format!(
            "Id={unit}\nActiveState=failed\nMainPID=0\nTasksCurrent=[not set]\nControlGroup=\n"
        );
        assert_eq!(
            parse_service_state(unit, &failed, || Ok(true)).unwrap(),
            ServiceProbe::None
        );
        assert!(matches!(
            parse_service_state(unit, &failed, || Ok(false)).unwrap(),
            ServiceProbe::Uncertain(_)
        ));
        assert!(matches!(
            parse_service_state(
                unit,
                &failed.replace("TasksCurrent=[not set]", "TasksCurrent=1"),
                || Ok(true)
            )
            .unwrap(),
            ServiceProbe::Uncertain(_)
        ));
        assert!(matches!(
            parse_service_state(unit, &failed.replace("MainPID=0", "MainPID=123"), || Ok(
                true
            ))
            .unwrap(),
            ServiceProbe::Uncertain(_)
        ));
        assert!(matches!(
            parse_service_state(
                unit,
                &failed.replace("ControlGroup=", "ControlGroup=/user.slice/other.service"),
                || Ok(true)
            )
            .unwrap(),
            ServiceProbe::Uncertain(_)
        ));
        assert!(
            parse_service_state(unit, &failed.replace(unit, "other.service"), || Ok(true)).is_err()
        );
        assert_eq!(
            parse_service_state(
                unit,
                &failed.replace("TasksCurrent=[not set]", "TasksCurrent=0"),
                || Ok(false)
            )
            .unwrap(),
            ServiceProbe::None
        );
    }

    #[test]
    fn service_cgroup_requires_absence_or_unpopulated_v2_group() {
        let root = crate::test_support::private_tempdir().unwrap();
        let unit = "coop-vmm-test.service";
        assert!(!service_cgroup_empty_at(root.path(), unit).unwrap());
        fs::write(root.path().join("cgroup.controllers"), "").unwrap();
        assert!(!service_cgroup_empty_at(root.path(), unit).unwrap());
        fs::create_dir(root.path().join("system.slice")).unwrap();
        assert!(service_cgroup_empty_at(root.path(), unit).unwrap());

        let group = root.path().join("system.slice").join(unit);
        fs::create_dir_all(&group).unwrap();
        fs::write(group.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(!service_cgroup_empty_at(root.path(), unit).unwrap());
        fs::write(group.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(service_cgroup_empty_at(root.path(), unit).unwrap());
        fs::remove_file(group.join("cgroup.events")).unwrap();
        assert!(service_cgroup_empty_at(root.path(), unit).is_err());
    }

    #[test]
    fn service_argument_keeps_dollar_bytes_literal() {
        let raw = OsStr::from_bytes(b"/tmp/coop-${NAME}/invalid-\xff-$HOME");
        assert_eq!(
            escape_service_argument(raw).as_bytes(),
            b"/tmp/coop-$${NAME}/invalid-\xff-$$HOME"
        );
    }
}
