use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cmd::Cmd;
use crate::config::{CoopConfig, ImageName, Instance};
use crate::guest::{
    BASE_PACKAGES, DOCKER_PACKAGES, GH_PACKAGES, ProfileDef, SCRIPT_CLAUDE_CODE, SCRIPT_CODEX,
    SCRIPT_DOCKER_REPO, SCRIPT_GH_REPO, resolve_profiles,
};
use crate::sha256_hash::Sha256Hash;

const S3_BUCKET: &str = "https://s3.amazonaws.com/spec.ccfc.min";
const S3_LIST: &str = "http://spec.ccfc.min.s3.amazonaws.com";
const GH_RELEASES: &str = "https://github.com/firecracker-microvm/firecracker/releases";

/// Monotonic version bumped when the base install script changes
/// in a way that requires rebuilding templates.
pub const TEMPLATE_VERSION: u32 = 1;

// ── Public types ──────────────────────────────────────────────

pub struct SetupOptions {
    pub skip_confirm: bool,
    pub rebuild: bool,
    pub profiles: Vec<ProfileDef>,
    pub extra_packages: Vec<String>,
    pub post_install: Option<PathBuf>,
    pub image: ImageName,
}

/// Persisted template configuration (profiles, packages, hashes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateConfig {
    pub version: u32,
    pub created: String,
    pub install_script_hash: Sha256Hash,
    pub profiles: Vec<String>,
    pub extra_packages: Vec<String>,
    pub post_install_hash: Option<Sha256Hash>,
    #[serde(default)]
    pub marketplaces: Vec<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
}

impl TemplateConfig {
    pub fn load_for(cfg: &CoopConfig, image: &ImageName) -> Result<Self> {
        let path = cfg.template_config_path_for(image);
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save_for(&self, cfg: &CoopConfig, image: &ImageName) -> Result<()> {
        let path = cfg.template_config_path_for(image);
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize template config")?;
        crate::fs_util::atomic_write_json(&path, &json)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        tracing::debug!("Wrote template config to {}", path.display());
        Ok(())
    }
}

// ── Public entry points ───────────────────────────────────────

/// Run the full setup: check prerequisites, install Firecracker,
/// fetch kernel, and build template rootfs.
pub fn run(cfg: &CoopConfig, opts: &SetupOptions) -> Result<()> {
    fs::create_dir_all(&cfg.data_dir).context("Failed to create data directory")?;

    check_host_requirements()?;
    install_system_packages(opts.skip_confirm)?;
    ensure_kvm_access(opts.skip_confirm)?;
    install_firecracker(cfg, opts.skip_confirm)?;
    fetch_kernel(cfg, opts.skip_confirm)?;
    build_or_check_template(cfg, opts)?;

    eprintln!(
        "\nSetup complete. All artifacts are in {}/",
        cfg.data_dir.display()
    );
    eprintln!("Run `coop start` to launch the VM.");
    Ok(())
}

/// Create an instance rootfs from the template.
///
/// Copies the template to the instance path, optionally resizing
/// if `disk_gib` is larger than the template.
pub fn create_instance(
    cfg: &CoopConfig,
    inst: &Instance,
    disk_gib: Option<crate::config::GiB>,
) -> Result<()> {
    let template = cfg.template_path_for(&inst.image);
    let rootfs = inst.rootfs_path();

    if !template.exists() {
        bail!(
            "No image '{}' found at {}.\n\
             Run `coop setup --image {}` first.",
            inst.image,
            template.display(),
            inst.image,
        );
    }

    // Remove existing instance rootfs if present (may be root-owned)
    if rootfs.exists() {
        Cmd::new("rm").arg("-f").arg(&rootfs).sudo().run()?;
    }

    tracing::info!("Creating instance '{}' from template", inst.name);
    Cmd::new("cp")
        .arg("--reflink=auto")
        .arg(&template)
        .arg(&rootfs)
        .sudo()
        .run()
        .context("Failed to copy template to instance")?;

    // Resize if requested and larger than template
    if let Some(requested) = disk_gib {
        let template_size = cfg.vm.template_size_gib;
        if requested < template_size {
            tracing::warn!(
                "Requested disk size ({requested} GiB) < template ({template_size} GiB), \
                 using template size"
            );
        } else if requested > template_size {
            tracing::info!("Resizing instance to {requested} GiB");
            let size_arg = format!("{requested}G");
            Cmd::new("truncate")
                .arg("-s")
                .arg(&size_arg)
                .arg(&rootfs)
                .sudo()
                .run()?;
            Cmd::new("e2fsck").arg("-fy").arg(&rootfs).sudo().run()?;
            Cmd::new("resize2fs").arg(&rootfs).sudo().run()?;
        }
    }

    // Patch the guest's network config with the correct IP for this instance
    patch_guest_network(inst)?;

    Ok(())
}

/// Resize a stopped Firecracker instance's rootfs.
pub fn resize_rootfs(inst: &Instance, new_size: crate::config::GiB) -> Result<()> {
    let rootfs = inst.rootfs_path();
    if !rootfs.exists() {
        bail!("Instance rootfs not found at {}", rootfs.display(),);
    }

    let current_bytes = std::fs::metadata(&rootfs)
        .with_context(|| format!("Failed to stat {}", rootfs.display()))?
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
        inst.name
    );
    let size_arg = format!("{new_size}G");
    Cmd::new("truncate")
        .arg("-s")
        .arg(&size_arg)
        .arg(&rootfs)
        .sudo()
        .run()?;
    Cmd::new("e2fsck").arg("-fy").arg(&rootfs).sudo().run()?;
    Cmd::new("resize2fs").arg(&rootfs).sudo().run()?;
    tracing::info!("Resize complete");
    Ok(())
}

