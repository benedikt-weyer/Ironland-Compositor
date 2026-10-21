package main

import (
	"errors"
	"strconv"
	"strings"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/widget"
)

// optionalText reads a nullable setting as the text shown in its entry.
func optionalText(p *string) string {
	if p == nil {
		return ""
	}
	return *p
}

// setOptionalText stores entry text into a nullable setting: blank means
// "not set" (null), which is how the compositor reads it.
func setOptionalText(p **string, text string) {
	text = strings.TrimSpace(text)
	if text == "" {
		*p = nil
		return
	}
	*p = &text
}

// sameOptional reports whether two nullable settings hold the same value,
// treating nil and "" alike.
func sameOptional(a, b *string) bool {
	return strings.TrimSpace(optionalText(a)) == strings.TrimSpace(optionalText(b))
}

// parseAtLeastOne parses a whole number >= 1 from entry text.
func parseAtLeastOne(text string) (uint32, error) {
	n, err := strconv.ParseUint(strings.TrimSpace(text), 10, 32)
	if err != nil || n == 0 {
		return 0, errors.New("enter a whole number of at least 1")
	}
	return uint32(n), nil
}

// buildVMTab lays out the settings for launching programs inside QEMU VMs
// (the launcher's VM tabs, reached with the Left/Right keys): how to log in
// to the guest and the VM's size. Which programs have a VM is set up in
// vm.programs, not here. Takes effect on the next VM launch.
func buildVMTab(cfg *Config) fyne.CanvasObject {
	defaults := defaultConfig().VM
	vm := &cfg.VM
	resets := newResetGroup()

	textEntry := func(placeholder string, get func() string, set func(string)) *widget.Entry {
		e := widget.NewEntry()
		e.SetPlaceHolder(placeholder)
		e.SetText(get())
		e.OnChanged = func(text string) {
			set(text)
			resets.refresh()
		}
		return e
	}
	numberEntry := func(field *uint32) *widget.Entry {
		e := widget.NewEntry()
		e.SetText(strconv.FormatUint(uint64(*field), 10))
		e.Validator = func(text string) error {
			_, err := parseAtLeastOne(text)
			return err
		}
		e.OnChanged = func(text string) {
			// An unparsable value keeps the last good one; the validator
			// marks the field so it's visible why.
			if n, err := parseAtLeastOne(text); err == nil {
				*field = n
			}
			resets.refresh()
		}
		return e
	}

	user := textEntry("user",
		func() string { return vm.User },
		func(t string) { vm.User = strings.TrimSpace(t) })
	sshKey := textEntry("private key (blank: ssh-agent / ~/.ssh)",
		func() string { return optionalText(vm.SSHKey) },
		func(t string) { setOptionalText(&vm.SSHKey, t) })
	publicKeyFile := textEntry("public key file (blank: see below)",
		func() string { return optionalText(vm.PublicKeyFile) },
		func(t string) { setOptionalText(&vm.PublicKeyFile, t) })

	publicKey := widget.NewMultiLineEntry()
	publicKey.SetPlaceHolder("ssh-ed25519 AAAA… (blank: see below)")
	publicKey.Wrapping = fyne.TextWrapBreak
	publicKey.SetMinRowsVisible(3)
	publicKey.SetText(optionalText(vm.PublicKey))
	publicKey.OnChanged = func(text string) {
		setOptionalText(&vm.PublicKey, text)
		resets.refresh()
	}

	memory := numberEntry(&vm.MemoryMB)
	cpus := numberEntry(&vm.CPUs)
	maxInstances := numberEntry(&vm.MaxInstances)

	kvm := widget.NewCheck("Use KVM acceleration", func(checked bool) {
		vm.KVM = checked
		resets.refresh()
	})
	kvm.SetChecked(vm.KVM)

	numberReset := func(e *widget.Entry, field func() uint32, def uint32) (func() bool, func()) {
		return func() bool { return field() != def },
			func() { e.SetText(strconv.FormatUint(uint64(def), 10)) }
	}
	memDiff, memReset := numberReset(memory, func() uint32 { return vm.MemoryMB }, defaults.MemoryMB)
	cpuDiff, cpuReset := numberReset(cpus, func() uint32 { return vm.CPUs }, defaults.CPUs)
	maxDiff, maxReset := numberReset(maxInstances, func() uint32 { return vm.MaxInstances }, defaults.MaxInstances)

	form := widget.NewForm(
		widget.NewFormItem("Guest user", resets.item(user,
			func() bool { return vm.User != defaults.User },
			func() { user.SetText(defaults.User) })),
		widget.NewFormItem("SSH key", resets.item(sshKey,
			func() bool { return !sameOptional(vm.SSHKey, defaults.SSHKey) },
			func() { sshKey.SetText(optionalText(defaults.SSHKey)) })),
		widget.NewFormItem("Public key file", resets.item(publicKeyFile,
			func() bool { return !sameOptional(vm.PublicKeyFile, defaults.PublicKeyFile) },
			func() { publicKeyFile.SetText(optionalText(defaults.PublicKeyFile)) })),
		widget.NewFormItem("Public key", resets.item(publicKey,
			func() bool { return !sameOptional(vm.PublicKey, defaults.PublicKey) },
			func() { publicKey.SetText(optionalText(defaults.PublicKey)) })),
		widget.NewFormItem("Memory (MiB)", resets.item(memory, memDiff, memReset)),
		widget.NewFormItem("CPUs", resets.item(cpus, cpuDiff, cpuReset)),
		widget.NewFormItem("Max running VMs", resets.item(maxInstances, maxDiff, maxReset)),
		widget.NewFormItem("", resets.item(kvm,
			func() bool { return vm.KVM != defaults.KVM },
			func() { kvm.SetChecked(defaults.KVM) })),
	)

	hint := widget.NewLabel(
		"In the launcher, Left opens the tab of programs that start in a throw-away VM and Right the tab of" +
			" persistent ones. Each VM runs one program from its own guest image (SSH server, waypipe and just" +
			" that program): list the programs under [vm.programs] in the config, or on NixOS in" +
			" services.ironland-compositor.vm.programs, which builds the images for you.\n\n" +
			"The guest accepts the public key set here; with neither field set, that is ~/.ssh/id_ed25519.pub" +
			" plus whatever your ssh-agent holds, and logging in uses the agent. Takes effect on the next VM launch.",
	)
	hint.Wrapping = fyne.TextWrapWord

	return resets.page(container.NewVBox(form, hint))
}
