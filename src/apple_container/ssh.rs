//! First-boot host-key enrollment and pinned SSH targets.
//!
//! The guest's Ed25519 host public key is read over the runtime's native
//! control channel (`coop-sandbox exec` over vsock, addressed to the exact
//! owned sandbox) — never over the network with `ssh-keyscan` — and written to a
//! per-instance known-hosts file under a stable alias. Every later connection
//! verifies against that pin; a missing or changed key is a hard error.

use std::net::Ipv4Addr;
use std::num::NonZeroU16;

use anyhow::{Context, Result, bail};
use sha2::Digest as _;

use super::AppleError;
use super::state::MachineName;
use crate::backend::{HostKeyPolicy, Hostname, PinnedHostKey, SshTarget, SshUser};
use crate::config::{CoopConfig, Instance};

const ED25519_PREFIX: &str = "ssh-ed25519";
/// Base64 of the ed25519 wire blob: `u32 len || "ssh-ed25519" || u32 len || 32 bytes`.
const ED25519_BLOB_LEN: usize = 4 + 11 + 4 + 32;

/// Why text is not a single OpenSSH ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum KeyFormatError {
    #[error("must be exactly one line")]
    NotOneLine,
    #[error("is malformed")]
    Malformed,
    #[error("has type {0:?}, not ssh-ed25519")]
    WrongType(String),
    #[error("is not valid base64")]
    NotBase64,
    #[error("is not an ed25519 public key")]
    BadBlob,
}

/// An OpenSSH ed25519 public key: its base64 text and decoded wire blob.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ed25519Key {
    base64: String,
    blob: Vec<u8>,
}

impl Ed25519Key {
    /// Parse exactly one `ssh-ed25519 <base64> [comment]` line, dropping the
    /// comment.
    fn parse(text: &str) -> std::result::Result<Self, KeyFormatError> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let (Some(line), None) = (lines.next(), lines.next()) else {
            return Err(KeyFormatError::NotOneLine);
        };
        let mut fields = line.split_whitespace();
        let (Some(kind), Some(b64)) = (fields.next(), fields.next()) else {
            return Err(KeyFormatError::Malformed);
        };
        if kind != ED25519_PREFIX {
            return Err(KeyFormatError::WrongType(super::cli::sanitize_for_display(
                kind,
            )));
        }
        let blob = crate::base64::decode(b64).ok_or(KeyFormatError::NotBase64)?;
        let well_formed = blob.len() == ED25519_BLOB_LEN
            && blob[..4] == [0, 0, 0, 11]
            && &blob[4..15] == ED25519_PREFIX.as_bytes()
            && blob[15..19] == [0, 0, 0, 32];
        if !well_formed {
            return Err(KeyFormatError::BadBlob);
        }
        Ok(Self {
            base64: b64.to_string(),
            blob,
        })
    }

    /// OpenSSH-style `SHA256:<base64>` fingerprint.
    fn fingerprint(&self) -> String {
        let digest = sha2::Sha256::digest(&self.blob);
        format!(
            "SHA256:{}",
            crate::base64::encode(&digest).trim_end_matches('=')
        )
    }
}

/// Fingerprint of one of coop's own ed25519 public keys (not a guest's).
pub(crate) fn ed25519_fingerprint(text: &str) -> std::result::Result<String, KeyFormatError> {
    Ed25519Key::parse(text).map(|k| k.fingerprint())
}

/// A validated guest host public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPublicKey(Ed25519Key);

impl HostPublicKey {
    /// Parse exactly one `ssh-ed25519 <base64> [comment]` line. The comment is
    /// guest-controlled and dropped.
    pub(crate) fn parse(text: &str) -> Result<Self> {
        Ed25519Key::parse(text)
            .map(Self)
            .map_err(|e| AppleError::HostKeyChanged(format!("guest host public key {e}")).into())
    }

    /// OpenSSH-style `SHA256:<base64>` fingerprint.
    pub(crate) fn fingerprint(&self) -> String {
        self.0.fingerprint()
    }

    fn known_hosts_line(&self, alias: &Hostname) -> String {
        format!("{alias} {ED25519_PREFIX} {}\n", self.0.base64)
    }
}

