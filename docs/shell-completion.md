# Shell completion

`coop completions <shell>` generates dynamic completion for bash, zsh, fish, PowerShell, and elvish via `clap_complete`. It completes subcommands and flags, and asks coop for live instance, image, and profile names when you press Tab.

## Generated completion scripts

Generate a script once and drop it where your shell looks for completions. Loading a saved script does not invoke coop at shell startup; coop runs when completing a command. Regenerate saved scripts after upgrading coop so their completion protocol matches the installed binary.

### bash

```sh
mkdir -p ~/.local/share/bash-completion/completions
coop completions bash > ~/.local/share/bash-completion/completions/coop
```

System-wide variant:

```sh
coop completions bash | sudo tee /etc/bash_completion.d/coop > /dev/null
```

### zsh

The completion file must live on `$fpath`, configured before `compinit`. If you don't already have a directory for it:

```sh
mkdir -p ~/.zfunc
echo 'fpath=(~/.zfunc $fpath)' >> ~/.zshrc
echo 'autoload -Uz compinit && compinit' >> ~/.zshrc
coop completions zsh > ~/.zfunc/_coop
```

### fish

```sh
mkdir -p ~/.config/fish/completions
coop completions fish > ~/.config/fish/completions/coop.fish
```

### PowerShell

```powershell
coop completions powershell | Set-Content -Encoding utf8 "$HOME/coop-completion.ps1"
Add-Content $PROFILE '. "$HOME/coop-completion.ps1"'
```

The profile line loads the saved script on each shell start. Ensure `$PROFILE` exists before adding the line.

### elvish

```sh
mkdir -p ~/.config/elvish/lib
coop completions elvish > ~/.config/elvish/lib/coop-completion.elv
echo 'use coop-completion' >> ~/.config/elvish/rc.elv
```

Restart the shell (or `source` your rc) after the first install.

## Generate on shell startup

Instead of saving a script, add one of these lines to your shell rc. This runs coop once per shell startup to generate the script, and avoids needing to regenerate a saved file after upgrades. Coop also runs on Tab to compute candidates; it does not start a VM.

```sh
# bash (~/.bashrc)
source <(coop completions bash)

# zsh (~/.zshrc, after compinit)
source <(coop completions zsh)

# fish (~/.config/fish/config.fish)
coop completions fish | source

# elvish (~/.config/elvish/rc.elv)
eval (coop completions elvish | slurp)
```

```powershell
# PowerShell ($PROFILE)
coop completions powershell | Out-String | Invoke-Expression
```

Existing `COMPLETE=<shell>` setup continues to work; only one setup is needed. Replace older static completion files with newly generated scripts.

## What completes where

| Argument | Source |
|----------|--------|
| Running instance name (`shell`, `claude`, `claude-agents`, `codex`, `push`, `pull`, `exec`, `agent update`) | Running VMs in `~/.coop/instances/` |
| Stopped instance name (`start`) | Stopped VMs in `~/.coop/instances/` |
| Other VM arguments (`stop`, `destroy`, `status`, `logs`, `editor`, `resize`, `model`, and management commands) | All registered VMs in `~/.coop/instances/` |
| `--image` (`up`, `setup`, `start` compatibility flag), `images --delete` | `~/.coop/images/` |
| `--profile` (`setup`, `up`), `profiles show <name>` | builtin profiles plus `[profiles.*]` from `~/.coop/config.toml` |
