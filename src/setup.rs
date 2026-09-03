use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cmd::{Cmd, command_exists};
use crate::config::{CoopConfig, ImageName, Instance};
use crate::devcontainer_oci::{InstalledFeature, ResolvedFeature, installed_features};
use crate::guest::{
    BASE_PACKAGES, DOCKER_PACKAGES, GH_PACKAGES, GuestUser, ProfileDef, SCRIPT_CLAUDE_CODE,
    SCRIPT_CODEX, SCRIPT_CODEX_ACCOUNT, SCRIPT_DOCKER_REPO, SCRIPT_GH_REPO, resolve_profiles,
};
use crate::sha256_hash::Sha256Hash;

// Path-style URL: the bucket name contains dots, so virtual-hosted HTTPS
// (`https://spec.ccfc.min.s3.amazonaws.com`) fails TLS wildcard-cert matching.
const S3_BUCKET: &str = "https://s3.amazonaws.com/spec.ccfc.min";
const GH_RELEASES: &str = "https://github.com/firecracker-microvm/firecracker/releases";

/// Monotonic version bumped when the base install script changes
/// in a way that requires rebuilding templates.
pub const TEMPLATE_VERSION: u32 = 2;

// ── Public types ──────────────────────────────────────────────

pub struct SetupOptions {
    pub skip_confirm: bool,
    pub rebuild: bool,
    pub profiles: Vec<ProfileDef>,
    pub oci_features: Vec<ResolvedFeature>,
    pub extra_packages: Vec<String>,
    pub post_install: Option<PathBuf>,
    pub image: ImageName,
    pub guest_user: GuestUser,
    pub builder_timeout: Option<Duration>,
}

/// Persisted template configuration (profiles, packages, hashes).
///
/// `guest_user` is set at setup time and immutable for the image's
/// lifetime — `coop start`/`shell`/`exec` read it from here rather
/// than accepting a flag. Existing `template_config.json` files
/// without the field deserialize via `GuestUser::default()` to the
/// historical `ubuntu` user.
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
    #[serde(default)]
    pub codex_marketplaces: Vec<String>,
    #[serde(default)]
    pub codex_plugins: Vec<String>,
    #[serde(default)]
    pub guest_user: GuestUser,
    #[serde(default)]
    pub oci_features: Vec<InstalledFeature>,
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
    eprintln!("Run `coop up` in a project directory to launch a VM.");
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
    reflink_copy(&template, &rootfs).context("Failed to copy template to instance")?;

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

/// Reflink-friendly copy of a rootfs image (`CoW` where the filesystem
/// supports it, full copy otherwise). Runs as root because instance and
/// template rootfs files are root-owned on Firecracker.
fn reflink_copy(src: &Path, dst: &Path) -> Result<()> {
    Cmd::new("cp")
        .arg("--reflink=auto")
        .arg(src)
        .arg(dst)
        .sudo()
        .run()
        .with_context(|| format!("Failed to copy {} -> {}", src.display(), dst.display()))
}

/// Save a stopped instance's rootfs as image `image`'s template.
///
/// The inverse of [`create_instance`]: reflink-copies the instance
/// `rootfs.ext4` to the image's `rootfs-template.ext4`. The caller has
/// already gated on the instance being stopped (filesystem consistency)
/// and decided whether overwriting an existing image is allowed.
pub fn commit_instance_rootfs(cfg: &CoopConfig, inst: &Instance, image: &ImageName) -> Result<()> {
    let rootfs = inst.rootfs_path();
    if !rootfs.exists() {
        bail!("Instance rootfs not found at {}", rootfs.display());
    }

    let image_dir = cfg.image_dir(image);
    fs::create_dir_all(&image_dir)
        .with_context(|| format!("Failed to create image dir {}", image_dir.display()))?;

    let template = cfg.template_path_for(image);
    // Remove an existing (root-owned) template so the copy is a clean
    // overwrite rather than appending to or failing on the old file.
    if template.exists() {
        Cmd::new("rm").arg("-f").arg(&template).sudo().run()?;
    }

    tracing::info!("Committing instance '{}' to image '{image}'", inst.name);
    reflink_copy(&rootfs, &template)
}

