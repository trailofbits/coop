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

/// Compile-time profile definition. Single source of truth for all
/// builtin profiles — name, packages, scripts, and plugins are
/// colocated in `BUILTIN_PROFILES`.
pub struct BuiltinProfile {
    pub name: &'static str,
    pub apt_packages: &'static [&'static str],
    pub pre_install: Option<&'static str>,
    pub post_install: Option<&'static str>,
    pub marketplaces: &'static [&'static str],
    pub plugins: &'static [&'static str],
}

/// Owned profile definition produced by `lookup_profile`. Handles both
/// builtin (converted from static data) and custom (cloned from config).
pub struct ProfileDef {
    pub apt_packages: Vec<String>,
    pub pre_install: Option<String>,
    pub post_install: Option<String>,
    pub marketplaces: Vec<String>,
    pub plugins: Vec<String>,
}

impl From<&BuiltinProfile> for ProfileDef {
    fn from(bp: &BuiltinProfile) -> Self {
        Self {
            apt_packages: bp.apt_packages.iter().map(|s| (*s).to_owned()).collect(),
            pre_install: bp.pre_install.map(str::to_owned),
            post_install: bp.post_install.map(str::to_owned),
            marketplaces: bp.marketplaces.iter().map(|s| (*s).to_owned()).collect(),
            plugins: bp.plugins.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

pub const BUILTIN_PROFILES: &[BuiltinProfile] = &[
    BuiltinProfile {
        name: "python",
        apt_packages: &["python3", "python3-pip", "python3-venv"],
        pre_install: None,
        post_install: None,
        marketplaces: &[],
        plugins: &[],
    },
    BuiltinProfile {
        name: "node",
        apt_packages: &["nodejs"],
        pre_install: Some(include_str!("../scripts/guest/profiles/node-pre.sh")),
        post_install: None,
        marketplaces: &[],
        plugins: &[],
    },
    BuiltinProfile {
        name: "c",
        apt_packages: &["clang", "llvm", "gdb", "valgrind", "cmake"],
        pre_install: None,
        post_install: None,
        marketplaces: &[],
        plugins: &["clangd-lsp@claude-plugins-official"],
    },
    BuiltinProfile {
        name: "fuzz",
        apt_packages: &["clang", "llvm", "afl++", "lcov"],
        pre_install: None,
        post_install: None,
        marketplaces: &[],
        plugins: &[],
    },
    BuiltinProfile {
        name: "rust",
        apt_packages: &[],
        pre_install: None,
        post_install: Some(include_str!("../scripts/guest/profiles/rust-post.sh")),
        marketplaces: &[],
        plugins: &["rust-analyzer-lsp@claude-plugins-official"],
    },
    BuiltinProfile {
        name: "go",
        apt_packages: &["golang"],
        pre_install: None,
        post_install: None,
        marketplaces: &[],
        plugins: &[],
    },
];

fn lookup_builtin(name: &str) -> Option<&'static BuiltinProfile> {
    BUILTIN_PROFILES.iter().find(|bp| bp.name == name)
}

/// Look up a profile by name. Checks custom profiles first, then builtins.
pub fn lookup_profile(name: &str, custom: &HashMap<String, CustomProfile>) -> Result<ProfileDef> {
    if let Some(cp) = custom.get(name) {
        return Ok(ProfileDef {
            apt_packages: cp.apt_packages.clone(),
            pre_install: cp.pre_install.clone(),
            post_install: cp.post_install.clone(),
            marketplaces: cp.marketplaces.clone(),
            plugins: cp.plugins.clone(),
        });
    }

    if let Some(bp) = lookup_builtin(name) {
        return Ok(ProfileDef::from(bp));
    }

    let mut available: Vec<&str> = BUILTIN_PROFILES.iter().map(|bp| bp.name).collect();
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
#[expect(clippy::panic, reason = "tests use panic for assertion failures")]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;

    #[test]
    fn all_builtins_resolve() {
        let custom = HashMap::new();
        for bp in BUILTIN_PROFILES {
            let def = lookup_profile(bp.name, &custom)
                .unwrap_or_else(|_| panic!("builtin '{}' failed to resolve", bp.name));
            assert_eq!(def.apt_packages.len(), bp.apt_packages.len());
        }
    }

    #[test]
    fn unknown_profile_fails() {
        let custom = HashMap::new();
        assert!(lookup_profile("nonexistent", &custom).is_err());
    }

    #[test]
    fn custom_overrides_builtin() {
        let mut custom = HashMap::new();
        custom.insert(
            "python".to_owned(),
            CustomProfile {
                apt_packages: vec!["custom-python".to_owned()],
                pre_install: None,
                post_install: None,
                marketplaces: vec![],
                plugins: vec![],
            },
        );
        let def = lookup_profile("python", &custom).unwrap();
        assert_eq!(def.apt_packages, vec!["custom-python"]);
    }
}
