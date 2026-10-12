// Package main: gui-settings' own Config/OutputSettings/etc. types mirror
// `ironland_config::{Config, OutputSettings, ...}` (see
// ../ironland-config/src/lib.rs) field-for-field, purely so the rest of this
// GUI has typed values to bind widgets to. Persistence itself - loading the
// effective config, saving edits - is delegated entirely to `ironlandctl`
// (see ironlandctl.go): this file has no TOML/file-reading code of its own,
// so the schema and the merge-with-defaults logic can't drift between the
// two.
package main

import "encoding/json"

// KeyboardSettings mirrors `ironland_config::KeyboardSettings`: fields are
// passed straight through to xkbcommon, and an empty string means "let
// xkbcommon fall back to its XKB_DEFAULT_* env vars / built-in default".
type KeyboardSettings struct {
	Rules   string `json:"rules"`
	Model   string `json:"model"`
	Layout  string `json:"layout"`
	Variant string `json:"variant"`
	Options string `json:"options"`
	// CapsLock reassigns Caps Lock: "off", "ctrl" or "level3" (AltGr).
	CapsLock string `json:"caps_lock"`
}

// AppearanceSettings is GUI-only state: the compositor itself has no notion
// of a color scheme, so this isn't part of `ironland_config::Config` (its
// TOML parser ignores tables it doesn't know about) - `ironland_config`
// carries it separately, alongside Config, purely so config.toml remembers
// this toggle across runs.
type AppearanceSettings struct {
	DarkMode bool `json:"dark_mode"`
}

type BlurSettings struct {
	Enabled bool `json:"enabled"`
	Radius  int  `json:"radius"`
}

// CornersSettings mirrors `ironland_config::CornersSettings`: rounded
// corners on window content, drawn via a GLES shader.
type CornersSettings struct {
	Enabled bool `json:"enabled"`
	Radius  int  `json:"radius"`
}

// GapsSettings mirrors `ironland_config::GapsSettings`: space between tiled
// windows, and between tiled windows and the output edges.
type GapsSettings struct {
	Inner int `json:"inner"`
	Outer int `json:"outer"`
}

// BorderSettings mirrors `ironland_config::BorderSettings`: a highlight
// border drawn around the currently focused window only. GradientColor
// empty means a solid Color border; set, it's a two-stop gradient from
// Color to GradientColor along Angle degrees.
type BorderSettings struct {
	Enabled       bool    `json:"enabled"`
	Thickness     int     `json:"thickness"`
	Color         string  `json:"color"`
	GradientColor *string `json:"gradient_color"`
	Angle         float64 `json:"angle"`
}

// CursorSettings mirrors `ironland_config::CursorSettings`: an empty Theme,
// or a Size of 0, means "fall back to the XCURSOR_THEME/XCURSOR_SIZE
// environment variables, or the compositor's own built-in default if those
// aren't set either".
type CursorSettings struct {
	Theme string `json:"theme"`
	Size  int    `json:"size"`
}

// WorkspaceSettings mirrors `ironland_config::WorkspaceSettings`: how many
// virtual desktops exist, whether each output gets its own set or every
// output shares one, whether the count grows/shrinks on demand, and whether
// the on-screen dot indicator flashes on switch.
type WorkspaceSettings struct {
	// Mode is either "per_monitor" (each output has its own workspaces) or
	// "combined" (every output shows the same workspace at once).
	Mode    string `json:"mode"`
	Count   int    `json:"count"`
	Dynamic bool   `json:"dynamic"`
	Overlay bool   `json:"overlay"`
	// TransitionMs is how long windows take to slide to/from the next
	// workspace on a switch, in milliseconds. 0 disables the animation.
	TransitionMs int `json:"transition_ms"`
	// TransitionAxis is either "horizontal" (windows slide left/right) or
	// "vertical" (windows slide up/down).
	TransitionAxis string `json:"transition_axis"`
}

