//! `coop github` subcommand implementations: the fine-grained PAT wizard
//! and helpers for `rotate-pat`, `status`, `forget-pat`.
//!
//! The wizard is intentionally a thin orchestration layer over the secret
//! store and the validation helpers. Anything that talks to GitHub goes
//! through `curl` so we don't pick up a new HTTP-client dependency.

#![expect(
    clippy::print_stderr,
    reason = "wizard is interactive CLI — stderr is user communication"
)]
#![expect(
    clippy::print_stdout,
    reason = "wizard is interactive CLI — stdout is user communication"
)]

use std::fs;
use std::io::{BufRead as _, Write as _};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cmd::Cmd;
use crate::config::{CoopConfig, GitHubAuth, resolve_cmd_value};
use crate::github_repo::{RepoSlug, detect_workspace_repo, parse_repo_slug_from_url};
use crate::github_submodules::{SubmoduleDiscovery, classify_urls, extract_urls};
use crate::secret_store::{
    SERVICE, account_for_repo, available_backends, delete_secret, infer_backend, store_secret,
};

/// Prefix every fine-grained PAT starts with. Surfaced as `pub const` so
/// `backend.rs` and `main.rs` can share the same literal for validation.
pub const TOKEN_PREFIX: &str = "github_pat_";

const PAT_NEW_URL: &str = "https://github.com/settings/personal-access-tokens/new";

/// Options resolved by clap for `coop github setup-pat`.
pub struct SetupOpts<'a> {
    pub repo: Option<&'a str>,
    pub config_path: &'a Path,
}

