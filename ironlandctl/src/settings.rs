//! The `get`/`set`/`unset <key>` commands: every scalar leaf setting,
//! addressed by its dotted TOML path, covering every field the settings GUI
//! exposes outside of `[shortcuts]` and `[outputs]` (see [`crate::shortcuts`]
//! and [`crate::outputs`] for those - they're maps, not scalars, so don't
//! fit this one key/value shape).

use anyhow::{Result, bail};
use clap::ValueEnum;
use ironland_config::FullConfig;
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum SettingKey {
    #[value(name = "terminal")]
    Terminal,
    #[value(name = "browser")]
    Browser,
    #[value(name = "file_manager")]
    FileManager,
    #[value(name = "top_bar")]
    TopBar,
    #[value(name = "wallpaper")]
    Wallpaper,
    #[value(name = "keyboard.rules")]
    KeyboardRules,
    #[value(name = "keyboard.model")]
    KeyboardModel,
    #[value(name = "keyboard.layout")]
    KeyboardLayout,
    #[value(name = "keyboard.variant")]
    KeyboardVariant,
    #[value(name = "keyboard.options")]
    KeyboardOptions,
    #[value(name = "keyboard.caps_lock")]
    KeyboardCapsLock,
    #[value(name = "blur.enabled")]
    BlurEnabled,
    #[value(name = "blur.radius")]
    BlurRadius,
    #[value(name = "corners.enabled")]
    CornersEnabled,
    #[value(name = "corners.radius")]
    CornersRadius,
    #[value(name = "gaps.inner")]
    GapsInner,
    #[value(name = "gaps.outer")]
    GapsOuter,
    #[value(name = "border.enabled")]
    BorderEnabled,
    #[value(name = "border.thickness")]
    BorderThickness,
    #[value(name = "border.color")]
    BorderColor,
    #[value(name = "border.gradient_color")]
    BorderGradientColor,
    #[value(name = "border.angle")]
    BorderAngle,
    #[value(name = "cursor.theme")]
    CursorTheme,
    #[value(name = "cursor.size")]
    CursorSize,
    #[value(name = "focus.follows_mouse")]
    FocusFollowsMouse,
    #[value(name = "focus.mouse_follows_focus")]
    FocusMouseFollowsFocus,
    #[value(name = "performance.fps_overlay")]
    PerformanceFpsOverlay,
    #[value(name = "performance.fps_overlay_position")]
    PerformanceFpsOverlayPosition,
    #[value(name = "performance.stutter_threshold_ms")]
    PerformanceStutterThresholdMs,
    #[value(name = "performance.stutter_log")]
    PerformanceStutterLog,
    #[value(name = "performance.fps_overlay_interval_ms")]
    PerformanceFpsOverlayIntervalMs,
    #[value(name = "key_tracker.enabled")]
    KeyTrackerEnabled,
    #[value(name = "key_tracker.keys")]
    KeyTrackerKeys,
    #[value(name = "key_tracker.position")]
    KeyTrackerPosition,
    #[value(name = "appearance.dark_mode")]
    AppearanceDarkMode,
    #[value(name = "workspaces.mode")]
    WorkspacesMode,
    #[value(name = "workspaces.count")]
    WorkspacesCount,
    #[value(name = "workspaces.dynamic")]
    WorkspacesDynamic,
    #[value(name = "workspaces.overlay")]
    WorkspacesOverlay,
    #[value(name = "window_animations.enabled")]
    WindowAnimationsEnabled,
    #[value(name = "window_animations.duration_ms")]
    WindowAnimationsDurationMs,
}

