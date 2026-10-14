use super::*;

/// Possible results of a keyboard action
#[allow(dead_code)] // some of these are only read if udev is enabled
#[derive(Debug, Clone)]
pub(crate) enum KeyAction {
    /// Quit the compositor
    Quit,
    /// Trigger a vt-switch
    VtSwitch(i32),
    /// run a command
    Run(String),
    /// Switch the current screen
    Screen(usize),
    ScaleUp,
    ScaleDown,
    TogglePreview,
    RotateOutput,
    ToggleTint,
    ToggleDecorations,
    /// Float or re-tile the focused window
    ToggleFloating,
    /// Close the focused window
    KillWindow,
    /// Move keyboard focus to the tiled window in a direction
    FocusDirection(crate::shell::tiling::Direction),
    /// Swap the focused tiled window with its neighbor in a direction
    SwapDirection(crate::shell::tiling::Direction),
    /// Grow the focused tiled window towards a direction
    ResizeTiled(crate::shell::tiling::Direction),
    /// Switch the active workspace by a relative step (-1 = previous, +1 = next)
    SwitchWorkspace(i32),
    /// Move the focused window to an adjacent workspace by a relative step;
    /// the `bool` selects whether the active workspace follows the window
    /// there (true) or stays put (false).
    MoveWindowWorkspace(i32, bool),
    /// Move the focused window to the adjacent monitor in a direction
    MoveWindowMonitor(crate::shell::tiling::Direction),
    /// Open or close the application launcher
    ToggleLauncher,
    /// Lock the screen until the user authenticates with PAM again
    LockScreen,
    /// Append a character to the launcher's search query
    LauncherType(char),
    LauncherBackspace,
    LauncherUp,
    LauncherDown,
    /// Launch the currently selected application and close the launcher
    LauncherActivate,
    LauncherClose,
    /// Fires `ironland_shortcut_v1.pressed` for the named shortcut (see
    /// `config::action_for_name`'s `"shortcut:<name>"` convention and
    /// `crate::shortcuts`). Produced only on a key press; the matching
    /// release produces [`KeyAction::ShortcutReleased`] instead (tracked by
    /// keysym in `keyboard_key_to_action`, the same way plain suppressed
    /// keys are).
    Shortcut(String),
    /// Fires `ironland_shortcut_v1.released` for the named shortcut - the
    /// release half of [`KeyAction::Shortcut`].
    ShortcutReleased(String),
    /// Fires both `ironland_shortcut_v1.pressed` and `.released` for the
    /// named shortcut back to back. Used for a bare Super-tap binding (see
    /// `keybindings::super_tap_action`): the physical Super key press is
    /// forwarded to the focused client rather than intercepted, so by the
    /// time a completed tap is recognized (on release) there's no second,
    /// separate physical event left to produce the release half - unlike
    /// [`KeyAction::Shortcut`]/[`KeyAction::ShortcutReleased`], which pair up
    /// across a real key-down and key-up.
    ShortcutTap(String),
    /// Do nothing more
    None,
}

/// Translates a key press into a launcher action while the launcher overlay
/// has keyboard focus. Anything that isn't a navigation key or a printable
/// character is swallowed silently (`KeyAction::None`).
pub(super) fn launcher_key_action(keysym: Keysym) -> KeyAction {
    match keysym {
        Keysym::Escape => KeyAction::LauncherClose,
        Keysym::Return | Keysym::KP_Enter => KeyAction::LauncherActivate,
        Keysym::BackSpace => KeyAction::LauncherBackspace,
        Keysym::Up => KeyAction::LauncherUp,
        Keysym::Down => KeyAction::LauncherDown,
        _ => match xkb_utf8::keysym_to_utf8(keysym).chars().next() {
            Some(c) if !c.is_control() => KeyAction::LauncherType(c),
            _ => KeyAction::None,
        },
    }
}

