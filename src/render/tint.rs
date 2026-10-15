//! Screen-wide warm tint for the `ironland-display-filter-v1` protocol
//! (see `crate::display_filter`).
//!
//! Smithay's renderers only alpha-blend, so a true per-channel multiply
//! (what a gamma-ramp night light does) isn't available; instead a
//! translucent, warm-colored quad is drawn over the whole output. That
//! lowers contrast slightly compared to a real color matrix but reads as
//! the same orange shift, and works identically on the winit and udev
//! backends.

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

/// Opacity of the tint at [`MIN_KELVIN`]; scales down linearly to nothing
/// at [`NEUTRAL_KELVIN`]. Kept well below 1 so text stays readable.
const MAX_ALPHA: f32 = 0.45;

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
    let kelvin = kelvin.clamp(MIN_KELVIN, NEUTRAL_KELVIN);
    let [r, g, b] = kelvin_to_rgb(kelvin);
    let warmth = (NEUTRAL_KELVIN - kelvin) as f32 / (NEUTRAL_KELVIN - MIN_KELVIN) as f32;
    let alpha = MAX_ALPHA * warmth;
    Color32F::new(r * alpha, g * alpha, b * alpha, alpha)
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
    fn tint_fades_out_at_neutral() {
        assert_eq!(tint_color(NEUTRAL_KELVIN).a(), 0.0);
        assert!(tint_color(3400).a() > 0.0);
        assert!(tint_color(1000).a() > tint_color(3400).a());
    }
}
