package main

import (
	"os/exec"
	"path/filepath"
	"reflect"
	"testing"
)

// requireIronlandctl skips the calling test unless a real `ironlandctl`
// binary is on PATH: loadConfig/saveConfig/defaultConfig now shell out to
// it (see ironlandctl.go), so a round-trip test genuinely needs the real
// thing rather than a Go-side reimplementation of its logic.
func requireIronlandctl(t *testing.T) {
	t.Helper()
	if _, err := exec.LookPath(ironlandctlBinary); err != nil {
		t.Skipf("ironlandctl not found on PATH (%v); build it with `cargo build -p ironlandctl` in ../.. and add it to PATH", err)
	}
}

func TestSplitKeyCombos(t *testing.T) {
	got := splitKeyCombos(" ctrl+q, ctrl+alt+backspace ,, ")
	want := []string{"ctrl+q", "ctrl+alt+backspace"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("splitKeyCombos: got %v, want %v", got, want)
	}
}

func TestSaveThenLoadRoundTrips(t *testing.T) {
	requireIronlandctl(t)
	dir := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", dir)
	t.Setenv("IRONLAND_COMPOSITOR_CONFIG", "")

	cfg := defaultConfig()
	cfg.Keyboard.Layout = "de"
	cfg.Keyboard.Variant = "nodeadkeys"
	cfg.Terminal = "alacritty"
	cfg.Browser = "firefox"
	cfg.FileManager = "nautilus"
	cfg.TopBar = true
	cfg.Appearance.DarkMode = true
	cfg.Blur = BlurSettings{Enabled: true, Radius: 20}
	cfg.Corners = CornersSettings{Enabled: true, Radius: 8}
	cfg.Gaps = GapsSettings{Inner: 12, Outer: 4}
	gradient := "#5555ff"
	cfg.Border = BorderSettings{Enabled: true, Thickness: 6, Color: "#ff5555", GradientColor: &gradient, Angle: 90}
	cfg.Shortcuts["quit"] = []string{"ctrl+alt+q"}
	cfg.Outputs["DP-1"] = OutputSettings{RefreshRate: 144_000}

	path, err := saveConfig(cfg)
	if err != nil {
		t.Fatalf("saveConfig: %v", err)
	}
	if want := filepath.Join(dir, "ironland-compositor", "config.toml"); path != want {
		t.Fatalf("saveConfig path = %q, want %q", path, want)
	}

	loaded, loadedFrom, err := loadConfig()
	if err != nil {
		t.Fatalf("loadConfig: %v", err)
	}
	if loadedFrom != path {
		t.Fatalf("loadConfig loadedFrom = %q, want %q", loadedFrom, path)
	}
	if loaded.Keyboard.Layout != "de" || loaded.Keyboard.Variant != "nodeadkeys" {
		t.Fatalf("loadConfig keyboard = %+v", loaded.Keyboard)
	}
	if loaded.Terminal != "alacritty" {
		t.Fatalf("loadConfig terminal = %q, want alacritty", loaded.Terminal)
	}
	if loaded.Browser != "firefox" {
		t.Fatalf("loadConfig browser = %q, want firefox", loaded.Browser)
	}
	if loaded.FileManager != "nautilus" {
		t.Fatalf("loadConfig fileManager = %q, want nautilus", loaded.FileManager)
	}
	if !loaded.TopBar {
		t.Fatalf("loadConfig topBar = %v, want true", loaded.TopBar)
	}
	if !loaded.Appearance.DarkMode {
		t.Fatalf("loadConfig appearance.darkMode = %v, want true", loaded.Appearance.DarkMode)
	}
	if !loaded.Blur.Enabled || loaded.Blur.Radius != 20 {
		t.Fatalf("loadConfig blur = %+v, want enabled radius 20", loaded.Blur)
	}
	if !loaded.Corners.Enabled || loaded.Corners.Radius != 8 {
		t.Fatalf("loadConfig corners = %+v, want enabled radius 8", loaded.Corners)
	}
	if loaded.Gaps.Inner != 12 || loaded.Gaps.Outer != 4 {
		t.Fatalf("loadConfig gaps = %+v, want inner 12 outer 4", loaded.Gaps)
	}
	if !loaded.Border.Enabled || loaded.Border.Thickness != 6 || loaded.Border.Color != "#ff5555" ||
		loaded.Border.GradientColor == nil || *loaded.Border.GradientColor != "#5555ff" || loaded.Border.Angle != 90 {
		t.Fatalf("loadConfig border = %+v, want enabled thickness 6 color #ff5555 gradient #5555ff angle 90", loaded.Border)
	}
	if !reflect.DeepEqual(loaded.Shortcuts["quit"], []string{"ctrl+alt+q"}) {
		t.Fatalf("loadConfig shortcuts[quit] = %v", loaded.Shortcuts["quit"])
	}
	if loaded.Outputs["DP-1"].RefreshRate != 144_000 {
		t.Fatalf("loadConfig outputs[DP-1].refreshRate = %d", loaded.Outputs["DP-1"].RefreshRate)
	}
	// An action untouched by the override should keep its built-in default.
	if !reflect.DeepEqual(loaded.Shortcuts["toggle_launcher"], []string{"ctrl+space"}) {
		t.Fatalf("loadConfig shortcuts[toggle_launcher] = %v", loaded.Shortcuts["toggle_launcher"])
	}
	if !reflect.DeepEqual(loaded.Shortcuts["shortcut:launcher"], []string{"super"}) {
		t.Fatalf("loadConfig shortcuts[shortcut:launcher] = %v", loaded.Shortcuts["shortcut:launcher"])
	}
}

