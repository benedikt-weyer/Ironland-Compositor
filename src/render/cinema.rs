//! Rendering of cinema mode (`ironland-cinema-mode-v1`, see
//! `crate::cinema_mode`): every output but one is covered in black, and
//! optionally lit by a glow made of the colors along the border of the lit
//! output.
//!
//! - The lit output is drawn as usual. While a glow is wanted, a few times a
//!   second the backend reads thin strips along its four edges back from the
//!   frame it just rendered ([`read_border_strips`]) and hands them to
//!   [`CinemaRender::submit`], which boils them down to a row of colors per
//!   side ([`BorderColors`]).
//! - Every other output gets a [`Cover`]: an opaque, fully black quad, or -
//!   with a glow - a small CPU-rasterized image ([`rasterize_glow`]) that
//!   the GPU scales up to the output (bilinear, so the gradient is smooth).
//!   For each pixel it takes the color at the nearest point on the lit
//!   output's border and fades it with the distance from that border, so a
//!   monitor to the right of the lit one lights up along its left edge in the
//!   colors of the lit one's right edge, and one diagonally across only gets
//!   the corner's color. The picture position of outputs is whatever the
//!   output layout says, gaps included.
//!
//! The cover is inserted behind everything the caller draws itself (cursor,
//! overlays) and in front of windows and layer surfaces, so the pointer stays
//! visible on dark outputs; the damage tracker skips whatever the opaque cover
//! hides.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Color32F, ExportMem, ImportAll, ImportMem, Renderer, TextureMapping,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::{SolidColorBuffer, SolidColorRenderElement},
            },
        },
    },
    desktop::Space,
    output::Output,
    utils::{Buffer as BufferCoord, Logical, Rectangle, Size, Transform},
};

use crate::{
    cinema_mode::Cinema, render::CustomRenderElements, rounded_corners::GlesCapable,
    shell::WindowElement,
};

/// Colors kept along each side of the lit output.
const EDGE_SAMPLES: usize = 48;
/// How often the lit output's border is read back while a glow is on.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(125);
/// Time constant of the glow following the sampled colors, so the 10 Hz
/// samples blend into continuous motion.
const COLOR_SMOOTHING: Duration = Duration::from_millis(120);
/// Logical pixels of a dark output covered by one texel of the glow image.
const LOGICAL_PIXELS_PER_TEXEL: f64 = 12.0;
/// How far the glow reaches into the dark outputs (logical pixels) at the
/// weakest and strongest setting.
const REACH: (f64, f64) = (250.0, 900.0);
/// How much the border colors are boosted at the weakest and strongest
/// setting (they would otherwise be rather dim next to a bright picture).
const GAIN: (f32, f32) = (0.7, 1.6);

/// A row of colors (0..=1, sRGB-encoded) per side of the lit output, each
/// running left to right or top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct BorderColors {
    top: Vec<[f32; 3]>,
    bottom: Vec<[f32; 3]>,
    left: Vec<[f32; 3]>,
    right: Vec<[f32; 3]>,
}

impl BorderColors {
    fn black() -> Self {
        let side = vec![[0.0; 3]; EDGE_SAMPLES];
        BorderColors {
            top: side.clone(),
            bottom: side.clone(),
            left: side.clone(),
            right: side,
        }
    }

    fn sides(&self) -> [&Vec<[f32; 3]>; 4] {
        [&self.top, &self.bottom, &self.left, &self.right]
    }

    fn sides_mut(&mut self) -> [&mut Vec<[f32; 3]>; 4] {
        [
            &mut self.top,
            &mut self.bottom,
            &mut self.left,
            &mut self.right,
        ]
    }

    /// Averages the strips into [`EDGE_SAMPLES`] colors per side.
    pub fn from_strips(strips: &BorderStrips) -> Self {
        let (w, h, depth) = (strips.width, strips.height, strips.depth);
        BorderColors {
            top: average_cells(&strips.top, w, depth, true),
            bottom: average_cells(&strips.bottom, w, depth, true),
            left: average_cells(&strips.left, depth, h, false),
            right: average_cells(&strips.right, depth, h, false),
        }
    }

