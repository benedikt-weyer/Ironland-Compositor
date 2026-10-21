// Command gui-settings is a small Fyne GUI for ironland-compositor's TOML
// settings file (see src/config.rs in the main crate for the schema this
// mirrors). It edits the user's own config file at
// $XDG_CONFIG_HOME/ironland-compositor/config.toml (falling back to
// ~/.config/...), which the compositor watches ahead of the system-wide file
// a NixOS module may have written to /etc. Saved changes apply live.
package main

import (
	"fmt"
	"sort"
	"strings"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/app"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/dialog"
	"fyne.io/fyne/v2/storage"
	"fyne.io/fyne/v2/theme"
	"fyne.io/fyne/v2/widget"
)

func main() {
	a := app.NewWithID("dev.ironland.compositor-settings")
	w := a.NewWindow("ironland-compositor Settings")

	cfg, loadedFrom, err := loadConfig()
	if err != nil {
		dialog.ShowError(fmt.Errorf("loading settings via ironlandctl: %w\n\nIs ironlandctl installed and on PATH?", err), w)
	}

	keyboardTab := buildKeyboardTab(&cfg)
	shortcutsTab := buildShortcutsTab(&cfg)
	outputsTab := buildOutputsTab(&cfg, w)
	workspacesTab := buildWorkspacesTab(&cfg)
	focusTab := buildFocusTab(&cfg)
	sessionTab := buildSessionTab(&cfg)
	vmTab := buildVMTab(&cfg)
	appearanceTab := buildAppearanceTab(&cfg, w)

	status := widget.NewLabel(statusText(loadedFrom))
	status.Wrapping = fyne.TextWrapWord

	saveButton := widget.NewButtonWithIcon("Save", nil, func() {
		path, err := saveConfig(cfg)
		if err != nil {
			dialog.ShowError(fmt.Errorf("saving settings: %w", err), w)
			return
		}
		status.SetText(fmt.Sprintf("Saved to %s. Changes applied live.", path))
	})
	saveButton.Importance = widget.HighImportance

	tabs := container.NewAppTabs(
		container.NewTabItem("Keyboard", keyboardTab),
		container.NewTabItem("Shortcuts", shortcutsTab),
		container.NewTabItem("Monitors", outputsTab),
		container.NewTabItem("Workspaces", workspacesTab),
		container.NewTabItem("Focus", focusTab),
		container.NewTabItem("Session", sessionTab),
		container.NewTabItem("Virtual machines", vmTab),
		container.NewTabItem("Appearance", appearanceTab),
	)

	content := container.NewBorder(nil, container.NewVBox(status, saveButton), nil, nil, tabs)
	w.SetContent(content)
	w.SetFixedSize(false)
	w.Resize(fyne.NewSize(640, 520))
	w.ShowAndRun()
}

func statusText(loadedFrom string) string {
	if loadedFrom == "" {
		return "No config file found yet; showing built-in defaults."
	}
	return fmt.Sprintf("Loaded from %s", loadedFrom)
}

