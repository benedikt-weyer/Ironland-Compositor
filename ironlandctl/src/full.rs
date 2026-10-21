//! Writes an entire [`FullConfig`] into a `toml_edit` document, replacing
//! whatever was there before. Used by `apply` - the settings GUI's bulk
//! "Save" - which always has a complete, fully-merged config in hand rather
//! than one changed field.

use crate::settings::caps_lock_str;
use ironland_config::{FullConfig, OutputPosition, WorkspaceMode, WorkspaceTransitionAxis};
use toml_edit::{Array, DocumentMut, Item, Table, value};

fn ensure_table<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut Table {
    if !doc.contains_key(key) {
        doc.insert(key, Item::Table(Table::new()));
    }
    doc[key]
        .as_table_mut()
        .unwrap_or_else(|| panic!("config.toml has a non-table [{key}]"))
}

pub fn write(doc: &mut DocumentMut, full: &FullConfig) {
    let cfg = &full.config;

    doc["terminal"] = value(cfg.terminal.as_str());
    doc["browser"] = value(cfg.browser.as_str());
    doc["file_manager"] = value(cfg.file_manager.as_str());
    doc["top_bar"] = value(cfg.top_bar);
    match cfg.wallpaper.as_deref() {
        Some(path) if !path.is_empty() => doc["wallpaper"] = value(path),
        _ => {
            doc.remove("wallpaper");
        }
    }

    let keyboard = ensure_table(doc, "keyboard");
    keyboard["rules"] = value(cfg.keyboard.rules.as_str());
    keyboard["model"] = value(cfg.keyboard.model.as_str());
    keyboard["layout"] = value(cfg.keyboard.layout.as_str());
    keyboard["variant"] = value(cfg.keyboard.variant.as_str());
    keyboard["options"] = value(cfg.keyboard.options.as_str());
    keyboard["caps_lock"] = value(caps_lock_str(cfg.keyboard.caps_lock));

    let blur = ensure_table(doc, "blur");
    blur["enabled"] = value(cfg.blur.enabled);
    blur["radius"] = value(i64::from(cfg.blur.radius));

    let corners = ensure_table(doc, "corners");
    corners["enabled"] = value(cfg.corners.enabled);
    corners["radius"] = value(i64::from(cfg.corners.radius));

    let gaps = ensure_table(doc, "gaps");
    gaps["inner"] = value(i64::from(cfg.gaps.inner));
    gaps["outer"] = value(i64::from(cfg.gaps.outer));

    let border = ensure_table(doc, "border");
    border["enabled"] = value(cfg.border.enabled);
    border["thickness"] = value(i64::from(cfg.border.thickness));
    border["color"] = value(cfg.border.color.as_str());
    match cfg.border.gradient_color.as_deref() {
        Some(color) if !color.is_empty() => border["gradient_color"] = value(color),
        _ => {
            border.remove("gradient_color");
        }
    }
    border["angle"] = value(f64::from(cfg.border.angle));

    let cursor = ensure_table(doc, "cursor");
    match cfg.cursor.theme.as_deref() {
        Some(theme) if !theme.is_empty() => cursor["theme"] = value(theme),
        _ => {
            cursor.remove("theme");
        }
    }
    match cfg.cursor.size {
        Some(size) if size > 0 => cursor["size"] = value(i64::from(size)),
        _ => {
            cursor.remove("size");
        }
    }

    let focus = ensure_table(doc, "focus");
    focus["follows_mouse"] = value(cfg.focus.follows_mouse);
    focus["mouse_follows_focus"] = value(cfg.focus.mouse_follows_focus);

    let appearance = ensure_table(doc, "appearance");
    appearance["dark_mode"] = value(full.appearance.dark_mode);

    let workspaces = ensure_table(doc, "workspaces");
    workspaces["mode"] = value(match cfg.workspaces.mode {
        WorkspaceMode::PerMonitor => "per_monitor",
        WorkspaceMode::Combined => "combined",
    });
    workspaces["count"] = value(cfg.workspaces.count as i64);
    workspaces["dynamic"] = value(cfg.workspaces.dynamic);
    workspaces["overlay"] = value(cfg.workspaces.overlay);
    workspaces["keep_disconnected"] = value(cfg.workspaces.keep_disconnected);
    workspaces["transition_ms"] = value(cfg.workspaces.transition_ms as i64);
    workspaces["transition_axis"] = value(match cfg.workspaces.transition_axis {
        WorkspaceTransitionAxis::Horizontal => "horizontal",
        WorkspaceTransitionAxis::Vertical => "vertical",
    });

    let session = ensure_table(doc, "session");
    session["start_shell"] = value(cfg.session.start_shell);
    session["shell_unit"] = value(cfg.session.shell_unit.as_str());

    let window_animations = ensure_table(doc, "window_animations");
    window_animations["enabled"] = value(cfg.window_animations.enabled);
    window_animations["duration_ms"] = value(i64::from(cfg.window_animations.duration_ms));

    let vm = ensure_table(doc, "vm");
    for (key, text) in [
        ("image", &cfg.vm.image),
        ("ssh_key", &cfg.vm.ssh_key),
        ("public_key", &cfg.vm.public_key),
        ("public_key_file", &cfg.vm.public_key_file),
    ] {
        match text.as_deref() {
            Some(text) if !text.trim().is_empty() => vm[key] = value(text),
            _ => {
                vm.remove(key);
            }
        }
    }
    vm["user"] = value(cfg.vm.user.as_str());
    vm["memory_mb"] = value(i64::from(cfg.vm.memory_mb));
    vm["cpus"] = value(i64::from(cfg.vm.cpus));
    vm["kvm"] = value(cfg.vm.kvm);
    vm["max_instances"] = value(i64::from(cfg.vm.max_instances));

    let mut shortcuts = Table::new();
    let mut action_names: Vec<&String> = cfg.shortcuts.keys().collect();
    action_names.sort();
    for name in action_names {
        let combos: Array = cfg.shortcuts[name].iter().map(String::as_str).collect();
        shortcuts[name.as_str()] = value(combos);
    }
    doc["shortcuts"] = Item::Table(shortcuts);

    let mut gestures = Table::new();
    let mut gesture_actions: Vec<&String> = cfg.gestures.keys().collect();
    gesture_actions.sort();
    for name in gesture_actions {
        let specs: Array = cfg.gestures[name].iter().map(String::as_str).collect();
        gestures[name.as_str()] = value(specs);
    }
    doc["gestures"] = Item::Table(gestures);

    let mut outputs = Table::new();
    let mut output_names: Vec<&String> = cfg.outputs.keys().collect();
    output_names.sort();
    for name in output_names {
        let settings = &cfg.outputs[name];
        let mut out = Table::new();
        out.set_implicit(false);
        if settings.primary {
            out.insert("primary", value(true));
        }
        if let Some(rate) = settings.refresh_rate {
            out.insert("refresh_rate", value(i64::from(rate)));
        }
        if let Some(target) = &settings.mirror_of {
            out.insert("mirror_of", value(target.as_str()));
        }
        if let Some(position) = &settings.position {
            let mut pos = Table::new();
            pos.set_implicit(false);
            match position {
                OutputPosition::RightOf { right_of } => {
                    pos.insert("right_of", value(right_of.as_str()));
                }
                OutputPosition::LeftOf { left_of } => {
                    pos.insert("left_of", value(left_of.as_str()));
                }
                OutputPosition::Above { above } => {
                    pos.insert("above", value(above.as_str()));
                }
                OutputPosition::Below { below } => {
                    pos.insert("below", value(below.as_str()));
                }
                OutputPosition::Absolute { x, y } => {
                    pos.insert("x", value(i64::from(*x)));
                    pos.insert("y", value(i64::from(*y)));
                }
            }
            out.insert("position", Item::Table(pos));
        }
        outputs.insert(name.as_str(), Item::Table(out));
    }
    doc["outputs"] = Item::Table(outputs);
}
