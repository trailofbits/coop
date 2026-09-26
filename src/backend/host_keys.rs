//! Host-key policy for guest SSH connections and the option values it emits.
//!
//! Pure option builders, split out of `backend.rs` (whose shell-out code is
//! excluded from mutation testing) so cargo-mutants covers them.

use std::path::PathBuf;

use crate::backend::Hostname;

/// How the host authenticates a guest's SSH host key.
///
/// Each backend picks one policy when it builds an [`SshTarget`], and every
/// transport (ssh, scp, rsync, multiplexed probes, the editor `~/.ssh/config`
/// block) derives its options from that single choice, so a transport cannot
/// silently downgrade checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// No host-key verification. Firecracker and Lima reach their guest over a
    /// per-instance TAP device or a hostagent-owned loopback forward that no
    /// other guest can occupy, so there is nothing to pin against.
    Unverified,
    /// Verify against a per-instance known-hosts file enrolled from a trusted
    /// channel, looked up under a stable alias rather than the (reassignable)
    /// address.
    #[cfg_attr(
        all(not(feature = "apple-container"), not(test)),
        expect(dead_code, reason = "constructed only by the apple-container backend")
    )]
    Pinned(PinnedHostKey),
}

/// A per-instance host-key pin: the known-hosts file holding the enrolled key
/// and the alias it is recorded under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedHostKey {
    pub known_hosts: PathBuf,
    pub alias: Hostname,
}

impl HostKeyPolicy {
    /// `ssh -o` option values (without the `-o`) for this policy, in order.
    pub(crate) fn ssh_options(&self) -> Vec<String> {
        match self {
            Self::Unverified => vec![
                "StrictHostKeyChecking=no".into(),
                "UserKnownHostsFile=/dev/null".into(),
            ],
            Self::Pinned(pin) => vec![
                "StrictHostKeyChecking=yes".into(),
                // OpenSSH splits this option's value on whitespace into a
                // list of files; quoting keeps a path with a space whole in
                // `-o`, rsync `-e`, and `~/.ssh/config` alike.
                format!(
                    "UserKnownHostsFile={}",
                    quote_ssh_value(&pin.known_hosts.display().to_string())
                ),
                "GlobalKnownHostsFile=/dev/null".into(),
                format!("HostKeyAlias={}", pin.alias),
                "UpdateHostKeys=no".into(),
                "ForwardAgent=no".into(),
                // Authentication uses coop's key file only; never consult the
                // host agent for a guest-facing connection.
                "IdentityAgent=none".into(),
            ],
        }
    }

    /// `~/.ssh/config` directive lines (`Key Value`) for this policy.
    /// Values are already quoted where needed by [`quote_ssh_value`].
    pub fn ssh_config_lines(&self) -> Vec<String> {
        self.ssh_options()
            .into_iter()
            .map(|opt| opt.replacen('=', " ", 1))
            .collect()
    }
}

/// Double-quote an SSH option value that contains whitespace. Assumes the
/// value has no quote or control character: pinned targets reject those
/// (`apple_container::ssh::pinned_target`); other callers pass paths from the
/// user's own `data_dir`.
pub(crate) fn quote_ssh_value(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::{HostKeyPolicy, Hostname, PinnedHostKey, quote_ssh_value};

    #[test]
    fn quote_ssh_value_quotes_only_whitespace_values() {
        assert_eq!(quote_ssh_value("/a/b"), "/a/b");
        assert_eq!(quote_ssh_value("/a b"), "\"/a b\"");
        assert_eq!(quote_ssh_value("/a\tb"), "\"/a\tb\"");
        assert_eq!(quote_ssh_value(""), "");
    }

    #[test]
    fn unverified_policy_disables_checking() {
        assert_eq!(
            HostKeyPolicy::Unverified.ssh_config_lines(),
            vec![
                "StrictHostKeyChecking no".to_string(),
                "UserKnownHostsFile /dev/null".to_string(),
            ]
        );
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "tests")]
    fn pinned_policy_config_lines_split_on_first_equals_only() {
        let pinned = HostKeyPolicy::Pinned(PinnedHostKey {
            known_hosts: "/state dir/known=hosts".into(),
            alias: Hostname::new("coop-abc").unwrap(),
        });
        assert_eq!(
            pinned.ssh_config_lines(),
            vec![
                "StrictHostKeyChecking yes".to_string(),
                "UserKnownHostsFile \"/state dir/known=hosts\"".to_string(),
                "GlobalKnownHostsFile /dev/null".to_string(),
                "HostKeyAlias coop-abc".to_string(),
                "UpdateHostKeys no".to_string(),
                "ForwardAgent no".to_string(),
                "IdentityAgent none".to_string(),
            ]
        );
    }
}