// The session-restore section has only two switches in the GUI; everything
// else in it must survive a save untouched, or editing any setting would
// quietly reset the user's exclude list and delays.
func TestSessionRestoreSurvivesASave(t *testing.T) {
	requireIronlandctl(t)
	dir := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", dir)
	t.Setenv("IRONLAND_COMPOSITOR_CONFIG", "")

	cfg := defaultConfig()
	if !cfg.SessionRestore.Enabled {
		t.Fatalf("session restore should default to on, got %+v", cfg.SessionRestore)
	}
	cfg.SessionRestore = SessionRestoreSettings{
		Enabled:          false,
		SaveDelayMs:      750,
		RestoreDelayMs:   4000,
		ClaimTimeoutSecs: 20,
		ExcludeAppIDs:    []string{"steam", "Discord"},
		RestoreVMs:       false,
	}
	if _, err := saveConfig(cfg); err != nil {
		t.Fatalf("saveConfig: %v", err)
	}
	loaded, _, err := loadConfig()
	if err != nil {
		t.Fatalf("loadConfig: %v", err)
	}
	if !reflect.DeepEqual(loaded.SessionRestore, cfg.SessionRestore) {
		t.Fatalf("session_restore changed in a save: got %+v, want %+v", loaded.SessionRestore, cfg.SessionRestore)
	}
}

func TestEqualStrings(t *testing.T) {
	for _, test := range []struct {
		name string
		a, b []string
		want bool
	}{
		{name: "equal", a: []string{"super+q", "super+x"}, b: []string{"super+q", "super+x"}, want: true},
		{name: "order matters", a: []string{"super+q", "super+x"}, b: []string{"super+x", "super+q"}},
		{name: "different length", a: []string{"super+q"}, b: []string{"super+q", "super+x"}},
	} {
		t.Run(test.name, func(t *testing.T) {
			if got := equalStrings(test.a, test.b); got != test.want {
				t.Fatalf("equalStrings(%v, %v) = %v, want %v", test.a, test.b, got, test.want)
			}
		})
	}
}

func TestLoadConfigWithNoFileReturnsDefaults(t *testing.T) {
	requireIronlandctl(t)
	dir := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", dir)
	t.Setenv("IRONLAND_COMPOSITOR_CONFIG", "")

	cfg, loadedFrom, err := loadConfig()
	if err != nil {
		t.Fatalf("loadConfig: %v", err)
	}
	if loadedFrom != "" {
		t.Fatalf("loadedFrom = %q, want empty", loadedFrom)
	}
	if !reflect.DeepEqual(cfg, defaultConfig()) {
		t.Fatalf("loadConfig without a file = %+v, want defaults", cfg)
	}
}