    /// Moves every color a fraction `k` (0..=1) of the way to `target`,
    /// snapping once it is within one 8-bit step. Returns whether anything
    /// changed.
    fn approach(&mut self, target: &BorderColors, k: f32) -> bool {
        let mut changed = false;
        for (side, goal) in self.sides_mut().into_iter().zip(target.sides()) {
            for (color, goal) in side.iter_mut().zip(goal.iter()) {
                for (c, g) in color.iter_mut().zip(goal) {
                    let next = if (*g - *c).abs() < 0.5 / 255.0 {
                        *g
                    } else {
                        *c + (*g - *c) * k
                    };
                    changed |= next != *c;
                    *c = next;
                }
            }
        }
        changed
    }

    /// Color of the border at `point`, a position on the lit output's edge
    /// in 0..=1 on both axes. At a corner the colors of both edges meet.
    fn at(&self, point: (f64, f64), tolerance: (f64, f64)) -> [f32; 3] {
        let (u, v) = (point.0 as f32, point.1 as f32);
        let mut sum = [0.0f32; 3];
        let mut count = 0.0f32;
        let mut add = |color: [f32; 3]| {
            for (s, c) in sum.iter_mut().zip(color) {
                *s += c;
            }
            count += 1.0;
        };
        if point.0 <= tolerance.0 {
            add(side_color(&self.left, v));
        }
        if point.0 >= 1.0 - tolerance.0 {
            add(side_color(&self.right, v));
        }
        if point.1 <= tolerance.1 {
            add(side_color(&self.top, u));
        }
        if point.1 >= 1.0 - tolerance.1 {
            add(side_color(&self.bottom, u));
        }
        if count == 0.0 {
            return [0.0; 3];
        }
        sum.map(|s| s / count)
    }
}

/// The color at `t` (0..=1) along a side, interpolating between the cells.
fn side_color(side: &[[f32; 3]], t: f32) -> [f32; 3] {
    let at = (t * side.len() as f32 - 0.5).clamp(0.0, (side.len() - 1) as f32);
    let (lo, frac) = (at.floor() as usize, at.fract());
    let hi = (lo + 1).min(side.len() - 1);
    [0, 1, 2].map(|i| side[lo][i] + (side[hi][i] - side[lo][i]) * frac)
}

/// Averages BGRA `pixels` (`width` x `height`, tightly packed) into
/// [`EDGE_SAMPLES`] cells along the x axis, or the y axis if not `horizontal`.
fn average_cells(pixels: &[u8], width: usize, height: usize, horizontal: bool) -> Vec<[f32; 3]> {
    let along = if horizontal { width } else { height };
    (0..EDGE_SAMPLES)
        .map(|cell| {
            let from = cell * along / EDGE_SAMPLES;
            let to = ((cell + 1) * along / EDGE_SAMPLES).max(from + 1).min(along);
            let mut sum = [0.0f32; 3];
            let mut count = 0.0f32;
            for a in from..to {
                for b in 0..(if horizontal { height } else { width }) {
                    let (x, y) = if horizontal { (a, b) } else { (b, a) };
                    let px = &pixels[(y * width + x) * 4..][..4];
                    // Memory order of `Fourcc::Argb8888`: B, G, R, A.
                    sum[0] += px[2] as f32;
                    sum[1] += px[1] as f32;
                    sum[2] += px[0] as f32;
                    count += 1.0;
                }
            }
            sum.map(|s| s / (count.max(1.0) * 255.0))
        })
        .collect()
}

/// Thin strips along the four edges of a rendered frame, as BGRA pixels in
/// picture order (top row first).
#[derive(Debug)]
pub struct BorderStrips {
    width: usize,
    height: usize,
    /// Thickness of the strips in pixels.
    depth: usize,
    top: Vec<u8>,
    bottom: Vec<u8>,
    left: Vec<u8>,
    right: Vec<u8>,
}

/// Thickness of the border strips for a frame of `size`.
fn strip_depth(size: Size<i32, BufferCoord>) -> i32 {
    (size.w.min(size.h) / 64).clamp(4, 24)
}

