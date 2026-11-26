#![no_main]

//! Fuzz target for the config loader: `toml::from_str` into
//! [`coop::config::CoopConfig`] followed by [`CoopConfig::validate`].
//!
//! Deserialization exercises custom field parsers, including `SubnetMask`,
//! `HostInterface`, and `PortForward`. Validation also checks configured
//! paths against the local filesystem. Both valid and invalid inputs must
//! complete without panicking.
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
