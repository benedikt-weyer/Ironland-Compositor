use super::*;

impl<BackendData: Backend> DataDeviceHandler for AnvilState<BackendData> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl<BackendData: Backend> WaylandDndGrabHandler for AnvilState<BackendData> {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        self.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: (0, 0).into(),
        });

        match type_ {
            GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let start_data = pointer.grab_start_data().unwrap();
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
                    serial,
                    Focus::Keep,
                );
            }
            GrabType::Touch => {
                let touch = seat.get_touch().unwrap();
                let start_data = touch.grab_start_data().unwrap();
                touch.set_grab(
                    self,
                    DnDGrab::new_touch(&self.display_handle, start_data, source, seat),
                    serial,
                );
            }
        }
    }
}

impl<BackendData: Backend> DndGrabHandler for AnvilState<BackendData> {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }
}

impl<BackendData: Backend> crate::shortcuts::ShortcutsHandler for AnvilState<BackendData> {
    fn shortcuts_state(&mut self) -> &mut crate::shortcuts::ShortcutsManagerState {
        &mut self.shortcuts_manager_state
    }
}

impl<BackendData: Backend> crate::focus_grab::FocusGrabHandler for AnvilState<BackendData> {
    fn focus_grab_state(&mut self) -> &mut crate::focus_grab::FocusGrabManagerState {
        &mut self.focus_grab_manager_state
    }
}

impl<BackendData: Backend> crate::frame_capture::FrameCaptureHandler for AnvilState<BackendData> {
    fn frame_capture_state(&mut self) -> &mut crate::frame_capture::FrameCaptureManagerState {
        &mut self.frame_capture_manager_state
    }
}

impl<BackendData: Backend> crate::display_filter::DisplayFilterHandler for AnvilState<BackendData> {
    fn display_filter_state(&mut self) -> &mut crate::display_filter::DisplayFilterState {
        &mut self.display_filter_state
    }
}

impl<BackendData: Backend> crate::workspace_windows::WorkspaceWindowsHandler for AnvilState<BackendData> {
    fn workspace_windows_state(&mut self) -> &mut crate::workspace_windows::WorkspaceWindowsState {
        &mut self.workspace_windows_state
    }

    fn windows_by_workspace(&self) -> Vec<(String, usize, String, String, bool)> {
        crate::shell::workspace::all_windows(self)
            .into_iter()
            .filter_map(|window| {
                let (output, idx) = crate::shell::workspace::window_home(&window)?;
                let (title, app_id) = crate::foreign_toplevel::title_and_app_id(&window.0);
                let floating = crate::shell::workspace::is_floating(&window);
                Some((output.name(), idx, title, app_id, floating))
            })
            .collect()
    }

    fn set_window_floating(&mut self, output: &str, workspace: usize, title: &str, app_id: &str, floating: bool) {
        let window = crate::shell::workspace::all_windows(self).into_iter().find(|window| {
            let Some((home_output, home_idx)) = crate::shell::workspace::window_home(window) else {
                return false;
            };
            if home_output.name() != output || home_idx != workspace {
                return false;
            }
            let (t, a) = crate::foreign_toplevel::title_and_app_id(&window.0);
            t == title && a == app_id
        });
        let Some(window) = window else {
            return;
        };
        crate::shell::tiling::set_floating(self, &window, floating);
        crate::ext_workspace::ext_workspace_sync(self);
        crate::foreign_toplevel::sync(self);
        crate::workspace_windows::sync(self);
    }
}

impl<BackendData: Backend> OutputHandler for AnvilState<BackendData> {}

impl<BackendData: Backend> SelectionHandler for AnvilState<BackendData> {
    type SelectionUserData = ();

    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, seat: Seat<Self>) {
        #[cfg(feature = "xwayland")]
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.new_selection(ty, source.as_ref().map(|source| source.mime_types())) {
                warn!(?err, ?ty, "Failed to set Xwayland selection");
            }

        // Clipboard-history capture (see `crate::clipboard`'s module doc)
        // cares only about the clipboard, not the primary selection, and
        // only about an actual client source, not it being cleared.
        if ty == SelectionTarget::Clipboard
            && let Some(source) = &source {
                crate::clipboard::capture(&self.handle.clone(), &seat, source);
            }
    }

    #[cfg(feature = "xwayland")]
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                warn!(?err, "Failed to send primary (X11 -> Wayland)");
            }
    }
}