/// Entry point: run the wizard end-to-end.
///
/// On success, the on-disk config has a fresh `[github.pat."owner/repo"]`
/// entry and any matching skip-marker is removed.
pub fn run_setup_pat(cfg: &CoopConfig, opts: &SetupOpts<'_>) -> Result<()> {
    let repo = resolve_target_repo(opts.repo, None)?;

    print_form_instructions(&repo);
    open_browser_best_effort(PAT_NEW_URL);

    let token = read_token_no_echo()?;
    warn_token_format(&token);
    eprintln!("\nValidating token against api.github.com…");
    probe_user(&token)?;
    probe_repo(&token, &repo)?;
    let token = handle_submodules(token, &repo)?;

    let backend = pick_backend()?;
    let account = account_for_repo(&repo);
    let state_dir = cfg.data_dir.join("state");
    fs::create_dir_all(&state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;

    let cmd_str = store_secret(backend, SERVICE, &account, &token, &state_dir)
        .context("Failed to store secret in chosen backend")?;

    upsert_pat_entry(opts.config_path, &repo, &cmd_str)?;

    eprintln!(
        "\nWrote {}:\n  github.mode = \"pat\"\n  [github.pat.\"{repo}\"]\n  token = \"{}\"",
        opts.config_path.display(),
        cmd_str,
    );
    eprintln!("\nDone. `coop start --git-repo https://github.com/{repo}` will use this token.");
    Ok(())
}

/// `coop github rotate-pat` — same wizard, but messaging hints that
/// we're replacing an existing entry.
pub fn run_rotate_pat(cfg: &CoopConfig, opts: &SetupOpts<'_>) -> Result<()> {
    let repo = resolve_target_repo(opts.repo, None)?;
    if cfg
        .github
        .as_ref()
        .and_then(|g| g.pat_entry(&repo))
        .is_none()
    {
        eprintln!(
            "note: no existing PAT entry for '{repo}' — proceeding as if this were a fresh setup."
        );
    }
    run_setup_pat(cfg, opts)
}

/// `coop github status` — print one line per pat entry.
///
/// Output is plain text, sorted by repo name. The token value is never
/// printed. When `probe` is true, each `cmd:` invocation is resolved to
/// confirm the secret store still serves it — this may trigger a
/// Keychain dialog / `op` Touch-ID prompt per entry, so it is opt-in.
pub fn run_status(cfg: &CoopConfig, probe: bool) {
    let pat = match cfg.github.as_ref() {
        Some(GitHubAuth::Pat(c)) => c,
        Some(other) => {
            println!("github mode: {} (no PAT entries)", other.mode_name());
            return;
        }
        None => {
            println!("github mode: off (no PAT entries)");
            return;
        }
    };

    if pat.entries.is_empty() && pat.skip.is_empty() {
        println!("github mode: pat (no entries)");
        return;
    }

    println!("github mode: pat");
    println!("entries ({}):", pat.entries.len());
    for (repo, entry) in &pat.entries {
        let backend_label = infer_backend(entry.token.expose())
            .map_or_else(|| "unknown".to_string(), |b| b.label().to_string());
        println!("  {repo}");
        println!("    storage: {backend_label}");
        if probe {
            let validity = match resolve_cmd_value(entry.token.expose()) {
                Ok(token) if token.starts_with(TOKEN_PREFIX) => "resolves, format ok",
                Ok(_) => "resolves but unexpected format",
                Err(_) => "FAILED to resolve",
            };
            println!("    status:  {validity}");
        }
    }
    if !pat.skip.is_empty() {
        println!("skip ({}):", pat.skip.len());
        for repo in &pat.skip {
            println!("  {repo}");
        }
    }
}

/// `coop github forget-pat --repo owner/name` — remove the secret and
/// the config entry. Does **not** add a skip marker.
pub fn run_forget_pat(cfg: &CoopConfig, repo: &RepoSlug, config_path: &Path) -> Result<()> {
    let entry = cfg
        .github
        .as_ref()
        .and_then(|g| g.pat_entry(repo))
        .with_context(|| {
            format!("No PAT entry for '{repo}' — nothing to forget. Run `coop github status`.")
        })?;
    let backend = infer_backend(entry.token.expose());
    let account = account_for_repo(repo);
    let state_dir = cfg.data_dir.join("state");
    if let Some(b) = backend {
        if let Err(e) = delete_secret(b, SERVICE, &account, &state_dir) {
            tracing::warn!("Failed to remove secret from backend (continuing): {e}");
        }
    } else {
        tracing::info!(
            "Storage backend not recognised from cmd: invocation — config entry removed; \
             clean up the underlying secret manually if needed."
        );
    }
    remove_pat_entry(config_path, repo)?;
    eprintln!("Removed PAT entry for {repo}.");
    eprintln!(
        "note: the token itself may still be live on GitHub — \
         revoke it under https://github.com/settings/personal-access-tokens \
         if you want to fully invalidate it."
    );
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────

/// Resolve the target repo from explicit `--repo`, fall back to a
/// best-effort scan of the current working directory.
fn resolve_target_repo(repo_arg: Option<&str>, git_repo_url: Option<&str>) -> Result<RepoSlug> {
    if let Some(repo) = repo_arg {
        return RepoSlug::new(repo.trim());
    }
    if let Some(url) = git_repo_url
        && let Some(slug) = parse_repo_slug_from_url(url)
    {
        return Ok(slug);
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(slug) = detect_workspace_repo(&cwd)?
    {
        return Ok(slug);
    }
    bail!(
        "Could not detect a target repo. Pass --repo owner/name, or run from a \
         git working tree whose origin points at github.com."
    )
}

fn print_form_instructions(repo: &RepoSlug) {
    eprintln!("\nConfigure the form at {PAT_NEW_URL}:");
    eprintln!("  Token name:        coop-{}-{}", repo.owner(), repo.repo());
    eprintln!("  Expiration:        (your choice — 90 days is a reasonable default)");
    eprintln!("  Resource owner:    {}", repo.owner());
    eprintln!("  Repository access: Only select repositories → {repo}");
    eprintln!("  Repository permissions:");
    eprintln!("    Contents:        Read and write");
    eprintln!("    Pull requests:   Read and write");
    eprintln!("    Issues:          Read and write");
    eprintln!("    Commit statuses: Read-only");
    eprintln!("    Metadata:        Read-only (auto-included)\n");
    eprintln!("Click \"Generate token\", then paste it below.");
}

fn open_browser_best_effort(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        // xdg-open on Linux; Windows uses `start` but coop is unix-only.
        "xdg-open"
    };
    let _ = Command::new(opener).arg(url).spawn();
}

fn read_token_no_echo() -> Result<String> {
    eprint!("Paste token: ");
    std::io::stderr().flush().ok();
    let guard = EchoGuard::off();
    let stdin = std::io::stdin();
    let mut buf = String::new();
    stdin
        .lock()
        .read_line(&mut buf)
        .context("Failed to read token from stdin")?;
    // Print a newline because the user's RET wasn't echoed.
    if guard.is_active() {
        eprintln!();
    }
    drop(guard);
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        bail!("No token entered");
    }
    Ok(trimmed)
}

/// RAII guard that turns terminal echo off via `stty -echo` and restores
/// it on drop. Disabling fails silently if no controlling TTY exists or
/// `stty` is unavailable (`is_active` then returns false, so the caller
/// can skip the trailing newline).
///
/// `Drop` runs on normal scope exit but **not** when the process is killed
/// by an uncaught signal (Ctrl-C / SIGTERM). Users who interrupt the
/// wizard mid-paste can restore echo with `stty echo`. A `tcsetattr`-based
/// implementation would let the kernel handle restoration via terminal
/// reset, but the shell-out keeps the dependency footprint minimal.
struct EchoGuard {
    active: bool,
}

impl EchoGuard {
    fn off() -> Self {
        let active = Command::new("stty")
            .arg("-echo")
            .status()
            .is_ok_and(|s| s.success());
        Self { active }
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = Command::new("stty").arg("echo").status();
        }
    }
}