/// Stable per-instance `HostKeyAlias`, independent of the (reassignable) IP.
pub(crate) fn host_key_alias(machine: &MachineName) -> Result<Hostname> {
    Hostname::new(format!("{machine}.coop-apple"))
}

/// Record the first-boot key for a newly created machine. Refuses to replace
/// an existing pin: re-enrollment is an explicit operator action.
pub(crate) fn enroll(inst: &Instance, machine: &MachineName, key: &HostPublicKey) -> Result<()> {
    let path = super::state::known_hosts_path(inst);
    if path.exists() {
        bail!(AppleError::HostKeyChanged(format!(
            "instance '{}' already has a pinned host key at {}; refusing to re-enroll",
            inst.name,
            path.display()
        )));
    }
    write_pin(inst, machine, key)
}

/// Replace the pin after coop itself replaced the instance's disk (`coop
/// restore`), which removes the guest's host keys. The caller must only
/// reach this on that path — never because a guest presented a new key.
pub(crate) fn reenroll_after_disk_replacement(
    inst: &Instance,
    machine: &MachineName,
    key: &HostPublicKey,
) -> Result<()> {
    write_pin(inst, machine, key)
}

fn write_pin(inst: &Instance, machine: &MachineName, key: &HostPublicKey) -> Result<()> {
    crate::fs_util::atomic_write_with_mode(
        &super::state::known_hosts_path(inst),
        &key.known_hosts_line(&host_key_alias(machine)?),
        0o600,
    )
    .context("Failed to write pinned host key")
}

/// Compare a freshly read key against the pin recorded at enrollment.
pub(crate) fn check_pin(inst: &Instance, machine: &MachineName, key: &HostPublicKey) -> Result<()> {
    let path = super::state::known_hosts_path(inst);
    let pinned = std::fs::read_to_string(&path).map_err(|_| {
        AppleError::HostKeyChanged(format!(
            "instance '{}' has no pinned host key; recreate the instance",
            inst.name
        ))
    })?;
    if pinned != key.known_hosts_line(&host_key_alias(machine)?) {
        bail!(AppleError::HostKeyChanged(format!(
            "the SSH host key of instance '{}' changed (now {}); refusing to connect. \
             Recreate the instance, or re-enroll deliberately after review.",
            inst.name,
            key.fingerprint()
        )));
    }
    Ok(())
}

