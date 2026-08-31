// Shared guest provisioning constants used by both Lima and Firecracker.

use std::collections::HashMap;
use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{CoopConfig, CustomProfile};
use crate::paths::GuestPath;

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

/// Default username when neither the CLI nor a devcontainer pins one.
/// Matches the user that Firecracker's CI rootfs already ships with and
/// that Lima's `useradd` block creates at uid 1000.
pub const DEFAULT_GUEST_USER: &str = "ubuntu";

const MAX_GUEST_USER_LEN: usize = 32;

/// Validated guest username, persisted in `TemplateConfig` and threaded
/// through both backends.
///
/// Enforces the POSIX-portable pattern `[a-z_][a-z0-9_-]{0,31}` and
/// rejects `root` so the rest of coop can assume an unprivileged uid-1000
/// account. The same rules as `backend::SshUser` plus the root exclusion,
/// so callers can losslessly convert to an `SshUser` for SSH plumbing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GuestUser(String);

impl GuestUser {
    /// Construct a validated guest user. Returns an error when the name
    /// is empty, exceeds 32 characters, contains characters outside the
    /// portable POSIX set, or is `root`.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        Self::validate(&name)?;
        Ok(Self(name))
    }

    /// `clap` value parser. Equivalent to `GuestUser::new` but with the
    /// `(&str) -> Result<Self>` signature clap's `value_parser` expects.
    pub fn parse(s: &str) -> Result<Self> {
        Self::new(s)
    }

    fn validate(name: &str) -> Result<()> {
        if name.is_empty() {
            bail!("guest user must not be empty");
        }
        if name.len() > MAX_GUEST_USER_LEN {
            bail!(
                "guest user too long ({} chars, max {MAX_GUEST_USER_LEN})",
                name.len()
            );
        }
        if name == "root" {
            bail!("guest user must not be 'root'; coop requires an unprivileged uid-1000 account");
        }
        for (i, c) in name.chars().enumerate() {
            let ok = if i == 0 {
                matches!(c, 'a'..='z' | '_')
            } else {
                matches!(c, 'a'..='z' | '0'..='9' | '_' | '-')
            };
            if !ok {
                if i == 0 {
                    bail!("guest user must start with [a-z_], got {c:?}");
                }
                bail!(
                    "guest user contains invalid character {c:?} \
                     (allowed: a-z, 0-9, '_', '-')"
                );
            }
        }
        Ok(())
    }

    /// Borrow as a string slice for paths, scripts, and shell composition.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Absolute home directory of this user inside the guest. Returned
    /// as a `GuestPath` so call sites can't accidentally confuse it
    /// with a host `PathBuf` or another string. The result is absolute
    /// by construction — `/home/<validated user>` — so we don't pay
    /// for the runtime check on `GuestPath::absolute`.
    pub fn home(&self) -> GuestPath {
        GuestPath::new(format!("/home/{}", self.0))
    }

    /// Where the Claude Code installer places the per-user binary.
    /// The installer writes to `~/.local/bin/claude`; coop calls this path
    /// directly as belt-and-braces. (`~/.local/bin` is also on PATH for every
    /// SSH session via `/etc/environment`, but the absolute path costs nothing
    /// and removes any dependence on PATH resolution.)
    pub fn claude_bin(&self) -> GuestPath {
        GuestPath::new(format!("/home/{}/.local/bin/claude", self.0))
    }
}

/// Where the Codex CLI installer places the system-wide binary.
/// Independent of the guest user — Codex lives in `/usr/local/bin`,
/// not in any user's home — so callers don't pass a `GuestUser`.
pub fn codex_bin() -> GuestPath {
    GuestPath::new("/usr/local/bin/codex")
}

/// Wrapper that runs Codex with a guest Linux Secret Service session.
pub fn codex_account_bin() -> GuestPath {
    GuestPath::new("/usr/local/bin/codex-account")
}

impl Default for GuestUser {
    fn default() -> Self {
        // Safe: DEFAULT_GUEST_USER ("ubuntu") satisfies the validator.
        Self(DEFAULT_GUEST_USER.to_owned())
    }
}