/// The four strips of a frame of `size` as `(x, y, w, h)` in picture
/// coordinates (origin top left): top, bottom, left, right.
fn strip_rects(size: Size<i32, BufferCoord>) -> [(i32, i32, i32, i32); 4] {
    let depth = strip_depth(size);
    [
        (0, 0, size.w, depth),
        (0, size.h - depth, size.w, depth),
        (0, 0, depth, size.h),
        (size.w - depth, 0, depth, size.h),
    ]
}

/// Reads the border strips of the frame bound as `framebuffer` (of `size`)
/// back from the GPU. `None` if any read fails.
pub fn read_border_strips<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    size: Size<i32, BufferCoord>,
) -> Option<BorderStrips>
where
    R: Renderer + ExportMem,
{
    if size.w < 8 || size.h < 8 {
        return None;
    }
    let [top, bottom, left, right] = strip_rects(size);
    let mut read = |(x, y, w, h): (i32, i32, i32, i32)| {
        read_rows(
            renderer,
            framebuffer,
            size.h,
            Rectangle::new((x, y).into(), (w, h).into()),
        )
    };
    Some(BorderStrips {
        width: size.w as usize,
        height: size.h as usize,
        depth: strip_depth(size) as usize,
        top: read(top)?,
        bottom: read(bottom)?,
        left: read(left)?,
        right: read(right)?,
    })
}

/// Reads `region` (picture coordinates, origin top left) of the bound
/// framebuffer as tightly packed BGRA rows, top row first.
///
/// Smithay's GLES readback addresses rows from the bottom of the frame and
/// returns them bottom-first (`TextureMapping::flipped`), so the region is
/// mirrored on the way in and the rows are reversed on the way out. Should a
/// renderer report the opposite, the read is repeated the other way around.
fn read_rows<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    frame_height: i32,
    region: Rectangle<i32, BufferCoord>,
) -> Option<Vec<u8>>
where
    R: Renderer + ExportMem,
{
    let mut bottom_first = true;
    for _ in 0..2 {
        let y = if bottom_first {
            frame_height - region.loc.y - region.size.h
        } else {
            region.loc.y
        };
        let area = Rectangle::new((region.loc.x, y).into(), region.size);
        let mapping = renderer
            .copy_framebuffer(framebuffer, area, Fourcc::Argb8888)
            .ok()?;
        if mapping.flipped() != bottom_first {
            bottom_first = mapping.flipped();
            continue;
        }
        let bytes = renderer.map_texture(&mapping).ok()?;
        let stride = region.size.w as usize * 4;
        let rows = region.size.h as usize;
        if bytes.len() < stride * rows {
            return None;
        }
        return Some(if bottom_first {
            bytes[..stride * rows]
                .chunks_exact(stride)
                .rev()
                .flatten()
                .copied()
                .collect()
        } else {
            bytes[..stride * rows].to_vec()
        });
    }
    None
}

