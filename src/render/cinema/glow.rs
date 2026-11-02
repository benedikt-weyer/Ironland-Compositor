//! The glow of cinema mode on a dark output, drawn by a GLES pixel shader
//! (`resources/cinema_glow.frag`).
//!
//! Every color along the lit output's four edges is a point light that
//! spreads out in all directions (quadratic falloff to zero at the glow's
//! reach), and the lights add up: a bright spot on the edge lights a soft
//! disc around it rather than a band straight across the dark output, and
//! lights of different colors mix where they meet.
//!
//! The edges are resampled to [`LIGHTS_PER_SIDE`] lights each, which the
//! shader gets as an array of `mat4` (smithay's only array uniform), a
//! column per light. The shader sums them for every pixel, so the glow is
//! sharp at any resolution and costs the CPU nothing but filling that
//! array; the damage tracker leaves the element alone while it doesn't
//! change. [`Lights::at`] does the same sum on the CPU for the tests.

use smithay::{
    backend::renderer::{
        ImportAll, Renderer,
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        gles::{
            GlesError, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
            UniformValue,
        },
        utils::{CommitCounter, OpaqueRegions},
    },
    utils::{
        Buffer as BufferCoords, Logical, Physical, Rectangle, Scale, Transform,
        user_data::UserDataMap,
    },
};

use super::{BorderColors, GAIN, REACH};
use crate::rounded_corners::GlesCapable;

/// Lights along each side of the lit output (a multiple of 4: the shader
/// gets them four to a matrix).
pub const LIGHTS_PER_SIDE: usize = 32;
const MATRICES_PER_SIDE: usize = LIGHTS_PER_SIDE / 4;
/// How much every light is softened, as a fraction of the distance between
/// lights, so a row of lights blends into an even line of light instead of
/// a string of beads (the remaining ripple is a few percent at most).
const SOFTENING: f32 = 0.5;

const SHADER: &str = include_str!("../../../resources/cinema_glow.frag");

/// The shader with the constants it shares with this file in front.
fn shader_source() -> String {
    format!(
        "#define LIGHTS_PER_SIDE {LIGHTS_PER_SIDE}\n\
         #define MATRICES_PER_SIDE {MATRICES_PER_SIDE}\n\
         #define SIDES_TIMES_MATRICES {}\n\
         #define SOFTENING {SOFTENING:?}\n\
         {SHADER}",
        4 * MATRICES_PER_SIDE
    )
}

#[derive(Debug, Clone)]
struct GlowProgram(GlesPixelProgram);

/// Compiles and caches (in `renderer`'s EGL context user data) the glow
/// shader, returning the cached copy on subsequent calls.
pub fn glow_program(renderer: &mut GlesRenderer) -> Result<GlesPixelProgram, GlesError> {
    if let Some(program) = renderer.egl_context().user_data().get::<GlowProgram>() {
        return Ok(program.0.clone());
    }
    let program = renderer.compile_custom_pixel_shader(
        shader_source(),
        &[
            UniformName::new("lit", UniformType::_4f),
            UniformName::new("reach", UniformType::_1f),
            UniformName::new("lights", UniformType::Matrix4x4),
        ],
    )?;
    renderer
        .egl_context()
        .user_data()
        .insert_if_missing(|| GlowProgram(program.clone()));
    Ok(program)
}

/// How much of a light's strength reaches distance `d` from it.
fn falloff(d: f64, reach: f64) -> f64 {
    let f = (1.0 - d / reach).max(0.0);
    f * f
}

/// The lights of one glow, for one dark output.
#[derive(Debug, Clone, PartialEq)]
pub struct Lights {
    /// The lit output's rectangle in the dark output's logical coordinates:
    /// x0, y0, x1, y1.
    lit: [f32; 4],
    /// Logical pixels the light reaches.
    reach: f32,
    /// [`LIGHTS_PER_SIDE`] colors for each of top, bottom, left and right
    /// (in that order), already multiplied by the light's strength.
    colors: Vec<[f32; 3]>,
}

impl Lights {
    /// The glow cast on the output at `dark` by the border colors of the one
    /// at `lit` (both in the global logical layout), at `glow` strength
    /// 0..=1.
    pub fn new(
        lit: Rectangle<i32, Logical>,
        dark: Rectangle<i32, Logical>,
        colors: &BorderColors,
        glow: f32,
    ) -> Self {
        let reach = REACH.0 + (REACH.1 - REACH.0) * glow as f64;
        let gain = GAIN.0 + (GAIN.1 - GAIN.0) * glow;

        let origin = lit.loc - dark.loc;
        let (x0, y0) = (origin.x as f64, origin.y as f64);
        let (w, h) = (lit.size.w as f64, lit.size.h as f64);

        let mut weighted = Vec::with_capacity(4 * LIGHTS_PER_SIDE);
        for (side, len) in colors.sides().into_iter().zip([w, w, h, h]) {
            // A line of lights of one color should be as bright right at
            // the line as that color itself (times the gain): divide by what
            // all of its lights add up to there.
            let strength = gain / line_strength(len / LIGHTS_PER_SIDE as f64, reach) as f32;
            weighted.extend(
                resample(side)
                    .into_iter()
                    .map(|color| color.map(|c| c * strength)),
            );
        }
        Lights {
            lit: [x0 as f32, y0 as f32, (x0 + w) as f32, (y0 + h) as f32],
            reach: reach as f32,
            colors: weighted,
        }
    }

