package main

import (
	"reflect"
	"testing"
)

func TestFilterUnits(t *testing.T) {
	units := []string{"caelestia-shell.service", "desktop-shell.service", "iron-polkit.service"}

	if got := filterUnits(units, ""); !reflect.DeepEqual(got, units) {
		t.Fatalf("empty query: got %v, want all", got)
	}
	if got, want := filterUnits(units, " SHELL "), units[:2]; !reflect.DeepEqual(got, want) {
		t.Fatalf("substring, case-insensitive: got %v, want %v", got, want)
	}
	if got := filterUnits(units, "desktop-shell.service"); !reflect.DeepEqual(got, units) {
		t.Fatalf("exact match shows everything: got %v", got)
	}
	if got := filterUnits(units, "nope"); len(got) != 0 {
		t.Fatalf("no match: got %v, want empty", got)
	}
}
