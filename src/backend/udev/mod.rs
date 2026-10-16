// Allow in this module because of existing usage
#![allow(clippy::uninlined_format_args)]
use std::{
    collections::hash_map::HashMap,
    io,
    ops::Not,
    path::Path,
    sync::{Mutex, Once, atomic::Ordering},
    time::{Duration, Instant},
};

use crate::{
    drawing::*,
    render::*,
    shell::WindowElement,
    state::{AnvilState, Backend, take_presentation_feedback, update_primary_scanout_output},
};
use crate::{
    shell::WindowRenderElement,
    state::{DndIcon, SurfaceDmabufFeedback},
};
#[cfg(feature = "renderer_sync")]
use smithay::backend::drm::compositor::PrimaryPlaneElement;
#[cfg(feature = "egl")]
use smithay::backend::renderer::ImportEgl;
#[cfg(feature = "debug")]
use smithay::backend::renderer::{ImportMem, multigpu::MultiTexture};
use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Fourcc, Modifier,
            dmabuf::Dmabuf,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            CreateDrmNodeError, DrmAccessError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent,
            DrmEventMetadata, DrmEventTime, DrmNode, DrmSurface, GbmBufferedSurface, NodeType,
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{self, EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            DebugFlags, ImportDma, ImportMemWl,
            damage::Error as OutputDamageTrackerError,
            element::{
                AsRenderElements, Kind, RenderElementStates,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
            gles::{Capability, GlesRenderer},
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{
            Event as SessionEvent, Session,
            libseat::{self, LibSeatSession},
        },
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        space::{Space, SurfaceTree},
        utils::OutputPresentationFeedback,
    },
    input::{
        keyboard::LedState,
        pointer::{CursorImageAttributes, CursorImageStatus},
    },
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::{
            Device as _,
            control::{Device, Mode as DrmMode, ModeTypeFlags, connector, crtc},
        },
        input::{DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::{Display, DisplayHandle, backend::GlobalId, protocol::wl_surface},
    },
    utils::{
        DeviceFd, IsAlive, Logical, Monotonic, Physical, Point, Rectangle, Scale, Size, Time,
        Transform,
    },
    wayland::{
        compositor,
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        drm_lease::{
            DrmLease, DrmLeaseBuilder, DrmLeaseHandler, DrmLeaseRequest, DrmLeaseState,
            LeaseRejected,
        },
        drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState, supports_syncobj_eventfd},
        presentation::Refresh,
    },
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};
use tracing::{debug, error, info, trace, warn};

// we cannot simply pick the first supported format of the intersection of *all* formats, because:
// - we do not want something like Abgr4444, which looses color information, if something better is available
// - some formats might perform terribly
// - we might need some work-arounds, if one supports modifiers, but the other does not
//
// So lets just pick `ARGB2101010` (10-bit) or `ARGB8888` (8-bit) for now, they are widely supported.
const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const SUPPORTED_FORMATS_8BIT_ONLY: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// Lets [`crate::rounded_corners`] reach into the udev backend's per-GPU
/// `GlesRenderer`/`GlesFrame` that `MultiRenderer`/`MultiFrame` wrap, using
/// the `AsMut` impls smithay's multi-GPU renderer already provides for
/// exactly this pair of backend types (both render and target node use the
/// same `GbmGlesBackend<GlesRenderer, _>` here, so there's only one GLES
/// renderer/frame to recover).
impl<'a> crate::rounded_corners::GlesCapable for UdevRenderer<'a> {
    fn gles_renderer(&mut self) -> &mut GlesRenderer {
        std::convert::AsMut::<GlesRenderer>::as_mut(self)
    }