impl<BackendData: Backend> PrimarySelectionHandler for AnvilState<BackendData> {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl<BackendData: Backend> DataControlHandler for AnvilState<BackendData> {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

impl<BackendData: Backend> ShmHandler for AnvilState<BackendData> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl<BackendData: Backend> SeatHandler for AnvilState<BackendData> {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = PointerFocusTarget;
    type TouchFocus = PointerFocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<AnvilState<BackendData>> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, target: Option<&KeyboardFocusTarget>) {
        let dh = &self.display_handle;

        let wl_surface = target.and_then(WaylandFocus::wl_surface);

        let focus = wl_surface.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, focus.clone());
        set_primary_focus(dh, seat, focus);
    }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.backend_data.update_led_state(led_state)
    }
}

impl<BackendData: Backend> TabletSeatHandler for AnvilState<BackendData> {
    type ToolFocus = PointerFocusTarget;

    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        // TODO: tablet tools should have their own cursors
        self.cursor_status = image;
    }
}

impl<BackendData: Backend> InputMethodHandler for AnvilState<BackendData> {
    fn new_popup(&mut self, surface: PopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!("Failed to track popup: {}", err);
        }
    }

    fn popup_repositioned(&mut self, _: PopupSurface) {}

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        self.space
            .elements()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(parent)).then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

impl<BackendData: Backend> KeyboardShortcutsInhibitHandler for AnvilState<BackendData> {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        // Just grant the wish for everyone
        inhibitor.activate();
    }
}

impl<BackendData: Backend> PointerConstraintsHandler for AnvilState<BackendData> {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // XXX region
        let Some(current_focus) = pointer.current_focus() else {
            return;
        };
        if current_focus.wl_surface().as_deref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                constraint.unwrap().activate();
            });
        }
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        constraint_remove: ConstraintRemove,
    ) {
        // Clear cursor_position_hint to prevent a oneshot PointerLocked constraint
        // from causing this function to be called again during PointerLeave and
        // unexpectedly changing the cursor position.
        let Some((hint_surface, hint_location)) = self.cursor_position_hint.take() else {
            return;
        };

        match constraint_remove {
            ConstraintRemove::Destroyed(pointer_constraint) => match pointer_constraint {
                PointerConstraint::Confined(_confined_pointer) => (),
                PointerConstraint::Locked(locked_pointer) => {
                    let origin = self
                        .space
                        .elements()
                        .find_map(|window| {
                            (window.wl_surface().as_deref() == Some(&hint_surface))
                                .then(|| window.geometry())
                        })
                        .unwrap_or_default()
                        .loc
                        .to_f64();

                    let surface_location = origin + hint_location;
                    if let Some(region) = locked_pointer.region()
                        && region.contains(hint_location.to_i32_floor())
                    {
                        pointer.set_location(surface_location);
                    } else {
                        pointer.set_location(surface_location);
                    }
                }
            },
            ConstraintRemove::PointerLeave(_region) => (),
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        if with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        }) {
            self.cursor_position_hint = Some((surface.clone(), location));
        }
    }
}

impl<BackendData: Backend> XdgActivationHandler for AnvilState<BackendData> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        if let Some((serial, seat)) = data.serial {
            let keyboard = self.seat.get_keyboard().unwrap();
            Seat::from_resource(&seat) == Some(self.seat.clone())
                && keyboard
                    .last_enter()
                    .map(|last_enter| serial.is_no_older_than(&last_enter))
                    .unwrap_or(false)
        } else {
            false
        }
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if token_data.timestamp.elapsed().as_secs() < 10 {
            // Just grant the wish
            let w = self
                .space
                .elements()
                .find(|window| window.wl_surface().map(|s| *s == surface).unwrap_or(false))
                .cloned();
            if let Some(window) = w {
                crate::shell::workspace::raise_window(&mut self.space, &window);
            }
        }
    }
}

