//! Self-update for the coop binary.
//!
//! Fetches the latest release metadata from GitHub, downloads the matching
//! tarball and `SHA256SUMS`, verifies the checksum (and optionally the
//! attestation via `gh`), then atomically replaces the running binary.
//!
//! Also provides a background update-check path used by every invocation
//! to nudge users when a newer release is available.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::cmd::{Cmd, command_exists};
use crate::fs_util::atomic_write_json;
use crate::prompt::confirm;
use crate::sha256_hash::Sha256Hash;

const REPO: &str = "trailofbits/coop";
const DEFAULT_API_BASE: &str = "https://api.github.com";
/// Release asset holding the Sigstore provenance bundle, published since #421.
const BUNDLE_ASSET: &str = "attestations.jsonl";
const DEFAULT_CHECK_INTERVAL_HOURS: u64 = 24;

// ── Configuration ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    /// Do not check for updates or display notifications.
    Off,
    /// Check in the background and print a banner when a newer release is known.
    #[default]
    Notify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default)]
    pub mode: UpdateMode,
    #[serde(default = "default_check_interval_hours")]
    pub check_interval_hours: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            mode: UpdateMode::default(),
            check_interval_hours: DEFAULT_CHECK_INTERVAL_HOURS,
        }
    }
}

fn default_check_interval_hours() -> u64 {
    DEFAULT_CHECK_INTERVAL_HOURS
}

// ── Command-line options ─────────────────────────────────────────────────────

/// Options for the `coop update` subcommand.
#[derive(Debug, Default)]
pub struct UpdateOpts {
    /// Probe latest release but do not download or install.
    pub check_only: bool,
    /// Reinstall even if the target version is not newer.
    pub force: bool,
    /// Pin to a specific version (with or without leading `v`).
    pub pinned_version: Option<String>,
    /// Skip the interactive confirmation prompt.
    pub skip_confirm: bool,
}

// ── Build metadata (from build.rs) ───────────────────────────────────────────

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[must_use]
pub fn is_dev_build() -> bool {
    env!("COOP_BUILD_KIND") == "dev"
}

// ── Platform + URL helpers ───────────────────────────────────────────────────

pub fn target_triple() -> Result<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Ok("aarch64-unknown-linux-musl")
    } else {
        bail!(
            "No prebuilt coop binary for {}-{}; build from source.",
            env::consts::OS,
            env::consts::ARCH
        )
    }
}

#[must_use]
pub fn asset_name(tag: &str, triple: &str) -> String {
    format!("coop-{tag}-{triple}.tar.gz")
}