fn warn_token_format(token: &str) {
    if !token.starts_with(TOKEN_PREFIX) {
        // Don't fail — bring-your-own tokens (e.g. classic PATs starting
        // with `ghp_`) are legitimate but won't get the FGPAT guarantees.
        eprintln!(
            "warning: token does not start with '{TOKEN_PREFIX}' — \
             not a fine-grained PAT. Proceeding anyway."
        );
    }
}

fn probe_user(token: &str) -> Result<()> {
    let (status, body) = curl_with_token(token, "https://api.github.com/user")?;
    if status != 200 {
        bail!("GET /user returned HTTP {status} (token may be invalid or revoked)");
    }
    if !body.contains("\"login\"") {
        bail!("Token authentication failed: /user returned unexpected response");
    }
    eprintln!("  ✓ /user");
    Ok(())
}

fn probe_repo(token: &str, repo: &RepoSlug) -> Result<()> {
    let url = format!("https://api.github.com/repos/{repo}");
    let (status, _body) = curl_with_token(token, &url)?;
    match status {
        200 => {
            eprintln!("  ✓ /repos/{repo}");
            Ok(())
        }
        403 => bail!(
            "GET /repos/{repo} returned 403. This is often the org's \
             fine-grained PAT policy: ask an org admin to approve \
             fine-grained PATs, or to approve your specific token."
        ),
        404 => bail!(
            "GET /repos/{repo} returned 404. Check the repo slug and \
             that the PAT was generated with the right resource owner."
        ),
        other => bail!("GET /repos/{repo} returned HTTP {other}"),
    }
}

/// Issue a GET with the supplied token via curl, returning `(http_status, body)`.
///
/// Uses `-w "\n%{http_code}"` to coax the status code into stdout so we
/// can map it without parsing curl's stderr. `-f` is **not** set: we want
/// to see the response body even on 4xx for diagnostics, and the status
/// alone is enough to drive the wizard branches.
fn curl_with_token(token: &str, url: &str) -> Result<(u16, String)> {
    curl_with_accept(token, url, "application/vnd.github+json")
}

fn curl_with_accept(token: &str, url: &str, accept: &str) -> Result<(u16, String)> {
    let out = Cmd::new("curl")
        .arg("-sSL")
        .arg("-H")
        .arg(format!("Accept: {accept}"))
        .arg("-H")
        .arg("@-")
        .stdin_input(format!("Authorization: token {token}\n"))
        .arg("-w")
        .arg("\n%{http_code}")
        .arg(url)
        .capture()
        .with_context(|| format!("curl GET {url} failed"))?;
    let (body, status_str) = out.rsplit_once('\n').unwrap_or(("", out.trim()));
    let status: u16 = status_str
        .trim()
        .parse()
        .with_context(|| format!("Failed to parse HTTP status from curl output: '{status_str}'"))?;
    Ok((status, body.to_string()))
}

