//! Server side of `wlr-foreign-toplevel-management-unstable-v1`.
//!
//! This is what lets a client (a dock like molunga-shell's) list every
//! running app - title, app id, which one is focused - and ask to activate
//! or close one, without a compositor-specific IPC. Quickshell has this
//! protocol supported natively (`Quickshell.Wayland.ToplevelManager`),
//! unlike `ext-workspace-v1` (see [`crate::ext_workspace`]), which needed
//! the separate `ironland-workspaces` helper.
//!
//! The window list itself is never a separate source of truth: it's read
//! straight from [`crate::shell::workspace::all_windows`], which already
//! walks every output's workspaces (tiled and floating) - deliberately
//! *not* `state.space.elements()`, which only contains windows on each
//! output's currently-visible workspace, and would make apps disappear
//! from the dock the moment you switch away from them.
//!
//! [`sync`] is the single entry point: call it after a window maps/unmaps,
//! its title or app id changes, or keyboard focus moves to/from a window,
//! and it brings every bound client's toplevel list and state up to date.
//! Like `ext_workspace::ext_workspace_sync`, it doesn't try to diff away
//! redundant events - it only runs at discrete change points, never per
//! frame, so resending title/app_id/state on every call is negligible.
//!
//! Only `activate` and `close` are wired up. `set_maximized`/
//! `set_minimized`/`set_fullscreen` (and their `unset_*` counterparts) and
//! `set_rectangle` are accepted but ignored: ironland-compositor's tiling
//! model has no per-window maximized/minimized concept to report back (a
//! tiled window isn't "maximized", and there's no minimize/hide state
//! distinct from "on another workspace"), so a client is free to ask, it
//! just won't see the corresponding `state` bit ever come back set.
//!
//! One known gap, mirroring one already documented on `ext_workspace`: the
//! `parent` event (v3) is never sent (ironland-compositor doesn't track
//! toplevel parenting) - not required by the protocol, simply never
//! emitted. `output_enter`/`output_leave` *are* sent (see [`sync_instance`]),
//! since the shell needs them to know which monitor a window is on.

use smithay::{
    desktop::{Window, WindowSurface},
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::{ClientId, GlobalId},
        },
    },
    wayland::{Dispatch2, GlobalDispatch2, compositor, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{
    shell::WindowElement,
    state::{AnvilState, Backend},
};

/// Implemented by the compositor state so this module can drive
/// activate/close from client requests without depending on [`AnvilState`]
/// (and its `Backend` type parameter) directly.
pub trait ForeignToplevelHandler: 'static {
    fn foreign_toplevel_state(&mut self) -> &mut ForeignToplevelManagerState;
    /// Handle a client's `zwlr_foreign_toplevel_handle_v1.activate` request.
    fn activate_toplevel(&mut self, window: &WindowElement);
    /// Handle a client's `zwlr_foreign_toplevel_handle_v1.close` request.
    fn close_toplevel(&mut self, window: &WindowElement);
}

impl<B: Backend> ForeignToplevelHandler for AnvilState<B> {
    fn foreign_toplevel_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.foreign_toplevel_manager_state
    }

    fn activate_toplevel(&mut self, window: &WindowElement) {
        crate::shell::workspace::activate_window(self, window);
        sync(self);
    }

    fn close_toplevel(&mut self, window: &WindowElement) {
        match window.0.underlying_surface() {
            WindowSurface::Wayland(toplevel) => toplevel.send_close(),
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(surface) => {
                let _ = surface.close();
            }
        }
    }
}

/// A single toplevel handle created for one bound client, tied to the
/// window it represents.
#[derive(Debug)]
struct ToplevelEntry {
    window: WindowElement,
    resource: ZwlrForeignToplevelHandleV1,
    /// Outputs this handle has already sent `output_enter` for (and not yet
    /// `output_leave`), so [`sync_instance`] can diff against the window's
    /// current outputs instead of re-entering/leaving redundantly.
    known_outputs: Vec<Output>,
}

/// One client's binding of the `zwlr_foreign_toplevel_manager_v1` global,
/// and every handle created for it so far.
#[derive(Debug)]
struct Instance {
    manager: ZwlrForeignToplevelManagerV1,
    toplevels: Vec<ToplevelEntry>,
}

/// State of the `zwlr_foreign_toplevel_manager_v1` global.
#[derive(Debug)]
pub struct ForeignToplevelManagerState {
    global: GlobalId,
    instances: Vec<Instance>,
}

