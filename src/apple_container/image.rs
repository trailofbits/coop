//! Sandbox image: minimal build context, manifest identity, and the
//! digest-backed `apple-image.json` template record.
//!
//! Stock Apple `container build` turns the context into an OCI image, which
//! is then imported into `coop-sandbox`'s private store; the builder's copy
//! is deleted. An image can also be a disk committed from an instance.
//!
//! The context is generated in a private temporary directory and holds only
//! the rendered Dockerfile and reviewed provisioning scripts, including the
//! coop VM-access **public** key. It is never the repository, the working
//! directory, or the macOS home, and it never carries a secret.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::AppleError;
use super::state::{BACKEND_TAG, Owner, SCHEMA_VERSION};
use crate::config::{CoopConfig, ImageName};
use crate::devcontainer_oci::ResolvedFeature;
use crate::guest::{GuestUser, ProfileDef};

/// Base image for every coop machine image (and the maintenance image),
/// pinned by its multi-arch index digest so a rebuild cannot silently pick
/// up a different base.
pub(crate) const BASE_IMAGE: &str = "docker.io/library/ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3";
/// The only guest platform version 1 supports.
pub(crate) const PLATFORM: &str = "linux/arm64";
/// Where the build context is copied inside the image during the build. Not
/// under `/tmp`, which the shared provisioning script wipes.
const CONTEXT_DIR: &str = "/opt/coop-build";

const MANIFEST_FILE: &str = "apple-image.json";

/// `images/<name>/apple-image.json` — published only after the image passed
/// verification in a disposable machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImageManifest {
    pub(crate) schema_version: u32,
    pub(crate) backend: String,
    /// Owned local tag, e.g. `local/coop-0a1b2c3d:0011223344556677`. For a
    /// committed disk, the image the committed instance was created from.
    pub(crate) image_ref: String,
    /// Content digest of `image_ref` in the runtime's store.
    pub(crate) digest: String,
    /// Set for an image made by `coop commit`: instances clone this disk
    /// instead of unpacking `image_ref`.
    #[serde(default)]
    pub(crate) disk: Option<CommittedDisk>,
    /// Hash of every build input; see [`manifest_id`].
    pub(crate) manifest_id: String,
    pub(crate) base_image: String,
    pub(crate) platform: String,
    pub(crate) guest_user: GuestUser,
    pub(crate) created: String,
}

/// A disk saved with `coop-sandbox commit`, identity removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommittedDisk {
    pub(crate) name: super::state::MachineName,
    pub(crate) bytes: u64,
}

impl ImageManifest {
    fn path(cfg: &CoopConfig, image: &ImageName) -> std::path::PathBuf {
        cfg.image_dir(image).join(MANIFEST_FILE)
    }

    pub(crate) fn try_load(cfg: &CoopConfig, image: &ImageName) -> Result<Option<Self>> {
        let path = Self::path(cfg, image);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
        };
        let manifest: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        if manifest.backend != BACKEND_TAG || manifest.schema_version != SCHEMA_VERSION {
            bail!(AppleError::IdentityConflict(format!(
                "{} is not a {BACKEND_TAG} v{SCHEMA_VERSION} image manifest",
                path.display()
            )));
        }
        Ok(Some(manifest))
    }

    /// [`Self::try_load`], treating an unreadable or foreign manifest as
    /// absent (with a warning). For paths that replace or remove it.
    pub(crate) fn load_lenient(cfg: &CoopConfig, image: &ImageName) -> Option<Self> {
        Self::try_load(cfg, image).unwrap_or_else(|e| {
            tracing::warn!("Ignoring unreadable image manifest for '{image}': {e:#}");
            None
        })
    }

    pub(crate) fn load(cfg: &CoopConfig, image: &ImageName) -> Result<Self> {
        Self::try_load(cfg, image)?.ok_or_else(|| {
            anyhow::anyhow!("No image '{image}' found.\nRun `coop setup --image {image}` first.")
        })
    }

    pub(crate) fn save(&self, cfg: &CoopConfig, image: &ImageName) -> Result<()> {
        super::state::ensure_private_dir(&cfg.image_dir(image))?;
        let json = serde_json::to_string_pretty(self).context("Failed to serialize manifest")?;
        crate::fs_util::atomic_write_with_mode(&Self::path(cfg, image), &format!("{json}\n"), 0o600)
    }
}

