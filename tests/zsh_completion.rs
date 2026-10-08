//! Exercise generated zsh scripts without a VM or an interactive terminal.

use std::process::Command;

// Mock only zsh's candidate renderer and the stop request. Profile candidates
// still come from the real binary, so registration and the dynamic protocol
// must both work. Repeating stop catches first-TAB autoload regressions.
const HARNESS: &str = r#"
set -eu
test_binary=$1
test_dir=$2
test_mode=$3
"$test_binary" completions zsh > "$test_dir/_coop"
fpath=("$test_dir" $fpath)
autoload -Uz compinit
# The disposable fpath is under /tmp; trust this test fixture without prompting.
compinit -u -D
if [[ $test_mode == source ]]; then
    source "$test_dir/_coop"
fi
coop() {
    if [[ ${3-} == stop ]]; then
        [[ $COMPLETE == zsh && $# == 4 && $1 == -- && $2 == coop && $4 == '' ]]
        print -r -- completion-vm
    else
        "$test_binary" "$@"
    fi
}
_describe() {
    print -rl -- "${other[@]}"
}
words=(coop stop '')
CURRENT=3
${_comps[coop]}
${_comps[coop]}
words=(coop profiles show py)
CURRENT=4
${_comps[coop]}
"#;

fn check_completion(mode: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let output = Command::new("zsh")
        .args(["-f", "-c", HARNESS, "zsh-completion-test"])
        .arg(env!("CARGO_BIN_EXE_coop"))
        .arg(dir.path())
        .arg(mode)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{mode}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidates = String::from_utf8(output.stdout)?;
    anyhow::ensure!(
        candidates.starts_with("completion-vm\ncompletion-vm\n"),
        "{mode}: first and subsequent requests must return live candidates, got {candidates:?}"
    );
    // User-defined profiles may also match `py`; require the builtin without
    // depending on the developer's config containing no other profiles.
    anyhow::ensure!(
        candidates.lines().skip(2).any(|value| value == "python"),
        "{mode}: profile completion must reach the real binary, got {candidates:?}"
    );
    Ok(())
}

#[test]
fn sourced_zsh_script_completes_live_values() -> anyhow::Result<()> {
    check_completion("source")
}

#[test]
fn autoloaded_zsh_script_completes_live_values_on_first_tab() -> anyhow::Result<()> {
    check_completion("autoload")
}