impl ForeignToplevelManagerState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: ForeignToplevelHandler + GlobalDispatch<ZwlrForeignToplevelManagerV1, ManagerGlobalData>,
    {
        let global = dh.create_global::<D, ZwlrForeignToplevelManagerV1, _>(3, ManagerGlobalData);
        ForeignToplevelManagerState {
            global,
            instances: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

/// Global data for the `zwlr_foreign_toplevel_manager_v1` global (nothing to
/// carry).
#[derive(Debug)]
pub struct ManagerGlobalData;

/// User data attached to a bound `zwlr_foreign_toplevel_manager_v1`
/// resource (nothing to carry - the instance's state lives in
/// [`ForeignToplevelManagerState::instances`], looked up by resource
/// identity).
#[derive(Debug)]
pub struct ManagerToken;

/// User data attached to a `zwlr_foreign_toplevel_handle_v1` resource.
#[derive(Debug, Clone)]
pub struct ToplevelData {
    window: WindowElement,
}

/// Title and app id for `window`, read straight from its role data (or, for
/// an X11 window, its WM_NAME/WM_CLASS) - never cached, since [`sync`] only
/// runs at discrete change points anyway.
pub(crate) fn title_and_app_id(window: &Window) -> (String, String) {
    match window.underlying_surface() {
        WindowSurface::Wayland(toplevel) => compositor::with_states(toplevel.wl_surface(), |states| {
            let attrs = states.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
            (attrs.title.clone().unwrap_or_default(), attrs.app_id.clone().unwrap_or_default())
        }),
        #[cfg(feature = "xwayland")]
        // Some clients (e.g. Bitwig) set WM_CLASS to a single string, which
        // leaves the class half empty; fall back to the instance name so the
        // window still gets an app id.
        WindowSurface::X11(surface) => {
            let class = surface.class();
            let app_id = if class.is_empty() { surface.instance() } else { class };
            (surface.title(), app_id)
        }
    }
}

/// Recomputes and pushes the full toplevel list to every bound client. Call
/// this after a window maps/unmaps, its title/app_id changes, or keyboard
/// focus moves to/from a window.
pub fn sync<D>(state: &mut D)
where
    D: ForeignToplevelHandler
        + Dispatch<ZwlrForeignToplevelManagerV1, ManagerToken>
        + Dispatch<ZwlrForeignToplevelHandleV1, ToplevelData>
        + AsWindowsFocusAndDisplay
        + crate::workspace_windows::WorkspaceWindowsHandler,
{
    let focused = state.focused_window();
    sync_with_focus(state, focused.as_ref());
}

/// Updates all clients after a keyboard-focus callback. Smithay invokes those
/// callbacks while its keyboard mutex is held, so they must use the focus value
/// supplied by Smithay rather than calling [`AsWindowsFocusAndDisplay::focused_window`],
/// which would try to lock the same mutex again.
pub(crate) fn sync_with_focus<D>(state: &mut D, focused: Option<&WindowElement>)
where
    D: ForeignToplevelHandler
        + Dispatch<ZwlrForeignToplevelManagerV1, ManagerToken>
        + Dispatch<ZwlrForeignToplevelHandleV1, ToplevelData>
        + AsWindowsFocusAndDisplay
        + crate::workspace_windows::WorkspaceWindowsHandler,
{
    let dh = state.display_handle();
    let windows = state.all_windows();
    let fullscreen = state.fullscreen_windows();
    let window_outputs: Vec<(WindowElement, Vec<Output>)> = windows
        .iter()
        .map(|w| (w.clone(), state.window_outputs(w)))
        .collect();
    let proto = state.foreign_toplevel_state();

    for instance in &mut proto.instances {
        let Ok(client) = dh.get_client(instance.manager.id()) else {
            continue;
        };
        sync_instance::<D>(&dh, &client, instance, &windows, focused, &fullscreen, &window_outputs);
    }

    // Titles/app ids can change independently of focus, and this is the
    // only hook that reliably fires on every such change (see this
    // function's callers) - piggyback the workspace-windows snapshot here
    // too rather than threading a separate call through all of them.
    crate::workspace_windows::sync(state);
}

/// Brings one client's handles in line with the current window list:
/// creates missing ones, tears down ones for windows that are no longer
/// around, and refreshes title/app_id/state on everything that remains.
fn sync_instance<D>(
    dh: &DisplayHandle,
    client: &Client,
    instance: &mut Instance,
    windows: &[WindowElement],
    focused: Option<&WindowElement>,
    fullscreen: &[WindowElement],
    window_outputs: &[(WindowElement, Vec<Output>)],
) where
    D: ForeignToplevelHandler
        + Dispatch<ZwlrForeignToplevelManagerV1, ManagerToken>
        + Dispatch<ZwlrForeignToplevelHandleV1, ToplevelData>,
{
    // Drop handles for windows that closed.
    instance.toplevels.retain(|entry| {
        let still_present = windows.contains(&entry.window);
        if !still_present {
            entry.resource.closed();
        }
        still_present
    });

    for window in windows {
        let entry = match instance.toplevels.iter_mut().find(|e| &e.window == window) {
            Some(e) => e,
            None => {
                let Ok(resource) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
                    dh,
                    instance.manager.version(),
                    ToplevelData { window: window.clone() },
                ) else {
                    continue;
                };
                instance.manager.toplevel(&resource);
                instance.toplevels.push(ToplevelEntry {
                    window: window.clone(),
                    resource,
                    known_outputs: Vec::new(),
                });
                instance.toplevels.last_mut().unwrap()
            }
        };

        let (title, app_id) = title_and_app_id(&window.0);
        entry.resource.title(title);
        entry.resource.app_id(app_id);

        let current_outputs = window_outputs
            .iter()
            .find(|(w, _)| w == window)
            .map(|(_, outputs)| outputs.as_slice())
            .unwrap_or_default();
        entry.known_outputs.retain(|known| {
            let still_on_it = current_outputs.contains(known);
            if !still_on_it {
                for wl_output in known.client_outputs(client) {
                    entry.resource.output_leave(&wl_output);
                }
            }
            still_on_it
        });
        for output in current_outputs {
            if !entry.known_outputs.contains(output) {
                for wl_output in output.client_outputs(client) {
                    entry.resource.output_enter(&wl_output);
                }
                entry.known_outputs.push(output.clone());
            }
        }

        let mut states = Vec::new();
        if focused == Some(window) {
            states.extend_from_slice(&(zwlr_foreign_toplevel_handle_v1::State::Activated as u32).to_ne_bytes());
        }
        if fullscreen.contains(window) {
            states.extend_from_slice(&(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32).to_ne_bytes());
        }
        entry.resource.state(states);
        entry.resource.done();
    }
}

/// Small seam so [`sync`] doesn't need to know about [`AnvilState`]/
/// [`Backend`] directly.
pub trait AsWindowsFocusAndDisplay {
    fn display_handle(&self) -> DisplayHandle;
    /// Every window the compositor currently knows about (see
    /// [`crate::shell::workspace::all_windows`]).
    fn all_windows(&self) -> Vec<WindowElement>;
    /// The window currently holding keyboard focus, if any.
    fn focused_window(&self) -> Option<WindowElement>;
    /// Every window that is some output's fullscreen surface right now (see
    /// `crate::shell::FullscreenSurface`) - at most one per output, but
    /// returned flattened since callers only ever need to test membership.
    fn fullscreen_windows(&self) -> Vec<WindowElement>;
    /// Every output `window` currently overlaps.
    fn window_outputs(&self, window: &WindowElement) -> Vec<Output>;
}

impl<B: Backend> AsWindowsFocusAndDisplay for AnvilState<B> {
    fn display_handle(&self) -> DisplayHandle {
        self.display_handle.clone()
    }

    fn all_windows(&self) -> Vec<WindowElement> {
        crate::shell::workspace::all_windows(self)
    }

    fn focused_window(&self) -> Option<WindowElement> {
        crate::shell::tiling::current_focused_window(self)
    }

    fn fullscreen_windows(&self) -> Vec<WindowElement> {
        self.space
            .outputs()
            .filter_map(|o| o.user_data().get::<crate::shell::FullscreenSurface>()?.get())
            .collect()
    }

    fn window_outputs(&self, window: &WindowElement) -> Vec<Output> {
        self.space.outputs_for_element(window)
    }
}

impl<D> GlobalDispatch2<ZwlrForeignToplevelManagerV1, D> for ManagerGlobalData
where
    D: ForeignToplevelHandler
        + AsWindowsFocusAndDisplay
        + Dispatch<ZwlrForeignToplevelManagerV1, ManagerToken>
        + Dispatch<ZwlrForeignToplevelHandleV1, ToplevelData>,
{
    fn bind(
        &self,
        state: &mut D,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ManagerToken);
        let mut instance = Instance {
            manager,
            toplevels: Vec::new(),
        };
        let windows = state.all_windows();
        let focused = state.focused_window();
        let fullscreen = state.fullscreen_windows();
        let window_outputs: Vec<(WindowElement, Vec<Output>)> = windows
            .iter()
            .map(|w| (w.clone(), state.window_outputs(w)))
            .collect();
        sync_instance::<D>(
            dh,
            client,
            &mut instance,
            &windows,
            focused.as_ref(),
            &fullscreen,
            &window_outputs,
        );
        state.foreign_toplevel_state().instances.push(instance);
    }
}

impl<D: ForeignToplevelHandler> Dispatch2<ZwlrForeignToplevelManagerV1, D> for ManagerToken {
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrForeignToplevelManagerV1) {
        state
            .foreign_toplevel_state()
            .instances
            .retain(|instance| &instance.manager != resource);
    }
}

impl<D: ForeignToplevelHandler> Dispatch2<ZwlrForeignToplevelHandleV1, D> for ToplevelData {
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        match request {
            Request::Activate { .. } => state.activate_toplevel(&self.window),
            Request::Close => state.close_toplevel(&self.window),
            // Not something ironland-compositor's tiling model has a notion
            // of - see the module doc. Accepted, just never followed by a
            // matching `state` bit.
            Request::SetMaximized
            | Request::UnsetMaximized
            | Request::SetMinimized
            | Request::UnsetMinimized
            | Request::SetFullscreen { .. }
            | Request::UnsetFullscreen
            | Request::SetRectangle { .. } => {}
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrForeignToplevelHandleV1) {
        for instance in &mut state.foreign_toplevel_state().instances {
            instance.toplevels.retain(|e| &e.resource != resource);
        }
    }
}
