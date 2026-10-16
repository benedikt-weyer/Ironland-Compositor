//! Config schema and file I/O for ironland-compositor's `config.toml`.
//!
//! Settings are read from the first of these that exists, in order:
//!
//! 1. `$IRONLAND_COMPOSITOR_CONFIG` (an explicit path, mainly for testing)
//! 2. `$XDG_CONFIG_HOME/ironland-compositor/config.toml` (or `~/.config/...`)
//! 3. `/etc/ironland-compositor/config.toml` (written by the NixOS module)
//!
//! None of these existing is not an error: callers fall back to the
//! defaults below, which reproduce the shortcuts that used to be
//! hardcoded. A malformed file is reported to the caller rather than
//! silently ignored here - [`Config::load`] logs and falls back to
//! defaults (matching the compositor's own live-reload tolerance), while
//! [`Config::try_load`] surfaces the parse error for a caller (`ironlandctl`,
//! the settings GUI) that wants to tell the user about it instead.
//!
//! This crate is deliberately free of any Wayland/smithay dependency so it
//! can be shared, as a lightweight build, between the compositor itself,
//! `ironlandctl`, and (indirectly, via `ironlandctl`) the Go settings GUI.
//! Anything that needs a keysym or geometry type - parsing a keybinding
//! spec into a modifiers+keysym pair, resolving an output's on-screen
//! position - lives in the compositor crate's own `src/keybindings.rs`
//! instead, built on top of the plain-data types here.

use std::{collections::HashMap, env, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

/// What the Caps Lock key is reassigned to. Applied as an extra XKB option
/// on top of [`KeyboardSettings::options`], so it can be toggled without
/// hand-editing the options string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapsLockRemap {
    /// Caps Lock keeps its normal behavior.
    #[default]
    Off,
    /// Caps Lock acts as an additional Ctrl key (`ctrl:nocaps`).
    Ctrl,
    /// Caps Lock acts as the Level-3 shift, i.e. AltGr (`lv3:caps_switch`).
    Level3,
}

impl CapsLockRemap {
    /// The XKB option implementing this remap, if any.
    pub fn xkb_option(self) -> Option<&'static str> {
        match self {
            CapsLockRemap::Off => None,
            CapsLockRemap::Ctrl => Some("ctrl:nocaps"),
            CapsLockRemap::Level3 => Some("lv3:caps_switch"),
        }
    }
}

/// Keyboard layout settings, passed straight through to xkbcommon.
///
/// An empty string for any field means "let xkbcommon fall back to its
/// `XKB_DEFAULT_*` environment variables / built-in default".
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct KeyboardSettings {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
    /// Reassigns Caps Lock; see [`CapsLockRemap`].
    pub caps_lock: CapsLockRemap,
}

/// Where to place an output relative to another, already-placed one, or at
/// an explicit logical position. Kept separate from [`OutputSettings::mirror_of`]:
/// mirroring takes priority over `position` when both are set for the same
/// output.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum OutputPosition {
    RightOf { right_of: String },
    LeftOf { left_of: String },
    Above { above: String },
    Below { below: String },
    Absolute { x: i32, y: i32 },
}

/// Per-output settings, keyed by connector name (e.g. `"eDP-1"`,
/// `"HDMI-A-1"`) in the `[outputs.*]` config table.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(default)]
pub struct OutputSettings {
    /// Marks this the primary monitor. At most one output should set this;
    /// if several do, which one wins is unspecified.
    pub primary: bool,
    /// Requested vertical refresh rate in millihertz. The compositor picks
    /// the closest advertised rate at the monitor's preferred resolution.
    pub refresh_rate: Option<i32>,
    /// Name of another output to duplicate ("mirror"/"clone" this one onto),
    /// by placing this output at that output's position. Takes priority over
    /// `position`.
    pub mirror_of: Option<String>,
    /// Where to place this output when it isn't mirroring another one.
    /// Defaults to auto-placement (stacked to the right of every other
    /// placed output), matching pre-existing behavior.
    pub position: Option<OutputPosition>,
}

/// Whether workspaces are independent per output ("split") or shared across
/// every connected output ("combined", i.e. switching workspaces moves every
/// monitor to the same slot at once, GNOME-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    #[default]
    PerMonitor,
    Combined,
}

/// Axis windows slide along during an animated workspace switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTransitionAxis {
    #[default]
    Horizontal,
    Vertical,
}

/// Workspace settings: how many virtual desktops exist, whether outputs
/// share them or each gets their own, and whether the on-screen dot
/// indicator (shown briefly on switch) is enabled.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct WorkspaceSettings {
    pub mode: WorkspaceMode,
    /// How many workspaces each output (or the whole session, in `Combined`
    /// mode) starts with.
    pub count: usize,
    /// If true, the workspace count isn't fixed at `count`: switching or
    /// moving a window past the last workspace creates a new one on demand,
    /// and trailing empty workspaces are dropped again automatically.
    pub dynamic: bool,
    /// Whether to flash a row of dots (like GNOME's workspace switcher) on
    /// screen briefly whenever the active workspace changes.
    pub overlay: bool,
    /// How long windows take to slide to/from the next workspace on a
    /// switch, in milliseconds. `0` disables the animation - the switch is
    /// instant, as it was before this setting existed.
    pub transition_ms: u32,
    /// Axis the slide animation moves along.
    pub transition_axis: WorkspaceTransitionAxis,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        WorkspaceSettings {
            mode: WorkspaceMode::default(),
            count: 4,
            dynamic: false,
            overlay: true,
            transition_ms: 220,
            transition_axis: WorkspaceTransitionAxis::default(),
        }
    }
}