// ── Mount guard (RAII) ─────────────────────────────────────────

/// RAII guard that unmounts a filesystem on drop, preventing leaked
/// mounts if an operation between mount and unmount fails.
struct MountGuard {
    mount_path: String,
    is_chroot: bool,
}

impl MountGuard {
    /// Simple loop mount: mount `rootfs` at `mount_path`.
    fn simple(rootfs: &str, mount_path: &str) -> Result<Self> {
        Cmd::new("mkdir").args(["-p", mount_path]).sudo().run()?;
        Cmd::new("mount")
            .args(["-o", "loop", rootfs, mount_path])
            .sudo()
            .run()
            .context("Failed to mount rootfs")?;
        Ok(Self {
            mount_path: mount_path.to_string(),
            is_chroot: false,
        })
    }

    /// Full chroot mount: rootfs + proc/sys/dev/devpts/tmp.
    fn chroot(rootfs: &str, mount_path: &str) -> Result<Self> {
        mount_chroot(rootfs, mount_path)?;
        Ok(Self {
            mount_path: mount_path.to_string(),
            is_chroot: true,
        })
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if self.is_chroot {
            unmount_chroot(&self.mount_path);
        } else {
            if let Err(e) = Cmd::new("umount").arg(&self.mount_path).sudo().run() {
                tracing::warn!("Failed to unmount {} (non-fatal): {e}", self.mount_path);
            }
            if let Err(e) = Cmd::new("rmdir").arg(&self.mount_path).sudo().run() {
                tracing::warn!(
                    "Failed to remove mount dir {} (non-fatal): {e}",
                    self.mount_path
                );
            }
        }
    }
}

/// Mount the instance rootfs and rewrite the systemd-networkd config
/// with the instance's unique guest IP.
fn patch_guest_network(inst: &Instance) -> Result<()> {
    let rootfs_str = inst.rootfs_path().display().to_string();
    let mount_dir = inst.dir.join("rootfs-mount");
    let mount_str = mount_dir.display().to_string();

    tracing::info!("Patching guest network: IP={}", inst.guest_ip());

    let _guard = MountGuard::simple(&rootfs_str, &mount_str)?;

    let network_config = format!(
        "[Match]\nName=eth0\n\n[Network]\nAddress={}/24\nGateway=172.16.0.1\nDNS=8.8.8.8\nDNS=8.8.4.4\n",
        inst.guest_ip()
    );
    let netfile = format!("{mount_str}/etc/systemd/network/10-eth0.network");

    // Write via tee to handle root-owned filesystem
    Cmd::new("tee")
        .arg(&netfile)
        .sudo()
        .stdin_write(network_config.as_bytes())
        .context("Failed to write guest network config")?;

    // Also patch hostname to include instance name
    let hostname = format!("claude-{}\n", inst.name);
    let hostfile = format!("{mount_str}/etc/hostname");
    Cmd::new("tee")
        .arg(&hostfile)
        .sudo()
        .stdin_write(hostname.as_bytes())
        .context("Failed to write hostname")?;

    // _guard dropped here → unmount + rmdir
    Ok(())
}

// ── Template management ───────────────────────────────────────

fn build_or_check_template(cfg: &CoopConfig, opts: &SetupOptions) -> Result<()> {
    let image = &opts.image;
    let template = cfg.template_path_for(image);
    let (profiles, extra_packages) = resolve_template_config(cfg, opts)?;

    // Compose recipe and compute hashes
    let recipe = compose_recipe(&profiles, &extra_packages);
    let script_hash = Sha256Hash::of(&recipe);
    let post_install_content = load_post_install(opts.post_install.as_ref())?;
    let post_install_hash = post_install_content.as_ref().map(Sha256Hash::of);

    // Check existing template — rebuild automatically if stale
    if template.exists() && !opts.rebuild {
        if !needs_rebuild(cfg, image, script_hash, post_install_hash) {
            step("Template rootfs: up to date");
            return Ok(());
        }
        step("Template rootfs is stale — rebuilding");
    }

    // Build new template to a staging path, then swap into place.
    // The old template is never touched until the new one is ready,
    // so a crash mid-build leaves the old image intact.
    let staging = template.with_extension("ext4.new");

    // Clean up leftover staging artifact from a previous failed build
    if staging.exists()
        && let Err(e) = Cmd::new("rm").arg("-f").arg(&staging).sudo().run()
    {
        tracing::debug!("Failed to remove stale staging image (non-fatal): {e}");
    }

    let result = build_template(
        cfg,
        opts,
        image,
        &profiles,
        &extra_packages,
        &recipe,
        script_hash,
        post_install_content.as_deref(),
        post_install_hash,
        &staging,
    );

    if let Err(e) = result {
        // Clean up failed staging image
        if let Err(rm_err) = Cmd::new("rm").arg("-f").arg(&staging).sudo().run() {
            tracing::debug!("Failed to remove staging image (non-fatal): {rm_err}");
        }
        return Err(e);
    }

    // Swap staging into place — old template is replaced atomically
    Cmd::new("mv")
        .arg(&staging)
        .arg(&template)
        .sudo()
        .run()
        .context("Failed to swap staging template into place")?;

    // Write template config only after image swap succeeds.
    // If we crash between swap and config write, staleness
    // detection triggers a rebuild on next run (safe).
    let template_config = TemplateConfig {
        version: TEMPLATE_VERSION,
        created: utc_timestamp(),
        install_script_hash: script_hash,
        profiles: profile_names(&profiles),
        extra_packages,
        post_install_hash,
        marketplaces: Vec::new(),
        plugins: Vec::new(),
    };
    template_config.save_for(cfg, image)?;

    Ok(())
}