// buildKeyboardTab lays out entries for the keyboard layout fields and the
// terminal command, writing every edit straight back into cfg so Save
// always has the current values.
func buildKeyboardTab(cfg *Config) fyne.CanvasObject {
	defaults := defaultConfig()
	resets := newResetGroup()

	layout := widget.NewEntry()
	layout.SetText(cfg.Keyboard.Layout)
	layout.SetPlaceHolder("e.g. us, de, gb (empty = system default)")
	layout.OnChanged = func(s string) { cfg.Keyboard.Layout = s; resets.refresh() }

	variant := widget.NewEntry()
	variant.SetText(cfg.Keyboard.Variant)
	variant.SetPlaceHolder("e.g. nodeadkeys, dvorak (optional)")
	variant.OnChanged = func(s string) { cfg.Keyboard.Variant = s; resets.refresh() }

	model := widget.NewEntry()
	model.SetText(cfg.Keyboard.Model)
	model.SetPlaceHolder("e.g. pc105 (optional)")
	model.OnChanged = func(s string) { cfg.Keyboard.Model = s; resets.refresh() }

	options := widget.NewEntry()
	options.SetText(cfg.Keyboard.Options)
	options.SetPlaceHolder("e.g. caps:swapescape (optional)")
	options.OnChanged = func(s string) { cfg.Keyboard.Options = s; resets.refresh() }

	capsLabels := map[string]string{
		"off":    "Normal Caps Lock",
		"ctrl":   "Ctrl",
		"level3": "Level 3 shift (AltGr)",
	}
	capsSelect := widget.NewSelect(
		[]string{capsLabels["off"], capsLabels["ctrl"], capsLabels["level3"]},
		func(selected string) {
			for value, label := range capsLabels {
				if label == selected {
					cfg.Keyboard.CapsLock = value
				}
			}
			resets.refresh()
		},
	)
	capsSelect.SetSelected(capsLabels[cfg.Keyboard.CapsLock])

	rules := widget.NewEntry()
	rules.SetText(cfg.Keyboard.Rules)
	rules.SetPlaceHolder("advanced: xkb rules file (usually leave empty)")
	rules.OnChanged = func(s string) { cfg.Keyboard.Rules = s; resets.refresh() }

	terminal := widget.NewEntry()
	terminal.SetText(cfg.Terminal)
	terminal.SetPlaceHolder("command run by the \"Open terminal\" shortcut")
	terminal.OnChanged = func(s string) { cfg.Terminal = s; resets.refresh() }

	browser := widget.NewEntry()
	browser.SetText(cfg.Browser)
	browser.SetPlaceHolder("command run by the \"Open browser\" shortcut")
	browser.OnChanged = func(s string) { cfg.Browser = s; resets.refresh() }

	fileManager := widget.NewEntry()
	fileManager.SetText(cfg.FileManager)
	fileManager.SetPlaceHolder("command run by the \"Open file manager\" shortcut")
	fileManager.OnChanged = func(s string) { cfg.FileManager = s; resets.refresh() }

	topBar := widget.NewCheck("", func(checked bool) { cfg.TopBar = checked; resets.refresh() })
	topBar.SetChecked(cfg.TopBar)

	lockOnLid := widget.NewCheck("", func(checked bool) { cfg.Lock.LockOnLidClose = checked; resets.refresh() })
	lockOnLid.SetChecked(cfg.Lock.LockOnLidClose)

	form := widget.NewForm(
		widget.NewFormItem("Layout", resets.item(layout, func() bool { return cfg.Keyboard.Layout != defaults.Keyboard.Layout }, func() { layout.SetText(defaults.Keyboard.Layout) })),
		widget.NewFormItem("Variant", resets.item(variant, func() bool { return cfg.Keyboard.Variant != defaults.Keyboard.Variant }, func() { variant.SetText(defaults.Keyboard.Variant) })),
		widget.NewFormItem("Model", resets.item(model, func() bool { return cfg.Keyboard.Model != defaults.Keyboard.Model }, func() { model.SetText(defaults.Keyboard.Model) })),
		widget.NewFormItem("Options", resets.item(options, func() bool { return cfg.Keyboard.Options != defaults.Keyboard.Options }, func() { options.SetText(defaults.Keyboard.Options) })),
		widget.NewFormItem("Caps Lock acts as", resets.item(capsSelect, func() bool { return cfg.Keyboard.CapsLock != defaults.Keyboard.CapsLock }, func() { capsSelect.SetSelected(capsLabels[defaults.Keyboard.CapsLock]) })),
		widget.NewFormItem("Rules", resets.item(rules, func() bool { return cfg.Keyboard.Rules != defaults.Keyboard.Rules }, func() { rules.SetText(defaults.Keyboard.Rules) })),
		widget.NewFormItem("Terminal command", resets.item(terminal, func() bool { return cfg.Terminal != defaults.Terminal }, func() { terminal.SetText(defaults.Terminal) })),
		widget.NewFormItem("Browser command", resets.item(browser, func() bool { return cfg.Browser != defaults.Browser }, func() { browser.SetText(defaults.Browser) })),
		widget.NewFormItem("File manager command", resets.item(fileManager, func() bool { return cfg.FileManager != defaults.FileManager }, func() { fileManager.SetText(defaults.FileManager) })),
		widget.NewFormItem("Show top bar", resets.item(topBar, func() bool { return cfg.TopBar != defaults.TopBar }, func() { topBar.SetChecked(defaults.TopBar) })),
		widget.NewFormItem("Lock on lid close", resets.item(lockOnLid, func() bool { return cfg.Lock.LockOnLidClose != defaults.Lock.LockOnLidClose }, func() { lockOnLid.SetChecked(defaults.Lock.LockOnLidClose) })),
	)

	hint := widget.NewLabel("Keyboard fields are passed straight to xkbcommon; leave a field empty to use its system default. The top bar is the compositor-drawn window header/title bar; it's off by default, so windows get no server-side decoration no matter what they request.")
	hint.Wrapping = fyne.TextWrapWord

	return resets.page(container.NewVBox(form, hint))
}