/// Fetch `.gitmodules` from the parent's default branch and classify the URLs.
///
/// Returns an empty discovery for `404` (the common "no submodules" case)
/// and for any other non-`200` status, logging a warning. We deliberately
/// don't fail the wizard on transient or permission errors here: the user
/// already authenticated to the parent repo, and a problem with the
/// contents endpoint shouldn't block writing a valid PAT entry. Worst
/// case, submodule clones will still surface a clear failure at clone
/// time — the wizard's job is purely advisory.
fn discover_submodule_repos(token: &str, parent: &RepoSlug) -> Result<SubmoduleDiscovery> {
    let url = format!("https://api.github.com/repos/{parent}/contents/.gitmodules");
    let (status, body) = curl_with_accept(token, &url, "application/vnd.github.raw+json")?;
    match status {
        200 => Ok(classify_urls(parent, &extract_urls(&body))),
        404 => Ok(SubmoduleDiscovery::default()),
        other => {
            eprintln!(
                "warning: GET /repos/{parent}/contents/.gitmodules returned HTTP {other} — \
                 skipping submodule check."
            );
            Ok(SubmoduleDiscovery::default())
        }
    }
}

/// Probe each `expected` slug with `token`. Returns those that the token
/// could not access (HTTP 403/404). Transient errors (non-2xx, non-4xx)
/// bubble up so the wizard can report them.
fn probe_same_owner_coverage(token: &str, expected: &[RepoSlug]) -> Result<Vec<RepoSlug>> {
    let mut failed = Vec::new();
    for slug in expected {
        let url = format!("https://api.github.com/repos/{slug}");
        let (status, _body) = curl_with_token(token, &url)?;
        match status {
            200 => {}
            403 | 404 => failed.push(slug.clone()),
            other => bail!("GET /repos/{slug} returned HTTP {other}"),
        }
    }
    Ok(failed)
}

/// Surface the discovery to the user and, when same-owner submodules
/// exist, loop until the pasted token covers them all.
///
/// Returns the (possibly re-pasted) token. The caller stores whatever
/// token this returns — by construction it is the latest one that
/// passed `probe_user` + `probe_repo` for `parent`, so the broader
/// scope replaces the originally pasted token.
fn handle_submodules(token: String, parent: &RepoSlug) -> Result<String> {
    let discovery = discover_submodule_repos(&token, parent)?;

    if discovery.is_empty() {
        return Ok(token);
    }

    if !discovery.non_github_urls.is_empty() {
        eprintln!("\nIgnoring non-GitHub submodule URLs (this token can't cover them):");
        for url in &discovery.non_github_urls {
            eprintln!("  - {url}");
        }
    }

    let mut token = token;
    if !discovery.same_owner.is_empty() {
        loop {
            let failed = probe_same_owner_coverage(&token, &discovery.same_owner)?;
            if failed.is_empty() {
                eprintln!("  ✓ all same-owner submodule repos covered");
                break;
            }
            print_widen_instructions(parent, &discovery.same_owner, &failed);
            token = read_token_no_echo()?;
            warn_token_format(&token);
            eprintln!("\nValidating widened token against api.github.com…");
            probe_user(&token)?;
            probe_repo(&token, parent)?;
        }
    }

    if !discovery.cross_owner.is_empty() {
        eprintln!(
            "\nNote: submodules under other resource owners need their own \
             FGPAT (one form per owner). Run:"
        );
        for (owner, repos) in &discovery.cross_owner {
            eprintln!("\n  Resource owner: {owner}");
            for slug in repos {
                eprintln!("    coop github setup-pat --repo {slug}");
            }
        }
    }

    eprintln!(
        "\nNote: only depth-1 submodules were checked. Nested submodules \
         (submodules of submodules) need their own `setup-pat` runs."
    );

    Ok(token)
}

fn print_widen_instructions(parent: &RepoSlug, expected: &[RepoSlug], failed: &[RepoSlug]) {
    eprintln!(
        "\n{} submodule repo(s) under '{}' are not covered by this token:",
        failed.len(),
        parent.owner(),
    );
    for slug in failed {
        eprintln!("  - {slug}");
    }
    eprintln!(
        "\nRe-open {PAT_NEW_URL} (or edit the existing token), keep \
         Resource owner '{}', and under 'Only select repositories' \
         include all of:",
        parent.owner(),
    );
    eprintln!("  - {parent}");
    for slug in expected {
        eprintln!("  - {slug}");
    }
    eprintln!("Generate a fresh token, then paste it below. (Empty input aborts.)");
}

