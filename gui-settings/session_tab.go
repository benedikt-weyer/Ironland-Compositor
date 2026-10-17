package main

import (
	"strings"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/widget"
)

// filterUnits narrows units to those containing query (case-insensitive),
// for the shell-unit dropdown. An empty query, or one that already is exactly
// one of the units (i.e. the user just picked it), shows the whole list, so
// reopening the dropdown after a selection doesn't leave a single entry.
func filterUnits(units []string, query string) []string {
	query = strings.TrimSpace(strings.ToLower(query))
	if query == "" {
		return units
	}
	filtered := []string{}
	for _, unit := range units {
		lower := strings.ToLower(unit)
		if lower == query {
			return units
		}
		if strings.Contains(lower, query) {
			filtered = append(filtered, unit)
		}
	}
	return filtered
}

// containsString reports whether list has s.
func containsString(list []string, s string) bool {
	for _, item := range list {
		if item == s {
			return true
		}
	}
	return false
}

// buildSessionTab lays out the "compositor starts the shell" toggle and a
// searchable dropdown of systemd --user services to pick the shell from.
func buildSessionTab(cfg *Config) fyne.CanvasObject {
	if cfg.Session.ShellUnit == "" {
		cfg.Session = defaultSessionSettings()
	}
	defaults := defaultSessionSettings()
	resets := newResetGroup()

	units, unitsErr := ironlandctlSessionUnits()

	warning := widget.NewLabel("")
	warning.Wrapping = fyne.TextWrapWord

	var unit *widget.SelectEntry
	updateWarning := func() {
		switch {
		case unitsErr != nil:
			warning.SetText("Couldn't list systemd user services (" + unitsErr.Error() + "); type the unit name instead.")
		case cfg.Session.StartShell && cfg.Session.ShellUnit == "":
			warning.SetText("Enter the shell's systemd unit.")
		case cfg.Session.StartShell && !containsString(units, cfg.Session.ShellUnit):
			warning.SetText("No installed or loaded user service with this name was found; it will still be started by name.")
		default:
			warning.SetText("")
		}
	}

	unit = widget.NewSelectEntry(units)
	unit.SetPlaceHolder("Search systemd user services…")
	unit.SetText(cfg.Session.ShellUnit)
	unit.OnChanged = func(text string) {
		cfg.Session.ShellUnit = strings.TrimSpace(text)
		unit.SetOptions(filterUnits(units, text))
		updateWarning()
		resets.refresh()
	}

	startShell := widget.NewCheck("Start the shell from the compositor", func(checked bool) {
		cfg.Session.StartShell = checked
		if checked {
			unit.Enable()
		} else {
			unit.Disable()
		}
		updateWarning()
		resets.refresh()
	})
	startShell.SetChecked(cfg.Session.StartShell)
	if !cfg.Session.StartShell {
		unit.Disable()
	}
	updateWarning()

	form := widget.NewForm(
		widget.NewFormItem("", resets.item(startShell, func() bool { return cfg.Session.StartShell != defaults.StartShell }, func() { startShell.SetChecked(defaults.StartShell) })),
		widget.NewFormItem("Shell unit", resets.item(unit, func() bool { return cfg.Session.ShellUnit != defaults.ShellUnit }, func() { unit.SetText(defaults.ShellUnit) })),
	)

	hint := widget.NewLabel(
		"With this on, the compositor runs `systemctl --user start <unit>` as the last step of bringing up" +
			" the session - after XWayland is ready and DISPLAY/WAYLAND_DISPLAY are exported - so the shell and" +
			" everything it launches (e.g. Steam) inherit a complete environment. Leave it off if something" +
			" else already starts your shell. Takes effect the next time the compositor starts.",
	)
	hint.Wrapping = fyne.TextWrapWord

	return resets.page(container.NewVBox(form, warning, hint))
}
