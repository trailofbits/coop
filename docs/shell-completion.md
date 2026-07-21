# Shell completion

`coop` ships with both static and dynamic shell completion via `clap_complete`. Static completion handles subcommand and flag names; dynamic completion additionally fills in live values for instance names, image names, and profile names by reading `~/.coop`.

## Static completion

Generate a script once and drop it where your shell looks for completions.

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

The completion file must live on `$fpath`. If you don't already have a directory for it:

```sh
mkdir -p ~/.zfunc
echo 'fpath=(~/.zfunc $fpath)' >> ~/.zshrc
echo 'autoload -Uz compinit && compinit' >> ~/.zshrc
coop completions zsh > ~/.zfunc/_coop
```

### fish

```sh
coop completions fish > ~/.config/fish/completions/coop.fish
```

### PowerShell

```powershell
coop completions powershell | Out-String | Invoke-Expression
```

Add the same line to your `$PROFILE` to load it on every shell start.

### elvish

```sh
coop completions elvish > ~/.config/elvish/lib/coop-completion.elv
echo 'use coop-completion' >> ~/.config/elvish/rc.elv
```

Restart the shell (or `source` your rc) after the first install.

## Dynamic completion

Dynamic completion lets `coop` itself compute candidates on TAB — so `coop shell <TAB>` lists your running instances, `coop up --image <TAB>` lists existing images, and `coop setup --profile <TAB>` / `coop up --profile <TAB>` list builtin and custom profiles.

Add one line to your shell rc:

```sh
# bash
echo 'source <(COMPLETE=bash coop)' >> ~/.bashrc

# zsh
echo 'source <(COMPLETE=zsh coop)' >> ~/.zshrc

# fish
echo 'source (COMPLETE=fish coop | psub)' >> ~/.config/fish/config.fish

# elvish
echo 'eval (COMPLETE=elvish coop | slurp)' >> ~/.config/elvish/rc.elv
```

Dynamic and static completion can coexist — static fills in subcommand and flag names even without `COMPLETE=…` set up.

## What completes where

| Argument | Source |
|----------|--------|
| Instance name (`shell`, `claude`, `claude-agents`, `codex`, `stop`, `destroy`, `status`, `logs`, `push`, `pull`, `exec`, `editor`, `resize`, `model`) | `~/.coop/instances/` |
| `--image` (`up`, `setup`, `start` compatibility flag), `images --delete` | `~/.coop/images/` |
| `--profile` (`setup`, `up`), `profiles show <name>` | builtin profiles plus `[profiles.*]` from `~/.coop/config.toml` |