fn profile_names(profiles: &[ProfileDef]) -> Vec<String> {
    profiles.iter().map(|p| p.name.clone()).collect()
}

/// Returns `true` if the template needs rebuilding.
///
/// A rebuild is needed when the config file is missing (orphaned
/// template image) or the install-script hash has changed.
fn needs_rebuild(
    cfg: &CoopConfig,
    image: &ImageName,
    current_hash: Sha256Hash,
    current_post_hash: Option<Sha256Hash>,
) -> bool {
    let Ok(existing) = TemplateConfig::load_for(cfg, image) else {
        tracing::info!("Template config missing — treating template as stale");
        return true;
    };
    existing.install_script_hash != current_hash || existing.post_install_hash != current_post_hash
}

#[expect(clippy::too_many_arguments, reason = "template build orchestration")]
fn build_template(
    cfg: &CoopConfig,
    opts: &SetupOptions,
    image: &ImageName,
    profiles: &[ProfileDef],
    extra_packages: &[String],
    recipe: &str,
    script_hash: Sha256Hash,
    post_install_content: Option<&str>,
    post_install_hash: Option<Sha256Hash>,
    output_path: &Path,
) -> Result<()> {
    step("Building template rootfs");

    let rootfs_url = discover_ci_rootfs(cfg)?;
    let rootfs_name = rootfs_url.rsplit('/').next().unwrap_or("rootfs");

    eprintln!("  Found rootfs: {rootfs_name}");
    eprintln!("  URL: {rootfs_url}");
    if !profiles.is_empty() {
        eprintln!("  Profiles: {}", profile_names(profiles).join(", "));
    }
    if !extra_packages.is_empty() {
        eprintln!("  Extra packages: {}", extra_packages.join(", "));
    }
    if post_install_content.is_some() {
        eprintln!("  Post-install script: yes");
    }
    eprintln!();
    eprintln!("  This will:");
    eprintln!("    1. Download the CI squashfs image");
    eprintln!("    2. Unpack it and generate SSH keys for access");
    eprintln!(
        "    3. Create a {} GiB ext4 template image",
        cfg.vm.template_size_gib
    );
    eprintln!("    4. Install Docker, Claude Code, Codex, and profile packages");
    eprintln!("  Image: {image}");
    eprintln!("  Output: {}", cfg.template_path_for(image).display());
    eprintln!();
    eprintln!("  NOTE: Steps 2-4 require root (sudo) for filesystem operations.");

    if !confirm("Proceed with template build", opts.skip_confirm)? {
        bail!("Template build cancelled");
    }

    download_and_unpack_rootfs(cfg, &rootfs_url, rootfs_name)?;
    crate::signal::check_shutdown()?;

    inject_ssh_keys(cfg)?;
    create_ext4_image(cfg, output_path)?;
    crate::signal::check_shutdown()?;

    // Compose and execute install script
    let marker_config = TemplateConfig {
        version: TEMPLATE_VERSION,
        created: utc_timestamp(),
        install_script_hash: script_hash,
        profiles: profile_names(profiles),
        extra_packages: extra_packages.to_vec(),
        post_install_hash,
        marketplaces: Vec::new(),
        plugins: Vec::new(),
    };
    let marker_json = serde_json::to_string_pretty(&marker_config)
        .context("Failed to serialize version marker")?;
    let full_script = compose_full_script(recipe, &marker_json, post_install_content);
    install_guest_packages(cfg, output_path, &full_script)?;

    // Clean up intermediate files
    eprintln!("  Cleaning up...");
    let unpack_dir = cfg.data_dir.join("squashfs-root");
    if let Err(e) = Cmd::new("rm").arg("-rf").arg(&unpack_dir).sudo().run() {
        tracing::debug!("Failed to clean up squashfs-root (non-fatal): {e}");
    }
    if let Err(e) = fs::remove_file(cfg.data_dir.join(rootfs_name)) {
        tracing::debug!("Failed to clean up rootfs download (non-fatal): {e}");
    }

    eprintln!(
        "  Template ready at {}",
        cfg.template_path_for(image).display()
    );
    Ok(())
}

/// Resolve effective profiles and extra packages.
///
/// If CLI flags provide profiles or packages, use those.
/// Otherwise, reuse the previous template config (resolving the persisted
/// profile names against the current builtin/custom set).
fn resolve_template_config(
    cfg: &CoopConfig,
    opts: &SetupOptions,
) -> Result<(Vec<ProfileDef>, Vec<String>)> {
    if !opts.profiles.is_empty() || !opts.extra_packages.is_empty() {
        return Ok((opts.profiles.clone(), opts.extra_packages.clone()));
    }

    if let Ok(existing) = TemplateConfig::load_for(cfg, &opts.image) {
        let profiles = resolve_profiles(&existing.profiles, &cfg.profiles)?;
        return Ok((profiles, existing.extra_packages));
    }

    Ok((Vec::new(), Vec::new()))
}

fn load_post_install(path: Option<&PathBuf>) -> Result<Option<String>> {
    match path {
        Some(p) => {
            let content = fs::read_to_string(p)
                .with_context(|| format!("Failed to read post-install script: {}", p.display()))?;
            Ok(Some(content))
        }
        None => Ok(None),
    }
}

// ── Script composition ────────────────────────────────────────