fn api_base() -> String {
    env::var("COOP_UPDATE_API_BASE_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

fn api_base_overridden() -> bool {
    env::var("COOP_UPDATE_API_BASE_URL").is_ok()
}

fn warn_if_api_base_overridden() {
    if api_base_overridden() {
        tracing::warn!(
            "COOP_UPDATE_API_BASE_URL is set — attestation verification is DISABLED. \
             This is a test-only mode; do not use with untrusted URLs."
        );
    }
}

/// Normalize a user-supplied version to a `v`-prefixed semver tag.
///
/// Rejects any input that does not parse as semver once the optional `v`
/// prefix is stripped. This is the security boundary for `coop update
/// --version <tag>`: the tag string is interpolated into the GitHub API
/// URL path, so admitting only semver-like values prevents path traversal
/// (e.g. `../other-user/repo/releases/latest`).
fn normalize_tag(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(body)
        .with_context(|| format!("--version {trimmed:?} is not a valid semver tag"))?;
    Ok(format!("v{body}"))
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

// ── GitHub release metadata ──────────────────────────────────────────────────

/// Minimal subset of the GitHub release JSON schema.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    #[serde(rename = "tag_name")]
    pub tag: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

impl Release {
    fn find_asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

pub fn fetch_latest() -> Result<Release> {
    fetch_release_metadata(REPO, "latest").context("Failed to fetch latest release metadata")
}

pub fn fetch_by_tag(tag: &str) -> Result<Release> {
    fetch_release_metadata(REPO, &format!("tags/{tag}"))
        .with_context(|| format!("Failed to fetch release metadata for {tag}"))
}

/// Fetch the `tag_name` of another repository's latest release.
///
/// Used by `coop agent update` to compare the guest's installed Codex
/// against the newest upstream tag. `repo` is a compile-time `owner/name`
/// slug (e.g. `openai/codex`) — never user input — so it carries none of
/// the path-traversal risk `coop update --version` guards against.
pub(crate) fn latest_release_tag(repo: &str) -> Result<String> {
    Ok(fetch_release_metadata(repo, "latest")
        .with_context(|| format!("Failed to fetch latest release metadata for {repo}"))?
        .tag)
}

/// Fetch release JSON for `repo` (an `owner/name` slug) and the given API
/// path suffix (`latest` or `tags/<tag>`).
///
/// Selects an auth strategy at call time so changes to `GITHUB_TOKEN` /
/// `gh auth` between invocations take effect.
fn fetch_release_metadata(repo: &str, path_suffix: &str) -> Result<Release> {
    let body = match select_auth_strategy_from_env() {
        AuthStrategy::Gh => gh_api_capture(&format!("repos/{repo}/releases/{path_suffix}"))?,
        AuthStrategy::CurlBearer(token) => {
            let url = format!("{}/repos/{repo}/releases/{path_suffix}", api_base());
            curl_capture(&url, Some(&token))?
        }
        AuthStrategy::CurlBare => {
            let url = format!("{}/repos/{repo}/releases/{path_suffix}", api_base());
            curl_capture(&url, None)?
        }
    };
    serde_json::from_str(&body).context("Failed to parse GitHub release JSON")
}

// ── Auth strategy selection ──────────────────────────────────────────────────

/// How to authenticate against GitHub for release metadata and asset downloads.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthStrategy {
    /// Use the `gh` CLI (already authenticated to github.com).
    Gh,
    /// Use curl with a bearer token from `GITHUB_TOKEN`.
    CurlBearer(String),
    /// Use curl without authentication (public repo or test fixture).
    CurlBare,
}

/// Pure strategy picker. Extracted from I/O so it is unit-testable.
///
/// When `api_base_overridden` is true, the integration test fixture is in use
/// and we must not consult `gh` or `GITHUB_TOKEN` — the local server speaks
/// neither.
fn select_auth_strategy(
    api_base_overridden: bool,
    has_gh: bool,
    gh_authed: bool,
    github_token: Option<&str>,
) -> AuthStrategy {
    if api_base_overridden {
        return AuthStrategy::CurlBare;
    }
    if has_gh && gh_authed {
        return AuthStrategy::Gh;
    }
    match github_token.filter(|t| !t.is_empty()) {
        Some(token) => AuthStrategy::CurlBearer(token.to_string()),
        None => AuthStrategy::CurlBare,
    }
}

fn select_auth_strategy_from_env() -> AuthStrategy {
    let overridden = api_base_overridden();
    let has_gh = !overridden && command_exists("gh");
    let gh_authed = has_gh && gh_authenticated();
    let token = env::var("GITHUB_TOKEN").ok();
    select_auth_strategy(overridden, has_gh, gh_authed, token.as_deref())
}

fn gh_authenticated() -> bool {
    Cmd::new("gh")
        .arg("auth")
        .arg("status")
        .arg("--hostname")
        .arg("github.com")
        .status_ok()
}

// ── Network I/O (shell-out to curl / gh) ─────────────────────────────────────

fn curl_capture(url: &str, bearer_token: Option<&str>) -> Result<String> {
    let mut cmd = Cmd::new("curl")
        .arg("-fsSL")
        .arg("-H")
        .arg("Accept: application/vnd.github+json");
    if let Some(token) = bearer_token {
        // Pass the auth header on stdin via curl's `-H @-` so the secret
        // never touches argv (visible in /proc and `Cmd::describe` logs).
        cmd = cmd
            .arg("-H")
            .arg("@-")
            .stdin_input(format!("Authorization: token {token}\n"));
    }
    cmd.arg(url)
        .capture()
        .with_context(|| format!("curl GET {url} failed"))
}

fn gh_api_capture(path: &str) -> Result<String> {
    Cmd::new("gh")
        .arg("api")
        .arg(path)
        .arg("-H")
        .arg("Accept: application/vnd.github+json")
        .capture()
        .with_context(|| format!("gh api {path} failed"))
}

/// Download a release asset, choosing auth strategy at call time.
///
/// `tag` and `asset_name` are required for the `gh release download` path;
/// `url` is the `browser_download_url` used by the curl fallbacks.
fn download_asset(tag: &str, asset_name: &str, url: &str, dest: &Path) -> Result<()> {
    match select_auth_strategy_from_env() {
        AuthStrategy::Gh => gh_release_download(tag, asset_name, dest),
        AuthStrategy::CurlBearer(token) => curl_download(url, dest, Some(&token)),
        AuthStrategy::CurlBare => curl_download(url, dest, None),
    }
}

fn curl_download(url: &str, dest: &Path, bearer_token: Option<&str>) -> Result<()> {
    let mut cmd = Cmd::new("curl").arg("-fsSL");
    if let Some(token) = bearer_token {
        // Pass the auth header on stdin via curl's `-H @-` so the secret
        // never touches argv (visible in /proc and `Cmd::describe` logs).
        cmd = cmd
            .arg("-H")
            .arg("@-")
            .stdin_input(format!("Authorization: token {token}\n"));
    }
    cmd.arg(url)
        .arg("-o")
        .arg(dest)
        .run()
        .with_context(|| format!("curl download from {url} failed"))
}

fn gh_release_download(tag: &str, asset_name: &str, dest: &Path) -> Result<()> {
    Cmd::new("gh")
        .arg("release")
        .arg("download")
        .arg(tag)
        .arg("--repo")
        .arg(REPO)
        .arg("--pattern")
        .arg(asset_name)
        .arg("--output")
        .arg(dest)
        .arg("--clobber")
        .run()
        .with_context(|| {
            format!(
                "gh release download {tag} --pattern {asset_name} -> {} failed",
                dest.display()
            )
        })
}

// ── Checksum verification ────────────────────────────────────────────────────

/// Parse a `sha256sum`-style `SHA256SUMS` file and return the digest for
/// `target_filename`. Handles both `<hash>  <file>` (binary mode) and
/// `<hash> *<file>` variants; tolerates blank lines and `#` comments.
///
/// Malformed digests for the target filename are skipped (parsing
/// continues), so the first well-formed entry wins.
#[must_use]
pub fn parse_sha256sums(content: &str, target_filename: &str) -> Option<Sha256Hash> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (hash, rest) = line.split_once(|c: char| c.is_ascii_whitespace())?;
        let file = rest.trim_start().trim_start_matches('*');
        if file == target_filename
            && let Ok(parsed) = hash.parse::<Sha256Hash>()
        {
            return Some(parsed);
        }
    }
    None
}

fn verify_sha256(file: &Path, expected: &Sha256Hash) -> Result<()> {
    let bytes = fs::read(file)
        .with_context(|| format!("Failed to read {} for checksum", file.display()))?;
    let actual = Sha256Hash::of(&bytes);
    ensure!(
        actual == *expected,
        "SHA-256 mismatch for {}: expected {expected}, got {actual}",
        file.display()
    );
    Ok(())
}

// ── Attestation verification (best-effort) ───────────────────────────────────

/// Build the `gh attestation verify` argument list.
///
/// With `bundle`, `gh` reads the Sigstore bundle from disk and makes no API
/// call, so verification needs no GitHub credential. Without it, `gh` fetches
/// the bundle from the attestations API and always attaches its stored token —
/// which 403s on public data when that token carries no SSO session for the
/// org. `--repo` pins the signer identity in both cases.
fn attestation_verify_args(tarball: &Path, bundle: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "attestation".into(),
        "verify".into(),
        tarball.as_os_str().to_owned(),
        "--repo".into(),
        REPO.into(),
    ];
    if let Some(bundle) = bundle {
        args.push("--bundle".into());
        args.push(bundle.as_os_str().to_owned());
    }
    args
}

