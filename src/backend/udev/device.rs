use super::*;

impl AnvilState<UdevData> {
    pub(super) fn device_added(&mut self, node: DrmNode, path: &Path) -> Result<(), DeviceAddError> {
        // Try to open the device
        let fd = self
            .backend_data
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(DeviceAddError::DeviceOpen)?;

        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).map_err(DeviceAddError::DrmDevice)?;
        let gbm = GbmDevice::new(fd).map_err(DeviceAddError::GbmDevice)?;

        let registration_token = self
            .handle
            .insert_source(
                notifier,
                move |event, metadata, data: &mut AnvilState<_>| match event {
                    DrmEvent::VBlank(crtc) => {
                        profiling::scope!("vblank", &format!("{crtc:?}"));
                        data.frame_finish(node, crtc, metadata);
                    }
                    DrmEvent::Error(error) => {
                        error!("{:?}", error);
                    }
                },
            )
            .unwrap();

        let mut try_initialize_gpu = || {
            let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(DeviceAddError::AddNode)? };
            let egl_device =
                EGLDevice::device_for_display(&display).map_err(DeviceAddError::AddNode)?;

            if egl_device.is_software() {
                return Err(DeviceAddError::NoRenderNode);
            }

            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .flatten()
                .unwrap_or(node);
            self.backend_data
                .gpus
                .as_mut()
                .add_node(render_node, gbm.clone())
                .map_err(DeviceAddError::AddNode)?;