/// Mouse cursor theme settings: `None` means "fall back to the
/// `XCURSOR_THEME`/`XCURSOR_SIZE` environment variable, or the compositor's
/// own built-in default if that isn't set either".
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(default)]
pub struct CursorSettings {
    pub theme: Option<String>,
    pub size: Option<u32>,
}

/// Bouncy grow-in/reflow animation for windows entering or leaving the
/// automatic tiling layout (see `shell::tiling` in the compositor crate -
/// this crate only carries the plain setting, not the animation itself).
/// Off by default, like every other opt-in visual effect this compositor
/// ships (see [`BlurSettings`], [`CornersSettings`]).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct WindowAnimationSettings {
    pub enabled: bool,
    /// How long the grow-in/reflow animation takes, in milliseconds. `0`
    /// disables it even if `enabled` is true.
    pub duration_ms: u32,
}

impl Default for WindowAnimationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: 260,
        }
    }
}

/// Background blur shown through translucent application surfaces.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct BlurSettings {
    pub enabled: bool,
    /// Gaussian standard deviation in logical pixels.
    pub radius: u32,
}

impl Default for BlurSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            radius: 12,
        }
    }
}

/// Rounded corners on window content, drawn via a GLES shader. Off by
/// default, matching the sharp-cornered windows this compositor had before
/// the feature existed.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct CornersSettings {
    pub enabled: bool,
    /// Corner radius in logical pixels.
    pub radius: u32,
}

impl Default for CornersSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            radius: 12,
        }
    }
}

/// Space between tiled windows, and between tiled windows and the output
/// edges. Both default to reproducing this compositor's hardcoded behavior
/// before either was configurable: an 8px gap between windows, and no gap
/// at the screen edge.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct GapsSettings {
    pub inner: u32,
    pub outer: u32,
}

impl Default for GapsSettings {
    fn default() -> Self {
        Self { inner: 8, outer: 0 }
    }
}

/// A highlight border drawn around the currently focused window only (not
/// every tiled window). Off by default. `gradient_color` unset draws a
/// solid `color` border; set, it draws a two-stop linear gradient from
/// `color` to `gradient_color` along `angle` degrees, similar to
/// Hyprland's `col.active_border`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct BorderSettings {
    pub enabled: bool,
    /// Border thickness in logical pixels.
    pub thickness: u32,
    /// `#rrggbb` or `#rrggbbaa`.
    pub color: String,
    /// `#rrggbb` or `#rrggbbaa`; unset means a solid `color` border.
    pub gradient_color: Option<String>,
    /// Gradient direction in degrees. Ignored when `gradient_color` is unset.
    pub angle: f32,
}

impl Default for BorderSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            thickness: 2,
            color: "#89b4fa".to_string(),
            gradient_color: None,
            angle: 45.0,
        }
    }
}

/// Pointer/keyboard-focus interaction. Both default off, matching the
/// click-to-focus behavior this compositor had before either existed.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(default)]
pub struct FocusSettings {
    /// If true, moving the pointer over a window focuses it, without
    /// needing a click. Hovering empty space (no window under the
    /// pointer) leaves the current focus alone rather than clearing it.
    pub follows_mouse: bool,
    /// If true, a focus change that didn't come from the pointer itself
    /// (switching workspaces, cycling windows, a newly mapped window
    /// taking focus, activating a window from the dock) warps the pointer
    /// to the center of the newly focused window.
    pub mouse_follows_focus: bool,
}

/// Screen lock behaviour (see `src/ui/lock.rs` in the compositor).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct LockSettings {
    /// Lock the screen automatically when the laptop lid is closed.
    pub lock_on_lid_close: bool,
}

impl Default for LockSettings {
    fn default() -> Self {
        Self {
            lock_on_lid_close: true,
        }
    }
}

/// Where the FPS/frame-time overlay is anchored on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

/// On-screen FPS overlay and frame stutter analytics. Off by default -
/// purely a diagnostic tool, not something that should draw over a user's
/// session unasked.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct PerformanceSettings {
    /// Draws a small per-output overlay with the current FPS, last frame
    /// time, and a running stutter count.
    pub fps_overlay: bool,
    /// Screen corner the overlay is anchored to.
    pub fps_overlay_position: OverlayPosition,
    /// Frame time, in milliseconds, above which a frame counts as a
    /// "stutter" (shown in the overlay and, if `stutter_log` is on, logged).
    /// `0.0` (the default) picks this automatically as 1.5x the expected
    /// frame time for the output's current refresh rate (falling back to
    /// 60Hz if that isn't known yet).
    pub stutter_threshold_ms: f32,
    /// Whether a detected stutter is also logged via `tracing::warn!`
    /// (`journalctl --user -u ...`, or the terminal under `run`), in
    /// addition to being reflected in the overlay itself.
    pub stutter_log: bool,
    /// How often the FPS overlay's *text* is redrawn, in milliseconds.
    /// Frame timing is still recorded every frame regardless (stutter
    /// detection doesn't depend on this), but rebuilding and re-rasterizing
    /// the overlay's own tiny texture on every single frame is unnecessary
    /// work purely for a number a human is going to glance at, not read at
    /// 180Hz - `0` disables the throttle and redraws every frame, mainly
    /// for comparing against this default while investigating a
    /// suspected-slow render path.
    pub fps_overlay_interval_ms: u32,
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            fps_overlay: false,
            fps_overlay_position: OverlayPosition::TopRight,
            stutter_threshold_ms: 0.0,
            stutter_log: true,
            fps_overlay_interval_ms: 500,
        }
    }
}

