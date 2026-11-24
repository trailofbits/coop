//! Pinned production Firecracker release and the root-owned matching pair.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::host_tool::{TrustedToolPolicy, validate_trusted_path};
use crate::sha256_hash::Sha256Hash;

pub(crate) const RELEASE: &str = "v1.18.0";
const INSTALL_ROOT: &str = "/var/lib/coop/firecracker";
const X86_64_ARCHIVE: &str = "1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd";
const AARCH64_ARCHIVE: &str = "2a6130ff0338901ff9d7fbd73ce046c6b1c6123889dc84a34d678aa53bd0c0c2";
const X86_64_VMM: &str = "b33f46cd6d88834cf6a4f8d184f08f219809c636236bffd286d233a60602a2ba";
const X86_64_JAILER: &str = "ddadbaa15b5c942271a83b7040ca7259c8ccf92a9e08d44cce0b86fa407ba460";
const AARCH64_VMM: &str = "6f9e08df0a0072e3e7086c51ff434c034d705845e349175a01beb3f9cc025ce3";
const AARCH64_JAILER: &str = "7ea8f46abdc6f1d1a0b58ce89ce6ca38be8a292a43d7e61cf88275318263c540";

pub(crate) fn install_root() -> &'static Path {
    Path::new(INSTALL_ROOT)
}

pub(crate) fn release_dir() -> PathBuf {
    install_root().join(RELEASE)
}

pub(crate) fn firecracker_path() -> PathBuf {
    release_dir().join("firecracker")
}

pub(crate) fn expected_archive_hash() -> Result<Sha256Hash> {
    let text = match std::env::consts::ARCH {
        "x86_64" => X86_64_ARCHIVE,
        "aarch64" => AARCH64_ARCHIVE,
        other => bail!("Unsupported Firecracker architecture: {other}"),
    };
    text.parse()
}

fn expected_pair_hashes() -> Result<(Sha256Hash, Sha256Hash)> {
    let (vmm, jailer) = match std::env::consts::ARCH {
        "x86_64" => (X86_64_VMM, X86_64_JAILER),
        "aarch64" => (AARCH64_VMM, AARCH64_JAILER),
        other => bail!("Unsupported Firecracker architecture: {other}"),
    };
    Ok((vmm.parse()?, jailer.parse()?))
}

pub(crate) fn verify_pair() -> Result<()> {
    verify_pair_at(&release_dir())
}

pub(crate) fn verify_pair_at(release: &Path) -> Result<()> {
    let policy = TrustedToolPolicy::new("/", [], 0);
    let (vmm_hash, jailer_hash) = expected_pair_hashes()?;
    verify_pair_at_with(release, &policy, vmm_hash, jailer_hash)
}

fn verify_pair_at_with(
    release: &Path,
    policy: &TrustedToolPolicy,
    vmm_hash: Sha256Hash,
    jailer_hash: Sha256Hash,
) -> Result<()> {
    let (_, metadata) = validate_trusted_path(release, policy)
        .map_err(anyhow::Error::msg)
        .context("Managed Firecracker release directory is not trusted")?;
    if !metadata.is_dir() {
        bail!("Managed Firecracker release path is not a directory");
    }

    for (name, expected) in [("firecracker", vmm_hash), ("jailer", jailer_hash)] {
        let path = release.join(name);
        if !fs::symlink_metadata(&path)
            .with_context(|| format!("Missing managed {name}"))?
            .file_type()
            .is_file()
        {
            bail!("Managed {name} is not a regular file");
        }
        let (_, metadata) = validate_trusted_path(&path, policy)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("Managed {name} path is not trusted"))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            bail!("Managed {name} is not an executable regular file");
        }
        let actual =
            Sha256Hash::of(fs::read(&path).with_context(|| format!("Cannot read managed {name}"))?);
        if actual != expected {
            bail!("Managed {name} differs from the pinned production release");
        }
    }
    Ok(())
}

#[cfg(any(not(target_os = "macos"), test))]
pub(crate) fn require_managed_path(path: &Path) -> Result<PathBuf> {
    let managed = firecracker_path();
    if path != managed {
        bail!(
            "Custom firecracker_bin '{}' cannot be used for a managed Firecracker launch. Remove that override and run `coop setup` to install the verified matching Firecracker/jailer pair at '{}'.",
            path.display(),
            managed.display()
        );
    }
    Ok(managed)
}