impl SettingKey {
    /// All keys, in declaration order - used to print `show`/`defaults` in a
    /// stable, human-friendly order.
    pub fn all() -> &'static [SettingKey] {
        use SettingKey::*;
        &[
            Terminal,
            Browser,
            FileManager,
            TopBar,
            Wallpaper,
            KeyboardRules,
            KeyboardModel,
            KeyboardLayout,
            KeyboardVariant,
            KeyboardOptions,
            KeyboardCapsLock,
            BlurEnabled,
            BlurRadius,
            CornersEnabled,
            CornersRadius,
            GapsInner,
            GapsOuter,
            BorderEnabled,
            BorderThickness,
            BorderColor,
            BorderGradientColor,
            BorderAngle,
            CursorTheme,
            CursorSize,
            FocusFollowsMouse,
            FocusMouseFollowsFocus,
            PerformanceFpsOverlay,
            PerformanceFpsOverlayPosition,
            PerformanceStutterThresholdMs,
            PerformanceStutterLog,
            PerformanceFpsOverlayIntervalMs,
            KeyTrackerEnabled,
            KeyTrackerKeys,
            KeyTrackerPosition,
            AppearanceDarkMode,
            WorkspacesMode,
            WorkspacesCount,
            WorkspacesDynamic,
            WorkspacesOverlay,
            WindowAnimationsEnabled,
            WindowAnimationsDurationMs,
        ]
    }

    pub fn dotted(self) -> &'static str {
        use SettingKey::*;
        match self {
            Terminal => "terminal",
            Browser => "browser",
            FileManager => "file_manager",
            TopBar => "top_bar",
            Wallpaper => "wallpaper",
            KeyboardRules => "keyboard.rules",
            KeyboardModel => "keyboard.model",
            KeyboardLayout => "keyboard.layout",
            KeyboardVariant => "keyboard.variant",
            KeyboardOptions => "keyboard.options",
            KeyboardCapsLock => "keyboard.caps_lock",
            BlurEnabled => "blur.enabled",
            BlurRadius => "blur.radius",
            CornersEnabled => "corners.enabled",
            CornersRadius => "corners.radius",
            GapsInner => "gaps.inner",
            GapsOuter => "gaps.outer",
            BorderEnabled => "border.enabled",
            BorderThickness => "border.thickness",
            BorderColor => "border.color",
            BorderGradientColor => "border.gradient_color",
            BorderAngle => "border.angle",
            CursorTheme => "cursor.theme",
            CursorSize => "cursor.size",
            FocusFollowsMouse => "focus.follows_mouse",
            FocusMouseFollowsFocus => "focus.mouse_follows_focus",
            PerformanceFpsOverlay => "performance.fps_overlay",
            PerformanceFpsOverlayPosition => "performance.fps_overlay_position",
            PerformanceStutterThresholdMs => "performance.stutter_threshold_ms",
            PerformanceStutterLog => "performance.stutter_log",
            PerformanceFpsOverlayIntervalMs => "performance.fps_overlay_interval_ms",
            KeyTrackerEnabled => "key_tracker.enabled",
            KeyTrackerKeys => "key_tracker.keys",
            KeyTrackerPosition => "key_tracker.position",
            AppearanceDarkMode => "appearance.dark_mode",
            WorkspacesMode => "workspaces.mode",
            WorkspacesCount => "workspaces.count",
            WorkspacesDynamic => "workspaces.dynamic",
            WorkspacesOverlay => "workspaces.overlay",
            WindowAnimationsEnabled => "window_animations.enabled",
            WindowAnimationsDurationMs => "window_animations.duration_ms",
        }
    }
}

