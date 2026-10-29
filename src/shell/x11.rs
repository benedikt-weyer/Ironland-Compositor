//! `XwmHandler`/`XWaylandShellHandler` impls: lets X11 clients (via XWayland)
//! map, move, resize, (un)maximize and (un)fullscreen windows, and own the
//! clipboard/primary selection, the same way `shell/xdg.rs` does for native
//! Wayland toplevels.
//!
//! X11 windows participate in tiling the same way native toplevels do
//! (`tiling::should_tile_x11` is `should_tile`'s X11 counterpart, driven by
//! the window's transient-for/window-type/size hints instead of an xdg
//! toplevel's parent/min-max size, since X11 windows have neither); ones
//! that opt out (dialogs, utility/menu/splash windows, fixed-size windows)
//! are placed and tracked as floating via [`place_new_window`] and
//! [`super::workspace::assign_new_window`], same as an xdg dialog.

use std::{cell::RefCell, os::unix::io::OwnedFd};

use smithay::{
    desktop::{Window, space::SpaceElement},
    input::pointer::Focus,
    output::Output,
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
    },
};
use tracing::{error, trace};

use crate::{AnvilState, focus::KeyboardFocusTarget, state::Backend};

use super::{
    FullscreenSurface, PointerMoveSurfaceGrab, PointerResizeSurfaceGrab, ResizeData, ResizeState,
    SurfaceData, TiledResizeGrab, TouchMoveSurfaceGrab, WindowElement, place_new_window, tiling, workspace,
};

#[derive(Debug, Default)]
struct OldGeometry(RefCell<Option<Rectangle<i32, Logical>>>);
impl OldGeometry {
    pub fn save(&self, geo: Rectangle<i32, Logical>) {
        *self.0.borrow_mut() = Some(geo);
    }

    pub fn restore(&self) -> Option<Rectangle<i32, Logical>> {
        self.0.borrow_mut().take()
    }
}

impl<BackendData: Backend> XWaylandShellHandler for AnvilState<BackendData> {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl<BackendData: Backend> XwmHandler for AnvilState<BackendData> {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().unwrap()
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        window.set_mapped(true).unwrap();
        // Games (Wine in particular) set `_NET_WM_STATE_FULLSCREEN` before
        // mapping; smithay reads that property on the map request, so it
        // shows up here instead of as a `fullscreen_request`.
        let wants_fullscreen = window.is_fullscreen();
        let should_tile = tiling::should_tile_x11(&window);
        let window = WindowElement(Window::new_x11_window(window));
        let output = if should_tile {
            let output = self
                .space
                .output_under(self.pointer.current_location())
                .next()
                .or_else(|| self.space.outputs().next())
                .cloned();
            // A fullscreen window stays in the tiling tree (so it returns to
            // its tile when unfullscreened) but must be the output's
            // fullscreen surface *before* the tree is laid out:
            // `tiling::apply_layout` leaves that window's size alone instead
            // of configuring it to its tile and then again to fullscreen.
            if wants_fullscreen && let Some(output) = &output {
                output.user_data().insert_if_missing(FullscreenSurface::default);
                output
                    .user_data()
                    .get::<FullscreenSurface>()
                    .unwrap()
                    .set(window.clone());
            }
            tiling::tile_new_window(self, &window, self.pointer.current_location());
            output
        } else {
            let output = place_new_window(&mut self.space, self.pointer.current_location(), &window, true);
            if let Some(output) = &output {
                workspace::assign_new_window(&window, output, true);
            }
            output
        };
        let Some(xsurface) = window.0.x11_surface() else {
            unreachable!()
        };
        // `top_bar` is the user's global switch for the compositor-drawn
        // window header bar - same gating as the xdg-decoration path in
        // `state.rs`'s `XdgDecorationHandler::request_mode`. X11 windows
        // have no xdg-decoration negotiation, so this has to be applied
        // here instead, using the window's own motif decoration hint only
        // to decide whether it *wants* SSD, not whether it's allowed to
        // have it.
        window.set_ssd(self.config.top_bar && !xsurface.is_decorated());
        match output {
            Some(output) if wants_fullscreen => self.fullscreen_x11(&window, &output),
            _ => {
                let bbox = self.space.element_bbox(&window).unwrap();
                xsurface.configure(Some(bbox)).unwrap();
            }
        }
        crate::foreign_toplevel::sync(self);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let location = window.last_configure().loc;
        let window = WindowElement(Window::new_x11_window(window));
        self.space.map_element(window, location, true);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let maybe = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned();
        if let Some(elem) = maybe {
            self.space.unmap_elem(&elem);
            // An unmapped window must not stay the output's fullscreen
            // surface, or its last frame keeps being drawn over everything.
            // (`is_fullscreen` stays set, so a remap fullscreens it again.)
            let output = self.space.outputs().find(|o| {
                o.user_data()
                    .get::<FullscreenSurface>()
                    .and_then(|f| f.get())
                    .is_some_and(|f| f == elem)
            });
            if let Some(output) = output.cloned() {
                output.user_data().get::<FullscreenSurface>().unwrap().clear();
                self.backend_data.reset_buffers(&output);
            }
        }
        if !window.is_override_redirect() {
            window.set_mapped(false).unwrap();
        }
        crate::foreign_toplevel::sync(self);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        crate::foreign_toplevel::sync(self);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // `all_windows` rather than `space.elements()`: a window on another
        // workspace is unmapped from the space but still isn't free to move.
        let elem = workspace::all_windows(self)
            .into_iter()
            .find(|e| matches!(e.0.x11_surface(), Some(s) if s == &window));

