#![cfg(target_os = "macos")]
#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

#[test]
fn cli_rejects_lima_mount_templates_and_imports_before_setup() {
    let root = tempfile::Builder::new()
        .prefix(".coop-mount-guard-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let data = root.path().join("state");
    fs::create_dir(&data).unwrap();
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(data.join("vm_key.pub")).unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, format!("data_dir = {data:?}\n")).unwrap();
    let global = root.path().join("lima/_config");
    fs::create_dir_all(&global).unwrap();
    fs::set_permissions(&global, fs::Permissions::from_mode(0o700)).unwrap();
    let keygen = root.path().join("ssh-keygen");
    fs::write(&keygen, "#!/bin/sh\nexit 37\n").unwrap();
    fs::set_permissions(&keygen, fs::Permissions::from_mode(0o755)).unwrap();

    let run_setup = || {
        Command::new(env!("CARGO_BIN_EXE_coop"))
            .args(["--config", config.to_str().unwrap(), "setup"])
            .env("LIMA_HOME", root.path().join("lima"))
            .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
            .output()
            .unwrap()
    };

    fs::write(global.join("default.yaml"), "mounts: []\n").unwrap();
    let safe = run_setup();
    let safe_stderr = String::from_utf8_lossy(&safe.stderr);
    assert!(safe_stderr.contains("ssh-keygen failed"), "{safe_stderr}");

    fs::write(
        global.join("default.yaml"),
        "mounts:\n- location: '/Users/{{.User}}'\n  writable: true\n",
    )
    .unwrap();
    let template = run_setup();
    assert!(
        String::from_utf8_lossy(&template.stderr).contains("path template"),
        "{}",
        String::from_utf8_lossy(&template.stderr)
    );

    fs::write(
        global.join("default.yaml"),
        "base:\n- url: ./unsafe.yaml\nmounts: []\n",
    )
    .unwrap();
    let imported = run_setup();
    assert!(
        String::from_utf8_lossy(&imported.stderr).contains("base template"),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
}