/// The output workspace navigation/movement should act on: the output
/// backing the currently focused window if there is one, else whichever
/// output the pointer is over, else the first output.
pub(super) fn current_output_for_workspace_nav<BackendData: Backend>(
    state: &AnvilState<BackendData>,
) -> Option<smithay::output::Output> {
    if let Some(keyboard) = state.seat.get_keyboard()
        && let Some(crate::focus::KeyboardFocusTarget::Window(w)) = keyboard.current_focus() {
            let window = crate::shell::WindowElement(w);
            if let Some(output) = state.space.outputs_for_element(&window).first().cloned() {
                return Some(output);
            }
        }
    state
        .space
        .output_under(state.pointer.current_location())
        .next()
        .or_else(|| state.space.outputs().next())
        .cloned()
}

/// Turns a config action name (see `config::known_actions`) into the
/// `KeyAction` it triggers. Kept in sync with `config::known_actions` and
/// `config::default_shortcuts` by the test at the bottom of `config.rs`.
pub(super) fn action_for_name(
    name: &str,
    terminal: &str,
    browser: &str,
    file_manager: &str,
) -> Option<KeyAction> {
    use crate::shell::tiling::Direction;

    Some(match name {
        "quit" => KeyAction::Quit,
        "run_terminal" => KeyAction::Run(terminal.to_string()),
        "toggle_launcher" => KeyAction::ToggleLauncher,
        "lock_screen" => KeyAction::LockScreen,
        "open_browser" => KeyAction::Run(browser.to_string()),
        "open_file_manager" => KeyAction::Run(file_manager.to_string()),
        "toggle_floating" => KeyAction::ToggleFloating,
        "kill_window" => KeyAction::KillWindow,
        "focus_left" => KeyAction::FocusDirection(Direction::Left),
        "focus_right" => KeyAction::FocusDirection(Direction::Right),
        "focus_up" => KeyAction::FocusDirection(Direction::Up),
        "focus_down" => KeyAction::FocusDirection(Direction::Down),
        "swap_left" => KeyAction::SwapDirection(Direction::Left),
        "swap_right" => KeyAction::SwapDirection(Direction::Right),
        "swap_up" => KeyAction::SwapDirection(Direction::Up),
        "swap_down" => KeyAction::SwapDirection(Direction::Down),
        "resize_left" => KeyAction::ResizeTiled(Direction::Left),
        "resize_right" => KeyAction::ResizeTiled(Direction::Right),
        "resize_up" => KeyAction::ResizeTiled(Direction::Up),
        "resize_down" => KeyAction::ResizeTiled(Direction::Down),
        "workspace_left" => KeyAction::SwitchWorkspace(-1),
        "workspace_right" => KeyAction::SwitchWorkspace(1),
        "move_workspace_left" => KeyAction::MoveWindowWorkspace(-1, false),
        "move_workspace_right" => KeyAction::MoveWindowWorkspace(1, false),
        "move_workspace_left_follow" => KeyAction::MoveWindowWorkspace(-1, true),
        "move_workspace_right_follow" => KeyAction::MoveWindowWorkspace(1, true),
        "move_monitor_left" => KeyAction::MoveWindowMonitor(Direction::Left),
        "move_monitor_right" => KeyAction::MoveWindowMonitor(Direction::Right),
        "move_monitor_up" => KeyAction::MoveWindowMonitor(Direction::Up),
        "move_monitor_down" => KeyAction::MoveWindowMonitor(Direction::Down),
        "scale_up" => KeyAction::ScaleUp,
        "scale_down" => KeyAction::ScaleDown,
        "toggle_preview" => KeyAction::TogglePreview,
        "rotate_output" => KeyAction::RotateOutput,
        "toggle_tint" => KeyAction::ToggleTint,
        "toggle_decorations" => KeyAction::ToggleDecorations,
        _ => {
            let shortcut_name = name.strip_prefix("shortcut:")?;
            KeyAction::Shortcut(shortcut_name.to_string())
        },
    })
}