// buildShortcutsTab lays out one editable row per known action, each
// holding its bound key combos as a comma-separated list (e.g.
// "super+left, super+kp_left"), followed by a second section for binding
// keys to named events a client advertises via `ironland-shortcuts-v1`
// (the `shortcut:<name>` action - see `advertisedEvents`), which can be
// added and removed freely since there's no fixed list of them.
func buildShortcutsTab(cfg *Config) fyne.CanvasObject {
	defaults := defaultShortcuts()
	resets := newResetGroup()
	form := widget.NewForm()

	for _, action := range knownActions {
		action := action // capture for the closure below
		entry := widget.NewEntry()
		entry.SetText(strings.Join(cfg.Shortcuts[action], ", "))
		entry.SetPlaceHolder("e.g. super+shift+q")
		entry.OnChanged = func(s string) {
			cfg.Shortcuts[action] = splitKeyCombos(s)
			resets.refresh()
		}

		label := actionLabels[action]
		if label == "" {
			label = action
		}
		form.Append(label, resets.item(entry,
			func() bool { return !equalStrings(cfg.Shortcuts[action], defaults[action]) },
			func() { entry.SetText(strings.Join(defaults[action], ", ")) },
		))
	}

	hint := widget.NewLabel("Separate multiple key combos for the same action with commas. A combo is modifiers and a key joined with '+', e.g. \"super+alt+left\".")
	hint.Wrapping = fyne.TextWrapWord

	eventsSection := buildShellEventsSection(cfg, resets)

	return resets.page(container.NewVBox(form, hint, widget.NewSeparator(), eventsSection))
}

// buildShellEventsSection is the "Shell events" part of the Shortcuts tab:
// one removable row per `shortcut:<name>` entry already in cfg.Shortcuts
// (pre-populated from the loaded config file, so a hand-edited entry stays
// visible), plus controls to add another - either picked from
// advertisedEvents or typed freely, for an event a future shell version
// advertises that isn't in that curated list yet.
func buildShellEventsSection(cfg *Config, resets *resetGroup) fyne.CanvasObject {
	eventsForm := widget.NewForm()
	var eventNames []string

	for action := range cfg.Shortcuts {
		if name, ok := strings.CutPrefix(action, "shortcut:"); ok {
			eventNames = append(eventNames, name)
		}
	}
	sort.Strings(eventNames)

	var rebuild func()
	addEventRow := func(name string) {
		for _, existing := range eventNames {
			if existing == name {
				return
			}
		}
		eventNames = append(eventNames, name)
		sort.Strings(eventNames)
		rebuild()
	}
	removeEventRow := func(name string) {
		// Set to an explicit empty list rather than deleting the map key:
		// deleting would just let the compositor's own built-in default
		// (see `defaultShortcuts`) show back through on next load, for any
		// event name - "launcher" today - that has one.
		cfg.Shortcuts["shortcut:"+name] = []string{}
		filtered := eventNames[:0]
		for _, existing := range eventNames {
			if existing != name {
				filtered = append(filtered, existing)
			}
		}
		eventNames = filtered
		resets.refresh()
		rebuild()
	}

	rebuild = func() {
		eventsForm.Items = nil
		for _, name := range eventNames {
			name := name // capture for the closures below
			action := "shortcut:" + name

			entry := widget.NewEntry()
			entry.SetText(strings.Join(cfg.Shortcuts[action], ", "))
			entry.SetPlaceHolder("e.g. super+g")
			entry.OnChanged = func(s string) {
				cfg.Shortcuts[action] = splitKeyCombos(s)
				resets.refresh()
			}

			remove := widget.NewButtonWithIcon("", theme.DeleteIcon(), func() { removeEventRow(name) })
			row := container.NewBorder(nil, nil, nil, remove, entry)

			eventsForm.Append(advertisedEventLabel(name), row)
		}
		eventsForm.Refresh()
	}
	rebuild()

	suggestionNames := func() []string {
		var names []string
		for _, e := range advertisedEvents {
			found := false
			for _, existing := range eventNames {
				if existing == e.Name {
					found = true
					break
				}
			}
			if !found {
				names = append(names, e.Name)
			}
		}
		return names
	}

	suggestions := widget.NewSelect(suggestionNames(), nil)
	suggestions.PlaceHolder = "Pick an advertised event…"
	addSuggested := widget.NewButton("Add", func() {
		if suggestions.Selected == "" {
			return
		}
		addEventRow(suggestions.Selected)
		suggestions.SetOptions(suggestionNames())
		suggestions.ClearSelected()
	})

	custom := widget.NewEntry()
	custom.SetPlaceHolder("Custom event name (e.g. one a third-party client advertises)")
	addCustom := widget.NewButton("Add", func() {
		name := strings.TrimSpace(custom.Text)
		if name == "" {
			return
		}
		addEventRow(name)
		suggestions.SetOptions(suggestionNames())
		custom.SetText("")
	})

	hint := widget.NewLabel(
		"Binds a key to a named shortcut a client requests over ironland-shortcuts-v1 (get_shortcut)" +
			" rather than one of the compositor's own actions above - caelestia-shell registers the" +
			" ones suggested here (see its modules/Shortcuts.qml). A name with no client currently" +
			" listening for it simply never fires.",
	)
	hint.Wrapping = fyne.TextWrapWord

	return container.NewVBox(
		widget.NewLabelWithStyle("Shell events", fyne.TextAlignLeading, fyne.TextStyle{Bold: true}),
		hint,
		eventsForm,
		container.NewBorder(nil, nil, nil, addSuggested, suggestions),
		container.NewBorder(nil, nil, nil, addCustom, custom),
	)
}