/// Reads `key`'s current value out of the effective config, formatted the
/// same way `set` expects it back (so `ironlandctl set $(ironlandctl get k)`
/// round-trips).
pub fn get(full: &FullConfig, key: SettingKey) -> String {
    let cfg = &full.config;
    use SettingKey::*;
    match key {
        Terminal => cfg.terminal.clone(),
        Browser => cfg.browser.clone(),
        FileManager => cfg.file_manager.clone(),
        TopBar => cfg.top_bar.to_string(),
        Wallpaper => cfg.wallpaper.clone().unwrap_or_default(),
        KeyboardRules => cfg.keyboard.rules.clone(),
        KeyboardModel => cfg.keyboard.model.clone(),
        KeyboardLayout => cfg.keyboard.layout.clone(),
        KeyboardVariant => cfg.keyboard.variant.clone(),
        KeyboardOptions => cfg.keyboard.options.clone(),
        KeyboardCapsLock => caps_lock_str(cfg.keyboard.caps_lock).to_string(),
        BlurEnabled => cfg.blur.enabled.to_string(),
        BlurRadius => cfg.blur.radius.to_string(),
        CornersEnabled => cfg.corners.enabled.to_string(),
        CornersRadius => cfg.corners.radius.to_string(),
        GapsInner => cfg.gaps.inner.to_string(),
        GapsOuter => cfg.gaps.outer.to_string(),
        BorderEnabled => cfg.border.enabled.to_string(),
        BorderThickness => cfg.border.thickness.to_string(),
        BorderColor => cfg.border.color.clone(),
        BorderGradientColor => cfg.border.gradient_color.clone().unwrap_or_default(),
        BorderAngle => cfg.border.angle.to_string(),
        CursorTheme => cfg.cursor.theme.clone().unwrap_or_default(),
        CursorSize => cfg.cursor.size.map(|s| s.to_string()).unwrap_or_default(),
        FocusFollowsMouse => cfg.focus.follows_mouse.to_string(),
        FocusMouseFollowsFocus => cfg.focus.mouse_follows_focus.to_string(),
        PerformanceFpsOverlay => cfg.performance.fps_overlay.to_string(),
        PerformanceFpsOverlayPosition => {
            overlay_position_str(cfg.performance.fps_overlay_position).to_string()
        }
        PerformanceStutterThresholdMs => cfg.performance.stutter_threshold_ms.to_string(),
        PerformanceStutterLog => cfg.performance.stutter_log.to_string(),
        PerformanceFpsOverlayIntervalMs => cfg.performance.fps_overlay_interval_ms.to_string(),
        KeyTrackerEnabled => cfg.key_tracker.enabled.to_string(),
        KeyTrackerKeys => cfg.key_tracker.keys.join(","),
        KeyTrackerPosition => overlay_position_str(cfg.key_tracker.position).to_string(),
        AppearanceDarkMode => full.appearance.dark_mode.to_string(),
        WorkspacesMode => workspace_mode_str(cfg.workspaces.mode).to_string(),
        WorkspacesCount => cfg.workspaces.count.to_string(),
        WorkspacesDynamic => cfg.workspaces.dynamic.to_string(),
        WorkspacesOverlay => cfg.workspaces.overlay.to_string(),
        WindowAnimationsEnabled => cfg.window_animations.enabled.to_string(),
        WindowAnimationsDurationMs => cfg.window_animations.duration_ms.to_string(),
    }
}

pub(crate) fn caps_lock_str(remap: ironland_config::CapsLockRemap) -> &'static str {
    use ironland_config::CapsLockRemap::*;
    match remap {
        Off => "off",
        Ctrl => "ctrl",
        Level3 => "level3",
    }
}

fn workspace_mode_str(mode: ironland_config::WorkspaceMode) -> &'static str {
    match mode {
        ironland_config::WorkspaceMode::PerMonitor => "per_monitor",
        ironland_config::WorkspaceMode::Combined => "combined",
    }
}

fn overlay_position_str(position: ironland_config::OverlayPosition) -> &'static str {
    use ironland_config::OverlayPosition::*;
    match position {
        TopLeft => "top_left",
        TopRight => "top_right",
        BottomLeft => "bottom_left",
        BottomRight => "bottom_right",
    }
}

fn parse_bool(raw: &str) -> Result<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        other => bail!("expected true/false, got {other:?}"),
    }
}

fn parse_u32(raw: &str) -> Result<u32> {
    raw.parse::<u32>()
        .map_err(|_| anyhow::anyhow!("expected a non-negative number, got {raw:?}"))
}

/// Validates `raw` is `#rrggbb` or `#rrggbbaa`, returning it unchanged
/// (lowercased) for storage.
fn parse_hex_color(raw: &str) -> Result<String> {
    let digits = raw
        .strip_prefix('#')
        .filter(|d| matches!(d.len(), 6 | 8) && d.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("expected #rrggbb or #rrggbbaa, got {raw:?}"))?;
    Ok(format!("#{}", digits.to_ascii_lowercase()))
}