/// A rasterized glow image: opaque BGRA pixels (black where nothing
/// glows).
#[derive(Debug, PartialEq)]
pub struct GlowImage {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Rasterizes the glow cast on the output at `dark` by the border of the one
/// at `lit` (both in the global logical layout), at `glow` strength 0..=1.
pub fn rasterize_glow(
    lit: Rectangle<i32, Logical>,
    dark: Rectangle<i32, Logical>,
    colors: &BorderColors,
    glow: f32,
) -> GlowImage {
    let width = ((dark.size.w as f64 / LOGICAL_PIXELS_PER_TEXEL).ceil() as usize).max(2);
    let height = ((dark.size.h as f64 / LOGICAL_PIXELS_PER_TEXEL).ceil() as usize).max(2);
    let reach = REACH.0 + (REACH.1 - REACH.0) * glow as f64;
    let gain = GAIN.0 + (GAIN.1 - GAIN.0) * glow;

    let (x0, y0) = (lit.loc.x as f64, lit.loc.y as f64);
    let (x1, y1) = (x0 + lit.size.w as f64, y0 + lit.size.h as f64);
    // Half a pixel of the lit output, in its 0..=1 coordinates.
    let tolerance = (
        0.5 / lit.size.w.max(1) as f64,
        0.5 / lit.size.h.max(1) as f64,
    );

    let mut pixels = vec![0u8; width * height * 4];
    for row in 0..height {
        for col in 0..width {
            let px = dark.loc.x as f64 + (col as f64 + 0.5) / width as f64 * dark.size.w as f64;
            let py = dark.loc.y as f64 + (row as f64 + 0.5) / height as f64 * dark.size.h as f64;

            // The nearest point on the lit output's border, and how far off.
            let (qx, qy, distance) = if (x0..x1).contains(&px) && (y0..y1).contains(&py) {
                // Overlapping layouts: light from the closest edge.
                let nearest = (px - x0).min(x1 - px).min(py - y0).min(y1 - py);
                if px - x0 == nearest {
                    (x0, py, 0.0)
                } else if x1 - px == nearest {
                    (x1, py, 0.0)
                } else if py - y0 == nearest {
                    (px, y0, 0.0)
                } else {
                    (px, y1, 0.0)
                }
            } else {
                let (qx, qy) = (px.clamp(x0, x1), py.clamp(y0, y1));
                (qx, qy, (px - qx).hypot(py - qy))
            };

            let falloff = (1.0 - distance / reach).max(0.0);
            if falloff == 0.0 {
                continue;
            }
            let falloff = (falloff * falloff) as f32;
            let point = ((qx - x0) / (x1 - x0), (qy - y0) / (y1 - y0));
            let [r, g, b] = colors.at(point, tolerance);
            let to_byte = |c: f32| (c * gain * falloff).clamp(0.0, 1.0).mul_add(255.0, 0.5) as u8;
            let at = (row * width + col) * 4;
            pixels[at] = to_byte(b);
            pixels[at + 1] = to_byte(g);
            pixels[at + 2] = to_byte(r);
        }
    }
    for px in pixels.chunks_exact_mut(4) {
        px[3] = 255;
    }
    GlowImage {
        pixels,
        width,
        height,
    }
}

/// What to draw on an output this frame.
#[derive(Debug)]
pub enum CinemaFrame {
    /// Nothing to do on this output: cinema mode is off, or this is the lit
    /// output and no glow sample is due.
    Normal,
    /// This output stays lit, and its border colors are wanted for the glow:
    /// read them back from the frame about to be rendered and hand them to
    /// [`CinemaRender::submit`]. With `force` even if the frame has no new
    /// content (there are no colors yet).
    Lit { force: bool },
    /// This output goes dark.
    Dark(Box<Cover>),
}

/// The black (or glowing) quad over a dark output.
#[derive(Debug)]
pub struct Cover {
    level: f32,
    size: Size<i32, Logical>,
    solid: SolidColorRenderElement,
    /// The glow image and its size in texels.
    glow: Option<(MemoryRenderBuffer, Size<i32, Logical>)>,
}

impl Cover {
    /// The element to put in front of the output's windows.
    pub fn element<R>(self, renderer: &mut R) -> CustomRenderElements<R>
    where
        R: Renderer + ImportAll + ImportMem + GlesCapable,
        R::TextureId: Clone + Send + 'static,
    {
        if let Some((buffer, texels)) = &self.glow
            && let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                (0.0, 0.0),
                buffer,
                Some(self.level),
                Some(Rectangle::from_size(texels.to_f64())),
                Some(self.size),
                Kind::Unspecified,
            )
        {
            return CustomRenderElements::Overlay(element);
        }
        CustomRenderElements::Tint(self.solid)
    }
}

/// A glow image uploaded for one dark output, with what it was made from.
#[derive(Debug)]
struct GlowBuffer {
    key: GlowKey,
    buffer: MemoryRenderBuffer,
    texels: Size<i32, Logical>,
}

#[derive(Debug, PartialEq)]
struct GlowKey {
    colors_generation: u64,
    lit: Rectangle<i32, Logical>,
    dark: Rectangle<i32, Logical>,
    glow: u32,
}

/// Cinema mode's state on the render side: the sampled border colors and the
/// per-output buffers.
#[derive(Debug, Default)]
pub struct CinemaRender {
    /// The colors the glow shows right now.
    displayed: Option<BorderColors>,
    /// The latest sampled colors, which `displayed` follows.
    target: Option<BorderColors>,
    /// Bumped whenever `displayed` changes.
    generation: u64,
    last_sample: Option<Instant>,
    last_tick: Option<Instant>,
    solid: HashMap<String, SolidColorBuffer>,
    glow: HashMap<String, GlowBuffer>,
}

