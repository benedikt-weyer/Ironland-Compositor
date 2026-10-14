#[cfg(feature = "xwayland")]
use std::os::unix::io::OwnedFd;
use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use tracing::{info, warn};

use smithay::{
    backend::{
        input::TabletToolDescriptor,
        renderer::element::{
            RenderElementStates, default_primary_scanout_output_compare,
            utils::select_dmabuf_feedback,
        },
    },
    delegate_dispatch2,
    desktop::{
        PopupKind, PopupManager, Space,
        space::SpaceElement,
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
            with_surfaces_surface_tree,
        },
    },
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source},
        keyboard::{Keysym, LedState},
        pointer::{CursorImageStatus, Focus, PointerHandle},
        tablet::TabletSeatHandler,
    },
    output::Output,
    reexports::{
        calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::decoration::{
            self as xdg_decoration,
            zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        },
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Serial, Time},
    wayland::{
        commit_timing::{CommitTimerBarrierStateUserData, CommitTimingManagerState},
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states,
        },
        dmabuf::DmabufFeedback,
        fifo::{FifoBarrierCachedState, FifoManagerState},
        fixes::FixesState,
        fractional_scale::{
            FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
        },
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, ImageCaptureSourceState,
            OutputCaptureSourceHandler, OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, Frame, ImageCopyCaptureHandler, ImageCopyCaptureState, Session,
            SessionRef,
        },
        input_method::{InputMethodHandler, InputMethodManagerState, PopupSurface},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{
            ConstraintRemove, PointerConstraint, PointerConstraintsHandler,
            PointerConstraintsState, with_pointer_constraint,
        },
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        security_context::{
            SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
            SecurityContextState,
        },
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
            wlr_data_control::{DataControlHandler, DataControlState},
        },
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{
                ToplevelSurface, XdgShellState,
                decoration::{XdgDecorationHandler, XdgDecorationState},
            },
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
    },
};

#[cfg(feature = "xwayland")]
use crate::cursor::Cursor;
use crate::{
    focus::{KeyboardFocusTarget, PointerFocusTarget},
    shell::WindowElement,
};
use smithay::wayland::selection::{SelectionSource, SelectionTarget};
#[cfg(feature = "xwayland")]
use smithay::{
    utils::Size,
    wayland::xwayland_keyboard_grab::{XWaylandKeyboardGrabHandler, XWaylandKeyboardGrabState},
    wayland::xwayland_shell,
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};

#[derive(Debug, Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub security_context: Option<SecurityContext>,
    /// Whether this client connected through the privileged capture socket
    /// (see `AnvilState::init`'s `capture_socket_name`), and so may bind
    /// the `ironland-permission-prompt-v1` manager global - unlike screen
    /// capture itself (see `crate::screencopy`'s module doc), which is
    /// unrestricted; this gate exists only so an arbitrary client can't pop
    /// a spoofed system permission dialog.
    pub capture_privileged: bool,
    /// This client's resolved executable identity (see
    /// `crate::screencopy::client_identity`), for the capture and
    /// clipboard-history permission gates (`crate::screencopy`,
    /// `crate::clipboard`) - both key their in-memory grant tables on this
    /// same identity. Resolved exactly once, immediately after the client
    /// connects
    /// (see `insert_client_with_identity`) - *not* lazily on first capture
    /// attempt, which could be arbitrarily later. `SO_PEERCRED` credentials
    /// are frozen by the kernel at connect time and can't be spoofed, but
    /// the pid they name can still be reused by an unrelated process once
    /// the original one exits; resolving `/proc/<pid>/exe` from that pid
    /// immediately - rather than whenever the client happens to first
    /// request a capture, possibly long after connecting, well past when
    /// the original process could have exited and that pid been recycled -
    /// keeps that race window effectively zero.
    pub capture_identity: std::sync::OnceLock<String>,
}
impl ClientData for ClientState {
    /// Notification that a client was initialized
    fn initialized(&self, _client_id: ClientId) {}
    /// Notification that a client is disconnected
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Inserts a newly-connected client, then immediately resolves and caches
/// its `capture_identity` (see [`ClientState`]'s doc on that field for why
/// "immediately" - right after `insert_client`, not lazily - matters). The
/// one place every client-accepting socket source in this module should
/// call through, instead of `display_handle.insert_client` directly.
fn insert_client_with_identity(
    dh: &mut DisplayHandle,
    stream: std::os::unix::net::UnixStream,
    client_state: ClientState,
) {
    let client_state = Arc::new(client_state);
    match dh.insert_client(stream, client_state.clone()) {
        Ok(client) => {
            let identity = crate::screencopy::client_identity(dh, &client);
            let _ = client_state.capture_identity.set(identity);
        }
        Err(err) => warn!("Error adding wayland client: {}", err),
    }
}

#[derive(Debug)]
pub struct AnvilState<BackendData: Backend + 'static> {
    pub backend_data: BackendData,
    pub socket_name: Option<String>,
    pub display_handle: DisplayHandle,
    pub running: Arc<AtomicBool>,
    pub handle: LoopHandle<'static, AnvilState<BackendData>>,

