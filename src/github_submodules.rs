//! Depth-1 GitHub submodule discovery for the FGPAT wizard.
//!
//! When the parent repo's `.gitmodules` lists submodules on
//! `github.com`, a token scoped to a single repo cannot follow them.
//! The wizard fetches `.gitmodules` via the GitHub Contents API and
//! classifies submodule URLs into three buckets:
//!
//! - **Same resource owner.** A single FGPAT can cover the parent plus
//!   sibling repos under the same owner — the wizard re-prompts the
//!   user to include them in the form's repository selection.
//! - **Other resource owners.** One FGPAT per owner; the wizard prints
//!   a `coop github setup-pat --repo <slug>` line for each.
//! - **Non-GitHub URLs.** Warned about once and otherwise ignored.
//!
//! The HTTP call lives in [`crate::github_pat`]; the parsing and
//! classification live here so they can be exercised by unit tests
//! without a network round-trip.
//!
//! Only depth 1 is inspected. Submodules of submodules need a
//! follow-up `coop github setup-pat` invocation.

use std::collections::{BTreeMap, BTreeSet};

use crate::github_repo::{RepoSlug, parse_repo_slug_from_url};

/// Classified, dedup'd submodule repositories discovered at depth 1.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SubmoduleDiscovery {
    /// Slugs under the same resource owner as the parent (parent excluded).
    pub(crate) same_owner: Vec<RepoSlug>,
    /// Slugs under other resource owners, grouped by owner.
    pub(crate) cross_owner: BTreeMap<String, Vec<RepoSlug>>,
    /// Submodule URLs that resolve to something other than github.com.
    pub(crate) non_github_urls: Vec<String>,
}

impl SubmoduleDiscovery {
    pub(crate) fn is_empty(&self) -> bool {
        self.same_owner.is_empty() && self.cross_owner.is_empty() && self.non_github_urls.is_empty()
    }

    /// Drop slugs that `is_public` accepts from both actionable buckets.
    ///
    /// Public GitHub repos clone without authentication, so suggesting
    /// a PAT for them is noise. Returns the dropped slugs (sorted) so
    /// the wizard can announce them. Empty cross-owner groups left
    /// behind by the filter are pruned to keep the output tight.
    ///
    /// The predicate is passed in (rather than calling out to curl
    /// here) so the function is exercised by unit tests without a
    /// network round-trip.
    pub(crate) fn drop_public(
        &mut self,
        mut is_public: impl FnMut(&RepoSlug) -> bool,
    ) -> Vec<RepoSlug> {
        let mut dropped = Vec::new();
        self.same_owner.retain(|slug| {
            if is_public(slug) {
                dropped.push(slug.clone());
                false
            } else {
                true
            }
        });
        for slugs in self.cross_owner.values_mut() {
            slugs.retain(|slug| {
                if is_public(slug) {
                    dropped.push(slug.clone());
                    false
                } else {
                    true
                }
            });
        }
        self.cross_owner.retain(|_, v| !v.is_empty());
        dropped.sort();
        dropped
    }
}

/// Extract `url = …` values from a `.gitmodules` body.
///
/// Ignores blank lines, `#`/`;` comment lines, and `[submodule "…"]`
/// section headers. The gitconfig grammar allows optional double-quotes
/// around the value — those are stripped. A submodule entry without a
/// `url` is invalid per git itself, so we don't surface partial entries.
pub(crate) fn extract_urls(gitmodules: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in gitmodules.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("url") {
            continue;
        }
        let value = value.trim().trim_matches('"');
        if !value.is_empty() {
            out.push(value.to_string());
        }
    }
    out
}

/// Resolve a submodule URL relative to its parent repository.
///
/// Absolute URLs are returned unchanged. A `../` prefix walks up the
/// parent's HTTPS path (`/owner/repo`). The join is bounded: a walk
/// that overshoots the owner level, or whose remaining tail still
/// contains `../` after a sanity limit, is dropped with a debug note
/// rather than synthesised into a bogus slug.
fn resolve_url(parent: &RepoSlug, raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("./") {
        return Some(format!("https://github.com/{parent}/{rest}"));
    }
    if !raw.starts_with("../") {
        return Some(raw.to_string());
    }
    let mut up: usize = 0;
    let mut rest = raw;
    while let Some(after) = rest.strip_prefix("../") {
        up = up.checked_add(1)?;
        rest = after;
        if up > 8 {
            return None;
        }
    }
    let segments: [&str; 2] = [parent.owner(), parent.repo()];
    if up > segments.len() {
        return None;
    }
    let mut kept: Vec<&str> = segments[..segments.len() - up].to_vec();
    if !rest.is_empty() {
        kept.push(rest);
    }
    if kept.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{}", kept.join("/")))
}