/// Download the release's provenance bundle, if it published one.
///
/// `None` means verification falls back to the attestations API, or is skipped
/// outright — never that the update fails. A download that blips is the same
/// situation as a release that never published the asset, and `install.sh`
/// resolves it the same way. The download is also skipped when
/// `verify_attestation` would not read the result, so an update whose
/// attestation step is a no-op does no pointless work.
fn fetch_attestation_bundle(release: &Release, dir: &Path) -> Option<PathBuf> {
    if api_base_overridden() || !command_exists("gh") {
        return None;
    }
    let Some(asset) = release.find_asset(BUNDLE_ASSET) else {
        tracing::info!(
            "Release {} publishes no {BUNDLE_ASSET} — verifying the attestation through the \
             GitHub API, which requires a credential authorized for {REPO}.",
            release.tag
        );
        return None;
    };
    let dest = dir.join(BUNDLE_ASSET);
    if let Err(err) = download_asset(&release.tag, BUNDLE_ASSET, &asset.url, &dest) {
        tracing::warn!(
            "Failed to download {BUNDLE_ASSET} for release {} ({err:#}) — verifying the \
             attestation through the GitHub API, which requires a credential authorized for \
             {REPO}.",
            release.tag
        );
        return None;
    }
    Some(dest)
}

fn verify_attestation(tarball: &Path, bundle: Option<&Path>) -> Result<()> {
    // Skip when the API base is overridden — the local test fixture serves
    // synthetic artifacts that have no provenance in GitHub's attestation
    // API. `warn_if_api_base_overridden` has already surfaced this to the
    // user as a visible stderr warning.
    if api_base_overridden() {
        return Ok(());
    }
    if !command_exists("gh") {
        tracing::info!(
            "Note: `gh` not installed — skipped cryptographic attestation verification. \
             The download was verified against the published `SHA256SUMS` checksum, which \
             is the same assurance level as most `curl | bash` installers. For end-to-end \
             Sigstore verification, install `gh` (https://cli.github.com) and re-run, or \
             verify manually: `gh attestation verify <tarball> --repo {REPO} --bundle \
             {BUNDLE_ASSET}` against the {BUNDLE_ASSET} asset from the same release."
        );
        return Ok(());
    }
    Cmd::new("gh")
        .args(attestation_verify_args(tarball, bundle))
        .run()
        .with_context(|| {
            let hint = if bundle.is_some() {
                String::new()
            } else {
                format!(
                    " (verified through the GitHub API because the release publishes no \
                     {BUNDLE_ASSET}; an HTTP 403 here means your GitHub credential has no \
                     SSO session for the org)"
                )
            };
            format!(
                "Attestation verification failed for {} — refusing to install{hint}",
                tarball.display()
            )
        })
}

