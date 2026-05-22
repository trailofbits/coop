use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::backend::{Hostname, LogMode, SshTarget, SshUser};
use crate::config::{CoopConfig, GiB, ImageName, Instance, InstanceName};
use crate::guest::{
    BASE_PACKAGES, DOCKER_PACKAGES, GH_PACKAGES, ProfileDef, SCRIPT_CLAUDE_CODE, SCRIPT_CODEX,
    SCRIPT_DOCKER_REPO, SCRIPT_GH_REPO,
};
use crate::setup::{SetupOptions, TEMPLATE_VERSION, TemplateConfig, utc_timestamp};
use crate::sha256_hash::Sha256Hash;

const LIMA_PREFIX: &str = "coop-";
const BUILDER_NAME: &str = "coop-builder";

// ── Public API ────────────────────────────────────────────────

/// Run first-time setup for Lima backend.
pub fn setup(cfg: &CoopConfig, opts: &SetupOptions) -> Result<()> {
    fs::create_dir_all(&cfg.data_dir).context("Failed to create data directory")?;

    check_requirements()?;
    ensure_ssh_key(cfg)?;

    let image = &opts.image;
    let base_img = cfg.lima_base_path(image);
    if base_img.exists() && !opts.rebuild && !needs_rebuild(cfg, image, &opts.profiles) {
        eprintln!(
            "\n=> Golden image '{image}': up to date ({})",
            base_img.display()
        );
    } else {
        if base_img.exists() && !opts.rebuild {
            eprintln!("\n=> Golden image '{image}' is stale — rebuilding");
        }
        build_golden_image(cfg, image, &opts.profiles)?;
    }

    generate_start_template(cfg, image)?;

    eprintln!("\nSetup complete.");
    eprintln!("Run `coop start` to launch a VM.");
    Ok(())
}

/// Create and start a Lima VM instance.
pub fn create_and_start(
    cfg: &CoopConfig,
    inst: &Instance,
    disk_gib: Option<crate::config::GiB>,
    mounts: &[crate::config::Mount],
) -> Result<()> {
    let name = lima_name(inst);
    let template_path = cfg.lima_template_path(&inst.image);
    let base_img = cfg.lima_base_path(&inst.image);

    if !template_path.exists() || !base_img.exists() {
        bail!(
            "No image '{}' found.\n\
             Run `coop setup --image {}` first.",
            inst.image,
            inst.image,
        );
    }

    // When mounts are requested, generate a per-instance template that
    // includes them. Otherwise use the shared image template.
    let effective_template = if mounts.is_empty() {
        template_path.clone()
    } else {
        let inst_template = inst.dir.join("lima-template.yaml");
        let base_yaml = fs::read_to_string(&template_path)
            .with_context(|| format!("Failed to read {}", template_path.display()))?;
        let yaml = inject_mounts(&base_yaml, mounts);
        fs::write(&inst_template, &yaml)
            .with_context(|| format!("Failed to write {}", inst_template.display()))?;
        for m in mounts {
            tracing::info!("Mount: {} -> {}", m.host_path.display(), m.guest_path);
        }
        inst_template
    };

    // Clean up leftover Lima instance from a previous failed start
    if let Some(status) = lima_status(&inst.name) {
        tracing::warn!(
            "Lima instance '{name}' already exists (status: {status}) — \
             cleaning up before re-creating"
        );
        if let Err(e) = Command::new("limactl")
            .args(["delete", "--force", &name])
            .status()
        {
            tracing::debug!("Failed to force-delete stale instance (non-fatal): {e}");
        }
    }

    tracing::info!("Creating Lima instance '{name}'");

    let disk = disk_gib.unwrap_or_else(|| {
        // Default to the base image size (rounded up to GiB) so the
        // instance disk is never smaller than the golden image it was
        // cloned from.  Fall back to template_size_gib if the file
        // size can't be read.
        base_image_size_gib(&base_img).unwrap_or(cfg.vm.template_size_gib)
    });
    let mem_gib = cfg.vm.mem_size_mib.as_gib_f64();
    let mut child = Command::new("limactl")
        .arg("start")
        .arg(&effective_template)
        .arg(format!("--name={name}"))
        .arg(format!("--cpus={}", cfg.vm.vcpu_count))
        .arg(format!("--memory={mem_gib}"))
        .arg(format!("--disk={disk}"))
        .arg("--tty=false")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn limactl start")?;

    if let Err(e) = wait_for_lima_ssh(&inst.name, &mut child, cfg, Duration::from_secs(60)) {
        let _ = child.kill();
        let lima_stderr = match child.wait_with_output() {
            Ok(out) => String::from_utf8_lossy(&out.stderr).to_string(),
            Err(_) => String::new(),
        };
        let detail = extract_lima_error(&lima_stderr);
        bail!(
            "Failed to start instance '{name}': {e}\n\
             {detail}\
             Check `limactl list` and logs with \
             `coop logs {}`.",
            inst.name,
        );
    }

    reap_in_background(child);
    tracing::info!("Lima instance '{name}' started");
    Ok(())
}

/// Start an existing stopped Lima instance (no template — resumes in place).
pub fn start_existing(cfg: &CoopConfig, inst: &Instance) -> Result<()> {
    let name = lima_name(inst);

    if is_running(inst) {
        bail!("Lima instance '{name}' is already running");
    }

    tracing::info!("Restarting stopped Lima instance '{name}'");

    let mut child = Command::new("limactl")
        .args(["start", &name])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn limactl start")?;

    if let Err(e) = wait_for_lima_ssh(&inst.name, &mut child, cfg, Duration::from_secs(60)) {
        let _ = child.kill();
        let lima_stderr = match child.wait_with_output() {
            Ok(out) => String::from_utf8_lossy(&out.stderr).to_string(),
            Err(_) => String::new(),
        };
        let detail = extract_lima_error(&lima_stderr);
        bail!(
            "Failed to restart instance '{}': {e}\n\
             {detail}\
             Check logs with `coop logs {}`.",
            inst.name,
            inst.name,
        );
    }

    reap_in_background(child);
    tracing::info!("Lima instance '{name}' restarted");
    Ok(())
}

/// Stop a Lima instance that has already been verified as running.
///
/// The caller's `RunningInstance` token proves the precondition, so
/// this skips the live-state probe and only handles the shutdown.
pub fn stop_running(inst: &Instance) -> Result<()> {
    let name = lima_name(inst);

    tracing::info!("Stopping Lima instance '{name}'");

    let status = Command::new("limactl")
        .args(["stop", &name])
        .status()
        .context("Failed to run limactl stop")?;

    if !status.success() {
        bail!("limactl stop failed for '{name}'");
    }

    tracing::info!("Lima instance '{name}' stopped");
    Ok(())
}