    fn gles_frame<'b, 'frame, 'buffer>(
        frame: &'b mut <Self as smithay::backend::renderer::RendererSuper>::Frame<'frame, 'buffer>,
    ) -> &'b mut smithay::backend::renderer::gles::GlesFrame<'frame, 'buffer> {
        std::convert::AsMut::<smithay::backend::renderer::gles::GlesFrame<'frame, 'buffer>>::as_mut(
            frame,
        )
    }

    fn gles_texture(
        gles_frame: &smithay::backend::renderer::gles::GlesFrame<'_, '_>,
        texture: &smithay::backend::renderer::multigpu::MultiTexture,
    ) -> Option<smithay::backend::renderer::gles::GlesTexture> {
        use smithay::backend::renderer::Frame;
        // `MultiTexture` caches one native texture per GPU node it has been
        // imported for; this recovers the one for whichever node `gles_frame`
        // belongs to (the node currently compositing this output). Absent
        // when the surface's buffer hasn't been imported for that node -
        // possible on hybrid graphics if a client's buffer still lives only
        // on a different GPU than the one rendering the current output.
        texture.get::<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>(&gles_frame.context_id())
    }

    fn map_gles_error(err: smithay::backend::renderer::gles::GlesError) -> Self::Error {
        smithay::backend::renderer::multigpu::Error::Render(err)
    }
}

/// Which DRM device/CRTC an [`Output`] is currently driven by. The fields are
/// cells because a parked output (see `workspaces.keep_disconnected`) is
/// reused when its monitor returns, possibly on a different CRTC.
#[derive(Debug, PartialEq)]
struct UdevOutputId {
    device_id: std::cell::Cell<DrmNode>,
    crtc: std::cell::Cell<crtc::Handle>,
}

impl UdevOutputId {
    fn new(device_id: DrmNode, crtc: crtc::Handle) -> Self {
        UdevOutputId {
            device_id: std::cell::Cell::new(device_id),
            crtc: std::cell::Cell::new(crtc),
        }
    }

    fn set(&self, device_id: DrmNode, crtc: crtc::Handle) {
        self.device_id.set(device_id);
        self.crtc.set(crtc);
    }
}

pub struct UdevData {
    pub session: LibSeatSession,
    dh: DisplayHandle,
    dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
    syncobj_state: Option<DrmSyncobjState>,
    primary_gpu: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    backends: HashMap<DrmNode, BackendData>,
    pointer_images: Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
    pointer_element: PointerElement,
    #[cfg(feature = "debug")]
    fps_texture: Option<MultiTexture>,
    pointer_image: crate::cursor::Cursor,
    debug_flags: DebugFlags,
    keyboards: Vec<smithay::reexports::input::Device>,
}

impl UdevData {
    pub fn set_debug_flags(&mut self, flags: DebugFlags) {
        if self.debug_flags != flags {
            self.debug_flags = flags;

            for backend in self.backends.values_mut() {
                for surface in backend.surfaces.values_mut() {
                    surface.drm_output.set_debug_flags(flags);
                }
            }
        }
    }

    pub fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
    }
}

impl DmabufHandler for AnvilState<UdevData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.backend_data.dmabuf_state.as_mut().unwrap().0
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self
            .backend_data
            .gpus
            .single_renderer(&self.backend_data.primary_gpu)
            .and_then(|mut renderer| renderer.import_dmabuf(&dmabuf, None))
            .is_ok()
        {
            if dmabuf.node().is_none() {
                dmabuf.set_node(self.backend_data.primary_gpu);
            }
            let _ = notifier.successful::<AnvilState<UdevData>>();
        } else {
            notifier.failed();
        }
    }
}