/// Classify resolved submodule URLs against `parent`.
///
/// The parent slug itself is dropped (a submodule pointing at the
/// parent is meaningless for token coverage). Joined relative URLs
/// that don't end up matching `github.com/{owner}/{repo}` are dropped
/// with a debug note rather than minted into a bogus slug or routed
/// into the non-GitHub bucket. Same-owner and cross-owner buckets
/// are deterministically ordered via `BTreeSet` so the wizard's
/// output is stable across invocations.
pub(crate) fn classify_urls(parent: &RepoSlug, urls: &[String]) -> SubmoduleDiscovery {
    let mut same_owner: BTreeSet<RepoSlug> = BTreeSet::new();
    let mut cross_owner: BTreeMap<String, BTreeSet<RepoSlug>> = BTreeMap::new();
    let mut non_github: BTreeSet<String> = BTreeSet::new();

    for raw in urls {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let was_relative = is_relative(trimmed);
        let Some(resolved) = resolve_url(parent, trimmed) else {
            tracing::debug!("submodule url not resolvable, dropping: {raw}");
            continue;
        };
        match (parse_repo_slug_from_url(&resolved), was_relative) {
            (Some(slug), _) if &slug == parent => {}
            (Some(slug), _) if slug.owner() == parent.owner() => {
                same_owner.insert(slug);
            }
            (Some(slug), _) => {
                cross_owner
                    .entry(slug.owner().to_string())
                    .or_default()
                    .insert(slug);
            }
            (None, true) => {
                tracing::debug!(
                    "joined relative submodule url doesn't match github.com/{{owner}}/{{repo}}, \
                     dropping: {resolved} (from {raw})",
                );
            }
            (None, false) => {
                non_github.insert(resolved);
            }
        }
    }

    SubmoduleDiscovery {
        same_owner: same_owner.into_iter().collect(),
        cross_owner: cross_owner
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect(),
        non_github_urls: non_github.into_iter().collect(),
    }
}

