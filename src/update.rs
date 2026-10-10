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
use url::Url;

use crate::cmd::{Cmd, command_exists};
use crate::fs_util::atomic_write_json;
use crate::prompt::confirm;
use crate::sha256_hash::Sha256Hash;
#[cfg(test)]
use crate::update_policy::{Asset, asset_name, normalize_tag, validate_asset_identity};
use crate::update_policy::{
    BUNDLE_ASSET, LEGACY_API_OVERRIDE, REPO, Release, ReleaseTag, TEST_API_BASE_URL,
    TEST_VERIFY_ATTESTATION, UpdateSource, ValidatedAsset, ValidatedRelease, strip_v,
    validate_release_assets, validate_release_tag,
};

const SIGNER_WORKFLOW: &str = ".github/workflows/release.yml";
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

fn self_update_source() -> Result<UpdateSource> {
    UpdateSource::for_current_build(
        env::var_os(LEGACY_API_OVERRIDE).is_some(),
        env::var(TEST_API_BASE_URL).ok().as_deref(),
        env::var(TEST_VERIFY_ATTESTATION).as_deref() == Ok("1"),
    )
}

fn fetch_self_release(source: &UpdateSource, requested: Option<&ReleaseTag>) -> Result<Release> {
    let suffix = requested.map_or_else(|| "latest".to_string(), |tag| format!("tags/{tag}"));
    fetch_release_metadata(source, REPO, &suffix).with_context(|| match requested {
        Some(tag) => format!("Failed to fetch release metadata for {tag}"),
        None => "Failed to fetch latest release metadata".to_string(),
    })
}

/// Fetch the `tag_name` of another repository's latest release.
///
/// Used by `coop agent update` to compare the guest's installed Codex
/// against the newest upstream tag. `repo` is a compile-time `owner/name`
/// slug (e.g. `openai/codex`) — never user input — so it carries none of
/// the path-traversal risk `coop update --version` guards against.
pub(crate) fn latest_release_tag(repo: &str) -> Result<String> {
    Ok(
        fetch_release_metadata(&UpdateSource::official(), repo, "latest")
            .with_context(|| format!("Failed to fetch latest release metadata for {repo}"))?
            .tag,
    )
}

