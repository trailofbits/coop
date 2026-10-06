#[cfg(target_os = "macos")]
#[test]
fn lima_rejects_explicit_setup_inputs_before_config_side_effects() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let data_dir = root.path().join("data");
    let config_path = root.path().join("config.json");
    let config = serde_json::json!({
        "data_dir": data_dir,
        "claude": {
            "config_dir": root.path().join("missing-claude-config"),
        },
        "updates": {
            "mode": "off",
        },
    });
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_coop"))
        .arg("--config")
        .arg(&config_path)
        .args([
            "setup",
            "--extra-packages",
            "ripgrep",
            "--post-install",
            "setup.sh",
        ])
        .env("COOP_NO_UPDATE_CHECK", "1")
        .output()?;

    anyhow::ensure!(!output.status.success(), "setup unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        stderr.contains("--extra-packages and --post-install"),
        "{stderr}"
    );
    anyhow::ensure!(stderr.contains("Lima backend"), "{stderr}");
    anyhow::ensure!(!stderr.contains("claude.config_dir"), "{stderr}");
    anyhow::ensure!(
        !data_dir.exists(),
        "setup must reject before preparing private storage"
    );
    Ok(())
}
