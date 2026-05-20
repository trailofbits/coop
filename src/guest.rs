// Shared guest provisioning constants used by both Lima and Firecracker.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::config::{CoopConfig, CustomProfile};

/// Devcontainer feature ids (bare names) that map to builtin profiles.
/// The id is the same string as the builtin name; this slice acts as the
/// allow-list so an unknown feature returns `None` rather than silently
/// resolving against an unrelated builtin added later.
const FEATURE_IDS: &[&str] = &["python", "node", "c", "fuzz", "rust", "go"];

/// Look up a builtin profile by its devcontainer feature id (a bare
/// name such as `rust`, after stripping any `ghcr.io/...:tag` prefix).
pub fn builtin_for_feature(id: &str) -> Option<&'static BuiltinProfile> {
    if FEATURE_IDS.contains(&id) {
        lookup_builtin(id)
    } else {
        None
    }
}

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

/// Absolute path to the Codex CLI binary in the guest.
///
/// The installer places a standalone binary in `/usr/local/bin`, so
/// both bootstrap and verification can rely on a stable path.
pub const CODEX_BIN: &str = "/usr/local/bin/codex";

/// Binaries that must exist in the guest image after provisioning.
/// Absolute paths are checked directly; bare names are looked up via
/// `command -v` (i.e. must be in the default system PATH).
pub const REQUIRED_GUEST_BINARIES: &[&str] =
    &["/usr/bin/docker", "/usr/bin/gh", CLAUDE_BIN, CODEX_BIN];

pub const SCRIPT_GH_REPO: &str = include_str!("../scripts/guest/gh-cli-repo.sh");
pub const SCRIPT_DOCKER_REPO: &str = include_str!("../scripts/guest/docker-repo.sh");
pub const SCRIPT_CLAUDE_CODE: &str = include_str!("../scripts/guest/claude-code.sh");
pub const SCRIPT_CODEX: &str = include_str!("../scripts/guest/codex.sh");

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

/// Resolved profile definition. Carries the profile name alongside its
/// effective contents so consumers don't need to re-look-up by name.
/// Produced once at the config boundary by [`resolve_profiles`].
#[derive(Debug, Clone)]
pub struct ProfileDef {
    pub name: String,
    pub apt_packages: Vec<String>,
    pub pre_install: Option<String>,
    pub post_install: Option<String>,
    pub marketplaces: Vec<String>,
    pub plugins: Vec<String>,
}

