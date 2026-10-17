# ironlandctl

A CLI for reading and editing `ironland-compositor`'s `config.toml`,
covering every setting the `gui-settings` Fyne app exposes. Both tools
share the same schema (the `ironland-config` crate), so they can never
drift on how the config file is read, merged with defaults, or written —
`gui-settings` itself now shells out to `ironlandctl` for its own load/save
(see `../gui-settings/ironlandctl.go`).

## Usage

```sh
# See the effective config (file merged over built-in defaults), or just
# one setting.
ironlandctl show
ironlandctl show --json
ironlandctl get keyboard.layout

# Change a setting; `unset` reverts it to the default.
ironlandctl set keyboard.layout us
ironlandctl set blur.enabled true
ironlandctl unset blur.radius

# Keybindings.
ironlandctl shortcuts list
ironlandctl shortcuts set quit "super+q,super+alt+backspace"
ironlandctl shortcuts events add nexus super+n

# Monitors.
ironlandctl outputs list
ironlandctl outputs set eDP-1 --primary
ironlandctl outputs set HDMI-A-1 --right-of eDP-1 --refresh-hz 144
ironlandctl outputs detect   # via wayland-info

# Let the compositor start the shell once the session is ready.
ironlandctl session units    # systemd --user services to pick from
ironlandctl set session.shell_unit desktop-shell.service
ironlandctl set session.start_shell true

# Start over.
ironlandctl reset
```

Run `ironlandctl --help` or `ironlandctl <command> --help` for the full
list of settings and flags; `ironlandctl get --help` lists every scalar
setting's dotted key (e.g. `blur.radius`, `focus.follows_mouse`,
`workspaces.count`).

By default, reads follow the same search path as the compositor itself
(`$IRONLAND_COMPOSITOR_CONFIG`, then `$XDG_CONFIG_HOME/ironland-compositor/
config.toml`, then `/etc/ironland-compositor/config.toml`) and writes
always go to the user's own config file. Pass `--config <path>` to point
both reads and writes at a specific file instead (mainly for testing).

## Shell completion

```sh
# zsh - add the generated function to a directory on $fpath (e.g. ~/.zfunc,
# after adding `fpath+=~/.zfunc` and `autoload -U compinit && compinit` to
# .zshrc):
ironlandctl completions zsh > ~/.zfunc/_ironlandctl

# bash:
ironlandctl completions bash > ~/.local/share/bash-completion/completions/ironlandctl

# fish:
ironlandctl completions fish > ~/.config/fish/completions/ironlandctl.fish
```

The Nix package (`packages.default` in the flake) installs all three
automatically.