// buildAppearanceTab holds the dark/light mode toggle. Unlike the other
// tabs, flipping it takes effect immediately (via setSystemColorScheme)
// rather than waiting for Save, since it's a live system-wide preference and
// not just something the compositor reads back at its next start.
func buildAppearanceTab(cfg *Config, w fyne.Window) fyne.CanvasObject {
	defaults := defaultConfig()
	resets := newResetGroup()
	// Set the loaded value directly (not via SetChecked, which fires
	// OnChanged on any change from the zero value) so opening the window
	// doesn't itself push a live color-scheme change before the user has
	// touched anything.
	darkMode := widget.NewCheck("Dark mode", nil)
	darkMode.Checked = cfg.Appearance.DarkMode
	darkMode.OnChanged = func(checked bool) {
		cfg.Appearance.DarkMode = checked
		resets.refresh()
		if err := setSystemColorScheme(checked); err != nil {
			dialog.ShowError(fmt.Errorf("setting system color scheme: %w", err), w)
		}
	}

	colorHint := widget.NewLabel(
		"Applies immediately (and is also saved to config.toml so this toggle remembers its state)." +
			" This sets the same GNOME setting that xdg-desktop-portal-gtk exposes as the org.freedesktop.appearance color scheme," +
			" so portal-aware apps pick it up too; it has no effect without xdg-desktop-portal-gtk (or gsettings) installed.",
	)
	colorHint.Wrapping = fyne.TextWrapWord

	wallpaper := widget.NewEntry()
	wallpaper.SetText(cfg.Wallpaper)
	wallpaper.SetPlaceHolder("empty = built-in default wallpaper")
	wallpaper.OnChanged = func(s string) { cfg.Wallpaper = s; resets.refresh() }

	browseButton := widget.NewButton("Browse…", func() {
		picker := dialog.NewFileOpen(func(reader fyne.URIReadCloser, err error) {
			if err != nil || reader == nil {
				return
			}
			defer reader.Close()
			path := reader.URI().Path()
			wallpaper.SetText(path)
			cfg.Wallpaper = path
		}, w)
		picker.SetFilter(storage.NewExtensionFileFilter([]string{".png", ".jpg", ".jpeg", ".webp"}))
		picker.Show()
	})

	wallpaperHint := widget.NewLabel(
		"Path to an image file (PNG/JPEG/WebP) used as the desktop background, scaled and center-cropped to" +
			" cover each output. Changes apply when saved.",
	)
	wallpaperHint.Wrapping = fyne.TextWrapWord

	blurEnabled := widget.NewCheck("Blur behind translucent application windows", nil)
	blurEnabled.SetChecked(cfg.Blur.Enabled)
	blurEnabled.OnChanged = func(checked bool) {
		cfg.Blur.Enabled = checked
		resets.refresh()
	}
	blurRadius := widget.NewEntry()
	blurRadius.SetText(fmt.Sprintf("%d", cfg.Blur.Radius))
	blurRadius.SetPlaceHolder("1–50")
	blurRadius.OnChanged = func(value string) {
		var radius int
		if _, err := fmt.Sscanf(value, "%d", &radius); err == nil && radius >= 1 && radius <= 50 {
			cfg.Blur.Radius = radius
		}
		resets.refresh()
	}
	blurForm := widget.NewForm(
		widget.NewFormItem("Enable blur", resets.item(blurEnabled,
			func() bool { return cfg.Blur.Enabled != defaults.Blur.Enabled },
			func() { blurEnabled.SetChecked(defaults.Blur.Enabled) },
		)),
		widget.NewFormItem("Blur radius", resets.item(blurRadius,
			func() bool { return cfg.Blur.Radius != defaults.Blur.Radius },
			func() { blurRadius.SetText(fmt.Sprintf("%d", defaults.Blur.Radius)) },
		)),
	)
	blurHint := widget.NewLabel("Gaussian blur is visible through transparent and translucent parts of application windows. Higher radii are smoother but require more work when the wallpaper changes or an output is resized. Changes apply when saved.")
	blurHint.Wrapping = fyne.TextWrapWord

	cornersEnabled := widget.NewCheck("Round window corners", nil)
	cornersEnabled.SetChecked(cfg.Corners.Enabled)
	cornersEnabled.OnChanged = func(checked bool) {
		cfg.Corners.Enabled = checked
		resets.refresh()
	}
	cornersRadius := widget.NewEntry()
	cornersRadius.SetText(fmt.Sprintf("%d", cfg.Corners.Radius))
	cornersRadius.SetPlaceHolder("1–50")
	cornersRadius.OnChanged = func(value string) {
		var radius int
		if _, err := fmt.Sscanf(value, "%d", &radius); err == nil && radius >= 1 && radius <= 50 {
			cfg.Corners.Radius = radius
		}
		resets.refresh()
	}
	cornersForm := widget.NewForm(
		widget.NewFormItem("Enable rounded corners", resets.item(cornersEnabled,
			func() bool { return cfg.Corners.Enabled != defaults.Corners.Enabled },
			func() { cornersEnabled.SetChecked(defaults.Corners.Enabled) },
		)),
		widget.NewFormItem("Corner radius", resets.item(cornersRadius,
			func() bool { return cfg.Corners.Radius != defaults.Corners.Radius },
			func() { cornersRadius.SetText(fmt.Sprintf("%d", defaults.Corners.Radius)) },
		)),
	)
	cornersHint := widget.NewLabel("Rounds the corners of window content in logical pixels, drawn on the GPU. Changes apply when saved.")
	cornersHint.Wrapping = fyne.TextWrapWord

	gapsInner := widget.NewEntry()
	gapsInner.SetText(fmt.Sprintf("%d", cfg.Gaps.Inner))
	gapsInner.OnChanged = func(value string) {
		var gap int
		if _, err := fmt.Sscanf(value, "%d", &gap); err == nil && gap >= 0 {
			cfg.Gaps.Inner = gap
		}
		resets.refresh()
	}
	gapsOuter := widget.NewEntry()
	gapsOuter.SetText(fmt.Sprintf("%d", cfg.Gaps.Outer))
	gapsOuter.OnChanged = func(value string) {
		var gap int
		if _, err := fmt.Sscanf(value, "%d", &gap); err == nil && gap >= 0 {
			cfg.Gaps.Outer = gap
		}
		resets.refresh()
	}
	gapsForm := widget.NewForm(
		widget.NewFormItem("Gap between windows", resets.item(gapsInner,
			func() bool { return cfg.Gaps.Inner != defaults.Gaps.Inner },
			func() { gapsInner.SetText(fmt.Sprintf("%d", defaults.Gaps.Inner)) },
		)),
		widget.NewFormItem("Gap to screen edge", resets.item(gapsOuter,
			func() bool { return cfg.Gaps.Outer != defaults.Gaps.Outer },
			func() { gapsOuter.SetText(fmt.Sprintf("%d", defaults.Gaps.Outer)) },
		)),
	)
	gapsHint := widget.NewLabel("Space, in logical pixels, between tiled windows and between them and the output edges. Changes apply when saved.")
	gapsHint.Wrapping = fyne.TextWrapWord

	borderEnabled := widget.NewCheck("Highlight the focused window", nil)
	borderEnabled.SetChecked(cfg.Border.Enabled)
	borderEnabled.OnChanged = func(checked bool) {
		cfg.Border.Enabled = checked
		resets.refresh()
	}
	borderThickness := widget.NewEntry()
	borderThickness.SetText(fmt.Sprintf("%d", cfg.Border.Thickness))
	borderThickness.SetPlaceHolder("0–32")
	borderThickness.OnChanged = func(value string) {
		var thickness int
		if _, err := fmt.Sscanf(value, "%d", &thickness); err == nil && thickness >= 0 && thickness <= 32 {
			cfg.Border.Thickness = thickness
		}
		resets.refresh()
	}
	borderColor := widget.NewEntry()
	borderColor.SetText(cfg.Border.Color)
	borderColor.SetPlaceHolder("#rrggbb or #rrggbbaa")
	borderColor.OnChanged = func(value string) { cfg.Border.Color = value; resets.refresh() }
	borderGradientColor := widget.NewEntry()
	borderGradientColor.SetText(formatOptionalColor(cfg.Border.GradientColor))
	borderGradientColor.SetPlaceHolder("empty = solid colour")
	borderGradientColor.OnChanged = func(value string) {
		if value == "" {
			cfg.Border.GradientColor = nil
		} else {
			v := value
			cfg.Border.GradientColor = &v
		}
		resets.refresh()
	}
	borderAngle := widget.NewEntry()
	borderAngle.SetText(fmt.Sprintf("%g", cfg.Border.Angle))
	borderAngle.OnChanged = func(value string) {
		var angle float64
		if _, err := fmt.Sscanf(value, "%g", &angle); err == nil && angle >= 0 && angle < 360 {
			cfg.Border.Angle = angle
		}
		resets.refresh()
	}
	borderForm := widget.NewForm(
		widget.NewFormItem("Enable focus border", resets.item(borderEnabled,
			func() bool { return cfg.Border.Enabled != defaults.Border.Enabled },
			func() { borderEnabled.SetChecked(defaults.Border.Enabled) },
		)),
		widget.NewFormItem("Border thickness", resets.item(borderThickness,
			func() bool { return cfg.Border.Thickness != defaults.Border.Thickness },
			func() { borderThickness.SetText(fmt.Sprintf("%d", defaults.Border.Thickness)) },
		)),
		widget.NewFormItem("Colour", resets.item(borderColor,
			func() bool { return cfg.Border.Color != defaults.Border.Color },
			func() { borderColor.SetText(defaults.Border.Color) },
		)),
		widget.NewFormItem("Gradient colour", resets.item(borderGradientColor,
			func() bool {
				return formatOptionalColor(cfg.Border.GradientColor) != formatOptionalColor(defaults.Border.GradientColor)
			},
			func() { borderGradientColor.SetText(formatOptionalColor(defaults.Border.GradientColor)) },
		)),
		widget.NewFormItem("Gradient angle", resets.item(borderAngle,
			func() bool { return cfg.Border.Angle != defaults.Border.Angle },
			func() { borderAngle.SetText(fmt.Sprintf("%g", defaults.Border.Angle)) },
		)),
	)
	borderHint := widget.NewLabel(
		"Draws a highlight border around whichever window currently has keyboard focus. Leave \"Gradient" +
			" colour\" empty for a solid border, or set it for a two-colour gradient along \"Gradient angle\"" +
			" degrees. Changes apply when saved.",
	)
	borderHint.Wrapping = fyne.TextWrapWord

	cursorTheme := widget.NewEntry()
	cursorTheme.SetText(cfg.Cursor.Theme)
	cursorTheme.SetPlaceHolder("e.g. Adwaita, Bibata-Modern-Classic (empty = system default)")
	cursorTheme.OnChanged = func(s string) { cfg.Cursor.Theme = s; resets.refresh() }

	cursorSize := widget.NewEntry()
	cursorSize.SetText(formatCursorSize(cfg.Cursor.Size))
	cursorSize.SetPlaceHolder("empty = system default (usually 24)")
	cursorSize.OnChanged = func(value string) {
		if value == "" {
			cfg.Cursor.Size = 0
			resets.refresh()
			return
		}
		var size int
		if _, err := fmt.Sscanf(value, "%d", &size); err == nil && size > 0 {
			cfg.Cursor.Size = size
		}
		resets.refresh()
	}
	cursorForm := widget.NewForm(
		widget.NewFormItem("Cursor theme", resets.item(cursorTheme,
			func() bool { return cfg.Cursor.Theme != defaults.Cursor.Theme },
			func() { cursorTheme.SetText(defaults.Cursor.Theme) },
		)),
		widget.NewFormItem("Cursor size", resets.item(cursorSize,
			func() bool { return cfg.Cursor.Size != defaults.Cursor.Size },
			func() { cursorSize.SetText(formatCursorSize(defaults.Cursor.Size)) },
		)),
	)
	cursorHint := widget.NewLabel(
		"Name of an installed Xcursor theme (as found under an icons directory's cursors/ subfolder), and the" +
			" nominal cursor size in pixels. Leaving either empty falls back to the XCURSOR_THEME/XCURSOR_SIZE" +
			" environment variables, or the compositor's own built-in cursor if those aren't set either." +
			" Changes apply live without needing to reopen windows.",
	)
	cursorHint.Wrapping = fyne.TextWrapWord

	darkModeRow := resets.item(darkMode,
		func() bool { return cfg.Appearance.DarkMode != defaults.Appearance.DarkMode },
		func() { darkMode.SetChecked(defaults.Appearance.DarkMode) },
	)
	wallpaperControl := container.NewBorder(nil, nil, nil, browseButton, wallpaper)
	wallpaperForm := widget.NewForm(widget.NewFormItem("Wallpaper", resets.item(wallpaperControl,
		func() bool { return cfg.Wallpaper != defaults.Wallpaper },
		func() { wallpaper.SetText(defaults.Wallpaper) },
	)))

	return resets.page(container.NewVBox(
		darkModeRow, colorHint,
		widget.NewSeparator(), wallpaperForm, wallpaperHint,
		widget.NewSeparator(), blurForm, blurHint,
		widget.NewSeparator(), cornersForm, cornersHint,
		widget.NewSeparator(), gapsForm, gapsHint,
		widget.NewSeparator(), borderForm, borderHint,
		widget.NewSeparator(), cursorForm, cursorHint,
	))
}

