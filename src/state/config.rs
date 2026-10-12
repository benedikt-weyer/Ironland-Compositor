use super::*;

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    /// Reloads and applies the effective config after it changes on disk.
    /// Invalid TOML is deliberately ignored so the currently running setup
    /// remains intact while the user fixes the file.
    pub fn reload_config_if_changed(&mut self) {
        if self.config_last_checked.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.config_last_checked = Instant::now();

        let (new_config, path) = match crate::config::Config::try_load() {
            Ok(loaded) => loaded,
            Err(err) => {
                warn!(%err, "Config reload skipped");
                return;
            }
        };
        if new_config == self.config {
            return;
        }

        self.apply_config(new_config);
        info!(path = ?path, "Applied compositor config without restarting");
    }

    pub(super) fn apply_config(&mut self, new_config: crate::config::Config) {
        let old_config = self.config.clone();

        if new_config.keyboard != old_config.keyboard {
            let keyboard_settings = new_config.keyboard.clone();
            if let Some(keyboard) = self.seat.get_keyboard()
                && let Err(err) = keyboard.set_xkb_config(self, crate::keybindings::to_xkb_config(&keyboard_settings))
            {
                warn!(
                    ?err,
                    "Failed to apply keyboard config; keeping the previous keymap"
                );
            }
        }

        self.keybindings = crate::input_handler::compile_keybindings(&new_config);
        self.super_tap_action = crate::input_handler::compile_super_tap_action(&new_config);
        self.super_tap_pending = None;
        self.gesture_bindings = crate::input_handler::compile_gesture_bindings(&new_config);
        self.gesture_tracker = None;

        if new_config.wallpaper != old_config.wallpaper {
            self.wallpaper = crate::wallpaper::Wallpaper::load(new_config.wallpaper.as_deref());
        }

        if new_config.key_tracker != old_config.key_tracker {
            self.key_tracker = crate::key_tracker::KeyTracker::new(&new_config.key_tracker);
        }

        let top_bar_changed = new_config.top_bar != old_config.top_bar;
        self.config = new_config;

        if top_bar_changed {
            use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
            let mode = if self.config.top_bar {
                Mode::ServerSide
            } else {
                Mode::ClientSide
            };
            let windows: Vec<_> = self.space.elements().cloned().collect();
            for window in windows {
                window.set_ssd(self.config.top_bar);
                if let Some(toplevel) = window.0.toplevel() {
                    toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));
                    if toplevel.is_initial_configure_sent() {
                        toplevel.send_pending_configure();
                    }
                }
            }
        }

        if self.config.workspaces != old_config.workspaces {
            crate::shell::workspace::apply_config(self);
        }
        if self.config.outputs != old_config.outputs {
            BackendData::apply_output_config(self, &old_config);
            self.apply_output_positions();
        }
        if self.config.cursor != old_config.cursor {
            BackendData::apply_cursor_config(self);
        }
        crate::ext_workspace::ext_workspace_sync(self);
    }

    pub(super) fn apply_output_positions(&mut self) {
        let mut pending: Vec<Output> = self.space.outputs().cloned().collect();
        let connected_names: Vec<String> = pending.iter().map(Output::name).collect();
        let mut placed: Vec<(String, Rectangle<i32, Logical>)> = Vec::new();

        while !pending.is_empty() {
            let ready = pending.iter().position(|output| {
                let settings = self.config.output_settings(&output.name());
                let reference =
                    settings
                        .mirror_of
                        .as_ref()
                        .or(match settings.position.as_ref() {
                            Some(crate::config::OutputPosition::RightOf { right_of }) => {
                                Some(right_of)
                            }
                            Some(crate::config::OutputPosition::LeftOf { left_of }) => {
                                Some(left_of)
                            }
                            Some(crate::config::OutputPosition::Above { above }) => Some(above),
                            Some(crate::config::OutputPosition::Below { below }) => Some(below),
                            _ => None,
                        });
                reference.is_none_or(|name| {
                    !connected_names.contains(name) || placed.iter().any(|(n, _)| n == name)
                })
            });

            // A cycle in relative placement cannot be satisfied. Resolve one
            // member with the normal fallback, then the remainder can follow.
            let output = pending.remove(ready.unwrap_or(0));
            let geometry = self.space.output_geometry(&output).unwrap_or_default();
            let position = crate::keybindings::resolve_output_position(
                &self.config.output_settings(&output.name()),
                &output.name(),
                geometry.size,
                &placed,
            );
            self.space.map_output(&output, position);
            placed.push((output.name(), Rectangle::new(position, geometry.size)));
        }

        if let Some(primary) = self.config.primary_output_name() {
            let outputs: Vec<(Output, Point<i32, Logical>)> = self
                .space
                .outputs()
                .map(|output| {
                    (
                        output.clone(),
                        self.space.output_geometry(output).unwrap().loc,
                    )
                })
                .collect();
            if outputs.iter().any(|(output, _)| output.name() == primary) {
                for (output, _) in &outputs {
                    self.space.unmap_output(output);
                }
                for (output, location) in outputs
                    .iter()
                    .filter(|(output, _)| output.name() == primary)
                {
                    self.space.map_output(output, *location);
                }
                for (output, location) in outputs
                    .iter()
                    .filter(|(output, _)| output.name() != primary)
                {
                    self.space.map_output(output, *location);
                }
            }
        }
        self.space.refresh();
    }
}