/// Delete a Lima instance.
pub fn destroy(inst: &Instance) -> Result<()> {
    let name = lima_name(inst);
    tracing::info!("Deleting Lima instance '{name}'");

    // If the instance doesn't exist in Lima, nothing to do
    if lima_status(&inst.name).is_none() {
        tracing::debug!("Lima instance '{name}' not found — already deleted");
        return Ok(());
    }

    // Stop first if running
    if is_running(inst)
        && let Err(e) = Command::new("limactl").args(["stop", &name]).status()
    {
        tracing::debug!("Failed to stop Lima instance '{name}' before delete (non-fatal): {e}");
    }

    let status = Command::new("limactl")
        .args(["delete", "--force", &name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run limactl delete")?;

    if !status.success() {
        bail!("limactl delete failed for '{name}'");
    }

    Ok(())
}

/// Resize the disk of a stopped Lima instance.
///
/// Truncates the disk to the new size. Cloud-init's `growpart`
/// will expand the partition and filesystem on next boot.
pub fn resize_disk(_cfg: &CoopConfig, inst: &Instance, new_size: crate::config::GiB) -> Result<()> {
    let disk = disk_path(inst)?;

    if !disk.exists() {
        bail!(
            "Lima disk not found at {}.\n\
             Is instance '{}' created?",
            disk.display(),
            inst.name,
        );
    }

    let current_bytes = std::fs::metadata(&disk)
        .with_context(|| format!("Failed to stat {}", disk.display()))?
        .len();
    let current_gib = current_bytes / (1024 * 1024 * 1024);
    let new_gib = u64::from(new_size.as_u32());

    if new_gib < current_gib {
        bail!(
            "Shrinking is not supported (current: {current_gib} GiB, \
             requested: {new_gib} GiB)"
        );
    }
    if new_gib == current_gib {
        tracing::info!("Disk is already {current_gib} GiB — nothing to do");
        return Ok(());
    }

    tracing::info!(
        "Resizing instance '{}' from {current_gib} to {new_gib} GiB",
        inst.name,
    );
    let status = Command::new("truncate")
        .arg("-s")
        .arg(format!("{new_size}G"))
        .arg(&disk)
        .status()
        .context("Failed to run truncate")?;

    if !status.success() {
        bail!("truncate failed for {}", disk.display());
    }

    tracing::info!(
        "Resize complete. The filesystem will expand on next start \
         (cloud-init growpart)."
    );
    Ok(())
}

/// Path to the VM disk for a Lima instance.
///
/// Lima v2.1.0 renamed `diffdisk` to `disk`. We try `disk` first
/// (new layout) and fall back to `diffdisk` (pre-v2.1.0).
pub fn disk_path(inst: &Instance) -> Result<PathBuf> {
    let name = lima_name(inst);
    let dir = lima_home()?.join(name);
    let new_path = dir.join("disk");
    if new_path.exists() {
        return Ok(new_path);
    }
    Ok(dir.join("diffdisk"))
}

/// Check if a Lima instance is running.
pub fn is_running(inst: &Instance) -> bool {
    match lima_status(&inst.name) {
        Some(s) => s == "Running",
        None => false,
    }
}

/// Get a human-readable status string.
pub fn status(cfg: &CoopConfig, inst: &Instance) -> Result<String> {
    let info = limactl_info(&inst.name)?;

    let status_str = info["status"].as_str().unwrap_or("Unknown");
    let arch = info["arch"].as_str().unwrap_or("unknown");
    let cpus = info["cpus"].as_u64().unwrap_or(0);
    let memory_bytes = info["memory"].as_u64().unwrap_or(0);
    let memory_mib = memory_bytes / (1024 * 1024);
    let disk_gib = disk_path(inst)
        .and_then(|p| {
            std::fs::metadata(&p).with_context(|| format!("Failed to stat {}", p.display()))
        })
        .map_or_else(
            |_| info["disk"].as_u64().unwrap_or(0) / (1024 * 1024 * 1024),
            |m| m.len() / (1024 * 1024 * 1024),
        );
    let ssh_port = info["sshLocalPort"].as_u64().unwrap_or(0);

    let mut out = format!(
        "Instance '{}' ({status_str})\n\
         \x20 Backend: lima\n\
         \x20 Arch: {arch}\n\
         \x20 vCPUs: {cpus}\n\
         \x20 Memory: {memory_mib} MiB\n\
         \x20 Disk: {disk_gib} GiB\n\
         \x20 SSH: localhost:{ssh_port} (key: {})",
        inst.name,
        cfg.ssh_key_path().display(),
    );

    if status_str == "Running"
        && let Ok(target) = ssh_target(cfg, inst)
        && let Some(usage) = crate::backend::query_resource_usage(&target)
    {
        use std::fmt::Write as _;
        let _ = write!(out, "\n  {usage}");
    }

    Ok(out)
}

/// Stream Lima instance logs.
pub fn stream_logs(inst: &Instance, mode: LogMode) -> Result<()> {
    let name = lima_name(inst);

    // Lima logs are in ~/.lima/<name>/serial.log
    let lima_dir = lima_home()?.join(&name);
    let serial_log = lima_dir.join("serial.log");
    let ha_log = lima_dir.join("ha.stderr.log");

    // Prefer serial.log, fall back to ha.stderr.log
    let log_path = if serial_log.exists() {
        serial_log
    } else if ha_log.exists() {
        ha_log
    } else {
        bail!(
            "No log files found for Lima instance '{name}'.\n\
             Expected: {}",
            serial_log.display()
        );
    };

    match mode {
        LogMode::Follow => {
            let mut child = Command::new("tail")
                .arg("-f")
                .arg(&log_path)
                .spawn()
                .context("Failed to tail log file")?;
            child.wait().context("Log streaming interrupted")?;
        }
        LogMode::Snapshot => {
            let file = fs::File::open(&log_path).context("Failed to open log file")?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line.context("Failed to read log line")?;
                writeln!(std::io::stdout(), "{line}").context("Failed to write log line")?;
            }
        }
    }
    Ok(())
}

/// Get SSH connection details for a Lima instance.
pub fn ssh_target(cfg: &CoopConfig, inst: &Instance) -> Result<SshTarget> {
    let info = limactl_info(&inst.name)?;

    let port = info["sshLocalPort"]
        .as_u64()
        .context("Lima instance has no sshLocalPort")?;
    let port = u16::try_from(port).context("SSH port out of range")?;
    let port = std::num::NonZeroU16::new(port).context("Lima SSH port is 0")?;

    Ok(SshTarget {
        host: Hostname::new("127.0.0.1")?,
        port,
        user: SshUser::new(crate::guest::GUEST_USER)?,
        key_path: cfg.ssh_key_path(),
    })
}

// ── Setup helpers ─────────────────────────────────────────────

fn check_requirements() -> Result<()> {
    eprintln!("\n=> Checking Lima requirements");

    let output = Command::new("limactl").arg("--version").output();

    match output {
        Ok(o) if o.status.success() => {
            let version = String::from_utf8_lossy(&o.stdout).trim().to_string();
            eprintln!("  limactl: {version}");
        }
        _ => {
            bail!(
                "limactl not found. Install it with:\n\
                 \n\
                 \x20   brew install lima\n\
                 \n\
                 Then re-run `coop setup`."
            );
        }
    }

    Ok(())
}

fn ensure_ssh_key(cfg: &CoopConfig) -> Result<()> {
    let key_path = cfg.ssh_key_path();
    if key_path.exists() {
        eprintln!("\n=> SSH key: already at {}", key_path.display());
        return Ok(());
    }

    eprintln!("\n=> Generating SSH key pair for VM access");
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&key_path)
        .args(["-N", "", "-q"])
        .status()
        .context("Failed to generate SSH key")?;

    if !status.success() {
        bail!("ssh-keygen failed");
    }

    eprintln!("  Key: {}", key_path.display());
    Ok(())
}

