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

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cmd::Cmd;
use crate::github_repo::RepoSlug;
use crate::naming::validate_safe_chars;

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
            Self::LinuxSecretService => {
                ToolName::new("secret-tool").is_ok_and(|n| tool_on_path(&n))
            }
            Self::OnePassword => ToolName::new("op").is_ok_and(|n| tool_on_path(&n)),
            Self::File => true,
        }
    }
}

fn tool_on_path(name: &ToolName) -> bool {
    // `command -v` is a shell builtin — execute it via `sh -c`. The
    // `ToolName` constructor already proved `name` matches
    // `[a-zA-Z0-9_.-]+`, so embedding it in the shell string can't inject.
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .status()
        .is_ok_and(|s| s.success())
}

/// Validated executable name for `tool_on_path` lookups.
///
/// The constructor enforces a non-empty `[a-zA-Z0-9_.-]+` string, so
/// downstream code can embed the name in a `sh -c` command line without
/// re-checking for shell metacharacters or empty input.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolName(String);

impl ToolName {
    pub fn new(name: &str) -> Result<Self> {
        if name.is_empty() {
            bail!("Tool name is empty");
        }
        validate_safe_chars(name, "Tool name")?;
        Ok(Self(name.to_string()))
    }
}

impl fmt::Display for ToolName {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ToolName {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Account identifier for a secret-store entry.
///
/// Built once from a [`RepoSlug`] by replacing the `/` separator with
/// `-`. Because `RepoSlug` already restricts segments to
/// `[a-zA-Z0-9_.-]`, the resulting string also matches that class and
/// is safe to use as a filename component or backend lookup key without
/// further sanitization.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountName(String);

impl AccountName {
    pub fn from_repo(slug: &RepoSlug) -> Self {
        Self(slug.as_str().replace('/', "-"))
    }