/// A pinned pair that passed root ownership and byte verification at launch.
pub(crate) struct VerifiedFirecracker(PathBuf);

impl VerifiedFirecracker {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn jailer_path(&self) -> PathBuf {
        self.0.with_file_name("jailer")
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn configured_firecracker(path: &Path) -> Result<VerifiedFirecracker> {
    let managed = require_managed_path(path)?;
    verify_pair().context(
        "The managed Firecracker installation is incomplete or changed. Stop VMs using it, then ask an administrator to remove its versioned release directory and rerun `coop setup`",
    )?;
    Ok(VerifiedFirecracker(managed))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures and assertions")]
mod tests {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    use super::*;

    #[test]
    fn managed_release_paths_and_archive_digest_are_pinned() {
        assert_eq!(install_root(), Path::new("/var/lib/coop/firecracker"));
        assert_eq!(
            release_dir(),
            PathBuf::from("/var/lib/coop/firecracker/v1.18.0")
        );
        assert_eq!(
            firecracker_path(),
            PathBuf::from("/var/lib/coop/firecracker/v1.18.0/firecracker")
        );
        let expected = match std::env::consts::ARCH {
            "x86_64" => X86_64_ARCHIVE,
            "aarch64" => AARCH64_ARCHIVE,
            _ => return,
        };
        assert_eq!(expected_archive_hash().unwrap(), expected.parse().unwrap());
        let (vmm, jailer) = match std::env::consts::ARCH {
            "x86_64" => (X86_64_VMM, X86_64_JAILER),
            "aarch64" => (AARCH64_VMM, AARCH64_JAILER),
            _ => return,
        };
        assert_eq!(
            expected_pair_hashes().unwrap(),
            (vmm.parse().unwrap(), jailer.parse().unwrap())
        );
    }

    #[test]
    fn managed_path_rejects_custom_binary_without_running_it() {
        assert_eq!(
            require_managed_path(&firecracker_path()).unwrap(),
            firecracker_path()
        );
        let error = require_managed_path(Path::new("/tmp/custom-vmm"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Custom firecracker_bin"));
        assert!(error.contains("Remove that override"));
    }

    #[test]
    fn pinned_pair_requires_both_trusted_executable_bytes() {
        let fixture = crate::host_tool::trusted_test_tempdir();
        let release = fixture.path().join("release");
        fs::create_dir(&release).unwrap();
        fs::set_permissions(&release, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(release.join("firecracker"), b"vmm witness").unwrap();
        let mut permissions = fs::metadata(release.join("firecracker"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(release.join("firecracker"), permissions).unwrap();
        let policy = TrustedToolPolicy::new(
            fixture.path(),
            [],
            fs::metadata(fixture.path()).unwrap().uid(),
        );
        let vmm = Sha256Hash::of(b"vmm witness");
        let jailer = Sha256Hash::of(b"jailer witness");
        assert!(verify_pair_at_with(&release, &policy, vmm, jailer).is_err());

        fs::write(release.join("jailer"), b"jailer witness").unwrap();
        fs::set_permissions(release.join("jailer"), fs::Permissions::from_mode(0o755)).unwrap();
        verify_pair_at_with(&release, &policy, vmm, jailer).unwrap();

        fs::write(release.join("firecracker"), b"wrong vmm").unwrap();
        assert!(
            verify_pair_at_with(&release, &policy, vmm, jailer)
                .unwrap_err()
                .to_string()
                .contains("Managed firecracker differs from the pinned production release")
        );
        fs::write(release.join("firecracker"), b"vmm witness").unwrap();
        verify_pair_at_with(&release, &policy, vmm, jailer).unwrap();

        fs::write(release.join("jailer"), b"wrong jailer").unwrap();
        assert!(
            verify_pair_at_with(&release, &policy, vmm, jailer)
                .unwrap_err()
                .to_string()
                .contains("pinned production release")
        );
        fs::remove_file(release.join("jailer")).unwrap();
        symlink(release.join("firecracker"), release.join("jailer")).unwrap();
        assert!(verify_pair_at_with(&release, &policy, vmm, jailer).is_err());

        fs::remove_file(release.join("jailer")).unwrap();
        fs::write(release.join("jailer"), b"jailer witness").unwrap();
        fs::set_permissions(release.join("jailer"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&release, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify_pair_at_with(&release, &policy, vmm, jailer).is_err());
    }
}
