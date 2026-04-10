// Shared guest provisioning constants used by both Lima and Firecracker.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::config::{CoopConfig, CustomProfile};

/// Username for the non-root guest user. Both backends ensure this user
/// exists at uid 1000. On Firecracker's rootfs the `ubuntu` user already
/// exists; on Lima, cloud-init may replace it with a host-mirror user,
/// so the provisioning script evicts that user and recreates `ubuntu`.
pub const GUEST_USER: &str = "ubuntu";

/// Absolute path to the Claude Code binary in the guest.
///
/// The installer puts it under the user's home directory. Bootstrap
/// commands and verification use this path directly rather than
/// relying on PATH (non-interactive SSH sessions don't source
/// `.bashrc`/`.profile`).
pub const CLAUDE_BIN: &str = "/home/ubuntu/.local/bin/claude";

/// Binaries that must exist in the guest image after provisioning.
/// Absolute paths are checked directly; bare names are looked up via
/// `command -v` (i.e. must be in the default system PATH).
pub const REQUIRED_GUEST_BINARIES: &[&str] = &["/usr/bin/docker", "/usr/bin/gh", CLAUDE_BIN];

pub const SCRIPT_GH_REPO: &str = include_str!("../scripts/guest/gh-cli-repo.sh");
pub const SCRIPT_DOCKER_REPO: &str = include_str!("../scripts/guest/docker-repo.sh");
pub const SCRIPT_CLAUDE_CODE: &str = include_str!("../scripts/guest/claude-code.sh");

pub const BASE_PACKAGES: &[&str] = &[
    "openssh-server",
    "curl",
    "wget",
    "git",
    "build-essential",
    "ca-certificates",
    "gnupg",
    "lsb-release",
    "sudo",
    "iproute2",
    "iptables",
    "kmod",
    "procps",
    "jq",
    "rsync",
    "tmux",
    "unzip",
    "zip",
    "file",
    "less",
];

pub const GH_PACKAGES: &[&str] = &["gh"];

pub const DOCKER_PACKAGES: &[&str] = &[
    "docker-ce",
    "docker-ce-cli",
    "containerd.io",
    "docker-compose-plugin",
];

// ── Profile definitions ───────────────────────────────────────

pub struct ProfileDef {
    pub apt_packages: Vec<String>,
    pub pre_install: Option<String>,
    pub post_install: Option<String>,
    pub marketplaces: Vec<String>,
    pub plugins: Vec<String>,
}

/// Single source of truth for builtin profile names and descriptions.
/// `lookup_builtin` must have a matching arm for every non-"full" entry;
/// the `builtin_profile_entries_match_lookup` test enforces this.
const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("python", "Python 3 with pip and venv"),
    ("node", "Node.js 22 via NodeSource"),
    ("c", "Clang, LLVM, GDB, Valgrind, CMake"),
    ("fuzz", "Clang, LLVM, AFL++, lcov"),
    ("rust", "Rust via rustup"),
    ("go", "Go"),
    ("full", "python + node + c + fuzz + rust + go"),
];

/// Returns `(name, description)` for each builtin profile.
pub fn builtin_profile_entries() -> &'static [(&'static str, &'static str)] {
    BUILTIN_PROFILES
}

fn lookup_builtin(name: &str) -> Option<ProfileDef> {
    match name {
        "python" => Some(ProfileDef {
            apt_packages: vec![
                "python3".into(),
                "python3-pip".into(),
                "python3-venv".into(),
            ],
            pre_install: None,
            post_install: None,
            marketplaces: vec![],
            plugins: vec![],
        }),
        "node" => Some(ProfileDef {
            apt_packages: vec!["nodejs".into()],
            pre_install: Some(include_str!("../scripts/guest/profiles/node-pre.sh").into()),
            post_install: None,
            marketplaces: vec![],
            plugins: vec![],
        }),
        "c" => Some(ProfileDef {
            apt_packages: vec![
                "clang".into(),
                "llvm".into(),
                "gdb".into(),
                "valgrind".into(),
                "cmake".into(),
            ],
            pre_install: None,
            post_install: None,
            marketplaces: vec![],
            plugins: vec!["clangd-lsp@claude-plugins-official".into()],
        }),
        "fuzz" => Some(ProfileDef {
            apt_packages: vec!["clang".into(), "llvm".into(), "afl++".into(), "lcov".into()],
            pre_install: None,
            post_install: None,
            marketplaces: vec![],
            plugins: vec![],
        }),
        "rust" => Some(ProfileDef {
            apt_packages: vec![],
            pre_install: None,
            post_install: Some(include_str!("../scripts/guest/profiles/rust-post.sh").into()),
            marketplaces: vec![],
            plugins: vec!["rust-analyzer-lsp@claude-plugins-official".into()],
        }),
        "go" => Some(ProfileDef {
            apt_packages: vec!["golang".into()],
            pre_install: None,
            post_install: None,
            marketplaces: vec![],
            plugins: vec![],
        }),
        _ => None,
    }
}