fn provision_script_hash(cfg: &CoopConfig, profiles: &[ProfileDef]) -> Sha256Hash {
    let pubkey_path = cfg.ssh_key_path().with_extension("pub");
    let pubkey = fs::read_to_string(&pubkey_path).unwrap_or_default();
    let script = compose_provision_script(pubkey.trim(), profiles);
    Sha256Hash::of(&script)
}

/// Returns `true` if the golden image needs rebuilding.
///
/// A rebuild is needed when the config file is missing (orphaned
/// image), the provision-script hash has changed, or the baked
/// marketplace/plugin lists have changed.
fn needs_rebuild(cfg: &CoopConfig, image: &ImageName, profiles: &[ProfileDef]) -> bool {
    let current_hash = provision_script_hash(cfg, profiles);
    let Ok(existing) = TemplateConfig::load_for(cfg, image) else {
        tracing::info!("Template config missing — treating golden image as stale");
        return true;
    };

    if existing.install_script_hash != current_hash {
        return true;
    }

    let (wanted_m, wanted_p) = crate::guest::collect_baked_lists(cfg, profiles);
    existing.marketplaces != wanted_m || existing.plugins != wanted_p
}

fn build_golden_image(cfg: &CoopConfig, image: &ImageName, profiles: &[ProfileDef]) -> Result<()> {
    eprintln!(
        "\n=> Building golden VM image \
         (this takes a few minutes on first run)"
    );

    let pubkey_path = cfg.ssh_key_path().with_extension("pub");
    let pubkey = fs::read_to_string(&pubkey_path)
        .with_context(|| format!("Failed to read SSH public key at {}", pubkey_path.display()))?;

    // Clean up any leftover builder instance from a previous failed build
    if let Err(e) = Command::new("limactl")
        .args(["delete", "--force", BUILDER_NAME])
        .output()
    {
        tracing::debug!("Failed to clean up leftover builder (non-fatal): {e}");
    }

    // Clean up leftover staging image from a previous failed build
    let base_img = cfg.lima_base_path(image);
    let staging = base_img.with_extension("img.new");
    if staging.exists()
        && let Err(e) = fs::remove_file(&staging)
    {
        tracing::debug!("Failed to remove stale staging image (non-fatal): {e}");
    }

    // Generate builder template with full provisioning
    let builder_template = cfg.data_dir.join("lima-builder.yaml");
    let provision_script = compose_provision_script(pubkey.trim(), profiles);
    let yaml = compose_template_yaml(cfg, &provision_script);
    fs::write(&builder_template, &yaml).with_context(|| {
        format!(
            "Failed to write builder template at {}",
            builder_template.display()
        )
    })?;

    if !profiles.is_empty() {
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        eprintln!("  Profiles: {}", names.join(", "));
    }

    // Build to staging path — old image is never touched
    let result = run_builder_vm(cfg, profiles, &staging, &builder_template);

    // Always clean up builder resources
    eprintln!("  Cleaning up builder VM...");
    if let Err(e) = Command::new("limactl")
        .args(["delete", "--force", BUILDER_NAME])
        .status()
    {
        tracing::warn!("Failed to delete builder VM (non-fatal): {e}");
    }
    if let Err(e) = fs::remove_file(&builder_template) {
        tracing::debug!("Failed to remove builder template (non-fatal): {e}");
    }

    let (marketplaces, plugins) = match result {
        Ok(lists) => lists,
        Err(e) => {
            // Clean up failed staging image
            if let Err(rm_err) = fs::remove_file(&staging) {
                tracing::debug!("Failed to remove staging image (non-fatal): {rm_err}");
            }
            return Err(e);
        }
    };

    // Swap staging into place — old image is replaced atomically
    fs::rename(&staging, &base_img).with_context(|| {
        format!(
            "Failed to swap staging image {} -> {}",
            staging.display(),
            base_img.display()
        )
    })?;

    #[expect(clippy::cast_precision_loss, reason = "file size fits in f64")]
    let size_gib = fs::metadata(&base_img)
        .map(|m| m.len() as f64 / (1024.0 * 1024.0 * 1024.0))
        .unwrap_or(0.0);
    eprintln!("  Golden image: {} ({size_gib:.1} GiB)", base_img.display());

    // Write template config only after image swap succeeds
    let template_config = TemplateConfig {
        version: TEMPLATE_VERSION,
        created: utc_timestamp(),
        install_script_hash: provision_script_hash(cfg, profiles),
        profiles: profiles.iter().map(|p| p.name.clone()).collect(),
        extra_packages: Vec::new(),
        post_install_hash: None,
        marketplaces,
        plugins,
    };
    template_config.save_for(cfg, image)?;

    Ok(())
}

/// Start the builder VM, install marketplaces/plugins, clean
/// cloud-init, stop it, and extract the disk image to `output_path`.
fn run_builder_vm(
    cfg: &CoopConfig,
    profiles: &[ProfileDef],
    output_path: &std::path::Path,
    builder_template: &std::path::Path,
) -> Result<(Vec<String>, Vec<String>)> {
    eprintln!("  Starting builder VM and installing packages...");
    let status = Command::new("limactl")
        .arg("start")
        .arg(builder_template)
        .arg(format!("--name={BUILDER_NAME}"))
        .arg("--tty=false")
        .status()
        .context("Failed to run limactl start for builder")?;

    if !status.success() {
        dump_builder_logs();
        bail!("Failed to build golden image — limactl start failed.");
    }

    // limactl start returns exit 0 even when the provision script
    // fails — it only waits for SSH, not cloud-init completion.
    // Verify cloud-init finished successfully before proceeding.
    verify_cloud_init_status()?;

    // Verify critical binaries are installed before extracting the
    // image. Cloud-init can report "done" even if individual install
    // steps failed silently (e.g. network timeout during download).
    verify_builder_binaries()?;

    // Install marketplaces and plugins into the builder VM so they're
    // baked into the golden image. Runs after binary verification
    // (confirms claude CLI is installed) and before cloud-init clean.
    let (marketplaces, plugins) = install_builder_plugins(cfg, profiles)?;

    // Clean cloud-init state so it re-runs on new instances
    eprintln!("  Preparing image for reuse...");
    let ci_status = Command::new("limactl")
        .args([
            "shell",
            BUILDER_NAME,
            "--",
            "sudo",
            "cloud-init",
            "clean",
            "--logs",
        ])
        .status()
        .context("Failed to run cloud-init clean in builder")?;

    if !ci_status.success() {
        bail!(
            "cloud-init clean failed in builder VM — \
             golden image may not be reusable"
        );
    }

    // Stop the builder VM
    eprintln!("  Stopping builder VM...");
    let status = Command::new("limactl")
        .args(["stop", BUILDER_NAME])
        .status()
        .context("Failed to stop builder VM")?;

    if !status.success() {
        bail!("Failed to stop builder VM");
    }

    // Extract the disk image.
    // Lima v2.1.0 renamed "diffdisk" to "disk"; try new name first.
    eprintln!("  Extracting disk image...");
    let lima_dir = lima_home()?.join(BUILDER_NAME);
    let src_disk = lima_dir.join("disk");
    let src_disk = if src_disk.exists() {
        src_disk
    } else {
        let legacy = lima_dir.join("diffdisk");
        if !legacy.exists() {
            bail!(
                "Builder disk not found at {} or {}.\n\
                 Lima may use a different disk layout.",
                lima_dir.join("disk").display(),
                legacy.display(),
            );
        }
        legacy
    };

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create dir {}", parent.display()))?;
    }
    fs::copy(&src_disk, output_path).with_context(|| {
        format!(
            "Failed to copy disk from {} to {}",
            src_disk.display(),
            output_path.display(),
        )
    })?;

    Ok((marketplaces, plugins))
}