/// A key's path into the TOML document, as `[table_path, leaf_key]`; the
/// table path is created if it doesn't exist yet.
fn table_and_leaf(key: SettingKey) -> (&'static [&'static str], &'static str) {
    use SettingKey::*;
    match key {
        Terminal => (&[], "terminal"),
        Browser => (&[], "browser"),
        FileManager => (&[], "file_manager"),
        TopBar => (&[], "top_bar"),
        Wallpaper => (&[], "wallpaper"),
        KeyboardRules => (&["keyboard"], "rules"),
        KeyboardModel => (&["keyboard"], "model"),
        KeyboardLayout => (&["keyboard"], "layout"),
        KeyboardVariant => (&["keyboard"], "variant"),
        KeyboardOptions => (&["keyboard"], "options"),
        KeyboardCapsLock => (&["keyboard"], "caps_lock"),
        BlurEnabled => (&["blur"], "enabled"),
        BlurRadius => (&["blur"], "radius"),
        CornersEnabled => (&["corners"], "enabled"),
        CornersRadius => (&["corners"], "radius"),
        GapsInner => (&["gaps"], "inner"),
        GapsOuter => (&["gaps"], "outer"),
        BorderEnabled => (&["border"], "enabled"),
        BorderThickness => (&["border"], "thickness"),
        BorderColor => (&["border"], "color"),
        BorderGradientColor => (&["border"], "gradient_color"),
        BorderAngle => (&["border"], "angle"),
        CursorTheme => (&["cursor"], "theme"),
        CursorSize => (&["cursor"], "size"),
        FocusFollowsMouse => (&["focus"], "follows_mouse"),
        FocusMouseFollowsFocus => (&["focus"], "mouse_follows_focus"),
        PerformanceFpsOverlay => (&["performance"], "fps_overlay"),
        PerformanceFpsOverlayPosition => (&["performance"], "fps_overlay_position"),
        PerformanceStutterThresholdMs => (&["performance"], "stutter_threshold_ms"),
        PerformanceStutterLog => (&["performance"], "stutter_log"),
        PerformanceFpsOverlayIntervalMs => (&["performance"], "fps_overlay_interval_ms"),
        KeyTrackerEnabled => (&["key_tracker"], "enabled"),
        KeyTrackerKeys => (&["key_tracker"], "keys"),
        KeyTrackerPosition => (&["key_tracker"], "position"),
        AppearanceDarkMode => (&["appearance"], "dark_mode"),
        WorkspacesMode => (&["workspaces"], "mode"),
        WorkspacesCount => (&["workspaces"], "count"),
        WorkspacesDynamic => (&["workspaces"], "dynamic"),
        WorkspacesOverlay => (&["workspaces"], "overlay"),
        WindowAnimationsEnabled => (&["window_animations"], "enabled"),
        WindowAnimationsDurationMs => (&["window_animations"], "duration_ms"),
    }
}

fn table_mut<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> &'a mut Table {
    let mut table = doc.as_table_mut();
    for segment in path {
        if !table.contains_key(segment) {
            table.insert(segment, Item::Table(Table::new()));
        }
        table = table[segment]
            .as_table_mut()
            .expect("config.toml has a non-table value where a settings table is expected");
    }
    table
}

