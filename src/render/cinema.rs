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
//!   with a glow - a pixel shader ([`glow`]) in which every color along the
//!   lit output's border is a point light, spreading out in all directions
//!   and mixing with its neighbours, so a monitor to the right of the lit
//!   one lights up along its left edge in the colors of the lit one's right
//!   edge, a bright spot lights a soft disc, and one diagonally across only
//!   gets the corner's glow. The picture position of outputs is whatever
//!   the output layout says, gaps included.
//!
//! The cover is inserted behind everything the caller draws itself (cursor,
//! overlays) and in front of windows and layer surfaces, so the pointer stays
//! visible on dark outputs; the damage tracker skips whatever the opaque cover
//! hides.

use std::collections::HashMap;
use std::time::{Duration, Instant};

mod glow;

pub use glow::GlowRenderElement;
use glow::{GlowCover, Lights};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Color32F, ExportMem, ImportAll, ImportMem, Renderer,
            element::{
                Id, Kind,
                solid::{SolidColorBuffer, SolidColorRenderElement},
            },
            utils::CommitCounter,
        },
    },
    desktop::Space,
    output::Output,
    utils::{Buffer as BufferCoord, Logical, Rectangle, Size},
};

use crate::{
    cinema_mode::Cinema, render::CustomRenderElements, rounded_corners::GlesCapable,
    shell::WindowElement,
};

