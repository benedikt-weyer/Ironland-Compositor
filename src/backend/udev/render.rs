use super::*;

impl AnvilState<UdevData> {
    pub(super) fn frame_finish(
        &mut self,
        dev_id: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        profiling::scope!("frame_finish", &format!("{crtc:?}"));

        // Only timestamped while a `crate::frame_capture` session is open -
        // an `Instant::now()` here isn't free enough to want on every
        // vblank forever, just cheap enough to afford while diagnosing.
        // Computed up front, before `device_backend`/`surface` below are
        // borrowed, since a call needing the whole `&mut self` can't be
        // interleaved with a live borrow of one of its fields.
        let vblank_observed_at = crate::frame_capture::is_capturing(self).then(Instant::now);

        let device_backend = match self.backend_data.backends.get_mut(&dev_id) {
            Some(backend) => backend,
            None => {
                error!("Trying to finish frame on non-existent backend {}", dev_id);
                return;
            }
        };

        let surface = match device_backend.surfaces.get_mut(&crtc) {
            Some(surface) => surface,
            None => {
                error!("Trying to finish frame on non-existent crtc {:?}", crtc);
                return;
            }
        };

        if let Some(timer_token) = surface.vblank_throttle_timer.take() {
            self.handle.remove(timer_token);
        }

        let output = if let Some(output) = self.space.outputs().find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: surface.device_id,
                    crtc,
                })
        }) {
            output.clone()
        } else {
            // somehow we got called with an invalid output
            return;
        };

        let Some(frame_duration) = output
            .current_mode()
            .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
        else {
            return;
        };

        let tp = metadata.as_ref().and_then(|metadata| match metadata.time {
            smithay::backend::drm::DrmEventTime::Monotonic(tp) => tp.is_zero().not().then_some(tp),
            smithay::backend::drm::DrmEventTime::Realtime(_) => None,
        });

        let seq = metadata
            .as_ref()
            .map(|metadata| metadata.sequence)
            .unwrap_or(0);

        let (clock, flags) = if let Some(tp) = tp {
            (
                tp.into(),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            )
        } else {
            (self.clock.now(), wp_presentation_feedback::Kind::Vsync)
        };

        let vblank_remaining_time = surface
            .last_presentation_time
            .map(|last_presentation_time| {
                frame_duration.saturating_sub(Time::elapsed(&last_presentation_time, clock))
            });

        if let Some(vblank_remaining_time) = vblank_remaining_time
            && vblank_remaining_time > frame_duration / 2 {
                static WARN_ONCE: Once = Once::new();
                WARN_ONCE.call_once(|| {
                    warn!("display running faster than expected, throttling vblanks and disabling HwClock")
                });
                let capture_frame_index = surface.capture_frame_index;
                let throttled_time = tp
                    .map(|tp| tp.saturating_add(vblank_remaining_time))
                    .unwrap_or(Duration::ZERO);
                let throttled_metadata = DrmEventMetadata {
                    sequence: seq,
                    time: DrmEventTime::Monotonic(throttled_time),
                };
                let timer_token = self
                    .handle
                    .insert_source(
                        Timer::from_duration(vblank_remaining_time),
                        move |_, _, data| {
                            data.frame_finish(dev_id, crtc, &mut Some(throttled_metadata));
                            TimeoutAction::Drop
                        },
                    )
                    .expect("failed to register vblank throttle timer");
                surface.vblank_throttle_timer = Some(timer_token);
                // `WARN_ONCE` above means this branch has no per-occurrence
                // signal otherwise - if it's being taken every frame (not
                // just the first time), this marker is the only way to see
                // that from a capture. `surface`'s last use is the
                // assignment just above, so it's safe to hand `self` to a
                // free function here.
                crate::frame_capture::record_stage(
                    self,
                    &output.name(),
                    capture_frame_index,
                    "vblank_throttled",
                    Duration::ZERO,
                    Duration::ZERO,
                );
                return;
            }
        surface.last_presentation_time = Some(clock);

        let submit_result = surface
            .drm_output
            .frame_submitted()
            .map_err(Into::<SwapBuffersError>::into);

        let schedule_render = match submit_result {
            Ok(user_data) => {
                if let Some(mut feedback) = user_data.flatten() {
                    feedback.presented(clock, Refresh::fixed(frame_duration), seq as u64, flags);
                }

                true
            }
            Err(err) => {
                warn!("Error during rendering: {:?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => true,
                    // If the device has been deactivated do not reschedule, this will be done
                    // by session resume
                    SwapBuffersError::TemporaryFailure(err)
                        if matches!(
                            err.downcast_ref::<DrmError>(),
                            Some(&DrmError::DeviceInactive)
                        ) =>
                    {
                        false
                    }
                    SwapBuffersError::TemporaryFailure(err) => matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(DrmError::Access(DrmAccessError {
                            source,
                            ..
                        })) if source.kind() == io::ErrorKind::PermissionDenied
                    ),
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {err}"),
                }
            }
        };

        if schedule_render {
            let next_frame_target = clock + frame_duration;
            surface.pending_vblank_at = vblank_observed_at;

            // What are we trying to solve by introducing a delay here:
            //
            // Basically it is all about latency of client provided buffers.
            // A client driven by frame callbacks will wait for a frame callback
            // to repaint and submit a new buffer. As we send frame callbacks
            // as part of the repaint in the compositor the latency would always
            // be approx. 2 frames. By introducing a delay before we repaint in
            // the compositor we can reduce the latency to approx. 1 frame + the
            // remaining duration from the repaint to the next VBlank.
            //
            // With the delay it is also possible to further reduce latency if
            // the client is driven by presentation feedback. As the presentation
            // feedback is directly sent after a VBlank the client can submit a
            // new buffer during the repaint delay that can hit the very next
            // VBlank, thus reducing the potential latency to below one frame.
            //
            // Choosing a good delay is a topic on its own so we just implement
            // a simple strategy here. We just split the duration between two
            // VBlanks into two steps, one for the client repaint and one for the
            // compositor repaint. Theoretically the repaint in the compositor should
            // be faster so we give the client a bit more time to repaint. On a typical
            // modern system the repaint in the compositor should not take more than 2ms
            // so this should be safe for refresh rates up to at least 120 Hz. For 120 Hz
            // this results in approx. 3.33ms time for repainting in the compositor.
            // A too big delay could result in missing the next VBlank in the compositor.
            //
            // A more complete solution could work on a sliding window analyzing past repaints
            // and do some prediction for the next repaint.
            let repaint_delay = Duration::from_secs_f64(frame_duration.as_secs_f64() * 0.6f64);

            let timer = if surface
                .render_node
                .map(|render_node| render_node != self.backend_data.primary_gpu)
                .unwrap_or(true)
            {
                // However, if we need to do a copy, that might not be enough.
                // (And without actual comparison to previous frames we cannot really know.)
                // So lets ignore that in those cases to avoid thrashing performance.
                trace!("scheduling repaint timer immediately on {:?}", crtc);
                Timer::immediate()
            } else {
                trace!(
                    "scheduling repaint timer with delay {:?} on {:?}",
                    repaint_delay, crtc
                );
                Timer::from_duration(repaint_delay)
            };

            self.handle
                .insert_source(timer, move |_, _, data| {
                    data.render(dev_id, Some(crtc), next_frame_target);
                    TimeoutAction::Drop
                })
                .expect("failed to schedule frame timer");
        }
    }

    // If crtc is `Some()`, render it, else render all crtcs
    pub(super) fn render(&mut self, node: DrmNode, crtc: Option<crtc::Handle>, frame_target: Time<Monotonic>) {
        let device_backend = match self.backend_data.backends.get_mut(&node) {
            Some(backend) => backend,
            None => {
                error!("Trying to render on non-existent backend {}", node);
                return;
            }
        };

        if let Some(crtc) = crtc {
            self.render_surface(node, crtc, frame_target);
        } else {
            let crtcs: Vec<_> = device_backend.surfaces.keys().copied().collect();
            for crtc in crtcs {
                self.render_surface(node, crtc, frame_target);
            }
        };
    }

    pub(super) fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle, frame_target: Time<Monotonic>) {
        profiling::scope!("render_surface", &format!("{crtc:?}"));

        let output = if let Some(output) = self.space.outputs().find(|o| {
            o.user_data().get::<UdevOutputId>()
                == Some(&UdevOutputId {
                    device_id: node,
                    crtc,
                })
        }) {
            output.clone()
        } else {
            // somehow we got called with an invalid output
            return;
        };

        self.pre_repaint(&output, frame_target);

        // Completed (or failed) from the frame this call renders - see
        // `capture_udev_frame`, called from the free `render_surface` below
        // once it has something to read pixels back from.
        let pending_captures = self.screencopy.take_pending(&output);
        let presented: Duration = frame_target.into();

        let focused_window_rect = crate::shell::tiling::current_focused_window(self)
            .and_then(|w| self.space.element_bbox(&w));
        let drop_indicator = self.tiling_drop_indicator;

        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let surface = if let Some(surface) = device.surfaces.get_mut(&crtc) {
            surface
        } else {
            return;
        };

        let start = Instant::now();

        // TODO get scale from the rendersurface when supporting HiDPI
        let frame = self
            .backend_data
            .pointer_image
            .get_image(1 /*scale*/, self.clock.now().into());

        let primary_gpu = self.backend_data.primary_gpu;
        let render_node = surface.render_node.unwrap_or(primary_gpu);
        let mut renderer = if primary_gpu == render_node {
            self.backend_data.gpus.single_renderer(&render_node)
        } else {
            let format = surface.drm_output.format();
            self.backend_data
                .gpus
                .renderer(&primary_gpu, &render_node, format)
        }
        .unwrap();

        let pointer_images = &mut self.backend_data.pointer_images;
        let pointer_image = pointer_images
            .iter()
            .find_map(|(image, texture)| {
                if image == &frame {
                    Some(texture.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                let buffer = MemoryRenderBuffer::from_slice(
                    &frame.pixels_rgba,
                    Fourcc::Argb8888,
                    (frame.width as i32, frame.height as i32),
                    1,
                    Transform::Normal,
                    None,
                );
                pointer_images.push((frame, buffer.clone()));
                buffer
            });

        let workspace_overlay_shown = self.workspace_overlay_shown.filter(|shown_at| {
            shown_at.elapsed().as_millis() < crate::shell::workspace::OVERLAY_DURATION_MS as u128
        });
        if workspace_overlay_shown.is_none() {
            self.workspace_overlay_shown = None;
        }

        let perf_stats = self.perf_stats.entry(output.name()).or_default();
        let fps_overlay_cache = self.fps_overlay_cache.entry(output.name()).or_default();
        let border_cache = self.border_cache.entry(output.name()).or_default();
        let drop_indicator_cache = self.drop_indicator_cache.entry(output.name()).or_default();
        let capture_frame_index = surface.capture_frame_index;
        let since_vblank = surface.pending_vblank_at.take().map(|at| at.elapsed());
        let result = render_surface(
            surface,
            &mut renderer,
            &self.space,
            &output,
            self.pointer.current_location(),
            &pointer_image,
            &mut self.backend_data.pointer_element,
            &self.dnd_icon,
            &mut self.cursor_status,
            self.show_window_preview,
            &mut self.launcher,
            &mut self.permission_prompt,
            workspace_overlay_shown.is_some(),
            &mut self.wallpaper,
            &self.config.blur,
            &self.config.corners,
            focused_window_rect,
            &self.config.border,
            border_cache,
            drop_indicator,
            drop_indicator_cache,
            &self.config.performance,
            &*perf_stats,
            fps_overlay_cache,
            &mut self.key_tracker,
            pending_captures,
            presented,
        );
        let reschedule = match result {
            Ok((has_rendered, states, timings)) => {
                let dmabuf_feedback = surface.dmabuf_feedback.clone();
                if has_rendered {
                    surface.capture_frame_index = surface.capture_frame_index.wrapping_add(1);
                }
                self.post_repaint(&output, frame_target, dmabuf_feedback, &states);
                if has_rendered {
                    if crate::frame_capture::is_capturing(self) {
                        let output_name = output.name();
                        let stages: [(&str, Duration); 4] = [
                            ("dispatch_clients", self.last_dispatch_duration),
                            ("build_elements", timings.build_elements),
                            ("damage_and_draw", timings.damage_and_draw),
                            ("submit", timings.submit),
                        ];
                        let mut offset = Duration::ZERO;
                        if let Some(since_vblank) = since_vblank {
                            crate::frame_capture::record_stage(
                                self,
                                &output_name,
                                capture_frame_index,
                                "since_vblank",
                                offset,
                                since_vblank,
                            );
                            offset += since_vblank;
                        }
                        for (name, duration) in stages {
                            crate::frame_capture::record_stage(
                                self,
                                &output_name,
                                capture_frame_index,
                                name,
                                offset,
                                duration,
                            );
                            offset += duration;
                        }
                    }
                    self.record_frame_stats(&output, Instant::now(), capture_frame_index);
                } else {
                    self.record_skipped_frame(&output, Instant::now());
                }
                !has_rendered
            }
            Err(err) => {
                warn!("Error during rendering: {:#?}", err);
                match err {
                    SwapBuffersError::AlreadySwapped => false,
                    SwapBuffersError::TemporaryFailure(err) => match err.downcast_ref::<DrmError>()
                    {
                        Some(DrmError::DeviceInactive) => true,
                        Some(DrmError::Access(DrmAccessError { source, .. })) => {
                            source.kind() == io::ErrorKind::PermissionDenied
                        }
                        _ => false,
                    },
                    SwapBuffersError::ContextLost(err) => match err.downcast_ref::<DrmError>() {
                        Some(DrmError::TestFailed(_)) => {
                            // reset the complete state, disabling all connectors and planes in case we hit a test failed
                            // most likely we hit this after a tty switch when a foreign master changed CRTC <-> connector bindings
                            // and we run in a mismatch
                            device
                                .drm_output_manager
                                .device_mut()
                                .reset_state()
                                .expect("failed to reset drm device");
                            true
                        }
                        _ => panic!("Rendering loop lost: {err}"),
                    },
                }
            }
        };

        if reschedule {
            let output_refresh = match output.current_mode() {
                Some(mode) => mode.refresh,
                None => return,
            };

            // If reschedule is true we either hit a temporary failure or more likely rendering
            // did not cause any damage on the output. In this case we just re-schedule a repaint
            // after approx. one frame to re-test for damage.
            let next_frame_target =
                frame_target + Duration::from_millis(1_000_000 / output_refresh as u64);
            let reschedule_timeout =
                Duration::from(next_frame_target).saturating_sub(self.clock.now().into());
            trace!(
                "reschedule repaint timer with delay {:?} on {:?}",
                reschedule_timeout, crtc,
            );
            let timer = Timer::from_duration(reschedule_timeout);
            self.handle
                .insert_source(timer, move |_, _, data| {
                    data.render(node, Some(crtc), next_frame_target);
                    TimeoutAction::Drop
                })
                .expect("failed to schedule frame timer");
        } else {
            let elapsed = start.elapsed();
            tracing::trace!(?elapsed, "rendered surface");
        }

        profiling::finish_frame!();
    }
}

/// How long each phase of one [`render_surface`] call took - always
/// measured (an `Instant::now()` pair per phase is cheap enough not to
/// bother gating), but only turned into `crate::frame_capture` `stage`
/// events by the caller while a capture session is actually open.
#[derive(Debug, Clone, Copy)]
struct RenderTimings {
    /// Cursor/overlay assembly plus `render::output_elements` - building
    /// the render element list, not yet drawing anything.
    build_elements: Duration,
    /// `DrmOutput::render_frame` - the actual damage-tracked composite.
    damage_and_draw: Duration,
    /// `DrmOutput::queue_frame` - the DRM atomic commit/page-flip submit.
    /// `Duration::ZERO` when nothing was rendered (queue_frame is skipped).
    submit: Duration,
}

#[allow(clippy::too_many_arguments)]
#[profiling::function]
pub(super) fn render_surface<'a>(
    surface: &'a mut SurfaceData,
    renderer: &mut UdevRenderer<'a>,
    space: &Space<WindowElement>,
    output: &Output,
    pointer_location: Point<f64, Logical>,
    pointer_image: &MemoryRenderBuffer,
    pointer_element: &mut PointerElement,
    dnd_icon: &Option<DndIcon>,
    cursor_status: &mut CursorImageStatus,
    show_window_preview: bool,
    launcher: &mut LauncherState,
    permission_prompt: &mut crate::permission_prompt::PermissionPromptManagerState,
    show_workspace_overlay: bool,
    wallpaper: &mut crate::wallpaper::Wallpaper,
    blur: &crate::config::BlurSettings,
    corners: &crate::config::CornersSettings,
    focused_window_rect: Option<Rectangle<i32, Logical>>,
    border: &crate::config::BorderSettings,
    border_cache: &mut crate::border::BorderCache,
    drop_indicator: Option<Rectangle<i32, Logical>>,
    drop_indicator_cache: &mut crate::border::BorderCache,
    performance: &crate::config::PerformanceSettings,
    perf_stats: &crate::perf_overlay::FrameStats,
    fps_overlay_cache: &mut crate::perf_overlay::OverlayCache,
    key_tracker: &mut crate::key_tracker::KeyTracker,
    pending_captures: Vec<smithay::wayland::image_copy_capture::Frame>,
    presented: Duration,
) -> Result<(bool, RenderElementStates, RenderTimings), SwapBuffersError> {
    let build_start = Instant::now();
    let output_geometry = space.output_geometry(output).unwrap();
    let scale = Scale::from(output.current_scale().fractional_scale());

    crate::rounded_corners::set_current(crate::rounded_corners::CornersConfig {
        enabled: corners.enabled,
        radius: corners.radius as f32,
    });

    let mut custom_elements: Vec<CustomRenderElements<_>> = Vec::new();

    if output_geometry.to_f64().contains(pointer_location) {
        let cursor_hotspot = if let CursorImageStatus::Surface(surface) = cursor_status {
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
        let cursor_pos = pointer_location - output_geometry.loc.to_f64();

        // set cursor
        pointer_element.set_buffer(pointer_image.clone());

        // draw the cursor as relevant
        {
            // reset the cursor if the surface is no longer alive
            let mut reset = false;
            if let CursorImageStatus::Surface(ref surface) = *cursor_status {
                reset = !surface.alive();
            }
            if reset {
                *cursor_status = CursorImageStatus::default_named();
            }

            pointer_element.set_status(cursor_status.clone());
        }

        custom_elements.extend(
            pointer_element.render_elements(
                renderer,
                (cursor_pos - cursor_hotspot.to_f64())
                    .to_physical(scale)
                    .to_i32_round(),
                scale,
                1.0,
            ),
        );

        // draw the dnd icon if applicable
        {
            if let Some(icon) = dnd_icon.as_ref() {
                let dnd_icon_pos = (cursor_pos + icon.offset.to_f64())
                    .to_physical(scale)
                    .to_i32_round();
                if icon.surface.alive() {
                    custom_elements.extend(AsRenderElements::<UdevRenderer<'a>>::render_elements(
                        &SurfaceTree::from_surface(&icon.surface),
                        renderer,
                        dnd_icon_pos,
                        scale,
                        1.0,
                    ));
                }
            }
        }
    }

    // The permission prompt (see `crate::permission_prompt`) is pushed
    // right after the pointer/dnd icon, ahead of every other overlay -
    // it's the one thing on screen a user must never be able to
    // accidentally cover, since answering it wrong grants or denies a
    // real capability.
    let permission_prompt_location = permission_prompt
        .origin_in(output_geometry.size)
        .to_f64()
        .to_physical(scale);
    if let Some(prompt_buffer) = permission_prompt.ensure_buffer() {
        let location = permission_prompt_location;
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            prompt_buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            custom_elements.push(CustomRenderElements::Overlay(element));
        }
    }

    #[cfg(feature = "debug")]
    if let Some(element) = surface.fps_element.as_mut() {
        element.update_fps(surface.fps.avg().round() as u32);
        surface.fps.tick();
        custom_elements.push(CustomRenderElements::Fps(element.clone()));
    }

    if let Some((buffer, location)) = key_tracker.overlay(output_geometry.size)
        && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location.to_f64().to_physical(scale),
            buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        )
    {
        custom_elements.push(CustomRenderElements::Overlay(element));
    }

    if performance.fps_overlay {
        let overlay_buffer = fps_overlay_cache.buffer(
            perf_stats,
            Duration::from_millis(performance.fps_overlay_interval_ms.into()),
        );
        let location = crate::perf_overlay::overlay_location(
            performance.fps_overlay_position,
            output_geometry.size,
        )
        .to_f64()
        .to_physical(scale);
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            overlay_buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            custom_elements.push(CustomRenderElements::Overlay(element));
        }
    }

    let launcher_size = launcher.logical_size();
    if let Some(launcher_buffer) = launcher.ensure_buffer() {
        let location = Point::<i32, Logical>::from((
            (output_geometry.size.w - launcher_size.w) / 2,
            (output_geometry.size.h - launcher_size.h) / 2,
        ))
        .to_f64()
        .to_physical(scale);
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            launcher_buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            custom_elements.push(CustomRenderElements::Overlay(element));
        }
    }

    if show_workspace_overlay {
        let (active, count) = crate::shell::workspace::overlay_info(output);
        let overlay_buffer = crate::drawing::workspace_overlay_buffer(active, count);
        let overlay_size = crate::drawing::workspace_overlay_size(count);
        let location = Point::<i32, Logical>::from((
            (output_geometry.size.w - overlay_size.w) / 2,
            output_geometry.size.h - overlay_size.h - 48,
        ))
        .to_f64()
        .to_physical(scale);
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &overlay_buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            custom_elements.push(CustomRenderElements::Overlay(element));
        }
    }

    let wallpaper_buffer = wallpaper.buffer_for(output_geometry.size).clone();
    let blurred_wallpaper_buffer = blur.enabled.then(|| {
        wallpaper
            .blurred_buffer_for(output_geometry.size, blur.radius)
            .clone()
    });
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

    let corner_radius = if corners.enabled { corners.radius as f32 } else { 0.0 };
    let (elements, clear_color) = output_elements(
        output,
        space,
        custom_elements,
        background_element,
        blurred_wallpaper_buffer.as_ref(),
        renderer,
        show_window_preview,
        focused_window_rect,
        border,
        corner_radius,
        border_cache,
        drop_indicator,
        drop_indicator_cache,
    );
    let build_elements = build_start.elapsed();

    let frame_mode = if surface.disable_direct_scanout {
        FrameFlags::empty()
    } else {
        FrameFlags::DEFAULT
    };
    let damage_start = Instant::now();
    let render_frame_result = surface
        .drm_output
        .render_frame(renderer, &elements, clear_color, frame_mode)
        .map_err(|err| match err {
            smithay::backend::drm::compositor::RenderFrameError::PrepareFrame(err) => {
                SwapBuffersError::from(err)
            }
            smithay::backend::drm::compositor::RenderFrameError::RenderFrame(
                OutputDamageTrackerError::Rendering(err),
            ) => SwapBuffersError::from(err),
            _ => unreachable!(),
        })?;
    let damage_and_draw = damage_start.elapsed();

    #[cfg(feature = "renderer_sync")]
    if let PrimaryPlaneElement::Swapchain(element) = render_frame_result.primary_element {
        element.sync.wait();
    }

    if !pending_captures.is_empty() {
        if render_frame_result.is_empty {
            // Nothing changed since the last frame, so there's nothing new
            // to read back - complete the request from whatever's already
            // on screen instead of waiting on damage that may never come.
            for frame in pending_captures {
                frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Unknown);
            }
        } else {
            capture_udev_frame(&render_frame_result, renderer, output, pending_captures, presented);
        }
    }

    let (rendered, states) = (!render_frame_result.is_empty, render_frame_result.states);

    update_primary_scanout_output(space, output, dnd_icon, cursor_status, &states);

    let mut submit = Duration::ZERO;
    if rendered {
        let output_presentation_feedback = take_presentation_feedback(output, space, &states);
        let submit_start = Instant::now();
        surface
            .drm_output
            .queue_frame(Some(output_presentation_feedback))
            .map_err(Into::<SwapBuffersError>::into)?;
        submit = submit_start.elapsed();
    }

    Ok((
        rendered,
        states,
        RenderTimings {
            build_elements,
            damage_and_draw,
            submit,
        },
    ))
}