    /// The colors as the shader's `mat4` array: four lights to a matrix, a
    /// light (rgb, the rest unused) per column.
    fn matrices(&self) -> Vec<[f32; 16]> {
        self.colors
            .chunks(4)
            .map(|chunk| {
                let mut matrix = [0.0; 16];
                for (column, color) in chunk.iter().enumerate() {
                    matrix[4 * column..4 * column + 3].copy_from_slice(color);
                }
                matrix
            })
            .collect()
    }

    /// The glow's color at `(x, y)` (logical, from the dark output's top
    /// left): the shader's sum, on the CPU.
    #[cfg(test)]
    pub fn at(&self, (x, y): (f32, f32)) -> [f32; 3] {
        let [x0, y0, x1, y1] = self.lit;
        let mut sum = [0.0f32; 3];
        for side in 0..4 {
            let horizontal = side < 2;
            let line = [y0, y1, x0, x1][side];
            let (start, len) = if horizontal {
                (x0, x1 - x0)
            } else {
                (y0, y1 - y0)
            };
            let (across, along) = if horizontal {
                (y - line, x)
            } else {
                (x - line, y)
            };
            let pitch = len / LIGHTS_PER_SIDE as f32;
            let soft = SOFTENING * pitch;
            let across2 = across * across + soft * soft;
            if across2 >= self.reach * self.reach {
                continue;
            }
            for j in 0..LIGHTS_PER_SIDE {
                let offset = start + (j as f32 + 0.5) * pitch - along;
                let distance = (across2 + offset * offset).sqrt();
                let f = (1.0 - distance / self.reach).max(0.0);
                let color = self.colors[side * LIGHTS_PER_SIDE + j];
                for (s, c) in sum.iter_mut().zip(color) {
                    *s += c * f * f;
                }
            }
        }
        sum.map(|s| s.clamp(0.0, 1.0))
    }
}

/// What an endless line of lights `pitch` apart (each of strength 1) adds up
/// to right at the line, softened like the shader does.
fn line_strength(pitch: f64, reach: f64) -> f64 {
    let soft = SOFTENING as f64 * pitch;
    let n = (reach / pitch).ceil() as i64;
    let sum: f64 = (-n..=n)
        .map(|j| falloff((j as f64 * pitch).hypot(soft), reach))
        .sum();
    sum.max(f64::EPSILON)
}

/// Brings one side's colors to [`LIGHTS_PER_SIDE`]. More colors than lights
/// are shared between the two nearest lights in proportion to how close they
/// are, so something small moving along the edge moves the glow smoothly
/// rather than from light to light; fewer are interpolated.
fn resample(side: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let (n, k) = (side.len(), LIGHTS_PER_SIDE);
    if n == 0 {
        return vec![[0.0; 3]; k];
    }
    if n < k {
        return (0..k)
            .map(|j| {
                let at = ((j as f32 + 0.5) / k as f32 * n as f32 - 0.5).clamp(0.0, (n - 1) as f32);
                let (lo, frac) = (at.floor() as usize, at.fract());
                let hi = (lo + 1).min(n - 1);
                [0, 1, 2].map(|i| side[lo][i] + (side[hi][i] - side[lo][i]) * frac)
            })
            .collect();
    }

    let mut sums = vec![[0.0f32; 3]; k];
    let mut weights = vec![0.0f32; k];
    for (i, cell) in side.iter().enumerate() {
        // Where the cell's center is in lights, the first light being 0.
        let at = (i as f32 + 0.5) / n as f32 * k as f32 - 0.5;
        let lo = at.floor();
        let frac = at - lo;
        for (j, share) in [(lo, 1.0 - frac), (lo + 1.0, frac)] {
            if j < 0.0 || j >= k as f32 || share <= 0.0 {
                continue;
            }
            let j = j as usize;
            weights[j] += share;
            for (s, c) in sums[j].iter_mut().zip(cell) {
                *s += c * share;
            }
        }
    }
    sums.into_iter()
        .zip(weights)
        .map(|(sum, weight)| sum.map(|s| s / weight.max(f32::EPSILON)))
        .collect()
}