/// Compose the install "recipe" — base + profiles + extras.
///
/// This portion is hashed for staleness detection.
/// Does NOT include version marker, post-install script, or cleanup.
fn compose_recipe(profiles: &[ProfileDef], extra_packages: &[String]) -> String {
    // Collect and deduplicate profile + extra apt packages
    let mut extra_apt: Vec<&str> = profiles
        .iter()
        .flat_map(|def| &def.apt_packages)
        .map(String::as_str)
        .chain(extra_packages.iter().map(String::as_str))
        .collect();
    extra_apt.sort_unstable();
    extra_apt.dedup();

    let pre_installs: Vec<&str> = profiles
        .iter()
        .filter_map(|d| d.pre_install.as_deref())
        .collect();
    let post_installs: Vec<&str> = profiles
        .iter()
        .filter_map(|d| d.post_install.as_deref())
        .collect();

    let mut s = String::with_capacity(8192);

    // Preamble (chroot workarounds + initial apt-get update)
    s.push_str(SCRIPT_PREAMBLE);

    // Install base packages first (provides curl/gpg for repo setup)
    s.push_str("echo '  [guest] Installing core tools...'\n");
    s.push_str(
        "apt-get install -y -qq \"${APT_OPTS[@]}\" \
         --no-install-recommends \\\n    ",
    );
    s.push_str(&BASE_PACKAGES.join(" "));
    s.push_str(" < /dev/null\n\n");

    // Profile pre-install (repo additions like NodeSource)
    for pre in &pre_installs {
        s.push('\n');
        s.push_str(pre);
        if !pre.ends_with('\n') {
            s.push('\n');
        }
    }

    // Add third-party repos (curl/gpg now available from base)
    s.push('\n');
    s.push_str(SCRIPT_GH_REPO);
    s.push('\n');
    s.push_str(SCRIPT_DOCKER_REPO);

    // Single update covering all third-party repos
    s.push_str(
        "\necho '  [guest] Updating package lists \
         (third-party repos)...'\n",
    );
    s.push_str("apt-get update -qq\n\n");

    // Single install for all third-party + profile packages
    let third_party: Vec<&str> = GH_PACKAGES
        .iter()
        .chain(DOCKER_PACKAGES)
        .chain(&extra_apt)
        .copied()
        .collect();

    if !third_party.is_empty() {
        s.push_str("echo '  [guest] Installing third-party packages...'\n");
        s.push_str(
            "apt-get install -y -qq \"${APT_OPTS[@]}\" \
             --no-install-recommends \\\n    ",
        );
        s.push_str(&third_party.join(" "));
        s.push_str(" < /dev/null\n");
    }

    // Profile post-install scripts (rustup, etc.)
    for post in &post_installs {
        s.push('\n');
        s.push_str(post);
        if !post.ends_with('\n') {
            s.push('\n');
        }
    }

    // Test hook: inject a provision failure to exercise error detection.
    // Only activates when COOP_TEST_INJECT_PROVISION_FAILURE is set.
    if std::env::var("COOP_TEST_INJECT_PROVISION_FAILURE").is_ok() {
        s.push_str("\necho '  [guest] INJECTED FAILURE FOR TESTING'\n");
        s.push_str("exit 1\n");
    }

    // Guest config configures the ubuntu user — must come before claude-code.
    s.push_str(SCRIPT_GUEST_CONFIG);
    // Direct binary download (runs as root in chroot, installs for ubuntu user).
    s.push_str(SCRIPT_CLAUDE_CODE);
    // Codex installs as a standalone binary under /usr/local/bin.
    s.push_str(SCRIPT_CODEX);

    s
}

/// Compose the full chroot script from recipe + marker + post-install + cleanup.
fn compose_full_script(
    recipe: &str,
    version_marker_json: &str,
    post_install: Option<&str>,
) -> String {
    let mut s = String::with_capacity(recipe.len() + 1024);
    s.push_str(recipe);

    if let Some(pi) = post_install {
        s.push_str("\necho '  [guest] Running post-install script...'\n");
        s.push_str(pi);
        s.push('\n');
    }

    s.push_str("\necho '  [guest] Writing version marker...'\n");
    s.push_str("cat > /etc/coop-template <<'MARKEREOF'\n");
    s.push_str(version_marker_json);
    s.push_str("\nMARKEREOF\n");

    s.push_str(SCRIPT_CLEANUP);

    s
}

// ── Script segments (embedded from scripts/guest/ at compile time) ─────

const SCRIPT_PREAMBLE: &str = include_str!("../scripts/guest/preamble.sh");
const SCRIPT_GUEST_CONFIG: &str = include_str!("../scripts/guest/guest-config.sh");
const SCRIPT_CLEANUP: &str = include_str!("../scripts/guest/cleanup.sh");

// ── Infrastructure (prerequisites, FC, kernel) ────────────────

fn check_host_requirements() -> Result<()> {
    step("Checking host requirements");

    if !Path::new("/dev/kvm").exists() {
        bail!(
            "/dev/kvm not found. Firecracker requires KVM.\n\
             Make sure you're on a Linux host with KVM enabled \
             (check `lsmod | grep kvm`)."
        );
    }
    eprintln!("  /dev/kvm: present");

    let arch = Architecture::current()?;
    eprintln!("  Architecture: {arch}");

    require_command("curl", "Required for downloading artifacts")?;

    Ok(())
}

fn ensure_kvm_access(skip_confirm: bool) -> Result<()> {
    let kvm = Path::new("/dev/kvm");
    if has_kvm_access(kvm) {
        step("KVM access: OK");
        return Ok(());
    }

    step("Granting KVM access");
    eprintln!("  /dev/kvm exists but you don't have read/write access.");
    fix_kvm_access(skip_confirm)?;
    eprintln!("  /dev/kvm: OK");
    Ok(())
}

