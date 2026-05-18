//! Pluggable secret-storage backends used by the `coop github setup-pat` wizard.
//!
//! Each backend stores a secret keyed by `(service, account)` and returns a
//! `cmd:`-prefixed string that coop's config layer (`resolve_cmd_value`) can
//! invoke at runtime to read the secret back. coop itself never reads the
//! stored secret directly — the indirection keeps long-lived plaintext off
//! disk and out of the config file.
//!
//! Backend detection is fail-soft: anything unavailable on the current host
//! is omitted from [`available_backends`]. The file fallback is always
//! available so the wizard always has at least one storage choice.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cmd::Cmd;

/// All secret-store backends recognised by the wizard.
///
/// Platform-specific system keychains (`MacosKeychain`, `LinuxSecretService`)
/// are `cfg`-gated: the variants only exist on the targets where they can be
/// read. A binary built for Linux therefore cannot represent — let alone
/// attempt to use — the macOS Keychain backend, and vice versa. The secret
/// itself is always host-local, so a config entry produced by one platform's
/// system keychain is meaningless on the other.
///
/// `OnePassword` and `File` are cross-platform and always present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// macOS Keychain via `/usr/bin/security`.
    #[cfg(target_os = "macos")]
    MacosKeychain,
    /// Linux Secret Service (GNOME Keyring / `KWallet`) via `secret-tool`.
    #[cfg(target_os = "linux")]
    LinuxSecretService,
    /// 1Password CLI (`op`) — used when explicitly chosen by the user.
    OnePassword,
    /// Plain file under `~/.coop/state/github-pat/<slug>.txt`, mode 0600.
    File,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacosKeychain => "macOS Keychain",
            #[cfg(target_os = "linux")]
            Self::LinuxSecretService => "Linux Secret Service (GNOME Keyring / KWallet)",
            Self::OnePassword => "1Password CLI",
            Self::File => "Plain file (~/.coop/state/github-pat/<slug>.txt, mode 0600)",
        }
    }

    /// Probe whether this backend is usable on the current host.
    ///
    /// The variant's existence already proves target-OS compatibility; this
    /// only checks the *runtime* prerequisites (binary on PATH, etc.).
    pub fn is_available(self) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacosKeychain => Path::new("/usr/bin/security").exists(),
            #[cfg(target_os = "linux")]
            Self::LinuxSecretService => tool_on_path("secret-tool"),
            Self::OnePassword => tool_on_path("op"),
            Self::File => true,
        }
    }
}

fn tool_on_path(name: &str) -> bool {
    // `command -v` is a shell builtin — execute it via `sh -c`. Reject
    // names with whitespace defensively so we don't shell-inject.
    if name.is_empty() || name.chars().any(|c| !is_safe_tool_name_char(c)) {
        return false;
    }
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .status()
        .is_ok_and(|s| s.success())
}

fn is_safe_tool_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// Return the list of usable backends on the current host, in the
/// natural-default order. The file fallback always appears last.
pub fn available_backends() -> Vec<Backend> {
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    if Backend::MacosKeychain.is_available() {
        out.push(Backend::MacosKeychain);
    }
    #[cfg(target_os = "linux")]
    if Backend::LinuxSecretService.is_available() {
        out.push(Backend::LinuxSecretService);
    }
    if Backend::OnePassword.is_available() {
        out.push(Backend::OnePassword);
    }
    out.push(Backend::File);
    out
}

/// Store `token` in `backend` keyed by `(service, account)`.
///
/// Returns a `cmd:`-prefixed string suitable for the `token = ...` field
/// in `[github.pat."owner/repo"]`. The returned command, when evaluated by
/// `resolve_cmd_value`, must print the token on stdout.
pub fn store_secret(
    backend: Backend,
    service: &str,
    account: &str,
    token: &str,
    state_dir: &Path,
) -> Result<String> {
    match backend {
        #[cfg(target_os = "macos")]
        Backend::MacosKeychain => store_keychain(service, account, token),
        #[cfg(target_os = "linux")]
        Backend::LinuxSecretService => store_secret_service(service, account, token),
        Backend::OnePassword => store_onepassword(service, account, token),
        Backend::File => store_file(service, account, token, state_dir),
    }
}