impl From<&BuiltinProfile> for ProfileDef {
    fn from(bp: &BuiltinProfile) -> Self {
        Self {
            name: bp.name.to_owned(),
            apt_packages: bp.apt_packages.iter().map(|s| (*s).to_owned()).collect(),
            pre_install: bp.pre_install.map(str::to_owned),
            post_install: bp.post_install.map(str::to_owned),
            marketplaces: bp.marketplaces.iter().map(|s| (*s).to_owned()).collect(),
            plugins: bp.plugins.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

impl ProfileDef {
    fn from_custom(name: &str, cp: &CustomProfile) -> Self {
        Self {
            name: name.to_owned(),
            apt_packages: cp.apt_packages.clone(),
            pre_install: cp.pre_install.clone(),
            post_install: cp.post_install.clone(),
            marketplaces: cp.marketplaces.clone(),
            plugins: cp.plugins.clone(),
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

fn resolve_one(name: &str, custom: &HashMap<String, CustomProfile>) -> Option<ProfileDef> {
    if let Some(cp) = custom.get(name) {
        return Some(ProfileDef::from_custom(name, cp));
    }
    lookup_builtin(name).map(ProfileDef::from)
}

fn available_profiles(custom: &HashMap<String, CustomProfile>) -> Vec<&str> {
    let mut available: Vec<&str> = BUILTIN_PROFILES.iter().map(|bp| bp.name).collect();
    let mut custom_names: Vec<&str> = custom.keys().map(String::as_str).collect();
    custom_names.sort_unstable();
    available.extend(custom_names);
    available
}

/// Resolve a list of profile names into [`ProfileDef`] values.
///
/// All unknown names are collected and reported in a single error so
/// the caller sees every offender at once. Custom profiles shadow
/// builtins with the same name.
pub fn resolve_profiles(
    names: &[String],
    custom: &HashMap<String, CustomProfile>,
) -> Result<Vec<ProfileDef>> {
    let mut resolved = Vec::with_capacity(names.len());
    let mut unknown: Vec<&str> = Vec::new();
    for name in names {
        match resolve_one(name, custom) {
            Some(def) => resolved.push(def),
            None => unknown.push(name.as_str()),
        }
    }
    if !unknown.is_empty() {
        bail!(
            "Unknown profile(s): {}\n\
             Available profiles: {}",
            unknown.join(", "),
            available_profiles(custom).join(", "),
        );
    }
    Ok(resolved)
}

/// Look up a single profile by name. Convenience wrapper around
/// [`resolve_profiles`] for the `coop profiles show <name>` path.
pub fn lookup_profile(name: &str, custom: &HashMap<String, CustomProfile>) -> Result<ProfileDef> {
    resolve_one(name, custom).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown profile: {name}\n\
             Available profiles: {}",
            available_profiles(custom).join(", "),
        )
    })
}

/// Collect combined marketplace and plugin lists from global config
/// and all active profiles. Results are sorted and deduplicated.
pub fn collect_baked_lists(
    cfg: &CoopConfig,
    profiles: &[ProfileDef],
) -> (Vec<String>, Vec<String>) {
    let mut marketplaces = cfg.claude.marketplaces.clone();
    let mut plugins = cfg.claude.plugins.clone();

    for def in profiles {
        marketplaces.extend(def.marketplaces.iter().cloned());
        plugins.extend(def.plugins.iter().cloned());
    }

    marketplaces.sort_unstable();
    marketplaces.dedup();
    plugins.sort_unstable();
    plugins.dedup();

    (marketplaces, plugins)
}

#[cfg(test)]
#[expect(clippy::panic, reason = "tests use panic for assertion failures")]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;

    #[test]
    fn required_guest_binaries_include_codex() {
        assert!(
            REQUIRED_GUEST_BINARIES.contains(&"/usr/local/bin/codex"),
            "guest image should include codex in the default PATH",
        );
    }

    #[test]
    fn codex_script_installs_extracted_binary_not_archive() {
        assert!(
            SCRIPT_CODEX.contains("BIN=\"$TMPDIR/${ASSET%.tar.gz}\""),
            "Codex installer should target the extracted binary path directly",
        );
    }

    #[test]
    fn all_builtins_resolve() {
        let custom = HashMap::new();
        for bp in BUILTIN_PROFILES {
            let def = lookup_profile(bp.name, &custom)
                .unwrap_or_else(|_| panic!("builtin '{}' failed to resolve", bp.name));
            assert_eq!(def.name, bp.name);
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
        assert_eq!(def.name, "python");
        assert_eq!(def.apt_packages, vec!["custom-python"]);
    }

    #[test]
    fn resolve_profiles_reports_all_unknowns() {
        let custom = HashMap::new();
        let names = vec!["rust".to_owned(), "bogus1".to_owned(), "bogus2".to_owned()];
        let err = resolve_profiles(&names, &custom).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bogus1"), "missing bogus1 in: {msg}");
        assert!(msg.contains("bogus2"), "missing bogus2 in: {msg}");
    }

    #[test]
    fn resolve_profiles_preserves_order_and_resolves_all() {
        let mut custom = HashMap::new();
        custom.insert(
            "data".to_owned(),
            CustomProfile {
                apt_packages: vec!["pandas".to_owned()],
                pre_install: None,
                post_install: None,
                marketplaces: vec![],
                plugins: vec![],
            },
        );
        let names = vec!["rust".to_owned(), "data".to_owned(), "node".to_owned()];
        let defs = resolve_profiles(&names, &custom).unwrap();
        let resolved_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(resolved_names, vec!["rust", "data", "node"]);
    }

    #[test]
    fn builtin_for_feature_matches_known_ids() {
        for id in FEATURE_IDS {
            let bp = builtin_for_feature(id)
                .unwrap_or_else(|| panic!("feature '{id}' should resolve to a builtin"));
            assert_eq!(bp.name, *id);
        }
        assert!(builtin_for_feature("nonexistent").is_none());
    }
}