/// Everything that goes into the build context, rendered in memory.
pub(crate) struct BuildContext {
    files: Vec<(&'static str, String, u32)>,
}

/// Inputs that determine an image's content.
pub(crate) struct BuildInputs<'a> {
    pub(crate) pubkey: &'a str,
    pub(crate) profiles: &'a [ProfileDef],
    pub(crate) oci_features: &'a [ResolvedFeature],
    pub(crate) guest_user: &'a GuestUser,
}

impl BuildContext {
    pub(crate) fn render(inputs: &BuildInputs<'_>) -> Self {
        let provision = crate::lima::compose_provision_script(
            inputs.pubkey,
            inputs.profiles,
            inputs.oci_features,
            inputs.guest_user,
        );
        Self {
            files: vec![
                ("Dockerfile", dockerfile(), 0o644),
                ("provision.sh", provision, 0o644),
                ("machine-setup.sh", machine_setup_script(), 0o644),
                ("coop-ssh-hostkeys.service", HOSTKEY_UNIT.to_string(), 0o644),
                ("10-coop.conf", SSHD_DROPIN.to_string(), 0o644),
            ],
        }
    }

    /// Stable hash of the context plus the identity inputs that are not in
    /// file contents.
    pub(crate) fn manifest_id(&self, guest_user: &GuestUser, pubkey_fingerprint: &str) -> String {
        let mut h = sha2::Sha256::new();
        for part in [
            format!("schema={SCHEMA_VERSION}"),
            format!("base={BASE_IMAGE}"),
            format!("platform={PLATFORM}"),
            format!("user={guest_user}"),
            format!("pubkey={pubkey_fingerprint}"),
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        for (name, content, mode) in &self.files {
            h.update(name.as_bytes());
            h.update([0]);
            h.update(mode.to_be_bytes());
            h.update(content.as_bytes());
            h.update([0]);
        }
        hex::encode(h.finalize())
    }

    /// Write the context into a fresh private directory.
    pub(crate) fn materialize(&self) -> Result<tempfile::TempDir> {
        let dir = tempfile::Builder::new()
            .prefix("coop-apple-build-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .context("Failed to create build context directory")?;
        for (name, content, mode) in &self.files {
            write_mode(&dir.path().join(name), content, *mode)?;
        }
        Ok(dir)
    }
}

/// Version of [`maintenance_dockerfile`]. `coop setup` reinstalls the
/// maintenance image when the runtime reports another; bump it with any
/// change to the recipe.
pub(crate) const MAINTENANCE_VERSION: &str = "2";

/// The maintenance image: a shell and e2fsprogs, nothing else. The runtime
/// boots it (networkless, from a disposable clone) to grow a disk or strip
/// a committed disk's identity, with the target attached as data. It is
/// independent of every application image, so neither an image's size nor
/// its deletion affects maintenance.
fn maintenance_dockerfile() -> String {
    format!(
        r"# Generated by coop — coop-sandbox maintenance image {MAINTENANCE_VERSION}.
FROM {BASE_IMAGE}
RUN set -eux; \
    export DEBIAN_FRONTEND=noninteractive; \
    apt-get update -qq; \
    apt-get install -y -qq --no-install-recommends e2fsprogs; \
    rm -rf /var/lib/apt/lists/*
"
    )
}

/// A private build context holding only the maintenance Dockerfile.
pub(crate) fn maintenance_context() -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("coop-apple-maintenance-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .context("Failed to create build context directory")?;
    write_mode(
        &dir.path().join("Dockerfile"),
        &maintenance_dockerfile(),
        0o644,
    )?;
    Ok(dir)
}

/// Owned tag for a maintenance build; deleted from the store after the
/// install attempt.
pub(crate) fn maintenance_ref(owner: &Owner, build_id: &str) -> String {
    format!(
        "local/coop-{}-maintenance:{MAINTENANCE_VERSION}-{build_id}",
        owner.id.short()
    )
}

fn write_mode(path: &Path, content: &str, mode: u32) -> Result<()> {
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("Failed to chmod {}", path.display()))
}

/// Owned tag for one build: repository scoped to this installation, tag from
/// the content hash plus a per-build nonce. Every build gets a fresh tag, so
/// a rebuild (even of identical inputs) never retags the image a published
/// manifest or an existing instance points at.
pub(crate) fn image_ref(owner: &Owner, manifest_id: &str, build_id: &str) -> String {
    format!(
        "{}{}-{build_id}",
        owned_repo(owner),
        &manifest_id[..16.min(manifest_id.len())]
    )
}

/// Whether `reference` is an application-image tag [`image_ref`] made for
/// `owner` (maintenance tags are not).
pub(crate) fn is_owned_ref(owner: &Owner, reference: &str) -> bool {
    reference.starts_with(&owned_repo(owner))
}

/// `local/coop-<owner8>:`, the repository prefix of every owned image tag.
fn owned_repo(owner: &Owner) -> String {
    format!("local/coop-{}:", owner.id.short())
}

fn dockerfile() -> String {
    format!(
        r"# Generated by coop — coop-sandbox image.
FROM {BASE_IMAGE}
ENV container=container
COPY . {CONTEXT_DIR}/
RUN set -eux; \
    export DEBIAN_FRONTEND=noninteractive; \
    apt-get update -qq; \
    apt-get install -y -qq --no-install-recommends \
        ca-certificates curl gnupg systemd systemd-sysv dbus openssh-server sudo \
        iproute2 iputils-ping lsb-release e2fsprogs; \
    bash {CONTEXT_DIR}/provision.sh; \
    bash {CONTEXT_DIR}/machine-setup.sh; \
    rm -rf {CONTEXT_DIR}
"
    )
}

/// systemd as the sandbox's init, plus per-instance identity: the reusable
/// image carries no SSH host keys and an empty machine-id, so each sandbox
/// generates its own on first boot and keeps them across restarts. The
/// runtime configures the interface, DNS, and hostname, so the units that
/// would fight it (or need hardware the VM lacks) are masked.
fn machine_setup_script() -> String {
    format!(
        r"#!/bin/bash
set -euo pipefail

systemctl set-default multi-user.target
systemctl mask \
    systemd-udevd.service systemd-udevd-kernel.socket systemd-udevd-control.socket \
    systemd-networkd.service systemd-networkd.socket systemd-networkd-wait-online.service \
    systemd-resolved.service systemd-timesyncd.service systemd-firstboot.service \
    getty.target console-getty.service
systemctl disable networkd-dispatcher.service 2>/dev/null || true

install -m 0644 {CONTEXT_DIR}/coop-ssh-hostkeys.service /etc/systemd/system/coop-ssh-hostkeys.service
install -d -m 0755 /etc/ssh/sshd_config.d
install -m 0644 {CONTEXT_DIR}/10-coop.conf /etc/ssh/sshd_config.d/10-coop.conf
systemctl disable ssh.socket 2>/dev/null || true
systemctl enable coop-ssh-hostkeys.service ssh.service docker.service

rm -f /etc/ssh/ssh_host_*
: > /etc/machine-id
rm -f /var/lib/dbus/machine-id
"
    )
}

const HOSTKEY_UNIT: &str = "\
[Unit]
Description=Generate this machine's SSH host keys
Before=ssh.service
ConditionPathExists=!/etc/ssh/ssh_host_ed25519_key

[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A

[Install]
WantedBy=multi-user.target
";

/// Included before the main `sshd_config`, so these values win. TCP
/// forwarding stays on for coop's own `-L`/`-R` tunnels; remote forwards bind
/// the guest loopback only.
const SSHD_DROPIN: &str = "\
PermitRootLogin no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AllowAgentForwarding no
AllowStreamLocalForwarding no
X11Forwarding no
PermitTunnel no
GatewayPorts no
AllowTcpForwarding yes
";

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn inputs(user: &GuestUser) -> BuildInputs<'_> {
        BuildInputs {
            pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINiqkOnkRV06x+SuorkF+O3KdBTVFznIV0+b58cidW1N coop",
            profiles: &[],
            oci_features: &[],
            guest_user: user,
        }
    }

    /// `scripts/guest/docker-repo.sh` resolves the Ubuntu codename with
    /// `lsb_release`, which the `ubuntu` base image does not ship.
    #[test]
    fn dockerfile_installs_provision_prerequisites() {
        assert!(dockerfile().contains(" lsb-release"));
    }

    #[test]
    fn manifest_id_tracks_inputs() {
        let user = GuestUser::default();
        let ctx = BuildContext::render(&inputs(&user));
        let a = ctx.manifest_id(&user, "SHA256:a");
        assert_eq!(a, ctx.manifest_id(&user, "SHA256:a"));
        assert_ne!(a, ctx.manifest_id(&user, "SHA256:b"));
        let other = GuestUser::new("coop").unwrap();
        let ctx2 = BuildContext::render(&inputs(&other));
        assert_ne!(a, ctx2.manifest_id(&other, "SHA256:a"));
    }

    #[test]
    fn context_is_private_and_minimal() {
        let user = GuestUser::default();
        let ctx = BuildContext::render(&inputs(&user));
        let dir = ctx.materialize().unwrap();
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "10-coop.conf",
                "Dockerfile",
                "coop-ssh-hostkeys.service",
                "machine-setup.sh",
                "provision.sh"
            ]
        );
        let all: String = ctx.files.iter().map(|(_, c, _)| c.as_str()).collect();
        assert!(!all.contains("PRIVATE KEY"));
        assert!(!all.contains("ARG "), "build args could leak into layers");
    }

    #[test]
    fn image_strips_host_identity_and_hardens_sshd() {
        let setup = machine_setup_script();
        assert!(setup.contains("rm -f /etc/ssh/ssh_host_*"));
        assert!(setup.contains(": > /etc/machine-id"));
        assert!(SSHD_DROPIN.contains("AllowAgentForwarding no"));
        assert!(SSHD_DROPIN.contains("PasswordAuthentication no"));
        assert!(SSHD_DROPIN.contains("PermitRootLogin no"));
        // The runtime owns addressing; networkd must never reconfigure eth0.
        assert!(setup.contains("systemd-networkd.service"));
    }

    /// Offline disk growth boots a clone of the image and runs resize2fs.
    fn manifest() -> ImageManifest {
        ImageManifest {
            schema_version: SCHEMA_VERSION,
            backend: BACKEND_TAG.into(),
            image_ref: "local/coop-0a1b2c3d:x".into(),
            digest: "sha256:x".into(),
            disk: None,
            manifest_id: "m".into(),
            base_image: "debian".into(),
            platform: "linux/arm64".into(),
            guest_user: GuestUser::default(),
            created: "now".into(),
        }
    }

    /// Absent is `None`; unreadable, another backend's, or another schema's
    /// manifest is an error (lenient callers treat it as absent).
    #[test]
    fn manifest_load_distinguishes_absent_from_foreign() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CoopConfig {
            data_dir: crate::config::ConfigPath::new(tmp.path()),
            ..CoopConfig::default()
        };
        let image = ImageName::new("default").unwrap();
        assert!(ImageManifest::try_load(&cfg, &image).unwrap().is_none());

        manifest().save(&cfg, &image).unwrap();
        assert_eq!(
            ImageManifest::try_load(&cfg, &image).unwrap(),
            Some(manifest())
        );
        assert_eq!(ImageManifest::load_lenient(&cfg, &image), Some(manifest()));

        let foreign = [
            ImageManifest {
                backend: "lima".into(),
                ..manifest()
            },
            ImageManifest {
                schema_version: SCHEMA_VERSION + 1,
                ..manifest()
            },
        ];
        for m in foreign {
            m.save(&cfg, &image).unwrap();
            let err = ImageManifest::try_load(&cfg, &image).unwrap_err();
            assert!(matches!(
                err.downcast_ref::<AppleError>(),
                Some(AppleError::IdentityConflict(_))
            ));
            assert!(ImageManifest::load_lenient(&cfg, &image).is_none());
        }

        let path = ImageManifest::path(&cfg, &image);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(ImageManifest::try_load(&cfg, &image).is_err());
    }

    #[test]
    fn image_ref_is_scoped_to_owner_and_build() {
        let owner = Owner {
            schema_version: 1,
            backend: BACKEND_TAG.into(),
            id: "0a1b2c3d00112233445566778899aabb"
                .to_string()
                .try_into()
                .unwrap(),
        };
        let tag = image_ref(&owner, "00112233445566778899", "abcd");
        assert_eq!(tag, "local/coop-0a1b2c3d:0011223344556677-abcd");
        assert!(is_owned_ref(&owner, &tag));
        for foreign in [
            "local/coop-ffffffff:0011223344556677-abcd",
            "local/coop-0a1b2c3dx:1",
            "docker.io/library/ubuntu:24.04",
        ] {
            assert!(!is_owned_ref(&owner, foreign), "{foreign}");
        }
    }

    /// Maintenance needs its own small image with the filesystem tools,
    /// under a tag that application-image cleanup never matches.
    #[test]
    fn maintenance_image_is_separate_and_minimal() {
        let dockerfile = maintenance_dockerfile();
        assert!(dockerfile.contains("install -y -qq --no-install-recommends e2fsprogs;"));
        assert!(!dockerfile.contains("COPY"));
        let owner = Owner {
            schema_version: 1,
            backend: BACKEND_TAG.into(),
            id: "0a1b2c3d00112233445566778899aabb"
                .to_string()
                .try_into()
                .unwrap(),
        };
        let tag = maintenance_ref(&owner, "ab");
        assert_eq!(
            tag,
            format!("local/coop-0a1b2c3d-maintenance:{MAINTENANCE_VERSION}-ab")
        );
        assert!(!is_owned_ref(&owner, &tag));
        let dir = maintenance_context().unwrap();
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }
}
