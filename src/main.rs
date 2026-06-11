//! Thin binary shim. All logic lives in the `coop` library crate so that fuzz
//! targets and integration tests can depend on it directly.

fn main() -> anyhow::Result<()> {
    coop::run()
}