/// Fetch release JSON for `repo` (an `owner/name` slug) and the given API
/// path suffix (`latest` or `tags/<tag>`).
///
/// Selects an auth strategy at call time so changes to `GITHUB_TOKEN` /
/// `gh auth` between invocations take effect.
fn fetch_release_metadata(source: &UpdateSource, repo: &str, path_suffix: &str) -> Result<Release> {
    let body = match select_auth_strategy_from_env(source) {
        AuthStrategy::Gh => gh_api_capture(&format!("repos/{repo}/releases/{path_suffix}"))?,
        AuthStrategy::CurlBearer(token) => {
            let url = source.metadata_url(repo, path_suffix)?;
            curl_capture(source, &url, Some(&token))?
        }
        AuthStrategy::CurlBare => {
            let url = source.metadata_url(repo, path_suffix)?;
            curl_capture(source, &url, None)?
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
/// When `fixture_source` is true, the integration test fixture is in use
/// and we must not consult `gh` or `GITHUB_TOKEN` — the local server speaks
/// neither.
fn select_auth_strategy(
    fixture_source: bool,
    has_gh: bool,
    gh_authed: bool,
    github_token: Option<&str>,
) -> AuthStrategy {
    if fixture_source {
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

fn select_auth_strategy_from_env(source: &UpdateSource) -> AuthStrategy {
    let fixture = source.is_fixture();
    let has_gh = !fixture && command_exists("gh");
    let gh_authed = has_gh && gh_authenticated();
    let token = env::var("GITHUB_TOKEN").ok();
    select_auth_strategy(fixture, has_gh, gh_authed, token.as_deref())
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

fn curl_command(source: &UpdateSource) -> Cmd {
    let (initial, redirects) = source.curl_protocols();
    Cmd::new("curl")
        .arg("--proto")
        .arg(initial)
        .arg("--proto-redir")
        .arg(redirects)
}

fn curl_capture(source: &UpdateSource, url: &Url, bearer_token: Option<&str>) -> Result<String> {
    let mut cmd = curl_command(source)
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
    cmd.arg(url.as_str())
        .capture()
        .with_context(|| format!("curl GET {url} failed"))
}

fn gh_api_capture(path: &str) -> Result<String> {
    gh_api_command(path)
        .capture()
        .with_context(|| format!("gh api {path} failed"))
}

fn gh_api_command(path: &str) -> Cmd {
    Cmd::new("gh")
        .arg("api")
        .arg(path)
        .arg("--hostname")
        .arg("github.com")
        .arg("-H")
        .arg("Accept: application/vnd.github+json")
}

/// Download a release asset, choosing auth strategy at call time.
///
fn download_asset(asset: &ValidatedAsset, dest: &Path) -> Result<()> {
    match select_auth_strategy_from_env(asset.source()) {
        AuthStrategy::Gh => gh_release_download(asset.tag(), asset.name(), dest),
        AuthStrategy::CurlBearer(token) => curl_download(asset, dest, Some(&token)),
        AuthStrategy::CurlBare => curl_download(asset, dest, None),
    }
}

fn curl_download(asset: &ValidatedAsset, dest: &Path, bearer_token: Option<&str>) -> Result<()> {
    let mut cmd = curl_command(asset.source()).arg("-fsSL");
    if let Some(token) = bearer_token {
        // Pass the auth header on stdin via curl's `-H @-` so the secret
        // never touches argv (visible in /proc and `Cmd::describe` logs).
        cmd = cmd
            .arg("-H")
            .arg("@-")
            .stdin_input(format!("Authorization: token {token}\n"));
    }
    cmd.arg(asset.url().as_str())
        .arg("-o")
        .arg(dest)
        .run()
        .with_context(|| format!("curl download of {} failed", asset.name()))
}

fn gh_release_download(tag: &ReleaseTag, asset_name: &str, dest: &Path) -> Result<()> {
    gh_release_download_command(tag, asset_name, dest)
        .run()
        .with_context(|| {
            format!(
                "gh release download {tag} --pattern {asset_name} -> {} failed",
                dest.display()
            )
        })
}

fn gh_release_download_command(tag: &ReleaseTag, asset_name: &str, dest: &Path) -> Cmd {
    Cmd::new("gh")
        .arg("release")
        .arg("download")
        .arg(tag.as_str())
        .arg("--repo")
        .arg(format!("github.com/{REPO}"))
        .arg("--pattern")
        .arg(asset_name)
        .arg("--output")
        .arg(dest)
        .arg("--clobber")
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
/// With `bundle`, `gh` reads the Sigstore bundle from disk rather than the
/// attestations API, so no GitHub credential is involved. What each transport
/// does and does not pin is recorded in the `coop update` trust chain in
/// `docs/trust-model.md`.
fn attestation_verify_args(tarball: &Path, tag: &str, bundle: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "attestation".into(),
        "verify".into(),
        tarball.as_os_str().to_owned(),
        "--repo".into(),
        REPO.into(),
        "--cert-identity".into(),
        format!("https://github.com/{REPO}/{SIGNER_WORKFLOW}@refs/tags/{tag}").into(),
        "--source-ref".into(),
        format!("refs/tags/{tag}").into(),
        "--deny-self-hosted-runners".into(),
    ];
    if let Some(bundle) = bundle {
        args.push("--bundle".into());
        args.push(bundle.as_os_str().to_owned());
    }
    args
}

/// Whether the release's provenance bundle can be used.
///
/// Decided before any IO so every outcome is unit-testable; `resolve_provenance`
/// performs the download this describes.
#[derive(Debug)]
enum BundleDecision<'a> {
    /// The explicit test source serves synthetic artifacts that have no
    /// provenance in GitHub's attestation API, so verification is skipped.
    TestMode,
    /// `gh` is absent, so nothing can verify an attestation.
    NoGh,
    /// The release publishes no bundle asset.
    NoAsset,
    /// The release publishes one; download it from this asset.
    Fetch(&'a ValidatedAsset),
}

fn bundle_decision(release: &ValidatedRelease, gh_present: bool) -> BundleDecision<'_> {
    if !release.tarball().source().verifies_attestation() {
        return BundleDecision::TestMode;
    }
    if !gh_present {
        return BundleDecision::NoGh;
    }
    match release.bundle() {
        Some(asset) => BundleDecision::Fetch(asset),
        None => BundleDecision::NoAsset,
    }
}

/// Why verification fell back to the attestations API.
///
/// All three mean the same thing to the client — no bundle to read — but they
/// are kept apart so the failure message can name the one that happened.
#[derive(Debug)]
enum ApiReason {
    /// The release publishes no bundle asset.
    NoAsset,
    /// The bundle asset exists but could not be downloaded.
    DownloadFailed,
    /// The bundle downloaded but is empty. `gh` before 2.56.0 (cli/cli#9541)
    /// reports success on an empty bundle, having verified nothing, so an
    /// empty file must never reach `--bundle`.
    EmptyBundle,
}

/// What `verify_attestation` verifies against.
///
/// The reason a bundle is absent is carried here rather than re-derived from
/// its absence, so a failure message cannot claim the wrong one.
#[derive(Debug)]
enum Provenance {
    /// Verification is skipped; see [`BundleDecision::TestMode`].
    TestMode,
    /// Verification is skipped; see [`BundleDecision::NoGh`].
    NoGh,
    /// Verify through the attestations API. The store itself is anonymously
    /// readable for a public repo, but `gh` refuses to run the command without
    /// `--bundle` unless it is logged in, so this path needs a credential
    /// authorized for the org.
    Api(ApiReason),
    /// Verify against this downloaded bundle, with no credential.
    Bundle(PathBuf),
}

impl Provenance {
    /// The path for `gh --bundle`, or `None` to use the attestations API.
    fn bundle(&self) -> Option<&Path> {
        match self {
            Provenance::Bundle(path) => Some(path),
            Provenance::TestMode | Provenance::NoGh | Provenance::Api(_) => None,
        }
    }

    /// Explains the API fallback in a verification failure message. Empty
    /// unless verification actually went through the API.
    fn api_fallback_hint(&self) -> String {
        let cause = match self {
            Provenance::Api(ApiReason::NoAsset) => {
                format!("the release publishes no {BUNDLE_ASSET}")
            }
            Provenance::Api(ApiReason::DownloadFailed) => {
                format!("{BUNDLE_ASSET} could not be downloaded")
            }
            Provenance::Api(ApiReason::EmptyBundle) => {
                format!("the published {BUNDLE_ASSET} is empty")
            }
            Provenance::TestMode | Provenance::NoGh | Provenance::Bundle(_) => {
                return String::new();
            }
        };
        format!(
            " (verified through the GitHub API because {cause}; an HTTP 403 here means your \
             GitHub credential has no SSO session for {REPO})"
        )
    }
}

/// Resolve how the attestation will be verified, downloading the release's
/// provenance bundle when it publishes a usable one.
///
/// Never fails the update: every problem with the bundle falls back to the
/// attestations API or skips verification outright.
fn resolve_provenance(release: &ValidatedRelease, dir: &Path) -> Provenance {
    let asset = match bundle_decision(release, command_exists("gh")) {
        BundleDecision::TestMode => return Provenance::TestMode,
        BundleDecision::NoGh => return Provenance::NoGh,
        BundleDecision::NoAsset => {
            tracing::info!(
                "Release {} publishes no {BUNDLE_ASSET} — verifying the attestation through the \
                 GitHub API, for which `gh` needs a credential authorized for {REPO}.",
                release.tag()
            );
            return Provenance::Api(ApiReason::NoAsset);
        }
        BundleDecision::Fetch(asset) => asset,
    };

    let dest = dir.join(BUNDLE_ASSET);
    // Deliberately not `download_asset`: it routes through
    // `select_auth_strategy_from_env`, which prefers `gh` and then a
    // `GITHUB_TOKEN` bearer, re-attaching the credential `--bundle` exists to
    // avoid. A token with no SSO session for the org would 403 on this one step
    // and drop the chain back to the API path, failing with the original error.
    // The bundle is a public release asset, so a bare curl reaches it and keeps
    // the path credential-free end to end.
    if let Err(err) = curl_download(asset, &dest, None) {
        tracing::warn!(
            "Failed to download {BUNDLE_ASSET} for release {} ({err:#}) — verifying the \
             attestation through the GitHub API, for which `gh` needs a credential authorized \
             for {REPO}.",
            release.tag()
        );
        return Provenance::Api(ApiReason::DownloadFailed);
    }
    if !fs::metadata(&dest).is_ok_and(|meta| meta.len() > 0) {
        tracing::warn!(
            "{BUNDLE_ASSET} for release {} is empty — verifying the attestation through the \
             GitHub API, for which `gh` needs a credential authorized for {REPO}.",
            release.tag()
        );
        return Provenance::Api(ApiReason::EmptyBundle);
    }
    Provenance::Bundle(dest)
}

fn verify_attestation(tarball: &Path, tag: &str, provenance: &Provenance) -> Result<()> {
    match provenance {
        Provenance::TestMode => return Ok(()),
        Provenance::NoGh => {
            tracing::info!(
                "Note: `gh` not installed — skipped cryptographic attestation verification. \
                 The download was verified against the published `SHA256SUMS` checksum, which \
                 is the same assurance level as most `curl | bash` installers. For end-to-end \
                 Sigstore verification, install `gh` (https://cli.github.com) and re-run, or \
                 verify manually: `gh attestation verify <tarball> --repo {REPO} \
                 --cert-identity https://github.com/{REPO}/{SIGNER_WORKFLOW}@refs/tags/{tag} \
                 --source-ref refs/tags/{tag} --deny-self-hosted-runners --bundle \
                 {BUNDLE_ASSET}` against the {BUNDLE_ASSET} asset from the same release."
            );
            return Ok(());
        }
        Provenance::Api(_) | Provenance::Bundle(_) => {}
    }
    Cmd::new("gh")
        .args(attestation_verify_args(tarball, tag, provenance.bundle()))
        .run()
        .with_context(|| {
            format!(
                "Attestation verification failed for {} — refusing to install{}",
                tarball.display(),
                provenance.api_fallback_hint()
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

    let source = self_update_source()?;
    if source.is_fixture() {
        if source.verifies_attestation() {
            tracing::warn!("using the explicit test update source with attestation verification");
        } else {
            tracing::warn!(
                "using the explicit test update source — attestation verification is DISABLED"
            );
        }
    }

    let triple = target_triple()?;
    let current = Version::parse(current_version())
        .with_context(|| format!("Current version {} is not valid semver", current_version()))?;

    let requested = opts
        .pinned_version
        .as_deref()
        .map(ReleaseTag::from_requested)
        .transpose()?;
    let release = fetch_self_release(&source, requested.as_ref())?;
    let returned_tag = validate_release_tag(&release, requested.as_ref())?;
    let target = returned_tag.version().clone();

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

    // Validate every asset identity together before prompting or crossing the
    // downloader boundary. A valid archive entry cannot authorize its checksum
    // or optional attestation neighbor.
    let validated = validate_release_assets(&source, &release, requested.as_ref(), triple)?;

    if !opts.skip_confirm && !confirm(&format!("Update coop from {current} to {target}?"))? {
        tracing::info!("Update cancelled");
        return Ok(());
    }

    perform_update(&validated, triple)?;
    tracing::info!("coop updated to {target}");
    persist_state(Some(validated.tag().as_str()));
    Ok(())
}

fn perform_update(release: &ValidatedRelease, triple: &str) -> Result<()> {
    let tmp = tempfile::tempdir().context("Failed to create temporary working directory")?;
    let tarball_name = release.tarball().name();

    let tarball_path = tmp.path().join(tarball_name);
    let sums_path = tmp.path().join("SHA256SUMS");

    tracing::info!("Downloading {tarball_name}");
    download_asset(release.tarball(), &tarball_path)?;
    download_asset(release.sums(), &sums_path)?;

    let sums_content = fs::read_to_string(&sums_path)
        .with_context(|| format!("Failed to read {}", sums_path.display()))?;
    let expected = parse_sha256sums(&sums_content, tarball_name)
        .with_context(|| format!("{tarball_name} not listed in SHA256SUMS"))?;
    verify_sha256(&tarball_path, &expected)?;

    let provenance = resolve_provenance(release, tmp.path());
    verify_attestation(&tarball_path, release.tag().as_str(), &provenance)?;

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

    let extract_dir = tmp.path().join(format!("coop-{}-{triple}", release.tag()));
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
    let source = match self_update_source() {
        Ok(source) => source,
        Err(error) => {
            tracing::debug!("background update source policy rejected the check: {error}");
            return;
        }
    };
    std::thread::spawn(move || {
        match fetch_self_release(&source, None)
            .and_then(|release| validate_release_tag(&release, None))
        {
            Ok(tag) => persist_state(Some(tag.as_str())),
            Err(e) => tracing::debug!("background update-check failed: {e}"),
        }
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
#[expect(clippy::panic, reason = "tests use panic! for unreachable arms")]
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
    fn official_source_is_fixed_and_rejects_the_legacy_override() {
        let source = UpdateSource::for_test_build_kind("release", false, None, false).unwrap();
        assert_eq!(source, UpdateSource::official());
        assert!(!source.is_fixture());
        assert_eq!(
            source.metadata_url(REPO, "latest").unwrap().as_str(),
            "https://api.github.com/repos/trailofbits/coop/releases/latest"
        );
        let error = UpdateSource::for_test_build_kind("release", true, None, false).unwrap_err();
        assert!(error.to_string().contains(LEGACY_API_OVERRIDE));
    }

    #[test]
    fn current_build_source_uses_the_compiled_build_kind() {
        let source =
            UpdateSource::for_current_build(false, Some("http://127.0.0.1:4321"), false).unwrap();
        if env!("COOP_BUILD_KIND") == "test" {
            assert!(source.is_fixture());
        } else {
            assert_eq!(source, UpdateSource::official());
            assert!(UpdateSource::for_current_build(true, None, false).is_err());
        }
    }

    #[test]
    fn test_source_is_explicit_and_loopback_only() {
        let source =
            UpdateSource::for_test_build_kind("test", false, Some("http://127.0.0.1:4321"), false)
                .unwrap();
        assert!(source.is_fixture());
        assert_eq!(
            source.metadata_url(REPO, "latest").unwrap().as_str(),
            "http://127.0.0.1:4321/repos/trailofbits/coop/releases/latest"
        );
        assert!(UpdateSource::for_test_build_kind("test", false, None, false).is_err());
        assert!(
            UpdateSource::for_test_build_kind("test", false, Some("https://example.com"), false)
                .is_err()
        );
        // A test-only runtime value cannot change an official build's policy.
        assert_eq!(
            UpdateSource::for_test_build_kind(
                "release",
                false,
                Some("http://127.0.0.1:4321"),
                true,
            )
            .unwrap(),
            UpdateSource::official()
        );
    }

    #[test]
    fn release_tag_validation_accepts_latest_and_matching_pinned_metadata() {
        let release = Release {
            tag: "v1.2.3".to_string(),
            assets: Vec::new(),
        };
        let requested = ReleaseTag::from_requested("1.2.3").unwrap();
        assert_eq!(requested.to_string(), "v1.2.3");
        assert_eq!(validate_release_tag(&release, None).unwrap(), requested);
        assert_eq!(
            validate_release_tag(&release, Some(&requested)).unwrap(),
            requested
        );
    }

    #[test]
    fn release_tag_validation_rejects_mismatch_and_ambiguous_latest_tags() {
        let requested = ReleaseTag::from_requested("1.2.3").unwrap();
        let mismatch = Release {
            tag: "v1.2.4".to_string(),
            assets: Vec::new(),
        };
        assert!(
            validate_release_tag(&mismatch, Some(&requested))
                .unwrap_err()
                .to_string()
                .contains("tag mismatch")
        );
        for tag in ["1.2.3", "latest", "v1.2.3/../other", " v1.2.3"] {
            let release = Release {
                tag: tag.to_string(),
                assets: Vec::new(),
            };
            assert!(
                validate_release_tag(&release, None).is_err(),
                "metadata tag {tag:?} must be rejected"
            );
        }
    }

    #[test]
    fn official_asset_identity_accepts_only_the_exact_release_url() {
        let source = UpdateSource::official();
        let tag = ReleaseTag::from_metadata("v1.2.3").unwrap();
        let name = asset_name(tag.as_str(), "x86_64-unknown-linux-musl");
        let expected = source.expected_asset_url(&tag, &name).unwrap().to_string();
        let valid = Asset {
            name: name.clone(),
            url: expected.clone(),
        };
        assert_eq!(
            validate_asset_identity(&source, &tag, &name, &valid)
                .unwrap()
                .url()
                .as_str(),
            expected
        );

        let invalid_urls = [
            expected.replacen("https://", "http://", 1),
            expected.replacen("github.com", "evil.example", 1),
            expected.replacen("trailofbits", "trailofbit", 1),
            expected.replacen("/coop/", "/coop-lookalike/", 1),
            expected.replacen("v1.2.3", "v1.2.4", 1),
            expected.replacen(&name, "coop-v1.2.3-lookalike.tar.gz", 1),
            expected.replacen("github.com", "user@github.com", 1),
            expected.replacen("github.com", "github.com:443", 1),
            format!("{expected}?download=1"),
            format!("{expected}#fragment"),
            expected.replacen("/releases/", "/releases%2f", 1),
            expected.replacen("/download/", "/other/../download/", 1),
            expected.replacen("/download/", "/download\\", 1),
        ];
        for url in invalid_urls {
            let asset = Asset {
                name: name.clone(),
                url,
            };
            assert!(
                validate_asset_identity(&source, &tag, &name, &asset).is_err(),
                "invalid asset URL was accepted: {}",
                asset.url
            );
        }

        let lookalike_name = Asset {
            name: format!("{name}.sig"),
            url: expected,
        };
        assert!(validate_asset_identity(&source, &tag, &name, &lookalike_name).is_err());
    }

    #[test]
    fn expected_assets_are_validated_independently_and_duplicates_fail_closed() {
        let source = UpdateSource::official();
        let mut release = raw_test_release(&source, "v9.9.9", true);
        validate_release_assets(&source, &release, None, "test-triple").unwrap();

        for expected_name in [
            asset_name("v9.9.9", "test-triple"),
            "SHA256SUMS".to_string(),
            BUNDLE_ASSET.to_string(),
        ] {
            let mut changed = release.clone();
            changed
                .assets
                .iter_mut()
                .find(|asset| asset.name == expected_name)
                .unwrap()
                .url
                .push_str("?wrong");
            assert!(
                validate_release_assets(&source, &changed, None, "test-triple").is_err(),
                "invalid {expected_name} identity was accepted"
            );
        }

        let exact_duplicate = release.assets[0].clone();
        release.assets.push(exact_duplicate);
        validate_release_assets(&source, &release, None, "test-triple").unwrap();
        let last = release.assets.last_mut().unwrap();
        last.url.push_str("?conflict");
        assert!(validate_release_assets(&source, &release, None, "test-triple").is_err());
    }

    #[test]
    fn production_curl_and_gh_commands_pin_protocol_origin_repo_tag_and_name() {
        let curl = curl_command(&UpdateSource::official()).build();
        let curl_args = curl
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(curl_args, ["--proto", "=https", "--proto-redir", "=https"]);
        assert!(!curl_args.iter().any(|arg| arg == "--location-trusted"));

        let gh_api = gh_api_command("repos/trailofbits/coop/releases/latest").build();
        let gh_api_args = gh_api
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            gh_api_args,
            [
                "api",
                "repos/trailofbits/coop/releases/latest",
                "--hostname",
                "github.com",
                "-H",
                "Accept: application/vnd.github+json",
            ]
        );

        let tag = ReleaseTag::from_metadata("v1.2.3").unwrap();
        let gh =
            gh_release_download_command(&tag, "SHA256SUMS", Path::new("/tmp/SHA256SUMS")).build();
        let gh_args = gh
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            gh_args,
            [
                "release",
                "download",
                "v1.2.3",
                "--repo",
                "github.com/trailofbits/coop",
                "--pattern",
                "SHA256SUMS",
                "--output",
                "/tmp/SHA256SUMS",
                "--clobber",
            ]
        );
    }

    #[test]
    fn generic_agent_metadata_uses_official_origin_without_asset_authority() {
        let url = UpdateSource::official()
            .metadata_url("openai/codex", "latest")
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.github.com/repos/openai/codex/releases/latest"
        );
        let self_tag = ReleaseTag::from_metadata("v1.2.3").unwrap();
        let self_asset = UpdateSource::official()
            .expected_asset_url(&self_tag, "SHA256SUMS")
            .unwrap();
        assert!(self_asset.as_str().contains("/trailofbits/coop/"));
        assert!(!self_asset.as_str().contains("/openai/codex/"));
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
    fn attestation_verify_args_pin_release_workflow_and_tag_without_bundle() {
        let args = attestation_verify_args(Path::new("/tmp/coop.tar.gz"), "v9.9.9", None);
        assert_eq!(
            args,
            [
                "attestation",
                "verify",
                "/tmp/coop.tar.gz",
                "--repo",
                REPO,
                "--cert-identity",
                "https://github.com/trailofbits/coop/.github/workflows/release.yml@refs/tags/v9.9.9",
                "--source-ref",
                "refs/tags/v9.9.9",
                "--deny-self-hosted-runners",
            ]
        );
    }

    #[test]
    fn attestation_verify_args_append_bundle_when_present() {
        let args = attestation_verify_args(
            Path::new("/tmp/coop.tar.gz"),
            "v1.2.3-rc.1",
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
                "--cert-identity",
                "https://github.com/trailofbits/coop/.github/workflows/release.yml@refs/tags/v1.2.3-rc.1",
                "--source-ref",
                "refs/tags/v1.2.3-rc.1",
                "--deny-self-hosted-runners",
                "--bundle",
                "/tmp/attestations.jsonl",
            ]
        );
    }

    fn raw_test_release(source: &UpdateSource, tag: &str, include_bundle: bool) -> Release {
        let tag = ReleaseTag::from_metadata(tag).unwrap();
        let tarball_name = asset_name(tag.as_str(), "test-triple");
        let mut names = vec![tarball_name, "SHA256SUMS".to_string()];
        if include_bundle {
            names.push(BUNDLE_ASSET.to_string());
        }
        let assets = names
            .into_iter()
            .map(|name| Asset {
                url: source.expected_asset_url(&tag, &name).unwrap().to_string(),
                name,
            })
            .collect();
        Release {
            tag: tag.as_str().to_string(),
            assets,
        }
    }

    fn validated_test_release(
        source: &UpdateSource,
        tag: &str,
        include_bundle: bool,
    ) -> ValidatedRelease {
        validate_release_assets(
            source,
            &raw_test_release(source, tag, include_bundle),
            None,
            "test-triple",
        )
        .unwrap()
    }

    /// The asset name is agreed across three files with no compiler link
    /// between them. A rename in one silently degrades `coop update` and
    /// `install.sh` back to the credential-requiring API path.
    ///
    /// Each assertion keys on a line that carries the *behavior*, not on the
    /// name alone: `install.sh` mentions `attestations.jsonl` literally only in
    /// its `BUNDLE=` declaration, so a `contains` over the whole file survives
    /// deletion of the download and the `--bundle` verify.
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
            installer
                .lines()
                .any(|l| l.contains(&format!("BUNDLE=\"{BUNDLE_ASSET}\""))),
            "install.sh no longer names {BUNDLE_ASSET} as the bundle asset"
        );
        // `download_bundle`, not `download_asset`: the latter prefers `gh` and
        // then a `GITHUB_TOKEN` bearer, which would re-attach the credential
        // this transport exists to avoid.
        assert!(
            installer
                .lines()
                .any(|l| l.contains("download_bundle \"${TMPDIR}/${BUNDLE}\"")),
            "install.sh no longer fetches the bundle asset with download_bundle"
        );
        let Some((bundle_fetch, _)) = installer
            .split_once("download_bundle() {")
            .and_then(|(_, rest)| rest.split_once("\n}"))
        else {
            panic!("install.sh no longer defines download_bundle");
        };
        // Keyed on the shape of the request, not on a list of credential
        // spellings: banning only `GITHUB_TOKEN` and `gh release` failed open
        // on `-H "Authorization: token $(gh auth token)"` and on `${GH_TOKEN}`,
        // either of which re-attaches the credential. A bare curl carries no
        // header and shells out to nothing.
        assert!(
            bundle_fetch.contains("curl -fsSL")
                && !bundle_fetch.contains("-H")
                && !bundle_fetch.contains("--header")
                && !bundle_fetch.contains("$(")
                && !bundle_fetch.contains("GITHUB_TOKEN")
                && !bundle_fetch.contains("gh release"),
            "the {BUNDLE_ASSET} fetch is no longer credential-free — it must be a bare curl, \
             or a SAML-restricted token can 403 on the one step --bundle exists to avoid"
        );
        // Keyed on the downloaded path, not the bare `--bundle` flag: the
        // `verify manually:` help text in the no-`gh` branch also names
        // `--bundle ${BUNDLE}`, so rewrapping those two `info` lines into one
        // would satisfy a bare-flag check on its own and let the real verify
        // lose `--bundle` unnoticed. Only the call site names the temp path.
        assert!(
            installer.contains(r#"--bundle "${TMPDIR}/${BUNDLE}""#),
            "install.sh no longer verifies against the downloaded bundle"
        );
    }

    #[test]
    fn bundle_decision_skips_when_verification_would_not_run() {
        let fixture =
            UpdateSource::for_test_build_kind("test", false, Some("http://127.0.0.1:1234"), false)
                .unwrap();
        let release = validated_test_release(&fixture, "v9.9.9", true);
        match bundle_decision(&release, true) {
            BundleDecision::TestMode => {}
            other => panic!("expected TestMode, got {other:?}"),
        }
        let official = validated_test_release(&UpdateSource::official(), "v9.9.9", true);
        match bundle_decision(&official, false) {
            BundleDecision::NoGh => {}
            other => panic!("expected NoGh, got {other:?}"),
        }
        // The fixture policy is checked first, so it reports TestMode rather
        // than NoGh when `gh` is also absent.
        match bundle_decision(&release, false) {
            BundleDecision::TestMode => {}
            other => panic!("expected TestMode, got {other:?}"),
        }
    }

    #[test]
    fn bundle_decision_fetches_only_when_the_release_publishes_the_asset() {
        let with_bundle = validated_test_release(&UpdateSource::official(), "v9.9.9", true);
        match bundle_decision(&with_bundle, true) {
            BundleDecision::Fetch(asset) => assert_eq!(asset.name(), BUNDLE_ASSET),
            other => panic!("expected Fetch, got {other:?}"),
        }
        let without_bundle = validated_test_release(&UpdateSource::official(), "v0.5.4", false);
        match bundle_decision(&without_bundle, true) {
            BundleDecision::NoAsset => {}
            other => panic!("expected NoAsset, got {other:?}"),
        }
    }

    #[test]
    fn only_the_bundle_variant_yields_a_bundle_path() {
        assert_eq!(
            Provenance::Bundle(PathBuf::from("/tmp/b.jsonl")).bundle(),
            Some(Path::new("/tmp/b.jsonl"))
        );
        assert_eq!(Provenance::TestMode.bundle(), None);
        assert_eq!(Provenance::NoGh.bundle(), None);
        assert_eq!(Provenance::Api(ApiReason::NoAsset).bundle(), None);
    }

    /// The failure message must name the reason that actually happened: a
    /// failed download previously reported "the release publishes no
    /// attestations.jsonl" on a release that publishes it.
    #[test]
    fn api_fallback_hint_names_the_reason_that_happened() {
        assert!(
            Provenance::Api(ApiReason::NoAsset)
                .api_fallback_hint()
                .contains("the release publishes no attestations.jsonl")
        );
        assert!(
            Provenance::Api(ApiReason::DownloadFailed)
                .api_fallback_hint()
                .contains("attestations.jsonl could not be downloaded")
        );
        assert!(
            Provenance::Api(ApiReason::EmptyBundle)
                .api_fallback_hint()
                .contains("the published attestations.jsonl is empty")
        );
    }

    #[test]
    fn api_fallback_hint_is_empty_when_the_api_was_not_used() {
        assert_eq!(
            Provenance::Bundle(PathBuf::from("/tmp/b.jsonl")).api_fallback_hint(),
            ""
        );
        assert_eq!(Provenance::TestMode.api_fallback_hint(), "");
        assert_eq!(Provenance::NoGh.api_fallback_hint(), "");
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
    fn select_auth_strategy_prefers_bare_for_fixture_source() {
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