// ── Atomic self-replace ──────────────────────────────────────────────────────

fn check_parent_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".coop-update-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            if let Err(e) = fs::remove_file(&probe) {
                tracing::debug!("Failed to remove probe file {}: {e}", probe.display());
            }
            Ok(())
        }
        Err(e) => bail!(
            "Cannot write to {}: {e}.\n\
             Try `sudo coop update` if coop is installed in a protected directory.",
            dir.display()
        ),
    }
}

/// Atomically swap `new_binary` over `target`: stage a copy in the target's
/// directory, chmod + fsync it, then `rename` it into place (atomic on the
/// same filesystem, safe over a running binary on Unix).
fn atomic_replace(new_binary: &Path, target: &Path) -> Result<()> {
    let dir = target
        .parent()
        .context("Target executable has no parent directory")?;
    check_parent_writable(dir)?;

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .context("Target executable has no file name")?;
    let tmp = dir.join(format!(".{file_name}-update-{}", std::process::id()));
    fs::copy(new_binary, &tmp)
        .with_context(|| format!("Failed to stage update at {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to chmod staged binary {}", tmp.display()))?;

    fs::File::open(&tmp)
        .with_context(|| format!("Failed to reopen staged binary {}", tmp.display()))?
        .sync_all()
        .with_context(|| format!("Failed to fsync staged binary {}", tmp.display()))?;

    fs::rename(&tmp, target)
        .with_context(|| format!("Failed to swap {} over {}", tmp.display(), target.display()))?;
    Ok(())
}

fn atomic_replace_self(new_binary: &Path) -> Result<()> {
    let current = env::current_exe().context("Failed to resolve current executable path")?;
    atomic_replace(new_binary, &current)
}

/// Replace the sibling `coop-proxy` (issue #411) from the same verified
/// tarball so it never drifts from `coop`. Fails closed: if the tarball
/// carries a proxy but the sibling cannot be written, the update aborts
/// before `coop` itself is swapped. A no-op for older releases that predate
/// the bundled proxy.
fn replace_sibling_proxy(extract_dir: &Path) -> Result<()> {
    let current = env::current_exe().context("Failed to resolve current executable path")?;
    replace_sibling_proxy_at(extract_dir, &current)
}

/// Core of [`replace_sibling_proxy`] with the running-binary path injected so
/// the swap destination is testable without touching the real `coop` binary.
fn replace_sibling_proxy_at(extract_dir: &Path, current_exe: &Path) -> Result<()> {
    let new_proxy = extract_dir.join("coop-proxy");
    if !new_proxy.exists() {
        return Ok(());
    }
    let dir = current_exe
        .parent()
        .context("Current executable has no parent directory")?;
    atomic_replace(&new_proxy, &dir.join("coop-proxy"))
}

// ── Main update flow ─────────────────────────────────────────────────────────

pub fn run(opts: &UpdateOpts) -> Result<()> {
    if is_dev_build() {
        bail!(
            "This is a dev build ({}); `coop update` only replaces release binaries.\n\
             Re-run install.sh (or build from source) to replace a dev build.",
            env!("COOP_VERSION_STR")
        );
    }

    warn_if_api_base_overridden();

    let triple = target_triple()?;
    let current = Version::parse(current_version())
        .with_context(|| format!("Current version {} is not valid semver", current_version()))?;

    let release = match &opts.pinned_version {
        Some(v) => fetch_by_tag(&normalize_tag(v)?)?,
        None => fetch_latest()?,
    };
    let target = Version::parse(strip_v(&release.tag))
        .with_context(|| format!("Release tag {} is not valid semver", release.tag))?;

    let newer = target > current;
    if opts.check_only {
        if newer {
            tracing::info!("Update available: {current} -> {target}");
        } else {
            tracing::info!("Up to date: coop {current}");
        }
        return Ok(());
    }

    if !newer && !opts.force && opts.pinned_version.is_none() {
        tracing::info!("Already on latest: coop {current}");
        return Ok(());
    }

    if !opts.skip_confirm && !confirm(&format!("Update coop from {current} to {target}?"))? {
        tracing::info!("Update cancelled");
        return Ok(());
    }

    perform_update(&release, triple)?;
    tracing::info!("coop updated to {target}");
    persist_state(Some(&release.tag));
    Ok(())
}