// FocusSettings mirrors `ironland_config::FocusSettings`: both default off,
// matching the click-to-focus behavior from before either existed.
type FocusSettings struct {
	// FollowsMouse, if true, focuses whatever window the pointer is over
	// without needing a click (hovering empty space leaves the current
	// focus alone).
	FollowsMouse bool `json:"follows_mouse"`
	// MouseFollowsFocus, if true, warps the pointer to the center of a
	// window whenever it's focused by something other than the pointer
	// itself (switching workspaces, cycling windows, a newly opened
	// window, activating a window from the dock).
	MouseFollowsFocus bool `json:"mouse_follows_focus"`
}

// PerformanceSettings mirrors `ironland_config::PerformanceSettings`: the
// on-screen FPS/frame-time overlay and frame stutter analytics, off by
// default. There is no dedicated GUI tab for these yet - this struct exists
// so a `Config` round-tripped through this GUI (load, edit something else,
// apply) doesn't silently wipe a performance section set via `ironlandctl`
// or the shell's Nexus settings.
type PerformanceSettings struct {
	FPSOverlay bool `json:"fps_overlay"`
	// FPSOverlayPosition is one of "top_left", "top_right", "bottom_left",
	// "bottom_right".
	FPSOverlayPosition string  `json:"fps_overlay_position"`
	StutterThresholdMs float64 `json:"stutter_threshold_ms"`
	StutterLog         bool    `json:"stutter_log"`
	// FPSOverlayIntervalMs is how often the overlay's text is redrawn, not
	// how often frame timing is sampled (that's still every frame).
	FPSOverlayIntervalMs uint32 `json:"fps_overlay_interval_ms"`
}

// KeyTrackerSettings mirrors `ironland_config::KeyTrackerSettings`: the
// stuck-key diagnostic overlay, off by default. Carried here so a `Config`
// round-tripped through this GUI doesn't wipe it (see PerformanceSettings).
type KeyTrackerSettings struct {
	Enabled bool `json:"enabled"`
	// Keys are xkb keysym names, e.g. "Control_L".
	Keys []string `json:"keys"`
	// Position is one of "top_left", "top_right", "bottom_left",
	// "bottom_right".
	Position string `json:"position"`
}

// Config mirrors `ironland_config::FullConfig` field-for-field (its own
// `#[serde(flatten)]` on the inner `Config` puts these fields at the top
// level, alongside Appearance) - see `ironlandctl show --json`.
type Config struct {
	Keyboard    KeyboardSettings `json:"keyboard"`
	Terminal    string           `json:"terminal"`
	Browser     string           `json:"browser"`
	FileManager string           `json:"file_manager"`
	// TopBar controls whether windows may get a compositor-drawn header
	// bar for server-side decoration. Off by default: a client's request
	// for server-side decoration is overridden back to client-side.
	TopBar bool `json:"top_bar"`
	// Wallpaper is a path to an image file (PNG/JPEG/WebP) used as the
	// desktop background, scaled and center-cropped to cover each output.
	// Empty uses the compositor's built-in default wallpaper.
	Wallpaper   string                    `json:"wallpaper"`
	Blur        BlurSettings              `json:"blur"`
	Corners     CornersSettings           `json:"corners"`
	Gaps        GapsSettings              `json:"gaps"`
	Border      BorderSettings            `json:"border"`
	Cursor      CursorSettings            `json:"cursor"`
	Focus       FocusSettings             `json:"focus"`
	Performance PerformanceSettings       `json:"performance"`
	KeyTracker  KeyTrackerSettings        `json:"key_tracker"`
	Appearance  AppearanceSettings        `json:"appearance"`
	Shortcuts   map[string][]string       `json:"shortcuts"`
	Gestures    map[string][]string       `json:"gestures"` // no GUI tab yet; carried through so saving doesn't drop it
	Outputs     map[string]OutputSettings `json:"outputs"`
	Workspaces  WorkspaceSettings         `json:"workspaces"`
}