impl CinemaRender {
    /// What `output` should show this frame, given the protocol's current
    /// state (`None` when cinema mode is off).
    pub fn frame(
        &mut self,
        cinema: Option<&Cinema>,
        space: &Space<WindowElement>,
        output: &Output,
    ) -> CinemaFrame {
        self.frame_at(cinema, space, output, Instant::now())
    }

    fn frame_at(
        &mut self,
        cinema: Option<&Cinema>,
        space: &Space<WindowElement>,
        output: &Output,
        now: Instant,
    ) -> CinemaFrame {
        let Some(cinema) = cinema else {
            self.reset();
            return CinemaFrame::Normal;
        };
        // A lit output that is gone (unplugged) means no mode at all rather
        // than a black desktop.
        let Some(lit) = space
            .outputs()
            .find(|o| o.name() == cinema.lit_output)
            .and_then(|o| space.output_geometry(o))
        else {
            self.reset();
            return CinemaFrame::Normal;
        };

        self.tick(now);

        if output.name() == cinema.lit_output {
            let due = self
                .last_sample
                .is_none_or(|at| now.saturating_duration_since(at) >= SAMPLE_INTERVAL);
            if cinema.glow > 0.0 && due {
                self.last_sample = Some(now);
                return CinemaFrame::Lit {
                    force: self.displayed.is_none(),
                };
            }
            return CinemaFrame::Normal;
        }

        let Some(dark) = space.output_geometry(output) else {
            return CinemaFrame::Normal;
        };
        let scale = output.current_scale().fractional_scale();
        let name = output.name();

        let solid = self
            .solid
            .entry(name.clone())
            .or_insert_with(|| SolidColorBuffer::new(dark.size, Color32F::BLACK));
        solid.update(dark.size, Color32F::BLACK);
        let solid = SolidColorRenderElement::from_buffer(
            solid,
            (0, 0),
            scale,
            cinema.level,
            Kind::Unspecified,
        );

        let glow = match (&self.displayed, cinema.glow > 0.0) {
            (Some(colors), true) => {
                let key = GlowKey {
                    colors_generation: self.generation,
                    lit,
                    dark,
                    glow: (cinema.glow * 1000.0).round() as u32,
                };
                if self.glow.get(&name).is_none_or(|g| g.key != key) {
                    let image = rasterize_glow(lit, dark, colors, cinema.glow);
                    let size = (image.width as i32, image.height as i32);
                    let buffer = MemoryRenderBuffer::from_slice(
                        &image.pixels,
                        Fourcc::Argb8888,
                        size,
                        1,
                        Transform::Normal,
                        Some(vec![Rectangle::from_size(size.into())]),
                    );
                    let texels = Size::from(size);
                    self.glow.insert(
                        name.clone(),
                        GlowBuffer {
                            key,
                            buffer,
                            texels,
                        },
                    );
                }
                self.glow.get(&name).map(|g| (g.buffer.clone(), g.texels))
            }
            _ => {
                self.glow.remove(&name);
                None
            }
        };

        CinemaFrame::Dark(Box::new(Cover {
            level: cinema.level,
            size: dark.size,
            solid,
            glow,
        }))
    }

    /// Takes the border colors read from the lit output's frame.
    pub fn submit(&mut self, strips: &BorderStrips) {
        let colors = BorderColors::from_strips(strips);
        // The first colors fade in from black instead of popping.
        self.displayed.get_or_insert_with(BorderColors::black);
        self.target = Some(colors);
    }

    /// Eases the displayed colors towards the sampled ones for the time
    /// passed since the last frame.
    fn tick(&mut self, now: Instant) {
        let dt = self
            .last_tick
            .replace(now)
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        let (Some(displayed), Some(target)) = (&mut self.displayed, &self.target) else {
            return;
        };
        let k = 1.0 - (-dt.as_secs_f32() / COLOR_SMOOTHING.as_secs_f32()).exp();
        if displayed.approach(target, k) {
            self.generation += 1;
        }
    }