impl Backend for UdevData {
    const HAS_RELATIVE_MOTION: bool = true;
    const HAS_GESTURES: bool = true;

    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<UdevOutputId>()
            && let Some(gpu) = self.backends.get_mut(&id.device_id.get())
                && let Some(surface) = gpu.surfaces.get_mut(&id.crtc.get()) {
                    surface.drm_output.reset_buffers();
                }
    }

    fn early_import(&mut self, surface: &wl_surface::WlSurface) {
        if let Err(err) = self.gpus.early_import(self.primary_gpu, surface) {
            warn!("Early buffer import failed: {}", err);
        }
    }

    fn apply_cursor_config(state: &mut AnvilState<Self>) {
        state.backend_data.pointer_image = crate::cursor::Cursor::load(&state.config.cursor);
        // Cached render buffers are keyed off the xcursor `Image` they were
        // built from, so stale entries from the old theme just go unused;
        // dropping them avoids holding onto memory for a theme no longer in
        // use.
        state.backend_data.pointer_images.clear();
    }

    fn update_led_state(&mut self, led_state: LedState) {
        for keyboard in self.keyboards.iter_mut() {
            keyboard.led_update(led_state.into());
        }
    }

    fn apply_output_config(state: &mut AnvilState<Self>, old_config: &crate::config::Config) {
        let UdevData {
            gpus,
            backends,
            primary_gpu,
            ..
        } = &mut state.backend_data;

        for (node, backend) in backends.iter_mut() {
            let render_node = backend.render_node.unwrap_or(*primary_gpu);
            let Ok(mut renderer) = gpus.single_renderer(&render_node) else {
                warn!(
                    ?node,
                    "Could not acquire renderer while applying monitor refresh rate"
                );
                continue;
            };

            for surface in backend.surfaces.values_mut() {
                let name = surface.output.name();
                let old_rate = old_config.output_settings(&name).refresh_rate;
                let requested_rate = state.config.output_settings(&name).refresh_rate;
                if old_rate == requested_rate {
                    continue;
                }

                let mode = requested_rate
                    .filter(|refresh| *refresh > 0)
                    .and_then(|requested| {
                        surface
                            .available_modes
                            .iter()
                            .filter(|mode| mode.size() == surface.preferred_mode.size())
                            .min_by_key(|mode| (WlMode::from(**mode).refresh - requested).abs())
                            .copied()
                    })
                    .unwrap_or(surface.preferred_mode);

                if surface
                    .drm_output
                    .with_compositor(|compositor| compositor.current_mode())
                    == mode
                {
                    continue;
                }

                match surface
                    .drm_output
                    .use_mode::<_, WindowRenderElement<UdevRenderer<'_>>>(
                        mode,
                        &mut renderer,
                        &DrmOutputRenderElements::default(),
                    ) {
                    Ok(()) => {
                        let wl_mode = WlMode::from(mode);
                        surface
                            .output
                            .change_current_state(Some(wl_mode), None, None, None);
                        info!(
                            output = name,
                            refresh = wl_mode.refresh,
                            "Applied monitor refresh rate"
                        );
                    }
                    Err(err) => warn!(output = name, ?err, "Failed to apply monitor refresh rate"),
                }
            }
        }
    }
}