fn perform_update(release: &Release, triple: &str) -> Result<()> {
    let tmp = tempfile::tempdir().context("Failed to create temporary working directory")?;
    let tarball_name = asset_name(&release.tag, triple);

    let tarball_asset = release.find_asset(&tarball_name).with_context(|| {
        format!(
            "Release {} has no asset {tarball_name}; \
             this platform may not be supported by that release.",
            release.tag
        )
    })?;
    let sums_asset = release
        .find_asset("SHA256SUMS")
        .context("Release has no SHA256SUMS asset; refusing to install unverified binary")?;

    let tarball_path = tmp.path().join(&tarball_name);
    let sums_path = tmp.path().join("SHA256SUMS");

    tracing::info!("Downloading {tarball_name}");
    download_asset(
        &release.tag,
        &tarball_name,
        &tarball_asset.url,
        &tarball_path,
    )?;
    download_asset(&release.tag, "SHA256SUMS", &sums_asset.url, &sums_path)?;

    let sums_content = fs::read_to_string(&sums_path)
        .with_context(|| format!("Failed to read {}", sums_path.display()))?;
    let expected = parse_sha256sums(&sums_content, &tarball_name)
        .with_context(|| format!("{tarball_name} not listed in SHA256SUMS"))?;
    verify_sha256(&tarball_path, &expected)?;

    let bundle_path = fetch_attestation_bundle(release, tmp.path());
    verify_attestation(&tarball_path, bundle_path.as_deref())?;

    // `--no-same-owner --no-same-permissions` ignore embedded uid/mode metadata.
    // `-C <tempdir>` plus modern tar's default refusal of `..`-segmented and absolute
    // paths keep extraction inside the tempdir even if the archive is malicious.
    Cmd::new("tar")
        .arg("-xzf")
        .arg(&tarball_path)
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .arg("-C")
        .arg(tmp.path())
        .run()
        .context("Failed to extract release tarball")?;

    let extract_dir = tmp.path().join(format!("coop-{}-{triple}", release.tag));
    let extracted = extract_dir.join("coop");
    ensure!(
        extracted.exists(),
        "Extracted binary not found at {}",
        extracted.display()
    );

    // Swap the sibling proxy first (from the same verified tarball) so a
    // proxy-write failure aborts before coop itself is replaced, keeping the
    // two in lockstep.
    replace_sibling_proxy(&extract_dir)?;
    atomic_replace_self(&extracted)
}

// ── Background update-check state ────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UpdateState {
    #[serde(default)]
    last_checked_at: u64,
    #[serde(default)]
    latest_known_version: Option<String>,
}

fn state_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_local_dir)?;
    Some(base.join("coop").join("update-check.json"))
}

/// Remove the background update-check state file (and its parent if empty).
///
/// Best-effort — used by `coop uninstall`. Returns `Ok` even if nothing exists.
pub fn remove_state() -> Result<()> {
    let Some(path) = state_path() else {
        return Ok(());
    };
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("Failed to remove {}", path.display()))?;
    }
    if let Some(parent) = path.parent()
        && parent.exists()
    {
        // remove_dir only succeeds when empty — perfect for "leave alone if shared".
        if let Err(e) = fs::remove_dir(parent) {
            tracing::debug!("Leaving state dir {} in place ({e})", parent.display());
        }
    }
    Ok(())
}