/// Get an SSH target for the builder VM.
fn builder_ssh_target(cfg: &CoopConfig) -> Result<SshTarget> {
    let info = limactl_list_entry(BUILDER_NAME)?;
    let port = info["sshLocalPort"]
        .as_u64()
        .context("Builder VM has no sshLocalPort")?;
    let port = u16::try_from(port).context("SSH port out of range")?;
    let port = std::num::NonZeroU16::new(port).context("Builder SSH port is 0")?;
    Ok(SshTarget {
        host: Hostname::new("127.0.0.1")?,
        port,
        user: SshUser::new(crate::guest::GUEST_USER)?,
        key_path: cfg.ssh_key_path(),
    })
}

/// Install marketplaces and plugins in the builder VM via SSH.
/// Returns the lists that were installed (for recording in
/// `TemplateConfig`).
fn install_builder_plugins(
    cfg: &CoopConfig,
    profiles: &[ProfileDef],
) -> Result<(Vec<String>, Vec<String>)> {
    let (marketplaces, plugins) = crate::guest::collect_baked_lists(cfg, profiles);

    if marketplaces.is_empty() && plugins.is_empty() {
        return Ok((marketplaces, plugins));
    }

    eprintln!("  Installing marketplaces and plugins...");
    let target = builder_ssh_target(cfg)?;

    // Minimal env forwarding: GITHUB_TOKEN for private repo access
    let mut env = crate::backend::EnvForward::default();
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        env.set("GITHUB_TOKEN", token);
    }

    let session = crate::backend::SshSession { target, env };

    if !marketplaces.is_empty() {
        crate::backend::install_marketplaces(&session, &marketplaces)?;
    }

    if !plugins.is_empty() {
        crate::backend::install_plugins(&session, &plugins)?;
    }

    Ok((marketplaces, plugins))
}

/// Dump diagnostic logs from the builder VM when `limactl start` fails.
///
/// Tries two sources: the Lima serial log on the host filesystem,
/// and (if SSH is still reachable) cloud-init-output.log inside the guest.
fn dump_builder_logs() {
    eprintln!("\n  --- Builder VM diagnostic logs ---");

    // 1. Host-side: Lima serial log
    if let Ok(lima_home) = lima_home() {
        let serial = lima_home.join(BUILDER_NAME).join("serial.log");
        if let Ok(content) = std::fs::read_to_string(&serial) {
            let lines: Vec<&str> = content.lines().collect();
            let tail: Vec<&str> = lines
                .iter()
                .rev()
                .take(60)
                .copied()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            eprintln!(
                "\n  Last 60 lines of {}:\n{}",
                serial.display(),
                tail.join("\n")
            );
        }
    }

    // 2. Guest-side: try to grab cloud-init-output.log via SSH
    if let Ok(output) = Command::new("limactl")
        .args([
            "shell",
            BUILDER_NAME,
            "--",
            "sudo",
            "tail",
            "-n",
            "40",
            "/var/log/cloud-init-output.log",
        ])
        .output()
    {
        let log = String::from_utf8_lossy(&output.stdout);
        if !log.trim().is_empty() {
            eprintln!(
                "\n  Last 40 lines of cloud-init-output.log:\n{}",
                log.trim()
            );
        }
    }

    eprintln!("  --- End diagnostic logs ---\n");
}

/// Verify cloud-init completed without errors in the builder VM.
///
/// `limactl start` returns exit 0 as soon as SSH is reachable, even
/// if the provision script failed. `cloud-init status --wait` blocks
/// until cloud-init finishes and returns exit 1 on error.
fn verify_cloud_init_status() -> Result<()> {
    eprintln!("  Verifying provision script completed...");
    let output = Command::new("limactl")
        .args([
            "shell",
            BUILDER_NAME,
            "--",
            "cloud-init",
            "status",
            "--wait",
        ])
        .output()
        .context("Failed to check cloud-init status in builder VM")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if let Err(e) = check_cloud_init_output(output.status.code(), &stdout) {
        // Grab diagnostics from both cloud-init logs
        let grab_log = |path: &str| -> String {
            Command::new("limactl")
                .args([
                    "shell",
                    BUILDER_NAME,
                    "--",
                    "sudo",
                    "tail",
                    "-n",
                    "30",
                    path,
                ])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default()
        };

        let output_log = grab_log("/var/log/cloud-init-output.log");
        let main_log = grab_log("/var/log/cloud-init.log");

        let log_section = if output_log.trim().is_empty() {
            format!(
                "(/var/log/cloud-init-output.log is empty)\n\n\
                 Last 30 lines of /var/log/cloud-init.log:\n{}",
                main_log.trim()
            )
        } else {
            format!(
                "Last 30 lines of /var/log/cloud-init-output.log:\n{}",
                output_log.trim()
            )
        };

        bail!("{e}\n\n{log_section}");
    }

    Ok(())
}

/// Check cloud-init status output to determine if provisioning succeeded.
///
/// Cloud-init 24.x exit codes:
///   0 — status: done (clean)
///   1 — status: error (fatal failure)
///   2 — status: done with recoverable errors (deprecation warnings, etc.)
///
/// Exit code 2 ("degraded done") is acceptable — Lima's cloud-config
/// triggers deprecation warnings (e.g., `ssh-authorized-keys` vs
/// `ssh_authorized_keys`) that don't affect provisioning.
fn check_cloud_init_output(exit_code: Option<i32>, stdout: &str) -> Result<()> {
    // Always fail if stdout explicitly reports an error status
    if stdout.contains("status: error") {
        bail!(
            "Provision script failed in builder VM.\n\
             cloud-init status: {}",
            stdout.trim(),
        );
    }
    // Accept exit 0 (clean done) and exit 2 (degraded done)
    if let Some(0 | 2) = exit_code {
        return Ok(());
    }
    // Also accept if stdout explicitly says "status: done" regardless
    // of exit code (defensive against future cloud-init changes)
    if stdout.contains("status: done") {
        return Ok(());
    }
    bail!(
        "Provision script failed in builder VM.\n\
         cloud-init status: {}",
        stdout.trim(),
    );
}

