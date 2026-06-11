#![no_main]

//! Fuzz target for [`coop::jsonc::jsonc_to_json`].
//!
//! The scanner is fed hand-authored `devcontainer.json` text — user-editable
//! input that crosses a trust boundary, which is the parser class #278
//! reserves a standing fuzz harness for. There is no correctness oracle for
//! arbitrary input, so the only property is **never panics**: libFuzzer
//! reports any panic, abort, hang, or OOM as a crash.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = coop::jsonc::jsonc_to_json(input);
    }
});