fn has_kvm_access(kvm: &Path) -> bool {
    fs::metadata(kvm)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            let mode = m.mode();
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            if uid == m.uid() {
                mode & 0o600 == 0o600
            } else if gid == m.gid() {
                mode & 0o060 == 0o060
            } else {
                mode & 0o006 == 0o006
            }
        })
        .unwrap_or(false)
}

fn fix_kvm_access(skip_confirm: bool) -> Result<()> {
    // Prefer setfacl (immediate, no re-login needed)
    if which("setfacl").is_some() {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        let cmd = format!("sudo setfacl -m u:{user}:rw /dev/kvm");
        if !confirm(&cmd, skip_confirm)? {
            bail!("Cannot proceed without /dev/kvm access");
        }
        Cmd::new("setfacl")
            .args(["-m", &format!("u:{user}:rw"), "/dev/kvm"])
            .sudo()
            .run()?;
        return Ok(());
    }

    // Fall back to adding user to kvm group
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let cmd = format!("sudo usermod -aG kvm {user}");
    eprintln!("  setfacl not available, will add you to the kvm group instead.");
    eprintln!("  NOTE: group changes require re-login to take effect.");
    if !confirm(&cmd, skip_confirm)? {
        bail!("Cannot proceed without /dev/kvm access");
    }
    Cmd::new("usermod")
        .args(["-aG", "kvm", &user])
        .sudo()
        .run()?;

    if !has_kvm_access(Path::new("/dev/kvm")) {
        bail!(
            "Added {user} to kvm group, but it won't take effect until you re-login.\n\
             Run `newgrp kvm` or log out and back in, then retry."
        );
    }

    Ok(())
}

fn install_system_packages(skip_confirm: bool) -> Result<()> {
    let mut missing = Vec::new();

    if which("setfacl").is_none() {
        missing.push("acl");
    }
    if which("debootstrap").is_none() {
        missing.push("debootstrap");
    }
    if which("unsquashfs").is_none() {
        missing.push("squashfs-tools");
    }
    if which("mkfs.ext4").is_none() {
        missing.push("e2fsprogs");
    }
    if which("ssh").is_none() {
        missing.push("openssh-client");
    }
    if which("rsync").is_none() {
        missing.push("rsync");
    }

    if missing.is_empty() {
        step("System packages: all present");
        return Ok(());
    }

    step("Missing system packages");
    let pkg_list = missing.join(" ");
    eprintln!("  Need to install: {pkg_list}");

    let cmd = format!("sudo apt-get install -y {pkg_list}");
    if !confirm(&cmd, skip_confirm)? {
        bail!("Cannot proceed without required packages: {pkg_list}");
    }

    Cmd::new("apt-get")
        .args(["install", "-y"])
        .args(&missing)
        .sudo()
        .run()
        .context("Package installation failed")?;

    Ok(())
}

fn install_firecracker(cfg: &CoopConfig, skip_confirm: bool) -> Result<()> {
    let fc_path = cfg.data_dir.join("firecracker");
    if fc_path.exists() {
        step(&format!(
            "Firecracker binary: already at {}",
            fc_path.display()
        ));
        return Ok(());
    }

    step("Installing Firecracker");

    let arch = Architecture::current()?;

    eprintln!("  Discovering latest Firecracker release...");
    let latest = discover_latest_fc_version()?;
    eprintln!("  Latest version: {latest}");

    let tarball_name = format!("firecracker-{latest}-{arch}.tgz");
    let url = format!("{GH_RELEASES}/download/{latest}/{tarball_name}");
    let tarball_path = cfg.data_dir.join(&tarball_name);

    eprintln!("  Download URL: {url}");
    eprintln!("  Install to:   {}", fc_path.display());

    if !confirm(
        &format!("Download and install Firecracker {latest}"),
        skip_confirm,
    )? {
        bail!("Firecracker installation cancelled");
    }

    Cmd::new("curl")
        .args(["-fSL", "-o"])
        .arg(&tarball_path)
        .arg(&url)
        .run()
        .context("Failed to download Firecracker tarball")?;

    let strip_prefix = format!("release-{latest}-{arch}/firecracker-{latest}-{arch}");
    Cmd::new("tar")
        .args(["-xzf"])
        .arg(&tarball_path)
        .arg("-C")
        .arg(&cfg.data_dir)
        .arg("--strip-components=1")
        .arg(&strip_prefix)
        .run()
        .context("Failed to extract Firecracker binary")?;

    let extracted = cfg.data_dir.join(format!("firecracker-{latest}-{arch}"));
    if extracted.exists() {
        fs::rename(&extracted, &fc_path).context("Failed to rename Firecracker binary")?;
    }

    // Also extract the jailer
    let jailer_prefix = format!("release-{latest}-{arch}/jailer-{latest}-{arch}");
    if let Err(e) = Cmd::new("tar")
        .args(["-xzf"])
        .arg(&tarball_path)
        .arg("-C")
        .arg(&cfg.data_dir)
        .arg("--strip-components=1")
        .arg(&jailer_prefix)
        .run()
    {
        tracing::debug!("Failed to extract jailer from tarball (non-fatal): {e}");
    }
    let jailer_extracted = cfg.data_dir.join(format!("jailer-{latest}-{arch}"));
    let jailer_target = cfg.data_dir.join("jailer");
    if jailer_extracted.exists()
        && let Err(e) = fs::rename(&jailer_extracted, &jailer_target)
    {
        tracing::debug!("Failed to rename jailer binary (non-fatal): {e}");
    }

    Cmd::new("chmod")
        .arg("+x")
        .arg(&fc_path)
        .run()
        .context("Failed to make Firecracker executable")?;
    if jailer_target.exists()
        && let Err(e) = Cmd::new("chmod").arg("+x").arg(&jailer_target).run()
    {
        tracing::debug!("Failed to make jailer executable (non-fatal): {e}");
    }

    if let Err(e) = fs::remove_file(&tarball_path) {
        tracing::debug!("Failed to remove tarball (non-fatal): {e}");
    }

    let output = Command::new(&fc_path)
        .arg("--version")
        .output()
        .context("Failed to run Firecracker")?;
    let version = String::from_utf8_lossy(&output.stdout);
    eprintln!("  Installed: {}", version.trim());

    Ok(())
}