    // desktop
    pub space: Space<WindowElement>,
    pub popups: PopupManager,

    // smithay state
    pub compositor_state: CompositorState,
    pub data_device_state: DataDeviceState,
    pub layer_shell_state: WlrLayerShellState,
    pub workspace_manager_state: crate::ext_workspace::WorkspaceManagerState,
    pub foreign_toplevel_manager_state: crate::foreign_toplevel::ForeignToplevelManagerState,
    pub shortcuts_manager_state: crate::shortcuts::ShortcutsManagerState,
    pub focus_grab_manager_state: crate::focus_grab::FocusGrabManagerState,
    pub frame_capture_manager_state: crate::frame_capture::FrameCaptureManagerState,
    pub workspace_windows_state: crate::workspace_windows::WorkspaceWindowsState,
    pub output_manager_state: OutputManagerState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub seat_state: SeatState<AnvilState<BackendData>>,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub shm_state: ShmState,
    pub viewporter_state: ViewporterState,
    pub xdg_activation_state: XdgActivationState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_shell_state: XdgShellState,
    pub presentation_state: PresentationState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub xdg_foreign_state: XdgForeignState,
    #[cfg(feature = "xwayland")]
    pub xwayland_shell_state: xwayland_shell::XWaylandShellState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub fifo_manager_state: FifoManagerState,
    pub commit_timing_manager_state: CommitTimingManagerState,
    pub image_capture_source_state: ImageCaptureSourceState,
    pub output_capture_source_state: OutputCaptureSourceState,
    pub image_copy_capture_state: ImageCopyCaptureState,
    /// Sessions/pending frames for the image-copy-capture pipeline; see
    /// `crate::screencopy`.
    pub screencopy: crate::screencopy::ScreencopyState,
    pub permission_prompt: crate::permission_prompt::PermissionPromptManagerState,
    /// Captured clipboard history and its grant table; see
    /// `crate::clipboard`.
    pub clipboard_history: crate::clipboard::ClipboardHistoryState,
    /// Name of the second, privileged Wayland socket that gates
    /// `ironland-permission-prompt-v1` and `ironland-capture-permissions-
    /// v1` (see `crate::screencopy`'s module doc - screen capture itself
    /// is *not* gated by this socket). `None` if `AnvilState::init`
    /// couldn't open it.
    pub capture_socket_name: Option<String>,

    pub dnd_icon: Option<DndIcon>,

    // input-related fields
    pub suppressed_keys: Vec<Keysym>,
    /// Which shortcut name (see `crate::shortcuts`) each currently-held,
    /// suppressed keysym triggered on press, so its release can fire that
    /// same shortcut's `released` event (see
    /// `input_handler::keyboard_key_to_action`).
    pub(crate) held_shortcut_keys: HashMap<Keysym, String>,
    pub cursor_status: CursorImageStatus,
    pub seat_name: String,
    pub seat: Seat<AnvilState<BackendData>>,
    pub clock: Clock<Monotonic>,
    pub pointer: PointerHandle<AnvilState<BackendData>>,
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,

