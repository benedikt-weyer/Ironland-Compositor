use super::*;

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Environment variables that point a spawned process at this compositor's
    /// own sockets, so it connects here instead of whatever session the
    /// compositor itself was started from.
    /// Environment for spawned clients: point them at this compositor's own
    /// Wayland socket and, when there's no XWayland to fall back to, strip any
    /// inherited `DISPLAY` so X11-capable toolkits can't quietly reconnect to
    /// the host's X server instead of rendering here. `None` means "unset".
    pub(super) fn compositor_envs(&self) -> impl Iterator<Item = (&'static str, Option<String>)> {
        let wayland_display = self
            .socket_name
            .clone()
            .map(|v| ("WAYLAND_DISPLAY", Some(v)));

        #[cfg(feature = "xwayland")]
        let display = Some(("DISPLAY", self.xdisplay.map(|v| format!(":{v}"))));
        #[cfg(not(feature = "xwayland"))]
        let display = Some(("DISPLAY", None));

        // Force Wayland-capable toolkits to use this compositor rather than
        // falling back to X11 (which, without XWayland, would fail anyway).
        let force_wayland = [
            ("GDK_BACKEND", Some("wayland".to_string())),
            ("QT_QPA_PLATFORM", Some("wayland".to_string())),
            ("SDL_VIDEODRIVER", Some("wayland".to_string())),
            ("CLUTTER_BACKEND", Some("wayland".to_string())),
            ("MOZ_ENABLE_WAYLAND", Some("1".to_string())),
            ("NIXOS_OZONE_WL", Some("1".to_string())),
        ];

        wayland_display
            .into_iter()
            .chain(display)
            .chain(force_wayland)
    }

    /// Applies [`compositor_envs`](Self::compositor_envs) to `cmd`, setting or
    /// unsetting each variable as appropriate.
    pub(super) fn apply_compositor_envs(&self, cmd: &mut Command) {
        for (key, value) in self.compositor_envs() {
            match value {
                Some(value) => {
                    cmd.env(key, value);
                }
                None => {
                    cmd.env_remove(key);
                }
            }
        }
    }

    // Allow in this method because of existing usage
    #[allow(clippy::uninlined_format_args)]
    pub(super) fn process_common_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::None => (),

            KeyAction::Quit => {
                info!("Quitting.");
                self.running.store(false, Ordering::SeqCst);
            }

            KeyAction::Run(cmd) => {
                info!(cmd, "Starting program");

                // Routed through the user's login shell rather than
                // exec'd directly: the compositor's own process
                // environment is whatever its systemd unit was started
                // with, which can have a stripped-down PATH (and miss
                // other session-wide env the user's shell rc/profile
                // sets up). `-lc` re-sources that profile so spawned
                // programs see the same environment they would from a
                // normal interactive shell - matching the companion
                // shell's own app launcher, which wraps every launch the
                // same way for the same reason.
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                let mut command = Command::new(&shell);
                command.arg("-lc").arg(&cmd);
                self.apply_compositor_envs(&mut command);
                if let Err(e) = command.spawn() {
                    error!(cmd, err = %e, "Failed to start program");
                }
            }

            KeyAction::TogglePreview => {
                self.show_window_preview = !self.show_window_preview;
            }

            KeyAction::ToggleLauncher => {
                self.launcher.toggle();
            }

            KeyAction::LockScreen => self.lock_screen(),

            KeyAction::ToggleOverview => self.toggle_overview(),

            KeyAction::SuperDoubleTap => {
                // The first tap of the pair already ran the Super-tap action;
                // when that's a toggle (the launcher), run it again so the
                // double tap nets out to just the overview.
                match self.super_tap_action.clone() {
                    Some(KeyAction::ToggleLauncher) => self.process_common_key_action(KeyAction::ToggleLauncher),
                    Some(KeyAction::Shortcut(name)) => {
                        self.process_common_key_action(KeyAction::ShortcutTap(name))
                    }
                    _ => {}
                }
                self.open_overview();
            }

            KeyAction::LauncherType(c) => {
                self.launcher.push_char(c);
            }

            KeyAction::LauncherBackspace => {
                self.launcher.backspace();
            }

            KeyAction::LauncherUp => {
                self.launcher.move_selection(-1);
            }

            KeyAction::LauncherDown => {
                self.launcher.move_selection(1);
            }

            KeyAction::LauncherClose => {
                self.launcher.close();
            }

            KeyAction::LauncherActivate => {
                if let Some(entry) = self.launcher.activate() {
                    let envs: Vec<_> = self.compositor_envs().collect();
                    crate::launcher::launch_and_log(&entry, &envs);
                }
            }

            KeyAction::ToggleDecorations => {
                for element in self.space.elements() {
                    #[allow(irrefutable_let_patterns)]
                    if let Some(toplevel) = element.0.toplevel() {
                        let mode_changed = toplevel.with_pending_state(|state| {
                            if let Some(current_mode) = state.decoration_mode {
                                let new_mode = if current_mode
                                    == zxdg_toplevel_decoration_v1::Mode::ClientSide
                                {
                                    zxdg_toplevel_decoration_v1::Mode::ServerSide
                                } else {
                                    zxdg_toplevel_decoration_v1::Mode::ClientSide
                                };
                                state.decoration_mode = Some(new_mode);
                                true
                            } else {
                                false
                            }
                        });

                        if mode_changed && toplevel.is_initial_configure_sent() {
                            toplevel.send_pending_configure();
                        }
                    }
                }
            }

            KeyAction::ToggleFloating => {
                if let Some(keyboard) = self.seat.get_keyboard()
                    && let Some(crate::focus::KeyboardFocusTarget::Window(w)) =
                        keyboard.current_focus()
                    {
                        crate::shell::tiling::toggle_floating(
                            self,
                            &crate::shell::WindowElement(w),
                        );
                    }
            }

            KeyAction::KillWindow => {
                if let Some(keyboard) = self.seat.get_keyboard()
                    && let Some(crate::focus::KeyboardFocusTarget::Window(w)) =
                        keyboard.current_focus()
                    {
                        match w.underlying_surface() {
                            smithay::desktop::WindowSurface::Wayland(toplevel) => {
                                toplevel.send_close();
                            }
                            #[cfg(feature = "xwayland")]
                            smithay::desktop::WindowSurface::X11(surface) => {
                                let _ = surface.close();
                            }
                        }
                    }
            }

            KeyAction::FocusDirection(dir) => {
                crate::shell::tiling::focus_direction(self, dir);
            }

            KeyAction::SwapDirection(dir) => {
                crate::shell::tiling::swap_direction(self, dir);
            }

            KeyAction::ResizeTiled(dir) => {
                crate::shell::tiling::resize_tiled(self, dir);
            }

            KeyAction::SwitchWorkspace(delta) => {
                if let Some(output) = current_output_for_workspace_nav(self) {
                    crate::shell::workspace::switch_workspace(self, &output, delta);
                    crate::ext_workspace::ext_workspace_sync(self);
                }
            }

            KeyAction::MoveWindowMonitor(dir) => {
                crate::shell::workspace::move_focused_window_to_monitor(self, dir);
                crate::ext_workspace::ext_workspace_sync(self);
            }

            KeyAction::MoveWindowWorkspace(delta, follow) => {
                crate::shell::workspace::move_focused_window(self, delta, follow);
                crate::ext_workspace::ext_workspace_sync(self);
            }

            KeyAction::Shortcut(name) => {
                let output = self
                    .space
                    .output_under(self.pointer.current_location())
                    .next()
                    .map(|o| o.name());
                crate::shortcuts::fire(self, &name, true, output.as_deref());
            }
            KeyAction::ShortcutReleased(name) => crate::shortcuts::fire(self, &name, false, None),

            KeyAction::ShortcutTap(name) => {
                let output = self
                    .space
                    .output_under(self.pointer.current_location())
                    .next()
                    .map(|o| o.name());
                crate::shortcuts::fire(self, &name, true, output.as_deref());
                crate::shortcuts::fire(self, &name, false, None);
            }

            _ => unreachable!(
                "Common key action handler encountered backend specific action {:?}",
                action
            ),
        }
    }

    pub(super) fn keyboard_key_to_action<B: InputBackend>(&mut self, evt: B::KeyboardKeyEvent) -> KeyAction {
        let keycode = evt.key_code();
        let state = evt.state();
        debug!(?keycode, ?state, "key");
        let serial = SCOUNTER.next_serial();
        let time = Event::time(&evt);
        let mut suppressed_keys = self.suppressed_keys.clone();
        let mut held_shortcut_keys = self.held_shortcut_keys.clone();
        let keyboard = self.seat.get_keyboard().unwrap();

        let locked = self.lock.is_locked();

        if !locked && let KeyState::Pressed = state {
            let focused = keyboard
                .current_focus()
                .and_then(|f| f.wl_surface().map(|s| s.into_owned()));
            crate::focus_grab::check(self, focused.as_ref());
        }

        // While locked, no layer surface (a shell's bar, say) may steal focus.
        for layer in self.layer_shell_state.layer_surfaces().rev().filter(|_| !locked) {
            let exclusive = layer.with_cached_state(|data| {
                data.keyboard_interactivity == KeyboardInteractivity::Exclusive
                    && (data.layer == WlrLayer::Top || data.layer == WlrLayer::Overlay)
            });
            if exclusive {
                let surface = self.space.outputs().find_map(|o| {
                    let map = layer_map_for_output(o);
                    map.layers().find(|l| l.layer_surface() == &layer).cloned()
                });
                if let Some(surface) = surface {
                    keyboard.set_focus(self, Some(surface.into()), serial);
                    keyboard.input::<(), _>(self, keycode, state, serial, time, |data, _, handle| {
                        data.key_tracker
                            .record(&handle.raw_syms(), state == KeyState::Pressed);
                        FilterResult::Forward
                    });
                    return KeyAction::None;
                };
            }
        }

        let inhibited = self
            .space
            .element_under(self.pointer.current_location())
            .and_then(|(window, _)| {
                let surface = window.wl_surface()?;
                self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface)
            })
            .map(|inhibitor| inhibitor.is_active())
            .unwrap_or(false);

        let action = keyboard
            .input(
                self,
                keycode,
                state,
                serial,
                time,
                |data, modifiers, handle| {
                    let keysym = handle.modified_sym();

                    data.key_tracker
                        .record(&handle.raw_syms(), state == KeyState::Pressed);

                    debug!(
                        ?state,
                        mods = ?modifiers,
                        keysym = ::xkbcommon::xkb::keysym_get_name(keysym),
                        "keysym"
                    );

                    // The screen lock grabs the whole keyboard before anything
                    // else (even the Super-tap and every shortcut): keys only
                    // edit the password, Enter submits it. VT switches stay
                    // available - the other VT has its own login.
                    if data.lock.is_locked() {
                        // Keys still held from before the lock (the `l` of
                        // Super+L, Super itself) release here: forget them
                        // so they neither stay "suppressed" nor leave a bare
                        // Super tap half-tracked once unlocked.
                        data.super_tap_pending = None;
                        if state == KeyState::Released {
                            suppressed_keys.retain(|k| *k != keysym);
                            held_shortcut_keys.remove(&keysym);
                        }
                        if let KeyState::Pressed = state {
                            if (xkb::KEY_XF86Switch_VT_1..=xkb::KEY_XF86Switch_VT_12)
                                .contains(&keysym.raw())
                            {
                                return FilterResult::Intercept(KeyAction::VtSwitch(
                                    (keysym.raw() - xkb::KEY_XF86Switch_VT_1 + 1) as i32,
                                ));
                            }
                            match keysym {
                                Keysym::Return | Keysym::KP_Enter => data.lock.submit(),
                                Keysym::BackSpace => data.lock.backspace(),
                                Keysym::Escape => data.lock.clear(),
                                _ if modifiers.ctrl || modifiers.alt || modifiers.logo => {}
                                _ => {
                                    if let Some(c) = xkb_utf8::keysym_to_utf8(keysym)
                                        .chars()
                                        .next()
                                        .filter(|c| !c.is_control())
                                    {
                                        data.lock.push_char(c);
                                    }
                                }
                            }
                        }
                        return FilterResult::Intercept(KeyAction::None);
                    }

                    // Track whether Super is being tapped alone (pressed and
                    // released with no other key in between), to fire the
                    // configured `super_tap_action` (see `config::Config::
                    // super_tap_action`). The Super key itself is always
                    // forwarded to the focused client like any other modifier
                    // key - only a successful tap's release is intercepted, the
                    // same trade-off other compositors make for this feature.
                    if is_super_keysym(keysym) {
                        return if let KeyState::Pressed = state {
                            if data.super_tap_pending.is_none() {
                                data.super_tap_pending = Some(true);
                            }
                            FilterResult::Forward
                        } else {
                            let was_tap = data.super_tap_pending == Some(true);
                            data.super_tap_pending = None;
                            // A tap while the overview is open closes it.
                            if was_tap && data.overview.is_visible() {
                                data.last_super_tap = None;
                                return FilterResult::Intercept(KeyAction::ToggleOverview);
                            }
                            // Two taps in quick succession open the overview.
                            if was_tap && !inhibited {
                                let now = std::time::Instant::now();
                                let double = data.last_super_tap.take().is_some_and(|last| {
                                    now.duration_since(last).as_millis()
                                        <= crate::overview::DOUBLE_TAP_MS
                                });
                                if double {
                                    return FilterResult::Intercept(KeyAction::SuperDoubleTap);
                                }
                                data.last_super_tap = Some(now);
                            }
                            match (was_tap, inhibited, &data.super_tap_action) {
                                (true, false, Some(KeyAction::Shortcut(name))) => {
                                    FilterResult::Intercept(KeyAction::ShortcutTap(name.clone()))
                                }
                                (true, false, Some(action)) => {
                                    FilterResult::Intercept(action.clone())
                                }
                                _ => FilterResult::Forward,
                            }
                        };
                    } else if let KeyState::Pressed = state {
                        // Only actually breaks a tap in progress (Super
                        // currently held down). Otherwise-unconditional
                        // `Some(false)` here would flip `None` to
                        // `Some(false)` on every ordinary keypress (typing
                        // into a text field, say) while Super isn't even
                        // held - stale state that then silently no-ops the
                        // *next* bare Super tap (its release finds
                        // `Some(false)` instead of `None`, so the `is_none()`
                        // check above never runs and `was_tap` reads false),
                        // requiring a second press to actually open the
                        // launcher.
                        if data.super_tap_pending == Some(true) {
                            data.super_tap_pending = Some(false);
                        }
                    }

                    // While a screen-capture permission prompt (see
                    // `crate::permission_prompt`) is showing it grabs the
                    // whole keyboard, same as the launcher below: every key
                    // is consumed, Enter/Y allow and Escape/N deny.
                    if data.permission_prompt.is_visible() {
                        if let KeyState::Pressed = state {
                            match keysym {
                                Keysym::Return | Keysym::KP_Enter | Keysym::y | Keysym::Y => {
                                    crate::permission_prompt::answer(data, true);
                                }
                                Keysym::Escape | Keysym::n | Keysym::N => {
                                    crate::permission_prompt::answer(data, false);
                                }
                                _ => {}
                            }
                            suppressed_keys.push(keysym);
                        } else {
                            suppressed_keys.retain(|k| *k != keysym);
                        }
                        return FilterResult::Intercept(KeyAction::None);
                    }

                    // The monitor chooser (also `crate::permission_prompt`) grabs
                    // the keyboard the same way: number keys pick that monitor,
                    // Escape cancels, everything else is consumed.
                    if data.permission_prompt.chooser_visible() {
                        if let KeyState::Pressed = state {
                            let number = match keysym {
                                Keysym::_1 | Keysym::KP_1 => Some(1),
                                Keysym::_2 | Keysym::KP_2 => Some(2),
                                Keysym::_3 | Keysym::KP_3 => Some(3),
                                Keysym::_4 | Keysym::KP_4 => Some(4),
                                Keysym::_5 | Keysym::KP_5 => Some(5),
                                Keysym::_6 | Keysym::KP_6 => Some(6),
                                Keysym::_7 | Keysym::KP_7 => Some(7),
                                Keysym::_8 | Keysym::KP_8 => Some(8),
                                Keysym::_9 | Keysym::KP_9 => Some(9),
                                _ => None,
                            };
                            if keysym == Keysym::Escape {
                                crate::permission_prompt::resolve_chooser(
                                    data,
                                    crate::permission_prompt::ChooserChoice::Cancel,
                                );
                            } else if let Some(choice) =
                                number.and_then(|n| data.permission_prompt.chooser_choice_for_number(n))
                            {
                                crate::permission_prompt::resolve_chooser(data, choice);
                            }
                            suppressed_keys.push(keysym);
                        } else {
                            suppressed_keys.retain(|k| *k != keysym);
                        }
                        return FilterResult::Intercept(KeyAction::None);
                    }

                    // The workspace overview grabs the keyboard too: Escape/Enter
                    // close it, everything else is swallowed.
                    if data.overview.is_visible() {
                        if let KeyState::Pressed = state {
                            suppressed_keys.push(keysym);
                            return FilterResult::Intercept(match keysym {
                                Keysym::Escape | Keysym::Return | Keysym::KP_Enter => {
                                    KeyAction::ToggleOverview
                                }
                                _ => KeyAction::None,
                            });
                        }
                        suppressed_keys.retain(|k| *k != keysym);
                        return FilterResult::Intercept(KeyAction::None);
                    }

                    // While the launcher overlay is open it grabs the whole keyboard:
                    // every key is consumed here instead of being forwarded to the
                    // focused client, whether or not it maps to a launcher action.
                    if data.launcher.is_visible() && !(modifiers.ctrl && keysym == Keysym::space) {
                        if let KeyState::Pressed = state {
                            let action = launcher_key_action(keysym);
                            suppressed_keys.push(keysym);
                            return FilterResult::Intercept(action);
                        } else {
                            suppressed_keys.retain(|k| *k != keysym);
                            return FilterResult::Intercept(KeyAction::None);
                        }
                    }

                    // If the key is pressed and triggered a action
                    // we will not forward the key to the client.
                    // Additionally add the key to the suppressed keys
                    // so that we can decide on a release if the key
                    // should be forwarded to the client or not.
                    if let KeyState::Pressed = state {
                        if !inhibited {
                            let bound_shortcuts =
                                data.shortcuts_state().dynamic_bindings().to_vec();
                            let action = process_keyboard_shortcut(
                                &data.keybindings,
                                &bound_shortcuts,
                                *modifiers,
                                keysym,
                            );

                            if let Some(KeyAction::Shortcut(name)) = &action {
                                held_shortcut_keys.insert(keysym, name.clone());
                            }

                            if action.is_some() {
                                suppressed_keys.push(keysym);
                            }

                            action
                                .map(FilterResult::Intercept)
                                .unwrap_or(FilterResult::Forward)
                        } else {
                            FilterResult::Forward
                        }
                    } else {
                        let suppressed = suppressed_keys.contains(&keysym);
                        if suppressed {
                            suppressed_keys.retain(|k| *k != keysym);
                            match held_shortcut_keys.remove(&keysym) {
                                Some(name) => FilterResult::Intercept(KeyAction::ShortcutReleased(name)),
                                None => FilterResult::Intercept(KeyAction::None),
                            }
                        } else {
                            FilterResult::Forward
                        }
                    }
                },
            )
            .unwrap_or(KeyAction::None);

        self.held_shortcut_keys = held_shortcut_keys;

        self.suppressed_keys = suppressed_keys;
        action
    }

}