impl fmt::Display for GuestUser {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for GuestUser {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GuestUser {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<GuestUser> for String {
    #[mutants::skip] // equivalent: trivial forwarder
    fn from(value: GuestUser) -> Self {
        value.0
    }
}

/// Binaries that must exist in the guest image after provisioning.
/// Absolute paths are checked directly; bare names are looked up via
/// `command -v` (i.e. must be in the default system PATH).
///
/// Returns `GuestPath`s rather than raw strings so call sites that
/// build shell commands or inspect the chroot get path semantics for
/// free (and the `/usr/bin/docker`/`/usr/bin/gh` entries can't be
/// mistaken for host paths).
pub fn required_guest_binaries(user: &GuestUser) -> [GuestPath; 8] {
    [
        GuestPath::new("/usr/bin/docker"),
        GuestPath::new("/usr/bin/gh"),
        user.claude_bin(),
        codex_bin(),
        codex_account_bin(),
        // The Secret Service stack `codex-account` drives. Checking the
        // wrapper alone proves nothing — the provision script always writes
        // it — so verify the three tools its BASE_PACKAGES entries install.
        GuestPath::new("/usr/bin/dbus-run-session"),
        GuestPath::new("/usr/bin/gnome-keyring-daemon"),
        GuestPath::new("/usr/bin/secret-tool"),
    ]
}

/// Verify that every required guest binary is present, bailing with a
/// uniform error if any are missing.
///
/// `probe` answers "does this binary exist in the image?" — its mechanism
/// differs per backend (a chroot `symlink_metadata` for Firecracker, a
/// `test -x` over `limactl shell` for Lima). `source_label` names what
/// produced the image ("install script" vs "provision script"), and
/// `diagnostics` supplies any trailing detail (e.g. a build-log tail),
/// evaluated lazily only when something is missing.
#[expect(
    clippy::print_stderr,
    reason = "verification progress is interactive CLI — stderr is user communication"
)]
pub fn verify_required_binaries(
    guest_user: &GuestUser,
    source_label: &str,
    probe: impl Fn(&GuestPath) -> bool,
    diagnostics: impl FnOnce() -> String,
) -> Result<()> {
    eprintln!("  Verifying installed binaries...");
    let missing: Vec<String> = required_guest_binaries(guest_user)
        .into_iter()
        .filter(|path| !probe(path))
        .map(|path| path.to_string())
        .collect();

    if !missing.is_empty() {
        bail!(
            "Golden image is missing required binaries: {}\n\
             The {source_label} completed but these tools were \
             not installed correctly.{}",
            missing.join(", "),
            diagnostics(),
        );
    }
    Ok(())
}

pub const SCRIPT_GH_REPO: &str = include_str!("../scripts/guest/gh-cli-repo.sh");
pub const SCRIPT_DOCKER_REPO: &str = include_str!("../scripts/guest/docker-repo.sh");
pub const SCRIPT_CLAUDE_CODE: &str = include_str!("../scripts/guest/claude-code.sh");
pub const SCRIPT_CODEX: &str = include_str!("../scripts/guest/codex.sh");
pub const SCRIPT_CODEX_ACCOUNT: &str = include_str!("../scripts/guest/codex-account.sh");

/// Packages installed into every golden image.
///
/// `dbus-user-session`, `gnome-keyring`, and `libsecret-tools` back the
/// Secret Service that `codex-account` needs under `[codex] auth =
/// "chatgpt"`. They are unconditional rather than gated on that config field
/// on purpose: the golden image is built once by `coop setup` and reused
/// across configs, so gating them would let a later `auth = "chatgpt"` edit
/// meet an image that cannot serve it — a silent skew that surfaces only at
/// `coop codex` time. The cost is the same for everyone (see
/// `docs/images-and-profiles.md`); the wrapper stays a no-op passthrough
/// unless the guest Codex config actually asks for the keyring.
pub const BASE_PACKAGES: &[&str] = &[
    "openssh-server",
    "dbus-user-session",
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
    "unzip",
    "zip",
    "file",
    "gnome-keyring",
    "less",
    "libsecret-tools",
];

pub const GH_PACKAGES: &[&str] = &["gh"];

pub const DOCKER_PACKAGES: &[&str] = &[
    "docker-ce",
    "docker-ce-cli",
    "containerd.io",
    "docker-buildx-plugin",
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

/// Collect Codex marketplace and plugin lists from global config.
/// Results are sorted and deduplicated. Unlike [`collect_baked_lists`],
/// profiles contribute nothing here: profile plugin lists are
/// Claude-only, so Codex plugins come solely from `[codex]`.
pub fn collect_codex_baked_lists(cfg: &CoopConfig) -> (Vec<String>, Vec<String>) {
    let mut marketplaces = cfg.codex.marketplaces.clone();
    let mut plugins = cfg.codex.plugins.clone();

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
    fn codex_script_installs_extracted_binary_not_archive() {
        assert!(
            SCRIPT_CODEX.contains("BIN=\"$TMPDIR/${ASSET%.tar.gz}\""),
            "Codex installer should target the extracted binary path directly",
        );
    }

    #[test]
    fn codex_account_script_uses_secret_service() {
        assert!(
            SCRIPT_CODEX_ACCOUNT.contains("/usr/local/bin/codex-account"),
            "Codex account wrapper should be installed in the guest image",
        );
        for expected in [
            "dbus-run-session",
            "gnome-keyring-daemon --unlock",
            "secret-tool store",
            // The probe item must be removed again, not left in the keyring.
            "secret-tool clear",
        ] {
            assert!(
                SCRIPT_CODEX_ACCOUNT.contains(expected),
                "Codex account wrapper is missing {expected:?}",
            );
        }
    }

    #[test]
    fn codex_account_script_passes_through_without_keyring_mode() {
        // Every Codex entry point routes through the wrapper, so it must be a
        // transparent exec unless the guest config asks for keyring storage.
        assert!(
            SCRIPT_CODEX_ACCOUNT.contains("cli_auth_credentials_store"),
            "wrapper should gate on the guest Codex credential-store setting",
        );
        assert!(
            SCRIPT_CODEX_ACCOUNT
                .contains("if ! keyring_mode; then\n    exec \"$CODEX_BIN\" \"$@\""),
            "wrapper should exec Codex directly when keyring mode is off",
        );
    }

    #[test]
    fn codex_account_script_confirms_a_newly_created_keyring_password() {
        // A fresh guest has no keyring, so the prompt creates one; an
        // unconfirmed typo would lock credentials behind an unreproducible
        // password.
        assert!(
            SCRIPT_CODEX_ACCOUNT.contains("Confirm keyring password: "),
            "wrapper should confirm the password when creating a keyring",
        );
        assert!(
            SCRIPT_CODEX_ACCOUNT.contains("this VM has no guest keyring yet"),
            "wrapper should explain that the first prompt chooses a password",
        );
    }

    #[test]
    fn collect_codex_baked_lists_sorts_and_dedups() {
        let mut cfg = CoopConfig::default();
        cfg.codex.marketplaces = vec!["b".into(), "a".into(), "a".into()];
        cfg.codex.plugins = vec!["p2@b".into(), "p1@a".into(), "p2@b".into()];
        let (marketplaces, plugins) = collect_codex_baked_lists(&cfg);
        assert_eq!(marketplaces, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(plugins, vec!["p1@a".to_string(), "p2@b".to_string()]);
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

    #[test]
    fn guest_user_default_is_ubuntu() {
        assert_eq!(GuestUser::default().as_str(), DEFAULT_GUEST_USER);
        assert_eq!(GuestUser::default().home().to_string(), "/home/ubuntu");
    }

    #[test]
    fn guest_user_claude_bin_lives_under_home() {
        let user = GuestUser::new("vscode").unwrap();
        assert_eq!(
            user.claude_bin().to_string(),
            "/home/vscode/.local/bin/claude"
        );
    }

    #[test]
    fn guest_user_accepts_valid_names() {
        for s in ["ubuntu", "vscode", "_dev", "u", "user_1", "ec2-user"] {
            let u = GuestUser::new(s).unwrap_or_else(|e| panic!("rejected {s:?}: {e}"));
            assert_eq!(u.as_str(), s);
        }
    }

    #[test]
    fn guest_user_rejects_root() {
        let err = GuestUser::new("root").unwrap_err().to_string();
        assert!(err.contains("must not be 'root'"), "{err}");
    }

    #[test]
    fn guest_user_rejects_empty_and_too_long() {
        assert!(GuestUser::new("").is_err());
        let long = "a".repeat(MAX_GUEST_USER_LEN + 1);
        assert!(GuestUser::new(long).is_err());
    }

    #[test]
    fn guest_user_rejects_invalid_chars() {
        for s in [
            "Ubuntu",
            "0user",
            "-user",
            "user.name",
            "user!",
            "user name",
        ] {
            assert!(GuestUser::new(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn guest_user_serde_roundtrip() {
        let u = GuestUser::new("vscode").unwrap();
        let json = serde_json::to_string(&u).unwrap();
        assert_eq!(json, "\"vscode\"");
        let back: GuestUser = serde_json::from_str(&json).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn guest_user_serde_rejects_invalid() {
        let err = serde_json::from_str::<GuestUser>("\"root\"").unwrap_err();
        assert!(err.to_string().contains("root"), "{err}");
    }

    #[test]
    fn required_guest_binaries_include_codex() {
        let user = GuestUser::default();
        let bins = required_guest_binaries(&user);
        assert!(
            bins.iter().any(|b| b.to_string() == "/usr/local/bin/codex"),
            "guest image should include codex in the default PATH",
        );
        assert!(
            bins.iter()
                .any(|b| b.to_string() == "/usr/local/bin/codex-account"),
            "guest image should include the Codex account-auth wrapper",
        );
        // The wrapper is written unconditionally by the provision script, so
        // verifying it alone cannot catch the packages failing to install.
        for tool in [
            "/usr/bin/dbus-run-session",
            "/usr/bin/gnome-keyring-daemon",
            "/usr/bin/secret-tool",
        ] {
            assert!(
                bins.iter().any(|b| b.to_string() == tool),
                "guest image should verify {tool} for Codex account auth",
            );
        }
    }

    #[test]
    fn base_packages_include_secret_service_support() {
        for expected in ["dbus-user-session", "gnome-keyring", "libsecret-tools"] {
            assert!(
                BASE_PACKAGES.contains(&expected),
                "base packages should include {expected}",
            );
        }
    }

    #[test]
    fn verify_required_binaries_ok_when_all_present_and_skips_diagnostics() {
        let user = GuestUser::default();
        let diag_called = std::cell::Cell::new(false);
        let result = verify_required_binaries(
            &user,
            "install script",
            |_path| true,
            || {
                diag_called.set(true);
                String::new()
            },
        );
        assert!(result.is_ok());
        assert!(
            !diag_called.get(),
            "diagnostics must not be evaluated when nothing is missing",
        );
    }

    #[test]
    fn verify_required_binaries_bails_with_missing_names_and_diagnostics() {
        let user = GuestUser::default();
        let result = verify_required_binaries(
            &user,
            "provision script",
            |path| path.to_string() != "/usr/bin/docker",
            || "\n\ntail of log".to_string(),
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("/usr/bin/docker"),
            "missing binary listed: {err}"
        );
        assert!(
            err.contains("provision script"),
            "source label included: {err}"
        );
        assert!(
            err.contains("tail of log"),
            "diagnostics suffix appended: {err}"
        );
    }

    #[test]
    fn verify_required_binaries_lists_every_missing_binary() {
        let user = GuestUser::default();
        let result = verify_required_binaries(&user, "install script", |_path| false, String::new);
        let err = result.unwrap_err().to_string();
        for expected in [
            "/usr/bin/docker",
            "/usr/bin/gh",
            "/usr/local/bin/codex",
            "/usr/local/bin/codex-account",
        ] {
            assert!(err.contains(expected), "{expected} missing from: {err}");
        }
    }
}