    /// Validate `s` as an account name (non-empty, `[a-zA-Z0-9_.-]`) and wrap
    /// it. Used when recovering a [`CmdToken::File`] from a stored `cmd:cat`
    /// path: the filename stem is parsed back into an `AccountName`.
    ///
    /// This accepts a *superset* of what [`from_repo`](Self::from_repo) emits
    /// — `from_repo` always inserts a `-` separator, so a dash-free name like
    /// `"abc"` passes here yet no repo slug could have produced it. That is
    /// deliberate: the character class (not the exact `from_repo` image) is
    /// the invariant that matters, because it keeps the value a safe filename
    /// component and keeps [`CmdToken::parse`] an unambiguous inverse. A
    /// recovered `File` token is only used to name the backend and to
    /// round-trip through its `Display`; it never drives a filesystem write,
    /// so a stem coop would not itself have written is harmless to accept
    /// (and a stem outside the class — spaces, quotes, a `/` — is still
    /// rejected, falling through to `None` rather than being misattributed to
    /// `File`).
    pub fn new(s: &str) -> Result<Self> {
        if s.is_empty() {
            bail!("Account name is empty");
        }
        validate_safe_chars(s, "Account name")?;
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountName {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AccountName {
    #[mutants::skip] // equivalent: trivial forwarder over self.0
    fn as_ref(&self) -> &str {
        &self.0
    }
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
/// Returns the [`CmdToken`] describing how to read the secret back. Its
/// `Display` renders the `cmd:`-prefixed string written to the `token = ...`
/// field in `[github.pat."owner/repo"]`; when evaluated by
/// `resolve_cmd_value`, that command prints the token on stdout.
pub fn store_secret(
    backend: Backend,
    service: &str,
    account: &AccountName,
    token: &str,
    state_dir: &Path,
) -> Result<CmdToken> {
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
    account: &AccountName,
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
                .arg(account.as_str())
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
                .arg(account.as_str())
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

/// A structured description of the `cmd:` invocation coop stores in the
/// config to read a PAT back from its secret store.
///
/// [`Display`](fmt::Display) renders the `cmd:`-prefixed form written to the
/// config file; [`CmdToken::parse`] recovers the token from that form. The
/// two are exact inverses (round-trip tested), so the stored command string
/// and the backend it denotes can never silently diverge — there is one
/// encoding of the format, not two.
///
/// System-keychain variants are `cfg`-gated for the same reason as
/// [`Backend`]: a binary built for one OS cannot represent — let alone read —
/// the other's keychain entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdToken {
    /// macOS Keychain lookup via `security find-generic-password`.
    #[cfg(target_os = "macos")]
    Keychain { service: String, account: String },
    /// Linux Secret Service lookup via `secret-tool lookup`.
    #[cfg(target_os = "linux")]
    SecretService { service: String, account: String },
    /// 1Password item read via `op item get`.
    OnePassword { title: String },
    /// Plain-file read via `cat`. Modelled by its varying parts — the state
    /// directory and the [`AccountName`] — rather than a free `PathBuf`, so
    /// the canonical `<dir>/github-pat/<account>.txt` layout holds by
    /// construction and every representable value round-trips through
    /// [`Display`](fmt::Display) / [`parse`](Self::parse).
    File { dir: PathBuf, account: AccountName },
}

impl CmdToken {
    /// The backend that produced (and can read) this token.
    pub fn backend(&self) -> Backend {
        match self {
            #[cfg(target_os = "macos")]
            Self::Keychain { .. } => Backend::MacosKeychain,
            #[cfg(target_os = "linux")]
            Self::SecretService { .. } => Backend::LinuxSecretService,
            Self::OnePassword { .. } => Backend::OnePassword,
            Self::File { .. } => Backend::File,
        }
    }

    /// Recover a `CmdToken` from a stored `cmd:` string, or `None` if the
    /// command is not one coop produces.
    ///
    /// Used by `coop github status` to name the storage location and by
    /// `forget-pat` to pick the deletion backend, both without reading the
    /// secret. Returns `None` for opaque commands *and* for system-keychain
    /// entries the current build cannot represent (e.g. a
    /// `security find-generic-password …` entry seen by a Linux binary — its
    /// secret is unreachable from this host anyway).
    ///
    /// The match is the exact inverse of [`Display`](fmt::Display): a command
    /// that does not follow the canonical shape coop writes (including a
    /// `cat` path not laid out as `…/github-pat/<name>.txt`) falls through to
    /// `None` rather than being misattributed to a backend.
    pub fn parse(cmd: &str) -> Option<Self> {
        let body = cmd.strip_prefix("cmd:").map_or(cmd, str::trim_start);
        let words = shell_split(body)?;
        Self::from_words(&words)
    }

    fn from_words(words: &[String]) -> Option<Self> {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        match words.as_slice() {
            #[cfg(target_os = "macos")]
            [
                "security",
                "find-generic-password",
                "-s",
                service,
                "-a",
                account,
                "-w",
            ] => Some(Self::Keychain {
                service: (*service).to_string(),
                account: (*account).to_string(),
            }),
            #[cfg(target_os = "linux")]
            [
                "secret-tool",
                "lookup",
                "service",
                service,
                "account",
                account,
            ] => Some(Self::SecretService {
                service: (*service).to_string(),
                account: (*account).to_string(),
            }),
            [
                "op",
                "item",
                "get",
                title,
                "--fields",
                "password",
                "--reveal",
            ] => Some(Self::OnePassword {
                title: (*title).to_string(),
            }),
            ["cat", path] => {
                parse_pat_file(Path::new(path)).map(|(dir, account)| Self::File { dir, account })
            }
            _ => None,
        }
    }
}

impl fmt::Display for CmdToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_os = "macos")]
            Self::Keychain { service, account } => write!(
                f,
                "cmd:security find-generic-password -s {} -a {} -w",
                shell_quote(service),
                shell_quote(account),
            ),
            #[cfg(target_os = "linux")]
            Self::SecretService { service, account } => write!(
                f,
                "cmd:secret-tool lookup service {} account {}",
                shell_quote(service),
                shell_quote(account),
            ),
            Self::OnePassword { title } => write!(
                f,
                "cmd:op item get {} --fields password --reveal",
                shell_quote(title),
            ),
            Self::File { dir, account } => {
                let path = file_backend_path(dir, account);
                write!(f, "cmd:cat {}", shell_quote(&path.to_string_lossy()))
            }
        }
    }
}

/// Decompose a `cat` path into the parts of a [`CmdToken::File`], or `None`
/// if it does not follow the canonical `<dir>/github-pat/<account>.txt`
/// layout coop writes for the file backend.
///
/// This is the exact inverse of joining those parts in
/// [`file_backend_path`]: the `.txt` suffix and `github-pat` parent are
/// stripped, and the filename stem must be a well-formed [`AccountName`].
/// A path that fails any check is reported as `None` rather than being
/// misattributed to the file backend.
fn parse_pat_file(path: &Path) -> Option<(PathBuf, AccountName)> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".txt")?;
    let parent = path.parent()?;
    if parent.file_name()?.to_str()? != PAT_DIR {
        return None;
    }
    let dir = parent.parent()?.to_path_buf();
    let account = AccountName::new(stem).ok()?;
    Some((dir, account))
}