    #[cfg(feature = "xwayland")]
    pub xwm: Option<X11Wm>,
    #[cfg(feature = "xwayland")]
    pub xdisplay: Option<u32>,

    #[cfg(feature = "debug")]
    pub renderdoc: Option<renderdoc::RenderDoc<renderdoc::V141>>,

    pub show_window_preview: bool,
    pub launcher: crate::drawing::LauncherState,
    /// Screen lock (see `crate::lock`).
    pub lock: crate::lock::LockState,
    /// Workspace overview overlay (see `crate::overview`).
    pub overview: crate::overview::OverviewState,
    /// Keyboard focus held before the screen was locked, restored on unlock.
    pub(crate) focus_before_lock: Option<crate::focus::KeyboardFocusTarget>,
    pub wallpaper: crate::wallpaper::Wallpaper,

    /// When the workspace dot overlay was last triggered (a switch or a
    /// window move between workspaces), if `config.workspaces.overlay` is
    /// on. Cleared implicitly once it's older than
    /// [`crate::shell::workspace::OVERLAY_DURATION_MS`]; see the render loops.
    pub workspace_overlay_shown: Option<std::time::Instant>,

    /// FPS/frame-time history and stutter counters, keyed by output name -
    /// see `crate::perf_overlay` and `config.performance`. Entries are
    /// created lazily the first time a given output renders a frame and
    /// simply left in place if the output disappears (cheap, and avoids
    /// churn on the common case of a monitor being unplugged and replugged).
    pub perf_stats: HashMap<String, crate::perf_overlay::FrameStats>,

    /// Rasterized FPS-overlay texture per output, rebuilt from `perf_stats`
    /// only on `config.performance.fps_overlay_interval_ms`'s cadence - see
    /// `crate::perf_overlay::OverlayCache`.
    pub fps_overlay_cache: HashMap<String, crate::perf_overlay::OverlayCache>,

    /// Pressed/unpressed state of the keys listed in `config.key_tracker`,
    /// fed from the keyboard filter - see `crate::key_tracker`.
    pub key_tracker: crate::key_tracker::KeyTracker,

    /// How long the most recent `dispatch_clients` call (the Wayland
    /// display socket's calloop source, below) took - fed into
    /// `crate::frame_capture`'s "dispatch_clients" stage by whichever
    /// output renders next, since client dispatch isn't itself per-output.
    /// Only ever measured while a capture session is open; `ZERO`
    /// otherwise.
    pub last_dispatch_duration: Duration,

    /// Persistent damage-tracking identity for the focus-highlight border
    /// and the tiling drag-and-drop indicator, keyed by output name (see
    /// `crate::border::BorderCache` for why a fresh element every frame was
    /// a real performance bug, and why this needs to be per-output rather
    /// than a single shared instance - the same window's border is drawn at
    /// a different output-local position on each output it's rendered for).
    pub border_cache: HashMap<String, crate::border::BorderCache>,
    pub drop_indicator_cache: HashMap<String, crate::border::BorderCache>,

    /// The drop-target rect a tiling drag-and-drop is currently previewing
    /// (space-global logical coordinates), if a tiled window is being
    /// dragged and the pointer is over another tile - see
    /// `shell::tiling::drop_target` and `shell::grabs::PointerMoveSurfaceGrab`.
    /// Drawn as a highlight border in the render loops.
    pub tiling_drop_indicator: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,

    /// Resolved keybinding table, built once at startup from `config::Config`
    /// (see `input_handler::compile_keybindings`).
    pub(crate) keybindings: Vec<(
        crate::keybindings::KeyModifiers,
        Keysym,
        crate::input_handler::KeyAction,
    )>,

    /// Resolved touchpad gesture bindings (see
    /// `input_handler::compile_gesture_bindings`).
    pub(crate) gesture_bindings: Vec<(
        crate::config::Gesture,
        crate::input_handler::KeyAction,
    )>,
    /// The touchpad gesture the compositor has claimed for a binding and is
    /// currently measuring, instead of forwarding to clients.
    pub(crate) gesture_tracker: Option<crate::input_handler::GestureTracker>,