pub fn run_udev() {
    let mut event_loop = EventLoop::try_new().unwrap();
    let display = Display::new().unwrap();
    let mut display_handle = display.handle();

    /*
     * Initialize session
     */
    let (session, notifier) = match LibSeatSession::new() {
        Ok(ret) => ret,
        Err(err) => {
            error!("Could not initialize a session: {}", err);
            return;
        }
    };

    /*
     * Initialize the compositor
     */
    let primary_gpu = if let Ok(var) = std::env::var("ANVIL_DRM_DEVICE") {
        DrmNode::from_path(var).expect("Invalid drm device path")
    } else {
        primary_gpu(session.seat())
            .unwrap()
            .and_then(|x| {
                DrmNode::from_path(x)
                    .ok()?
                    .node_with_type(NodeType::Render)?
                    .ok()
            })
            .unwrap_or_else(|| {
                all_gpus(session.seat())
                    .unwrap()
                    .into_iter()
                    .find_map(|x| DrmNode::from_path(x).ok())
                    .expect("No GPU!")
            })
    };
    info!("Using {} as primary gpu.", primary_gpu);

    let gpus = GpuManager::new(GbmGlesBackend::with_factory(|display| {
        let context = EGLContext::new_with_priority(display, ContextPriority::High)?;
        let mut capabilities = unsafe { GlesRenderer::supported_capabilities(&context)? };
        if std::env::var("ANVIL_GLES_DISABLE_INSTANCING").is_ok() {
            capabilities.retain(|capability| *capability != Capability::Instancing);
        }
        Ok(unsafe { GlesRenderer::with_capabilities(context, capabilities)? })
    }))
    .unwrap();

    let data = UdevData {
        dh: display_handle.clone(),
        dmabuf_state: None,
        syncobj_state: None,
        session,
        primary_gpu,
        gpus,
        backends: HashMap::new(),
        pointer_image: crate::cursor::Cursor::load(&crate::config::Config::load().cursor),
        pointer_images: Vec::new(),
        pointer_element: PointerElement::default(),
        #[cfg(feature = "debug")]
        fps_texture: None,
        debug_flags: DebugFlags::empty(),
        keyboards: Vec::new(),
    };
    let mut state = AnvilState::init(display, event_loop.handle(), data, true);

    /*
     * Initialize the udev backend
     */
    let udev_backend = match UdevBackend::new(&state.seat_name) {
        Ok(ret) => ret,
        Err(err) => {
            error!(error = ?err, "Failed to initialize udev backend");
            return;
        }
    };

    /*
     * Initialize libinput backend
     */
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        state.backend_data.session.clone().into(),
    );
    libinput_context.udev_assign_seat(&state.seat_name).unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    /*
     * Bind all our objects that get driven by the event loop
     */
    event_loop
        .handle()
        .insert_source(libinput_backend, move |mut event, _, data| {
            let dh = data.backend_data.dh.clone();
            if let InputEvent::DeviceAdded { device } = &mut event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    if let Some(led_state) = data
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state())
                    {
                        device.led_update(led_state.into());
                    }
                    data.backend_data.keyboards.push(device.clone());
                }
            } else if let InputEvent::DeviceRemoved { ref device } = event
                && device.has_capability(DeviceCapability::Keyboard) {
                    data.backend_data.keyboards.retain(|item| item != device);
                }

            data.process_input_event(&dh, event)
        })
        .unwrap();

    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), data| match event {
            SessionEvent::PauseSession => {
                libinput_context.suspend();
                info!("pausing session");

                for backend in data.backend_data.backends.values_mut() {
                    backend.drm_output_manager.pause();
                    backend.active_leases.clear();
                    if let Some(lease_global) = backend.leasing_global.as_mut() {
                        lease_global.suspend();
                    }
                }
            }
            SessionEvent::ActivateSession => {
                info!("resuming session");

                if let Err(err) = libinput_context.resume() {
                    error!("Failed to resume libinput context: {:?}", err);
                }
                for (node, backend) in data
                    .backend_data
                    .backends
                    .iter_mut()
                    .map(|(handle, backend)| (*handle, backend))
                {
                    // if we do not care about flicking (caused by modesetting) we could just
                    // pass true for disable connectors here. this would make sure our drm
                    // device is in a known state (all connectors and planes disabled).
                    // but for demonstration we choose a more optimistic path by leaving the
                    // state as is and assume it will just work. If this assumption fails
                    // we will try to reset the state when trying to queue a frame.
                    backend
                        .drm_output_manager
                        .lock()
                        .activate(false)
                        .expect("failed to activate drm backend");
                    for surface in backend.surfaces.values_mut() {
                        surface.gamma_dirty = true;
                    }
                    if let Some(lease_global) = backend.leasing_global.as_mut() {
                        lease_global.resume::<AnvilState<UdevData>>();
                    }
                    data.handle
                        .insert_idle(move |data| data.render(node, None, data.clock.now()));
                }
            }
        })
        .unwrap();

    // We try to initialize the primary node before others to make sure
    // any display only node can fall back to the primary node for rendering
    let primary_node = primary_gpu
        .node_with_type(NodeType::Primary)
        .and_then(|node| node.ok());
    let primary_device = udev_backend.device_list().find(|(device_id, _)| {
        primary_node
            .map(|primary_node| *device_id == primary_node.dev_id())
            .unwrap_or(false)
            || *device_id == primary_gpu.dev_id()
    });

    if let Some((device_id, path)) = primary_device {
        let node = DrmNode::from_dev_id(device_id).expect("failed to get primary node");
        state
            .device_added(node, path)
            .expect("failed to initialize primary node");
    }

    let primary_device_id = primary_device.map(|(device_id, _)| device_id);
    for (device_id, path) in udev_backend.device_list() {
        if Some(device_id) == primary_device_id {
            continue;
        }

        if let Err(err) = DrmNode::from_dev_id(device_id)
            .map_err(DeviceAddError::DrmNode)
            .and_then(|node| state.device_added(node, path))
        {
            error!("Skipping device {device_id}: {err}");
        }
    }
    state.shm_state.update_formats(
        state
            .backend_data
            .gpus
            .single_renderer(&primary_gpu)
            .unwrap()
            .shm_formats(),
    );

    #[cfg_attr(not(feature = "egl"), allow(unused_mut))]
    let mut renderer = state
        .backend_data
        .gpus
        .single_renderer(&primary_gpu)
        .unwrap();

    #[cfg(feature = "debug")]
    {
        #[allow(deprecated)]
        let fps_image = image::io::Reader::with_format(
            std::io::Cursor::new(FPS_NUMBERS_PNG),
            image::ImageFormat::Png,
        )
        .decode()
        .unwrap();
        let fps_texture = renderer
            .import_memory(
                &fps_image.to_rgba8(),
                Fourcc::Abgr8888,
                (fps_image.width() as i32, fps_image.height() as i32).into(),
                false,
            )
            .expect("Unable to upload FPS texture");

        for backend in state.backend_data.backends.values_mut() {
            for surface in backend.surfaces.values_mut() {
                surface.fps_element = Some(FpsElement::new(fps_texture.clone()));
            }
        }
        state.backend_data.fps_texture = Some(fps_texture);
    }

    #[cfg(feature = "egl")]
    {
        info!(
            ?primary_gpu,
            "Trying to initialize EGL Hardware Acceleration",
        );
        match renderer.bind_wl_display(&display_handle) {
            Ok(_) => info!("EGL hardware-acceleration enabled"),
            Err(err) => info!(?err, "Failed to initialize EGL hardware-acceleration"),
        }
    }

    // init dmabuf support with format list from our primary gpu
    let dmabuf_formats = renderer.dmabuf_formats();
    let default_feedback = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), dmabuf_formats)
        .build()
        .unwrap();
    let mut dmabuf_state = DmabufState::new();
    let global = dmabuf_state.create_global_with_default_feedback::<AnvilState<UdevData>>(
        &display_handle,
        &default_feedback,
    );
    state.backend_data.dmabuf_state = Some((dmabuf_state, global));

    let gpus = &mut state.backend_data.gpus;
    state
        .backend_data
        .backends
        .iter_mut()
        .for_each(|(node, backend_data)| {
            // Update the per drm surface dmabuf feedback
            backend_data.surfaces.values_mut().for_each(|surface_data| {
                surface_data.dmabuf_feedback = surface_data.dmabuf_feedback.take().or_else(|| {
                    surface_data.drm_output.with_compositor(|compositor| {
                        get_surface_dmabuf_feedback(
                            primary_gpu,
                            surface_data.render_node,
                            *node,
                            gpus,
                            compositor.surface(),
                        )
                    })
                });
            });
        });

    // Expose syncobj protocol if supported by primary GPU
    if let Some(primary_node) = state
        .backend_data
        .primary_gpu
        .node_with_type(NodeType::Primary)
        .and_then(|x| x.ok())
        && let Some(backend) = state.backend_data.backends.get(&primary_node) {
            let import_device = backend.drm_output_manager.device().device_fd().clone();
            if supports_syncobj_eventfd(&import_device) {
                let syncobj_state =
                    DrmSyncobjState::new::<AnvilState<UdevData>>(&display_handle, import_device);
                state.backend_data.syncobj_state = Some(syncobj_state);
            }
        }

    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, data| match event {
            UdevEvent::Added { device_id, path } => {
                if let Err(err) = DrmNode::from_dev_id(device_id)
                    .map_err(DeviceAddError::DrmNode)
                    .and_then(|node| data.device_added(node, &path))
                {
                    error!("Skipping device {device_id}: {err}");
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    data.device_changed(node)
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    data.device_removed(node)
                }
            }
        })
        .unwrap();

    /*
     * Start XWayland if supported. Session announcement (which starts
     * `graphical-session.target` and, through it, the shell) is deferred
     * until XWayland has actually settled, rather than fired right away -
     * otherwise the shell can be forked with `DISPLAY` still unset,
     * permanently missing the later env update (see
     * `crate::session::announce_xwayland_ready`'s doc) and failing to
     * launch any XWayland-dependent app (e.g. Steam) for the rest of that
     * session.
     */
    #[cfg(feature = "xwayland")]
    let socket_name = state.socket_name.clone();
    #[cfg(feature = "xwayland")]
    state.start_xwayland(move |_| crate::session::announce_session_start(socket_name.as_deref()));

    #[cfg(feature = "libei")]
    crate::libei::listen_eis(&event_loop.handle());

    #[cfg(not(feature = "xwayland"))]
    crate::session::announce_session_start(state.socket_name.as_deref());

    /*
     * And run our loop
     */

    while state.running.load(Ordering::SeqCst) {
        let result = event_loop.dispatch(Some(Duration::from_millis(16)), &mut state);
        if result.is_err() {
            state.running.store(false, Ordering::SeqCst);
        } else {
            state.reload_config_if_changed();
            state.space.refresh();
            if crate::shell::tiling::cleanup_dead(&mut state) {
                crate::foreign_toplevel::sync(&mut state);
            }
            state.popups.cleanup();
            display_handle.flush_clients().unwrap();
        }
    }

    crate::session::announce_session_end();
}