/// Bounds of `cinema.edge_sample_px`: pixels of the lit output's edge per
/// color kept along it.
const EDGE_SAMPLE_PX: std::ops::RangeInclusive<u32> = 1..=512;
/// Bounds of the time the glow takes to slide to a new sample: the time
/// since the previous sample, so it arrives just as the next one is due,
/// but never so long that the glow trails after a pause in the picture.
const BLEND: (Duration, Duration) = (Duration::from_millis(1), Duration::from_millis(80));
/// How long the first colors take to fade in from black.
const FADE_IN: Duration = Duration::from_millis(250);
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
    /// All black, with as many colors per side as `like`.
    fn black_like(like: &BorderColors) -> Self {
        let black = |side: &Vec<[f32; 3]>| vec![[0.0; 3]; side.len()];
        BorderColors {
            top: black(&like.top),
            bottom: black(&like.bottom),
            left: black(&like.left),
            right: black(&like.right),
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

    /// Averages the strips into one color per `sample_px` pixels along each
    /// side (clamped to [`EDGE_SAMPLE_PX`]).
    pub fn from_strips(strips: &BorderStrips, sample_px: u32) -> Self {
        let (w, h, depth) = (strips.width, strips.height, strips.depth);
        let px = sample_px.clamp(*EDGE_SAMPLE_PX.start(), *EDGE_SAMPLE_PX.end()) as usize;
        BorderColors {
            top: average_cells(&strips.top, w, depth, true, px),
            bottom: average_cells(&strips.bottom, w, depth, true, px),
            left: average_cells(&strips.left, depth, h, false, px),
            right: average_cells(&strips.right, depth, h, false, px),
        }
    }

    /// The colors a fraction `t` (0..=1) of the way from `self` to `to`.
    /// (A different number of colors per side, from a changed output size
    /// or setting, can't be blended: it is `to` at once.)
    fn lerp(&self, to: &BorderColors, t: f32) -> BorderColors {
        let same_shape = self
            .sides()
            .into_iter()
            .zip(to.sides())
            .all(|(a, b)| a.len() == b.len());
        if !same_shape {
            return to.clone();
        }
        let mut out = self.clone();
        for (side, goal) in out.sides_mut().into_iter().zip(to.sides()) {
            for (color, goal) in side.iter_mut().zip(goal.iter()) {
                for (c, g) in color.iter_mut().zip(goal) {
                    *c += (*g - *c) * t;
                }
            }
        }
        out
    }
}

/// Averages BGRA `pixels` (`width` x `height`, tightly packed) into cells of
/// about `cell_px` pixels along the x axis, or the y axis if not `horizontal`.
fn average_cells(
    pixels: &[u8],
    width: usize,
    height: usize,
    horizontal: bool,
    cell_px: usize,
) -> Vec<[f32; 3]> {
    let along = if horizontal { width } else { height };
    let cells = along.div_ceil(cell_px).max(1);
    (0..cells)
        .map(|cell| {
            let from = cell * along / cells;
            let to = ((cell + 1) * along / cells).max(from + 1).min(along);
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
/// The frame is composited into an offscreen buffer with `Transform::Normal`,
/// which Smithay's GLES renderer lays out with the picture's top row at GL
/// row 0, so the readback is already in picture order. The mapping's
/// `flipped()` says nothing about that (`GlesMapping` hard-codes `true`, which
/// only suits window surfaces), and honouring it mirrors the glow vertically;
/// `screencopy::fulfill` ignores it too.
fn read_rows<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    region: Rectangle<i32, BufferCoord>,
) -> Option<Vec<u8>>
where
    R: Renderer + ExportMem,
{
    let mapping = renderer
        .copy_framebuffer(framebuffer, region, Fourcc::Argb8888)
        .ok()?;
    let bytes = renderer.map_texture(&mapping).ok()?;
    let len = region.size.w as usize * 4 * region.size.h as usize;
    (bytes.len() >= len).then(|| bytes[..len].to_vec())
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
    glow: Option<GlowCover>,
}

impl Cover {
    /// The element to put in front of the output's windows.
    pub fn element<R>(self, renderer: &mut R) -> CustomRenderElements<R>
    where
        R: Renderer + ImportAll + ImportMem + GlesCapable,
        R::TextureId: Clone + Send + 'static,
    {
        if let Some(glow) = self.glow {
            match glow::glow_program(renderer.gles_renderer()) {
                Ok(program) => {
                    return CustomRenderElements::Glow(
                        glow.element(program, self.size, self.level),
                    );
                }
                Err(err) => {
                    static WARN_ONCE: std::sync::Once = std::sync::Once::new();
                    WARN_ONCE.call_once(|| {
                        tracing::warn!(?err, "cinema: glow shader failed to compile, no glow");
                    });
                }
            }
        }
        CustomRenderElements::Tint(self.solid)
    }
}

/// What a dark output's glow element was made from.
#[derive(Debug, Clone, PartialEq)]
struct GlowKey {
    colors_generation: u64,
    lit: Rectangle<i32, Logical>,
    dark: Rectangle<i32, Logical>,
    glow: u32,
    level: u32,
}

/// A dark output's glow: the lights, and the identity its element keeps
/// across frames (so the damage tracker sees "unchanged" while the key is).
#[derive(Debug)]
struct GlowState {
    key: GlowKey,
    lights: Lights,
    id: Id,
    commit: CommitCounter,
}

/// Cinema mode's state on the render side: the sampled border colors and the
/// per-output buffers.
#[derive(Debug, Default)]
pub struct CinemaRender {
    /// The colors the glow shows right now.
    displayed: Option<BorderColors>,
    /// The latest sampled colors, which `displayed` slides to.
    target: Option<BorderColors>,
    /// What `displayed` was when `target` came in, where the slide starts.
    from: Option<BorderColors>,
    /// When `target` came in, and how long the slide to it takes.
    blend: Option<(Instant, Duration)>,
    last_submit: Option<Instant>,
    /// Bumped whenever `displayed` changes.
    generation: u64,
    last_sample: Option<Instant>,
    solid: HashMap<String, SolidColorBuffer>,
    glow: HashMap<String, GlowState>,
}

impl CinemaRender {
    /// What `output` should show this frame, given the protocol's current
    /// state (`None` when cinema mode is off).
    ///
    /// The lit output's border is asked for at most `sample_rate_hz` times a
    /// second (`0`: whenever it changed).
    pub fn frame(
        &mut self,
        cinema: Option<&Cinema>,
        space: &Space<WindowElement>,
        output: &Output,
        sample_rate_hz: u32,
    ) -> CinemaFrame {
        let interval = match sample_rate_hz {
            0 => Duration::ZERO,
            hz => Duration::from_secs_f64(1.0 / f64::from(hz)),
        };
        self.frame_at(cinema, space, output, interval, Instant::now())
    }

    fn frame_at(
        &mut self,
        cinema: Option<&Cinema>,
        space: &Space<WindowElement>,
        output: &Output,
        sample_interval: Duration,
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
            // A little early counts as due: frames arrive a few microseconds
            // off their nominal time, and a rate equal to the refresh rate
            // should sample every frame, not most of them.
            let wait = sample_interval - sample_interval / 8;
            let due = self
                .last_sample
                .is_none_or(|at| now.saturating_duration_since(at) >= wait);
            if cinema.glow > 0.0 && due {
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
                    level: (cinema.level * 1000.0).round() as u32,
                };
                if self.glow.get(&name).is_none_or(|g| g.key != key) {
                    let lights = Lights::new(lit, dark, colors, cinema.glow);
                    let state = self.glow.entry(name.clone()).or_insert_with(|| GlowState {
                        key: key.clone(),
                        lights: lights.clone(),
                        id: Id::new(),
                        commit: CommitCounter::default(),
                    });
                    state.key = key;
                    state.lights = lights;
                    state.commit.increment();
                }
                self.glow.get(&name).map(|g| GlowCover {
                    id: g.id.clone(),
                    commit: g.commit,
                    lights: g.lights.clone(),
                })
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

    /// Marks that the lit output's border was just sampled (or tried to be),
    /// starting the wait for the next sample. Not done by `frame`: a frame
    /// without changes is not sampled, and must not use up the interval -
    /// the glow would then wait a whole interval after the picture changed.
    pub fn sampled(&mut self) {
        self.last_sample = Some(Instant::now());
    }

    /// Takes the border colors read from the lit output's frame.
    pub fn submit(&mut self, strips: &BorderStrips, sample_px: u32) {
        self.submit_at(strips, sample_px, Instant::now());
    }

    fn submit_at(&mut self, strips: &BorderStrips, sample_px: u32, now: Instant) {
        let colors = BorderColors::from_strips(strips, sample_px);
        // The first colors fade in from black instead of popping; later ones
        // are reached over the time the sample took to arrive, from wherever
        // the glow is now, so it never jumps.
        let len = match self.last_submit.replace(now) {
            Some(last) if self.displayed.is_some() => {
                now.saturating_duration_since(last).clamp(BLEND.0, BLEND.1)
            }
            _ => FADE_IN,
        };
        self.from = Some(
            self.displayed
                .get_or_insert_with(|| BorderColors::black_like(&colors))
                .clone(),
        );
        self.target = Some(colors);
        self.blend = Some((now, len));
    }

    /// Slides the displayed colors from where they were towards the latest
    /// sample, for the time passed since it came in.
    fn tick(&mut self, now: Instant) {
        let (Some(from), Some(target), Some((start, len))) = (&self.from, &self.target, self.blend)
        else {
            return;
        };
        let t = (now.saturating_duration_since(start).as_secs_f32() / len.as_secs_f32()).min(1.0);
        let next = if t >= 1.0 {
            target.clone()
        } else {
            from.lerp(target, t)
        };
        if self.displayed.as_ref() != Some(&next) {
            self.displayed = Some(next);
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

    /// Pixels of an edge per color in these tests.
    const PX: u32 = 10;
    /// The sample interval (25 Hz) the tests run at.
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(40);

    fn colors_of(strips: &BorderStrips) -> BorderColors {
        BorderColors::from_strips(strips, PX)
    }

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

    /// The glow on `dark` (a lit output at `lit` next to it) as 8-bit RGB at
    /// `(x, y)` logical pixels from the top left of `dark`.
    fn glow_at(
        lit: Rectangle<i32, Logical>,
        dark: Rectangle<i32, Logical>,
        colors: &BorderColors,
        strength: f32,
        (x, y): (i32, i32),
    ) -> [u8; 3] {
        let lights = Lights::new(lit, dark, colors, strength);
        lights
            .at((x as f32, y as f32))
            .map(|c| (c * 255.0 + 0.5) as u8)
    }

    /// The usual layout: a 1920x1080 output with another one to its right.
    fn left_of_dark() -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
        (rect(0, 0, 1920, 1080), rect(1920, 0, 1920, 1080))
    }

    #[test]
    fn strips_are_averaged_per_side_in_rgb() {
        let strips = flat_strips(
            640,
            360,
            [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]],
        );
        let colors = colors_of(&strips);
        assert_eq!(colors.top.len(), 64);
        assert_eq!(colors.left.len(), 36);
        assert_eq!(colors.top[7], [1.0, 0.0, 0.0]);
        assert_eq!(colors.bottom[0], [0.0, 1.0, 0.0]);
        assert_eq!(colors.left[35], [0.0, 0.0, 1.0]);
        assert_eq!(colors.right[20], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn the_sample_width_sets_the_number_of_colors_per_side() {
        let strips = flat_strips(1920, 1080, [[10, 20, 30]; 4]);
        for (px, top, left) in [(1, 1920, 1080), (10, 192, 108), (100, 20, 11), (4000, 4, 3)] {
            let colors = BorderColors::from_strips(&strips, px);
            assert_eq!((colors.top.len(), colors.left.len()), (top, left), "{px}");
            assert_eq!(colors.bottom.len(), top);
            assert_eq!(colors.right.len(), left);
        }
        // 0 and absurd values are clamped rather than dividing by zero.
        assert_eq!(BorderColors::from_strips(&strips, 0).top.len(), 1920);
        assert_eq!(BorderColors::from_strips(&strips, u32::MAX).top.len(), 4);
    }

    #[test]
    fn blending_between_different_numbers_of_colors_snaps() {
        let coarse = BorderColors::from_strips(&flat_strips(640, 360, [[255; 3]; 4]), 100);
        let fine = BorderColors::from_strips(&flat_strips(640, 360, [[0; 3]; 4]), 10);
        assert_eq!(coarse.lerp(&fine, 0.5), fine);
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
        let colors = colors_of(&strips);
        assert_eq!(colors.top[0], [1.0, 0.0, 0.0]);
        assert_eq!(colors.top[63], [0.0, 0.0, 1.0]);
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
        let colors = colors_of(&flat_strips(
            640,
            360,
            [[0, 0, 0], [0, 0, 0], [0, 0, 0], [255, 128, 0]],
        ));
        let (lit, dark) = left_of_dark();
        let at = |x, y| glow_at(lit, dark, &colors, 1.0, (x, y));

        // Left edge of the dark output: the right edge's orange, bright.
        let near = at(6, 540);
        assert!(
            near[0] > 200 && near[1] > 80 && near[1] < near[0] && near[2] == 0,
            "{near:?}"
        );
        // It fades with distance and is black beyond the reach.
        let mid = at(240, 540);
        assert!(mid[1] < near[1] && mid[1] > 0, "{mid:?} vs {near:?}");
        assert_eq!(at(1914, 540), [0, 0, 0]);
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
        let colors = colors_of(&strips);
        let (lit, dark) = left_of_dark();
        let upper = glow_at(lit, dark, &colors, 1.0, (6, 270));
        let lower = glow_at(lit, dark, &colors, 1.0, (6, 810));
        // Each half is dominated by its own color; the other one's light
        // reaches over a little, since lights mix.
        assert!(upper[0] > 100 && upper[2] < upper[0] / 2, "{upper:?}");
        assert!(lower[2] > 100 && lower[0] < lower[2] / 2, "{lower:?}");
    }

    /// The right edge of a 640x360 frame, white where `lit(y)` and black elsewhere.
    fn right_edge_lit_where(lit: impl Fn(usize) -> bool) -> BorderColors {
        let mut strips = flat_strips(640, 360, [[0; 3]; 4]);
        for y in 0..360 {
            for x in 0..strips.depth {
                let px = &mut strips.right[(y * strips.depth + x) * 4..][..4];
                px.copy_from_slice(if lit(y) {
                    &[255, 255, 255, 255]
                } else {
                    &[0, 0, 0, 255]
                });
            }
        }
        colors_of(&strips)
    }

    #[test]
    fn a_bright_spot_on_the_edge_lights_a_disc_not_a_stripe() {
        // A short bright stretch in the middle of the right edge.
        let colors = right_edge_lit_where(|y| (170..190).contains(&y));
        let (lit, dark) = left_of_dark();
        let at = |x, y| glow_at(lit, dark, &colors, 1.0, (x, y))[0];

        let at_spot = at(12, 540);
        // A quarter of the screen above and below the spot (still within
        // reach) the light has spread there, dimmer than at the spot, and
        // the same either way.
        let (above, below) = (at(12, 270), at(12, 810));
        assert!(above > 0 && below > 0, "{above} {below}");
        assert!(
            above < at_spot && below < at_spot,
            "{above} {below} vs {at_spot}"
        );
        assert!(above.abs_diff(below) <= 1, "{above} {below}");
        // It dims smoothly going out, in every direction.
        let along: Vec<u8> = (0..12).map(|i| at(12 + i * 50, 540)).collect();
        assert!(along.windows(2).all(|w| w[1] <= w[0]), "{along:?}");
        let away: Vec<u8> = (0..10).map(|i| at(12, 540 + i * 50)).collect();
        assert!(away.windows(2).all(|w| w[1] <= w[0]), "{away:?}");
    }

    #[test]
    fn lights_of_different_colors_mix_between_them() {
        // Right edge: red in the upper quarter, blue in the lower quarter.
        let mut strips = flat_strips(640, 360, [[0; 3]; 4]);
        for y in 0..360 {
            for x in 0..strips.depth {
                let px = &mut strips.right[(y * strips.depth + x) * 4..][..4];
                px.copy_from_slice(if y < 90 {
                    &[0, 0, 255, 255]
                } else if y >= 270 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 0, 255]
                });
            }
        }
        let (lit, dark) = left_of_dark();
        let middle = glow_at(lit, dark, &colors_of(&strips), 1.0, (6, 540));
        // Halfway between them both reach the texel: a mix, the same of each.
        assert!(middle[0] > 10 && middle[2] > 10, "{middle:?}");
        assert!(middle[0].abs_diff(middle[2]) <= 1, "{middle:?}");
        assert_eq!(middle[1], 0);
    }

    #[test]
    fn a_uniform_edge_is_as_bright_as_its_color_whatever_the_sample_width() {
        let strips = flat_strips(640, 360, [[0, 0, 0], [0, 0, 0], [0, 0, 0], [100, 100, 100]]);
        let (lit, dark) = left_of_dark();
        // Half strength boosts the colors 1.15 times.
        for px in [1, 4, 10, 64, 512] {
            let colors = BorderColors::from_strips(&strips, px);
            // (Away from the ends of the edge, where a line of lights is short of
            // the lights beyond it.)
            for y in [400, 540, 680] {
                let at_edge = glow_at(lit, dark, &colors, 0.5, (0, y))[0];
                assert!(
                    (f32::from(at_edge) - 100.0 * 1.15).abs() < 8.0,
                    "{px} px, y {y}: {at_edge}"
                );
            }
        }
    }

    #[test]
    fn moving_a_small_bright_object_along_the_edge_moves_the_glow_smoothly() {
        // A 4 px bright dot on a 1920 px edge, far narrower than the 60 px
        // between lights, moved a color (10 px) at a time: where the glow
        // is brightest along the edge must follow it step by step, not
        // stay at a light and then leap to the next.
        let (lit, dark) = (rect(0, 1080, 1920, 1080), rect(0, 0, 1920, 1080));
        let centroid = |dot_x: usize| {
            let mut strips = flat_strips(1920, 1080, [[0; 3]; 4]);
            for y in 0..strips.depth {
                for x in dot_x..dot_x + 4 {
                    strips.top[(y * 1920 + x) * 4..][..4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
            let lights = Lights::new(lit, dark, &colors_of(&strips), 1.0);
            let (mut sum, mut weighted) = (0.0f32, 0.0f32);
            for x in (0..1920).step_by(4) {
                let brightness = lights.at((x as f32, 1074.0))[0];
                sum += brightness;
                weighted += brightness * x as f32;
            }
            weighted / sum
        };
        let positions: Vec<f32> = (0..9).map(|i| centroid(900 + 10 * i)).collect();
        let steps: Vec<f32> = positions.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            steps.iter().all(|&s| (5.0..=15.0).contains(&s)),
            "{steps:?} (positions {positions:?})"
        );
    }

    #[test]
    fn a_monitor_above_gets_the_top_edge_and_a_diagonal_one_the_corner() {
        let colors = colors_of(&flat_strips(
            640,
            360,
            [[0, 255, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0]],
        ));
        let lit = rect(0, 1080, 1920, 1080);
        let above = rect(0, 0, 1920, 1080);
        // Bottom of the dark output touches the lit one's top edge.
        let near = glow_at(lit, above, &colors, 1.0, (960, 1074));
        assert!(near[1] > 150 && near[0] == 0, "{near:?}");
        assert_eq!(glow_at(lit, above, &colors, 1.0, (960, 0)), [0, 0, 0]);

        // Diagonally up-left: only the corner's neighbourhood glows, and
        // only faintly compared to the directly adjacent one.
        let diagonal = rect(-1920, 0, 1920, 1080);
        let corner = glow_at(lit, diagonal, &colors, 1.0, (1914, 1074));
        assert!(corner[1] > 0 && corner[1] <= near[1], "{corner:?}");
        assert_eq!(glow_at(lit, diagonal, &colors, 1.0, (0, 0)), [0, 0, 0]);
    }

    #[test]
    fn a_stronger_glow_is_brighter_and_reaches_further() {
        let colors = colors_of(&flat_strips(640, 360, [[200, 200, 200]; 4]));
        let (lit, dark) = left_of_dark();
        let at = |strength, x| glow_at(lit, dark, &colors, strength, (x, 540))[0];
        assert!(at(1.0, 0) > at(0.1, 0));
        let reach = |strength| (0..1920).filter(|&x| at(strength, x) > 0).count();
        assert!(reach(1.0) > reach(0.1));
    }

    #[test]
    fn a_dark_output_inside_the_lit_one_still_glows_from_its_edges() {
        let colors = colors_of(&flat_strips(
            640,
            360,
            [[0, 0, 0], [0, 0, 0], [0, 0, 0], [255, 255, 255]],
        ));
        // Overlapping layout: the dark output sits over the lit one's right half.
        let (lit, dark) = (rect(0, 0, 1920, 1080), rect(960, 0, 960, 1080));
        // Its right side is next to the lit output's right edge: bright.
        assert!(glow_at(lit, dark, &colors, 1.0, (954, 540))[0] > 200);
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
            render.frame_at(None, &space, &left, SAMPLE_INTERVAL, now),
            CinemaFrame::Normal
        ));

        // Without a glow the lit output is left alone, the other gets a plain cover.
        let plain = cinema("DP-1", 0.0);
        assert!(matches!(
            render.frame_at(Some(&plain), &space, &left, SAMPLE_INTERVAL, now),
            CinemaFrame::Normal
        ));
        match render.frame_at(Some(&plain), &space, &right, SAMPLE_INTERVAL, now) {
            CinemaFrame::Dark(cover) => {
                assert_eq!(cover.size, Size::from((1920, 1080)));
                assert!(cover.glow.is_none());
            }
            other => panic!("{other:?}"),
        }

        // With a glow the lit output is sampled, forced while there are no colors yet.
        let glowing = cinema("DP-1", 1.0);
        assert!(matches!(
            render.frame_at(Some(&glowing), &space, &left, SAMPLE_INTERVAL, now),
            CinemaFrame::Lit { force: true }
        ));
        // A frame that was not sampled (no changes) leaves the sample due.
        assert!(matches!(
            render.frame_at(
                Some(&glowing),
                &space,
                &left,
                SAMPLE_INTERVAL,
                now + SAMPLE_INTERVAL / 2
            ),
            CinemaFrame::Lit { force: true }
        ));
        // Not again before the interval is over.
        render.last_sample = Some(now);
        assert!(matches!(
            render.frame_at(
                Some(&glowing),
                &space,
                &left,
                SAMPLE_INTERVAL,
                now + SAMPLE_INTERVAL / 2
            ),
            CinemaFrame::Normal
        ));
        let later = now + SAMPLE_INTERVAL;
        // Until colors are in, the dark output has no glow image.
        match render.frame_at(Some(&glowing), &space, &right, SAMPLE_INTERVAL, now) {
            CinemaFrame::Dark(cover) => assert!(cover.glow.is_none()),
            other => panic!("{other:?}"),
        }

        render.submit_at(&flat_strips(640, 360, [[255, 255, 255]; 4]), PX, later);
        assert!(matches!(
            render.frame_at(Some(&glowing), &space, &left, SAMPLE_INTERVAL, later),
            CinemaFrame::Lit { force: false }
        ));
        // The glow fades in over a few frames rather than appearing at once.
        let mut brightness = vec![];
        for frame in 1..=40 {
            let at = later + Duration::from_millis(16 * frame);
            match render.frame_at(Some(&glowing), &space, &right, SAMPLE_INTERVAL, at) {
                CinemaFrame::Dark(cover) => {
                    assert!(cover.glow.is_some(), "colors are in");
                }
                other => panic!("{other:?}"),
            }
            brightness.push(render.displayed.as_ref().unwrap().right[0][0]);
        }
        assert!(brightness[0] > 0.0 && brightness[0] < 1.0, "{brightness:?}");
        assert!(brightness.windows(2).all(|w| w[1] >= w[0]));
        assert!(
            brightness.iter().filter(|&&b| b < 1.0).count() > 10,
            "{brightness:?}"
        );
        assert_eq!(*brightness.last().unwrap(), 1.0);

        // Back to off: everything is forgotten.
        assert!(matches!(
            render.frame_at(None, &space, &right, SAMPLE_INTERVAL, later),
            CinemaFrame::Normal
        ));
        assert!(render.displayed.is_none() && render.glow.is_empty());
    }

    #[test]
    fn the_sample_rate_sets_how_often_the_lit_output_is_sampled() {
        let (left, right) = (output("DP-1", 0), output("DP-2", 1920));
        let mut space = Space::<WindowElement>::default();
        space.map_output(&left, (0, 0));
        space.map_output(&right, (1920, 0));
        let glowing = cinema("DP-1", 1.0);
        let now = Instant::now();

        // Counts the samples asked for over one second of 60 Hz frames, each
        // answered as soon as it is asked for.
        let samples_at = |interval: Duration| {
            let mut render = CinemaRender::default();
            (0..60)
                .filter(|frame| {
                    let at = now + Duration::from_micros(1_000_000 * frame / 60);
                    let due = matches!(
                        render.frame_at(Some(&glowing), &space, &left, interval, at),
                        CinemaFrame::Lit { .. }
                    );
                    if due {
                        render.last_sample = Some(at);
                    }
                    due
                })
                .count()
        };
        assert_eq!(samples_at(Duration::from_secs_f64(1.0 / 10.0)), 10);
        assert_eq!(samples_at(Duration::from_secs_f64(1.0 / 30.0)), 30);
        // As often as there are frames, or more: every frame.
        assert_eq!(samples_at(Duration::from_secs_f64(1.0 / 60.0)), 60);
        assert_eq!(samples_at(Duration::from_secs_f64(1.0 / 240.0)), 60);
        assert_eq!(samples_at(Duration::ZERO), 60);
    }

    #[test]
    fn a_missing_lit_output_means_no_cinema_mode() {
        let only = output("DP-1", 0);
        let mut space = Space::<WindowElement>::default();
        space.map_output(&only, (0, 0));
        let mut render = CinemaRender::default();
        let unplugged = cinema("HDMI-A-1", 1.0);
        assert!(matches!(
            render.frame_at(
                Some(&unplugged),
                &space,
                &only,
                SAMPLE_INTERVAL,
                Instant::now()
            ),
            CinemaFrame::Normal
        ));
    }

    #[test]
    fn displayed_colors_slide_from_the_old_sample_to_the_new_one() {
        let white = colors_of(&flat_strips(640, 360, [[255, 255, 255]; 4]));
        let black = BorderColors::black_like(&white);
        assert_eq!(black.lerp(&white, 0.0), black);
        assert_eq!(black.lerp(&white, 1.0), white);
        assert!((black.lerp(&white, 0.25).top[0][0] - 0.25).abs() < 1e-6);

        let mut render = CinemaRender::default();
        let start = Instant::now();
        render.submit_at(&flat_strips(640, 360, [[255, 255, 255]; 4]), PX, start);
        render.tick(start + FADE_IN);
        assert_eq!(render.displayed.as_ref(), Some(&white));

        // A darker sample after a long pause is reached linearly over the
        // longest slide, starting from the colors shown when it came in.
        let at = start + FADE_IN + Duration::from_millis(500);
        render.submit_at(&flat_strips(640, 360, [[0, 0, 0]; 4]), PX, at);
        render.tick(at + BLEND.1 / 2);
        let half = render.displayed.as_ref().unwrap().top[0][0];
        assert!((half - 0.5).abs() < 1e-3, "{half}");
        render.tick(at + BLEND.1);
        assert_eq!(render.displayed.as_ref(), Some(&black));

        // A sample 40 ms after a slide starts takes 40 ms, and starts from
        // the mid-slide colors rather than jumping.
        let first = at + BLEND.1 + Duration::from_millis(500);
        render.submit_at(&flat_strips(640, 360, [[255, 255, 255]; 4]), PX, first);
        let next = first + Duration::from_millis(40);
        render.tick(next);
        assert!((render.displayed.as_ref().unwrap().top[0][0] - 0.5).abs() < 1e-3);
        render.submit_at(&flat_strips(640, 360, [[0, 0, 0]; 4]), PX, next);
        render.tick(next + Duration::from_millis(20));
        let quarter = render.displayed.as_ref().unwrap().top[0][0];
        assert!((quarter - 0.25).abs() < 1e-3, "{quarter}");
        render.tick(next + Duration::from_millis(40));
        assert_eq!(render.displayed.as_ref(), Some(&black));

        // Settled colors stop bumping the generation (no re-rasterizing).
        let generation = render.generation;
        render.tick(next + Duration::from_millis(100));
        assert_eq!(render.generation, generation);
    }
}