fn is_relative(url: &str) -> bool {
    url.starts_with("./") || url.starts_with("../")
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::new(s).unwrap()
    }

    #[test]
    fn extract_urls_picks_url_lines() {
        let body = "\
[submodule \"a\"]
    path = vendor/a
    url = https://github.com/owner/a
[submodule \"b\"]
    path = vendor/b
    url = ../b
";
        assert_eq!(
            extract_urls(body),
            vec!["https://github.com/owner/a", "../b"]
        );
    }

    #[test]
    fn extract_urls_skips_comments_and_blank_lines() {
        let body = "\
# top-level comment
; another comment

[submodule \"a\"]
    url = https://github.com/o/a
";
        assert_eq!(extract_urls(body), vec!["https://github.com/o/a"]);
    }

    #[test]
    fn extract_urls_strips_surrounding_quotes() {
        let body = "    url = \"https://github.com/o/a\"\n";
        assert_eq!(extract_urls(body), vec!["https://github.com/o/a"]);
    }

    #[test]
    fn extract_urls_is_case_insensitive_on_key() {
        // gitconfig keys are case-insensitive; `URL = …` is legal.
        let body = "    URL = https://github.com/o/a\n";
        assert_eq!(extract_urls(body), vec!["https://github.com/o/a"]);
    }

    #[test]
    fn extract_urls_drops_empty_value() {
        let body = "    url =\n    url = \n";
        assert!(extract_urls(body).is_empty());
    }

    #[test]
    fn classify_groups_same_and_cross_owner() {
        let parent = slug("acme/parent");
        let urls = vec![
            "https://github.com/acme/lib-a".to_string(),
            "https://github.com/acme/lib-b".to_string(),
            "git@github.com:third-party/foo.git".to_string(),
            "https://github.com/other/bar".to_string(),
        ];
        let d = classify_urls(&parent, &urls);
        assert_eq!(d.same_owner, vec![slug("acme/lib-a"), slug("acme/lib-b")]);
        assert_eq!(
            d.cross_owner.get("other").map(Vec::as_slice),
            Some(&[slug("other/bar")][..]),
        );
        assert_eq!(
            d.cross_owner.get("third-party").map(Vec::as_slice),
            Some(&[slug("third-party/foo")][..]),
        );
        assert!(d.non_github_urls.is_empty());
    }

    #[test]
    fn classify_dedupes_repeated_urls() {
        let parent = slug("acme/parent");
        let urls = vec![
            "https://github.com/acme/lib-a".to_string(),
            "https://github.com/acme/lib-a.git".to_string(),
            "git@github.com:acme/lib-a.git".to_string(),
        ];
        let d = classify_urls(&parent, &urls);
        assert_eq!(d.same_owner, vec![slug("acme/lib-a")]);
    }

    #[test]
    fn classify_drops_self_reference() {
        let parent = slug("acme/parent");
        let urls = vec![
            "https://github.com/acme/parent.git".to_string(),
            "https://github.com/acme/sibling".to_string(),
        ];
        let d = classify_urls(&parent, &urls);
        assert_eq!(d.same_owner, vec![slug("acme/sibling")]);
    }

    #[test]
    fn classify_collects_non_github_urls() {
        let parent = slug("acme/parent");
        let urls = vec![
            "https://gitlab.com/acme/lib-a".to_string(),
            "https://internal.example.com/x/y".to_string(),
        ];
        let d = classify_urls(&parent, &urls);
        assert!(d.same_owner.is_empty());
        assert!(d.cross_owner.is_empty());
        assert_eq!(
            d.non_github_urls,
            vec![
                "https://gitlab.com/acme/lib-a",
                "https://internal.example.com/x/y",
            ]
        );
    }

    #[test]
    fn classify_resolves_relative_url_to_sibling_under_same_owner() {
        let parent = slug("acme/parent");
        let urls = vec!["../sibling".to_string()];
        let d = classify_urls(&parent, &urls);
        assert_eq!(d.same_owner, vec![slug("acme/sibling")]);
    }

    #[test]
    fn classify_resolves_relative_url_to_other_owner() {
        let parent = slug("acme/parent");
        let urls = vec!["../../other/repo".to_string()];
        let d = classify_urls(&parent, &urls);
        assert!(d.same_owner.is_empty());
        assert_eq!(
            d.cross_owner.get("other").map(Vec::as_slice),
            Some(&[slug("other/repo")][..]),
        );
    }

    #[test]
    fn classify_drops_overshoot_relative_url() {
        // `../../../too-far` walks past the owner level — the join is
        // bounded to keep us from minting a bogus slug.
        let parent = slug("acme/parent");
        let urls = vec!["../../../too-far".to_string()];
        let d = classify_urls(&parent, &urls);
        assert!(d.is_empty());
    }

    #[test]
    fn classify_drops_joined_url_that_isnt_owner_repo_shape() {
        // `./extra/foo` joins to `github.com/acme/parent/extra/foo` which
        // has too many path segments to be a repo slug. Per the bounded-
        // join rule, drop it silently rather than routing it to
        // `non_github_urls` (the host is still github.com).
        let parent = slug("acme/parent");
        let urls = vec!["./extra/foo".to_string()];
        let d = classify_urls(&parent, &urls);
        assert!(d.is_empty());
    }

    #[test]
    fn classify_drops_empty_url() {
        let parent = slug("acme/parent");
        let urls = vec![String::new()];
        let d = classify_urls(&parent, &urls);
        assert!(d.is_empty());
    }

    #[test]
    fn drop_public_filters_both_buckets_and_returns_sorted_list() {
        let parent = slug("acme/parent");
        let urls = vec![
            "https://github.com/acme/private-lib".to_string(),
            "https://github.com/acme/public-lib".to_string(),
            "https://github.com/other/public-thing".to_string(),
            "https://github.com/other/private-thing".to_string(),
        ];
        let mut d = classify_urls(&parent, &urls);
        let publics = [slug("acme/public-lib"), slug("other/public-thing")]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        let dropped = d.drop_public(|s| publics.contains(s));

        assert_eq!(
            dropped,
            vec![slug("acme/public-lib"), slug("other/public-thing")]
        );
        assert_eq!(d.same_owner, vec![slug("acme/private-lib")]);
        assert_eq!(d.cross_owner.len(), 1);
        assert_eq!(
            d.cross_owner.get("other").map(Vec::as_slice),
            Some(&[slug("other/private-thing")][..])
        );
    }

    #[test]
    fn drop_public_prunes_empty_cross_owner_groups() {
        let parent = slug("acme/parent");
        let urls = vec![
            "https://github.com/other/only-public".to_string(),
            "https://github.com/another/keep-private".to_string(),
        ];
        let mut d = classify_urls(&parent, &urls);
        let dropped = d.drop_public(|s| s.owner() == "other");

        assert_eq!(dropped, vec![slug("other/only-public")]);
        assert!(!d.cross_owner.contains_key("other"));
        assert!(d.cross_owner.contains_key("another"));
    }

    #[test]
    fn drop_public_no_matches_is_noop() {
        let parent = slug("acme/parent");
        let urls = vec!["https://github.com/acme/lib-a".to_string()];
        let mut d = classify_urls(&parent, &urls);
        let dropped = d.drop_public(|_| false);

        assert!(dropped.is_empty());
        assert_eq!(d.same_owner, vec![slug("acme/lib-a")]);
    }

    #[test]
    fn is_empty_reflects_all_three_buckets() {
        assert!(SubmoduleDiscovery::default().is_empty());
        let mut d = SubmoduleDiscovery::default();
        d.same_owner.push(slug("a/b"));
        assert!(!d.is_empty());
    }
}