/// Remove the secret stored under `(service, account)` from `backend`.
///
/// Best-effort: missing entries are not an error.
pub fn delete_secret(
    backend: Backend,
    service: &str,
    account: &str,
    state_dir: &Path,
) -> Result<()> {
    match backend {
        #[cfg(target_os = "macos")]
        Backend::MacosKeychain => {
            let _ = Cmd::new("security")
                .arg("delete-generic-password")
                .arg("-s")
                .arg(service)
                .arg("-a")
                .arg(account)
                .output();
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Backend::LinuxSecretService => {
            let _ = Cmd::new("secret-tool")
                .arg("clear")
                .arg("service")
                .arg(service)
                .arg("account")
                .arg(account)
                .output();
            Ok(())
        }
        Backend::OnePassword => {
            // Best-effort soft-delete: `--archive` keeps the item
            // recoverable from the 1Password trash. Manual cleanup is
            // always available via the 1Password UI.
            let title = format!("{service} ({account})");
            let _ = Cmd::new("op")
                .arg("item")
                .arg("delete")
                .arg("--archive")
                .arg(&title)
                .output();
            Ok(())
        }
        Backend::File => {
            let path = file_backend_path(state_dir, account);
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
            Ok(())
        }
    }
}

/// Recognise a `cmd:` invocation as the backend that wrote it.
///
/// Used by `coop github status` to display the storage location without
/// reading the secret. Returns `None` for opaque commands *and* for entries
/// produced by a system keychain that the current build cannot represent
/// (e.g. a `security find-generic-password …` entry seen by a Linux binary
/// — the secret itself is unreachable from this host anyway, so we treat
/// it as unknown rather than describing a variant that doesn't exist here).
///
/// The File-backend match requires the path to end in `/github-pat/<name>.txt`
/// (the canonical layout coop writes) so user-supplied `cmd:cat …` paths
/// that don't follow that shape correctly fall through to `unknown`.
pub fn infer_backend(cmd: &str) -> Option<Backend> {
    let cmd_str = cmd.strip_prefix("cmd:").map_or(cmd, str::trim_start);
    #[cfg(target_os = "macos")]
    if cmd_str.starts_with("security find-generic-password") {
        return Some(Backend::MacosKeychain);
    }
    #[cfg(target_os = "linux")]
    if cmd_str.starts_with("secret-tool lookup") {
        return Some(Backend::LinuxSecretService);
    }
    if cmd_str.starts_with("op read") || cmd_str.starts_with("op item get") {
        Some(Backend::OnePassword)
    } else if cmd_str.starts_with("cat ")
        && cmd_str.contains("github-pat/")
        && cmd_str.contains(".txt")
    {
        Some(Backend::File)
    } else {
        None
    }
}

// ── Backend impls ──────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn store_keychain(service: &str, account: &str, token: &str) -> Result<String> {
    // macOS `security` lacks a stdin-driven write for generic passwords:
    // `-w <password>` reads the secret from argv. The bytes are briefly
    // visible to local `ps`-equivalent observers — an unavoidable cost
    // for the duration of the call. `redacted_arg` keeps the token out
    // of coop's own debug log, but `ps` is out of our reach. Users with
    // stronger threat models can use the file or Secret Service backends.
    Cmd::new("security")
        .arg("add-generic-password")
        .arg("-U")
        .arg("-s")
        .arg(service)
        .arg("-a")
        .arg(account)
        .arg("-w")
        .redacted_arg(token)
        .run()
        .context("Failed to write secret to macOS Keychain")?;
    Ok(format!(
        "cmd:security find-generic-password -s {} -a {} -w",
        shell_quote(service),
        shell_quote(account),
    ))
}

#[cfg(target_os = "linux")]
fn store_secret_service(service: &str, account: &str, token: &str) -> Result<String> {
    // `secret-tool store` reads the password from stdin.
    Cmd::new("secret-tool")
        .arg("store")
        .arg("--label")
        .arg(format!("coop github PAT for {account}"))
        .arg("service")
        .arg(service)
        .arg("account")
        .arg(account)
        .stdin_input(token.as_bytes().to_vec())
        .run()
        .context("Failed to write secret to Linux Secret Service")?;
    Ok(format!(
        "cmd:secret-tool lookup service {} account {}",
        shell_quote(service),
        shell_quote(account),
    ))
}

fn store_onepassword(service: &str, account: &str, token: &str) -> Result<String> {
    // `op item create` reads field values from argv. The token is briefly
    // visible to local observers via /proc; redacted from coop's own
    // debug log via `redacted_arg`. 1Password rejects duplicate titles, so
    // soft-delete any existing item with the same title first (rotate-pat
    // calls store_onepassword on an existing entry).
    let title = format!("{service} ({account})");
    let _ = Cmd::new("op")
        .arg("item")
        .arg("delete")
        .arg("--archive")
        .arg(&title)
        .output();
    let password_field = format!("password={token}");
    Cmd::new("op")
        .arg("item")
        .arg("create")
        .arg("--category=login")
        .arg(format!("--title={title}"))
        .redacted_arg(password_field)
        .run()
        .context("Failed to create 1Password item")?;
    let title_quoted = shell_quote(&title);
    Ok(format!(
        "cmd:op item get {title_quoted} --fields password --reveal"
    ))
}