/// Verify that critical binaries are installed in the builder VM.
///
/// Runs after cloud-init completes but before extracting the disk
/// image. Catches silent install failures (e.g. network timeouts)
/// that cloud-init doesn't flag as errors.
fn verify_builder_binaries() -> Result<()> {
    use crate::guest::REQUIRED_GUEST_BINARIES;

    eprintln!("  Verifying installed binaries...");
    let mut missing = Vec::new();

    for &path in REQUIRED_GUEST_BINARIES {
        let check_cmd = format!("test -x {path}");
        let status = Command::new("limactl")
            .args(["shell", BUILDER_NAME, "--", "bash", "-c", &check_cmd])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        if !status.is_ok_and(|s| s.success()) {
            missing.push(path);
        }
    }

    if !missing.is_empty() {
        let log_tail = Command::new("limactl")
            .args([
                "shell",
                BUILDER_NAME,
                "--",
                "sudo",
                "tail",
                "-n",
                "50",
                "/var/log/cloud-init-output.log",
            ])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        let log_section = if log_tail.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\nLast 50 lines of cloud-init-output.log:\n{}",
                log_tail.trim()
            )
        };

        bail!(
            "Golden image is missing required binaries: {}\n\
             The provision script completed but these tools were \
             not installed correctly.{log_section}",
            missing.join(", "),
        );
    }

    Ok(())
}

fn generate_start_template(cfg: &CoopConfig, image: &ImageName) -> Result<()> {
    eprintln!("\n=> Generating fast-start template");

    let base_img = cfg
        .lima_base_path(image)
        .canonicalize()
        .context("Golden image not found — run setup first")?;

    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };

    let yaml = format!(
        r#"# Generated by coop — do not edit manually
vmType: "vz"
rosetta:
  enabled: true
  binfmt: true

images:
- location: "{image_path}"
  arch: "{arch}"

cpus: {vcpus}
memory: "{mem_gib}GiB"
disk: "{disk_gib}GiB"

mountType: "virtiofs"
mounts: []

containerd:
  system: false
  user: false
"#,
        image_path = base_img.display(),
        arch = arch,
        vcpus = cfg.vm.vcpu_count,
        mem_gib = cfg.vm.mem_size_mib.as_gib_f64(),
        disk_gib = cfg.vm.template_size_gib,
    );

    let template_path = cfg.lima_template_path(image);
    fs::write(&template_path, &yaml)
        .with_context(|| format!("Failed to write {}", template_path.display()))?;

    eprintln!("  Template: {}", template_path.display());
    Ok(())
}

/// Replace `mounts: []` in a Lima YAML template with writable virtiofs
/// mount entries. Returns the modified YAML string.
fn inject_mounts(yaml: &str, mounts: &[crate::config::Mount]) -> String {
    use std::fmt::Write;
    let mut mount_yaml = String::from("mounts:\n");
    for m in mounts {
        let _ = write!(
            mount_yaml,
            "- location: \"{}\"\n  mountPoint: \"{}\"\n  writable: true\n",
            m.host_path.display(),
            m.guest_path,
        );
    }
    yaml.replace("mounts: []", mount_yaml.trim_end())
}

fn compose_template_yaml(cfg: &CoopConfig, provision_script: &str) -> String {
    // Indent each line of the provision script for YAML embedding
    let indented: String = provision_script
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("      {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"# Generated by coop — do not edit manually
vmType: "vz"
rosetta:
  enabled: true
  binfmt: true

images:
- location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
  arch: "aarch64"
- location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img"
  arch: "x86_64"

cpus: {vcpus}
memory: "{mem_gib}GiB"
disk: "{disk_gib}GiB"

mountType: "virtiofs"
mounts: []

containerd:
  system: false
  user: false

provision:
- mode: system
  script: |
{script}
"#,
        vcpus = cfg.vm.vcpu_count,
        mem_gib = cfg.vm.mem_size_mib.as_gib_f64(),
        disk_gib = cfg.vm.template_size_gib,
        script = indented,
    )
}

fn compose_provision_script(ssh_pubkey: &str, profiles: &[ProfileDef]) -> String {
    let mut s = String::with_capacity(8192);

    // Preamble
    s.push_str("#!/bin/bash\n");
    s.push_str("set -euo pipefail\n");
    s.push_str("export DEBIAN_FRONTEND=noninteractive\n");
    s.push_str("export DPKG_OPTIONS='--force-confnew'\n");
    s.push_str("APT_OPTS=(-o Dpkg::Options::=--force-confnew)\n\n");

    // Add all third-party repos first (curl/gpg available on cloud image)
    s.push_str(SCRIPT_GH_REPO);
    s.push('\n');
    s.push_str(SCRIPT_DOCKER_REPO);
    s.push('\n');

    // Collect packages/scripts from already-resolved profiles
    let profile_apt: Vec<&str> = profiles
        .iter()
        .flat_map(|d| d.apt_packages.iter().map(String::as_str))
        .collect();
    let pre_scripts: Vec<&str> = profiles
        .iter()
        .filter_map(|d| d.pre_install.as_deref())
        .collect();
    let post_scripts: Vec<&str> = profiles
        .iter()
        .filter_map(|d| d.post_install.as_deref())
        .collect();

    // Profile pre-scripts (may add repos, e.g. NodeSource)
    for pre in &pre_scripts {
        s.push('\n');
        s.push_str(pre);
        if !pre.ends_with('\n') {
            s.push('\n');
        }
    }

    // Single apt-get update covering all repos
    s.push_str("\necho '  [guest] Updating package lists...'\n");
    s.push_str("apt-get update -qq\n\n");

    // Combine all packages into a single install
    let all_packages: Vec<&str> = BASE_PACKAGES
        .iter()
        .chain(GH_PACKAGES)
        .chain(DOCKER_PACKAGES)
        .copied()
        .chain(profile_apt.iter().copied())
        .collect();

    s.push_str("echo '  [guest] Installing all packages...'\n");
    s.push_str(
        "apt-get install -y -qq \"${APT_OPTS[@]}\" \
         --no-install-recommends \\\n    ",
    );
    s.push_str(&all_packages.join(" "));
    s.push_str(" < /dev/null\n");

    // Profile post-install scripts
    for post in &post_scripts {
        s.push('\n');
        s.push_str(post);
        if !post.ends_with('\n') {
            s.push('\n');
        }
    }

    // Guest configuration configures the ubuntu user — must come before claude-code install
    s.push_str(&compose_lima_guest_config(ssh_pubkey));

    // Claude Code (direct binary download, runs as root, chowns to claude)
    s.push_str(SCRIPT_CLAUDE_CODE);
    s.push('\n');

    // Codex CLI (standalone binary under /usr/local/bin)
    s.push_str(SCRIPT_CODEX);
    s.push('\n');

    // Test hook: inject a provision failure to exercise error detection.
    // Only activates when COOP_TEST_INJECT_PROVISION_FAILURE is set.
    if std::env::var("COOP_TEST_INJECT_PROVISION_FAILURE").is_ok() {
        s.push_str("\necho '  [guest] INJECTED FAILURE FOR TESTING'\n");
        s.push_str("exit 1\n");
    }

    // Cleanup
    s.push_str("\necho '  [guest] Cleaning up...'\n");
    s.push_str("apt-get clean\n");
    s.push_str("rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*\n");
    s.push_str("\necho '  [guest] Provisioning complete'\n");

    s
}