/// Key-state overlay for diagnosing stuck/phantom keys (e.g. a Ctrl key that
/// reads as held without being touched). Off by default - purely a
/// diagnostic tool.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct KeyTrackerSettings {
    /// Draws a small overlay listing each tracked key with a pressed/
    /// unpressed keycap icon and its name.
    pub enabled: bool,
    /// xkb keysym names of the keys to track, e.g. `"Control_L"`, `"Shift_R"`,
    /// `"a"` (matched case-insensitively against the key's layout-level-0
    /// symbol, so it follows the active layout/Caps Lock remap).
    pub keys: Vec<String>,
    /// Screen corner the overlay is anchored to.
    pub position: OverlayPosition,
}

impl Default for KeyTrackerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            keys: vec!["Control_L".to_string(), "Control_R".to_string()],
            position: OverlayPosition::TopLeft,
        }
    }
}

/// GUI/CLI-only appearance state: the compositor itself has no notion of a
/// color scheme, so this isn't part of [`Config`] - the compositor's
/// [`RawConfig`]-equivalent parser simply ignores an `[appearance]` table it
/// doesn't know about. Kept in the same `config.toml` purely so a dark/light
/// mode toggle (in `ironlandctl` or the settings GUI) remembers its state
/// across runs.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(default)]
pub struct AppearanceSettings {
    pub dark_mode: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
struct RawConfig {
    keyboard: KeyboardSettings,
    terminal: Option<String>,
    browser: Option<String>,
    file_manager: Option<String>,
    top_bar: bool,
    wallpaper: Option<String>,
    blur: BlurSettings,
    corners: CornersSettings,
    gaps: GapsSettings,
    border: BorderSettings,
    cursor: CursorSettings,
    focus: FocusSettings,
    lock: LockSettings,
    performance: PerformanceSettings,
    key_tracker: KeyTrackerSettings,
    shortcuts: HashMap<String, Vec<String>>,
    gestures: HashMap<String, Vec<String>>,
    outputs: HashMap<String, OutputSettings>,
    workspaces: WorkspaceSettings,
    window_animations: WindowAnimationSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub keyboard: KeyboardSettings,
    pub terminal: String,
    /// Command spawned by the `open_browser` action.
    pub browser: String,
    /// Command spawned by the `open_file_manager` action.
    pub file_manager: String,
    /// Whether windows may get a compositor-drawn header bar ("top bar") for
    /// server-side decoration. Off by default: a client's request for
    /// server-side decoration is overridden back to client-side, so no
    /// header bar is drawn regardless of what individual clients ask for.
    pub top_bar: bool,
    /// Path to an image file (PNG/JPEG/WebP) to use as the desktop
    /// background, scaled and center-cropped to cover each output. `None`
    /// (the default, and also the fallback if the path fails to load) uses
    /// the compositor's built-in default wallpaper.
    pub wallpaper: Option<String>,
    /// Gaussian backdrop blur settings for translucent application windows.
    pub blur: BlurSettings,
    /// Rounded corner settings for window content (see [`CornersSettings`]).
    pub corners: CornersSettings,
    /// Gap between tiled windows and between them and the output edges (see
    /// [`GapsSettings`]).
    pub gaps: GapsSettings,
    /// Highlight border around the focused window (see [`BorderSettings`]).
    pub border: BorderSettings,
    /// Mouse cursor theme/size (see [`CursorSettings`]).
    pub cursor: CursorSettings,
    /// Pointer/keyboard-focus interaction (see [`FocusSettings`]).
    pub focus: FocusSettings,
    /// Screen lock behaviour (see [`LockSettings`]).
    pub lock: LockSettings,
    /// FPS overlay and frame stutter analytics (see [`PerformanceSettings`]).
    pub performance: PerformanceSettings,
    /// Key-state overlay for finding stuck keys (see [`KeyTrackerSettings`]).
    pub key_tracker: KeyTrackerSettings,
    /// action name -> key combos, e.g. `"toggle_launcher" -> ["ctrl+space"]`.
    /// Always fully populated: entries not overridden by the config file
    /// keep their built-in default.
    pub shortcuts: HashMap<String, Vec<String>>,
    /// action name -> touchpad gesture specs, e.g. `"workspace_right" ->
    /// ["swipe:3:left"]` (see [`parse_gesture`] for the spec format). Takes
    /// the same action names as `shortcuts` (including `shortcut:<name>`).
    /// Always fully populated like `shortcuts`: entries not overridden by
    /// the config file keep their built-in default.
    #[serde(default = "default_gestures")]
    pub gestures: HashMap<String, Vec<String>>,
    /// connector name (e.g. `"eDP-1"`) -> settings for that output. Outputs
    /// not present here use [`OutputSettings::default`] (auto-placed,
    /// extended, not primary).
    pub outputs: HashMap<String, OutputSettings>,
    /// Virtual desktop settings (see [`WorkspaceSettings`]).
    pub workspaces: WorkspaceSettings,
    /// Bouncy grow-in/reflow animation for auto-tiled windows (see
    /// [`WindowAnimationSettings`]).
    pub window_animations: WindowAnimationSettings,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            keyboard: KeyboardSettings::default(),
            terminal: default_terminal(),
            browser: default_browser(),
            file_manager: default_file_manager(),
            top_bar: false,
            wallpaper: None,
            blur: BlurSettings::default(),
            corners: CornersSettings::default(),
            gaps: GapsSettings::default(),
            border: BorderSettings::default(),
            cursor: CursorSettings::default(),
            focus: FocusSettings::default(),
            lock: LockSettings::default(),
            performance: PerformanceSettings::default(),
            key_tracker: KeyTrackerSettings::default(),
            shortcuts: default_shortcuts(),
            gestures: default_gestures(),
            outputs: HashMap::new(),
            workspaces: WorkspaceSettings::default(),
            window_animations: WindowAnimationSettings::default(),
        }
    }
}

