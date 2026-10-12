use std::{
    sync::{Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

#[cfg(feature = "egl")]
use smithay::backend::renderer::ImportEgl;
#[cfg(feature = "debug")]
use smithay::{
    backend::{allocator::Fourcc, renderer::ImportMem},
    reexports::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
};

use smithay::{
    backend::{
        SwapBuffersError,
        allocator::dmabuf::Dmabuf,
        egl::EGLDevice,
        renderer::{
            ImportDma, ImportMemWl,
            damage::{Error as OutputDamageTrackerError, OutputDamageTracker},
            element::{AsRenderElements, Kind, memory::MemoryRenderBufferRenderElement},
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::{
        keyboard::LedState,
        pointer::{CursorImageAttributes, CursorImageStatus},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{Display, protocol::wl_surface},
        winit::event_loop::pump_events::PumpStatus,
    },
    utils::{IsAlive, Logical, Point, Scale, Transform},
    wayland::{
        compositor,
        dmabuf::{
            DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState,
            ImportNotifier,
        },
        presentation::Refresh,
    },
};
use tracing::{error, info, warn};

use crate::state::{
    AnvilState, Backend, take_presentation_feedback, update_primary_scanout_output,
};
use crate::{drawing::*, render::*, rounded_corners};

pub const OUTPUT_NAME: &str = "winit";

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: OutputDamageTracker,
    dmabuf_state: (DmabufState, DmabufGlobal, Option<DmabufFeedback>),
    full_redraw: u8,
    #[cfg(feature = "debug")]
    pub fps: fps_ticker::Fps,
    /// Monotonic frame counter feeding `crate::frame_capture`'s
    /// `frame_index` for this backend's one output - see
    /// `crate::udev::SurfaceData::capture_frame_index`'s doc comment for
    /// why this is separate from `crate::perf_overlay::FrameStats`.
    capture_frame_index: u32,
}

impl DmabufHandler for AnvilState<WinitData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.backend_data.dmabuf_state.0
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self
            .backend_data
            .backend
            .renderer()
            .import_dmabuf(&dmabuf, None)
            .is_ok()
        {
            let _ = notifier.successful::<AnvilState<WinitData>>();
        } else {
            notifier.failed();
        }
    }
}

impl Backend for WinitData {
    fn seat_name(&self) -> String {
        String::from("winit")
    }
    fn reset_buffers(&mut self, _output: &Output) {
        self.full_redraw = 4;
    }
    fn early_import(&mut self, _surface: &wl_surface::WlSurface) {}
    fn update_led_state(&mut self, _led_state: LedState) {}
}

pub fn run_winit() {
    let mut event_loop = EventLoop::try_new().unwrap();
    let display = Display::new().unwrap();
    let mut display_handle = display.handle();

    #[cfg_attr(not(feature = "egl"), allow(unused_mut))]
    let (mut backend, mut winit) = match winit::init::<GlesRenderer>() {
        Ok(ret) => ret,
        Err(err) => {
            error!("Failed to initialize Winit backend: {}", err);
            return;
        }
    };
    let size = backend.window_size();

    let mode = Mode {
        size,
        refresh: 60_000,
    };
    let output = Output::new(
        OUTPUT_NAME.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<AnvilState<WinitData>>(&display.handle());
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    #[cfg(feature = "debug")]
    #[allow(deprecated)]
    let fps_image = image::io::Reader::with_format(
        std::io::Cursor::new(FPS_NUMBERS_PNG),
        image::ImageFormat::Png,
    )
    .decode()
    .unwrap();
    #[cfg(feature = "debug")]
    let fps_texture = backend
        .renderer()
        .import_memory(
            &fps_image.to_rgba8(),
            Fourcc::Abgr8888,
            (fps_image.width() as i32, fps_image.height() as i32).into(),
            false,
        )
        .expect("Unable to upload FPS texture");
    #[cfg(feature = "debug")]
    let mut fps_element = FpsElement::new(fps_texture);

    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());

    let dmabuf_default_feedback = match render_node {
        Ok(Some(node)) => {
            let dmabuf_formats = backend.renderer().dmabuf_formats();
            let dmabuf_default_feedback = DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats)
                .build()
                .unwrap();
            Some(dmabuf_default_feedback)
        }
        Ok(None) => {
            warn!("failed to query render node, dmabuf will use v3");
            None
        }
        Err(err) => {
            warn!(?err, "failed to egl device for display, dmabuf will use v3");
            None
        }
    };

    // if we failed to build dmabuf feedback we fall back to dmabuf v3
    // Note: egl on Mesa requires either v4 or wl_drm (initialized with bind_wl_display)
    let dmabuf_state = if let Some(default_feedback) = dmabuf_default_feedback {
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global = dmabuf_state
            .create_global_with_default_feedback::<AnvilState<WinitData>>(
                &display.handle(),
                &default_feedback,
            );
        (dmabuf_state, dmabuf_global, Some(default_feedback))
    } else {
        let dmabuf_formats = backend.renderer().dmabuf_formats();
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global =
            dmabuf_state.create_global::<AnvilState<WinitData>>(&display.handle(), dmabuf_formats);
        (dmabuf_state, dmabuf_global, None)
    };

    #[cfg(feature = "egl")]
    if backend
        .renderer()
        .bind_wl_display(&display.handle())
        .is_ok()
    {
        info!("EGL hardware-acceleration enabled");
    };

    let data = {
        let damage_tracker = OutputDamageTracker::from_output(&output);

        WinitData {
            backend,
            damage_tracker,
            dmabuf_state,
            full_redraw: 0,
            #[cfg(feature = "debug")]
            fps: fps_ticker::Fps::default(),
            capture_frame_index: 0,
        }
    };
    let mut state = AnvilState::init(display, event_loop.handle(), data, true);
    state
        .shm_state
        .update_formats(state.backend_data.backend.renderer().shm_formats());
    state.space.map_output(&output, (0, 0));
    crate::shell::workspace::init_output(&state.config, &state.space, &output);
    crate::ext_workspace::ext_workspace_sync(&mut state);

    #[cfg(feature = "xwayland")]
    state.start_xwayland(|_| {});

    info!("Initialization completed, starting the main loop.");

    let mut pointer_element = PointerElement::default();

    while state.running.load(Ordering::SeqCst) {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                // We only have one output
                let output = state.space.outputs().next().unwrap().clone();
                state.space.map_output(&output, (0, 0));
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                crate::shell::fixup_positions(&mut state.space, state.pointer.current_location());
            }
            WinitEvent::Input(event) => state.process_input_event_windowed(event, OUTPUT_NAME),
            _ => (),
        });

        if let PumpStatus::Exit(_) = status {
            state.running.store(false, Ordering::SeqCst);
            break;
        }

        // drawing logic
        {
            let now = state.clock.now();
            let frame_target = now
                + output
                    .current_mode()
                    .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
                    .unwrap_or_default();
            state.pre_repaint(&output, frame_target);

            let focused_window_rect = crate::shell::tiling::current_focused_window(&state)
                .and_then(|w| state.space.element_bbox(&w));
            let drop_indicator = state.tiling_drop_indicator;
            let border = state.config.border.clone();
            let corner_radius = if state.config.corners.enabled {
                state.config.corners.radius as f32
            } else {
                0.0
            };

            let border_cache = state.border_cache.entry(output.name()).or_default();
            let drop_indicator_cache = state.drop_indicator_cache.entry(output.name()).or_default();

            let backend = &mut state.backend_data.backend;

            // draw the cursor as relevant
            // reset the cursor if the surface is no longer alive
            let mut reset = false;
            if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
                reset = !surface.alive();
            }
            if reset {
                state.cursor_status = CursorImageStatus::default_named();
            }
            let cursor_visible = !matches!(state.cursor_status, CursorImageStatus::Surface(_));

            pointer_element.set_status(state.cursor_status.clone());

            #[cfg(feature = "debug")]
            let fps = state.backend_data.fps.avg().round() as u32;
            #[cfg(feature = "debug")]
            fps_element.update_fps(fps);

            let scale = Scale::from(output.current_scale().fractional_scale());
            let output_size = state.space.output_geometry(&output).unwrap().size;
            let wallpaper_buffer = state.wallpaper.buffer_for(output_size).clone();
            let blurred_wallpaper_buffer = state.config.blur.enabled.then(|| {
                state
                    .wallpaper
                    .blurred_buffer_for(output_size, state.config.blur.radius)
                    .clone()
            });
            let launcher_buffer = state.launcher.ensure_buffer().cloned();
            let launcher_location = launcher_buffer.as_ref().map(|_| {
                let output_size = state.space.output_geometry(&output).unwrap().size;
                let launcher_size = state.launcher.logical_size();
                Point::<i32, Logical>::from((
                    (output_size.w - launcher_size.w) / 2,
                    (output_size.h - launcher_size.h) / 2,
                ))
                .to_f64()
                .to_physical(scale)
            });

            let performance = state.config.performance.clone();
            let overlay_interval = Duration::from_millis(performance.fps_overlay_interval_ms.into());
            let perf_overlay_buffer_and_location = performance.fps_overlay.then(|| {
                let stats = state.perf_stats.entry(output.name()).or_default();
                let cache = state.fps_overlay_cache.entry(output.name()).or_default();
                let buffer = cache.buffer(stats, overlay_interval).clone();
                let output_size = state.space.output_geometry(&output).unwrap().size;
                let location = crate::perf_overlay::overlay_location(
                    performance.fps_overlay_position,
                    output_size,
                )
                .to_f64()
                .to_physical(scale);
                (buffer, location)
            });

            let key_tracker_buffer_and_location = {
                let output_size = state.space.output_geometry(&output).unwrap().size;
                state.key_tracker.overlay(output_size).map(|(buffer, location)| {
                    (buffer.clone(), location.to_f64().to_physical(scale))
                })
            };

            let permission_prompt_buffer = state.permission_prompt.ensure_buffer().cloned();
            let permission_prompt_location = permission_prompt_buffer.as_ref().map(|_| {
                let output_size = state.space.output_geometry(&output).unwrap().size;
                state
                    .permission_prompt
                    .origin_in(output_size)
                    .to_f64()
                    .to_physical(scale)
            });

            // The monitor chooser (also `crate::permission_prompt`): the panel
            // listing monitors, plus this output's own number + name marker
            // in its top-left corner.
            let chooser_buffer = state.permission_prompt.ensure_chooser_buffer().cloned();
            let chooser_location = chooser_buffer.as_ref().map(|_| {
                let output_size = state.space.output_geometry(&output).unwrap().size;
                state
                    .permission_prompt
                    .chooser_origin_in(output_size)
                    .to_f64()
                    .to_physical(scale)
            });
            let chooser_label = state
                .permission_prompt
                .chooser_label(&output.name())
                .map(|(buffer, location)| (buffer.clone(), location.to_f64().to_physical(scale)));

            let workspace_overlay = state.workspace_overlay_shown.filter(|shown_at| {
                shown_at.elapsed().as_millis()
                    < crate::shell::workspace::OVERLAY_DURATION_MS as u128
            });
            let workspace_overlay_buffer_and_location = workspace_overlay.map(|_| {
                let (active, count) = crate::shell::workspace::overlay_info(&output);
                let buffer = crate::drawing::workspace_overlay_buffer(active, count);
                let output_size = state.space.output_geometry(&output).unwrap().size;
                let overlay_size = crate::drawing::workspace_overlay_size(count);
                let location = Point::<i32, Logical>::from((
                    (output_size.w - overlay_size.w) / 2,
                    output_size.h - overlay_size.h - 48,
                ))
                .to_f64()
                .to_physical(scale);
                (buffer, location)
            });
            if workspace_overlay.is_none() {
                state.workspace_overlay_shown = None;
            }

            let full_redraw = &mut state.backend_data.full_redraw;
            *full_redraw = full_redraw.saturating_sub(1);
            let space = &mut state.space;
            let damage_tracker = &mut state.backend_data.damage_tracker;
            let show_window_preview = state.show_window_preview;

            let dnd_icon = state.dnd_icon.as_ref();

            let cursor_hotspot =
                if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
                    compositor::with_states(surface, |states| {
                        states
                            .data_map
                            .get::<Mutex<CursorImageAttributes>>()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .hotspot
                    })
                } else {
                    (0, 0).into()
                };
            let cursor_pos = state.pointer.current_location();

            #[cfg(feature = "debug")]
            let mut renderdoc = state.renderdoc.as_mut();

            let age = if *full_redraw > 0 {
                0
            } else {
                backend.buffer_age().unwrap_or(0)
            };
            #[cfg(feature = "debug")]
            let window_handle = backend
                .window()
                .window_handle()
                .map(|handle| {
                    if let RawWindowHandle::Wayland(handle) = handle.as_raw() {
                        handle.surface.as_ptr()
                    } else {
                        std::ptr::null_mut()
                    }
                })
                .unwrap_or_else(|_| std::ptr::null_mut());
            rounded_corners::set_current(rounded_corners::CornersConfig {
                enabled: state.config.corners.enabled,
                radius: state.config.corners.radius as f32,
            });

            // Taken up-front (rather than through `state` inside the render
            // closure below, which already has `state.backend_data`
            // mutably borrowed as `backend`) - completed from the
            // just-rendered framebuffer right after `render_output`
            // succeeds, while it's still bound. See `crate::screencopy`.
            let pending_captures = state.screencopy.take_pending(&output);
            let capture_size = crate::screencopy::output_buffer_size(&output);
            let presented: Duration = frame_target.into();

            let build_and_draw_start = Instant::now();
            let render_res = backend.bind().and_then(|(renderer, mut fb)| {
                #[cfg(feature = "debug")]
                if let Some(renderdoc) = renderdoc.as_mut() {
                    renderdoc.start_frame_capture(
                        renderer.egl_context().get_context_handle(),
                        window_handle,
                    );
                }

                let mut elements = Vec::<CustomRenderElements<GlesRenderer>>::new();

                elements.extend(
                    pointer_element.render_elements(
                        renderer,
                        (cursor_pos - cursor_hotspot.to_f64())
                            .to_physical(scale)
                            .to_i32_round(),
                        scale,
                        1.0,
                    ),
                );

                // draw the dnd icon if any
                if let Some(icon) = dnd_icon {
                    let dnd_icon_pos = (cursor_pos + icon.offset.to_f64())
                        .to_physical(scale)
                        .to_i32_round();
                    if icon.surface.alive() {
                        elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
                            &smithay::desktop::space::SurfaceTree::from_surface(&icon.surface),
                            renderer,
                            dnd_icon_pos,
                            scale,
                            1.0,
                        ));
                    }
                }

                // The permission prompt (see `crate::permission_prompt`) is
                // pushed right after the pointer/dnd icon, ahead of every
                // other overlay - it's the one thing on screen a user must
                // never be able to accidentally cover, since answering it
                // wrong grants or denies a real capability.
                if let (Some(prompt_buffer), Some(location)) =
                    (&permission_prompt_buffer, permission_prompt_location)
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        location,
                        prompt_buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }

                if let (Some(buffer), Some(location)) = (&chooser_buffer, chooser_location)
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        location,
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }
                if let Some((buffer, location)) = &chooser_label
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        *location,
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }

                #[cfg(feature = "debug")]
                elements.push(CustomRenderElements::Fps(fps_element.clone()));

                if let Some((buffer, location)) = &key_tracker_buffer_and_location
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        *location,
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }

                if let Some((buffer, location)) = &perf_overlay_buffer_and_location
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        *location,
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }

                let background_element = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    Point::from((0.0, 0.0)),
                    &wallpaper_buffer,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                )
                .ok()
                .map(CustomRenderElements::Overlay);

                if let (Some(launcher_buffer), Some(location)) =
                    (&launcher_buffer, launcher_location)
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        location,
                        launcher_buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }

                if let Some((buffer, location)) = &workspace_overlay_buffer_and_location
                    && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        *location,
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        elements.push(CustomRenderElements::Overlay(element));
                    }

                render_output(
                    &output,
                    space,
                    elements,
                    background_element,
                    blurred_wallpaper_buffer.as_ref(),
                    renderer,
                    &mut fb,
                    damage_tracker,
                    age,
                    show_window_preview,
                    focused_window_rect,
                    &border,
                    corner_radius,
                    border_cache,
                    drop_indicator,
                    drop_indicator_cache,
                )
                .inspect(|_result| {
                    if let Some(size) = capture_size {
                        crate::screencopy::fulfill(
                            pending_captures,
                            renderer,
                            &fb,
                            size,
                            presented,
                        );
                    } else {
                        for frame in pending_captures {
                            frame.fail(
                                smithay::wayland::image_copy_capture::CaptureFailureReason::Unknown,
                            );
                        }
                    }
                })
                .map_err(|err| match err {
                    OutputDamageTrackerError::Rendering(err) => err.into(),
                    _ => unreachable!(),
                })
            });
            // Combined build+damage-track+draw time - `render_output`
            // (`render.rs`) doesn't expose a seam between "building the
            // element list" and "compositing it" the way `udev.rs`'s
            // separate `render_frame`/`queue_frame` calls do, so this
            // backend reports one coarser stage instead of splitting it.
            let build_and_draw = build_and_draw_start.elapsed();

            match render_res {
                Ok(render_output_result) => {
                    let has_rendered = render_output_result.damage.is_some();
                    let mut submit = Duration::ZERO;
                    if let Some(damage) = render_output_result.damage {
                        let submit_start = Instant::now();
                        if let Err(err) = backend.submit(Some(damage)) {
                            warn!("Failed to submit buffer: {}", err);
                        }
                        submit = submit_start.elapsed();
                    }

                    #[cfg(feature = "debug")]
                    if let Some(renderdoc) = renderdoc.as_mut() {
                        renderdoc.end_frame_capture(
                            backend.renderer().egl_context().get_context_handle(),
                            backend
                                .window()
                                .window_handle()
                                .map(|handle| {
                                    if let RawWindowHandle::Wayland(handle) = handle.as_raw() {
                                        handle.surface.as_ptr()
                                    } else {
                                        std::ptr::null_mut()
                                    }
                                })
                                .unwrap_or_else(|_| std::ptr::null_mut()),
                        );
                    }

                    backend.window().set_cursor_visible(cursor_visible);

                    let states = render_output_result.states;

                    update_primary_scanout_output(
                        &state.space,
                        &output,
                        &state.dnd_icon,
                        &state.cursor_status,
                        &states,
                    );

                    if has_rendered {
                        let frame_index = state.backend_data.capture_frame_index;
                        state.backend_data.capture_frame_index =
                            state.backend_data.capture_frame_index.wrapping_add(1);
                        if crate::frame_capture::is_capturing(&mut state) {
                            let output_name = output.name();
                            let mut offset = Duration::ZERO;
                            for (name, duration) in [
                                ("dispatch_clients", state.last_dispatch_duration),
                                ("build_and_draw", build_and_draw),
                                ("submit", submit),
                            ] {
                                crate::frame_capture::record_stage(
                                    &mut state,
                                    &output_name,
                                    frame_index,
                                    name,
                                    offset,
                                    duration,
                                );
                                offset += duration;
                            }
                        }
                        state.record_frame_stats(&output, Instant::now(), frame_index);
                        let mut output_presentation_feedback =
                            take_presentation_feedback(&output, &state.space, &states);
                        output_presentation_feedback.presented(
                            frame_target,
                            output
                                .current_mode()
                                .map(|mode| {
                                    Refresh::fixed(Duration::from_secs_f64(
                                        1_000f64 / mode.refresh as f64,
                                    ))
                                })
                                .unwrap_or(Refresh::Unknown),
                            0,
                            wp_presentation_feedback::Kind::Vsync,
                        )
                    } else {
                        state.record_skipped_frame(&output, Instant::now());
                    }

                    // Send frame events so that client start drawing their next frame
                    state.post_repaint(&output, frame_target, None, &states);
                }
                Err(SwapBuffersError::ContextLost(err)) => {
                    #[cfg(feature = "debug")]
                    if let Some(renderdoc) = renderdoc.as_mut() {
                        renderdoc.discard_frame_capture(
                            backend.renderer().egl_context().get_context_handle(),
                            backend
                                .window()
                                .window_handle()
                                .map(|handle| {
                                    if let RawWindowHandle::Wayland(handle) = handle.as_raw() {
                                        handle.surface.as_ptr()
                                    } else {
                                        std::ptr::null_mut()
                                    }
                                })
                                .unwrap_or_else(|_| std::ptr::null_mut()),
                        );
                    }

                    error!("Critical Rendering Error: {}", err);
                    state.running.store(false, Ordering::SeqCst);
                }
                Err(err) => warn!("Rendering error: {}", err),
            }
        }

        let result = event_loop.dispatch(Some(Duration::from_millis(1)), &mut state);
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

        #[cfg(feature = "debug")]
        state.backend_data.fps.tick();
    }
}
