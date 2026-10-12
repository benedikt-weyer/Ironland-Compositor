# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`ironland-compositor` is a Wayland compositor built on [Smithay](https://github.com/Smithay/smithay) (git dependency, pinned by rev in `Cargo.toml`), originally derived from Smithay's `anvil` example (`AnvilState` in `src/state/mod.rs`). It's designed to pair with a companion shell called molunga-shell (built on Quickshell), and ships two small companion binaries plus a Go settings GUI alongside the compositor itself.

The repo is developed inside a Nix flake / direnv environment (`.envrc` runs `use flake` and adds `scripts/` to `PATH`).

## Commands

Enter the dev shell first (direnv does this automatically via `.envrc`; otherwise `nix develop`).

- `run` — build and run the compositor (`cargo run`, nested via winit by default: `run --winit`). Defined in `scripts/run`.
- `run-vm` — boots a full NixOS VM (via `nix build .#nixosConfigurations.compositor-vm...`) with the compositor autostarting on login as user `dev`; use this to test standalone/tty-udev behavior or session integration that a nested winit window can't exercise. See `scripts/run-vm` for host GL caveats.
- `help` — lists the scripts above.
- `cargo build` / `cargo run -- --winit` / `cargo run -- --tty-udev` — standard cargo entry points; `--winit` nests the compositor in an existing session (development), `--tty-udev` runs standalone on a tty via DRM/KMS + libinput.
- `cargo test` — unit tests live inline (`#[cfg(test)]`) in `src/config.rs`, `src/wallpaper.rs`, `src/ui/launcher.rs`, `src/shell/workspace.rs`. Run a single test with `cargo test <name>`.
- `cargo clippy` / `cargo fmt` — both `clippy` and `rustfmt` are provided by `devShells.default` in `flake.nix`.
- `nix build .#default` — builds the compositor via crane (`packages.default` in `flake.nix`); `nix build .#settings-gui` builds the Go GUI.

### `gui-settings/` (separate Go module)

A standalone Fyne GUI for editing the compositor's TOML config (`ironland-compositor/gui-settings` module, Go 1.26). It has its own devshell: `nix develop .#settings-gui` (sets `GOFLAGS=-tags=wayland`, required for Fyne's native-Wayland glfw backend — don't drop that tag when invoking `go build`/`go test` outside the devshell). From within `gui-settings/`: `go test ./...`, `go test -run TestName ./...`. Note `gui-settings` (the built binary name) is gitignored — don't confuse it with the source directory of the same name.

There is no CI config in this repo (no `.github/workflows`); the closest thing is `nix flake check`-style building via the flake outputs above.

## Architecture

### Crate layout

`src/` is grouped into `backend/`, `input/`, `protocols/`, `render/`, `ui/` and `shell/`. `lib.rs` re-exports the leaf modules at the crate root, so `crate::foo::` paths (e.g. `crate::wallpaper`) still resolve. The three largest files are split into submodule directories sharing the parent's imports via `use super::*`: `state/` (`handlers.rs` = Smithay handler trait impls, `config.rs`, `repaint.rs`), `backend/udev/` (`device.rs` = DRM device/connector lifecycle, `render.rs`), `input/input_handler/` (`keyboard`, `pointer`, `touch`, `tablet_tool`, `gestures`, `actions`, `windowed`).

- `src/main.rs` — parses `--winit`/`--tty-udev` and dispatches to `winit::run_winit()` / `udev::run_udev()`.
- `src/state/mod.rs` — `AnvilState<BackendData>`, the central compositor state struct and the hub that Smithay's handler traits (`CompositorHandler`, `SeatHandler`, etc.) are implemented on. Almost every subsystem module is reached through fields/methods on this type.
- `src/backend/winit.rs` / `src/backend/udev.rs` — the two backends selectable at runtime, plus `src/backend/x11.rs` (feature-gated) for an X11 backend. `winit` nests in an existing session for development; `udev` runs standalone using DRM/KMS + libinput and needs `backend/session.rs`.
- `src/shell/` — window management: `xdg.rs` (native Wayland toplevels/popups), `x11.rs` (XWayland client handling — X11 windows tile the same as native toplevels via `tiling::should_tile_x11`; dialogs/utility/fixed-size windows opt out and stay floating), `tiling.rs` (automatic BSP/"dwindle" layout, Hyprland-style, one tree per output), `workspace.rs` (virtual desktops layered on top of tiling; per-output, with `PerMonitor` vs `Combined` switching modes and optional dynamic growth), `grabs.rs`/`ssd.rs`/`element.rs` (move/resize grabs, server-side decorations, the generic window element type).
- `src/config.rs` — user settings (keyboard layout + shortcuts) loaded from a TOML file, checked in this priority order: `$IRONLAND_COMPOSITOR_CONFIG` → `$XDG_CONFIG_HOME/ironland-compositor/config.toml` → `/etc/ironland-compositor/config.toml` (written by the NixOS module) → hardcoded defaults. A malformed file is logged and ignored, not fatal.
- `src/input/input_handler.rs` — keyboard/pointer input dispatch; the single call site that resolves key bindings to actions and routes surface-targeted grabs to `focus_grab.rs`/`shortcuts.rs`.
- `src/render/` (`mod.rs` is the pipeline; also `drawing.rs`, `font.rs`, `cursor.rs`, `wallpaper.rs`) — rendering pipeline, including a tiny embedded 5x7 bitmap font for the launcher overlay (no font-rendering dependency) and per-output wallpaper scaling/caching.
- `src/ui/launcher.rs` — XDG desktop entry discovery and fuzzy filtering for the app launcher overlay.
- `src/backend/session.rs` — announces the compositor session to systemd/D-Bus (`graphical-session.target`), required for `xdg-desktop-portal` and portal-backed dialogs to start at all.