fn pick_backend() -> Result<crate::secret_store::Backend> {
    let backends = available_backends();
    eprintln!("\nWhere should I store this token?");
    for (i, b) in backends.iter().enumerate() {
        eprintln!("  {}) {}", i + 1, b.label());
    }
    eprint!("> ");
    std::io::stderr().flush().ok();
    let mut buf = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buf)
        .context("Failed to read backend choice")?;
    let n: usize = buf
        .trim()
        .parse()
        .context("Backend choice must be a number")?;
    if n == 0 || n > backends.len() {
        bail!(
            "Backend choice out of range (got {n}, expected 1..={})",
            backends.len()
        );
    }
    Ok(backends[n - 1])
}

// ── Config-file mutation ───────────────────────────────────────

/// Read the config TOML at `path`, add/replace the entry for `repo`, and
/// write it back. Creates a fresh file if it doesn't exist.
///
/// Uses `toml::Value` for round-trip-safe edits: comments are lost, but
/// every other key/value is preserved. The wizard is the only place we
/// rewrite config, so the trade-off is acceptable.
///
/// Holds an exclusive `flock` on a sibling `.lock` file across the
/// read-modify-write window so concurrent wizard invocations (e.g. two
/// `coop start` auto-prompts firing at once) don't lose each other's
/// writes.
fn upsert_pat_entry(path: &Path, repo: &RepoSlug, token_cmd: &str) -> Result<()> {
    let _lock = crate::fs_util::lock_sibling(path)?;
    let original = read_or_init_doc(path)?;
    let mut doc = original.clone();

    // Ensure the `github` table exists.
    let github = doc
        .as_table_mut()
        .context("Config root is not a table")?
        .entry("github".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));

    // If `github` was previously a string (`github = "off"` etc.), upgrade
    // to a table form.
    if !github.is_table() {
        *github = toml::Value::Table(toml::Table::new());
    }
    let github_tbl = github
        .as_table_mut()
        .context("github value is not a table")?;
    github_tbl.insert("mode".to_string(), toml::Value::String("pat".to_string()));

    // Ensure `[github.pat]` is a table.
    let pat = github_tbl
        .entry("pat".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !pat.is_table() {
        *pat = toml::Value::Table(toml::Table::new());
    }
    let pat_tbl = pat.as_table_mut().context("github.pat is not a table")?;

    // Replace the entry for `repo`.
    let mut entry = toml::Table::new();
    entry.insert(
        "token".to_string(),
        toml::Value::String(token_cmd.to_string()),
    );
    pat_tbl.insert(repo.as_str().to_string(), toml::Value::Table(entry));

    // Drop the repo from the skip list, if present.
    if let Some(skip) = github_tbl.get_mut("skip")
        && let Some(arr) = skip.as_array_mut()
    {
        arr.retain(|v| v.as_str() != Some(repo.as_str()));
    }

    // Skip the rewrite when the parsed result equals what's already on disk.
    // `rotate-pat` routinely hits this: the `cmd:` invocation produced by
    // `store_secret` is deterministic in `(service, account)` for every
    // backend we support, so re-storing the same repo's token through the
    // same backend yields an entry value identical to the existing one.
    // Without this guard each rotation would rewrite the file (bumping mtime
    // and stripping any user-added comments via the `toml::Value` round-trip)
    // for no observable change in coop's behaviour.
    if doc == original {
        return Ok(());
    }
    write_doc(path, &doc)
}

fn remove_pat_entry(path: &Path, repo: &RepoSlug) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _lock = crate::fs_util::lock_sibling(path)?;
    let mut doc = read_or_init_doc(path)?;
    if let Some(github_tbl) = doc.get_mut("github").and_then(toml::Value::as_table_mut)
        && let Some(pat_tbl) = github_tbl
            .get_mut("pat")
            .and_then(toml::Value::as_table_mut)
    {
        pat_tbl.remove(repo.as_str());
    }
    write_doc(path, &doc)
}