        // The compositor owns the geometry of fullscreen and tiled windows.
        // Answer with that geometry anyway (rather than staying silent): the
        // client waits for a ConfigureNotify, and e.g. Wine/DXVK validates
        // the result of a `Reset` against what it asked for.
        if let Some(elem) = &elem {
            if window.is_fullscreen() {
                let geometry = self
                    .space
                    .outputs()
                    .find(|o| {
                        o.user_data()
                            .get::<FullscreenSurface>()
                            .and_then(|f| f.get())
                            .is_some_and(|f| &f == elem)
                    })
                    .and_then(|o| self.space.output_geometry(o))
                    .unwrap_or_else(|| window.last_configure());
                let _ = window.configure(geometry);
                return;
            }
            if tiling::locate(self, elem).is_some() {
                let _ = window.configure(window.last_configure());
                return;
            }
        }

        // Floating and not-yet-mapped windows place and size themselves.
        let mut geo = window.last_configure();
        if let Some(x) = x {
            geo.loc.x = x;
        }
        if let Some(y) = y {
            geo.loc.y = y;
        }
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let Some(elem) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned()
        else {
            return;
        };
        self.space.map_element(elem, geometry.loc, false);
        // TODO: We don't properly handle the order of override-redirect windows here,
        //       they are always mapped top and then never reordered.
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.maximize_request_x11(&window);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned()
        else {
            return;
        };