/// Split a `cmd:` body into shell words, inverting [`shell_quote`].
///
/// Handles the only constructs `shell_quote` can emit — single-quoted
/// segments and backslash-escaped characters (the `'\''` idiom) — alongside
/// plain unquoted runs. Returns `None` on an unterminated single quote.
fn shell_split(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                let mut closed = false;
                for qc in chars.by_ref() {
                    if qc == '\'' {
                        closed = true;
                        break;
                    }
                    cur.push(qc);
                }
                if !closed {
                    return None;
                }
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

// ── Backend impls ──────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn store_keychain(service: &str, account: &AccountName, token: &str) -> Result<CmdToken> {
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
        .arg(account.as_str())
        .arg("-w")
        .redacted_arg(token)
        .run()
        .context("Failed to write secret to macOS Keychain")?;
    Ok(CmdToken::Keychain {
        service: service.to_string(),
        account: account.as_str().to_string(),
    })
}

#[cfg(target_os = "linux")]
fn store_secret_service(service: &str, account: &AccountName, token: &str) -> Result<CmdToken> {
    // `secret-tool store` reads the password from stdin.
    Cmd::new("secret-tool")
        .arg("store")
        .arg("--label")
        .arg(format!("coop github PAT for {account}"))
        .arg("service")
        .arg(service)
        .arg("account")
        .arg(account.as_str())
        .stdin_input(token.as_bytes().to_vec())
        .run()
        .context("Failed to write secret to Linux Secret Service")?;
    Ok(CmdToken::SecretService {
        service: service.to_string(),
        account: account.as_str().to_string(),
    })
}

fn store_onepassword(service: &str, account: &AccountName, token: &str) -> Result<CmdToken> {
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
    Ok(CmdToken::OnePassword { title })
}

fn store_file(
    service: &str,
    account: &AccountName,
    token: &str,
    state_dir: &Path,
) -> Result<CmdToken> {
    let dir = state_dir.join(PAT_DIR);
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
    Ok(CmdToken::File {
        dir: state_dir.to_path_buf(),
        account: account.clone(),
    })
}

fn file_backend_path(state_dir: &Path, account: &AccountName) -> PathBuf {
    state_dir.join(PAT_DIR).join(format!("{account}.txt"))
}