/// Completes `pending` by reading back the frame `render_frame_result`
/// (from `surface.drm_output.render_frame`, called immediately before this)
/// just produced.
///
/// The DRM/KMS compositor may have scanned a client's buffer out directly
/// (direct scanout) rather than compositing into a readable framebuffer, so
/// there isn't always one to read pixels back from directly - instead this
/// composites the same result again into an offscreen renderbuffer via
/// [`RenderFrameResult::blit_frame_result`] (blitting the direct-scanout
/// plane's dmabuf in, same as every other composited element), then reads
/// that back the same way `crate::screencopy::fulfill` does for the winit
/// backend.
pub(super) fn capture_udev_frame<'a, B, F, E>(
    render_frame_result: &smithay::backend::drm::compositor::RenderFrameResult<'_, B, F, E>,
    renderer: &mut UdevRenderer<'a>,
    output: &Output,
    pending: Vec<smithay::wayland::image_copy_capture::Frame>,
    presented: Duration,
) where
    B: smithay::backend::allocator::Buffer + smithay::backend::allocator::dmabuf::AsDmabuf,
    <B as smithay::backend::allocator::dmabuf::AsDmabuf>::Error: std::fmt::Debug,
    F: smithay::backend::drm::Framebuffer,
    E: smithay::backend::renderer::element::Element + smithay::backend::renderer::element::RenderElement<UdevRenderer<'a>>,
{
    use smithay::backend::renderer::gles::GlesRenderbuffer;
    use smithay::backend::renderer::{Bind, Offscreen};
    use smithay::wayland::image_copy_capture::CaptureFailureReason;

    fn fail_all(pending: Vec<smithay::wayland::image_copy_capture::Frame>) {
        for frame in pending {
            frame.fail(CaptureFailureReason::Unknown);
        }
    }

    let Some(size) = crate::screencopy::output_buffer_size(output) else {
        fail_all(pending);
        return;
    };

    let mut offscreen: GlesRenderbuffer = match renderer.create_buffer(Fourcc::Argb8888, size) {
        Ok(buffer) => buffer,
        Err(err) => {
            tracing::warn!(?err, "screencopy: failed to allocate offscreen capture buffer");
            fail_all(pending);
            return;
        }
    };
    let mut fb = match renderer.bind(&mut offscreen) {
        Ok(fb) => fb,
        Err(err) => {
            tracing::warn!(?err, "screencopy: failed to bind offscreen capture buffer");
            fail_all(pending);
            return;
        }
    };

    let physical_size: Size<i32, Physical> = (size.w, size.h).into();
    let damage = Rectangle::from_size(physical_size);
    if let Err(err) = render_frame_result.blit_frame_result(
        physical_size,
        output.current_transform(),
        output.current_scale().fractional_scale(),
        renderer,
        &mut fb,
        [damage],
        [],
    ) {
        tracing::warn!(?err, "screencopy: failed to composite frame for capture");
        fail_all(pending);
        return;
    }

    crate::screencopy::fulfill(pending, renderer, &fb, size, presented);
}

