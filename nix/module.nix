{ config, lib, pkgs, ... }:

let
  cfg = config.services.ironland-compositor;
  settingsFormat = pkgs.formats.toml { };
in
{
  options.services.ironland-compositor.settings = lib.mkOption {
    type = settingsFormat.type;
    default = { };
    description = ''
      Settings for ironland-compositor, written to
      `/etc/ironland-compositor/config.toml` and read by the compositor at
      startup (a restart is needed to pick up changes). See the
      compositor's `src/config.rs` for the full schema: keyboard layout
      under `keyboard` (passed straight through to xkbcommon), the
      terminal launched by the `run_terminal` shortcut under `terminal`, the
      browser/file manager launched by `open_browser`/`open_file_manager`
      under `browser`/`file_manager`,
      `top_bar` (whether windows may get a compositor-drawn header/title
      bar for server-side decoration; off by default, so a client's request
      for one is overridden back to client-side), key bindings under
      `shortcuts` —
      only the actions you set there override the built-in defaults, so a
      partial table is fine — and
      per-monitor settings under `outputs.<connector-name>` (e.g.
      `outputs."eDP-1"`, `outputs."HDMI-A-1"`): `primary` marks the main
      monitor, `refresh_rate` selects its refresh rate in millihertz,
      `mirror_of` duplicates another output's position onto this one, and
      `position` places this output relative to another
      (`right_of`/`left_of`/`above`/`below`, each an output name) or at an
      absolute `{ x, y }`. An output not listed here is auto-placed to the
      right of the others, matching the compositor's previous behavior.
      Gaussian background blur for translucent application windows is under
      `blur`, with `enabled` (default false) and `radius` (default 12). The
      mouse cursor theme is under `cursor`: `theme` (an installed Xcursor
      theme name) and `size` (in pixels); leaving either unset falls back to
      the `XCURSOR_THEME`/`XCURSOR_SIZE` environment variables, or the
      compositor's own built-in cursor if those aren't set either. And
      virtual desktops under `workspaces`: `mode` is `"per_monitor"`
      (default, each output has its own set) or `"combined"` (every output
      switches together); `count` is the starting number of workspaces
      (default 4); `dynamic` (default false), if true, grows the count on
      demand and prunes empty trailing workspaces automatically instead of
      keeping it fixed at `count`; `overlay` (default true) shows a row of
      dots on screen briefly whenever the active workspace changes;
      `keep_disconnected` (default true) parks a disconnected monitor's
      workspaces and windows (even if it was the last monitor) and restores
      them when a monitor returns on the same connector, instead of moving
      its windows onto another monitor. Switch
      workspaces with Super+Left/Right; move the focused window to an
      adjacent one and follow it there with Super+Alt+Left/Right, or move it
      without following with Super+Ctrl+Alt+Left/Right (all rebindable in
      `shortcuts` as `workspace_left`/`workspace_right`,
      `move_workspace_left_follow`/`move_workspace_right_follow`, and
      `move_workspace_left`/`move_workspace_right`). Super+L locks the screen
      until the user's password is entered again (PAM; rebindable as
      `lock_screen`). Pointer/keyboard-focus
      interaction is under `focus`, both off by default (click-to-focus,
      unchanged from before either existed): `follows_mouse`, if true,
      focuses whatever window the pointer is over without needing a click
      (hovering empty space leaves the current focus alone); and
      `mouse_follows_focus`, if true, warps the pointer to the center of a
      window whenever it's focused by something other than the pointer
      itself (switching workspaces, cycling windows, a newly opened window,
      activating a window from the dock). Under `session`, `start_shell`
      (default false), if true, makes the compositor run `systemctl --user
      start <shell_unit>` (default `"desktop-shell.service"`) as the last step
      of bringing up the session, after XWayland is ready and `DISPLAY`/
      `WAYLAND_DISPLAY` are exported, so the shell and everything it launches
      inherit a complete environment; the unit must exist but must not be
      started by anything else. Under `vm`, launcher entries can be started
      inside a throw-away QEMU VM (press Left instead of Enter in the
      launcher) with their windows shown on the desktop like any other (Right
      starts a *persistent* VM instead: its disk and memory are saved when the
      program exits and restored on the next persistent launch of that
      application, keeping its files and starting in about a second; they live
      under `state_dir`, default
      `~/.local/share/ironland-compositor/vm/persistent`):
      `image` is the bootable guest disk (qcow2/raw; VMs boot a snapshot of
      it, so it's never modified and one image serves any number of VMs),
      which must run an SSH server accepting `ssh_key` for `user` (default
      `"user"`) and have `waypipe` installed; `memory_mb` (default 2048),
      `cpus` (default 2), `kvm` (default true), `max_instances` (default 8),
      `startup_timeout_secs` (default 180), `terminal` (the guest's terminal
      for `Terminal=true` entries, default the top-level `terminal`) and
      `extra_qemu_args` tune the rest. Nothing happens until `image` is set.
      `scripts/build-vm-guest` builds a suitable guest image.
    '';
    example = lib.literalExpression ''
      {
        keyboard = {
          layout = "de";
          variant = "nodeadkeys";
        };
        terminal = "alacritty";
        shortcuts = {
          # Overrides just this one binding; every other shortcut keeps
          # its built-in default.
          open_browser = [ "super+b" ];
        };
        outputs = {
          "eDP-1".primary = true;
          "HDMI-A-1".position.right_of = "eDP-1";
          # "DP-1".mirror_of = "eDP-1";
        };
      }
    '';
  };

  # PAM stack the screen lock (Super+L by default) checks the password
  # against; the compositor authenticates as the user it runs as, using the
  # service name `ironland-compositor` (see `src/ui/lock.rs`).
  config.security.pam.services.ironland-compositor = { };

  config.environment.etc."ironland-compositor/config.toml".source =
    settingsFormat.generate "ironland-compositor-config.toml" cfg.settings;
}