impl DrmLeaseHandler for AnvilState<UdevData> {
    fn drm_lease_state(&mut self, node: DrmNode) -> &mut DrmLeaseState {
        self.backend_data
            .backends
            .get_mut(&node)
            .unwrap()
            .leasing_global
            .as_mut()
            .unwrap()
    }

    fn lease_request(
        &mut self,
        node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        let backend = self
            .backend_data
            .backends
            .get(&node)
            .ok_or(LeaseRejected::default())?;

        let drm_device = backend.drm_output_manager.device();
        let mut builder = DrmLeaseBuilder::new(drm_device);
        for conn in request.connectors {
            if let Some((_, crtc)) = backend
                .non_desktop_connectors
                .iter()
                .find(|(handle, _)| *handle == conn)
            {
                builder.add_connector(conn);
                builder.add_crtc(*crtc);
                let planes = drm_device.planes(crtc).map_err(LeaseRejected::with_cause)?;
                let (primary_plane, primary_plane_claim) = planes
                    .primary
                    .iter()
                    .find_map(|plane| {
                        drm_device
                            .claim_plane(plane.handle, *crtc)
                            .map(|claim| (plane, claim))
                    })
                    .ok_or_else(LeaseRejected::default)?;
                builder.add_plane(primary_plane.handle, primary_plane_claim);
                if let Some((cursor, claim)) = planes.cursor.iter().find_map(|plane| {
                    drm_device
                        .claim_plane(plane.handle, *crtc)
                        .map(|claim| (plane, claim))
                }) {
                    builder.add_plane(cursor.handle, claim);
                }
            } else {
                tracing::warn!(
                    ?conn,
                    "Lease requested for desktop connector, denying request"
                );
                return Err(LeaseRejected::default());
            }
        }

        Ok(builder)
    }