impl<BackendData: Backend> XdgDecorationHandler for AnvilState<BackendData> {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        // Set the default to client side
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ClientSide);
        });
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        // `top_bar` is the user's global switch for the compositor-drawn
        // window header bar. With it off, a client asking for server-side
        // decoration is overridden back to client-side so the header bar
        // never appears, no matter what individual clients request.
        let allow_ssd = self.config.top_bar;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(match mode {
                DecorationMode::ServerSide if allow_ssd => Mode::ServerSide,
                _ => Mode::ClientSide,
            });
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ClientSide);
        });

        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl<BackendData: Backend> FractionalScaleHandler for AnvilState<BackendData> {
    fn new_fractional_scale(
        &mut self,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Here we can set the initial fractional scale
        //
        // First we look if the surface already has a primary scan-out output, if not
        // we test if the surface is a subsurface and try to use the primary scan-out output
        // of the root surface. If the root also has no primary scan-out output we just try
        // to use the first output of the toplevel.
        // If the surface is the root we also try to use the first output of the toplevel.
        //
        // If all the above tests do not lead to a output we just use the first output
        // of the space (which in case of anvil will also be the output a toplevel will
        // initially be placed on)
        #[allow(clippy::redundant_clone)]
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        with_states(&surface, |states| {
            let primary_scanout_output = surface_primary_scanout_output(&surface, states)
                .or_else(|| {
                    if root != surface {
                        with_states(&root, |states| {
                            surface_primary_scanout_output(&root, states).or_else(|| {
                                self.window_for_surface(&root).and_then(|window| {
                                    self.space.outputs_for_element(&window).first().cloned()
                                })
                            })
                        })
                    } else {
                        self.window_for_surface(&root).and_then(|window| {
                            self.space.outputs_for_element(&window).first().cloned()
                        })
                    }
                })
                .or_else(|| self.space.outputs().next().cloned());
            if let Some(output) = primary_scanout_output {
                with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(output.current_scale().fractional_scale());
                });
            }
        });
    }
}

impl<BackendData: Backend + 'static> SecurityContextHandler for AnvilState<BackendData> {
    fn context_created(
        &mut self,
        source: SecurityContextListenerSource,
        security_context: SecurityContext,
    ) {
        self.handle
            .insert_source(source, move |client_stream, _, data| {
                let client_state = ClientState {
                    security_context: Some(security_context.clone()),
                    ..ClientState::default()
                };
                insert_client_with_identity(&mut data.display_handle, client_stream, client_state);
            })
            .expect("Failed to init wayland socket source");
    }
}

#[cfg(feature = "xwayland")]
impl<BackendData: Backend + 'static> XWaylandKeyboardGrabHandler for AnvilState<BackendData> {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<KeyboardFocusTarget> {
        let elem = self
            .space
            .elements()
            .find(|elem| elem.wl_surface().as_deref() == Some(surface))?;
        Some(KeyboardFocusTarget::Window(elem.0.clone()))
    }
}

impl<BackendData: Backend> XdgForeignHandler for AnvilState<BackendData> {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

impl<BackendData: Backend> ImageCaptureSourceHandler for AnvilState<BackendData> {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {
        // Anvil doesn't track sources
    }
}

impl<BackendData: Backend> OutputCaptureSourceHandler for AnvilState<BackendData> {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl<BackendData: Backend> ImageCopyCaptureHandler for AnvilState<BackendData> {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        use smithay::output::WeakOutput;
        let weak_output = source.user_data().get::<WeakOutput>()?;
        let output = weak_output.upgrade()?;
        let mode = output.current_mode()?;

        Some(BufferConstraints {
            size: mode
                .size
                .to_logical(1)
                .to_buffer(1, smithay::utils::Transform::Normal),
            shm: vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
            ],
            #[cfg(any(feature = "udev", feature = "winit", feature = "x11"))]
            dma: None,
        })
    }