fn read_state() -> Option<UpdateState> {
    let path = state_path()?;
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_state(state: &UpdateState) -> Result<()> {
    let path = state_path().context("Cannot determine state directory")?;
    let json = serde_json::to_string_pretty(state).context("Failed to serialize update state")?;
    atomic_write_json(&path, &json)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Persist the update-check state, swallowing I/O failures.
///
/// State writes are best-effort — if XDG dirs are unavailable or the filesystem
/// is read-only we silently skip, letting the background check retry next run.
fn persist_state(tag: Option<&str>) {
    let state = UpdateState {
        last_checked_at: now_unix(),
        latest_known_version: tag.map(str::to_string),
    };
    if let Err(e) = write_state(&state) {
        tracing::debug!("Failed to persist update-check state: {e}");
    }
}

// ── Disable sources (env + TTY + dev) ────────────────────────────────────────

fn background_check_disabled() -> bool {
    if is_dev_build() {
        return true;
    }
    if env::var("COOP_NO_UPDATE_CHECK")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return true;
    }
    if env::var("CI").map(|v| v == "true").unwrap_or(false) {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        return true;
    }
    false
}

// ── Public notify + check entrypoints ────────────────────────────────────────

/// Print a one-line notice on stderr if a newer release is known from the
/// last successful background check. Silent otherwise.
pub fn maybe_print_notify(cfg: &UpdateConfig) {
    if cfg.mode == UpdateMode::Off || background_check_disabled() {
        return;
    }
    let Ok(current) = Version::parse(current_version()) else {
        return;
    };
    let Some(latest) = read_state()
        .as_ref()
        .and_then(|s| s.latest_known_version.as_deref())
        .and_then(|t| Version::parse(strip_v(t)).ok())
    else {
        return;
    };
    if notify_is_due(&current, &latest) {
        tracing::warn!("A newer coop ({latest}) is available. Run `coop update` to install it.");
    }
}

/// Pure comparison used by [`maybe_print_notify`]. Extracted for testability —
/// the state-I/O seam in `maybe_print_notify` is not easily mocked without
/// dependency injection.
fn notify_is_due(current: &Version, latest: &Version) -> bool {
    latest > current
}

/// Kick off a non-blocking background refresh of release metadata if the
/// persisted state is older than `check_interval_hours`. Safe to call on
/// every command: honours all disable sources and never blocks the caller.
///
/// The `last_checked_at` stamp is written synchronously **before** the thread
/// is spawned — short-lived commands (`coop status`, `coop stop`) often exit
/// before the HTTPS round-trip completes, so without the pre-spawn stamp the
/// interval gate would retrigger on every invocation.
pub fn maybe_run_background_check(cfg: &UpdateConfig) {
    if cfg.mode == UpdateMode::Off || background_check_disabled() {
        return;
    }
    let state = read_state().unwrap_or_default();
    if !interval_elapsed(now_unix(), state.last_checked_at, cfg.check_interval_hours) {
        return;
    }
    persist_state(state.latest_known_version.as_deref());
    std::thread::spawn(|| match fetch_latest() {
        Ok(release) => persist_state(Some(&release.tag)),
        Err(e) => tracing::debug!("background update-check failed: {e}"),
    });
}

/// Pure interval check used by [`maybe_run_background_check`]. Returns `true`
/// when `now - last_checked_at >= interval_hours * 3600`.
fn interval_elapsed(now: u64, last_checked_at: u64, interval_hours: u64) -> bool {
    let interval_secs = interval_hours.saturating_mul(3600);
    now.saturating_sub(last_checked_at) >= interval_secs
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    #[test]
    fn strip_v_removes_leading_v() {
        assert_eq!(strip_v("v0.3.1"), "0.3.1");
        assert_eq!(strip_v("0.3.1"), "0.3.1");
        assert_eq!(strip_v("v"), "");
    }

    #[test]
    fn normalize_tag_adds_v_when_missing() {
        assert_eq!(normalize_tag("0.3.1").unwrap(), "v0.3.1");
        assert_eq!(normalize_tag("v0.3.1").unwrap(), "v0.3.1");
        assert_eq!(normalize_tag(" 0.3.1 ").unwrap(), "v0.3.1");
        assert_eq!(normalize_tag("0.3.1-rc.1").unwrap(), "v0.3.1-rc.1");
    }

    #[test]
    fn normalize_tag_rejects_non_semver_inputs() {
        // Prevents path traversal via the tag segment of the GitHub API URL.
        for bad in [
            "../attacker/evil/releases/latest",
            "latest",
            "0.3.1/../evil",
            "",
            "v",
            "not-a-version",
        ] {
            assert!(
                normalize_tag(bad).is_err(),
                "normalize_tag should reject {bad:?}"
            );
        }
    }

    #[test]
    fn asset_name_matches_release_workflow_naming() {
        assert_eq!(
            asset_name("v0.3.1", "x86_64-unknown-linux-musl"),
            "coop-v0.3.1-x86_64-unknown-linux-musl.tar.gz"
        );
    }

    #[test]
    fn parse_sha256sums_finds_matching_entry() {
        let content = concat!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  other.tar.gz\n",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  coop-v1.tar.gz\n",
        );
        let got = parse_sha256sums(content, "coop-v1.tar.gz").unwrap();
        assert_eq!(
            got.to_string(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn parse_sha256sums_handles_binary_asterisk_form() {
        let content =
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc *coop.tar.gz\n";
        assert_eq!(
            parse_sha256sums(content, "coop.tar.gz"),
            Some(
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn parse_sha256sums_skips_blank_and_comment_lines() {
        let content = concat!(
            "\n",
            "# generated by release.yml\n",
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd  x.tar.gz\n",
        );
        assert_eq!(
            parse_sha256sums(content, "x.tar.gz"),
            Some(
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn parse_sha256sums_returns_none_for_missing_entry() {
        let content =
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee  other.tar.gz\n";
        assert!(parse_sha256sums(content, "coop.tar.gz").is_none());
    }

    #[test]
    fn parse_sha256sums_rejects_malformed_hash() {
        let short = "abcd  x.tar.gz\n";
        assert!(parse_sha256sums(short, "x.tar.gz").is_none());
        let nonhex = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  x.tar.gz\n";
        assert!(parse_sha256sums(nonhex, "x.tar.gz").is_none());
    }

    #[test]
    fn semver_ordering_tracks_expectations() {
        let a = Version::parse("0.3.1").unwrap();
        let b = Version::parse("0.3.2").unwrap();
        let pre = Version::parse("0.4.0-rc.1").unwrap();
        let rel = Version::parse("0.4.0").unwrap();
        assert!(b > a);
        assert!(rel > pre, "pre-release must sort below its release");
        assert!(pre > b);
    }

    #[test]
    fn target_triple_resolves_on_this_host() {
        // Should succeed on every host CI runs on; the function only bails
        // on truly unsupported targets (e.g. x86_64-apple-darwin).
        let triple = target_triple();
        if cfg!(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "aarch64"),
        )) {
            assert!(triple.is_ok());
        }
    }

    #[test]
    fn update_mode_default_is_notify() {
        assert_eq!(UpdateMode::default(), UpdateMode::Notify);
    }

    #[test]
    fn update_config_default_interval_is_24_hours() {
        let cfg = UpdateConfig::default();
        assert_eq!(cfg.check_interval_hours, 24);
        assert_eq!(cfg.mode, UpdateMode::Notify);
    }

    #[test]
    fn update_config_deserializes_snake_case_modes() {
        let cfg: UpdateConfig = toml::from_str(r#"mode = "off""#).unwrap();
        assert_eq!(cfg.mode, UpdateMode::Off);
        let cfg: UpdateConfig = toml::from_str(r#"mode = "notify""#).unwrap();
        assert_eq!(cfg.mode, UpdateMode::Notify);
    }

    #[test]
    fn verify_sha256_accepts_correct_hash_and_rejects_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload");
        fs::write(&path, b"hello world").unwrap();

        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        let correct: Sha256Hash =
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
                .parse()
                .unwrap();
        verify_sha256(&path, &correct).unwrap();
        let wrong: Sha256Hash = "0".repeat(64).parse().unwrap();
        verify_sha256(&path, &wrong).unwrap_err();
    }

    #[test]
    fn attestation_verify_args_pin_repo_and_omit_bundle_when_absent() {
        let args = attestation_verify_args(Path::new("/tmp/coop.tar.gz"), None);
        assert_eq!(
            args,
            ["attestation", "verify", "/tmp/coop.tar.gz", "--repo", REPO]
        );
    }

    #[test]
    fn attestation_verify_args_append_bundle_when_present() {
        let args = attestation_verify_args(
            Path::new("/tmp/coop.tar.gz"),
            Some(Path::new("/tmp/attestations.jsonl")),
        );
        assert_eq!(
            args,
            [
                "attestation",
                "verify",
                "/tmp/coop.tar.gz",
                "--repo",
                REPO,
                "--bundle",
                "/tmp/attestations.jsonl",
            ]
        );
    }

    #[test]
    fn bundle_asset_is_found_on_a_release_that_publishes_it() {
        let release: Release = serde_json::from_str(&format!(
            r#"{{"tag_name":"v9.9.9","assets":[
                 {{"name":"SHA256SUMS","browser_download_url":"https://example.com/S"}},
                 {{"name":"{BUNDLE_ASSET}","browser_download_url":"https://example.com/B"}}
               ]}}"#
        ))
        .unwrap();
        assert_eq!(
            release.find_asset(BUNDLE_ASSET).map(|a| a.url.as_str()),
            Some("https://example.com/B")
        );

        // A release predating the asset must fall back, not match something else.
        let old: Release = serde_json::from_str(
            r#"{"tag_name":"v0.5.4","assets":[
                 {"name":"SHA256SUMS","browser_download_url":"https://example.com/S"}
               ]}"#,
        )
        .unwrap();
        assert!(old.find_asset(BUNDLE_ASSET).is_none());
    }

    /// The asset name is agreed across three files with no compiler link
    /// between them. A rename in one silently degrades `coop update` and
    /// `install.sh` back to the credential-requiring API path.
    ///
    /// `include_str!` is the tripwire, so moving either file breaks this test
    /// as a compile error rather than a named assertion failure.
    #[test]
    fn bundle_asset_name_matches_release_workflow_and_installer() {
        let workflow = include_str!("../.github/workflows/release.yml");
        let installer = include_str!("../install.sh");
        // The workflow names the asset twice: the `jq` output redirect that
        // creates it, and the `gh release create` that publishes it. Only the
        // latter makes it reachable by a client, so assert on that line.
        assert!(
            workflow
                .lines()
                .any(|l| l.contains("gh release create") && l.contains(BUNDLE_ASSET)),
            "release.yml no longer publishes {BUNDLE_ASSET} as a release asset"
        );
        assert!(
            installer.contains(BUNDLE_ASSET),
            "install.sh no longer downloads {BUNDLE_ASSET}"
        );
    }

    #[test]
    fn parse_sha256sums_returns_first_of_duplicates() {
        let content = concat!(
            "1111111111111111111111111111111111111111111111111111111111111111  x.tar.gz\n",
            "2222222222222222222222222222222222222222222222222222222222222222  x.tar.gz\n",
        );
        assert_eq!(
            parse_sha256sums(content, "x.tar.gz"),
            Some(
                "1111111111111111111111111111111111111111111111111111111111111111"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn parse_sha256sums_tolerates_crlf_line_endings() {
        let content =
            "3333333333333333333333333333333333333333333333333333333333333333  x.tar.gz\r\n";
        assert_eq!(
            parse_sha256sums(content, "x.tar.gz"),
            Some(
                "3333333333333333333333333333333333333333333333333333333333333333"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn notify_is_due_tracks_strict_newer() {
        let current = Version::parse("0.3.1").unwrap();
        assert!(notify_is_due(&current, &Version::parse("0.3.2").unwrap()));
        assert!(notify_is_due(&current, &Version::parse("1.0.0").unwrap()));
        assert!(!notify_is_due(&current, &current));
        assert!(!notify_is_due(&current, &Version::parse("0.3.0").unwrap()));
        // Pre-release of a future minor still sorts below the release but above current.
        assert!(notify_is_due(
            &current,
            &Version::parse("0.4.0-rc.1").unwrap()
        ));
    }

    #[test]
    fn interval_elapsed_behaviour() {
        let one_hour = 3600;
        // Zero last_checked_at means "never" — always elapsed.
        assert!(interval_elapsed(100_000, 0, 24));
        // Freshly checked — not elapsed.
        assert!(!interval_elapsed(100_000, 99_999, 1));
        // Exactly at the boundary counts as elapsed.
        assert!(interval_elapsed(100_000 + one_hour, 100_000, 1));
        // One second before the boundary does not.
        assert!(!interval_elapsed(100_000 + one_hour - 1, 100_000, 1));
        // Saturating arithmetic: now earlier than last_checked_at must not panic.
        assert!(!interval_elapsed(0, 100_000, 24));
    }

    #[test]
    fn select_auth_strategy_prefers_bare_when_api_base_overridden() {
        // Even with gh authed and a token present, the local fixture forces bare curl.
        let strat = select_auth_strategy(true, true, true, Some("ghp_xyz"));
        assert_eq!(strat, AuthStrategy::CurlBare);
    }

    #[test]
    fn select_auth_strategy_prefers_gh_when_authed() {
        let strat = select_auth_strategy(false, true, true, Some("ghp_xyz"));
        assert_eq!(strat, AuthStrategy::Gh);
    }

    #[test]
    fn select_auth_strategy_falls_back_to_token_when_gh_unauthed() {
        // gh installed but not authed -> use token if available.
        let strat = select_auth_strategy(false, true, false, Some("ghp_abc"));
        assert_eq!(strat, AuthStrategy::CurlBearer("ghp_abc".to_string()));
    }

    #[test]
    fn select_auth_strategy_falls_back_to_token_when_gh_missing() {
        let strat = select_auth_strategy(false, false, false, Some("ghp_abc"));
        assert_eq!(strat, AuthStrategy::CurlBearer("ghp_abc".to_string()));
    }

    #[test]
    fn select_auth_strategy_uses_bare_when_no_auth_available() {
        let strat = select_auth_strategy(false, false, false, None);
        assert_eq!(strat, AuthStrategy::CurlBare);
    }

    #[test]
    fn select_auth_strategy_treats_empty_token_as_absent() {
        // GITHUB_TOKEN="" should not produce a bogus Authorization header.
        let strat = select_auth_strategy(false, false, false, Some(""));
        assert_eq!(strat, AuthStrategy::CurlBare);
    }

    #[test]
    fn select_auth_strategy_ignores_token_when_gh_authed() {
        // gh wins over token when both are available — no need to leak the token.
        let strat = select_auth_strategy(false, true, true, Some("ghp_xyz"));
        assert_eq!(strat, AuthStrategy::Gh);
    }

    #[test]
    fn serde_deserializes_release_from_github_shape() {
        let json = r#"{
            "tag_name": "v9.9.9",
            "assets": [
                {"name": "x.tar.gz", "browser_download_url": "https://example.com/x.tar.gz"}
            ]
        }"#;
        let release: Release = serde_json::from_str(json).unwrap();
        assert_eq!(release.tag, "v9.9.9");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].name, "x.tar.gz");
        assert_eq!(release.assets[0].url, "https://example.com/x.tar.gz");
    }

    #[test]
    fn replace_sibling_proxy_is_noop_for_release_without_proxy() {
        // Old release: the tarball carries no coop-proxy, so the swap returns
        // before ever resolving the running binary.
        let extract = tempfile::tempdir().unwrap();
        replace_sibling_proxy(extract.path()).unwrap();
    }

    #[test]
    fn replace_sibling_proxy_swaps_sibling_next_to_coop() {
        let extract = tempfile::tempdir().unwrap();
        fs::write(extract.path().join("coop-proxy"), b"new-proxy").unwrap();

        let install = tempfile::tempdir().unwrap();
        let coop = install.path().join("coop");
        fs::write(&coop, b"coop-binary").unwrap();

        replace_sibling_proxy_at(extract.path(), &coop).unwrap();

        let sibling = install.path().join("coop-proxy");
        assert_eq!(fs::read(&sibling).unwrap(), b"new-proxy");
        assert_eq!(
            fs::metadata(&sibling).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn replace_sibling_proxy_fails_closed_when_sibling_unwritable() {
        // The tarball carries a proxy but the sibling cannot be written: the
        // swap must return Err so perform_update never reaches
        // atomic_replace_self and coop is left untouched.
        let extract = tempfile::tempdir().unwrap();
        fs::write(extract.path().join("coop-proxy"), b"new-proxy").unwrap();

        // A running-binary path whose parent directory does not exist.
        let missing = extract.path().join("no-such-dir").join("coop");
        replace_sibling_proxy_at(extract.path(), &missing).unwrap_err();
    }
}