### Custom Wayland protocol extensions (`protocols/*.xml`, `src/protocols/ironland_protocols.rs`)

Three protocols are custom to this compositor, generated at compile time via `wayland-scanner` (see `src/protocols/ironland_protocols.rs` module doc for why these aren't reused from `hyprland-*` equivalents — this compositor's needs are narrower):

- `ironland-shortcuts-v1` (`src/protocols/shortcuts.rs`) — named keybinding actions exposed to clients; `fire()` is the single entry point, called from `input_handler.rs`. Also backs the `GlobalShortcuts` xdg-desktop-portal backend.
- `ironland-focus-grab-v1` (`src/protocols/focus_grab.rs`) — single-surface pointer/key focus grabs (e.g. Quickshell popups); `check()` is the single entry point.
- `ironland-workspace-windows-v1` (`src/protocols/workspace_windows.rs`) — per-window workspace membership; `sync()` is the single entry point, called alongside `foreign_toplevel::sync` and `ext_workspace::ext_workspace_sync` on the same event set (window map/unmap, title/app-id change, workspace move).

### Standard protocol integrations that bridge to the shell

- `src/protocols/ext_workspace.rs` — server side of `ext-workspace-v1`; a thin read/write projection of `shell::workspace` state, not a second source of truth. `sync()` is the entry point.
- `src/protocols/foreign_toplevel.rs` — server side of `wlr-foreign-toplevel-management-unstable-v1`; reads window lists from `shell::workspace::all_windows` (deliberately not `state.space.elements()`, which would drop windows on inactive workspaces). `sync()` is the entry point.

These three `sync`/`fire`/`check` entry points (`ext_workspace`, `foreign_toplevel`, `workspace_windows`) intentionally don't diff against previous state before emitting — call all three together after any window/workspace-affecting event, matching the existing call sites.

### Companion binaries (`src/bin/`, not part of the compositor process)

- `ironland-workspaces` — a standalone Wayland client bridging `ext-workspace-v1` (+ best-effort `ironland-workspace-windows-v1`) to line-delimited JSON on stdin/stdout, since Quickshell has no built-in `ext-workspace-v1` support. See its module doc for the exact wire format.
- `ironland-portal-global-shortcuts` — the `org.freedesktop.impl.portal.GlobalShortcuts` xdg-desktop-portal backend, D-Bus-activated, bridging portal `BindShortcuts` requests to `ironland-shortcuts-v1`'s `bind` request. Packaging note: `resources/ironland.portal` is checked in as-is; the matching `.service` file's `Exec=` is generated by `flake.nix` (needs the Nix output path), not checked in.
- `ironland-portal-screenshot` / `ironland-portal-screencast` — the `Screenshot` and `ScreenCast` xdg-desktop-portal backends. Both capture over the compositor's privileged capture socket (`$IRONLAND_CAPTURE_SOCKET`) via `ext-image-copy-capture-v1` (`src/protocols/screencopy.rs` is the compositor side and gates every capturer by executable). `ironland-portal-screencast` publishes frames as a PipeWire video node (the `pipewire` crate needs libpipewire + libclang at build time, see `flake.nix`'s `bindgenEnv`); `xdg-desktop-portal` itself opens the PipeWire remote, so there's no `OpenPipeWireRemote`. Like the shortcuts backend, each has a checked-in `resources/*.portal` and a flake-generated `.service`.

### Nix packaging (`flake.nix`, `nix/module.nix`)

- `packages.default` builds the compositor via crane, wraps `Xwayland` onto `PATH` (Smithay's `XWayland::spawn` looks it up by bare name), and installs the portal registration files described above.
- `packages.settings-gui` builds the Go GUI via `buildGoModule`, tagged `wayland` so Fyne's glfw backend doesn't require `DISPLAY`.
- A VM configuration (`nixosConfigurations.compositor-vm`) autostarts the compositor as user `dev`, driven by `scripts/run-vm`.