fn fetch_kernel(cfg: &CoopConfig, skip_confirm: bool) -> Result<()> {
    if cfg.vm.kernel_path.exists() {
        step(&format!(
            "Kernel: already at {}",
            cfg.vm.kernel_path.display()
        ));
        return Ok(());
    }

    step("Fetching guest kernel");

    let arch = Architecture::current()?;

    let latest = discover_latest_fc_version()?;
    let ci_version = derive_ci_version(&latest);

    eprintln!("  Looking for kernels in CI bucket (version {ci_version})...");
    let prefix = format!("firecracker-ci/{ci_version}/{arch}/vmlinux-");
    let list_url = format!("{S3_LIST}/?prefix={prefix}&list-type=2");

    let output = Command::new("curl")
        .args(["-sf", &list_url])
        .output()
        .context("Failed to list kernel artifacts from S3")?;
    let body = String::from_utf8_lossy(&output.stdout);

    let keys: Vec<&str> = body
        .split("<Key>")
        .skip(1)
        .filter_map(|s| s.split("</Key>").next())
        .filter(|k| k.contains("vmlinux-") && !k.ends_with(".config"))
        .collect();

    if keys.is_empty() {
        bail!(
            "No kernels found in Firecracker CI bucket for {ci_version}/{arch}.\n\
             The S3 listing URL was: {list_url}\n\
             You may need to build a kernel manually."
        );
    }

    let kernel_key = keys.into_iter().max().context("No kernel keys found")?;
    let kernel_url = format!("{S3_BUCKET}/{kernel_key}");
    let kernel_name = kernel_key.rsplit('/').next().unwrap_or("vmlinux");

    eprintln!("  Found kernel: {kernel_name}");
    eprintln!("  URL: {kernel_url}");
    eprintln!("  Output: {}", cfg.vm.kernel_path.display());

    if !confirm(&format!("Download kernel {kernel_name}"), skip_confirm)? {
        bail!("Kernel download cancelled");
    }

    if let Some(parent) = cfg.vm.kernel_path.parent() {
        fs::create_dir_all(parent)?;
    }

    Cmd::new("curl")
        .args(["-fSL", "-o"])
        .arg(&cfg.vm.kernel_path)
        .arg(&kernel_url)
        .run()
        .context("Failed to download kernel")?;

    eprintln!("  Kernel downloaded");
    Ok(())
}

// ── Rootfs build helpers ──────────────────────────────────────

fn discover_ci_rootfs(cfg: &CoopConfig) -> Result<String> {
    let arch = Architecture::current()?;
    let latest = discover_latest_fc_version()?;
    let ci_version = derive_ci_version(&latest);

    eprintln!("  Looking for rootfs in CI bucket (version {ci_version})...");
    let prefix = format!("firecracker-ci/{ci_version}/{arch}/ubuntu-");
    let list_url = format!("{S3_LIST}/?prefix={prefix}&list-type=2");

    let output = Command::new("curl")
        .args(["-sf", &list_url])
        .output()
        .context("Failed to list rootfs artifacts from S3")?;
    let body = String::from_utf8_lossy(&output.stdout);

    let keys: Vec<&str> = body
        .split("<Key>")
        .skip(1)
        .filter_map(|s| s.split("</Key>").next())
        .filter(|k| k.ends_with(".squashfs"))
        .collect();

    if keys.is_empty() {
        bail!(
            "No rootfs found in Firecracker CI bucket for {ci_version}/{arch}.\n\
             You can build one manually with:\n\
             \n\
               sudo bash scripts/build-rootfs.sh {}\n",
            cfg.template_path().display()
        );
    }

    let rootfs_key = keys.into_iter().max().context("No rootfs keys found")?;
    Ok(format!("{S3_BUCKET}/{rootfs_key}"))
}

fn download_and_unpack_rootfs(cfg: &CoopConfig, rootfs_url: &str, rootfs_name: &str) -> Result<()> {
    let squashfs_path = cfg.data_dir.join(rootfs_name);
    let unpack_dir = cfg.data_dir.join("squashfs-root");

    eprintln!("  Downloading rootfs...");
    Cmd::new("curl")
        .args(["-fSL", "-o"])
        .arg(&squashfs_path)
        .arg(rootfs_url)
        .run()
        .context("Failed to download rootfs")?;

    eprintln!("  Unpacking squashfs...");
    if let Err(e) = Cmd::new("rm").arg("-rf").arg(&unpack_dir).sudo().run() {
        tracing::debug!("Failed to remove old unpack dir (non-fatal): {e}");
    }
    Cmd::new("unsquashfs")
        .arg("-d")
        .arg(&unpack_dir)
        .arg(&squashfs_path)
        .sudo()
        .run()
        .context("Failed to unpack squashfs image")?;

    Ok(())
}