/// Mark `repo` in `[github] skip`. Used by the auto-prompt's `never` answer.
///
/// Always forces `mode = "pat"`. Without this, a skip marker recorded
/// against `github = "off"` (or unset) would be silently dropped on the
/// next config load: the deserializer only reads the `skip` field for
/// pat-mode. The prompt itself is gated to only fire when the mode is
/// already "off" / unset / "pat" with no entry, so promoting to "pat"
/// (with no entries) here is a no-op for token forwarding — pat-mode
/// with no entries forwards no token, same as "off" — while making
/// per-repo skip state survive a config reload.
pub fn add_skip_marker(path: &Path, repo: &RepoSlug) -> Result<()> {
    let _lock = crate::fs_util::lock_sibling(path)?;
    let mut doc = read_or_init_doc(path)?;
    let github = doc
        .as_table_mut()
        .context("Config root is not a table")?
        .entry("github".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !github.is_table() {
        *github = toml::Value::Table(toml::Table::new());
    }
    let github_tbl = github
        .as_table_mut()
        .context("github value is not a table")?;
    github_tbl.insert("mode".to_string(), toml::Value::String("pat".to_string()));
    let skip = github_tbl
        .entry("skip".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if !skip.is_array() {
        *skip = toml::Value::Array(Vec::new());
    }
    let arr = skip.as_array_mut().context("github.skip is not an array")?;
    if !arr.iter().any(|v| v.as_str() == Some(repo.as_str())) {
        arr.push(toml::Value::String(repo.as_str().to_string()));
    }
    write_doc(path, &doc)
}

fn read_or_init_doc(path: &Path) -> Result<toml::Value> {
    if path.exists() {
        let s = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        toml::from_str::<toml::Value>(&s)
            .with_context(|| format!("Failed to parse {}", path.display()))
    } else {
        Ok(toml::Value::Table(toml::Table::new()))
    }
}

fn write_doc(path: &Path, doc: &toml::Value) -> Result<()> {
    let serialized = toml::to_string_pretty(doc)
        .with_context(|| format!("Failed to serialize {}", path.display()))?;
    // Pick mode 0600 if any pat token is stored as a literal (no `cmd:`
    // prefix). When a token is behind `cmd:`, only the invocation is on
    // disk and 0644 matches the existing convention. Literal tokens are
    // a foot-gun coop accepts for BYO setups — 0600 is the minimum.
    let mode = if doc_contains_literal_token(doc) {
        0o600
    } else {
        0o644
    };
    crate::fs_util::atomic_write_with_mode(path, &serialized, mode)
}

fn doc_contains_literal_token(doc: &toml::Value) -> bool {
    let Some(github_tbl) = doc.get("github").and_then(toml::Value::as_table) else {
        return false;
    };
    let Some(pat_tbl) = github_tbl.get("pat").and_then(toml::Value::as_table) else {
        return false;
    };
    pat_tbl.values().any(|entry| {
        entry
            .as_table()
            .and_then(|t| t.get("token"))
            .and_then(toml::Value::as_str)
            .is_some_and(|t| !t.starts_with("cmd:"))
    })
}

/// Hint helper used by `coop start` and other code paths to format the
/// "no entry for this repo" error consistently.
pub fn missing_entry_error(repo: &RepoSlug) -> String {
    format!(
        "No GitHub PAT configured for '{repo}'. \
         Run `coop github setup-pat --repo {repo}` to add one, \
         or set `github = \"off\"` to disable token forwarding."
    )
}

/// Test helper: read a pat config out of a file path.
#[cfg(test)]
pub(crate) fn pat_config_at(path: &Path) -> Result<Option<crate::config::PatConfig>> {
    let cfg = CoopConfig::load(path)?;
    Ok(cfg.github.and_then(|g| match g {
        GitHubAuth::Pat(c) => Some(c),
        _ => None,
    }))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::new(s).unwrap()
    }

    #[test]
    fn upsert_creates_new_file_with_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let s = slug("trailofbits/coop");
        upsert_pat_entry(&path, &s, "cmd:cat ./t.txt").unwrap();
        let cfg = pat_config_at(&path).unwrap().unwrap();
        assert_eq!(cfg.entries.len(), 1);
        let entry = cfg.entries.get(&s).unwrap();
        assert_eq!(entry.token.expose(), "cmd:cat ./t.txt");
    }

    #[test]
    fn upsert_preserves_other_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "ssh_port = 2222\n\
             [vm]\n\
             vcpu_count = 4\n",
        )
        .unwrap();
        upsert_pat_entry(&path, &slug("a/b"), "cmd:echo x").unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("ssh_port = 2222"));
        assert!(s.contains("vcpu_count = 4"));
        assert!(s.contains("[github.pat"));
    }

    #[test]
    fn upsert_upgrades_string_github_to_table() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "github = \"off\"\n").unwrap();
        upsert_pat_entry(&path, &slug("a/b"), "cmd:echo x").unwrap();
        let cfg = CoopConfig::load(&path).unwrap();
        assert!(matches!(cfg.github, Some(GitHubAuth::Pat(_))));
    }

    #[test]
    fn upsert_is_noop_when_entry_unchanged() {
        // Re-upserting the same (repo, cmd) into an already-pat config must
        // not rewrite the file. Verified by leaving a comment in the input
        // — `toml::Value` strips comments on round-trip, so a rewrite would
        // be visible as comment loss.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "# user comment\n[github]\nmode = \"pat\"\n\n[github.pat.\"a/b\"]\ntoken = \"cmd:echo x\"\n";
        std::fs::write(&path, original).unwrap();
        upsert_pat_entry(&path, &slug("a/b"), "cmd:echo x").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after, original,
            "no-op upsert must not rewrite the file (lost comments / formatting)"
        );
    }

    #[test]
    fn upsert_removes_repo_from_skip_list() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[github]\nmode = \"pat\"\nskip = [\"a/b\", \"c/d\"]\n",
        )
        .unwrap();
        upsert_pat_entry(&path, &slug("a/b"), "cmd:echo x").unwrap();
        let cfg = pat_config_at(&path).unwrap().unwrap();
        assert_eq!(cfg.skip, vec![slug("c/d")]);
    }

    #[test]
    fn remove_pat_entry_is_idempotent_on_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        remove_pat_entry(&path, &slug("x/y")).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remove_pat_entry_drops_only_named_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[github]\nmode = \"pat\"\n\n[github.pat.\"a/b\"]\ntoken = \"cmd:echo x\"\n\n[github.pat.\"c/d\"]\ntoken = \"cmd:echo y\"\n",
        )
        .unwrap();
        remove_pat_entry(&path, &slug("a/b")).unwrap();
        let cfg = pat_config_at(&path).unwrap().unwrap();
        assert!(!cfg.entries.contains_key(&slug("a/b")));
        assert!(cfg.entries.contains_key(&slug("c/d")));
    }

    #[test]
    fn skip_marker_added_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        add_skip_marker(&path, &slug("a/b")).unwrap();
        add_skip_marker(&path, &slug("a/b")).unwrap();
        let cfg = pat_config_at(&path).unwrap().unwrap();
        assert_eq!(cfg.skip, vec![slug("a/b")]);
    }

    #[test]
    fn skip_marker_survives_when_starting_mode_is_off() {
        // Regression: previously the marker was written under
        // `[github] mode = "off"`, and the deserializer dropped it.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "github = \"off\"\n").unwrap();
        add_skip_marker(&path, &slug("a/b")).unwrap();
        let cfg = pat_config_at(&path).unwrap().unwrap();
        assert_eq!(cfg.skip, vec![slug("a/b")]);
    }

    #[test]
    fn upsert_removes_skip_marker_after_starting_from_off() {
        // Regression: full sequence — start at "off", record skip, then
        // run setup-pat. The skip marker must be cleared, and the new
        // entry must be present.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "github = \"off\"\n").unwrap();
        add_skip_marker(&path, &slug("a/b")).unwrap();
        upsert_pat_entry(&path, &slug("a/b"), "cmd:echo x").unwrap();
        let cfg = pat_config_at(&path).unwrap().unwrap();
        assert!(!cfg.skip.contains(&slug("a/b")));
        assert!(cfg.entries.contains_key(&slug("a/b")));
    }

    #[test]
    fn missing_entry_error_mentions_repo_and_command() {
        let msg = missing_entry_error(&slug("trailofbits/coop"));
        assert!(msg.contains("trailofbits/coop"));
        assert!(msg.contains("setup-pat"));
    }
}
