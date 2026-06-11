#![no_main]

//! Fuzz target for [`github_repo::parse_repo_slug_from_url`].
//!
//! That parser is fed `git remote get-url` output and CLI `--git-repo`
//! arguments — user-editable text that crosses a trust boundary, which is the
//! parser #278 reserves a standing fuzz harness for. There is no oracle for
//! correctness here, so the only property is **never panics**: libFuzzer
//! reports any panic, abort, hang, or OOM as a crash.
//!
//! `coop` is a binary-only crate (no lib target), so the function cannot be
//! imported as a dependency. The parser module and its single in-crate
//! dependency (`naming`) are included by path instead. Their `#[cfg(test)]`
//! blocks are inactive in a fuzz build, so only production code compiles.

use libfuzzer_sys::fuzz_target;

// Most of each included module (git invocation, the `GitRepoUrl` newtype, the
// serde impls) is unused from this target — only the URL parser is exercised.
#[allow(dead_code)]
#[path = "../../src/github_repo.rs"]
mod github_repo;
#[allow(dead_code)]
#[path = "../../src/naming.rs"]
mod naming;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = github_repo::parse_repo_slug_from_url(input);
    }
});