/// Validates and writes `raw` into `doc` at `key`'s path.
pub fn set(doc: &mut DocumentMut, key: SettingKey, raw: &str) -> Result<()> {
    let (path, leaf) = table_and_leaf(key);
    let table = table_mut(doc, path);

    use SettingKey::*;
    match key {
        Terminal | Browser | FileManager | Wallpaper | KeyboardRules | KeyboardModel
        | KeyboardLayout | KeyboardVariant | KeyboardOptions | CursorTheme => {
            table[leaf] = value(raw);
        }
        BorderColor | BorderGradientColor => {
            table[leaf] = value(parse_hex_color(raw)?);
        }
        TopBar
        | BlurEnabled
        | CornersEnabled
        | BorderEnabled
        | FocusFollowsMouse
        | FocusMouseFollowsFocus
        | PerformanceFpsOverlay
        | KeyTrackerEnabled
        | PerformanceStutterLog
        | WorkspacesDynamic
        | WorkspacesOverlay
        | WindowAnimationsEnabled
        | AppearanceDarkMode => {
            table[leaf] = value(parse_bool(raw)?);
        }
        BlurRadius | CornersRadius => {
            let radius = parse_u32(raw)?;
            if !(1..=50).contains(&radius) {
                bail!("radius must be between 1 and 50, got {radius}");
            }
            table[leaf] = value(i64::from(radius));
        }
        GapsInner | GapsOuter => {
            table[leaf] = value(i64::from(parse_u32(raw)?));
        }
        BorderThickness => {
            let thickness = parse_u32(raw)?;
            if thickness > 32 {
                bail!("thickness must be between 0 and 32, got {thickness}");
            }
            table[leaf] = value(i64::from(thickness));
        }
        BorderAngle => {
            let angle = raw
                .parse::<f32>()
                .map_err(|_| anyhow::anyhow!("expected a number of degrees, got {raw:?}"))?;
            table[leaf] = value(f64::from(angle.rem_euclid(360.0)));
        }
        CursorSize => {
            table[leaf] = value(i64::from(parse_u32(raw)?));
        }
        KeyboardCapsLock => {
            let remap = match raw {
                "off" => "off",
                "ctrl" => "ctrl",
                "level3" => "level3",
                other => bail!("expected off, ctrl or level3, got {other:?}"),
            };
            table[leaf] = value(remap);
        }
        WorkspacesMode => {
            let mode = match raw {
                "per_monitor" => "per_monitor",
                "combined" => "combined",
                other => bail!("expected per_monitor or combined, got {other:?}"),
            };
            table[leaf] = value(mode);
        }
        KeyTrackerKeys => {
            let keys: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .collect();
            if keys.is_empty() {
                bail!("expected a comma-separated list of keysym names, e.g. Control_L,Control_R");
            }
            let mut array = toml_edit::Array::new();
            for key in keys {
                array.push(key);
            }
            table[leaf] = value(array);
        }
        PerformanceFpsOverlayPosition | KeyTrackerPosition => {
            let position = match raw {
                "top_left" => "top_left",
                "top_right" => "top_right",
                "bottom_left" => "bottom_left",
                "bottom_right" => "bottom_right",
                other => bail!(
                    "expected top_left, top_right, bottom_left or bottom_right, got {other:?}"
                ),
            };
            table[leaf] = value(position);
        }
        PerformanceStutterThresholdMs => {
            let threshold = raw
                .parse::<f32>()
                .map_err(|_| anyhow::anyhow!("expected a number of milliseconds, got {raw:?}"))?;
            if threshold < 0.0 {
                bail!("stutter threshold must be 0 (auto) or positive, got {threshold}");
            }
            table[leaf] = value(f64::from(threshold));
        }
        WorkspacesCount => {
            let count = parse_u32(raw)?;
            if count == 0 {
                bail!("workspace count must be at least 1");
            }
            table[leaf] = value(i64::from(count));
        }
        PerformanceFpsOverlayIntervalMs => {
            table[leaf] = value(i64::from(parse_u32(raw)?));
        }
        WindowAnimationsDurationMs => {
            let duration = parse_u32(raw)?;
            if duration > 2000 {
                bail!("duration must be at most 2000ms, got {duration}");
            }
            table[leaf] = value(i64::from(duration));
        }
    }
    Ok(())
}

/// Removes `key`'s override, so the effective value reverts to its default
/// on next load.
pub fn unset(doc: &mut DocumentMut, key: SettingKey) {
    let (path, leaf) = table_and_leaf(key);
    let table = table_mut(doc, path);
    table.remove(leaf);
}