/// What [`CinemaFrame::Dark`](super::CinemaFrame::Dark) carries for a glow:
/// everything but the shader, which only the renderer can compile.
#[derive(Debug)]
pub struct GlowCover {
    pub(super) id: Id,
    pub(super) commit: CommitCounter,
    pub(super) lights: Lights,
}

impl GlowCover {
    /// The element covering the whole output (`size`, logical) at
    /// `alpha` (cinema mode's fade level).
    pub fn element(
        self,
        program: GlesPixelProgram,
        size: smithay::utils::Size<i32, Logical>,
        alpha: f32,
    ) -> GlowRenderElement {
        GlowRenderElement {
            id: self.id,
            commit: self.commit,
            area: Rectangle::from_size(size),
            program,
            lights: self.lights,
            alpha,
        }
    }
}

/// The glow shader over a dark output: black, lit by the lights.
pub struct GlowRenderElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    lights: Lights,
    alpha: f32,
}

impl Element for GlowRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        Rectangle::from_size(self.area.size.to_f64().to_buffer(1.0, Transform::Normal))
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Black all over, so whatever is under it needn't be drawn - unless
        // it is still fading in.
        if self.alpha >= 1.0 {
            OpaqueRegions::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
        } else {
            OpaqueRegions::default()
        }
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl<R> RenderElement<R> for GlowRenderElement
where
    R: Renderer + ImportAll + GlesCapable,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let uniforms = [
            Uniform::new("lit", self.lights.lit),
            Uniform::new("reach", self.lights.reach),
            Uniform::new(
                "lights",
                UniformValue::Matrix4x4 {
                    matrices: self.lights.matrices(),
                    transpose: false,
                },
            ),
        ];

        R::gles_frame(frame)
            .render_pixel_shader_to(
                &self.program,
                src,
                dst,
                self.area.size.to_buffer(1, Transform::Normal),
                Some(damage),
                self.alpha,
                &uniforms,
            )
            .map_err(R::map_gles_error)
    }

    #[inline]
    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lights(colors: Vec<[f32; 3]>) -> Lights {
        Lights {
            lit: [0.0, 0.0, 100.0, 100.0],
            reach: 10.0,
            colors,
        }
    }

    #[test]
    fn the_lights_fit_the_shaders_uniform_array() {
        let colors: Vec<[f32; 3]> = (0..4 * LIGHTS_PER_SIDE)
            .map(|i| [i as f32, 0.5, 0.25])
            .collect();
        let matrices = lights(colors.clone()).matrices();
        assert_eq!(matrices.len(), 4 * MATRICES_PER_SIDE);
        // A side's lights fill whole matrices, four to a matrix, a light per
        // column (column-major), rgb with the fourth row unused.
        for (i, color) in colors.iter().enumerate() {
            let (matrix, column) = (i / 4, i % 4);
            assert_eq!(matrices[matrix][4 * column..4 * column + 3], color[..]);
            assert_eq!(matrices[matrix][4 * column + 3], 0.0);
        }
    }

    #[test]
    fn the_shader_source_gets_the_constants_it_shares() {
        let source = shader_source();
        assert!(source.starts_with("#define LIGHTS_PER_SIDE 32\n"));
        assert!(source.contains("#define MATRICES_PER_SIDE 8\n"));
        assert!(source.contains("#define SIDES_TIMES_MATRICES 32\n"));
        assert!(source.contains("#define SOFTENING 0.5\n"));
    }

    #[test]
    fn more_colors_than_lights_share_them_by_closeness() {
        // A single bright color between two lights lights both, the nearer
        // one more; and moving it moves the balance, not just the winner.
        let spot = |cell: usize| {
            let mut side = vec![[0.0; 3]; 8 * LIGHTS_PER_SIDE];
            side[cell] = [1.0, 0.0, 0.0];
            resample(&side)
        };
        let (a, b) = (spot(8 * 10 + 1), spot(8 * 10 + 5));
        // Cells 80..88 are the span of light 10; 81 is left of its center, 85 right.
        assert!(a[9][0] > 0.0 && a[10][0] > a[9][0], "{:?}", &a[8..12]);
        assert!(b[11][0] > 0.0 && b[10][0] > b[11][0], "{:?}", &b[8..12]);
        assert!(b[9][0] < a[9][0] && b[11][0] > a[11][0]);
    }

    #[test]
    fn fewer_colors_than_lights_are_interpolated() {
        let side = [[0.0; 3], [1.0; 3]];
        let resampled = resample(&side);
        assert_eq!(resampled.len(), LIGHTS_PER_SIDE);
        assert_eq!(resampled[0], [0.0; 3]);
        assert_eq!(resampled[LIGHTS_PER_SIDE - 1], [1.0; 3]);
        assert!(resampled.windows(2).all(|w| w[1][0] >= w[0][0]));
        assert_eq!(resample(&[]), vec![[0.0; 3]; LIGHTS_PER_SIDE]);
    }
}