/// Build the pinned SSH target for `machine` at its current address.
pub(crate) fn pinned_target(
    cfg: &CoopConfig,
    inst: &Instance,
    machine: &MachineName,
    ip: Ipv4Addr,
    user: &crate::guest::GuestUser,
) -> Result<SshTarget> {
    let known_hosts = super::state::known_hosts_path(inst);
    // The path is passed to ssh as a (possibly quoted) option value and
    // written into `~/.ssh/config`; a quote or control character there
    // could change how ssh parses it.
    if known_hosts
        .to_string_lossy()
        .chars()
        .any(|c| c == '"' || c == '\'' || c.is_control())
    {
        bail!(
            "data directory path {} contains a quote or control character; SSH \
             options cannot carry it safely",
            known_hosts.display()
        );
    }
    if !known_hosts.exists() {
        bail!(AppleError::HostKeyChanged(format!(
            "instance '{}' has no pinned host key at {}; recreate the instance",
            inst.name,
            known_hosts.display()
        )));
    }
    Ok(SshTarget {
        host: Hostname::from(ip),
        port: NonZeroU16::new(22).context("port 22")?,
        user: SshUser::new(user.as_str())?,
        key_path: cfg.ssh_key_path(),
        host_keys: HostKeyPolicy::Pinned(PinnedHostKey {
            known_hosts,
            alias: host_key_alias(machine)?,
        }),
    })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    // Throwaway keys generated with `ssh-keygen -t ed25519` for these tests.
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINiqkOnkRV06x+SuorkF+O3KdBTVFznIV0+b58cidW1N root@guest";
    const OTHER_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKB2Bohi4Rqvfx9oW+/9UovsYrOdYeFuWqvUuZpjig+1 x";

    fn machine() -> MachineName {
        MachineName::new("coop-0a1b2c3d-00112233445566ff").unwrap()
    }

    fn inst(dir: &std::path::Path) -> Instance {
        Instance {
            name: crate::config::InstanceName::new("t").unwrap(),
            index: crate::config::InstanceIndex::new(0).unwrap(),
            dir: dir.to_path_buf(),
            image: crate::config::ImageName::new("default").unwrap(),
        }
    }

    #[test]
    fn parses_one_ed25519_key_and_fingerprints_it() {
        let key = HostPublicKey::parse(KEY).unwrap();
        // Matches `ssh-keygen -lf` for the same key.
        assert_eq!(
            key.fingerprint(),
            "SHA256:10O2vYbKkmA/sBuRrfwbSNiR5pAFM/qtkqldbUJESvk"
        );
    }

    /// coop's own key fingerprints the same way, but a bad one is not
    /// reported as a changed guest host key.
    #[test]
    fn own_key_fingerprint_is_neutral() {
        assert_eq!(
            ed25519_fingerprint(KEY).unwrap(),
            HostPublicKey::parse(KEY).unwrap().fingerprint()
        );
        assert_eq!(
            ed25519_fingerprint("ssh-rsa AAAA x").unwrap_err(),
            KeyFormatError::WrongType("ssh-rsa".into())
        );
        let err = HostPublicKey::parse("ssh-rsa AAAA x").unwrap_err();
        assert!(matches!(
            err.downcast_ref::<AppleError>(),
            Some(AppleError::HostKeyChanged(m)) if m.contains("not ssh-ed25519")
        ));
    }

    #[test]
    fn rejects_other_types_multiple_lines_and_garbage() {
        assert!(HostPublicKey::parse("").is_err());
        assert!(HostPublicKey::parse(&format!("{KEY}\n{KEY}")).is_err());
        assert!(HostPublicKey::parse("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ== x").is_err());
        assert!(HostPublicKey::parse("ssh-ed25519 not*base64").is_err());
        assert!(HostPublicKey::parse("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5").is_err());
    }

    #[test]
    fn enroll_once_then_pin_is_enforced() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let inst = inst(tmp.path());
        let key = HostPublicKey::parse(KEY).unwrap();
        enroll(&inst, &machine(), &key).unwrap();
        let mode = std::fs::metadata(super::super::state::known_hosts_path(&inst))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        check_pin(&inst, &machine(), &key).unwrap();

        // Re-enrollment is refused.
        assert!(enroll(&inst, &machine(), &key).is_err());

        // A different key is a hard error.
        let other = HostPublicKey::parse(OTHER_KEY).unwrap();
        let err = check_pin(&inst, &machine(), &other).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<AppleError>(),
            Some(AppleError::HostKeyChanged(_))
        ));
    }

    #[test]
    fn missing_pin_blocks_target() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = inst(tmp.path());
        let cfg = CoopConfig::default();
        let user = crate::guest::GuestUser::default();
        let key = HostPublicKey::parse(KEY).unwrap();
        assert!(check_pin(&inst, &machine(), &key).is_err());
        assert!(pinned_target(&cfg, &inst, &machine(), Ipv4Addr::new(10, 0, 0, 2), &user).is_err());
        enroll(&inst, &machine(), &key).unwrap();
        let target =
            pinned_target(&cfg, &inst, &machine(), Ipv4Addr::new(10, 0, 0, 2), &user).unwrap();
        assert!(matches!(target.host_keys, HostKeyPolicy::Pinned(_)));
        assert!(
            target
                .ssh_opts()
                .iter()
                .any(|o| o == "StrictHostKeyChecking=yes")
        );
    }

    /// The known-hosts path goes into ssh options and `~/.ssh/config`, so a
    /// data directory whose path could change how ssh parses it is refused.
    #[test]
    fn pinned_target_refuses_unsafe_data_dir() {
        let cfg = CoopConfig::default();
        let user = crate::guest::GuestUser::default();
        let key = HostPublicKey::parse(KEY).unwrap();
        for bad in ["dq\"x", "sq'x", "nl\nx"] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join(bad);
            std::fs::create_dir(&dir).unwrap();
            let inst = inst(&dir);
            enroll(&inst, &machine(), &key).unwrap();
            let err = pinned_target(&cfg, &inst, &machine(), Ipv4Addr::new(10, 0, 0, 2), &user)
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("quote or control"),
                "{bad:?}: {err:#}"
            );
        }
    }
}
