//! The small corner badge marking a window as running inside a VM.
//!
//! It is a pictogram - a shaded isometric cube on a rounded blue tile, the
//! usual "virtual machine / container" glyph - rasterized on first use, at twice its logical size so it stays
//! crisp on HiDPI outputs (buffer scale 2) and merely downsamples on 1x ones.
//! No texture assets or font involved: like the launcher overlay, it's drawn
//! straight into a CPU buffer and uploaded as a `MemoryRenderBuffer`.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};

/// Logical size (width and height) of the badge, in pixels.
pub const BADGE_SIZE: i32 = 22;
/// Distance between the badge and the window's top/right edges, in logical pixels.
pub const BADGE_MARGIN: i32 = 6;
/// The badge is drawn at this multiple of its logical size.
const BADGE_BUFFER_SCALE: i32 = 2;

const TILE_COLOR: [f32; 3] = [58.0, 82.0, 188.0];
const TILE_ALPHA: f32 = 0.94;
const CUBE_TOP: [f32; 3] = [255.0, 255.0, 255.0];
const CUBE_LEFT: [f32; 3] = [196.0, 207.0, 245.0];
const CUBE_RIGHT: [f32; 3] = [226.0, 232.0, 252.0];

/// Samples per pixel along each axis, for smooth edges.
const SUPERSAMPLE: usize = 4;

type Point = (f32, f32);

/// Whether `p` lies inside the convex polygon `poly` (any winding).
fn inside_convex(poly: &[Point], p: Point) -> bool {
    let mut sign = 0.0f32;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        if cross == 0.0 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

/// Whether `p` lies inside the rounded square covering `[0, size]²` with corner `radius`.
fn inside_rounded_square(size: f32, radius: f32, p: Point) -> bool {
    let half = size / 2.0;
    let dx = ((p.0 - half).abs() - (half - radius)).max(0.0);
    let dy = ((p.1 - half).abs() - (half - radius)).max(0.0);
    dx * dx + dy * dy <= radius * radius
}

/// Renders the badge as premultiplied BGRA (`Fourcc::Argb8888` memory order),
/// returning the pixels and the buffer's side length in pixels.
pub fn rasterize() -> (Vec<u8>, usize) {
    let side = (BADGE_SIZE * BADGE_BUFFER_SCALE) as usize;
    let size = side as f32;
    let radius = size * 0.24;

    // Hexagonal cube outline split into its three visible faces.
    let (cx, cy, r) = (size / 2.0, size / 2.0 + 0.5, size * 0.33);
    let dx = r * 0.866;
    let top = (cx, cy - r);
    let top_right = (cx + dx, cy - r / 2.0);
    let bottom_right = (cx + dx, cy + r / 2.0);
    let bottom = (cx, cy + r);
    let bottom_left = (cx - dx, cy + r / 2.0);
    let top_left = (cx - dx, cy - r / 2.0);
    let center = (cx, cy);
    let faces: [([Point; 4], [f32; 3]); 3] = [
        ([top, top_right, center, top_left], CUBE_TOP),
        ([top_left, center, bottom, bottom_left], CUBE_LEFT),
        ([top_right, bottom_right, bottom, center], CUBE_RIGHT),
    ];

    let mut pixels = vec![0u8; side * side * 4];
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
    for y in 0..side {
        for x in 0..side {
            // Accumulated premultiplied color and coverage.
            let (mut r_acc, mut g_acc, mut b_acc, mut a_acc) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let p = (
                        x as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32,
                        y as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32,
                    );
                    let (color, alpha) = if let Some((_, face)) =
                        faces.iter().find(|(poly, _)| inside_convex(poly, p))
                    {
                        (*face, 1.0)
                    } else if inside_rounded_square(size, radius, p) {
                        (TILE_COLOR, TILE_ALPHA)
                    } else {
                        continue;
                    };
                    r_acc += color[0] * alpha;
                    g_acc += color[1] * alpha;
                    b_acc += color[2] * alpha;
                    a_acc += alpha;
                }
            }
            let i = (y * side + x) * 4;
            pixels[i] = (b_acc / samples).round() as u8;
            pixels[i + 1] = (g_acc / samples).round() as u8;
            pixels[i + 2] = (r_acc / samples).round() as u8;
            pixels[i + 3] = (a_acc / samples * 255.0).round() as u8;
        }
    }
    (pixels, side)
}

thread_local! {
    static BADGE: MemoryRenderBuffer = {
        let (pixels, side) = rasterize();
        MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (side as i32, side as i32),
            BADGE_BUFFER_SCALE,
            Transform::Normal,
            None,
        )
    };
}

/// The badge's render buffer (rasterized on first use, then shared).
pub fn badge_buffer() -> MemoryRenderBuffer {
    BADGE.with(|buffer| buffer.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(pixels: &[u8], side: usize, x: usize, y: usize) -> u8 {
        pixels[(y * side + x) * 4 + 3]
    }

    #[test]
    fn badge_buffer_is_twice_the_logical_size() {
        let (pixels, side) = rasterize();
        assert_eq!(side as i32, BADGE_SIZE * BADGE_BUFFER_SCALE);
        assert_eq!(pixels.len(), side * side * 4);
    }

    #[test]
    fn tile_corners_are_transparent_and_the_cube_is_opaque() {
        let (pixels, side) = rasterize();
        assert_eq!(alpha_at(&pixels, side, 0, 0), 0);
        assert_eq!(alpha_at(&pixels, side, side - 1, side - 1), 0);
        // The tile's flat edge is translucent-but-visible, the cube solid.
        assert!(alpha_at(&pixels, side, side / 2, 1) > 200);
        assert_eq!(alpha_at(&pixels, side, side / 2, side / 2), 255);
    }

    #[test]
    fn colors_are_premultiplied() {
        let (pixels, _) = rasterize();
        for px in pixels.chunks_exact(4) {
            assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3], "{px:?}");
        }
    }

    #[test]
    fn convex_polygon_test_accepts_either_winding() {
        let square = [(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)];
        let reversed = [(0.0, 0.0), (0.0, 4.0), (4.0, 4.0), (4.0, 0.0)];
        for poly in [square, reversed] {
            assert!(inside_convex(&poly, (2.0, 2.0)));
            assert!(!inside_convex(&poly, (5.0, 2.0)));
        }
    }
}