        window.set_maximized(false).unwrap();
        if let Some(old_geo) = window
            .user_data()
            .get::<OldGeometry>()
            .and_then(|data| data.restore())
        {
            window.configure(old_geo).unwrap();
            self.space.map_element(elem, old_geo.loc, false);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned()
        else {
            // Not mapped yet (or unmapped again): there is no output to
            // fullscreen on, but record the state - `map_window_request`
            // fullscreens the window when it maps with it set.
            let _ = window.set_fullscreen(true);
            return;
        };
        let output = self
            .space
            .outputs_for_element(&elem)
            .first()
            .cloned()
            .or_else(|| self.space.outputs().next().cloned())
            // Assumes that at least one output exists
            .expect("No outputs found");
        self.fullscreen_x11(&elem, &output);
        crate::foreign_toplevel::sync(self);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let unfullscreened = if let Some(elem) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
        {
            window.set_fullscreen(false).unwrap();
            elem.set_ssd(self.config.top_bar && !window.is_decorated());
            if let Some(output) = self.space.outputs().find(|o| {
                o.user_data()
                    .get::<FullscreenSurface>()
                    .and_then(|f| f.get())
                    .map(|w| &w == elem)
                    .unwrap_or(false)
            }) {
                trace!("Unfullscreening: {:?}", elem);
                output.user_data().get::<FullscreenSurface>().unwrap().clear();
                window.configure(self.space.element_bbox(elem)).unwrap();
                self.backend_data.reset_buffers(output);
                true
            } else {
                false
            }
        } else {
            // Not mapped: just drop the recorded state (see `fullscreen_request`).
            let _ = window.set_fullscreen(false);
            false
        };
        if unfullscreened {
            crate::foreign_toplevel::sync(self);
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edges: X11ResizeEdge) {
        // only one seat, like the rest of this compositor
        let start_data = self.pointer.grab_start_data().unwrap();

        let Some(element) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
        else {
            return;
        };

        // A tiled window is resized in place - its dragged edge pushes the
        // tiling tree's border rather than pulling the window out into
        // floating. See `shell::xdg::XdgShellHandler::resize_request`.
        if let Some((output, workspace_idx)) = tiling::locate(self, element) {
            let grab = TiledResizeGrab {
                start_data,
                window: element.clone(),
                edges: edges.into(),
                output,
                workspace_idx,
            };
            let pointer = self.pointer.clone();
            pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
            return;
        }

        let geometry = element.geometry();
        let loc = self.space.element_location(element).unwrap();
        let (initial_window_location, initial_window_size) = (loc, geometry.size);

        with_states(&element.wl_surface().unwrap(), move |states| {
            states
                .data_map
                .get::<RefCell<SurfaceData>>()
                .unwrap()
                .borrow_mut()
                .resize_state = ResizeState::Resizing(ResizeData {
                edges: edges.into(),
                initial_window_location,
                initial_window_size,
            });
        });

        let grab = PointerResizeSurfaceGrab {
            start_data,
            window: element.clone(),
            edges: edges.into(),
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
        };

        let pointer = self.pointer.clone();
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        self.move_request_x11(&window)
    }

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        if let Some(keyboard) = self.seat.get_keyboard() {
            // check that an X11 window is focused
            if let Some(KeyboardFocusTarget::Window(w)) = keyboard.current_focus()
                && let Some(surface) = w.x11_surface()
                    && surface.xwm_id().unwrap() == xwm {
                        return true;
                    }
        }
        false
    }

    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    error!(?err, "Failed to request current wayland clipboard for Xwayland",);
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    error!(
                        ?err,
                        "Failed to request current wayland primary selection for Xwayland",
                    );
                }
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        trace!(?selection, ?mime_types, "Got Selection from X11",);
        // TODO check, that focused windows is X11 window before doing this
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat)
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat)
                }
            }
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
    }
}

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Makes `elem` (an X11 window) `output`'s fullscreen surface and
    /// configures it to cover the output.
    fn fullscreen_x11(&mut self, elem: &WindowElement, output: &Output) {
        let (Some(xsurface), Some(geometry)) = (elem.0.x11_surface(), self.space.output_geometry(output)) else {
            return;
        };
        let _ = xsurface.set_fullscreen(true);
        elem.set_ssd(false);
        let _ = xsurface.configure(geometry);
        output.user_data().insert_if_missing(FullscreenSurface::default);
        output
            .user_data()
            .get::<FullscreenSurface>()
            .unwrap()
            .set(elem.clone());
        trace!("Fullscreening: {:?}", elem);
    }

    pub fn maximize_request_x11(&mut self, window: &X11Surface) {
        let Some(elem) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == window))
            .cloned()
        else {
            return;
        };

        let old_geo = self.space.element_bbox(&elem).unwrap();
        let outputs_for_window = self.space.outputs_for_element(&elem);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| self.space.outputs().next())
            // Assumes that at least one output exists
            .expect("No outputs found");
        let geometry = self.space.output_geometry(output).unwrap();

        window.set_maximized(true).unwrap();
        window.configure(geometry).unwrap();
        window.user_data().insert_if_missing(OldGeometry::default);
        window.user_data().get::<OldGeometry>().unwrap().save(old_geo);
        self.space.map_element(elem, geometry.loc, false);
    }

    pub fn move_request_x11(&mut self, window: &X11Surface) {
        if let Some(touch) = self.seat.get_touch()
            && let Some(start_data) = touch.grab_start_data() {
                let element = self
                    .space
                    .elements()
                    .find(|e| matches!(e.0.x11_surface(), Some(w) if w == window));

                if let Some(element) = element {
                    let mut initial_window_location = self.space.element_location(element).unwrap();

                    // If surface is maximized then unmaximize it
                    if window.is_maximized() {
                        window.set_maximized(false).unwrap();
                        let pos = start_data.location;
                        initial_window_location = (pos.x as i32, pos.y as i32).into();
                        if let Some(old_geo) = window
                            .user_data()
                            .get::<OldGeometry>()
                            .and_then(|data| data.restore())
                        {
                            window
                                .configure(Rectangle::new(initial_window_location, old_geo.size))
                                .unwrap();
                        }
                    }

                    let grab = TouchMoveSurfaceGrab {
                        last_location: start_data.location,
                        start_data,
                        window: element.clone(),
                        initial_window_location,
                        // X11 windows are always floating, never tiled (see
                        // the module doc), so there's nothing to snap back.
                        was_tiled: false,
                    };

                    touch.set_grab(self, grab, SERIAL_COUNTER.next_serial());
                    return;
                }
            }

        // only one seat, like the rest of this compositor
        let Some(start_data) = self.pointer.grab_start_data() else {
            return;
        };

        let Some(element) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == window))
        else {
            return;
        };

        let mut initial_window_location = self.space.element_location(element).unwrap();

        // If surface is maximized then unmaximize it
        if window.is_maximized() {
            window.set_maximized(false).unwrap();
            let pos = self.pointer.current_location();
            initial_window_location = (pos.x as i32, pos.y as i32).into();
            if let Some(old_geo) = window
                .user_data()
                .get::<OldGeometry>()
                .and_then(|data| data.restore())
            {
                window
                    .configure(Rectangle::new(initial_window_location, old_geo.size))
                    .unwrap();
            }
        }

        let grab = PointerMoveSurfaceGrab {
            last_location: start_data.location,
            start_data,
            window: element.clone(),
            initial_window_location,
            // X11 windows are always floating, never tiled (see the module
            // doc), so there's nothing to snap back.
            was_tiled: false,
        };

        let pointer = self.pointer.clone();
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }
}