fn inject_ssh_keys(cfg: &CoopConfig) -> Result<()> {
    let ssh_key_path = cfg.ssh_key_path();
    if !ssh_key_path.exists() {
        eprintln!("  Generating SSH key pair for VM access...");
        Cmd::new("ssh-keygen")
            .args(["-t", "ed25519", "-f"])
            .arg(&ssh_key_path)
            .args(["-N", "", "-q"])
            .run()
            .context("Failed to generate SSH key")?;
    }

    let pubkey = fs::read_to_string(ssh_key_path.with_extension("pub"))
        .context("Failed to read generated SSH public key")?;

    let unpack_dir = cfg.data_dir.join("squashfs-root");
    let auth_keys_dir = unpack_dir.join("root/.ssh");
    Cmd::new("mkdir")
        .arg("-p")
        .arg(&auth_keys_dir)
        .sudo()
        .run()?;

    let auth_keys_path = auth_keys_dir.join("authorized_keys");
    Cmd::new("tee")
        .arg(&auth_keys_path)
        .sudo()
        .stdin_write(pubkey.as_bytes())
        .context("Failed to write authorized_keys")?;

    Ok(())
}

/// Create the ext4 template image from the unpacked squashfs.
fn create_ext4_image(cfg: &CoopConfig, output_path: &Path) -> Result<()> {
    eprintln!(
        "  Creating ext4 template image ({} GiB)...",
        cfg.vm.template_size_gib
    );
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let unpack_dir = cfg.data_dir.join("squashfs-root");

    let size_arg = format!("{}G", cfg.vm.template_size_gib);
    Cmd::new("truncate")
        .arg("-s")
        .arg(&size_arg)
        .arg(output_path)
        .run()?;
    Cmd::new("mkfs.ext4")
        .arg("-d")
        .arg(&unpack_dir)
        .arg("-F")
        .arg(output_path)
        .sudo()
        .run()
        .context("Failed to create ext4 template image")?;
    Cmd::new("e2fsck")
        .arg("-fn")
        .arg(output_path)
        .sudo()
        .run()
        .context("Template verification failed")?;

    Ok(())
}

/// Mount the template rootfs and run the install script in a chroot.
fn install_guest_packages(cfg: &CoopConfig, image_path: &Path, script: &str) -> Result<()> {
    eprintln!("  Installing guest packages (Docker, Claude Code, Codex)...");
    eprintln!("  This requires sudo and may take several minutes.");

    let template_str = image_path.display().to_string();
    let mount_dir = cfg.data_dir.join("rootfs-mount");
    let mount_str = mount_dir.display().to_string();

    let _guard = MountGuard::chroot(&template_str, &mount_str)?;

    Cmd::new("chroot")
        .args([&mount_str, "bash", "-c", script])
        .sudo()
        .run()
        .context("Guest package installation failed")?;

    verify_chroot_binaries(&mount_str)?;

    // _guard dropped here → unmount_chroot runs regardless of status
    Ok(())
}

/// Verify critical binaries exist in the chroot after provisioning.
///
/// Uses `symlink_metadata` (lstat) instead of `exists` (stat) because
/// the Claude binary is a symlink with an absolute target
/// (`/home/ubuntu/.local/share/claude/versions/X.Y.Z`). When checked
/// from outside the chroot, `exists()` follows the symlink to the
/// host filesystem where the target doesn't exist. A symlink being
/// present is sufficient — it will resolve correctly inside the guest.
fn verify_chroot_binaries(mount_str: &str) -> Result<()> {
    use crate::guest::REQUIRED_GUEST_BINARIES;

    eprintln!("  Verifying installed binaries...");
    let mut missing = Vec::new();

    for &path in REQUIRED_GUEST_BINARIES {
        let full_path = format!("{mount_str}{path}");
        let exists = std::fs::symlink_metadata(&full_path).is_ok();
        if !exists {
            missing.push(path);
        }
    }

    if !missing.is_empty() {
        bail!(
            "Golden image is missing required binaries: {}\n\
             The install script completed but these tools were \
             not installed correctly.",
            missing.join(", "),
        );
    }

    Ok(())
}

fn mount_chroot(rootfs: &str, mount_str: &str) -> Result<()> {
    Cmd::new("mkdir").args(["-p", mount_str]).sudo().run()?;
    Cmd::new("mount")
        .args(["-o", "loop", rootfs, mount_str])
        .sudo()
        .run()?;
    Cmd::new("mount")
        .args(["-t", "proc", "proc", &format!("{mount_str}/proc")])
        .sudo()
        .run()?;
    Cmd::new("mount")
        .args(["-t", "sysfs", "sys", &format!("{mount_str}/sys")])
        .sudo()
        .run()?;
    Cmd::new("mount")
        .args(["--bind", "/dev", &format!("{mount_str}/dev")])
        .sudo()
        .run()?;
    Cmd::new("mount")
        .args(["-t", "devpts", "devpts", &format!("{mount_str}/dev/pts")])
        .sudo()
        .run()?;
    Cmd::new("mount")
        .args(["-t", "tmpfs", "tmpfs", &format!("{mount_str}/tmp")])
        .sudo()
        .run()?;
    if let Err(e) = Cmd::new("cp")
        .args(["/etc/resolv.conf", &format!("{mount_str}/etc/resolv.conf")])
        .sudo()
        .run()
    {
        tracing::debug!("Failed to copy resolv.conf into chroot (non-fatal): {e}");
    }
    Ok(())
}

fn unmount_chroot(mount_str: &str) {
    let mounts = [
        format!("{mount_str}/tmp"),
        format!("{mount_str}/dev/pts"),
        format!("{mount_str}/dev"),
        format!("{mount_str}/sys"),
        format!("{mount_str}/proc"),
        mount_str.to_string(),
    ];
    for mount in &mounts {
        if let Err(e) = Cmd::new("umount").arg(mount).sudo().run() {
            tracing::warn!("Failed to unmount {mount} (non-fatal): {e}");
        }
    }
    if let Err(e) = Cmd::new("rmdir").arg(mount_str).sudo().run() {
        tracing::warn!("Failed to remove mount dir {mount_str} (non-fatal): {e}");
    }
}