    fn new_active_lease(&mut self, node: DrmNode, lease: DrmLease) {
        let backend = self.backend_data.backends.get_mut(&node).unwrap();
        backend.active_leases.push(lease);
    }

    fn lease_destroyed(&mut self, node: DrmNode, lease: u32) {
        let backend = self.backend_data.backends.get_mut(&node).unwrap();
        backend.active_leases.retain(|l| l.id() != lease);
    }
}

impl DrmSyncobjHandler for AnvilState<UdevData> {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.backend_data.syncobj_state.as_mut()
    }
}

pub type RenderSurface =
    GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, Option<OutputPresentationFeedback>>;

pub type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmDevice<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

struct SurfaceData {
    dh: DisplayHandle,
    device_id: DrmNode,
    render_node: Option<DrmNode>,
    output: Output,
    global: Option<GlobalId>,
    drm_output: DrmOutput<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    available_modes: Vec<DrmMode>,
    preferred_mode: DrmMode,
    disable_direct_scanout: bool,
    #[cfg(feature = "debug")]
    fps: fps_ticker::Fps,
    #[cfg(feature = "debug")]
    fps_element: Option<FpsElement<MultiTexture>>,
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    last_presentation_time: Option<Time<Monotonic>>,
    vblank_throttle_timer: Option<RegistrationToken>,
    /// Set by `frame_finish` right before scheduling the repaint timer, so
    /// `render_surface` can report `crate::frame_capture`'s "since_vblank"
    /// stage - how long actually elapsed between the vblank event and the
    /// compositor starting its own repaint, including any timer-scheduling
    /// overhead on top of the intentional `repaint_delay`. Only ever set
    /// while a capture session is open (see `frame_capture::is_capturing`);
    /// taken (not just read) by `render_surface` so a frame that's
    /// rescheduled without an intervening vblank doesn't report stale data.
    pending_vblank_at: Option<Instant>,
    /// Monotonic per-output frame counter feeding `crate::frame_capture`'s
    /// `frame_index` - unrelated to `crate::perf_overlay::FrameStats`'s own
    /// history, which is keyed by output name instead and serves a
    /// different purpose (the FPS overlay's rolling average).
    capture_frame_index: u32,
    /// Night-mode color temperature currently loaded into this CRTC's gamma
    /// LUT (`None` = identity ramp).
    gamma_kelvin: Option<u32>,
    /// The LUT must be rewritten even if `gamma_kelvin` already matches (set
    /// when the session is re-activated, which may have lost it).
    gamma_dirty: bool,
    /// Setting the LUT failed (no gamma support on this CRTC); night mode
    /// falls back to the tint overlay for this output.
    gamma_unsupported: bool,
}