/// Look up a profile by name. Checks custom profiles first, then
/// built-in ones. The `full` meta-profile expands all built-in profiles.
pub fn lookup_profile(name: &str, custom: &HashMap<String, CustomProfile>) -> Result<ProfileDef> {
    // Custom profiles take precedence
    if let Some(cp) = custom.get(name) {
        return Ok(ProfileDef {
            apt_packages: cp.apt_packages.clone(),
            pre_install: cp.pre_install.clone(),
            post_install: cp.post_install.clone(),
            marketplaces: cp.marketplaces.clone(),
            plugins: cp.plugins.clone(),
        });
    }

    if name == "full" {
        let all = ["python", "node", "c", "fuzz", "rust", "go"];
        let mut apt_packages = Vec::new();
        let mut pre_parts = Vec::new();
        let mut post_parts = Vec::new();
        let mut marketplaces = Vec::new();
        let mut plugins = Vec::new();
        for sub in all {
            let Some(def) = lookup_builtin(sub) else {
                bail!("BUG: built-in profile '{sub}' missing from full expansion");
            };
            apt_packages.extend(def.apt_packages);
            if let Some(s) = def.pre_install {
                pre_parts.push(s);
            }
            if let Some(s) = def.post_install {
                post_parts.push(s);
            }
            marketplaces.extend(def.marketplaces);
            plugins.extend(def.plugins);
        }
        apt_packages.sort_unstable();
        apt_packages.dedup();
        marketplaces.sort_unstable();
        marketplaces.dedup();
        plugins.sort_unstable();
        plugins.dedup();
        return Ok(ProfileDef {
            apt_packages,
            pre_install: if pre_parts.is_empty() {
                None
            } else {
                Some(pre_parts.join("\n"))
            },
            post_install: if post_parts.is_empty() {
                None
            } else {
                Some(post_parts.join("\n"))
            },
            marketplaces,
            plugins,
        });
    }

    if let Some(def) = lookup_builtin(name) {
        return Ok(def);
    }

    let mut available: Vec<&str> = BUILTIN_PROFILES.iter().map(|(n, _)| *n).collect();
    let mut custom_names: Vec<&str> = custom.keys().map(String::as_str).collect();
    custom_names.sort_unstable();
    available.extend(custom_names);

    bail!(
        "Unknown profile: {name}\n\
         Available profiles: {}",
        available.join(", ")
    )
}

/// Collect combined marketplace and plugin lists from global config
/// and all active profiles. Results are sorted and deduplicated.
pub fn collect_baked_lists(
    cfg: &CoopConfig,
    profiles: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut marketplaces = cfg.claude.marketplaces.clone();
    let mut plugins = cfg.claude.plugins.clone();

    for name in profiles {
        let def = lookup_profile(name, &cfg.profiles)?;
        marketplaces.extend(def.marketplaces);
        plugins.extend(def.plugins);
    }

    marketplaces.sort_unstable();
    marketplaces.dedup();
    plugins.sort_unstable();
    plugins.dedup();

    Ok((marketplaces, plugins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profile_entries_lists_all() {
        let entries = builtin_profile_entries();
        let names: Vec<&str> = entries.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec!["python", "node", "c", "fuzz", "rust", "go", "full"]
        );
        for &(name, desc) in entries {
            assert!(
                !desc.is_empty(),
                "builtin profile '{name}' has empty description"
            );
        }
    }

    #[test]
    fn builtin_profile_entries_match_lookup() {
        let custom = HashMap::new();
        for &(name, _) in builtin_profile_entries() {
            assert!(
                lookup_profile(name, &custom).is_ok(),
                "builtin_profile_entries lists '{name}' but lookup_profile rejects it"
            );
        }
    }
}