/// [`Config`] plus the GUI/CLI-only [`AppearanceSettings`], as it round-trips
/// through `config.toml` end to end. `ironlandctl`'s `show --json`/
/// `defaults --json`/`apply` and the settings GUI all speak this shape.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct FullConfig {
    #[serde(flatten)]
    pub config: Config,
    #[serde(default)]
    pub appearance: AppearanceSettings,
}

fn default_terminal() -> String {
    "weston-terminal".to_string()
}

fn default_browser() -> String {
    "brave".to_string()
}

fn default_file_manager() -> String {
    "iron-file".to_string()
}

/// The shortcuts the compositor shipped with before it became configurable.
/// Kept as the baseline so an empty/missing/partial config file still
/// behaves exactly as before.
pub fn default_shortcuts() -> HashMap<String, Vec<String>> {
    [
        ("quit", vec!["super+alt+backspace", "super+q"]),
        ("run_terminal", vec!["super+c"]),
        ("toggle_launcher", vec!["ctrl+space"]),
        // Fires the shell's own "launcher" shortcut (see caelestia-shell's
        // `modules/Shortcuts.qml`) via the `ironland-shortcuts-v1`
        // `shortcut:<name>` escape hatch - not the compositor's built-in
        // launcher above.
        ("shortcut:launcher", vec!["super"]),
        // Fires the shell's own "screenshot" shortcut (see
        // caelestia-shell-iron's `modules/areapicker/AreaPicker.qml`) the
        // same way - the shell's picker overlay handles whole-display vs.
        // drag-to-select itself.
        ("shortcut:screenshot", vec!["print"]),
        // Hardware media keys, fired to the shell's own named shortcuts
        // (see caelestia-shell-iron's `services/Players.qml` for
        // `mediaToggle`/`mediaPrev`/`mediaNext` and `modules/Shortcuts.qml`
        // for `volumeMute`) the same way as `shortcut:launcher` above.
        // `XF86AudioPlay`/`XF86AudioPause` both map to `mediaToggle` since
        // keyboards vary in which one they actually send for a play/pause
        // key.
        (
            "shortcut:mediaToggle",
            vec!["XF86AudioPlay", "XF86AudioPause"],
        ),
        ("shortcut:mediaPrev", vec!["XF86AudioPrev"]),
        ("shortcut:mediaNext", vec!["XF86AudioNext"]),
        ("shortcut:volumeMute", vec!["XF86AudioMute"]),
        ("open_browser", vec!["super+b"]),
        ("open_file_manager", vec!["super+f"]),
        ("toggle_floating", vec!["super+shift+space"]),
        ("kill_window", vec!["super+x"]),
        // Focus-by-direction lives on `super+shift+<arrow>`; swapping the
        // focused window with its neighbor is `super+shift+alt+<arrow>`
        // (`super+ctrl+<arrow>` moves it to another monitor, below).
        // Bare `super+left/right` switch workspaces (see `workspace_left`/
        // `workspace_right` below).
        ("focus_left", vec!["super+shift+left"]),
        ("focus_right", vec!["super+shift+right"]),
        ("focus_up", vec!["super+shift+up"]),
        ("focus_down", vec!["super+shift+down"]),
        ("swap_left", vec!["super+shift+alt+left"]),
        ("swap_right", vec!["super+shift+alt+right"]),
        ("swap_up", vec!["super+shift+alt+up"]),
        ("swap_down", vec!["super+shift+alt+down"]),
        // Moves the focused window to the adjacent monitor in that direction.
        ("move_monitor_left", vec!["super+ctrl+left"]),
        ("move_monitor_right", vec!["super+ctrl+right"]),
        ("move_monitor_up", vec!["super+ctrl+up"]),
        ("move_monitor_down", vec!["super+ctrl+down"]),
        // Likewise, `resize_left`/`resize_right` moved off `super+alt+left/
        // right`, which now moves the focused window to an adjacent
        // workspace and follows it there (see `move_workspace_left_follow`/
        // `move_workspace_right_follow`).
        ("resize_left", vec!["super+ctrl+shift+left"]),
        ("resize_right", vec!["super+ctrl+shift+right"]),
        ("resize_up", vec!["super+alt+up"]),
        ("resize_down", vec!["super+alt+down"]),
        ("workspace_left", vec!["super+left"]),
        ("workspace_right", vec!["super+right"]),
        // Moves the focused window to an adjacent workspace without
        // switching to it, unlike the `_follow` pair below.
        ("move_workspace_left", vec!["super+ctrl+alt+left"]),
        ("move_workspace_right", vec!["super+ctrl+alt+right"]),
        ("move_workspace_left_follow", vec!["super+alt+left"]),
        ("move_workspace_right_follow", vec!["super+alt+right"]),
        ("scale_up", vec!["super+shift+p"]),
        ("scale_down", vec!["super+shift+m"]),
        ("toggle_preview", vec!["super+shift+w"]),
        ("rotate_output", vec!["super+shift+r"]),
        ("toggle_tint", vec!["super+shift+t"]),
        ("toggle_decorations", vec!["super+shift+d"]),
        // Locks the screen until the user re-enters their password (PAM).
        ("lock_screen", vec!["super+l"]),
        // Opens/closes the workspace overview.
        ("toggle_overview", vec!["super+space"]),
    ]
    .into_iter()
    .map(|(name, keys)| {
        (
            name.to_string(),
            keys.into_iter().map(String::from).collect(),
        )
    })
    .collect()
}