    fn new_session(&mut self, session: Session) {
        self.screencopy.sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        use smithay::output::WeakOutput;
        use smithay::wayland::image_copy_capture::CaptureFailureReason;

        // Gate here, not at session creation: negotiating a session never
        // reads screen content, so it's harmless to let any client do -
        // only an actual `capture` request needs a decision on file. See
        // `crate::screencopy`'s module doc for the full model.
        let Some(client) = frame.buffer().client() else {
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        // Cached at connect time by `insert_client_with_identity` - see
        // `ClientState::capture_identity`'s doc for why that matters.
        let subject = client
            .get_data::<ClientState>()
            .and_then(|data| data.capture_identity.get())
            .cloned()
            .unwrap_or_else(|| "an unidentified application".to_string());
        match self.screencopy.grant(&subject) {
            Some(true) => {}
            Some(false) => {
                frame.fail(CaptureFailureReason::Unknown);
                return;
            }
            None => {
                self.permission_prompt.queue_internal(
                    crate::permission_prompt::PromptKind::Capture,
                    subject,
                    "capture your screen".to_string(),
                );
                frame.fail(CaptureFailureReason::Unknown);
                return;
            }
        }

        let Some(output) = session
            .source()
            .user_data()
            .get::<WeakOutput>()
            .and_then(WeakOutput::upgrade)
        else {
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        // Completed by the next successful render of `output` - see
        // `crate::screencopy::fulfill`, called from each backend's render
        // loop once the frame it just drew is still readable.
        //
        // An output with nothing changing renders an empty frame, which
        // there's nothing to read back from (the capture fails instead), so
        // a session's very first capture forces a full redraw - otherwise a
        // recorder started on a static screen would never get an initial
        // frame. Later frames only ever need to carry what changed.
        if session
            .source()
            .user_data()
            .insert_if_missing(|| crate::screencopy::FirstCaptureDone)
        {
            self.backend_data.reset_buffers(&output);
        }
        self.screencopy.queue_frame(output.name(), frame);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.screencopy.forget_session(&session);
    }
}

impl<BackendData: Backend> crate::permission_prompt::PermissionPromptHandler for AnvilState<BackendData> {
    fn permission_prompt_state(&mut self) -> &mut crate::permission_prompt::PermissionPromptManagerState {
        &mut self.permission_prompt
    }

    fn internal_prompt_resolved(
        &mut self,
        kind: crate::permission_prompt::PromptKind,
        subject: String,
        allowed: bool,
    ) {
        match kind {
            crate::permission_prompt::PromptKind::Capture => {
                self.screencopy.set_grant(subject, allowed);
            }
            crate::permission_prompt::PromptKind::ClipboardHistory => {
                crate::clipboard::resolve_grant(self, subject, allowed);
            }
        }
    }

    fn chooser_outputs(&self) -> Vec<crate::permission_prompt::ChooserOutput> {
        // Numbered left to right (then top to bottom), matching how the
        // monitors sit side by side, so "1" is the leftmost.
        let mut outputs: Vec<_> = self
            .space
            .outputs()
            .filter_map(|output| {
                let geo = self.space.output_geometry(output)?;
                Some((geo.loc, output))
            })
            .collect();
        outputs.sort_by_key(|(loc, output)| (loc.x, loc.y, output.name()));
        outputs
            .into_iter()
            .map(|(_, output)| crate::permission_prompt::ChooserOutput {
                name: output.name(),
                description: output
                    .current_mode()
                    .map(|mode| format!("{}x{}", mode.size.w, mode.size.h))
                    .unwrap_or_default(),
            })
            .collect()
    }
}

impl<BackendData: Backend> crate::clipboard::ClipboardHistoryHandler for AnvilState<BackendData> {
    fn clipboard_history_state(&mut self) -> &mut crate::clipboard::ClipboardHistoryState {
        &mut self.clipboard_history
    }

    fn clipboard_client_identity(&self, client: &Client) -> String {
        client
            .get_data::<ClientState>()
            .and_then(|data| data.capture_identity.get())
            .cloned()
            .unwrap_or_else(|| "an unidentified application".to_string())
    }
}

delegate_dispatch2!(@<BackendData: Backend + 'static> AnvilState<BackendData>);