fn store_file(service: &str, account: &str, token: &str, state_dir: &Path) -> Result<String> {
    let dir = state_dir.join("github-pat");
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    set_dir_mode(&dir, 0o700)?;
    let path = file_backend_path(state_dir, account);
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    f.write_all(token.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    // Newline-terminate so `cat` output is friendly.
    if !token.ends_with('\n') {
        f.write_all(b"\n")
            .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    // Suppress unused-warning for `service` when the file backend ignores it.
    let _ = service;
    Ok(format!("cmd:cat {}", shell_quote(&path.to_string_lossy())))
}

fn file_backend_path(state_dir: &Path, account: &str) -> PathBuf {
    state_dir
        .join("github-pat")
        .join(format!("{}.txt", sanitize(account)))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn set_dir_mode(path: &Path, mode: u32) -> Result<()> {
    let perms = std::fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to chmod {} to {mode:#o}", path.display()))
}

/// POSIX shell-quote `s` for safe embedding in a `cmd:` invocation.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Build the conventional account name for a repo's PAT entry.
pub fn account_for_repo(repo: &str) -> String {
    repo.replace('/', "-")
}

/// Conventional service name used across all backends.
pub const SERVICE: &str = "coop-github-pat";

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn file_backend_always_available() {
        assert!(Backend::File.is_available());
    }

    #[test]
    fn available_backends_includes_file_last() {
        let backends = available_backends();
        assert!(!backends.is_empty());
        assert_eq!(*backends.last().unwrap(), Backend::File);
    }

    #[test]
    fn account_replaces_slash() {
        assert_eq!(
            account_for_repo("trailofbits/coop"),
            "trailofbits-coop".to_string()
        );
    }

    #[test]
    fn shell_quote_passes_simple_strings() {
        assert_eq!(shell_quote("trailofbits-coop"), "trailofbits-coop");
        assert_eq!(shell_quote("/tmp/foo.txt"), "/tmp/foo.txt");
    }

    #[test]
    fn shell_quote_wraps_unsafe_strings() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize("owner/repo"), "owner-repo");
        assert_eq!(sanitize("safe-name_v2"), "safe-name_v2");
    }

    #[test]
    fn file_backend_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cmd = store_file(SERVICE, "trailofbits-coop", "github_pat_xyz", tmp.path()).unwrap();
        assert!(cmd.starts_with("cmd:cat "));
        // Verify the file exists with the right contents and mode.
        let path = file_backend_path(tmp.path(), "trailofbits-coop");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.trim(), "github_pat_xyz");
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn file_backend_delete_removes_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _ = store_file(SERVICE, "x-y", "token", tmp.path()).unwrap();
        delete_secret(Backend::File, SERVICE, "x-y", tmp.path()).unwrap();
        let path = file_backend_path(tmp.path(), "x-y");
        assert!(!path.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn infer_backend_recognises_macos_keychain() {
        assert_eq!(
            infer_backend("cmd:security find-generic-password -s coop-github-pat -a x -w"),
            Some(Backend::MacosKeychain)
        );
        // Non-target keychain invocations are opaque to this build.
        assert_eq!(
            infer_backend("cmd:secret-tool lookup service coop-github-pat account x"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn infer_backend_recognises_linux_secret_service() {
        assert_eq!(
            infer_backend("cmd:secret-tool lookup service coop-github-pat account x"),
            Some(Backend::LinuxSecretService)
        );
        // Non-target keychain invocations are opaque to this build.
        assert_eq!(
            infer_backend("cmd:security find-generic-password -s coop-github-pat -a x -w"),
            None
        );
    }

    #[test]
    fn infer_backend_recognises_cross_platform_backends() {
        assert_eq!(
            infer_backend("cmd:op read op://Private/coop/token"),
            Some(Backend::OnePassword)
        );
        assert_eq!(
            infer_backend("cmd:op item get 'foo' --fields password --reveal"),
            Some(Backend::OnePassword)
        );
        assert_eq!(
            infer_backend("cmd:cat ~/.coop/state/github-pat/x.txt"),
            Some(Backend::File)
        );
        assert_eq!(infer_backend("cmd:echo opaque"), None);
    }

    #[test]
    fn infer_backend_rejects_cat_without_canonical_layout() {
        // A `cat` invocation that doesn't follow the `…/github-pat/<x>.txt`
        // layout should not be reported as the File backend — otherwise
        // `forget-pat` would happily try to delete a file coop never wrote.
        assert_eq!(
            infer_backend("cmd:cat ~/some/path/secret.txt"),
            None,
            "cat invocation without github-pat in the path must not match File"
        );
        assert_eq!(
            infer_backend("cmd:cat ~/.coop/state/github-pat/x.bin"),
            None,
            "non-.txt suffix must not match File"
        );
    }
}