            std::result::Result::<DrmNode, DeviceAddError>::Ok(render_node)
        };

        let render_node = try_initialize_gpu()
            .inspect_err(|err| {
                warn!(?err, "failed to initialize gpu");
            })
            .ok();

        let allocator = render_node
            .is_some()
            .then(|| {
                GbmAllocator::new(
                    gbm.clone(),
                    GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
                )
            })
            .or_else(|| {
                self.backend_data
                    .backends
                    .get(&self.backend_data.primary_gpu)
                    .or_else(|| {
                        self.backend_data.backends.values().find(|backend| {
                            backend.render_node == Some(self.backend_data.primary_gpu)
                        })
                    })
                    .map(|backend| backend.drm_output_manager.allocator().clone())
            })
            .ok_or(DeviceAddError::PrimaryGpuMissing)?;

        let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

        let color_formats = if std::env::var("ANVIL_DISABLE_10BIT").is_ok() {
            SUPPORTED_FORMATS_8BIT_ONLY
        } else {
            SUPPORTED_FORMATS
        };
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node.unwrap_or(self.backend_data.primary_gpu))
            .unwrap();
        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| render_node.is_some() || format.modifier == Modifier::Linear)
            .copied()
            .collect::<FormatSet>();

        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            framebuffer_exporter,
            Some(gbm),
            color_formats.iter().copied(),
            render_formats,
        );

        self.backend_data.backends.insert(
            node,
            BackendData {
                registration_token,
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                non_desktop_connectors: Vec::new(),
                render_node,
                surfaces: HashMap::new(),
                leasing_global: DrmLeaseState::new::<AnvilState<UdevData>>(
                    &self.display_handle,
                    &node,
                )
                .inspect_err(|err| {
                    warn!(?err, "Failed to initialize drm lease global for: {}", node);
                })
                .ok(),
                active_leases: Vec::new(),
            },
        );

        self.device_changed(node);

        Ok(())
    }

    pub(super) fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        info!(?crtc, "Trying to setup connector {}", output_name,);

        let drm_device = device.drm_output_manager.device();

        let non_desktop = drm_device
            .get_properties(connector.handle())
            .ok()
            .and_then(|props| {
                let (info, value) = props
                    .into_iter()
                    .filter_map(|(handle, value)| {
                        let info = drm_device.get_property(handle).ok()?;

                        Some((info, value))
                    })
                    .find(|(info, _)| info.name().to_str() == Ok("non-desktop"))?;

                info.value_type().convert_value(value).as_boolean()
            })
            .unwrap_or(false);

        let display_info = display_info::for_connector(drm_device, connector.handle());

        let make = display_info
            .as_ref()
            .and_then(|info| info.make())
            .unwrap_or_else(|| "Unknown".into());

        let model = display_info
            .as_ref()
            .and_then(|info| info.model())
            .unwrap_or_else(|| "Unknown".into());

        let serial_number = display_info
            .as_ref()
            .and_then(|info| info.serial())
            .unwrap_or_else(|| "Unknown".into());

        if non_desktop {
            info!(
                "Connector {} is non-desktop, setting up for leasing",
                output_name
            );
            device
                .non_desktop_connectors
                .push((connector.handle(), crtc));
            if let Some(lease_state) = device.leasing_global.as_mut() {
                lease_state.add_connector::<AnvilState<UdevData>>(
                    connector.handle(),
                    output_name,
                    format!("{make} {model}"),
                );
            }
        } else {
            let preferred_mode_id = connector
                .modes()
                .iter()
                .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                .unwrap_or(0);

            let preferred_mode = connector.modes()[preferred_mode_id];
            let output_settings = self.config.output_settings(&output_name);
            let drm_mode = output_settings
                .refresh_rate
                .filter(|refresh| *refresh > 0)
                .and_then(|requested| {
                    connector
                        .modes()
                        .iter()
                        .filter(|mode| mode.size() == preferred_mode.size())
                        .min_by_key(|mode| (WlMode::from(**mode).refresh - requested).abs())
                        .copied()
                })
                .unwrap_or(preferred_mode);
            let wl_mode = WlMode::from(drm_mode);

            let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
            let output = Output::new(
                output_name,
                PhysicalProperties {
                    size: (phys_w as i32, phys_h as i32).into(),
                    subpixel: connector.subpixel().into(),
                    make,
                    model,
                    serial_number,
                },
            );
            for mode in connector.modes() {
                output.add_mode(WlMode::from(*mode));
            }
            let global = output.create_global::<AnvilState<UdevData>>(&self.display_handle);

            let placed: Vec<(String, Rectangle<i32, Logical>)> = self
                .space
                .outputs()
                .map(|o| (o.name(), self.space.output_geometry(o).unwrap()))
                .collect();
            let logical_size: Size<i32, Logical> =
                (drm_mode.size().0 as i32, drm_mode.size().1 as i32).into();
            let position = crate::keybindings::resolve_output_position(
                &output_settings,
                &output.name(),
                logical_size,
                &placed,
            );

            output.set_preferred(wl_mode);
            output.change_current_state(Some(wl_mode), None, None, Some(position));
            self.space.map_output(&output, position);
            crate::shell::workspace::init_output(&self.config, &self.space, &output);

            output.user_data().insert_if_missing(|| UdevOutputId {
                crtc,
                device_id: node,
            });

            #[cfg(feature = "debug")]
            let fps_element = self.backend_data.fps_texture.clone().map(FpsElement::new);

            let driver = match drm_device.get_driver() {
                Ok(driver) => driver,
                Err(err) => {
                    warn!("Failed to query drm driver: {}", err);
                    return;
                }
            };

            let mut planes = match drm_device.planes(&crtc) {
                Ok(planes) => planes,
                Err(err) => {
                    warn!("Failed to query crtc planes: {}", err);
                    return;
                }
            };

            // Using an overlay plane on a nvidia card breaks
            if driver
                .name()
                .to_string_lossy()
                .to_lowercase()
                .contains("nvidia")
                || driver
                    .description()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("nvidia")
            {
                planes.overlay = vec![];
            }

            let drm_output = match device
                .drm_output_manager
                .lock()
                .initialize_output::<_, OutputRenderElements<UdevRenderer<'_>, WindowRenderElement<UdevRenderer<'_>>>>(
                    crtc,
                    drm_mode,
                    &[connector.handle()],
                    &output,
                    Some(planes),
                    &mut renderer,
                    &DrmOutputRenderElements::default(),
                ) {
                Ok(drm_output) => drm_output,
                Err(err) => {
                    warn!("Failed to initialize drm output: {}", err);
                    return;
                }
            };

            let disable_direct_scanout = std::env::var("ANVIL_DISABLE_DIRECT_SCANOUT").is_ok();

            let dmabuf_feedback = drm_output.with_compositor(|compositor| {
                compositor.set_debug_flags(self.backend_data.debug_flags);

                get_surface_dmabuf_feedback(
                    self.backend_data.primary_gpu,
                    device.render_node,
                    node,
                    &mut self.backend_data.gpus,
                    compositor.surface(),
                )
            });

            let surface = SurfaceData {
                dh: self.display_handle.clone(),
                device_id: node,
                render_node: device.render_node,
                output,
                global: Some(global),
                drm_output,
                available_modes: connector.modes().to_vec(),
                preferred_mode,
                disable_direct_scanout,
                #[cfg(feature = "debug")]
                fps: fps_ticker::Fps::default(),
                #[cfg(feature = "debug")]
                fps_element,
                dmabuf_feedback,
                last_presentation_time: None,
                vblank_throttle_timer: None,
                pending_vblank_at: None,
                capture_frame_index: 0,
                gamma_kelvin: None,
                gamma_dirty: false,
                gamma_unsupported: false,
            };

            device.surfaces.insert(crtc, surface);

            self.reorder_primary_output();

            // kick-off rendering
            self.handle.insert_idle(move |state| {
                state.render_surface(node, crtc, state.clock.now());
            });
        }
    }

    /// Ensures the output marked `primary = true` in the config (if any and
    /// if connected) is the first one returned by `space.outputs()`, which
    /// several call sites use as their "no output under pointer" fallback.
    /// Positions are preserved; only iteration order changes.
    pub(super) fn reorder_primary_output(&mut self) {
        let Some(primary_name) = self.config.primary_output_name().map(str::to_string) else {
            return;
        };

        if self.space.outputs().next().map(|o| o.name()) == Some(primary_name.clone()) {
            return;
        }

        let outputs_with_locations: Vec<(Output, Point<i32, Logical>)> = self
            .space
            .outputs()
            .map(|o| (o.clone(), self.space.output_geometry(o).unwrap().loc))
            .collect();

        if !outputs_with_locations
            .iter()
            .any(|(o, _)| o.name() == primary_name)
        {
            return;
        }

        for (output, _) in &outputs_with_locations {
            self.space.unmap_output(output);
        }
        for (output, location) in &outputs_with_locations {
            if output.name() == primary_name {
                self.space.map_output(output, *location);
            }
        }
        for (output, location) in &outputs_with_locations {
            if output.name() != primary_name {
                self.space.map_output(output, *location);
            }
        }
    }

    pub(super) fn connector_disconnected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        if let Some(pos) = device
            .non_desktop_connectors
            .iter()
            .position(|(handle, _)| *handle == connector.handle())
        {
            let _ = device.non_desktop_connectors.remove(pos);
            if let Some(leasing_state) = device.leasing_global.as_mut() {
                leasing_state.withdraw_connector(connector.handle());
            }
        } else if let Some(surface) = device.surfaces.remove(&crtc) {
            self.space.unmap_output(&surface.output);
            self.space.refresh();
        }

        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();
        let _ = device.drm_output_manager.lock().try_to_restore_modifiers::<_, OutputRenderElements<
            UdevRenderer<'_>,
            WindowRenderElement<UdevRenderer<'_>>,
        >>(
            &mut renderer,
            // FIXME: For a flicker free operation we should return the actual elements for this output..
            // Instead we just use black to "simulate" a modeset :)
            &DrmOutputRenderElements::default(),
        );
    }

    pub(super) fn device_changed(&mut self, node: DrmNode) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let scan_result = match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(scan_result) => scan_result,
            Err(err) => {
                tracing::warn!(?err, "Failed to scan connectors");
                return;
            }
        };

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_connected(node, connector, crtc);
                }
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(node, connector, crtc);
                }
                // The connector's mode list changed while it stayed connected (e.g. EDID
                // arrived after the initial probe returned empty/fallback modes). Compositors
                // should re-evaluate the output's mode selection and recreate the surface here.
                DrmScanEvent::Changed { .. } => {}
                _ => {}
            }
        }

        // fixup window coordinates
        crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
        crate::ext_workspace::ext_workspace_sync(self);
    }

    pub(super) fn device_removed(&mut self, node: DrmNode) {
        let device = if let Some(device) = self.backend_data.backends.get_mut(&node) {
            device
        } else {
            return;
        };

        let crtcs: Vec<_> = device
            .drm_scanner
            .crtcs()
            .map(|(info, crtc)| (info.clone(), crtc))
            .collect();

        for (connector, crtc) in crtcs {
            self.connector_disconnected(node, connector, crtc);
        }
        crate::ext_workspace::ext_workspace_sync(self);

        debug!("Surfaces dropped");

        // drop the backends on this side
        if let Some(mut backend_data) = self.backend_data.backends.remove(&node) {
            if let Some(mut leasing_global) = backend_data.leasing_global.take() {
                leasing_global.disable_global::<AnvilState<UdevData>>();
            }

            if let Some(render_node) = backend_data.render_node {
                self.backend_data.gpus.as_mut().remove_node(&render_node);
            }

            self.handle.remove(backend_data.registration_token);

            debug!("Dropping device");
        }

        crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
    }
}