fn compose_lima_guest_config(ssh_pubkey: &str) -> String {
    format!(
        r#"
echo '  [guest] Ensuring ubuntu user exists (uid 1000)...'
if id ubuntu &>/dev/null; then
    usermod -aG sudo,docker ubuntu
else
    # Remove any other user occupying uid 1000 (e.g. Lima's host-mirror user)
    EXISTING=$(getent passwd 1000 | cut -d: -f1) || true
    if [[ -n "$EXISTING" ]]; then
        userdel "$EXISTING"
    fi
    useradd -m -s /bin/bash --uid 1000 -G sudo,docker ubuntu
fi
echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu
chmod 440 /etc/sudoers.d/ubuntu

echo '  [guest] Setting up home and SSH for ubuntu user...'
mkdir -p /home/ubuntu
chown ubuntu:ubuntu /home/ubuntu
chmod 755 /home/ubuntu
install -d -o ubuntu -g ubuntu /home/ubuntu/.local
install -d -o ubuntu -g ubuntu /home/ubuntu/.local/bin
install -d -o ubuntu -g ubuntu /home/ubuntu/.local/share
mkdir -p /home/ubuntu/.ssh
echo '{ssh_pubkey}' > /home/ubuntu/.ssh/authorized_keys
chown -R ubuntu:ubuntu /home/ubuntu/.ssh
chmod 700 /home/ubuntu/.ssh
chmod 600 /home/ubuntu/.ssh/authorized_keys

echo '  [guest] Configuring ubuntu user PATH...'
echo 'export PATH="$HOME/.local/bin:$PATH"' >> /home/ubuntu/.profile
echo 'export PATH="$HOME/.local/bin:$PATH"' >> /home/ubuntu/.bashrc

echo '  [guest] Symlinking claude into system PATH...'
ln -sf /home/ubuntu/.local/bin/claude /usr/local/bin/claude

echo '  [guest] Installing claude-yolo shortcut...'
cat > /usr/local/bin/claude-yolo <<'YOLOEOF'
#!/bin/bash
exec claude --dangerously-skip-permissions "$@"
YOLOEOF
chmod 755 /usr/local/bin/claude-yolo

echo '  [guest] Installing codex-yolo shortcut...'
cat > /usr/local/bin/codex-yolo <<'YOLOEOF'
#!/bin/bash
exec codex --dangerously-bypass-approvals-and-sandbox "$@"
YOLOEOF
chmod 755 /usr/local/bin/codex-yolo

echo '  [guest] Preparing workspace directory...'
mkdir -p /workspace
chown ubuntu:ubuntu /workspace

# No iptables-legacy or NO_IPTABLES_RAW needed — Lima uses a full kernel with
# nftables and iptable_raw support. These workarounds are Firecracker-only
# (see scripts/guest/guest-config.sh and CLAUDE.md).

echo '  [guest] Enabling services...'
systemctl enable docker ssh

echo '  [guest] Configuring SSH daemon...'
sed -i 's/#PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
sed -i 's/#PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config

echo '  [guest] Configuring SSH env forwarding...'
echo 'AcceptEnv *' >> /etc/ssh/sshd_config
"#
    )
}

// ── Fast start helpers ────────────────────────────────────────

/// Poll for SSH readiness with fast auth probing.
///
/// Lima's hostagent polls SSH every 10 seconds. By probing SSH
/// auth ourselves with sub-second intervals, we detect readiness
/// much sooner — typically saving 5–8 seconds.
///
/// Returns without waiting for `limactl start` to finish. The
/// hostagent is already running as a daemon; the CLI process is
/// just waiting to print "READY".
fn wait_for_lima_ssh(
    name: &InstanceName,
    child: &mut std::process::Child,
    cfg: &CoopConfig,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();

    // Phase 1: Wait for the hostagent to assign an SSH port.
    let port = loop {
        if start.elapsed() >= timeout {
            bail!("Timeout waiting for Lima to assign SSH port");
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("limactl start failed");
            }
            return Ok(());
        }
        if let Some(p) = get_lima_ssh_port(name) {
            break p;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    tracing::debug!("Lima SSH port: {port}");

    // Phase 2: SSH auth probe until sshd accepts connections.
    // TCP connects to Lima's proxy port immediately, but the guest
    // sshd isn't ready for several seconds. Auth probing detects
    // true readiness so the caller's wait_until_ready is instant.
    let target = SshTarget {
        host: Hostname::new("127.0.0.1")?,
        port: std::num::NonZeroU16::new(port).context("Lima assigned SSH port 0")?,
        user: SshUser::new(crate::guest::GUEST_USER)?,
        key_path: cfg.ssh_key_path(),
    };
    let mut delay = Duration::from_millis(500);

    loop {
        if start.elapsed() >= timeout {
            bail!("Timeout waiting for SSH auth on port {port}");
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("limactl start failed");
            }
            return Ok(());
        }
        if target.exec_ok("true") {
            tracing::info!("SSH ready on port {port}");
            return Ok(());
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(1));
    }
}

/// Get the SSH local port for a Lima instance by reading `ssh.config`.
///
/// Lima's hostagent writes `~/.lima/<name>/ssh.config` once it has
/// assigned an SSH port. Reading the file is much cheaper than forking
/// `limactl list --json` on every poll tick, so phase-1 of
/// [`wait_for_lima_ssh`] uses it as the readiness signal.
///
/// Returns `None` if the file is missing or has no usable `Port` line —
/// both signal "not ready yet" and the caller will retry.
fn get_lima_ssh_port(name: &InstanceName) -> Option<u16> {
    let config_path = lima_home()
        .ok()?
        .join(lima_name_for(name))
        .join("ssh.config");
    let content = fs::read_to_string(&config_path).ok()?;
    parse_ssh_config_port(&content)
}