/// Resolves every keybinding from `config` into the table
/// `keyboard_key_to_action` consults on each key press. Rebuilt whenever the
/// config changes so bindings and their associated commands update live.
pub(crate) fn compile_keybindings(
    config: &crate::config::Config,
) -> Vec<(crate::keybindings::KeyModifiers, Keysym, KeyAction)> {
    crate::keybindings::parsed_keybindings(config)
        .into_iter()
        .filter_map(|binding| {
            match action_for_name(
                &binding.action,
                &config.terminal,
                &config.browser,
                &config.file_manager,
            ) {
                Some(action) => Some((binding.modifiers, binding.keysym, action)),
                None => {
                    // `parsed_keybindings` already validated the name against
                    // `known_actions`, so this would mean the two tables
                    // drifted apart - a bug here, not a bad config file.
                    error!(
                        action = binding.action,
                        "No KeyAction for a known config action name"
                    );
                    None
                }
            }
        })
        .collect()
}

/// Resolves the `KeyAction` to fire on a bare Super key tap (see
/// [`crate::keybindings::super_tap_action`]), if one is configured. Called
/// once at startup alongside `compile_keybindings`.
pub(crate) fn compile_super_tap_action(config: &crate::config::Config) -> Option<KeyAction> {
    let action_name = crate::keybindings::super_tap_action(config)?;
    match action_for_name(
        action_name,
        &config.terminal,
        &config.browser,
        &config.file_manager,
    ) {
        Some(action) => Some(action),
        None => {
            error!(
                action = action_name,
                "No KeyAction for a known config action name"
            );
            None
        }
    }
}

/// Whether `keysym` is one of the physical Super/logo keys, used to detect a
/// bare Super tap (see `keybindings::super_tap_action`).
pub(super) fn is_super_keysym(keysym: Keysym) -> bool {
    matches!(keysym, Keysym::Super_L | Keysym::Super_R)
}

/// The dynamic shortcuts that aren't representable as a single fixed
/// modifiers+key combo (a VT switch key or the digit is itself part of the
/// action) and so aren't part of the configurable keybinding table.
pub(super) fn process_dynamic_shortcut(modifiers: ModifiersState, keysym: Keysym) -> Option<KeyAction> {
    if (xkb::KEY_XF86Switch_VT_1..=xkb::KEY_XF86Switch_VT_12).contains(&keysym.raw()) {
        // VTSwitch
        Some(KeyAction::VtSwitch(
            (keysym.raw() - xkb::KEY_XF86Switch_VT_1 + 1) as i32,
        ))
    } else if modifiers.ctrl && (xkb::KEY_1..=xkb::KEY_9).contains(&keysym.raw()) {
        Some(KeyAction::Screen((keysym.raw() - xkb::KEY_1) as usize))
    } else {
        None
    }
}

pub(super) fn process_keyboard_shortcut(
    keybindings: &[(crate::keybindings::KeyModifiers, Keysym, KeyAction)],
    bound_shortcuts: &[(crate::keybindings::KeyModifiers, Keysym, String)],
    modifiers: ModifiersState,
    keysym: Keysym,
) -> Option<KeyAction> {
    process_dynamic_shortcut(modifiers, keysym)
        .or_else(|| {
            keybindings
                .iter()
                .find(|(binding_mods, binding_sym, _)| {
                    *binding_sym == keysym && binding_mods.matches(&modifiers)
                })
                .map(|(_, _, action)| action.clone())
        })
        .or_else(|| {
            // Triggers registered dynamically through `ironland-shortcuts-v1`'s
            // `bind` request (see `crate::shortcuts::ShortcutsManagerState::
            // dynamic_bindings`) - checked last so a `[shortcuts]` config
            // entry or `get_shortcut` name for the same combo always wins.
            bound_shortcuts
                .iter()
                .find(|(binding_mods, binding_sym, _)| {
                    *binding_sym == keysym && binding_mods.matches(&modifiers)
                })
                .map(|(_, _, name)| KeyAction::Shortcut(name.clone()))
        })
}