// OutputPosition mirrors `ironland_config::OutputPosition`: exactly one of
// RightOf/LeftOf/Above/Below (each another output's connector name) or X/Y
// (an absolute logical position) should be set. It's kept flat rather than
// as a Go-level tagged union because that's how it round-trips through JSON
// into Rust's `#[serde(untagged)]` enum: only the keys present in the
// object matter, and `omitempty` keeps the others out.
type OutputPosition struct {
	RightOf string `json:"right_of,omitempty"`
	LeftOf  string `json:"left_of,omitempty"`
	Above   string `json:"above,omitempty"`
	Below   string `json:"below,omitempty"`
	X       *int   `json:"x,omitempty"`
	Y       *int   `json:"y,omitempty"`
}

// OutputSettings mirrors `ironland_config::OutputSettings`, keyed by
// connector name (e.g. "eDP-1", "HDMI-A-1") in Config.Outputs.
type OutputSettings struct {
	Primary     bool            `json:"primary,omitempty"`
	RefreshRate int             `json:"refresh_rate,omitempty"`
	MirrorOf    string          `json:"mirror_of,omitempty"`
	Position    *OutputPosition `json:"position,omitempty"`
}

// MarshalJSON round-trips RefreshRate/MirrorOf/Position as JSON
// null/absent rather than Go's zero values, matching what `ironlandctl`
// expects for "no override" (its own Option<..> fields serialize the same
// way) - encoding/json's `omitempty` alone can't do this for a 0 that's a
// meaningful value elsewhere, so this is spelled out explicitly.
func (s OutputSettings) MarshalJSON() ([]byte, error) {
	type wire struct {
		Primary     bool            `json:"primary"`
		RefreshRate *int            `json:"refresh_rate"`
		MirrorOf    *string         `json:"mirror_of"`
		Position    *OutputPosition `json:"position"`
	}
	w := wire{Primary: s.Primary, Position: s.Position}
	if s.RefreshRate != 0 {
		w.RefreshRate = &s.RefreshRate
	}
	if s.MirrorOf != "" {
		w.MirrorOf = &s.MirrorOf
	}
	return json.Marshal(w)
}

// knownActions lists every action the compositor recognizes in
// [shortcuts], in the same order as `config::known_actions` in
// src/config.rs. Keep the two in sync: an action missing here just won't
// be editable in the GUI, and one missing there is silently ignored by the
// compositor with a warning.
var knownActions = []string{
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
	"scale_up",
	"scale_down",
	"toggle_preview",
	"rotate_output",
	"toggle_tint",
	"toggle_decorations",
}

// actionLabels gives each action a human-readable name for the GUI.
var actionLabels = map[string]string{
	"quit":                        "Quit compositor",
	"run_terminal":                "Open terminal",
	"toggle_launcher":             "Toggle app launcher",
	"open_browser":                "Open browser",
	"open_file_manager":           "Open file manager",
	"toggle_floating":             "Toggle floating/tiled",
	"kill_window":                 "Kill active window",
	"focus_left":                  "Focus window: left",
	"focus_right":                 "Focus window: right",
	"focus_up":                    "Focus window: up",
	"focus_down":                  "Focus window: down",
	"swap_left":                   "Swap window: left",
	"swap_right":                  "Swap window: right",
	"swap_up":                     "Swap window: up",
	"swap_down":                   "Swap window: down",
	"resize_left":                 "Resize tiled window: left",
	"resize_right":                "Resize tiled window: right",
	"resize_up":                   "Resize tiled window: up",
	"resize_down":                 "Resize tiled window: down",
	"workspace_left":              "Switch workspace: previous",
	"workspace_right":             "Switch workspace: next",
	"move_workspace_left":         "Move window to workspace: previous",
	"move_workspace_right":        "Move window to workspace: next",
	"move_workspace_left_follow":  "Move window to workspace and follow: previous",
	"move_workspace_right_follow": "Move window to workspace and follow: next",
	"scale_up":                    "Increase output scale",
	"scale_down":                  "Decrease output scale",
	"toggle_preview":              "Toggle window preview",
	"rotate_output":               "Rotate output",
	"toggle_tint":                 "Toggle debug tint",
	"toggle_decorations":          "Toggle window decorations",
}

