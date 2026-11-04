use std::process::Command;

#[test]
fn malformed_env_value_is_not_printed_or_allowed_to_reach_config_loading() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let config = root.path().join("invalid.toml");
    std::fs::write(&config, "[invalid\n")?;

    for command in ["up", "start"] {
        for (input, expected) in [
            ("1BAD=secret-sentinel", "--env KEY is invalid"),
            ("secret-sentinel", "--env expects KEY=VALUE"),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_coop"))
                .arg("--config")
                .arg(&config)
                .args([command, "--env", input])
                .output()?;
            anyhow::ensure!(!output.status.success(), "{command} unexpectedly succeeded");
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::ensure!(stderr.contains(expected), "{stderr}");
            anyhow::ensure!(!stderr.contains("secret-sentinel"), "{stderr}");
            anyhow::ensure!(!stderr.contains("invalid.toml"), "{stderr}");
        }
    }
    Ok(())
}