/// Replace a stopped instance's rootfs with image `image`'s template.
///
/// Mirrors [`create_instance`]'s copy + network-patch, but sources the
/// rootfs from an arbitrary image rather than the instance's origin
/// image. The network config is re-patched for this instance's IP,
/// overwriting whatever address the template baked in at commit time.
pub fn restore_instance_rootfs(cfg: &CoopConfig, inst: &Instance, image: &ImageName) -> Result<()> {
    let template = cfg.template_path_for(image);
    if !template.exists() {
        bail!("No image '{image}' found at {}.", template.display(),);
    }

    let rootfs = inst.rootfs_path();
    if rootfs.exists() {
        Cmd::new("rm").arg("-f").arg(&rootfs).sudo().run()?;
    }

    tracing::info!("Restoring instance '{}' from image '{image}'", inst.name);
    reflink_copy(&template, &rootfs)?;
    patch_guest_network(inst)?;
    Ok(())
}

// ── Mount guard (RAII) ─────────────────────────────────────────

/// How a [`MountGuard`] was set up, selecting the matching teardown on drop.
enum MountKind {
    /// Single loop mount of the rootfs.
    Simple,
    /// Full chroot: rootfs plus proc/sys/dev/devpts/tmp.
    Chroot,
}

/// RAII guard that unmounts a filesystem on drop, preventing leaked
/// mounts if an operation between mount and unmount fails.
struct MountGuard {
    mount_path: String,
    kind: MountKind,
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
            kind: MountKind::Simple,
        })
    }

    /// Full chroot mount: rootfs + proc/sys/dev/devpts/tmp.
    fn chroot(rootfs: &str, mount_path: &str) -> Result<Self> {
        mount_chroot(rootfs, mount_path)?;
        Ok(Self {
            mount_path: mount_path.to_string(),
            kind: MountKind::Chroot,
        })
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        match self.kind {
            MountKind::Chroot => unmount_chroot(&self.mount_path),
            MountKind::Simple => {
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

    // Compose the install recipe and compute its hashes.
    let recipe_script = compose_recipe(
        &profiles,
        &opts.oci_features,
        &extra_packages,
        &opts.guest_user,
    );
    let post_install_content = load_post_install(opts.post_install.as_ref())?;
    let recipe = BuildRecipe {
        profiles: &profiles,
        extra_packages: &extra_packages,
        script_hash: Sha256Hash::of(&recipe_script),
        post_install_hash: post_install_content.as_ref().map(Sha256Hash::of),
        script: &recipe_script,
        post_install_content: post_install_content.as_deref(),
    };

    // Check existing template — rebuild automatically if stale
    if template.exists() && !opts.rebuild {
        if !needs_rebuild(cfg, image, recipe.script_hash, recipe.post_install_hash) {
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

    // Build the persisted config once: `build_template` embeds it as the
    // in-guest version marker, and we save the same value after the image
    // swap. Constructing it once also pins a single `created` timestamp.
    let template_config = TemplateConfig {
        version: TEMPLATE_VERSION,
        created: utc_timestamp(),
        install_script_hash: recipe.script_hash,
        profiles: profile_names(recipe.profiles),
        extra_packages: recipe.extra_packages.to_vec(),
        post_install_hash: recipe.post_install_hash,
        marketplaces: Vec::new(),
        plugins: Vec::new(),
        codex_marketplaces: Vec::new(),
        codex_plugins: Vec::new(),
        guest_user: opts.guest_user.clone(),
        oci_features: installed_features(&opts.oci_features),
    };

    let result = build_template(cfg, opts, image, &recipe, &template_config, &staging);

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

/// The recipe inputs for a template build: the composed install script,
/// its hash, the resolved profiles and extra packages (for display), and
/// the optional post-install script and its hash.
struct BuildRecipe<'a> {
    profiles: &'a [ProfileDef],
    extra_packages: &'a [String],
    script: &'a str,
    script_hash: Sha256Hash,
    post_install_content: Option<&'a str>,
    post_install_hash: Option<Sha256Hash>,
}

fn build_template(
    cfg: &CoopConfig,
    opts: &SetupOptions,
    image: &ImageName,
    recipe: &BuildRecipe,
    template_config: &TemplateConfig,
    output_path: &Path,
) -> Result<()> {
    step("Building template rootfs");

    let rootfs_url = discover_ci_rootfs()?;
    let rootfs_name = rootfs_url.rsplit('/').next().unwrap_or("rootfs");

    eprintln!("  Found rootfs: {rootfs_name}");
    eprintln!("  URL: {rootfs_url}");
    if !recipe.profiles.is_empty() {
        eprintln!("  Profiles: {}", profile_names(recipe.profiles).join(", "));
    }
    if !opts.oci_features.is_empty() {
        let features = opts
            .oci_features
            .iter()
            .map(|f| format!("{} ({})", f.installed.id, f.installed.digest))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("  Devcontainer OCI features: {features}");
    }
    if !recipe.extra_packages.is_empty() {
        eprintln!("  Extra packages: {}", recipe.extra_packages.join(", "));
    }
    if recipe.post_install_content.is_some() {
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

    // Compose and execute install script. The in-guest version marker is
    // the same `TemplateConfig` we persist on the host after the swap.
    let marker_json = serde_json::to_string_pretty(template_config)
        .context("Failed to serialize version marker")?;
    let full_script = compose_full_script(recipe.script, &marker_json, recipe.post_install_content);
    install_guest_packages(
        cfg,
        output_path,
        &full_script,
        &opts.guest_user,
        opts.builder_timeout,
    )?;

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
    if !opts.profiles.is_empty() || !opts.extra_packages.is_empty() || !opts.oci_features.is_empty()
    {
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
fn compose_recipe(
    profiles: &[ProfileDef],
    oci_features: &[ResolvedFeature],
    extra_packages: &[String],
    guest_user: &GuestUser,
) -> String {
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

    // Export GUEST_USER so the chroot scripts (`guest-config.sh`,
    // `claude-code.sh`) and any user-supplied post-install can pick it
    // up. `GuestUser::new` validated the name against POSIX-portable
    // chars, so single-quoting is sufficient against shell injection.
    s.push_str("\nexport GUEST_USER='");
    s.push_str(guest_user.as_str());
    s.push_str("'\n");

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

    // Guest config configures the guest user — must come before claude-code.
    s.push_str(SCRIPT_GUEST_CONFIG);
    for feature in oci_features {
        s.push_str(&crate::devcontainer_oci::compose_install_snippet(feature));
    }
    // Direct binary download (runs as root in chroot, installs for guest user).
    s.push_str(SCRIPT_CLAUDE_CODE);
    // Codex installs as a package with stable entrypoints under /usr/local/bin.
    s.push_str(SCRIPT_CODEX);
    s.push_str(SCRIPT_CODEX_ACCOUNT);

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
    if command_exists("setfacl") {
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

    if !command_exists("setfacl") {
        missing.push("acl");
    }
    if !command_exists("debootstrap") {
        missing.push("debootstrap");
    }
    if !command_exists("unsquashfs") {
        missing.push("squashfs-tools");
    }
    if !command_exists("mkfs.ext4") {
        missing.push("e2fsprogs");
    }
    if !command_exists("ssh") {
        missing.push("openssh-client");
    }
    if !command_exists("rsync") {
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
    let start = FcVersion::parse(&discover_latest_fc_version()?)?;

    eprintln!(
        "  Looking for kernels in CI bucket (latest: {})...",
        start.ci_dirname()
    );

    let (ci_version, keys) = find_latest_ci_assets(
        start,
        arch,
        "vmlinux-",
        |k| k.contains("vmlinux-") && !k.ends_with(".config"),
        s3_list_keys,
    )?;

    let kernel_key = keys.into_iter().max().context("No kernel keys found")?;
    let kernel_url = format!("{S3_BUCKET}/{kernel_key}");
    let kernel_name = kernel_key.rsplit('/').next().unwrap_or("vmlinux");

    eprintln!(
        "  Found kernel: {kernel_name} (from {})",
        ci_version.ci_dirname()
    );
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

fn discover_ci_rootfs() -> Result<String> {
    let arch = Architecture::current()?;
    let start = FcVersion::parse(&discover_latest_fc_version()?)?;

    eprintln!(
        "  Looking for rootfs in CI bucket (latest: {})...",
        start.ci_dirname()
    );

    let (ci_version, keys) = find_latest_ci_assets(
        start,
        arch,
        "ubuntu-",
        |k| k.ends_with(".squashfs"),
        s3_list_keys,
    )
    .context(
        "Could not find a Firecracker CI rootfs in S3. CI assets lag a \
         Firecracker release by days to weeks; retry later once the matching \
         firecracker-ci/vX.Y/ directory is published.",
    )?;

    let rootfs_key = keys.into_iter().max().context("No rootfs keys found")?;
    eprintln!("  Found rootfs in {}", ci_version.ci_dirname());
    Ok(format!("{S3_BUCKET}/{rootfs_key}"))
}

/// Maximum number of older minor versions to try when the latest
/// Firecracker release has no CI assets in S3.
///
/// CI assets typically lag a release by days to weeks (the GitHub release
/// is cut before the Buildkite pipeline publishes the matching
/// `firecracker-ci/vX.Y/` directory). Walking back too far risks pulling
/// a rootfs/kernel built for a Firecracker that has diverged in API.
const FC_CI_FALLBACK_MINORS: u16 = 4;

/// List S3 keys under a given prefix in the Firecracker CI bucket.
///
/// Returns the raw `<Key>` strings parsed out of the S3 `ListObjectsV2`
/// XML response. An empty response (the prefix has no objects) returns
/// an empty Vec rather than an error.
fn s3_list_keys(prefix: &str) -> Result<Vec<String>> {
    let list_url = format!("{S3_BUCKET}/?prefix={prefix}&list-type=2");
    let output = Command::new("curl")
        .args(["-sf", &list_url])
        .output()
        .with_context(|| format!("Failed to list S3 artifacts under {prefix}"))?;
    let body = String::from_utf8_lossy(&output.stdout);
    Ok(body
        .split("<Key>")
        .skip(1)
        .filter_map(|s| s.split("</Key>").next())
        .map(str::to_owned)
        .collect())
}

/// Walk back minor versions from `start` until a CI bucket with matching
/// assets is found, returning the version used and the matching keys.
///
/// CI assets for the latest Firecracker release can lag the GitHub
/// release by days. Rather than failing setup when that happens, fall
/// back to the next older minor (still within the same major). The
/// first version whose listing contains at least one key matching
/// `filter` is returned. A WARN is logged whenever a fallback is used
/// so the user knows the CI bucket is behind.
///
/// `list_keys` is injected so unit tests can exercise the walk without
/// touching the network.
fn find_latest_ci_assets(
    start: FcVersion,
    arch: Architecture,
    prefix_suffix: &str,
    filter: impl Fn(&str) -> bool,
    list_keys: impl Fn(&str) -> Result<Vec<String>>,
) -> Result<(FcVersion, Vec<String>)> {
    let mut tried = Vec::with_capacity(usize::from(FC_CI_FALLBACK_MINORS));
    for back in 0..FC_CI_FALLBACK_MINORS {
        let Some(minor) = start.minor.checked_sub(back) else {
            break;
        };
        let candidate = FcVersion {
            major: start.major,
            minor,
        };
        let ci_dir = candidate.ci_dirname();
        let prefix = format!("firecracker-ci/{ci_dir}/{arch}/{prefix_suffix}");
        let matches: Vec<String> = list_keys(&prefix)?
            .into_iter()
            .filter(|k| filter(k))
            .collect();
        if !matches.is_empty() {
            if back > 0 {
                tracing::warn!(
                    latest = %start.ci_dirname(),
                    using = %ci_dir,
                    "Firecracker CI bucket has no '{prefix_suffix}' assets for the latest release; \
                     falling back to {ci_dir}"
                );
                eprintln!(
                    "  Warning: CI bucket has no '{prefix_suffix}' assets for {} \
                     — using {ci_dir} instead",
                    start.ci_dirname()
                );
            }
            return Ok((candidate, matches));
        }
        tried.push(ci_dir);
    }
    bail!(
        "No Firecracker CI assets matching '{prefix_suffix}' found for {arch} \
         in any of: {}",
        tried.join(", ")
    );
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
fn install_guest_packages(
    cfg: &CoopConfig,
    image_path: &Path,
    script: &str,
    guest_user: &GuestUser,
    builder_timeout: Option<Duration>,
) -> Result<()> {
    eprintln!("  Installing guest packages (Docker, Claude Code, Codex)...");
    eprintln!("  This requires sudo and may take several minutes.");

    let template_str = image_path.display().to_string();
    let mount_dir = cfg.data_dir.join("rootfs-mount");
    let mount_str = mount_dir.display().to_string();

    let _guard = MountGuard::chroot(&template_str, &mount_str)?;

    let mut command = Cmd::new("chroot").arg(&mount_str);
    if let Some(timeout) = builder_timeout {
        let timeout_arg = crate::format_duration(timeout);
        command = command.args(["timeout", timeout_arg.as_str()]);
    }
    command
        .args(["bash", "-c", script])
        .sudo()
        .run()
        .context("Guest package installation failed")?;

    verify_chroot_binaries(&mount_str, guest_user)?;

    // _guard dropped here → unmount_chroot runs regardless of status
    Ok(())
}

/// Verify critical binaries exist in the chroot after provisioning.
///
/// Runs `test -x` inside the chroot so absolute symlink targets resolve against
/// the guest root. Checking from the host with either `exists` or `lstat` would
/// respectively reject valid absolute guest symlinks or accept dangling ones.
fn verify_chroot_binaries(mount_str: &str, guest_user: &GuestUser) -> Result<()> {
    crate::guest::verify_required_binaries(
        guest_user,
        "install script",
        |path| {
            let path = path.to_string();
            Cmd::new("chroot")
                .args([mount_str, "/usr/bin/test", "-x", path.as_str()])
                .sudo()
                .run()
                .is_ok()
        },
        String::new,
    )
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

/// Parsed Firecracker release version (e.g. `v1.15`).
///
/// Carries the major and minor components so the CI bucket directory
/// name is derived from a parsed value rather than re-slicing a string
/// each time it's needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FcVersion {
    pub major: u16,
    pub minor: u16,
}

impl FcVersion {
    /// Parse a Firecracker release tag like `v1.15.0`.
    ///
    /// Accepts the GitHub release tag format: a leading `v` followed by
    /// at least `MAJOR.MINOR` numeric components. Any trailing `.PATCH`
    /// (or further) segments are tolerated but discarded — the CI bucket
    /// is keyed by `vMAJOR.MINOR` only.
    pub fn parse(release: &str) -> Result<Self> {
        let rest = release
            .strip_prefix('v')
            .with_context(|| format!("Firecracker version must start with 'v': '{release}'"))?;
        let mut parts = rest.split('.');
        let major_str = parts
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("Firecracker version is missing MAJOR: '{release}'"))?;
        let minor_str = parts
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("Firecracker version is missing MINOR: '{release}'"))?;
        let major = major_str.parse::<u16>().with_context(|| {
            format!("Firecracker MAJOR is not a u16 in '{release}' (got '{major_str}')")
        })?;
        let minor = minor_str.parse::<u16>().with_context(|| {
            format!("Firecracker MINOR is not a u16 in '{release}' (got '{minor_str}')")
        })?;
        Ok(Self { major, minor })
    }

    /// CI bucket directory name, e.g. `v1.15`.
    pub fn ci_dirname(self) -> String {
        format!("v{}.{}", self.major, self.minor)
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

fn require_command(name: &str, purpose: &str) -> Result<()> {
    if command_exists(name) {
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
#[expect(clippy::unwrap_used, clippy::panic, reason = "tests")]
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
        let script = compose_recipe(&[], &[], &[], &GuestUser::default());
        assert!(script.contains("apt-get"));
        assert!(
            script.contains("Installing Codex CLI"),
            "base recipe should install Codex CLI",
        );
        assert!(
            script.contains("Installing codex-account shortcut"),
            "base recipe should install Codex account-auth wrapper",
        );
        assert!(
            script.contains("exec codex-account --dangerously-bypass-approvals-and-sandbox"),
            "codex-yolo should route through the account wrapper so keyring \
             mode works from an in-guest shell",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_recipe_exports_guest_user() {
        let user = GuestUser::new("vscode").unwrap();
        let script = compose_recipe(&[], &[], &[], &user);
        assert!(
            script.contains("export GUEST_USER='vscode'"),
            "recipe must export GUEST_USER for the chroot scripts:\n{script}"
        );
    }

    #[test]
    fn template_config_loads_legacy_json_without_guest_user_field() {
        // Pre-PR images on disk have no `guest_user` field; the serde
        // default keeps them deserializable as `ubuntu`. Regression
        // guard against accidentally dropping the `#[serde(default)]`.
        let json = r#"{
            "version": 1,
            "created": "2026-01-01T00:00:00Z",
            "install_script_hash": "0000000000000000000000000000000000000000000000000000000000000000",
            "profiles": [],
            "extra_packages": [],
            "post_install_hash": null
        }"#;
        let tc: TemplateConfig = serde_json::from_str(json).unwrap();
        assert_eq!(tc.guest_user, GuestUser::default());
        // Codex bake lists were added later; legacy JSON omits them and
        // must default to empty rather than failing to deserialize.
        assert!(tc.codex_marketplaces.is_empty());
        assert!(tc.codex_plugins.is_empty());
    }

    #[test]
    fn compose_recipe_post_install_without_trailing_newline() {
        let profiles = vec![profile("test", &["curl"], None, Some("echo done"))];
        let script = compose_recipe(&profiles, &[], &[], &GuestUser::default());

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
        let script = compose_recipe(&profiles, &[], &[], &GuestUser::default());

        assert!(
            script.contains("| bash\n"),
            "pre_install should end with newline",
        );
        no_consecutive_concat(&script);
    }

    #[test]
    fn compose_recipe_scripts_with_trailing_newline_no_double() {
        let profiles = vec![profile("test", &[], Some("pre-cmd\n"), Some("post-cmd\n"))];
        let script = compose_recipe(&profiles, &[], &[], &GuestUser::default());

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
        let script = compose_recipe(&profiles, &[], &[], &GuestUser::default());

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

    #[test]
    fn fc_version_parses_release_tag() {
        let v = FcVersion::parse("v1.15.0").unwrap();
        assert_eq!(
            v,
            FcVersion {
                major: 1,
                minor: 15
            }
        );
    }

    #[test]
    fn fc_version_parses_double_digit_minor() {
        let v = FcVersion::parse("v2.103.7").unwrap();
        assert_eq!(
            v,
            FcVersion {
                major: 2,
                minor: 103,
            }
        );
    }

    #[test]
    fn fc_version_ci_dirname_keeps_v_prefix() {
        assert_eq!(
            FcVersion {
                major: 1,
                minor: 15
            }
            .ci_dirname(),
            "v1.15"
        );
    }

    #[test]
    fn fc_version_rejects_missing_v_prefix() {
        let err = FcVersion::parse("1.15.0").unwrap_err().to_string();
        assert!(err.contains("must start with 'v'"), "{err}");
    }

    #[test]
    fn fc_version_rejects_missing_minor() {
        let err = FcVersion::parse("v1").unwrap_err().to_string();
        assert!(err.contains("missing MINOR"), "{err}");
    }

    #[test]
    fn fc_version_rejects_non_numeric_major() {
        let err = FcVersion::parse("vfoo.bar").unwrap_err().to_string();
        assert!(err.contains("MAJOR is not a u16"), "{err}");
    }

    #[test]
    fn fc_version_rejects_empty_segments() {
        let err = FcVersion::parse("v.15").unwrap_err().to_string();
        assert!(err.contains("missing MAJOR"), "{err}");
    }

    fn keys_with(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn find_latest_ci_assets_returns_first_when_present() {
        let start = FcVersion {
            major: 1,
            minor: 16,
        };
        let list = |prefix: &str| -> Result<Vec<String>> {
            assert!(
                prefix.starts_with("firecracker-ci/v1.16/"),
                "expected to query v1.16 first, got {prefix}"
            );
            Ok(keys_with(&[
                "firecracker-ci/v1.16/x86_64/ubuntu-24.04.squashfs",
            ]))
        };
        let (version, matches) = find_latest_ci_assets(
            start,
            Architecture::X86_64,
            "ubuntu-",
            |k| k.ends_with(".squashfs"),
            list,
        )
        .unwrap();
        assert_eq!(version, start);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn find_latest_ci_assets_falls_back_one_minor() {
        let start = FcVersion {
            major: 1,
            minor: 16,
        };
        let list = |prefix: &str| -> Result<Vec<String>> {
            if prefix.starts_with("firecracker-ci/v1.16/") {
                Ok(Vec::new())
            } else if prefix.starts_with("firecracker-ci/v1.15/") {
                Ok(keys_with(&[
                    "firecracker-ci/v1.15/x86_64/ubuntu-24.04.squashfs",
                ]))
            } else {
                panic!("unexpected prefix: {prefix}")
            }
        };
        let (version, matches) = find_latest_ci_assets(
            start,
            Architecture::X86_64,
            "ubuntu-",
            |k| k.ends_with(".squashfs"),
            list,
        )
        .unwrap();
        assert_eq!(
            version,
            FcVersion {
                major: 1,
                minor: 15
            }
        );
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn find_latest_ci_assets_filter_drops_non_matching_keys() {
        // Bucket has objects but none match the filter — keep walking.
        let start = FcVersion {
            major: 1,
            minor: 16,
        };
        let list = |prefix: &str| -> Result<Vec<String>> {
            if prefix.starts_with("firecracker-ci/v1.16/") {
                // Has files but none are .squashfs (e.g. only manifests/configs).
                Ok(keys_with(&[
                    "firecracker-ci/v1.16/x86_64/ubuntu-24.04.manifest",
                ]))
            } else if prefix.starts_with("firecracker-ci/v1.15/") {
                Ok(keys_with(&[
                    "firecracker-ci/v1.15/x86_64/ubuntu-24.04.squashfs",
                ]))
            } else {
                panic!("unexpected prefix: {prefix}")
            }
        };
        let (version, _) = find_latest_ci_assets(
            start,
            Architecture::X86_64,
            "ubuntu-",
            |k| k.ends_with(".squashfs"),
            list,
        )
        .unwrap();
        assert_eq!(version.minor, 15);
    }

    #[test]
    fn find_latest_ci_assets_bails_when_all_empty() {
        let start = FcVersion {
            major: 1,
            minor: 16,
        };
        let list = |_: &str| -> Result<Vec<String>> { Ok(Vec::new()) };
        let err = find_latest_ci_assets(
            start,
            Architecture::X86_64,
            "ubuntu-",
            |k| k.ends_with(".squashfs"),
            list,
        )
        .unwrap_err()
        .to_string();
        // Should mention each version it attempted, up to FC_CI_FALLBACK_MINORS.
        for minor in (16 - i32::from(FC_CI_FALLBACK_MINORS) + 1)..=16 {
            assert!(err.contains(&format!("v1.{minor}")), "{err}");
        }
    }

    #[test]
    fn find_latest_ci_assets_stops_at_minor_zero() {
        use std::cell::RefCell;

        // Don't underflow when start.minor < FC_CI_FALLBACK_MINORS.
        let start = FcVersion { major: 1, minor: 2 };
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let err = find_latest_ci_assets(
            start,
            Architecture::X86_64,
            "ubuntu-",
            |k| k.ends_with(".squashfs"),
            |prefix: &str| -> Result<Vec<String>> {
                seen.borrow_mut().push(prefix.to_string());
                Ok(Vec::new())
            },
        )
        .unwrap_err()
        .to_string();
        let seen = seen.into_inner();
        // We should have tried exactly minors 2, 1, 0 — three calls, not four.
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen[0].contains("v1.2/"), "{}", seen[0]);
        assert!(seen[1].contains("v1.1/"), "{}", seen[1]);
        assert!(seen[2].contains("v1.0/"), "{}", seen[2]);
        assert!(err.contains("v1.0"), "{err}");
    }

    #[test]
    fn find_latest_ci_assets_propagates_lookup_error() {
        let start = FcVersion {
            major: 1,
            minor: 16,
        };
        let list = |_: &str| -> Result<Vec<String>> { anyhow::bail!("network is on fire") };
        let err = find_latest_ci_assets(
            start,
            Architecture::X86_64,
            "ubuntu-",
            |k| k.ends_with(".squashfs"),
            list,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("network is on fire"), "{err}");
    }
}
