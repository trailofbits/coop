#![no_main]

//! Fuzz target for the config loader: `toml::from_str` into
//! [`coop::config::CoopConfig`] followed by [`CoopConfig::validate`].
//!
//! `config.toml` is user-editable input that crosses a trust boundary. The
//! deserialize path runs the custom `Deserialize`/`visit_map` impls
//! (`SubnetMask`, `HostInterface`, `PortForward`, …) and `validate` then
//! checks the assembled config. There is no correctness oracle for arbitrary
//! input, so the only property is **never panics, only returns `Err`**:
//! libFuzzer reports any panic, abort, hang, or OOM as a crash.
//!
//! This deliberately exercises `from_str` + `validate` rather than
//! `CoopConfig::load`, which touches the filesystem (tilde expansion, reading
//! the file) and so is not a pure parser.
//!
//! [`CoopConfig::validate`]: coop::config::CoopConfig::validate

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        if let Ok(cfg) = toml::from_str::<coop::config::CoopConfig>(input) {
            let _ = cfg.validate();
        }
    }
});
