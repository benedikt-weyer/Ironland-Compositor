//! Screen-wide warm tint for the `ironland-display-filter-v1` protocol
//! (see `crate::display_filter`).
//!
//! Two ways to apply it:
//!
//! - [`gamma_ramp`]: a per-channel CRTC gamma LUT, used by the udev backend.
//!   A true multiply (blacks stay black), no render cost and no damage, and
//!   it doesn't block direct scanout or show up in screenshots.
//! - [`DisplayTint`]: Smithay's renderers only alpha-blend, so where there's
//!   no gamma LUT (winit, or a CRTC without one) a translucent, warm-colored
//!   quad is drawn over the whole output instead. That lifts blacks and
//!   lowers contrast slightly compared to a real multiply but reads as the
//!   same orange shift.

use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        Color32F,
        element::{
            Kind,
            solid::{SolidColorBuffer, SolidColorRenderElement},
        },
    },
    utils::{Logical, Size},
};

use crate::display_filter::{MIN_KELVIN, NEUTRAL_KELVIN};

/// Opacity of the tint at [`MIN_KELVIN`]; scales down to nothing at
/// [`NEUTRAL_KELVIN`] (linearly in mired, see [`warmth`]). Kept well below 1
/// so text stays readable and blacks don't lift too far.
const MAX_ALPHA: f32 = 0.35;

/// Per-output solid-color buffers, kept across frames so an unchanged tint
/// keeps its element id/commit counter and costs no extra damage.
#[derive(Debug, Default)]
pub struct DisplayTint {
    buffers: HashMap<String, SolidColorBuffer>,
}

impl DisplayTint {
    /// The overlay element for `output_name`, or `None` when no filter is
    /// active (also forgetting that output's buffer).
    pub fn element(
        &mut self,
        output_name: &str,
        size: Size<i32, Logical>,
        scale: f64,
        kelvin: Option<u32>,
    ) -> Option<SolidColorRenderElement> {
        let Some(kelvin) = kelvin else {
            self.buffers.remove(output_name);
            return None;
        };
        let color = tint_color(kelvin);
        let buffer = self
            .buffers
            .entry(output_name.to_string())
            .or_insert_with(|| SolidColorBuffer::new(size, color));
        buffer.update(size, color);
        Some(SolidColorRenderElement::from_buffer(
            buffer,
            (0, 0),
            scale,
            1.0,
            Kind::Unspecified,
        ))
    }
}

/// Premultiplied overlay color for a color temperature: the blackbody
/// color at `kelvin`, more opaque the warmer it is.
fn tint_color(kelvin: u32) -> Color32F {
    let [r, g, b] = kelvin_to_rgb(kelvin.clamp(MIN_KELVIN, NEUTRAL_KELVIN));
    let alpha = MAX_ALPHA * warmth(kelvin);
    Color32F::new(r * alpha, g * alpha, b * alpha, alpha)
}

/// How warm `kelvin` is, from 0 (neutral) to 1 ([`MIN_KELVIN`]). Linear in
/// mired (10^6 / K), which tracks perceived warmth far better than linear
/// kelvin (6500 -> 5000 K is about as noticeable as 2000 -> 1500 K).
fn warmth(kelvin: u32) -> f32 {
    let mired = |k: u32| 1.0e6 / k as f32;
    let kelvin = kelvin.clamp(MIN_KELVIN, NEUTRAL_KELVIN);
    (mired(kelvin) - mired(NEUTRAL_KELVIN)) / (mired(MIN_KELVIN) - mired(NEUTRAL_KELVIN))
}

/// Per-channel multiplier (0..=1) for `kelvin`: [`kelvin_to_rgb`] divided by
/// the white point at [`NEUTRAL_KELVIN`], so neutral is exactly (1, 1, 1)
/// rather than the curve fit's slightly off-white (255, 254, 250).
fn channel_scale(kelvin: u32) -> [f32; 3] {
    let white = kelvin_to_rgb(NEUTRAL_KELVIN);
    let rgb = kelvin_to_rgb(kelvin.clamp(MIN_KELVIN, NEUTRAL_KELVIN));
    [0, 1, 2].map(|i| (rgb[i] / white[i]).min(1.0))
}

/// Gamma ramps (red, green, blue; `len` entries each, as the CRTC reports
/// via `gamma_length`) that multiply every channel by [`channel_scale`].
/// `None` gives the identity ramp. Applied to encoded values, like every
/// other night-light implementation.
pub fn gamma_ramp(kelvin: Option<u32>, len: usize) -> [Vec<u16>; 3] {
    let scale = channel_scale(kelvin.unwrap_or(NEUTRAL_KELVIN));
    scale.map(|scale| {
        (0..len)
            .map(|i| {
                let x = if len > 1 {
                    i as f32 / (len - 1) as f32
                } else {
                    1.0
                };
                (x * scale * f32::from(u16::MAX)).round() as u16
            })
            .collect()
    })
}

/// Approximate sRGB color (0..=1 per channel) of a blackbody at `kelvin`
/// (Tanner Helland's curve fit, valid for 1000-40000K).
fn kelvin_to_rgb(kelvin: u32) -> [f32; 3] {
    let t = kelvin as f32 / 100.0;
    let r = if t <= 66.0 {
        255.0
    } else {
        329.698_73 * (t - 60.0).powf(-0.133_204_76)
    };
    let g = if t <= 66.0 {
        99.470_8 * t.ln() - 161.119_57
    } else {
        288.122_17 * (t - 60.0).powf(-0.075_514_85)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_73 * (t - 10.0).ln() - 305.044_8
    };
    [r, g, b].map(|c| c.clamp(0.0, 255.0) / 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_is_orange() {
        let [r, g, b] = kelvin_to_rgb(3400);
        assert!(r > g && g > b, "{r} {g} {b}");
    }

    #[test]
    fn neutral_ramp_is_identity() {
        let [r, g, b] = gamma_ramp(None, 256);
        for ramp in [&r, &g, &b] {
            assert_eq!(ramp[0], 0);
            assert_eq!(ramp[255], u16::MAX);
            assert_eq!(ramp[128], (128.0f32 / 255.0 * 65535.0).round() as u16);
        }
        assert_eq!(gamma_ramp(Some(NEUTRAL_KELVIN), 256), [r, g, b]);
    }

    #[test]
    fn warm_ramp_cuts_blue_most_and_keeps_black() {
        let [r, g, b] = gamma_ramp(Some(3400), 256);
        assert_eq!([r[0], g[0], b[0]], [0, 0, 0]);
        assert!(r[255] > g[255] && g[255] > b[255]);
    }

    #[test]
    fn warmth_is_linear_in_mired() {
        assert_eq!(warmth(NEUTRAL_KELVIN), 0.0);
        assert_eq!(warmth(MIN_KELVIN), 1.0);
        // 2000 -> 1000 K is a much bigger step than 6500 -> 5500 K.
        assert!(warmth(1000) - warmth(2000) > 5.0 * (warmth(5500) - warmth(6500)).abs());
    }

    #[test]
    fn tint_fades_out_at_neutral() {
        assert_eq!(tint_color(NEUTRAL_KELVIN).a(), 0.0);
        assert!(tint_color(3400).a() > 0.0);
        assert!(tint_color(1000).a() > tint_color(3400).a());
    }
}
