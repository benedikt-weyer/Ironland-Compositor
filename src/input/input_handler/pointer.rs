use super::*;

impl<BackendData: Backend> AnvilState<BackendData> {
    pub(super) fn on_pointer_button<B: InputBackend>(&mut self, evt: B::PointerButtonEvent) {
        let serial = SCOUNTER.next_serial();
        let button = evt.button_code();

        let state = wl_pointer::ButtonState::from(evt.state());

        if self.lock.is_locked() {
            return;
        }

        // While a permission prompt (see `crate::permission_prompt`) is
        // showing it grabs the whole pointer too, same as the keyboard
        // grab below: every button is consumed, and a left-click release
        // lands answering it if it's on Allow/Deny. Answering on release
        // (not press) matches how a normal button widget behaves - a press
        // that drags off the button before releasing doesn't activate it.
        if self.permission_prompt.is_visible() {
            if wl_pointer::ButtonState::Released == state && evt.button() == Some(MouseButton::Left) {
                let location = self.pointer.current_location();
                let hit = self.space.output_under(location).next().and_then(|output| {
                    let output_geo = self.space.output_geometry(output).unwrap();
                    let local = (location - output_geo.loc.to_f64()).to_i32_round();
                    self.permission_prompt.hit_test(output_geo.size, local)
                });
                if let Some(allow) = hit {
                    crate::permission_prompt::answer(self, allow);
                }
            }
            return;
        }

        // Same grab for the monitor chooser: a left-click release on one of
        // its rows picks that monitor (or cancels).
        if self.permission_prompt.chooser_visible() {
            if wl_pointer::ButtonState::Released == state && evt.button() == Some(MouseButton::Left) {
                let location = self.pointer.current_location();
                let hit = self.space.output_under(location).next().and_then(|output| {
                    let output_geo = self.space.output_geometry(output).unwrap();
                    let local = (location - output_geo.loc.to_f64()).to_i32_round();
                    self.permission_prompt.chooser_hit_test(output_geo.size, local)
                });
                if let Some(choice) = hit {
                    crate::permission_prompt::resolve_chooser(self, choice);
                }
            }
            return;
        }

        if wl_pointer::ButtonState::Pressed == state {
            let super_held = self
                .seat
                .get_keyboard()
                .map(|keyboard| keyboard.modifier_state().logo)
                .unwrap_or(false);
            if super_held
                && evt.button() == Some(MouseButton::Left)
                && !self.pointer.is_grabbed()
                && self.start_super_move_grab(serial, button)
            {
                return;
            }

            self.update_keyboard_focus(self.pointer.current_location(), serial);

            let focused = self
                .pointer
                .current_focus()
                .and_then(|f| f.wl_surface().map(|s| s.into_owned()));
            crate::focus_grab::check(self, focused.as_ref());
        };
        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: state.try_into().unwrap(),
                serial,
                time: evt.time(),
            },
        );
        pointer.frame(self);
    }

    pub(super) fn update_keyboard_focus(&mut self, location: Point<f64, Logical>, serial: Serial) {
        if self.lock.is_locked() {
            return;
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        let touch = self.seat.get_touch();
        let input_method = self.seat.input_method();
        // change the keyboard focus unless the pointer or keyboard is grabbed
        // We test for any matching surface type here but always use the root
        // (in case of a window the toplevel) surface for the focus.
        // So for example if a user clicks on a subsurface or popup the toplevel
        // will receive the keyboard focus. Directly assigning the focus to the
        // matching surface leads to issues with clients dismissing popups and
        // subsurface menus (for example firefox-wayland).
        // see here for a discussion about that issue:
        // https://gitlab.freedesktop.org/wayland/wayland/-/issues/294
        if !self.pointer.is_grabbed()
            && (!keyboard.is_grabbed() || input_method.keyboard_grabbed())
            && !touch.map(|touch| touch.is_grabbed()).unwrap_or(false)
        {
            let output = self.space.output_under(location).next().cloned();
            if let Some(output) = output.as_ref() {
                let output_geo = self.space.output_geometry(output).unwrap();
                if let Some(window) = output
                    .user_data()
                    .get::<FullscreenSurface>()
                    .and_then(|f| f.get())
                    && let Some((_, _)) = window
                        .surface_under(location - output_geo.loc.to_f64(), WindowSurfaceType::ALL)
                    {
                        #[cfg(feature = "xwayland")]
                        if let Some(surface) = window.0.x11_surface() {
                            self.xwm.as_mut().unwrap().raise_window(surface).unwrap();
                        }
                        keyboard.set_focus(self, Some(window.into()), serial);
                        return;
                    }

                let layers = layer_map_for_output(output);
                if let Some(layer) = layers
                    .layer_under(WlrLayer::Overlay, location - output_geo.loc.to_f64())
                    .or_else(|| {
                        layers.layer_under(WlrLayer::Top, location - output_geo.loc.to_f64())
                    })
                    && layer.can_receive_keyboard_focus()
                        && let Some((_, _)) = layer.surface_under(
                            location
                                - output_geo.loc.to_f64()
                                - layers.layer_geometry(layer).unwrap().loc.to_f64(),
                            WindowSurfaceType::ALL,
                        ) {
                            keyboard.set_focus(self, Some(layer.clone().into()), serial);
                            return;
                        }
            }

            if let Some((window, _)) = self
                .space
                .element_under(location)
                .map(|(w, p)| (w.clone(), p))
            {
                self.space.raise_element(&window, true);
                #[cfg(feature = "xwayland")]
                if let Some(surface) = window.0.x11_surface() {
                    self.xwm.as_mut().unwrap().raise_window(surface).unwrap();
                }
                keyboard.set_focus(self, Some(window.into()), serial);
                return;
            }

            if let Some(output) = output.as_ref() {
                let output_geo = self.space.output_geometry(output).unwrap();
                let layers = layer_map_for_output(output);
                if let Some(layer) = layers
                    .layer_under(WlrLayer::Bottom, location - output_geo.loc.to_f64())
                    .or_else(|| {
                        layers.layer_under(WlrLayer::Background, location - output_geo.loc.to_f64())
                    })
                    && layer.can_receive_keyboard_focus()
                        && let Some((_, _)) = layer.surface_under(
                            location
                                - output_geo.loc.to_f64()
                                - layers.layer_geometry(layer).unwrap().loc.to_f64(),
                            WindowSurfaceType::ALL,
                        ) {
                            keyboard.set_focus(self, Some(layer.clone().into()), serial);
                        }
            };
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        // Nothing is reachable behind the screen lock.
        if self.lock.is_locked() {
            return None;
        }
        let output = self.space.outputs().find(|o| {
            let geometry = self.space.output_geometry(o).unwrap();
            geometry.contains(pos.to_i32_round())
        })?;
        let output_geo = self.space.output_geometry(output).unwrap();
        let layers = layer_map_for_output(output);

        let mut under = None;
        if let Some((surface, loc)) = output
            .user_data()
            .get::<FullscreenSurface>()
            .and_then(|f| f.get())
            .and_then(|w| w.surface_under(pos - output_geo.loc.to_f64(), WindowSurfaceType::ALL))
        {
            under = Some((surface, loc + output_geo.loc));
        } else if let Some(focus) = layers
            .layer_under(WlrLayer::Overlay, pos - output_geo.loc.to_f64())
            .or_else(|| layers.layer_under(WlrLayer::Top, pos - output_geo.loc.to_f64()))
            .and_then(|layer| {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                layer
                    .surface_under(
                        pos - output_geo.loc.to_f64() - layer_loc.to_f64(),
                        WindowSurfaceType::ALL,
                    )
                    .map(|(surface, loc)| {
                        (
                            PointerFocusTarget::from(surface),
                            loc + layer_loc + output_geo.loc,
                        )
                    })
            })
        {
            under = Some(focus)
        } else if let Some(focus) = self.space.element_under(pos).and_then(|(window, loc)| {
            window
                .surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, surf_loc)| (surface, surf_loc + loc))
        }) {
            under = Some(focus);
        } else if let Some(focus) = layers
            .layer_under(WlrLayer::Bottom, pos - output_geo.loc.to_f64())
            .or_else(|| layers.layer_under(WlrLayer::Background, pos - output_geo.loc.to_f64()))
            .and_then(|layer| {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                layer
                    .surface_under(
                        pos - output_geo.loc.to_f64() - layer_loc.to_f64(),
                        WindowSurfaceType::ALL,
                    )
                    .map(|(surface, loc)| {
                        (
                            PointerFocusTarget::from(surface),
                            loc + layer_loc + output_geo.loc,
                        )
                    })
            })
        {
            under = Some(focus)
        };
        under.map(|(s, l)| (s, l.to_f64()))
    }

    pub(super) fn on_pointer_axis<B: InputBackend>(&mut self, evt: B::PointerAxisEvent) {
        let horizontal_amount = evt.amount(input::Axis::Horizontal).unwrap_or_else(|| {
            evt.amount_v120(input::Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
        });
        let vertical_amount = evt
            .amount(input::Axis::Vertical)
            .unwrap_or_else(|| evt.amount_v120(input::Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);
        let horizontal_amount_discrete = evt.amount_v120(input::Axis::Horizontal);
        let vertical_amount_discrete = evt.amount_v120(input::Axis::Vertical);

        // Super+wheel switches workspaces instead of scrolling the surface
        // under the pointer, mirroring the `workspace_left`/`workspace_right`
        // keybindings. Only fires on a discrete wheel step (not touchpad
        // scroll), so it can't be triggered by continuous finger scrolling.
        if let Some(discrete) = vertical_amount_discrete
            && discrete != 0.0 {
                let logo_held = self
                    .seat
                    .get_keyboard()
                    .map(|keyboard| keyboard.modifier_state().logo)
                    .unwrap_or(false);
                if logo_held {
                    // Super was used as a modifier, so its release must not
                    // count as a bare tap (which would open the launcher).
                    if self.super_tap_pending == Some(true) {
                        self.super_tap_pending = Some(false);
                    }
                    if let Some(output) = current_output_for_workspace_nav(self) {
                        let delta = if discrete > 0.0 { 1 } else { -1 };
                        crate::shell::workspace::switch_workspace(self, &output, delta);
                        crate::ext_workspace::ext_workspace_sync(self);
                    }
                    return;
                }
            }

        {
            let mut frame = AxisFrame::new(evt.time()).source(evt.source());
            if horizontal_amount != 0.0 {
                frame = frame
                    .relative_direction(Axis::Horizontal, evt.relative_direction(Axis::Horizontal));
                frame = frame.value(Axis::Horizontal, horizontal_amount);
                if let Some(discrete) = horizontal_amount_discrete {
                    frame = frame.v120(Axis::Horizontal, discrete as i32);
                }
            }
            if vertical_amount != 0.0 {
                frame = frame
                    .relative_direction(Axis::Vertical, evt.relative_direction(Axis::Vertical));
                frame = frame.value(Axis::Vertical, vertical_amount);
                if let Some(discrete) = vertical_amount_discrete {
                    frame = frame.v120(Axis::Vertical, discrete as i32);
                }
            }
            if evt.source() == AxisSource::Finger {
                if evt.amount(Axis::Horizontal) == Some(0.0) {
                    frame = frame.stop(Axis::Horizontal);
                }
                if evt.amount(Axis::Vertical) == Some(0.0) {
                    frame = frame.stop(Axis::Vertical);
                }
            }
            let pointer = self.pointer.clone();
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }

}

#[cfg(feature = "udev")]
impl AnvilState<UdevData> {
    pub(super) fn on_pointer_move<B: InputBackend>(
        &mut self,
        _dh: &DisplayHandle,
        evt: B::PointerMotionEvent,
    ) {
        let mut pointer_location = self.pointer.current_location();
        let serial = SCOUNTER.next_serial();

        let pointer = self.pointer.clone();
        let under = self.surface_under(pointer_location);

        let mut pointer_locked = false;
        let mut pointer_confined = false;
        let mut confine_region = None;
        if let Some((surface, surface_loc)) = under
            .as_ref()
            .and_then(|(target, l)| Some((target.wl_surface()?, l)))
        {
            with_pointer_constraint(&surface, &pointer, |constraint| match constraint {
                Some(constraint) if constraint.is_active() => {
                    // Constraint does not apply if not within region
                    if !constraint.region().is_none_or(|x| {
                        x.contains((pointer_location - *surface_loc).to_i32_round())
                    }) {
                        return;
                    }
                    match &*constraint {
                        PointerConstraint::Locked(_locked) => {
                            pointer_locked = true;
                        }
                        PointerConstraint::Confined(confine) => {
                            pointer_confined = true;
                            confine_region = confine.region().cloned();
                        }
                    }
                }
                _ => {}
            });
        }

        pointer.relative_motion(
            self,
            under.clone(),
            &RelativeMotionEvent {
                delta: evt.delta(),
                delta_unaccel: evt.delta_unaccel(),
                time: evt.time(),
            },
        );

        // If pointer is locked, only emit relative motion
        if pointer_locked {
            pointer.frame(self);
            return;
        }

        // Clamp delta, such that the pointer never moves out of bounds
        let mut delta = self.clamp_coords(pointer_location + evt.delta()) - pointer_location;

        // Clamp the individual x and y components of delta to respect confinement regions
        if pointer_confined
            && let Some((_, surface_loc)) = &under
                && let Some(region) = &confine_region {
                    // Clamp delta.x
                    if !region.contains(
                        (pointer_location + Point::new(delta.x, 0f64) - *surface_loc)
                            .to_i32_round(),
                    ) {
                        delta.x = 0f64;
                    }

                    // Clamp delta.y
                    if !region.contains(
                        (pointer_location + Point::new(0f64, delta.y) - *surface_loc)
                            .to_i32_round(),
                    ) {
                        delta.y = 0f64;
                    }
                }

        pointer_location += delta;

        if self.config.focus.follows_mouse {
            self.update_keyboard_focus(pointer_location, SCOUNTER.next_serial());
        }

        let new_under = self.surface_under(pointer_location);

        // If confined, don't move pointer if it would go outside surface
        if pointer_confined
            && let Some((surface, _)) = &under
                && new_under.as_ref().and_then(|(under, _)| under.wl_surface())
                    != surface.wl_surface()
                {
                    pointer.frame(self);
                    return;
                }

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pointer_location,
                serial,
                time: evt.time(),
            },
        );
        pointer.frame(self);

        // If pointer is now in a constraint region, activate it
        // TODO Anywhere else pointer is moved needs to do this
        if let Some((under, surface_location)) =
            new_under.and_then(|(target, loc)| Some((target.wl_surface()?.into_owned(), loc)))
        {
            with_pointer_constraint(&under, &pointer, |constraint| match constraint {
                Some(constraint) if !constraint.is_active() => {
                    let point = (pointer_location - surface_location).to_i32_round();
                    if constraint
                        .region()
                        .is_none_or(|region| region.contains(point))
                    {
                        constraint.activate();
                    }
                }
                _ => {}
            });
        }
    }

    pub(super) fn on_pointer_move_absolute<B: InputBackend>(
        &mut self,
        _dh: &DisplayHandle,
        evt: B::PointerMotionAbsoluteEvent,
    ) {
        let serial = SCOUNTER.next_serial();

        let max_x = self.space.outputs().fold(0, |acc, o| {
            acc + self.space.output_geometry(o).unwrap().size.w
        });

        let max_h_output = self
            .space
            .outputs()
            .max_by_key(|o| self.space.output_geometry(o).unwrap().size.h)
            .unwrap();

        let max_y = self.space.output_geometry(max_h_output).unwrap().size.h;

        let mut pointer_location = (evt.x_transformed(max_x), evt.y_transformed(max_y)).into();

        // clamp to screen limits
        pointer_location = self.clamp_coords(pointer_location);

        if self.config.focus.follows_mouse {
            self.update_keyboard_focus(pointer_location, SCOUNTER.next_serial());
        }

        let pointer = self.pointer.clone();
        let under = self.surface_under(pointer_location);

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pointer_location,
                serial,
                time: evt.time(),
            },
        );
        pointer.frame(self);
    }

    pub(super) fn clamp_coords(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.space.outputs().next().is_none() {
            return pos;
        }

        // Output geometry hit-testing (e.g. `Rectangle::contains`) treats the
        // output's bounds as half-open (`[loc, loc + size)`), so a pointer
        // clamped to exactly the rightmost/bottommost coordinate would fall
        // just outside every output and fail to hit any window there. Back
        // off by a tiny epsilon so the pointer stays inside the last output.
        const EDGE_EPSILON: f64 = 0.01;

        let (pos_x, pos_y) = pos.into();
        let max_x = self.space.outputs().fold(0, |acc, o| {
            acc + self.space.output_geometry(o).unwrap().size.w
        });
        let clamped_x = pos_x.clamp(0.0, max_x as f64 - EDGE_EPSILON);
        let max_y = self
            .space
            .outputs()
            .find(|o| {
                let geo = self.space.output_geometry(o).unwrap();
                geo.contains((clamped_x as i32, 0))
            })
            .map(|o| self.space.output_geometry(o).unwrap().size.h);

        if let Some(max_y) = max_y {
            let clamped_y = pos_y.clamp(0.0, max_y as f64 - EDGE_EPSILON);
            (clamped_x, clamped_y).into()
        } else {
            (clamped_x, pos_y).into()
        }
    }
}