impl SurfaceData {
    /// Brings the CRTC's gamma LUT in line with the night-mode `kelvin` and
    /// returns whether the LUT is applying it (so the overlay isn't needed).
    fn sync_gamma(&mut self, drm: &DrmDevice, crtc: crtc::Handle, kelvin: Option<u32>) -> bool {
        if self.gamma_unsupported {
            return false;
        }
        if !self.gamma_dirty && self.gamma_kelvin == kelvin {
            return kelvin.is_some();
        }
        let result = drm.get_crtc(crtc).and_then(|info| {
            let len = info.gamma_length() as usize;
            if len < 2 {
                return Err(std::io::Error::other("CRTC has no gamma LUT"));
            }
            let [r, g, b] = crate::tint::gamma_ramp(kelvin, len);
            drm.set_gamma(crtc, &r, &g, &b)
        });
        match result {
            Ok(()) => {
                self.gamma_kelvin = kelvin;
                self.gamma_dirty = false;
                kelvin.is_some()
            }
            Err(err) => {
                warn!(
                    "Failed to set gamma LUT on {}, using the tint overlay for night mode: {err}",
                    self.output.name()
                );
                self.gamma_unsupported = true;
                false
            }
        }
    }
}

impl Drop for SurfaceData {
    fn drop(&mut self) {
        self.output.leave_all();
        if let Some(global) = self.global.take() {
            self.dh.remove_global::<AnvilState<UdevData>>(global);
        }
    }
}

