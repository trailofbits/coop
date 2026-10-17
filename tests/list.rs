#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

#[test]
fn list_and_ls_share_sorted_output_and_one_inventory() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let data = root.path().join("data");
    for (index, name) in ["beta", "alpha"].into_iter().enumerate() {
        let dir = data.join("instances").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("instance.json"),
            format!(r#"{{"name":"{name}","index":{index}}}"#),
        )
        .unwrap();
    }
    let config = root.path().join("config.toml");
    fs::write(&config, format!("data_dir = {data:?}\n")).unwrap();
    let count = root.path().join("count");
    let shim = root.path().join("limactl");
    fs::write(
        &shim,
        "#!/bin/sh\n[ \"$*\" = 'list --json' ] || exit 99\nprintf 'query\\n' >> \"$LIMA_HOME/count\"\nprintf '%s\\n' '{\"name\":\"coop-alpha\",\"status\":\"Running\"}' '{\"name\":\"coop-beta\",\"status\":\"Stopped\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let alpha_state = if cfg!(target_os = "macos") {
        "running"
    } else {
        "stopped"
    };
    for command in ["list", "ls"] {
        for json in [false, true] {
            fs::write(&count, "").unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_coop"));
            cmd.args(["--config", config.to_str().unwrap(), command])
                .env("LIMA_HOME", root.path())
                .env("PATH", root.path());
            if json {
                cmd.arg("--json");
            }
            let output = cmd.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            if json {
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(
                    value,
                    serde_json::json!([
                        {"name": "alpha", "state": alpha_state},
                        {"name": "beta", "state": "stopped"},
                    ])
                );
            } else {
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    format!(
                        "{:<16} STATE\n{:<16} {alpha_state}\n{:<16} stopped\n",
                        "NAME", "alpha", "beta"
                    ),
                );
            }
            assert_eq!(
                fs::read_to_string(&count).unwrap().lines().count(),
                usize::from(cfg!(target_os = "macos")),
            );
        }
    }
}