// ── General helpers ───────────────────────────────────────────

pub fn utc_timestamp() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn discover_latest_fc_version() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-fsSLI",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            &format!("{GH_RELEASES}/latest"),
        ])
        .output()
        .context("Failed to discover latest Firecracker version")?;

    let url = String::from_utf8_lossy(&output.stdout);
    let version = url
        .trim()
        .rsplit('/')
        .next()
        .context("Could not parse version from redirect URL")?
        .to_string();

    if !version.starts_with('v') {
        bail!("Unexpected version format: {version}");
    }

    Ok(version)
}

/// Derive the CI bucket version from a release version.
/// e.g., "v1.15.0" -> "v1.15", "v1.14.2" -> "v1.14"
fn derive_ci_version(release_version: &str) -> String {
    let parts: Vec<&str> = release_version.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[0], parts[1])
    } else {
        release_version.to_string()
    }
}

/// Host architectures supported by the Firecracker setup path.
///
/// A closed enum lets `match` arms stay exhaustive and prevents string typos
/// from silently producing broken URLs or download paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    Aarch64,
}

impl Architecture {
    /// Detect the architecture this binary was built for.
    ///
    /// Reads `std::env::consts::ARCH`, which is fixed at compile time and
    /// matches the kernel's `uname -m` output for the targets coop supports.
    pub fn current() -> Result<Self> {
        Self::from_consts_str(std::env::consts::ARCH)
    }

    fn from_consts_str(s: &str) -> Result<Self> {
        match s {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            other => bail!("Unsupported architecture: {other}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn which(program: &str) -> Option<PathBuf> {
    Command::new("which")
        .arg(program)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
}

fn require_command(name: &str, purpose: &str) -> Result<()> {
    if which(name).is_some() {
        eprintln!("  {name}: OK");
        Ok(())
    } else {
        bail!("{name} not found. {purpose}.");
    }
}

fn step(msg: &str) {
    eprintln!("\n=> {msg}");
}

fn confirm(action: &str, skip: bool) -> Result<bool> {
    if skip {
        return Ok(true);
    }
    eprint!("  {action}? [Y/n] ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(trimmed.is_empty() || trimmed == "y" || trimmed == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn compose_recipe_no_profiles_succeeds() {
        let script = compose_recipe(&[], &[]);
        assert!(script.contains("apt-get"));
        assert!(
            script.contains("Installing Codex CLI"),
            "base recipe should install Codex CLI",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_recipe_post_install_without_trailing_newline() {
        let profiles = vec![profile("test", &["curl"], None, Some("echo done"))];
        let script = compose_recipe(&profiles, &[]);

        // The post_install "echo done" must be on its own line
        assert!(
            script.contains("echo done\n"),
            "post_install should end with newline",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_recipe_pre_install_without_trailing_newline() {
        let profiles = vec![profile(
            "test",
            &[],
            Some("curl -fsSL https://example.com | bash"),
            None,
        )];
        let script = compose_recipe(&profiles, &[]);

        assert!(
            script.contains("| bash\n"),
            "pre_install should end with newline",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_recipe_scripts_with_trailing_newline_no_double() {
        let profiles = vec![profile("test", &[], Some("pre-cmd\n"), Some("post-cmd\n"))];
        let script = compose_recipe(&profiles, &[]);

        // Should not produce triple+ newlines from double-adding
        assert!(
            !script.contains("\n\n\n\n"),
            "should not have excessive blank lines",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_recipe_multiple_profiles_separated() {
        let profiles = vec![
            profile("a", &[], None, Some("echo a-done")),
            profile("b", &[], None, Some("echo b-done")),
        ];
        let script = compose_recipe(&profiles, &[]);

        // Each post_install must be on its own line
        assert!(script.contains("echo a-done\n"));
        assert!(script.contains("echo b-done\n"));
        // They must not be on the same line
        for line in script.lines() {
            assert!(
                !(line.contains("a-done") && line.contains("b-done")),
                "two post_install scripts concatenated on one line",
            );
        }
    }

    #[test]
    fn compose_full_script_separates_post_install() {
        let recipe = "recipe-end";
        let script = compose_full_script(recipe, "{}", Some("user-post"));

        assert!(script.contains("user-post\n"));
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_full_script_no_post_install() {
        let recipe = "recipe-content\n";
        let script = compose_full_script(recipe, "{}", None);

        assert!(script.contains("recipe-content\n"));
        assert!(script.contains("MARKEREOF"));
        assert!(!script.contains("post-install"));
    }

    #[test]
    fn architecture_from_consts_str_known() {
        assert!(matches!(
            Architecture::from_consts_str("x86_64"),
            Ok(Architecture::X86_64)
        ));
        assert!(matches!(
            Architecture::from_consts_str("aarch64"),
            Ok(Architecture::Aarch64)
        ));
    }

    #[test]
    fn architecture_from_consts_str_rejects_unknown() {
        assert!(Architecture::from_consts_str("riscv64").is_err());
        assert!(Architecture::from_consts_str("").is_err());
        assert!(Architecture::from_consts_str("arm64").is_err());
    }

    #[test]
    fn architecture_display_matches_as_str() {
        assert_eq!(Architecture::X86_64.to_string(), "x86_64");
        assert_eq!(Architecture::Aarch64.to_string(), "aarch64");
    }

    #[test]
    fn architecture_current_returns_supported_variant() {
        // cargo runs tests on the host arch, which must be one of the
        // two supported variants.
        assert!(matches!(
            Architecture::current(),
            Ok(Architecture::X86_64 | Architecture::Aarch64)
        ));
    }
}
