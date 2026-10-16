//! Server side of the `ironland-display-filter-v1` protocol (see
//! `crate::ironland_protocols::display_filter` for the generated bindings
//! and `protocols/ironland-display-filter-v1.xml` for the wire format).
//!
//! A client (the shell's night mode) asks for a color temperature; the
//! renderer reads it back through [`DisplayFilterState::kelvin`] each frame
//! and applies it as a CRTC gamma ramp on the udev backend or a screen-wide
//! tint overlay otherwise (see `crate::tint`). The filter lives
//! exactly as long as the protocol object that requested it, so a crashed
//! client can't leave the screen stuck orange.

use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    backend::{ClientId, GlobalId},
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::ironland_protocols::display_filter::ironland_display_filter_v1::{
    self, IronlandDisplayFilterV1,
};

/// Color temperature at (and above) which no tint is applied.
pub const NEUTRAL_KELVIN: u32 = 6500;
/// Warmest temperature a client can request; lower values are clamped.
pub const MIN_KELVIN: u32 = 1000;

/// Implemented by the compositor state so this module can store filter
/// requests without depending on `AnvilState` directly.
pub trait DisplayFilterHandler: 'static {
    fn display_filter_state(&mut self) -> &mut DisplayFilterState;
}

#[derive(Debug)]
struct FilterEntry {
    resource: IronlandDisplayFilterV1,
    /// `None` until the object's first `set_color_temperature`, so merely
    /// binding the global never overrides another client's filter.
    request: Option<Request>,
}

#[derive(Debug, Clone, Copy)]
struct Request {
    kelvin: u32,
    /// Request order, so the most recent `set_color_temperature` wins when
    /// several live objects disagree.
    serial: u64,
}

#[derive(Debug)]
pub struct DisplayFilterState {
    global: GlobalId,
    entries: Vec<FilterEntry>,
    next_serial: u64,
}

impl DisplayFilterState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<IronlandDisplayFilterV1, GlobalData> + 'static,
    {
        let global = dh.create_global::<D, IronlandDisplayFilterV1, _>(1, GlobalData);
        DisplayFilterState {
            global,
            entries: Vec::new(),
            next_serial: 0,
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// The color temperature to render with, or `None` when no live client
    /// wants a filter (nothing requested, or the latest request was neutral).
    /// Destroying the object that made the latest request falls back to the
    /// latest request of the remaining live objects.
    pub fn kelvin(&self) -> Option<u32> {
        self.entries
            .iter()
            .filter_map(|e| e.request)
            .max_by_key(|r| r.serial)
            .map(|r| r.kelvin)
            .filter(|&k| k < NEUTRAL_KELVIN)
    }
}

/// Normalizes a requested temperature: 0 and anything above neutral mean
/// "no filter", anything below [`MIN_KELVIN`] is clamped up to it.
fn clamp_kelvin(requested: u32) -> u32 {
    match requested {
        0 => NEUTRAL_KELVIN,
        k => k.clamp(MIN_KELVIN, NEUTRAL_KELVIN),
    }
}

/// Global data for the `ironland_display_filter_v1` global (nothing to
/// carry - also reused as the per-object user data, since each object's
/// state lives in [`DisplayFilterState::entries`]).
#[derive(Debug)]
pub struct GlobalData;

impl<D> GlobalDispatch2<IronlandDisplayFilterV1, D> for GlobalData
where
    D: DisplayFilterHandler + Dispatch<IronlandDisplayFilterV1, GlobalData>,
{
    fn bind(
        &self,
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<IronlandDisplayFilterV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let resource = data_init.init(resource, GlobalData);
        state.display_filter_state().entries.push(FilterEntry {
            resource,
            request: None,
        });
    }
}

impl<D: DisplayFilterHandler> Dispatch2<IronlandDisplayFilterV1, D> for GlobalData {
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &IronlandDisplayFilterV1,
        request: ironland_display_filter_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ironland_display_filter_v1::Request::Destroy => {}
            ironland_display_filter_v1::Request::SetColorTemperature { kelvin } => {
                let filter = state.display_filter_state();
                let serial = filter.next_serial;
                filter.next_serial += 1;
                if let Some(entry) = filter.entries.iter_mut().find(|e| &e.resource == resource) {
                    entry.request = Some(Request {
                        kelvin: clamp_kelvin(kelvin),
                        serial,
                    });
                }
            }
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &IronlandDisplayFilterV1) {
        state
            .display_filter_state()
            .entries
            .retain(|e| &e.resource != resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_requests() {
        assert_eq!(clamp_kelvin(0), NEUTRAL_KELVIN);
        assert_eq!(clamp_kelvin(9000), NEUTRAL_KELVIN);
        assert_eq!(clamp_kelvin(500), MIN_KELVIN);
        assert_eq!(clamp_kelvin(3400), 3400);
    }
}