struct BackendData {
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    non_desktop_connectors: Vec<(connector::Handle, crtc::Handle)>,
    leasing_global: Option<DrmLeaseState>,
    active_leases: Vec<DrmLease>,
    drm_output_manager: DrmOutputManager<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        Option<OutputPresentationFeedback>,
        DrmDeviceFd,
    >,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    registration_token: RegistrationToken,
}

#[derive(Debug, thiserror::Error)]
enum DeviceAddError {
    #[error("Failed to open device using libseat: {0}")]
    DeviceOpen(libseat::Error),
    #[error("Failed to initialize drm device: {0}")]
    DrmDevice(DrmError),
    #[error("Failed to initialize gbm device: {0}")]
    GbmDevice(std::io::Error),
    #[error("Failed to access drm node: {0}")]
    DrmNode(CreateDrmNodeError),
    #[error("Failed to add device to GpuManager: {0}")]
    AddNode(egl::Error),
    #[error("The device has no render node")]
    NoRenderNode,
    #[error("Primary GPU is missing")]
    PrimaryGpuMissing,
}

fn get_surface_dmabuf_feedback(
    primary_gpu: DrmNode,
    render_node: Option<DrmNode>,
    scanout_node: DrmNode,
    gpus: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surface: &DrmSurface,
) -> Option<SurfaceDmabufFeedback> {
    let primary_formats = gpus.single_renderer(&primary_gpu).ok()?.dmabuf_formats();
    let render_formats = if let Some(render_node) = render_node {
        gpus.single_renderer(&render_node).ok()?.dmabuf_formats()
    } else {
        FormatSet::default()
    };

    let all_render_formats = primary_formats
        .iter()
        .chain(render_formats.iter())
        .copied()
        .collect::<FormatSet>();

    let planes = surface.planes().clone();

    // We limit the scan-out tranche to formats we can also render from
    // so that there is always a fallback render path available in case
    // the supplied buffer can not be scanned out directly
    let planes_formats = surface
        .plane_info()
        .formats
        .iter()
        .copied()
        .chain(planes.overlay.into_iter().flat_map(|p| p.formats))
        .collect::<FormatSet>()
        .intersection(&all_render_formats)
        .copied()
        .collect::<FormatSet>();

    let builder = DmabufFeedbackBuilder::new(primary_gpu.dev_id(), primary_formats);
    let render_feedback = if let Some(render_node) = render_node {
        builder
            .clone()
            .add_preference_tranche(
                render_node.dev_id(),
                zwp_linux_dmabuf_feedback_v1::TrancheFlags::Sampling,
                render_formats.clone(),
                3u32..=6,
            )
            .build()
            .unwrap()
    } else {
        builder.clone().build().unwrap()
    };

    let scanout_feedback = builder
        .add_preference_tranche(
            surface.device_fd().dev_id().unwrap(),
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout,
            planes_formats,
            4u32..=6,
        )
        .add_preference_tranche(
            scanout_node.dev_id(),
            zwp_linux_dmabuf_feedback_v1::TrancheFlags::Sampling,
            render_formats,
            4u32..=6,
        )
        .build()
        .unwrap();

    Some(SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    })
}
mod device;
mod render;