fn set_dir_mode(path: &Path, mode: u32) -> Result<()> {
    let perms = std::fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to chmod {} to {mode:#o}", path.display()))
}

/// POSIX shell-quote `s` for safe embedding in a `cmd:` invocation.
///
/// The empty string takes the quoted path (`''`): an unquoted empty string
/// is dropped entirely by both `sh -c` (in `resolve_cmd_value`) and
/// [`shell_split`], so the fast path must not swallow it.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Conventional service name used across all backends.
pub const SERVICE: &str = "coop-github-pat";

/// Directory under `<data>/state/` holding file-backend PAT secrets.
const PAT_DIR: &str = "github-pat";

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    fn account(s: &str) -> AccountName {
        AccountName::from_repo(&RepoSlug::new(s).unwrap())
    }

    #[test]
    fn account_replaces_slash() {
        let slug = RepoSlug::new("trailofbits/coop").unwrap();
        assert_eq!(AccountName::from_repo(&slug).as_str(), "trailofbits-coop");
    }

    #[test]
    fn account_name_new_rejects_empty_and_slashes() {
        // `new` is the parse-side smart constructor `parse_pat_file` relies
        // on: a `/` (single, repeated, or leading) in the filename stem would
        // make the path decomposition ambiguous, so it must be rejected here.
        // This pins what keeps the `File` round-trip unambiguous, independent
        // of `RepoSlug`'s own gating on the `from_repo` path.
        assert!(AccountName::new("").is_err(), "empty must be rejected");
        for bad in ["a/b", "/a", "a/", "a//b", "/"] {
            assert!(
                AccountName::new(bad).is_err(),
                "slash-bearing '{bad}' must be rejected"
            );
        }
        assert_eq!(
            AccountName::new("trailofbits-coop").unwrap().as_str(),
            "trailofbits-coop"
        );
    }

    #[test]
    fn account_from_repo_uses_safe_chars_only() {
        // Constructor is infallible; result must consist only of the
        // [a-zA-Z0-9_.-] class so downstream filename / lookup-key use
        // is safe without further sanitization.
        let slug = RepoSlug::new("trail-of.bits_1/coop").unwrap();
        let acc = AccountName::from_repo(&slug);
        assert!(
            acc.as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "AccountName must only contain [a-zA-Z0-9_.-], got '{acc}'"
        );
        assert!(!acc.as_str().contains('/'), "slash must be replaced");
    }

    #[test]
    fn tool_name_forwards_display_and_as_ref() {
        // Confirms the Display / AsRef impls forward to the inner string.
        // Char-class acceptance is covered by the `naming` module's tests.
        let t = ToolName::new("secret-tool").unwrap();
        assert_eq!(t.to_string(), "secret-tool");
        assert_eq!(<ToolName as AsRef<str>>::as_ref(&t), "secret-tool");
    }

    #[test]
    fn tool_name_rejects_empty() {
        assert!(ToolName::new("").is_err());
    }

    #[test]
    fn tool_name_rejects_unsafe_char() {
        // Smoke test that the constructor wires through to
        // `validate_safe_chars`; the exhaustive char-class rejection is
        // covered by the `naming` module's tests.
        assert!(ToolName::new("rm -rf /").is_err());
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
    fn shell_quote_quotes_empty_string() {
        // An unquoted empty string is dropped by both `sh -c` and
        // `shell_split`; it must take the quoted path.
        assert_eq!(shell_quote(""), "''");
        assert_eq!(
            shell_split("a '' b"),
            Some(vec!["a".into(), String::new(), "b".into()])
        );
    }

    #[test]
    fn file_backend_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let acc = account("trailofbits/coop");
        let cmd = store_file(SERVICE, &acc, "github_pat_xyz", tmp.path()).unwrap();
        assert!(cmd.to_string().starts_with("cmd:cat "));
        assert_eq!(cmd.backend(), Backend::File);
        // Verify the file exists with the right contents and mode.
        let path = file_backend_path(tmp.path(), &acc);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.trim(), "github_pat_xyz");
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn file_backend_delete_removes_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let acc = account("x/y");
        let _ = store_file(SERVICE, &acc, "token", tmp.path()).unwrap();
        delete_secret(Backend::File, SERVICE, &acc, tmp.path()).unwrap();
        let path = file_backend_path(tmp.path(), &acc);
        assert!(!path.exists());
    }

    fn backend_of(cmd: &str) -> Option<Backend> {
        CmdToken::parse(cmd).map(|t| t.backend())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_recognises_macos_keychain() {
        assert_eq!(
            backend_of("cmd:security find-generic-password -s coop-github-pat -a x -w"),
            Some(Backend::MacosKeychain)
        );
        // Non-target keychain invocations are opaque to this build.
        assert_eq!(
            backend_of("cmd:secret-tool lookup service coop-github-pat account x"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_recognises_linux_secret_service() {
        assert_eq!(
            backend_of("cmd:secret-tool lookup service coop-github-pat account x"),
            Some(Backend::LinuxSecretService)
        );
        // Non-target keychain invocations are opaque to this build.
        assert_eq!(
            backend_of("cmd:security find-generic-password -s coop-github-pat -a x -w"),
            None
        );
    }

    #[test]
    fn parse_recognises_cross_platform_backends() {
        assert_eq!(
            backend_of("cmd:op item get 'foo' --fields password --reveal"),
            Some(Backend::OnePassword)
        );
        assert_eq!(
            backend_of("cmd:cat ~/.coop/state/github-pat/x.txt"),
            Some(Backend::File)
        );
        assert_eq!(backend_of("cmd:echo opaque"), None);
    }

    #[test]
    fn parse_rejects_non_canonical_onepassword() {
        // coop only ever writes `op item get … --fields password --reveal`.
        // A hand-written `op read` reference is not a form coop can read back
        // by title, so it is treated as opaque rather than misattributed.
        assert_eq!(backend_of("cmd:op read op://Private/coop/token"), None);
    }

    #[test]
    fn parse_rejects_cat_without_canonical_layout() {
        // A `cat` invocation that doesn't follow the `…/github-pat/<x>.txt`
        // layout should not be reported as the File backend — otherwise
        // `forget-pat` would happily try to delete a file coop never wrote.
        assert_eq!(
            backend_of("cmd:cat ~/some/path/secret.txt"),
            None,
            "cat invocation without github-pat in the path must not match File"
        );
        assert_eq!(
            backend_of("cmd:cat ~/.coop/state/github-pat/x.bin"),
            None,
            "non-.txt suffix must not match File"
        );
    }

    #[test]
    fn parse_strips_and_trims_cmd_prefix() {
        // `resolve_cmd_value` tolerates `cmd:` with leading space; parsing
        // must recover the same backend whether or not the space is present.
        assert_eq!(
            backend_of("cmd: op item get foo --fields password --reveal"),
            Some(Backend::OnePassword)
        );
    }

    #[test]
    fn store_outputs_round_trip_through_parse() {
        // The cmd string a backend actually writes must parse back to the
        // backend that wrote it — this is the divergence the type prevents.
        let tmp = tempfile::TempDir::new().unwrap();
        let acc = account("trailofbits/coop");
        let token = store_file(SERVICE, &acc, "github_pat_xyz", tmp.path()).unwrap();
        let rendered = token.to_string();
        assert_eq!(CmdToken::parse(&rendered), Some(token));
    }

    #[test]
    fn shell_split_handles_quotes_and_escapes() {
        assert_eq!(
            shell_split("a b c"),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(shell_split("'a b' c"), Some(vec!["a b".into(), "c".into()]));
        // `shell_quote("a'b")` emits `'a'\''b'`, which must split back to `a'b`.
        assert_eq!(shell_split(r"'a'\''b'"), Some(vec!["a'b".into()]));
        // Unterminated single quote is rejected.
        assert_eq!(shell_split("'unterminated"), None);
    }

    // ── Property tests ─────────────────────────────────────────────
    //
    // `proptest!` blocks live alongside the example tests above (per the
    // testing conventions), each pinning an invariant the examples only
    // sample.

    /// A well-formed account name — non-empty and in the `[a-zA-Z0-9_.-]`
    /// class, exactly what [`AccountName::from_repo`] can produce.
    fn arb_account_name() -> impl Strategy<Value = AccountName> {
        "[a-zA-Z0-9_.-]{1,20}".prop_map(|s| AccountName::new(&s).unwrap())
    }

    /// A state directory. Mixes genuinely path-shaped values — multiple
    /// segments, separators, empty/trailing components, and segments that
    /// collide with the canonical layout (`github-pat`, a `*.txt` stem) —
    /// with free text (spaces, quotes, unicode, the empty path) so the
    /// round-trip is exercised on the inputs most likely to confuse the
    /// `parse_pat_file` decomposition, not just random bytes. The NUL byte is
    /// excluded; no real filesystem path carries it.
    fn arb_dir() -> impl Strategy<Value = PathBuf> {
        prop_oneof![
            prop::collection::vec("(github-pat|[^\u{0}/]{0,8})", 0..4)
                .prop_map(|segs| segs.iter().collect::<PathBuf>()),
            "[^\u{0}]{0,40}".prop_map(PathBuf::from),
        ]
    }

    /// A string field embedded into a `cmd:` invocation. Biased toward the
    /// values that could collide with the surrounding literal/flag words —
    /// the empty string, flag-shaped tokens (`-w`, `--fields`), and the
    /// command words themselves — alongside arbitrary text. `shell_quote`
    /// renders each as exactly one word, so a collision would have to defeat
    /// the fixed-length slice match in `from_words`, not just look like a
    /// flag.
    fn arb_field() -> impl Strategy<Value = String> {
        prop_oneof![
            any::<String>(),
            prop::sample::select(vec![
                String::new(),
                "-s".to_string(),
                "-a".to_string(),
                "-w".to_string(),
                "--fields".to_string(),
                "password".to_string(),
                "--reveal".to_string(),
                "cat".to_string(),
                "lookup".to_string(),
                "a b".to_string(),
                "'".to_string(),
            ]),
        ]
    }

    #[cfg(target_os = "macos")]
    fn arb_system_token() -> impl Strategy<Value = CmdToken> {
        (arb_field(), arb_field())
            .prop_map(|(service, account)| CmdToken::Keychain { service, account })
    }

    #[cfg(target_os = "linux")]
    fn arb_system_token() -> impl Strategy<Value = CmdToken> {
        (arb_field(), arb_field())
            .prop_map(|(service, account)| CmdToken::SecretService { service, account })
    }

    fn arb_cmd_token() -> impl Strategy<Value = CmdToken> {
        prop_oneof![
            arb_system_token(),
            arb_field().prop_map(|title| CmdToken::OnePassword { title }),
            (arb_dir(), arb_account_name())
                .prop_map(|(dir, account)| CmdToken::File { dir, account }),
        ]
    }

    proptest! {
        /// `shell_quote` always produces exactly one shell word that splits
        /// back to the original string — the "escaped string parses back as
        /// exactly one literal argument" invariant the `cmd:` encoding rests
        /// on.
        #[test]
        fn shell_quote_split_round_trips(s in any::<String>()) {
            prop_assert_eq!(shell_split(&shell_quote(&s)), Some(vec![s]));
        }

        /// `Display` and `parse` are inverses (under `CmdToken`'s `PartialEq`)
        /// for every representable token. For `File` this holds *by
        /// construction* now that the variant is modelled by its varying parts
        /// rather than a free path — the case the old hand-picked examples
        /// could not exhaust, and which surfaced the empty-string quoting bug
        /// in the #271/#277 review. `dir` round-trips up to `Path`'s
        /// component-wise equality (a trailing or doubled separator
        /// normalises away), which is exactly the equality the property
        /// asserts and matches the normalised `state_dir` `store_file` feeds
        /// in.
        #[test]
        fn cmd_token_round_trips(token in arb_cmd_token()) {
            let rendered = token.to_string();
            prop_assert_eq!(CmdToken::parse(&rendered), Some(token));
        }
    }
}
