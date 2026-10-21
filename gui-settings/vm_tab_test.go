package main

import "testing"

func TestSetOptionalText(t *testing.T) {
	var p *string
	setOptionalText(&p, "  /a/b  ")
	if p == nil || *p != "/a/b" {
		t.Fatalf("got %v, want /a/b", p)
	}
	setOptionalText(&p, "   ")
	if p != nil {
		t.Fatalf("blank should clear the setting, got %q", *p)
	}
}

func TestSameOptionalTreatsNilAndEmptyAlike(t *testing.T) {
	empty := ""
	if !sameOptional(nil, &empty) {
		t.Fatal("nil and empty should match")
	}
	key := "k"
	if sameOptional(&key, nil) {
		t.Fatal("a set value differs from nil")
	}
}

func TestParseAtLeastOne(t *testing.T) {
	if n, err := parseAtLeastOne(" 2048 "); err != nil || n != 2048 {
		t.Fatalf("got %d, %v", n, err)
	}
	for _, bad := range []string{"", "0", "-1", "x", "1.5"} {
		if _, err := parseAtLeastOne(bad); err == nil {
			t.Fatalf("%q should be rejected", bad)
		}
	}
}