    /// Forgets everything once cinema mode is off, so the next time starts
    /// from fresh samples.
    fn reset(&mut self) {
        if self.displayed.is_some() || !self.glow.is_empty() || !self.solid.is_empty() {
            *self = CinemaRender::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// Strips of a `w`x`h` frame where each side is one flat color.
    fn flat_strips(
        w: usize,
        h: usize,
        colors: [[u8; 3]; 4], // top, bottom, left, right as RGB
    ) -> BorderStrips {
        let depth = 4;
        let fill = |w: usize, h: usize, [r, g, b]: [u8; 3]| {
            (0..w * h).flat_map(|_| [b, g, r, 255]).collect::<Vec<u8>>()
        };
        BorderStrips {
            width: w,
            height: h,
            depth,
            top: fill(w, depth, colors[0]),
            bottom: fill(w, depth, colors[1]),
            left: fill(depth, h, colors[2]),
            right: fill(depth, h, colors[3]),
        }
    }

    fn pixel(image: &GlowImage, col: usize, row: usize) -> [u8; 3] {
        let px = &image.pixels[(row * image.width + col) * 4..][..4];
        [px[2], px[1], px[0]] // RGB
    }

    #[test]
    fn strips_are_averaged_per_side_in_rgb() {
        let strips = flat_strips(
            640,
            360,
            [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]],
        );
        let colors = BorderColors::from_strips(&strips);
        assert_eq!(colors.top.len(), EDGE_SAMPLES);
        assert_eq!(colors.top[7], [1.0, 0.0, 0.0]);
        assert_eq!(colors.bottom[0], [0.0, 1.0, 0.0]);
        assert_eq!(colors.left[EDGE_SAMPLES - 1], [0.0, 0.0, 1.0]);
        assert_eq!(colors.right[20], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn a_gradient_along_a_side_is_kept() {
        // Left half of the top strip red, right half blue.
        let mut strips = flat_strips(640, 360, [[0; 3]; 4]);
        for y in 0..strips.depth {
            for x in 0..640 {
                let px = &mut strips.top[(y * 640 + x) * 4..][..4];
                px.copy_from_slice(if x < 320 {
                    &[0, 0, 255, 255]
                } else {
                    &[255, 0, 0, 255]
                });
            }
        }
        let colors = BorderColors::from_strips(&strips);
        assert_eq!(colors.top[0], [1.0, 0.0, 0.0]);
        assert_eq!(colors.top[EDGE_SAMPLES - 1], [0.0, 0.0, 1.0]);
        // Interpolating between cells stays between the two.
        let mid = side_color(&colors.top, 0.5);
        assert!(mid[0] > 0.2 && mid[0] < 0.8, "{mid:?}");
    }

    #[test]
    fn strip_rects_cover_the_edges_in_picture_coordinates() {
        let size: Size<i32, BufferCoord> = (1920, 1080).into();
        let depth = strip_depth(size);
        assert_eq!(depth, 16);
        assert_eq!(
            strip_rects(size),
            [
                (0, 0, 1920, 16),
                (0, 1064, 1920, 16),
                (0, 0, 16, 1080),
                (1904, 0, 16, 1080)
            ]
        );
    }

    #[test]
    fn glow_on_a_right_neighbour_takes_the_lit_outputs_right_edge() {
        let colors = BorderColors::from_strips(&flat_strips(
            640,
            360,
            [[0, 0, 0], [0, 0, 0], [0, 0, 0], [255, 128, 0]],
        ));
        let image = rasterize_glow(
            rect(0, 0, 1920, 1080),
            rect(1920, 0, 1920, 1080),
            &colors,
            1.0,
        );

        // Left edge of the dark output: the right edge's orange, bright.
        let near = pixel(&image, 0, image.height / 2);
        assert!(
            near[0] > 200 && near[1] > 80 && near[1] < near[0] && near[2] == 0,
            "{near:?}"
        );
        // It fades with distance and is black beyond the reach.
        let mid = pixel(&image, image.width / 8, image.height / 2);
        assert!(mid[0] < near[0] && mid[0] > 0, "{mid:?} vs {near:?}");
        assert_eq!(pixel(&image, image.width - 1, image.height / 2), [0, 0, 0]);
        // Fully opaque.
        assert!(image.pixels.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn glow_follows_the_position_along_the_edge() {
        // Right edge: red in the upper half, blue in the lower half.
        let mut strips = flat_strips(640, 360, [[0; 3]; 4]);
        for y in 0..360 {
            for x in 0..strips.depth {
                let px = &mut strips.right[(y * strips.depth + x) * 4..][..4];
                px.copy_from_slice(if y < 180 {
                    &[0, 0, 255, 255]
                } else {
                    &[255, 0, 0, 255]
                });
            }
        }
        let colors = BorderColors::from_strips(&strips);
        let image = rasterize_glow(
            rect(0, 0, 1920, 1080),
            rect(1920, 0, 1920, 1080),
            &colors,
            1.0,
        );
        let upper = pixel(&image, 1, image.height / 4);
        let lower = pixel(&image, 1, image.height * 3 / 4);
        assert!(upper[0] > 100 && upper[2] < 20, "{upper:?}");
        assert!(lower[2] > 100 && lower[0] < 20, "{lower:?}");
    }

    #[test]
    fn a_monitor_above_gets_the_top_edge_and_a_diagonal_one_the_corner() {
        let colors = BorderColors::from_strips(&flat_strips(
            640,
            360,
            [[0, 255, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0]],
        ));
        let lit = rect(0, 1080, 1920, 1080);
        let above = rasterize_glow(lit, rect(0, 0, 1920, 1080), &colors, 1.0);
        // Bottom of the dark output touches the lit one's top edge.
        let near = pixel(&above, above.width / 2, above.height - 1);
        assert!(near[1] > 150 && near[0] == 0, "{near:?}");
        assert_eq!(pixel(&above, above.width / 2, 0), [0, 0, 0]);

        // Diagonally up-left: only the corner's neighbourhood glows, and
        // only faintly compared to the directly adjacent one.
        let diagonal = rasterize_glow(lit, rect(-1920, 0, 1920, 1080), &colors, 1.0);
        let corner = pixel(&diagonal, diagonal.width - 1, diagonal.height - 1);
        assert!(corner[1] > 0 && corner[1] <= near[1], "{corner:?}");
        assert_eq!(pixel(&diagonal, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn a_stronger_glow_is_brighter_and_reaches_further() {
        let colors = BorderColors::from_strips(&flat_strips(640, 360, [[200, 200, 200]; 4]));
        let lit = rect(0, 0, 1920, 1080);
        let dark = rect(1920, 0, 1920, 1080);
        let weak = rasterize_glow(lit, dark, &colors, 0.1);
        let strong = rasterize_glow(lit, dark, &colors, 1.0);
        let row = weak.height / 2;
        assert!(pixel(&strong, 0, row)[0] > pixel(&weak, 0, row)[0]);
        let lit_texels = |image: &GlowImage| {
            (0..image.width)
                .filter(|&c| pixel(image, c, row)[0] > 0)
                .count()
        };
        assert!(lit_texels(&strong) > lit_texels(&weak));
    }

    #[test]
    fn a_dark_output_inside_the_lit_one_still_glows_from_its_edges() {
        let colors = BorderColors::from_strips(&flat_strips(
            640,
            360,
            [[0, 0, 0], [0, 0, 0], [0, 0, 0], [255, 255, 255]],
        ));
        // Overlapping layout: the dark output sits over the lit one's right half.
        let image = rasterize_glow(
            rect(0, 0, 1920, 1080),
            rect(960, 0, 960, 1080),
            &colors,
            1.0,
        );
        // Its right side is next to the lit output's right edge: bright.
        assert!(pixel(&image, image.width - 1, image.height / 2)[0] > 200);
    }

    fn output(name: &str, x: i32) -> Output {
        use smithay::output::{Mode, PhysicalProperties, Subpixel};
        let output = Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        let mode = Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((x, 0).into()));
        output.set_preferred(mode);
        output
    }

    fn cinema(lit: &str, glow: f32) -> Cinema {
        Cinema {
            lit_output: lit.to_string(),
            glow,
            level: 1.0,
        }
    }

    #[test]
    fn frames_per_output_follow_the_lit_one() {
        let (left, right) = (output("DP-1", 0), output("DP-2", 1920));
        let mut space = Space::<WindowElement>::default();
        space.map_output(&left, (0, 0));
        space.map_output(&right, (1920, 0));
        let mut render = CinemaRender::default();
        let now = Instant::now();

        // Off: nothing special anywhere.
        assert!(matches!(
            render.frame_at(None, &space, &left, now),
            CinemaFrame::Normal
        ));

        // Without a glow the lit output is left alone, the other gets a plain cover.
        let plain = cinema("DP-1", 0.0);
        assert!(matches!(
            render.frame_at(Some(&plain), &space, &left, now),
            CinemaFrame::Normal
        ));
        match render.frame_at(Some(&plain), &space, &right, now) {
            CinemaFrame::Dark(cover) => {
                assert_eq!(cover.size, Size::from((1920, 1080)));
                assert!(cover.glow.is_none());
            }
            other => panic!("{other:?}"),
        }

        // With a glow the lit output is sampled, forced while there are no colors yet.
        let glowing = cinema("DP-1", 1.0);
        assert!(matches!(
            render.frame_at(Some(&glowing), &space, &left, now),
            CinemaFrame::Lit { force: true }
        ));
        // Not again before the interval is over.
        assert!(matches!(
            render.frame_at(Some(&glowing), &space, &left, now + SAMPLE_INTERVAL / 2),
            CinemaFrame::Normal
        ));
        // Until colors are in, the dark output has no glow image.
        match render.frame_at(Some(&glowing), &space, &right, now) {
            CinemaFrame::Dark(cover) => assert!(cover.glow.is_none()),
            other => panic!("{other:?}"),
        }

        render.submit(&flat_strips(640, 360, [[255, 255, 255]; 4]));
        let later = now + SAMPLE_INTERVAL;
        assert!(matches!(
            render.frame_at(Some(&glowing), &space, &left, later),
            CinemaFrame::Lit { force: false }
        ));
        // The glow fades in over a few frames rather than appearing at once.
        let mut brightness = vec![];
        for frame in 1..=40 {
            let at = later + Duration::from_millis(16 * frame);
            match render.frame_at(Some(&glowing), &space, &right, at) {
                CinemaFrame::Dark(cover) => {
                    let (buffer, texels) = cover.glow.expect("colors are in");
                    assert_eq!(texels, Size::from((160, 90)));
                    drop(buffer);
                }
                other => panic!("{other:?}"),
            }
            brightness.push(render.displayed.as_ref().unwrap().right[0][0]);
        }
        assert!(brightness[0] > 0.0 && brightness[0] < 1.0, "{brightness:?}");
        assert!(brightness.windows(2).all(|w| w[1] >= w[0]));
        assert!(
            brightness.iter().filter(|&&b| b < 1.0).count() > 20,
            "{brightness:?}"
        );
        assert_eq!(*brightness.last().unwrap(), 1.0);

        // Back to off: everything is forgotten.
        assert!(matches!(
            render.frame_at(None, &space, &right, later),
            CinemaFrame::Normal
        ));
        assert!(render.displayed.is_none() && render.glow.is_empty());
    }

    #[test]
    fn a_missing_lit_output_means_no_cinema_mode() {
        let only = output("DP-1", 0);
        let mut space = Space::<WindowElement>::default();
        space.map_output(&only, (0, 0));
        let mut render = CinemaRender::default();
        let unplugged = cinema("HDMI-A-1", 1.0);
        assert!(matches!(
            render.frame_at(Some(&unplugged), &space, &only, Instant::now()),
            CinemaFrame::Normal
        ));
    }

    #[test]
    fn displayed_colors_ease_towards_the_samples_and_settle() {
        let target = BorderColors::from_strips(&flat_strips(640, 360, [[255, 255, 255]; 4]));
        let mut displayed = BorderColors::black();
        assert!(displayed.approach(&target, 0.5));
        assert!((displayed.top[0][0] - 0.5).abs() < 1e-6);
        let mut steps = 0;
        while displayed.approach(&target, 0.5) {
            steps += 1;
            assert!(steps < 50, "never settles");
        }
        assert_eq!(displayed, target);
        assert!(!displayed.approach(&target, 0.5));
    }
}