/// Extract the `Port` directive from a Lima-generated `ssh.config`.
///
/// SSH config directive names are case-insensitive; Lima emits a single
/// indented `Port <N>` line under one `Host` block.
fn parse_ssh_config_port(content: &str) -> Option<u16> {
    for line in content.lines() {
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let Some(key) = parts.next() else { continue };
        if !key.eq_ignore_ascii_case("Port") {
            continue;
        }
        let Some(value) = parts.next() else { continue };
        if let Ok(port) = value.trim().parse::<u16>()
            && port > 0
        {
            return Some(port);
        }
    }
    None
}

/// Wait for a child process in a background thread to prevent zombies.
fn reap_in_background(child: std::process::Child) {
    std::thread::spawn(move || {
        let mut child = child;
        match child.wait() {
            Ok(s) if !s.success() => {
                tracing::debug!("Background limactl exited with: {s}");
            }
            Err(e) => tracing::debug!("Failed to reap limactl: {e}"),
            _ => {}
        }
    });
}

// ── Lima helpers ──────────────────────────────────────────────

/// Read a raw disk image's file size and convert to GiB (rounded up).
fn base_image_size_gib(path: &Path) -> Option<GiB> {
    let bytes = fs::metadata(path).ok()?.len();
    let gib = bytes.div_ceil(1024 * 1024 * 1024);
    #[expect(clippy::cast_possible_truncation, reason = "disk images < 4 TiB")]
    GiB::new(gib as u32)
}

/// Extract the most useful error line(s) from Lima's stderr output.
///
/// Lima emits many info-level lines before the fatal error.  Pull out
/// lines containing "fatal" or "error" (case-insensitive) and extract
/// the `msg="..."` payload so the user sees the root cause.
fn extract_lima_error(stderr: &str) -> String {
    let messages: Vec<String> = stderr
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("fatal") || lower.contains("level=error")
        })
        .map(|line| extract_logfmt_msg(line).unwrap_or_else(|| line.to_string()))
        .collect();

    if messages.is_empty() {
        return String::new();
    }

    format!("Lima: {}\n", messages.join("\n       "))
}

/// Pull the `msg="..."` value out of a Lima logfmt line.
fn extract_logfmt_msg(line: &str) -> Option<String> {
    let start = line.find("msg=\"")? + 5;
    let rest = &line[start..];
    // Find the closing quote, handling escaped quotes
    let mut chars = rest.chars();
    let mut msg = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                msg.push(next);
            }
        } else if ch == '"' {
            break;
        } else {
            msg.push(ch);
        }
    }
    Some(msg)
}

fn lima_name(inst: &Instance) -> String {
    lima_name_for(&inst.name)
}

fn lima_name_for(name: &InstanceName) -> String {
    format!("{LIMA_PREFIX}{name}")
}