/// Direction of a touchpad swipe, as the direction the fingers travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SwipeDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A touchpad gesture a binding can trigger on. Only discrete, one-shot
/// gestures are bindable: the action fires once when the gesture ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Gesture {
    /// Multi-finger swipe, e.g. `swipe:3:left`.
    Swipe {
        fingers: u32,
        direction: SwipeDirection,
    },
    /// Multi-finger pinch, e.g. `pinch:2:in` (fingers moving together) or
    /// `pinch:2:out` (fingers spreading apart).
    Pinch { fingers: u32, zoom_in: bool },
}

impl Gesture {
    /// Number of fingers the gesture needs, used to decide whether the
    /// compositor claims a gesture from the moment it begins.
    pub fn fingers(&self) -> u32 {
        match *self {
            Gesture::Swipe { fingers, .. } | Gesture::Pinch { fingers, .. } => fingers,
        }
    }
}

/// Parses a gesture spec: `swipe:<fingers>:<left|right|up|down>` or
/// `pinch:<fingers>:<in|out>`. Finger count must be 2-5 for pinch and 3-5
/// for swipe (2-finger swipes are scrolling).
pub fn parse_gesture(spec: &str) -> Option<Gesture> {
    let mut parts = spec.split(':').map(|p| p.trim().to_ascii_lowercase());
    let kind = parts.next()?;
    let fingers: u32 = parts.next()?.parse().ok()?;
    let arg = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    match kind.as_str() {
        "swipe" if (3..=5).contains(&fingers) => {
            let direction = match arg.as_str() {
                "left" => SwipeDirection::Left,
                "right" => SwipeDirection::Right,
                "up" => SwipeDirection::Up,
                "down" => SwipeDirection::Down,
                _ => return None,
            };
            Some(Gesture::Swipe { fingers, direction })
        }
        "pinch" if (2..=5).contains(&fingers) => match arg.as_str() {
            "in" => Some(Gesture::Pinch {
                fingers,
                zoom_in: true,
            }),
            "out" => Some(Gesture::Pinch {
                fingers,
                zoom_in: false,
            }),
            _ => None,
        },
        _ => None,
    }
}

/// Built-in touchpad gestures: three-finger horizontal swipes switch
/// workspaces, with the content following the fingers (swipe left to reveal
/// the workspace on the right).
pub fn default_gestures() -> HashMap<String, Vec<String>> {
    [
        ("workspace_left", vec!["swipe:3:right"]),
        ("workspace_right", vec!["swipe:3:left"]),
    ]
    .into_iter()
    .map(|(name, specs)| {
        (
            name.to_string(),
            specs.into_iter().map(String::from).collect(),
        )
    })
    .collect()
}

/// All action names the compositor understands, for validation and for
/// `ironlandctl`/the settings GUI. Kept next to `default_shortcuts` so the
/// two can't drift.
pub fn known_actions() -> Vec<&'static str> {
    [
        "quit",
        "run_terminal",
        "toggle_launcher",
        "open_browser",
        "open_file_manager",
        "toggle_floating",
        "kill_window",
        "focus_left",
        "focus_right",
        "focus_up",
        "focus_down",
        "swap_left",
        "swap_right",
        "swap_up",
        "swap_down",
        "resize_left",
        "resize_right",
        "resize_up",
        "resize_down",
        "workspace_left",
        "workspace_right",
        "move_workspace_left",
        "move_workspace_right",
        "move_workspace_left_follow",
        "move_workspace_right_follow",
        "move_monitor_left",
        "move_monitor_right",
        "move_monitor_up",
        "move_monitor_down",
        "scale_up",
        "scale_down",
        "toggle_preview",
        "rotate_output",
        "toggle_tint",
        "toggle_decorations",
        "lock_screen",
        "toggle_overview",
    ]
    .to_vec()
}