// advertisedEvent describes one named shortcut a client can register via
// `ironland-shortcuts-v1`'s `get_shortcut` request (the `shortcut:<name>`
// action escape hatch - see `config::is_shortcut_action` in src/config.rs).
// This list is curated from caelestia-shell's `modules/Shortcuts.qml`
// (the only client this GUI ships alongside), so it can drift if that
// file's `CustomShortcut { name: ... }` entries change; a name that isn't
// listed here can still be bound by typing it into "Add a custom event"
// below, it just won't show up as a suggestion.
type advertisedEvent struct {
	Name  string
	Label string
}

var advertisedEvents = []advertisedEvent{
	{"launcher", "Shell: toggle launcher"},
	{"showall", "Shell: toggle launcher/dashboard/OSD"},
	{"dashboard", "Shell: toggle dashboard"},
	{"session", "Shell: toggle session menu"},
	{"sidebar", "Shell: toggle sidebar"},
	{"utilities", "Shell: toggle utilities"},
	{"nexus", "Shell: open nexus"},
	{"launcherInterrupt", "Shell: interrupt launcher keybind"},
}

func advertisedEventLabel(name string) string {
	for _, e := range advertisedEvents {
		if e.Name == name {
			return e.Label
		}
	}
	return "Shell: " + name
}

// defaultShortcuts is the baseline the compositor falls back to for any
// action not overridden in the config file. Fetched from `ironlandctl`
// (see ironlandctl.go) rather than hardcoded here, so it can't drift from
// `ironland_config::default_shortcuts`.
func defaultShortcuts() map[string][]string {
	return defaultConfig().Shortcuts
}

func defaultWorkspaceSettings() WorkspaceSettings {
	return defaultConfig().Workspaces
}

// defaultConfig returns ironlandctl's built-in defaults (`ironlandctl
// defaults --json`). Each settings tab asks for its own copy at build time,
// purely to compute its "differs from default" reset affordances - a fresh
// call each time (rather than a cached one) sidesteps having to deep-copy
// Config's maps before handing them to a caller that mutates its own copy.
func defaultConfig() Config {
	cfg, err := ironlandctlDefaults()
	if err != nil {
		// No sensible fallback here that wouldn't reintroduce the exact
		// schema duplication this delegation is meant to avoid; an empty
		// Config at least keeps every widget's zero-value/maps non-nil so
		// the GUI doesn't panic before the caller's own error dialog (see
		// loadConfig/saveConfig) is shown.
		return Config{Shortcuts: map[string][]string{}, Outputs: map[string]OutputSettings{}}
	}
	return cfg
}

// loadConfig returns the settings that would be active if the compositor
// started right now (`ironlandctl show --json`: the first config file on
// its search path, merged over the built-in defaults), plus a
// human-readable description of where that came from - or a non-nil error
// if `ironlandctl` itself couldn't be run at all (not installed, not on
// PATH), which the caller should surface to the user since there's no
// sensible settings to show otherwise.
func loadConfig() (Config, string, error) {
	cfg, err := ironlandctlShow()
	if err != nil {
		return defaultConfig(), "", err
	}
	loadedFrom, pathErr := ironlandctlActivePath()
	if pathErr != nil || loadedFrom == "" {
		return cfg, "", nil
	}
	return cfg, loadedFrom, nil
}

// saveConfig hands cfg to `ironlandctl apply`, which atomically writes it
// to the user's config file (creating its parent directory if needed) and
// reports the path it wrote to.
func saveConfig(cfg Config) (string, error) {
	return ironlandctlApply(cfg)
}
