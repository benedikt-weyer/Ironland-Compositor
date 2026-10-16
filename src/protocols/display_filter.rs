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

use std::time::{Duration, Instant};

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
/// How long a change of the requested temperature takes to fade in or out.
pub const TRANSITION: Duration = Duration::from_secs(3);

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
    fade: Fade,
}

/// A linear fade between two color temperatures, interpolated in mired
/// (10^6 / K) so equal steps look equally warm (see `crate::tint`).
#[derive(Debug)]
struct Fade {
    from: f32,
    to: f32,
    start: Instant,
}

fn mired(kelvin: u32) -> f32 {
    1.0e6 / kelvin as f32
}

impl Fade {
    fn at(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.start).as_secs_f32() / TRANSITION.as_secs_f32();
        self.from + (self.to - self.from) * t.clamp(0.0, 1.0)
    }

    /// Retargets the fade (starting from wherever it currently is) if
    /// `target` changed, and returns the current temperature.
    fn update(&mut self, target: Option<u32>, now: Instant) -> Option<u32> {
        let to = mired(target.unwrap_or(NEUTRAL_KELVIN));
        if to != self.to {
            *self = Fade {
                from: self.at(now),
                to,
                start: now,
            };
        }
        let kelvin = (1.0e6 / self.at(now)).round() as u32;
        (kelvin < NEUTRAL_KELVIN).then_some(kelvin)
    }
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
            fade: Fade {
                from: mired(NEUTRAL_KELVIN),
                to: mired(NEUTRAL_KELVIN),
                start: Instant::now(),
            },
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// The color temperature to render with right now, or `None` for no
    /// filter: [`Self::kelvin`] eased in over [`TRANSITION`], so enabling and
    /// disabling night mode (or changing its temperature) fades linearly.
    /// Call once per frame; it is cheap and safe to call per output.
    pub fn current_kelvin(&mut self) -> Option<u32> {
        let target = self.kelvin();
        self.fade.update(target, Instant::now())
    }

    /// The color temperature the clients asked for, or `None` when no live
    /// client wants a filter (nothing requested, or the latest request was
    /// neutral). Destroying the object that made the latest request falls back to the
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

    fn fresh_fade(now: Instant) -> Fade {
        Fade {
            from: mired(NEUTRAL_KELVIN),
            to: mired(NEUTRAL_KELVIN),
            start: now,
        }
    }

    #[test]
    fn fade_is_linear_in_mired_and_reaches_the_target() {
        let t0 = Instant::now();
        let mut fade = fresh_fade(t0);
        assert_eq!(fade.update(None, t0), None);
        // Start of the fade is still neutral.
        assert_eq!(fade.update(Some(3000), t0), None);
        let half = fade.update(Some(3000), t0 + TRANSITION / 2).unwrap();
        let expected = 1.0e6 / ((mired(NEUTRAL_KELVIN) + mired(3000)) / 2.0);
        assert!(
            (half as f32 - expected).abs() <= 1.0,
            "{half} vs {expected}"
        );
        assert_eq!(fade.update(Some(3000), t0 + TRANSITION * 2), Some(3000));
    }

    #[test]
    fn fade_out_ends_with_no_filter_and_retargets_from_current() {
        let t0 = Instant::now();
        let mut fade = fresh_fade(t0);
        fade.update(Some(3000), t0);
        let done = t0 + TRANSITION;
        assert_eq!(fade.update(Some(3000), done), Some(3000));
        // Turning off starts from 3000 K and warms back up gradually.
        assert_eq!(fade.update(None, done), Some(3000));
        let mid = fade.update(None, done + TRANSITION / 2).unwrap();
        assert!(mid > 3000 && mid < NEUTRAL_KELVIN);
        assert_eq!(fade.update(None, done + TRANSITION * 2), None);
    }

    #[test]
    fn clamps_requests() {
        assert_eq!(clamp_kelvin(0), NEUTRAL_KELVIN);
        assert_eq!(clamp_kelvin(9000), NEUTRAL_KELVIN);
        assert_eq!(clamp_kelvin(500), MIN_KELVIN);
        assert_eq!(clamp_kelvin(3400), 3400);
    }
}