// buildWorkspacesTab lays out the virtual-desktop settings: split-per-monitor
// vs. combined-across-monitors, the starting workspace count, whether that
// count grows/shrinks on demand, and the on-screen dot overlay toggle.
func buildWorkspacesTab(cfg *Config) fyne.CanvasObject {
	if cfg.Workspaces.Mode == "" {
		cfg.Workspaces = defaultWorkspaceSettings()
	}
	defaults := defaultWorkspaceSettings()
	resets := newResetGroup()

	modeLabels := map[string]string{
		"per_monitor": "Split per monitor (each screen has its own workspaces)",
		"combined":    "Combined across monitors (every screen shows the same workspace)",
	}
	modeSelect := widget.NewSelect(
		[]string{modeLabels["per_monitor"], modeLabels["combined"]},
		func(selected string) {
			if selected == modeLabels["combined"] {
				cfg.Workspaces.Mode = "combined"
			} else {
				cfg.Workspaces.Mode = "per_monitor"
			}
			resets.refresh()
		},
	)
	modeSelect.SetSelected(modeLabels[cfg.Workspaces.Mode])

	count := widget.NewEntry()
	count.SetText(fmt.Sprintf("%d", cfg.Workspaces.Count))
	count.OnChanged = func(s string) {
		var n int
		if _, err := fmt.Sscanf(s, "%d", &n); err == nil && n > 0 {
			cfg.Workspaces.Count = n
		}
		resets.refresh()
	}

	dynamic := widget.NewCheck("Grow/shrink workspace count automatically", func(checked bool) {
		cfg.Workspaces.Dynamic = checked
		resets.refresh()
	})
	dynamic.SetChecked(cfg.Workspaces.Dynamic)

	overlay := widget.NewCheck("Show workspace indicator dots on switch", func(checked bool) {
		cfg.Workspaces.Overlay = checked
		resets.refresh()
	})
	overlay.SetChecked(cfg.Workspaces.Overlay)

	keepDisconnected := widget.NewCheck("Keep workspaces and windows of disconnected monitors", func(checked bool) {
		cfg.Workspaces.KeepDisconnected = checked
		resets.refresh()
	})
	keepDisconnected.SetChecked(cfg.Workspaces.KeepDisconnected)

	transitionMs := widget.NewEntry()
	transitionMs.SetText(fmt.Sprintf("%d", cfg.Workspaces.TransitionMs))
	transitionMs.OnChanged = func(s string) {
		var n int
		if _, err := fmt.Sscanf(s, "%d", &n); err == nil && n >= 0 {
			cfg.Workspaces.TransitionMs = n
		}
		resets.refresh()
	}

	axisLabels := map[string]string{
		"horizontal": "Horizontal (slide left/right)",
		"vertical":   "Vertical (slide up/down)",
	}
	transitionAxis := widget.NewSelect(
		[]string{axisLabels["horizontal"], axisLabels["vertical"]},
		func(selected string) {
			if selected == axisLabels["vertical"] {
				cfg.Workspaces.TransitionAxis = "vertical"
			} else {
				cfg.Workspaces.TransitionAxis = "horizontal"
			}
			resets.refresh()
		},
	)
	transitionAxis.SetSelected(axisLabels[cfg.Workspaces.TransitionAxis])

	form := widget.NewForm(
		widget.NewFormItem("Layout", resets.item(modeSelect, func() bool { return cfg.Workspaces.Mode != defaults.Mode }, func() { modeSelect.SetSelected(modeLabels[defaults.Mode]) })),
		widget.NewFormItem("Starting workspace count", resets.item(count, func() bool { return cfg.Workspaces.Count != defaults.Count }, func() { count.SetText(fmt.Sprintf("%d", defaults.Count)) })),
		widget.NewFormItem("Dynamic count", resets.item(dynamic, func() bool { return cfg.Workspaces.Dynamic != defaults.Dynamic }, func() { dynamic.SetChecked(defaults.Dynamic) })),
		widget.NewFormItem("On-screen overlay", resets.item(overlay, func() bool { return cfg.Workspaces.Overlay != defaults.Overlay }, func() { overlay.SetChecked(defaults.Overlay) })),
		widget.NewFormItem("Disconnected monitors", resets.item(keepDisconnected, func() bool { return cfg.Workspaces.KeepDisconnected != defaults.KeepDisconnected }, func() { keepDisconnected.SetChecked(defaults.KeepDisconnected) })),
		widget.NewFormItem("Switch animation (ms, 0 = off)", resets.item(transitionMs, func() bool { return cfg.Workspaces.TransitionMs != defaults.TransitionMs }, func() { transitionMs.SetText(fmt.Sprintf("%d", defaults.TransitionMs)) })),
		widget.NewFormItem("Switch animation direction", resets.item(transitionAxis, func() bool { return cfg.Workspaces.TransitionAxis != defaults.TransitionAxis }, func() { transitionAxis.SetSelected(axisLabels[defaults.TransitionAxis]) })),
	)

	hint := widget.NewLabel(
		"Switch workspaces with Super+Left/Right, and move the focused window to an adjacent workspace" +
			" with Super+Alt+Left/Right (both rebindable in the Shortcuts tab). With \"Dynamic count\" on," +
			" navigating or moving a window past the last workspace creates a new one, and empty trailing" +
			" workspaces are dropped again automatically; otherwise the count above is fixed. With \"Keep workspaces" +
			" and windows of disconnected monitors\" on, unplugging a monitor (even the last one) hides its windows" +
			" instead of moving them, and plugging it back into the same connector restores them where they were.",
	)
	hint.Wrapping = fyne.TextWrapWord

	return resets.page(container.NewVBox(form, hint))
}