/// True for an action name of the form `"shortcut:<name>"`, the escape
/// hatch that lets `[shortcuts]` bind a key to an `ironland-shortcuts-v1`
/// name instead of one of the compositor's own fixed [`known_actions`].
/// Kept separate from `known_actions` since the set of valid `<name>`s is
/// whatever a client has registered at runtime, not something this module
/// can enumerate.
pub fn is_shortcut_action(action: &str) -> bool {
    action.starts_with("shortcut:")
}

/// The paths the compositor (and `ironlandctl`) check for a config file, in
/// priority order: an explicit test override, the user's own config, then
/// the system-wide file a NixOS module may have written.
pub fn config_search_path() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(explicit) = env::var("IRONLAND_COMPOSITOR_CONFIG") {
        paths.push(PathBuf::from(explicit));
    }

    if let Some(user_path) = user_config_path() {
        paths.push(user_path);
    }

    paths.push(PathBuf::from("/etc/ironland-compositor/config.toml"));

    paths
}

/// Where a user's own config lives: `$XDG_CONFIG_HOME/ironland-compositor/
/// config.toml`, falling back to `~/.config/...`. This is deliberately the
/// same path regardless of `$IRONLAND_COMPOSITOR_CONFIG` - that variable is
/// an explicit override for reads (mainly for testing), not somewhere a
/// normal user's edits (via `ironlandctl` or the settings GUI) should land.
/// `None` only if neither `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn user_config_path() -> Option<PathBuf> {
    let config_home = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok()?;
    Some(config_home.join("ironland-compositor/config.toml"))
}

impl Config {
    /// Loads settings from the first config file found on
    /// [`config_search_path`], falling back to [`Config::default`] if none
    /// exist or the one found doesn't parse.
    pub fn load() -> Config {
        match Self::try_load() {
            Ok((config, path)) => {
                if let Some(path) = path {
                    tracing::info!(path = %path.display(), "Loaded compositor config");
                }
                config
            }
            Err(err) => {
                tracing::warn!(%err, "Failed to load compositor config, using defaults");
                Config::default()
            }
        }
    }

    /// Reads the current effective config without silently replacing a
    /// malformed file with defaults. Live reload uses this so a temporary
    /// typo never wipes the running configuration; it simply retries after
    /// the next file change. `ironlandctl` also uses this directly, to
    /// surface a parse error instead of silently falling back like
    /// [`Config::load`].
    pub fn try_load() -> Result<(Config, Option<PathBuf>), String> {
        for path in config_search_path() {
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "Failed to read compositor config, skipping it");
                    continue;
                }
            };

            let raw: RawConfig = match toml::from_str(&contents) {
                Ok(raw) => raw,
                Err(err) => {
                    return Err(format!("failed to parse {}: {err}", path.display()));
                }
            };

            let mut shortcuts = default_shortcuts();
            shortcuts.extend(raw.shortcuts);

            let mut gestures = default_gestures();
            gestures.extend(raw.gestures);

            return Ok((
                Config {
                    keyboard: raw.keyboard,
                    terminal: raw.terminal.unwrap_or_else(default_terminal),
                    browser: raw.browser.unwrap_or_else(default_browser),
                    file_manager: raw.file_manager.unwrap_or_else(default_file_manager),
                    top_bar: raw.top_bar,
                    wallpaper: raw.wallpaper,
                    blur: raw.blur,
                    corners: raw.corners,
                    gaps: raw.gaps,
                    border: raw.border,
                    cursor: raw.cursor,
                    focus: raw.focus,
                    lock: raw.lock,
                    performance: raw.performance,
                    key_tracker: raw.key_tracker,
                    shortcuts,
                    gestures,
                    outputs: raw.outputs,
                    workspaces: raw.workspaces,
                    window_animations: raw.window_animations,
                },
                Some(path),
            ));
        }

        Ok((Config::default(), None))
    }

    /// Settings for the output named `name`, or the all-default settings
    /// (auto-placed, extended, not primary) if it isn't configured.
    pub fn output_settings(&self, name: &str) -> OutputSettings {
        self.outputs.get(name).cloned().unwrap_or_default()
    }

    /// Name of the output marked `primary = true`, if any. If more than one
    /// is marked primary, which one wins is unspecified.
    pub fn primary_output_name(&self) -> Option<&str> {
        self.outputs
            .iter()
            .find(|(_, settings)| settings.primary)
            .map(|(name, _)| name.as_str())
    }
}

impl AppearanceSettings {
    /// Reads just the `[appearance]` table from the first config file found
    /// on [`config_search_path`] - kept separate from [`Config::try_load`]
    /// since the compositor's own schema has no notion of this table.
    pub fn load() -> AppearanceSettings {
        for path in config_search_path() {
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct WithAppearance {
                appearance: AppearanceSettings,
            }
            return toml::from_str::<WithAppearance>(&contents)
                .map(|w| w.appearance)
                .unwrap_or_default();
        }
        AppearanceSettings::default()
    }
}

