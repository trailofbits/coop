#![no_main]

//! Fuzz target for [`jsonc::jsonc_to_json`].
//!
//! The scanner is fed hand-authored `devcontainer.json` text — user-editable
//! input that crosses a trust boundary, which is the parser class #278
//! reserves a standing fuzz harness for. There is no correctness oracle for
//! arbitrary input, so the only property is **never panics**: libFuzzer
//! reports any panic, abort, hang, or OOM as a crash.
//!
//! `coop` is a binary-only crate (no lib target), so the function cannot be
//! imported as a dependency. The `jsonc` module is self-contained (std only),
//! so it is included by path. Its `#[cfg(test)]` block is inactive in a fuzz
//! build, so only production code compiles.

use libfuzzer_sys::fuzz_target;

#[path = "../../src/jsonc.rs"]
mod jsonc;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = jsonc::jsonc_to_json(input);
    }
});