// defaultFocusSettings mirrors config::FocusSettings' Default impl: both off,
// matching click-to-focus behavior from before either setting existed.
func defaultFocusSettings() FocusSettings {
	return FocusSettings{}
}

// buildFocusTab lays out the two focus-follows-mouse/mouse-follows-focus
// toggles.
func buildFocusTab(cfg *Config) fyne.CanvasObject {
	defaults := defaultFocusSettings()
	resets := newResetGroup()

	followsMouse := widget.NewCheck("Focus follows mouse", func(checked bool) {
		cfg.Focus.FollowsMouse = checked
		resets.refresh()
	})
	followsMouse.SetChecked(cfg.Focus.FollowsMouse)

	mouseFollowsFocus := widget.NewCheck("Mouse follows focus", func(checked bool) {
		cfg.Focus.MouseFollowsFocus = checked
		resets.refresh()
	})
	mouseFollowsFocus.SetChecked(cfg.Focus.MouseFollowsFocus)

	form := widget.NewForm(
		widget.NewFormItem("", resets.item(followsMouse, func() bool { return cfg.Focus.FollowsMouse != defaults.FollowsMouse }, func() { followsMouse.SetChecked(defaults.FollowsMouse) })),
		widget.NewFormItem("", resets.item(mouseFollowsFocus, func() bool { return cfg.Focus.MouseFollowsFocus != defaults.MouseFollowsFocus }, func() { mouseFollowsFocus.SetChecked(defaults.MouseFollowsFocus) })),
	)

	hint := widget.NewLabel(
		"\"Focus follows mouse\" focuses whatever window the pointer is over without needing a click" +
			" (hovering empty space leaves the current focus alone). \"Mouse follows focus\" warps the" +
			" pointer to the center of a window whenever it's focused some other way - switching" +
			" workspaces, cycling windows (Alt+Tab-style bindings), a newly opened window, or activating" +
			" a window from the dock.",
	)
	hint.Wrapping = fyne.TextWrapWord

	keyTrackerEnabled := widget.NewCheck("Show key tracker overlay (top-left)", func(checked bool) {
		cfg.KeyTracker.Enabled = checked
	})
	keyTrackerEnabled.SetChecked(cfg.KeyTracker.Enabled)

	keyTrackerKeys := widget.NewEntry()
	keyTrackerKeys.SetText(strings.Join(cfg.KeyTracker.Keys, ", "))
	keyTrackerKeys.OnChanged = func(text string) {
		keys := []string{}
		for _, key := range strings.Split(text, ",") {
			if key = strings.TrimSpace(key); key != "" {
				keys = append(keys, key)
			}
		}
		cfg.KeyTracker.Keys = keys
	}

	keyTrackerHint := widget.NewLabel(
		"The key tracker shows whether each listed key is currently pressed (xkb keysym names," +
			" comma-separated, e.g. Control_L, Control_R) - useful for finding a stuck key.",
	)
	keyTrackerHint.Wrapping = fyne.TextWrapWord

	keyTrackerForm := widget.NewForm(
		widget.NewFormItem("", keyTrackerEnabled),
		widget.NewFormItem("Keys", keyTrackerKeys),
	)

	return resets.page(container.NewVBox(form, hint, widget.NewSeparator(), keyTrackerForm, keyTrackerHint))
}

// formatOptionalColor renders a BorderSettings.GradientColor for display:
// nil (solid-colour border) shows as an empty field rather than "<nil>".
func formatOptionalColor(color *string) string {
	if color == nil {
		return ""
	}
	return *color
}

// formatCursorSize renders a CursorSettings.Size for display: 0 means
// "unset" (fall back to XCURSOR_SIZE/the compositor default), shown as an
// empty field rather than "0".
func formatCursorSize(size int) string {
	if size <= 0 {
		return ""
	}
	return fmt.Sprintf("%d", size)
}

func equalStrings(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func splitKeyCombos(s string) []string {
	var combos []string
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		if part != "" {
			combos = append(combos, part)
		}
	}
	return combos
}
