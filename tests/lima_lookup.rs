#![cfg(target_os = "macos")]
#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

/// Exercise the production resolver, not the unit-test PATH shim. CI installs
/// Lima before running this test. Stop at SSH key generation, before any VM or
/// image can be created, as the Darwin Nix install check does.
#[test]
fn setup_executes_trusted_lima_with_hostile_path() {
    let root = tempfile::Builder::new()
        .prefix(".coop-lima-lookup-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let data = root.path().join("state");
    fs::create_dir(&data).unwrap();
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(data.join("vm_key.pub")).unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, format!("data_dir = {data:?}\n")).unwrap();
    // Bound setup even in a Nix sandbox without system ssh-keygen access.
    let keygen = root.path().join("ssh-keygen");
    fs::write(&keygen, "#!/bin/sh\nexit 37\n").unwrap();
    fs::set_permissions(&keygen, fs::Permissions::from_mode(0o755)).unwrap();
    let shim = root.path().join("limactl");
    fs::write(
        &shim,
        "#!/bin/sh\nprintf 'hostile-lima-shim\\n' >&2\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_coop"))
        .args(["--config", config.to_str().unwrap(), "setup"])
        .env("LIMA_HOME", root.path().join("lima"))
        .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "setup passed its stop point");
    assert!(stderr.contains("  limactl: "), "{stderr}");
    assert!(stderr.contains("ssh-keygen failed"), "{stderr}");
    assert!(!stderr.contains("hostile-lima-shim"), "{stderr}");
}