fn lima_home() -> Result<PathBuf> {
    // LIMA_HOME env var, or ~/.lima
    if let Ok(home) = std::env::var("LIMA_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".lima"))
}

/// Get the status string for a Lima instance ("Running", "Stopped", etc.).
fn lima_status(name: &InstanceName) -> Option<String> {
    let info = limactl_info(name).ok()?;
    info["status"].as_str().map(String::from)
}

/// Get full instance info from limactl for a coop instance.
fn limactl_info(name: &InstanceName) -> Result<serde_json::Value> {
    limactl_list_entry(&lima_name_for(name))
}

/// Find a `limactl list --json` entry by its Lima VM name (the
/// `coop-<instance>` form, or the special `coop-builder`).
fn limactl_list_entry(lima_name: &str) -> Result<serde_json::Value> {
    let output = Command::new("limactl")
        .args(["list", "--json"])
        .output()
        .context("Failed to run limactl list")?;

    if !output.status.success() {
        bail!("limactl list failed");
    }

    // limactl list --json outputs one JSON object per line (NDJSON)
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line).with_context(|| {
            format!(
                "Failed to parse limactl JSON output: \
                     {line}"
            )
        })?;
        if val["name"].as_str() == Some(lima_name) {
            return Ok(val);
        }
    }

    bail!("Lima instance '{lima_name}' not found in limactl list")
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    #[test]
    fn cloud_init_exit0_done_succeeds() {
        let result = check_cloud_init_output(Some(0), "status: done\n");
        assert!(result.is_ok());
    }

    #[test]
    fn cloud_init_exit2_degraded_done_succeeds() {
        // Exit 2 = "degraded done" (recoverable deprecation warnings)
        let result = check_cloud_init_output(Some(2), "status: done\n");
        assert!(result.is_ok());
    }

    #[test]
    fn cloud_init_exit1_error_fails() {
        let result = check_cloud_init_output(Some(1), "status: error\n");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Provision script failed"));
        assert!(msg.contains("status: error"));
    }

    #[test]
    fn cloud_init_exit0_with_status_error_fails() {
        // Exit 0 but stdout says "status: error" — trust the status text
        let result = check_cloud_init_output(Some(0), "status: error\n");
        assert!(result.is_err());
    }

    #[test]
    fn cloud_init_unknown_exit_code_with_done_succeeds() {
        // Unknown exit code but stdout says done without error
        let result = check_cloud_init_output(Some(42), "status: done\n");
        assert!(result.is_ok());
    }

    #[test]
    fn cloud_init_unknown_exit_code_without_done_fails() {
        let result = check_cloud_init_output(Some(42), "something unexpected\n");
        assert!(result.is_err());
    }

    #[test]
    fn cloud_init_no_exit_code_fails() {
        // Process killed by signal (no exit code)
        let result = check_cloud_init_output(None, "");
        assert!(result.is_err());
    }

    fn profile(name: &str, apt: &[&str], pre: Option<&str>, post: Option<&str>) -> ProfileDef {
        ProfileDef {
            name: name.into(),
            apt_packages: apt.iter().map(|s| (*s).into()).collect(),
            pre_install: pre.map(Into::into),
            post_install: post.map(Into::into),
            marketplaces: Vec::new(),
            plugins: Vec::new(),
        }
    }

    fn no_consecutive_concat(script: &str) {
        for (i, line) in script.lines().enumerate() {
            assert!(
                !line.contains("set -euo pipefail") || line.trim() == "set -euo pipefail",
                "line {}: 'set -euo pipefail' concatenated with other content: {line}",
                i + 1,
            );
        }
    }

    #[test]
    fn provision_script_post_install_without_trailing_newline() {
        let profiles = vec![profile("test", &["curl"], None, Some("echo done"))];
        let script = compose_provision_script("ssh-ed25519 AAAA test@test", &profiles);

        assert!(
            script.contains("echo done\n"),
            "post_install should end with newline",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn provision_script_pre_install_without_trailing_newline() {
        let profiles = vec![profile(
            "test",
            &[],
            Some("curl -fsSL https://example.com | bash"),
            None,
        )];
        let script = compose_provision_script("ssh-ed25519 AAAA test@test", &profiles);

        assert!(
            script.contains("| bash\n"),
            "pre_install should end with newline",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn provision_script_multiple_profiles_separated() {
        let profiles = vec![
            profile("a", &[], None, Some("echo a-done")),
            profile("b", &[], None, Some("echo b-done")),
        ];
        let script = compose_provision_script("ssh-ed25519 AAAA test@test", &profiles);

        assert!(script.contains("echo a-done\n"));
        assert!(script.contains("echo b-done\n"));
        for line in script.lines() {
            assert!(
                !(line.contains("a-done") && line.contains("b-done")),
                "two post_install scripts concatenated on one line",
            );
        }
    }

    #[test]
    fn provision_script_installs_codex() {
        let script = compose_provision_script("ssh-ed25519 AAAA test@test", &[]);

        assert!(
            script.contains("Installing Codex CLI"),
            "Lima provision script should install Codex CLI",
        );
    }

    // ── inject_mounts ───────────────────────────────────────────

    #[test]
    fn inject_mounts_single() {
        let yaml = "mountType: \"virtiofs\"\nmounts: []\n";
        let mounts = vec![crate::config::Mount {
            host_path: "/home/user/project".into(),
            guest_path: crate::workspace::default_workspace_path(),
        }];
        let result = super::inject_mounts(yaml, &mounts);
        assert!(
            result.contains("location: \"/home/user/project\""),
            "missing host path: {result}"
        );
        assert!(
            result.contains("mountPoint: \"/workspace\""),
            "missing guest path: {result}"
        );
        assert!(
            result.contains("writable: true"),
            "missing writable flag: {result}"
        );
        assert!(
            !result.contains("mounts: []"),
            "empty mounts not replaced: {result}"
        );
    }

    #[test]
    fn inject_mounts_multiple() {
        let yaml = "mounts: []\n";
        let mounts = vec![
            crate::config::Mount {
                host_path: "/a".into(),
                guest_path: crate::workspace::default_workspace_path(),
            },
            crate::config::Mount {
                host_path: "/b".into(),
                guest_path: crate::paths::GuestPath::absolute("/data").unwrap(),
            },
        ];
        let result = super::inject_mounts(yaml, &mounts);
        assert!(result.contains("location: \"/a\""), "missing /a: {result}");
        assert!(result.contains("location: \"/b\""), "missing /b: {result}");
        assert!(
            result.contains("mountPoint: \"/data\""),
            "missing /data: {result}"
        );
    }

    #[test]
    fn inject_mounts_preserves_surrounding_yaml() {
        let yaml = "vmType: \"vz\"\nmounts: []\ncontainerd:\n  system: false\n";
        let mounts = vec![crate::config::Mount {
            host_path: "/x".into(),
            guest_path: crate::paths::GuestPath::absolute("/y").unwrap(),
        }];
        let result = super::inject_mounts(yaml, &mounts);
        assert!(result.contains("vmType: \"vz\""), "lost vmType: {result}");
        assert!(result.contains("containerd:"), "lost containerd: {result}");
    }

    #[test]
    fn extract_lima_error_picks_fatal() {
        let stderr = r#"time="..." level=info msg="Starting the instance"
time="..." level=info msg="Attempting to download the image"
time="..." level=fatal msg="disk too small: 1G < 20G"
"#;
        let result = super::extract_lima_error(stderr);
        assert!(result.contains("disk too small"), "got: {result}");
        assert!(
            !result.contains("level="),
            "should strip logfmt prefix: {result}"
        );
    }

    #[test]
    fn extract_lima_error_empty_on_info_only() {
        let stderr = "time=\"...\" level=info msg=\"all good\"\n";
        assert!(super::extract_lima_error(stderr).is_empty());
    }

    #[test]
    fn extract_logfmt_msg_handles_escaped_quotes() {
        let line = r#"time="..." level=fatal msg="path \"foo\" not found""#;
        let msg = super::extract_logfmt_msg(line).unwrap();
        assert_eq!(msg, r#"path "foo" not found"#);
    }

    #[test]
    fn base_image_size_gib_exact() {
        let dir = tempfile::TempDir::new().unwrap();
        let img = dir.path().join("base.img");
        // Write exactly 20 GiB of zeros (sparse file)
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(20 * 1024 * 1024 * 1024).unwrap();
        let gib = super::base_image_size_gib(&img).unwrap();
        assert_eq!(gib.as_u32(), 20);
    }

    #[test]
    fn base_image_size_gib_rounds_up() {
        let dir = tempfile::TempDir::new().unwrap();
        let img = dir.path().join("base.img");
        // 20 GiB + 1 byte → should round up to 21
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(20 * 1024 * 1024 * 1024 + 1).unwrap();
        let gib = super::base_image_size_gib(&img).unwrap();
        assert_eq!(gib.as_u32(), 21);
    }

    #[test]
    fn base_image_size_gib_missing_file() {
        assert!(super::base_image_size_gib(Path::new("/nonexistent/base.img")).is_none());
    }

    #[test]
    fn base_image_size_gib_zero_length() {
        let dir = tempfile::TempDir::new().unwrap();
        let img = dir.path().join("empty.img");
        std::fs::File::create(&img).unwrap();
        // 0 bytes → GiB::new(0) returns None
        assert!(super::base_image_size_gib(&img).is_none());
    }

    // ── parse_ssh_config_port ───────────────────────────────────

    #[test]
    fn parse_ssh_config_port_typical_lima_output() {
        let content = "\
# This SSH config file can be passed to 'ssh -F'.
# This file is created by Lima.

Host lima-coop-myinst
  IdentityFile \"/Users/me/.lima/_config/user\"
  User myuser
  Hostname 127.0.0.1
  Port 60022
  StrictHostKeyChecking no
";
        assert_eq!(super::parse_ssh_config_port(content), Some(60022));
    }

    #[test]
    fn parse_ssh_config_port_missing_returns_none() {
        let content = "\
Host lima-coop-myinst
  User myuser
  Hostname 127.0.0.1
";
        assert!(super::parse_ssh_config_port(content).is_none());
    }

    #[test]
    fn parse_ssh_config_port_empty_returns_none() {
        assert!(super::parse_ssh_config_port("").is_none());
    }

    #[test]
    fn parse_ssh_config_port_zero_rejected() {
        // A zero port would be a degenerate value; treat as "not ready".
        let content = "Host h\n  Port 0\n";
        assert!(super::parse_ssh_config_port(content).is_none());
    }

    #[test]
    fn parse_ssh_config_port_case_insensitive() {
        let content = "  port 60022\n";
        assert_eq!(super::parse_ssh_config_port(content), Some(60022));
    }

    #[test]
    fn parse_ssh_config_port_ignores_unrelated_directives() {
        // Other directives that share a prefix or contain "Port" as a
        // substring must not be misread as the Port directive.
        let content = "\
Host h
  LocalForward 8080 127.0.0.1:8080
  PortForwarding yes
  Port 60022
";
        assert_eq!(super::parse_ssh_config_port(content), Some(60022));
    }

    #[test]
    fn parse_ssh_config_port_out_of_range_rejected() {
        let content = "Host h\n  Port 99999\n";
        assert!(super::parse_ssh_config_port(content).is_none());
    }
}