impl FullConfig {
    /// The effective config plus appearance, exactly as `ironlandctl show`
    /// and the settings GUI see it.
    pub fn load() -> FullConfig {
        FullConfig {
            config: Config::load(),
            appearance: AppearanceSettings::load(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_shortcuts_override_only_the_named_action() {
        let mut shortcuts = HashMap::new();
        shortcuts.insert("quit".to_string(), vec!["ctrl+alt+q".to_string()]);
        let raw = RawConfig {
            keyboard: KeyboardSettings::default(),
            terminal: None,
            browser: None,
            file_manager: None,
            top_bar: false,
            wallpaper: None,
            blur: BlurSettings::default(),
            corners: CornersSettings::default(),
            gaps: GapsSettings::default(),
            border: BorderSettings::default(),
            cursor: CursorSettings::default(),
            focus: FocusSettings::default(),
            lock: LockSettings::default(),
            performance: PerformanceSettings::default(),
            key_tracker: KeyTrackerSettings::default(),
            shortcuts,
            gestures: HashMap::new(),
            outputs: HashMap::new(),
            workspaces: WorkspaceSettings::default(),
            window_animations: WindowAnimationSettings::default(),
        };

        let mut merged = default_shortcuts();
        merged.extend(raw.shortcuts);

        assert_eq!(merged["quit"], vec!["ctrl+alt+q"]);
        assert_eq!(merged["toggle_launcher"], vec!["ctrl+space"]);
    }

    #[test]
    fn unknown_tables_are_ignored() {
        // `[appearance]` is written by `ironlandctl`/the settings GUI for
        // their own dark-mode toggle, which the compositor itself has no
        // use for. Serde's default behavior (no `deny_unknown_fields`) is
        // what makes this safe to add there without needing a matching
        // field in `RawConfig`.
        let raw: Result<RawConfig, _> = toml::from_str("[appearance]\ndark_mode = true\n");
        assert!(raw.is_ok(), "unexpected parse error: {:?}", raw.err());
    }

    #[test]
    fn top_bar_defaults_to_disabled_but_can_be_enabled() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert!(!raw.top_bar);

        let raw: RawConfig = toml::from_str("top_bar = true").unwrap();
        assert!(raw.top_bar);
    }

    #[test]
    fn wallpaper_defaults_to_none_but_can_be_set() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.wallpaper, None);

        let raw: RawConfig = toml::from_str(r#"wallpaper = "/home/user/wallpaper.png""#).unwrap();
        assert_eq!(raw.wallpaper.as_deref(), Some("/home/user/wallpaper.png"));
    }

    #[test]
    fn parses_relative_and_absolute_positions() {
        let toml = r#"
            [outputs."HDMI-A-1"]
            position = { right_of = "eDP-1" }

            [outputs."DP-1"]
            position = { x = 100, y = 200 }

            [outputs."eDP-1"]
            primary = true
        "#;
        let raw: RawConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            raw.outputs["HDMI-A-1"].position,
            Some(OutputPosition::RightOf {
                right_of: "eDP-1".to_string()
            })
        );
        assert_eq!(
            raw.outputs["DP-1"].position,
            Some(OutputPosition::Absolute { x: 100, y: 200 })
        );
        assert!(raw.outputs["eDP-1"].primary);
    }