    /// Action to fire when the Super key is tapped alone (see
    /// `keybindings::super_tap_action`), if one is configured.
    pub(crate) super_tap_action: Option<crate::input_handler::KeyAction>,
    /// `Some(true)` while Super is held and no other key has been pressed
    /// since, meaning it's still a candidate bare tap; `Some(false)` once
    /// another key has broken that; `None` while Super isn't held.
    pub(crate) super_tap_pending: Option<bool>,
    /// Bumped on every Super press; a long-press timer (see
    /// `start_super_hold_timer`) only acts if it still matches.
    pub(crate) super_hold_token: u64,

    /// User-facing settings loaded at startup (see [`crate::config::Config`]),
    /// kept around so backends can consult output placement/mirroring/primary
    /// settings as monitors connect.
    pub config: crate::config::Config,

    /// Last time the effective config file was checked. Both backends call
    /// the same inexpensive polling hook from their event loops.
    config_last_checked: Instant,
}

#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

mod config;
mod handlers;
mod repaint;

pub use repaint::{SurfaceDmabufFeedback, take_presentation_feedback, update_primary_scanout_output};

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    pub fn init(
        display: Display<AnvilState<BackendData>>,
        handle: LoopHandle<'static, AnvilState<BackendData>>,
        backend_data: BackendData,
        listen_on_socket: bool,
    ) -> AnvilState<BackendData> {
        let dh = display.handle();

        // wayland-backend's per-client outgoing buffer defaults to 4096
        // bytes and a single message that doesn't fit in it (even after
        // growing up to that cap) makes `write_message` return `E2BIG`,
        // which disconnects the client outright - surfacing on the client
        // side as "the Wayland connection broke". `clipboard::send_entry`
        // inlines an image entry's PNG-encoded thumbnail directly into the
        // `thumbnail` event's wire message (see that protocol's doc for
        // why it's inlined rather than streamed like `receive`), and a
        // `THUMBNAIL_MAX_DIM`-bounded thumbnail routinely exceeds 4096
        // bytes for anything but a near-blank image, so the default here
        // must be raised well above the largest thumbnail that can occur.
        dh.backend_handle().set_default_max_buffer_size(1024 * 1024);

        let clock = Clock::new();

        let config = crate::config::Config::load();
        let keybindings = crate::input_handler::compile_keybindings(&config);
        let super_tap_action = crate::input_handler::compile_super_tap_action(&config);
        let gesture_bindings = crate::input_handler::compile_gesture_bindings(&config);

        // init wayland clients
        let socket_name = if listen_on_socket {
            let source = ListeningSocketSource::new_auto().unwrap();
            let socket_name = source.socket_name().to_string_lossy().into_owned();
            handle
                .insert_source(source, |client_stream, _, data| {
                    insert_client_with_identity(
                        &mut data.display_handle,
                        client_stream,
                        ClientState::default(),
                    );
                })
                .expect("Failed to init wayland socket source");
            info!(name = socket_name, "Listening on wayland socket");
            Some(socket_name)
        } else {
            None
        };

        // A second, privileged socket (see `crate::screencopy`'s module
        // doc): every client connected through it is flagged
        // `capture_privileged`, the only clients the
        // `ironland-permission-prompt-v1` global is advertised to (screen
        // capture itself is unrestricted, gated per-executable instead -
        // see that module doc). In practice the sole client that connects
        // here is this compositor's own `ironland-portal-screenshot`
        // binary. Opened even when `!listen_on_socket` (nested inside
        // another session's own socket handling) so the same gating works
        // there too during development.
        let capture_socket_name = ListeningSocketSource::new_auto().ok().map(|source| {
            let name = source.socket_name().to_string_lossy().into_owned();
            handle
                .insert_source(source, |client_stream, _, data| {
                    let client_state = ClientState {
                        capture_privileged: true,
                        ..ClientState::default()
                    };
                    insert_client_with_identity(&mut data.display_handle, client_stream, client_state);
                })
                .expect("Failed to init capture wayland socket source");
            info!(name = name, "Listening on privileged capture wayland socket");
            name
        });
        // Safety: single-threaded at this point in startup.
        if let Some(name) = capture_socket_name.as_deref() {
            unsafe {
                std::env::set_var("IRONLAND_CAPTURE_SOCKET", name);
            }
        }

        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, data| {
                    profiling::scope!("dispatch_clients");
                    // Timed only while a `crate::frame_capture` session is
                    // open - see `last_dispatch_duration`'s own doc comment
                    // for why this is where it's measured.
                    let timed = crate::frame_capture::is_capturing(data).then(Instant::now);
                    // Safety: we don't drop the display
                    unsafe {
                        display.get_mut().dispatch_clients(data).unwrap();
                    }
                    if let Some(start) = timed {
                        data.last_dispatch_duration = start.elapsed();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("Failed to init wayland server source");

        // Results of background PAM checks for the screen lock.
        let (lock_result_tx, lock_result_rx) = smithay::reexports::calloop::channel::channel::<bool>();
        handle
            .insert_source(lock_result_rx, |event, _, data| {
                if let smithay::reexports::calloop::channel::Event::Msg(ok) = event {
                    data.finish_unlock_attempt(ok);
                }
            })
            .expect("Failed to init lock screen channel");

        // init globals
        let compositor_state = CompositorState::new::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let workspace_manager_state = crate::ext_workspace::WorkspaceManagerState::new::<Self>(&dh);
        let foreign_toplevel_manager_state =
            crate::foreign_toplevel::ForeignToplevelManagerState::new::<Self>(&dh);
        let shortcuts_manager_state = crate::shortcuts::ShortcutsManagerState::new::<Self>(&dh);
        let focus_grab_manager_state = crate::focus_grab::FocusGrabManagerState::new::<Self>(&dh);
        let frame_capture_manager_state = crate::frame_capture::FrameCaptureManagerState::new::<Self>(&dh);
        let workspace_windows_state = crate::workspace_windows::WorkspaceWindowsState::new::<Self>(&dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true);
        let mut seat_state = SeatState::new();
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, clock.id() as u32);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let fifo_manager_state = FifoManagerState::new::<Self>(&dh);
        let commit_timing_manager_state = CommitTimingManagerState::new::<Self>(&dh);
        TextInputManagerState::new::<Self>(&dh);
        InputMethodManagerState::new::<Self, _>(&dh, |_client| true);
        VirtualKeyboardManagerState::new::<Self, _>(&dh, |_client| true);
        // Expose global only if backend supports relative motion events
        if BackendData::HAS_RELATIVE_MOTION {
            RelativePointerManagerState::new::<Self>(&dh);
        }
        PointerConstraintsState::new::<Self>(&dh);
        if BackendData::HAS_GESTURES {
            PointerGesturesState::new::<Self>(&dh);
        }
        TabletManagerState::new::<Self>(&dh);
        SecurityContextState::new::<Self, _>(&dh, |client| {
            client
                .get_data::<ClientState>()
                .is_none_or(|client_state| client_state.security_context.is_none())
        });
        FixesState::new::<Self>(&dh);

        // Image capture protocols (screencopy) - unrestricted, like every
        // other wlroots-style compositor's screencopy protocol: the
        // trusted shell's own native screenshot tool needs direct,
        // unprompted access to these exactly like the compositor's own
        // overlays do, so gating this at the Wayland-protocol level would
        // either lock the shell out or make it indistinguishable from a
        // random untrusted client. Per-app consent for *portal*-mediated
        // screenshots (the path sandboxed/third-party apps actually use)
        // is enforced one layer up, entirely within
        // `ironland-portal-screenshot` - see `crate::screencopy`'s module
        // doc.
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state = OutputCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture_state = ImageCopyCaptureState::new::<Self>(&dh);
        // The permission-prompt global stays gated to the privileged
        // capture socket, unlike the above: unrestricted access here would
        // let *any* client pop a spoofed "X wants to Y" system dialog.
        let capture_privileged = |client: &Client| {
            client
                .get_data::<ClientState>()
                .is_some_and(|c| c.capture_privileged)
        };
        let permission_prompt = crate::permission_prompt::PermissionPromptManagerState::new::<Self, _>(
            &dh,
            capture_privileged,
        );
        // Unrestricted, unlike the above - see `crate::clipboard`'s
        // module doc: binding this alone reveals nothing, access to actual
        // history content is gated per-executable instead.
        let clipboard_history = crate::clipboard::ClipboardHistoryState::new::<Self>(&dh);

        // init input
        let seat_name = backend_data.seat_name();
        let mut seat = seat_state.new_wl_seat(&dh, seat_name.clone());

        let pointer = seat.add_pointer();
        seat.add_keyboard(crate::keybindings::to_xkb_config(&config.keyboard), 200, 25)
            .expect("Failed to initialize the keyboard");

        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);

        #[cfg(feature = "xwayland")]
        let xwayland_shell_state = xwayland_shell::XWaylandShellState::new::<Self>(&dh.clone());

        #[cfg(feature = "xwayland")]
        XWaylandKeyboardGrabState::new::<Self>(&dh.clone());

        AnvilState {
            backend_data,
            display_handle: dh,
            socket_name,
            running: Arc::new(AtomicBool::new(true)),
            handle,
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            data_device_state,
            layer_shell_state,
            workspace_manager_state,
            foreign_toplevel_manager_state,
            shortcuts_manager_state,
            focus_grab_manager_state,
            frame_capture_manager_state,
            workspace_windows_state,
            output_manager_state,
            primary_selection_state,
            data_control_state,
            seat_state,
            keyboard_shortcuts_inhibit_state,
            shm_state,
            viewporter_state,
            xdg_activation_state,
            xdg_decoration_state,
            xdg_shell_state,
            presentation_state,
            fractional_scale_manager_state,
            xdg_foreign_state,
            single_pixel_buffer_state,
            fifo_manager_state,
            commit_timing_manager_state,
            image_capture_source_state,
            output_capture_source_state,
            image_copy_capture_state,
            screencopy: crate::screencopy::ScreencopyState::default(),
            permission_prompt,
            clipboard_history,
            capture_socket_name,
            dnd_icon: None,
            suppressed_keys: Vec::new(),
            held_shortcut_keys: HashMap::new(),
            cursor_status: CursorImageStatus::default_named(),
            seat_name,
            seat,
            pointer,
            cursor_position_hint: None,
            clock,

            #[cfg(feature = "xwayland")]
            xwayland_shell_state,
            #[cfg(feature = "xwayland")]
            xwm: None,
            #[cfg(feature = "xwayland")]
            xdisplay: None,
            #[cfg(feature = "debug")]
            renderdoc: renderdoc::RenderDoc::new().ok(),
            show_window_preview: false,
            launcher: crate::drawing::LauncherState::default(),
            lock: crate::lock::LockState::new(lock_result_tx),
            overview: Default::default(),
            focus_before_lock: None,
            wallpaper: crate::wallpaper::Wallpaper::load(config.wallpaper.as_deref()),
            workspace_overlay_shown: None,
            perf_stats: HashMap::new(),
            fps_overlay_cache: HashMap::new(),
            key_tracker: crate::key_tracker::KeyTracker::new(&config.key_tracker),
            last_dispatch_duration: Duration::ZERO,
            border_cache: HashMap::new(),
            drop_indicator_cache: HashMap::new(),
            tiling_drop_indicator: None,
            keybindings,
            gesture_bindings,
            gesture_tracker: None,
            super_tap_action,
            super_tap_pending: None,
            super_hold_token: 0,
            config,
            config_last_checked: Instant::now(),
        }
    }

    /// Records that `output` just rendered/presented a frame at `now`,
    /// updating its [`crate::perf_overlay::FrameStats`], logging a
    /// `tracing::warn!` if it counted as a stutter and
    /// `config.performance.stutter_log` is on, and - while a
    /// `crate::frame_capture` session is open - reporting it as that
    /// session's `frame_index`th frame on `output`. Both backends call this
    /// once per successfully rendered frame (see `udev.rs`/`winit.rs`),
    /// whether or not the FPS overlay is currently shown, so the overlay's
    /// history isn't empty the moment it's toggled on.
    /// Locks the screen: nothing but the lock panel is drawn and every input
    /// event is swallowed until [`finish_unlock_attempt`](Self::finish_unlock_attempt)
    /// succeeds.
    pub fn lock_screen(&mut self) {
        if self.lock.is_locked() {
            return;
        }
        info!("Locking screen");
        self.launcher.close();
        self.close_overview();
        self.lock.lock();

        // Take focus away from every client so none keeps receiving input
        // (or believes keys are still held) behind the lock.
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            self.focus_before_lock = keyboard.current_focus();
            keyboard.set_focus(self, None, serial);
        }
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            None,
            &smithay::input::pointer::MotionEvent {
                location: pointer.current_location(),
                serial,
                time: smithay::backend::input::InputTime::now(),
            },
        );
        pointer.frame(self);
    }

    /// Called with the outcome of a PAM check started from the lock screen.
    pub fn finish_unlock_attempt(&mut self, ok: bool) {
        if !self.lock.is_locked() {
            return;
        }
        if !ok {
            self.lock.attempt_failed();
            return;
        }
        info!("Screen unlocked");
        self.lock.unlock();
        if let Some(target) = self.focus_before_lock.take()
            && smithay::utils::IsAlive::alive(&target)
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(target), serial);
        }
    }

    pub fn record_frame_stats(&mut self, output: &Output, now: Instant, frame_index: u32) {
        let refresh_mhz = output.current_mode().map(|mode| mode.refresh);
        let threshold = crate::perf_overlay::stutter_threshold(&self.config.performance, refresh_mhz);
        let name = output.name();
        let stats = self.perf_stats.entry(name.clone()).or_default();
        let stutter = stats.record_frame(now, threshold);
        let stutter_count = stats.stutter_count();
        let frame_time = Duration::from_secs_f64(stats.last_frame_ms() / 1000.0);
        if let Some(frame_time) = stutter
            && self.config.performance.stutter_log
        {
            warn!(
                output = %name,
                frame_time_ms = frame_time.as_secs_f64() * 1000.0,
                threshold_ms = threshold.as_secs_f64() * 1000.0,
                total_stutters = stutter_count,
                "frame stutter detected"
            );
        }
        if crate::frame_capture::is_capturing(self) {
            let live_output_count = self.space.outputs().count();
            crate::frame_capture::record_frame(
                self,
                &name,
                frame_index,
                frame_time,
                stutter.is_some(),
                live_output_count,
            );
        }
    }

    /// Records that `output` just had a repaint attempt at `now` that found
    /// no damage (nothing changed) and so rendered nothing - the
    /// [`record_frame_stats`](Self::record_frame_stats) counterpart for that
    /// outcome, feeding the FPS overlay's "SFPS" (skipped) reading (see
    /// `crate::perf_overlay::FrameStats::record_skip`). Both backends call
    /// this from the same place they'd otherwise call `record_frame_stats`.
    pub fn record_skipped_frame(&mut self, output: &Output, now: Instant) {
        let stats = self.perf_stats.entry(output.name()).or_default();
        stats.record_skip(now);
    }

    /// Spawns XWayland, wiring up the X11 window manager once it's ready.
    /// `on_settled` runs exactly once after that - either once XWayland
    /// actually came up, or once it's given up (crashed on startup) - so a
    /// caller that needs XWayland's `DISPLAY` to already be in the
    /// environment of anything it spawns next (see
    /// `crate::session::announce_xwayland_ready`'s doc) can defer that
    /// spawning until this fires, rather than racing XWayland's own
    /// (asynchronous, variable-latency) startup.
    #[cfg(feature = "xwayland")]
    pub fn start_xwayland(&mut self, on_settled: impl FnOnce(&mut Self) + 'static) {
        use std::process::Stdio;

        use std::{cell::RefCell, rc::Rc};

        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        use smithay::wayland::compositor::CompositorHandler;

        type SettledCallback<S> = Rc<RefCell<Option<Box<dyn FnOnce(&mut S)>>>>;
        let on_settled: SettledCallback<Self> = Rc::new(RefCell::new(Some(Box::new(on_settled))));
        let (xwayland, client) = XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            std::iter::empty::<String>(),
            true,
            Stdio::null(),
            Stdio::inherit(),
            |_| (),
        )
        .expect("failed to start XWayland");

        let display_handle = self.display_handle.clone();
        let client_id = client.id();
        let settled = on_settled.clone();
        let ret = self
            .handle
            .insert_source(xwayland, move |event, _, data| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    let xwayland_scale = std::env::var("ANVIL_XWAYLAND_SCALE")
                        .ok()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(1.);
                    data.client_compositor_state(&client)
                        .set_client_scale(xwayland_scale);
                    let mut wm = X11Wm::start_wm(
                        data.handle.clone(),
                        &display_handle,
                        x11_socket,
                        client.clone(),
                    )
                    .expect("Failed to attach X11 Window Manager");

                    let cursor = Cursor::load(&data.config.cursor);
                    let image = cursor.get_image(1, Duration::ZERO);
                    wm.set_cursor(
                        &image.pixels_rgba,
                        Size::from((image.width as u16, image.height as u16)),
                        Point::from((image.xhot as u16, image.yhot as u16)),
                    )
                    .expect("Failed to set xwayland default cursor");
                    data.xwm = Some(wm);
                    data.xdisplay = Some(display_number);
                    crate::session::announce_xwayland_ready(display_number);
                    if let Some(on_settled) = settled.borrow_mut().take() {
                        on_settled(data);
                    }
                }
                XWaylandEvent::Error => {
                    warn!("XWayland crashed on startup");
                    if let Some(on_settled) = settled.borrow_mut().take() {
                        on_settled(data);
                    }
                }
            });
        let token = match ret {
            Ok(token) => token,
            Err(e) => {
                tracing::error!(
                    "Failed to insert the XWaylandSource into the event loop: {}",
                    e
                );
                return;
            }
        };

        // Smithay's XWayland source never reports a child that dies before
        // writing its displayfd: the pipe hits EOF, `take_socket` maps that
        // to "not ready yet", and the level-triggered source stays readable
        // forever - the event loop then spins a full core and `on_settled`
        // never fires. Watch for the client disappearing before XWayland
        // became ready and tear the source down ourselves.
        let _ = self.handle.insert_source(
            Timer::from_duration(Duration::from_millis(250)),
            move |_, _, data| {
                if data.xwm.is_some() || on_settled.borrow().is_none() {
                    return TimeoutAction::Drop;
                }
                if data
                    .display_handle
                    .backend_handle()
                    .get_client_data(client_id.clone())
                    .is_ok()
                {
                    return TimeoutAction::ToDuration(Duration::from_millis(250));
                }
                warn!("XWayland exited before becoming ready; running without X11 support");
                data.handle.remove(token);
                if let Some(on_settled) = on_settled.borrow_mut().take() {
                    on_settled(data);
                }
                TimeoutAction::Drop
            },
        );
    }
}

pub trait Backend {
    const HAS_RELATIVE_MOTION: bool = false;
    const HAS_GESTURES: bool = false;
    fn seat_name(&self) -> String;
    fn reset_buffers(&mut self, output: &Output);
    fn early_import(&mut self, surface: &WlSurface);
    fn update_led_state(&mut self, led_state: LedState);
    fn apply_output_config(_state: &mut AnvilState<Self>, _old_config: &crate::config::Config)
    where
        Self: Sized,
    {
    }
    /// Reloads the rendered mouse cursor after `cursor` settings change.
    /// No-op by default: only backends that render their own cursor image
    /// (currently the udev/DRM backend) need to act on this.
    fn apply_cursor_config(_state: &mut AnvilState<Self>)
    where
        Self: Sized,
    {
    }
}
