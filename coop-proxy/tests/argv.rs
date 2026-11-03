//! Process-boundary tests for the production argument grammar.

#![expect(clippy::expect_used, reason = "tests")]

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_coop-proxy");

#[tokio::test]
async fn removed_no_jail_flag_fails_before_stdin_is_read() {
    let mut child = Command::new(BIN)
        .arg("--no-jail")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn coop-proxy");

    // Keep the write end open. If argument validation moved after the startup
    // document read, this wait would block instead of observing prompt refusal.
    let _open_stdin = child.stdin.take().expect("piped stdin");
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .expect("unknown argument should fail before reading stdin")
        .expect("wait for coop-proxy");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported coop-proxy arguments"),
        "unexpected stderr: {stderr}"
    );
}
