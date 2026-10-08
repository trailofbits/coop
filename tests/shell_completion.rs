//! Check every dynamic adapter without requiring every shell interpreter.

use std::process::Command;

const SHELLS: [(&str, &str); 5] = [
    ("bash", "COMPLETE=\"bash\""),
    ("zsh", "COMPLETE=\"zsh\""),
    ("fish", "COMPLETE=fish"),
    ("powershell", "$env:COMPLETE = \"powershell\""),
    ("elvish", "COMPLETE=\"elvish\""),
];

#[test]
fn generated_scripts_register_dynamic_completion_for_every_shell() -> anyhow::Result<()> {
    let binary = env!("CARGO_BIN_EXE_coop");
    for (shell, activation) in SHELLS {
        let output = Command::new(binary).args(["completions", shell]).output()?;
        anyhow::ensure!(output.status.success(), "{shell}: generation failed");
        let script = String::from_utf8(output.stdout)?;
        anyhow::ensure!(
            script.contains(activation),
            "{shell}: generated script must activate its dynamic adapter"
        );
        anyhow::ensure!(
            !script.contains(binary),
            "{shell}: saved script must not pin the generating binary's path"
        );
    }
    Ok(())
}

#[test]
fn completion_protocol_returns_live_candidates_for_every_shell() -> anyhow::Result<()> {
    for (shell, _) in SHELLS {
        for (words, index, candidate) in [
            (vec!["coop", ""], "1", "stop"),
            (vec!["coop", "profiles", "show", "py"], "3", "python"),
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_coop"))
                .arg("--")
                .args(words)
                .env("COMPLETE", shell)
                .env("_CLAP_COMPLETE_INDEX", index)
                .env("_CLAP_IFS", "\n")
                .output()?;
            anyhow::ensure!(output.status.success(), "{shell}: request failed");
            let candidates = String::from_utf8(output.stdout)?;
            // Fish/PowerShell append tab-separated help, zsh uses a colon.
            anyhow::ensure!(
                candidates
                    .lines()
                    .any(|line| line.split(['\t', ':']).next() == Some(candidate)),
                "{shell}: expected {candidate:?}, got {candidates:?}"
            );
        }
    }
    Ok(())
}

const BASH_HARNESS: &str = r#"
set -eu
test_binary=$1
source <("$test_binary" completions bash)
coop() {
    if [[ ${3-} == stop ]]; then
        [[ $COMPLETE == bash && $# == 4 && $1 == -- && $2 == coop && $4 == '' ]]
        printf '%s\n' completion-vm
    else
        "$test_binary" "$@"
    fi
}
COMP_WORDS=(coop stop '')
COMP_CWORD=2
COMP_TYPE=9
_clap_complete_coop coop ''
printf '%s\n' "${COMPREPLY[@]}"
COMP_WORDS=(coop profiles show py)
COMP_CWORD=3
_clap_complete_coop coop py
printf '%s\n' "${COMPREPLY[@]}"
"#;

#[test]
fn sourced_bash_script_completes_live_values() -> anyhow::Result<()> {
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            BASH_HARNESS,
            "bash-completion-test",
        ])
        .arg(env!("CARGO_BIN_EXE_coop"))
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "bash: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidates = String::from_utf8(output.stdout)?;
    anyhow::ensure!(
        candidates.starts_with("completion-vm\n"),
        "bash: stop completion must invoke the live handler, got {candidates:?}"
    );
    anyhow::ensure!(
        candidates.lines().skip(1).any(|value| value == "python"),
        "bash: profile completion must reach the real binary, got {candidates:?}"
    );
    Ok(())
}
