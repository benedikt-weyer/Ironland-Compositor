// This file is the only place gui-settings talks to `ironlandctl` (see
// ../ironlandctl): every load/save in config.go goes through one of the
// functions here, so the settings file's actual schema, search path, and
// merge-with-defaults behavior live in exactly one place (`ironlandctl`
// and the `ironland-config` crate it's built on) rather than being
// reimplemented in Go too.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
)

// ironlandctlBinary is overridden by tests to point at a stub script.
var ironlandctlBinary = "ironlandctl"

// runIronlandctl runs `ironlandctl <args>`, feeding it stdin if non-nil,
// and returns its stdout. A non-zero exit surfaces ironlandctl's own stderr
// message (it's written to be user-facing) rather than just the exit code.
func runIronlandctl(stdin []byte, args ...string) ([]byte, error) {
	cmd := exec.Command(ironlandctlBinary, args...)
	if stdin != nil {
		cmd.Stdin = bytes.NewReader(stdin)
	}
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		if message := strings.TrimSpace(stderr.String()); message != "" {
			return nil, fmt.Errorf("%s", message)
		}
		return nil, fmt.Errorf("running %s %s: %w", ironlandctlBinary, strings.Join(args, " "), err)
	}
	return stdout.Bytes(), nil
}

// ironlandctlShow is the effective config `ironland-compositor` would use
// if it started right now (`ironlandctl show --json`).
func ironlandctlShow() (Config, error) {
	return decodeConfig(runIronlandctl(nil, "show", "--json"))
}

// ironlandctlDefaults is ironlandctl's built-in defaults
// (`ironlandctl defaults --json`), fully populated (every action has a
// default binding, etc.) - used as the baseline for every tab's
// "differs from default" reset affordances.
func ironlandctlDefaults() (Config, error) {
	return decodeConfig(runIronlandctl(nil, "defaults", "--json"))
}

func decodeConfig(data []byte, err error) (Config, error) {
	if err != nil {
		return Config{}, err
	}
	var cfg Config
	if err := json.Unmarshal(data, &cfg); err != nil {
		return Config{}, fmt.Errorf("parsing ironlandctl's config JSON: %w", err)
	}
	return cfg, nil
}

// ironlandctlApply hands cfg to `ironlandctl apply`, which writes it out
// atomically and prints the path it wrote to (see ironlandctl's own
// `apply` command - that path is deliberately the only thing on its
// stdout).
func ironlandctlApply(cfg Config) (string, error) {
	payload, err := json.Marshal(cfg)
	if err != nil {
		return "", fmt.Errorf("encoding settings as JSON: %w", err)
	}
	out, err := runIronlandctl(payload, "apply")
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(string(out)), nil
}

// ironlandctlPath is `ironlandctl path --json`'s "active" field: the config
// file that's actually in effect right now, or "" if none of the search
// path's candidates exist yet.
func ironlandctlActivePath() (string, error) {
	out, err := runIronlandctl(nil, "path", "--json")
	if err != nil {
		return "", err
	}
	var result struct {
		Active *string `json:"active"`
	}
	if err := json.Unmarshal(out, &result); err != nil {
		return "", fmt.Errorf("parsing ironlandctl path --json output: %w", err)
	}
	if result.Active == nil {
		return "", nil
	}
	return *result.Active, nil
}

// ironlandctlSessionUnits lists the systemd --user services a shell could be
// started from (`ironlandctl session units --json`), sorted.
func ironlandctlSessionUnits() ([]string, error) {
	out, err := runIronlandctl(nil, "session", "units", "--json")
	if err != nil {
		return nil, err
	}
	var units []string
	if err := json.Unmarshal(out, &units); err != nil {
		return nil, fmt.Errorf("parsing ironlandctl's unit list: %w", err)
	}
	return units, nil
}