    #[test]
    fn workspace_settings_default_to_split_per_monitor() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.workspaces, WorkspaceSettings::default());
        assert_eq!(raw.workspaces.mode, WorkspaceMode::PerMonitor);
        assert_eq!(raw.workspaces.count, 4);
        assert!(!raw.workspaces.dynamic);
        assert!(raw.workspaces.overlay);
    }

    #[test]
    fn output_refresh_rate_parses_in_millihertz() {
        let raw: RawConfig = toml::from_str("[outputs.DP-1]\nrefresh_rate = 144000\n").unwrap();
        assert_eq!(raw.outputs["DP-1"].refresh_rate, Some(144_000));
    }

    #[test]
    fn blur_defaults_off_and_parses() {
        assert_eq!(
            BlurSettings::default(),
            BlurSettings {
                enabled: false,
                radius: 12
            }
        );
        let raw: RawConfig = toml::from_str("[blur]\nenabled = true\nradius = 20\n").unwrap();
        assert_eq!(
            raw.blur,
            BlurSettings {
                enabled: true,
                radius: 20
            }
        );
    }

    #[test]
    fn window_animations_default_off_and_parse() {
        assert_eq!(
            WindowAnimationSettings::default(),
            WindowAnimationSettings {
                enabled: false,
                duration_ms: 260
            }
        );
        let raw: RawConfig =
            toml::from_str("[window_animations]\nenabled = true\nduration_ms = 400\n").unwrap();
        assert_eq!(
            raw.window_animations,
            WindowAnimationSettings {
                enabled: true,
                duration_ms: 400
            }
        );
    }

    #[test]
    fn corners_default_off_and_parse() {
        assert_eq!(
            CornersSettings::default(),
            CornersSettings {
                enabled: false,
                radius: 12
            }
        );
        let raw: RawConfig = toml::from_str("[corners]\nenabled = true\nradius = 8\n").unwrap();
        assert_eq!(
            raw.corners,
            CornersSettings {
                enabled: true,
                radius: 8
            }
        );
    }

    #[test]
    fn cursor_settings_default_to_none_but_can_be_set() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.cursor, CursorSettings::default());
        assert_eq!(raw.cursor.theme, None);
        assert_eq!(raw.cursor.size, None);

        let raw: RawConfig = toml::from_str("[cursor]\ntheme = \"Adwaita\"\nsize = 32\n").unwrap();
        assert_eq!(raw.cursor.theme.as_deref(), Some("Adwaita"));
        assert_eq!(raw.cursor.size, Some(32));
    }

    #[test]
    fn lock_on_lid_close_defaults_on_and_can_be_disabled() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert!(raw.lock.lock_on_lid_close);
        let raw: RawConfig = toml::from_str("[lock]\nlock_on_lid_close = false").unwrap();
        assert!(!raw.lock.lock_on_lid_close);
    }

    #[test]
    fn focus_settings_default_off_and_parse() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.focus, FocusSettings::default());
        assert!(!raw.focus.follows_mouse);
        assert!(!raw.focus.mouse_follows_focus);

        let raw: RawConfig =
            toml::from_str("[focus]\nfollows_mouse = true\nmouse_follows_focus = true\n").unwrap();
        assert!(raw.focus.follows_mouse);
        assert!(raw.focus.mouse_follows_focus);
    }

    #[test]
    fn workspace_settings_parse_from_config() {
        let toml = r#"
            [workspaces]
            mode = "combined"
            count = 6
            dynamic = true
            overlay = false
        "#;
        let raw: RawConfig = toml::from_str(toml).unwrap();
        assert_eq!(raw.workspaces.mode, WorkspaceMode::Combined);
        assert_eq!(raw.workspaces.count, 6);
        assert!(raw.workspaces.dynamic);
        assert!(!raw.workspaces.overlay);
    }

    #[test]
    fn workspace_transition_axis_defaults_to_horizontal_and_parses() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(
            raw.workspaces.transition_axis,
            WorkspaceTransitionAxis::Horizontal
        );

        let raw: RawConfig =
            toml::from_str("[workspaces]\ntransition_axis = \"vertical\"\n").unwrap();
        assert_eq!(
            raw.workspaces.transition_axis,
            WorkspaceTransitionAxis::Vertical
        );
    }

    #[test]
    fn performance_defaults_off_and_parses() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.performance, PerformanceSettings::default());
        assert!(!raw.performance.fps_overlay);
        assert_eq!(
            raw.performance.fps_overlay_position,
            OverlayPosition::TopRight
        );
        assert_eq!(raw.performance.stutter_threshold_ms, 0.0);
        assert!(raw.performance.stutter_log);
        assert_eq!(raw.performance.fps_overlay_interval_ms, 500);

        let raw: RawConfig = toml::from_str(
            "[performance]\nfps_overlay = true\nfps_overlay_position = \"bottom_left\"\nstutter_threshold_ms = 20.0\nstutter_log = false\nfps_overlay_interval_ms = 1000\n",
        )
        .unwrap();
        assert!(raw.performance.fps_overlay);
        assert_eq!(
            raw.performance.fps_overlay_position,
            OverlayPosition::BottomLeft
        );
        assert_eq!(raw.performance.stutter_threshold_ms, 20.0);
        assert!(!raw.performance.stutter_log);
        assert_eq!(raw.performance.fps_overlay_interval_ms, 1000);
    }

    #[test]
    fn key_tracker_defaults_off_and_parses() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.key_tracker, KeyTrackerSettings::default());
        assert!(!raw.key_tracker.enabled);
        assert_eq!(raw.key_tracker.position, OverlayPosition::TopLeft);

        let raw: RawConfig = toml::from_str(
            "[key_tracker]\nenabled = true\nkeys = [\"Shift_L\"]\nposition = \"bottom_left\"\n",
        )
        .unwrap();
        assert!(raw.key_tracker.enabled);
        assert_eq!(raw.key_tracker.keys, vec!["Shift_L".to_string()]);
        assert_eq!(raw.key_tracker.position, OverlayPosition::BottomLeft);
    }

    #[test]
    fn parses_gesture_specs() {
        assert_eq!(
            parse_gesture("swipe:3:left"),
            Some(Gesture::Swipe {
                fingers: 3,
                direction: SwipeDirection::Left
            })
        );
        assert_eq!(
            parse_gesture(" Pinch:2:IN "),
            Some(Gesture::Pinch {
                fingers: 2,
                zoom_in: true
            })
        );
        assert_eq!(parse_gesture("swipe:2:left"), None);
        assert_eq!(parse_gesture("swipe:3:sideways"), None);
        assert_eq!(parse_gesture("pinch:3:left"), None);
        assert_eq!(parse_gesture("swipe:3"), None);
        assert_eq!(parse_gesture("swipe:3:left:x"), None);
    }

    #[test]
    fn default_gestures_use_known_actions_and_parse() {
        let known = known_actions();
        for (action, specs) in default_gestures() {
            assert!(known.contains(&action.as_str()), "{action}");
            for spec in specs {
                assert!(parse_gesture(&spec).is_some(), "{spec}");
            }
        }
    }

    #[test]
    fn gestures_table_overrides_defaults_per_action() {
        let raw: RawConfig =
            toml::from_str("[gestures]\nworkspace_left = [\"swipe:4:right\"]\n").unwrap();
        let mut merged = default_gestures();
        merged.extend(raw.gestures);
        assert_eq!(merged["workspace_left"], vec!["swipe:4:right"]);
        assert_eq!(merged["workspace_right"], vec!["swipe:3:left"]);
    }

    #[test]
    fn full_config_round_trips_through_json() {
        let full = FullConfig {
            config: Config::default(),
            appearance: AppearanceSettings { dark_mode: true },
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: FullConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(full, back);
    }
}
