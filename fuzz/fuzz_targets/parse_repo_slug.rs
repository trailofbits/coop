#![no_main]

//! Fuzz target for [`coop::github_repo::parse_repo_slug_from_url`].
//!
//! That parser is fed `git remote get-url` output and CLI `--git-repo`
//! arguments — user-editable text that crosses a trust boundary, which is the
//! parser #278 reserves a standing fuzz harness for. There is no oracle for
//! correctness here, so the only property is **never panics**: libFuzzer
//! reports any panic, abort, hang, or OOM as a crash.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = coop::github_repo::parse_repo_slug_from_url(input);
    }
});
