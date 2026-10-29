//! A tiny embedded 5x7 pixel font used to draw the launcher overlay directly
//! into a pixel buffer, without depending on any font-rendering library.

pub const GLYPH_WIDTH: usize = 5;
pub const GLYPH_HEIGHT: usize = 7;

/// BGRA byte order, matching `Fourcc::Argb8888` memory layout.
pub type Color = [u8; 4];

fn glyph_rows(ch: char) -> [&'static str; GLYPH_HEIGHT] {
    match ch.to_ascii_uppercase() {
        'A' => [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
        'B' => ["####.", "#...#", "#...#", "####.", "#...#", "#...#", "####."],
        'C' => [".####", "#....", "#....", "#....", "#....", "#....", ".####"],
        'D' => ["####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####."],
        'E' => ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
        'F' => ["#####", "#....", "#....", "####.", "#....", "#....", "#...."],
        'G' => [".####", "#....", "#....", "#.###", "#...#", "#...#", ".####"],
        'H' => ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
        'I' => ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
        'J' => ["..###", "...#.", "...#.", "...#.", "...#.", "#..#.", ".##.."],
        'K' => ["#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#"],
        'L' => ["#....", "#....", "#....", "#....", "#....", "#....", "#####"],
        'M' => ["#...#", "##.##", "#.#.#", "#...#", "#...#", "#...#", "#...#"],
        'N' => ["#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"],
        'O' => [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'P' => ["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."],
        'Q' => [".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#"],
        'R' => ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
        'S' => [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
        'T' => ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
        'U' => ["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'V' => ["#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#.."],
        'W' => ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"],
        'X' => ["#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#"],
        'Y' => ["#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."],
        'Z' => ["#####", "....#", "...#.", "..#..", ".#...", "#....", "#####"],
        '0' => [".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###."],
        '1' => ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."],
        '2' => [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"],
        '3' => ["####.", "....#", "....#", ".###.", "....#", "....#", "####."],
        '4' => ["...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#."],
        '5' => ["#####", "#....", "#....", "####.", "....#", "....#", "####."],
        '6' => [".###.", "#....", "#....", "####.", "#...#", "#...#", ".###."],
        '7' => ["#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#..."],
        '8' => [".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###."],
        '9' => [".###.", "#...#", "#...#", ".####", "....#", "....#", ".###."],
        _ => punctuation_rows(ch),
    }
}

fn punctuation_rows(ch: char) -> [&'static str; GLYPH_HEIGHT] {
    match ch {
        '.' => [".....", ".....", ".....", ".....", ".....", ".##..", ".##.."],
        ',' => [".....", ".....", ".....", ".....", ".....", "..#..", ".#..."],
        '-' => [".....", ".....", ".....", "#####", ".....", ".....", "....."],
        ':' => [".....", "..#..", ".....", ".....", "..#..", ".....", "....."],
        '\'' => ["..#..", "..#..", ".....", ".....", ".....", ".....", "....."],
        '(' => ["...#.", "..#..", ".#...", ".#...", ".#...", "..#..", "...#."],
        ')' => [".#...", "..#..", "...#.", "...#.", "...#.", "..#..", ".#..."],
        '&' => [".##..", "#..#.", ".##..", ".##.#", "#..#.", "#..#.", ".##.#"],
        '_' => [".....", ".....", ".....", ".....", ".....", ".....", "#####"],
        '/' => ["....#", "...#.", "..#..", "..#..", ".#...", "#....", "....."],
        '+' => [".....", "..#..", "..#..", "#####", "..#..", "..#..", "....."],
        '!' => ["..#..", "..#..", "..#..", "..#..", "..#..", ".....", "..#.."],
        '?' => [".###.", "#...#", "....#", "...#.", "..#..", ".....", "..#.."],
        '%' => ["##..#", "##..#", "...#.", "..#..", ".#...", "#..##", "#..##"],
        '>' => ["#....", ".#...", "..#..", "...#.", "..#..", ".#...", "#...."],
        '<' => ["....#", "...#.", "..#..", ".#...", "..#..", "...#.", "....#"],
        '=' => [".....", ".....", "#####", ".....", "#####", ".....", "....."],
        '@' => [".###.", "#...#", "#.###", "#.#.#", "#.##.", "#....", ".###."],
        _ => [".....", ".....", ".....", ".....", ".....", ".....", "....."],
    }
}

/// Returns the on/off pixels for a glyph, top-to-bottom, left-to-right.
pub fn glyph_pixels(ch: char) -> [[bool; GLYPH_WIDTH]; GLYPH_HEIGHT] {
    let rows = glyph_rows(ch);
    let mut out = [[false; GLYPH_WIDTH]; GLYPH_HEIGHT];
    for (y, row) in rows.iter().enumerate() {
        for (x, c) in row.chars().enumerate() {
            out[y][x] = c == '#';
        }
    }
    out
}

/// A simple RGBA (well, BGRA) pixel buffer of a fixed size, used to build the
/// launcher overlay before it is uploaded as a `MemoryRenderBuffer`.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub fn new(width: usize, height: usize, background: Color) -> Self {
        let mut pixels = vec![0u8; width * height * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&background);
        }
        Self { width, height, pixels }
    }

    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = (y * self.width + x) * 4;
        self.pixels[idx..idx + 4].copy_from_slice(&color);
    }

    pub fn fill_rect(&mut self, x0: i32, y0: i32, w: i32, h: i32, color: Color) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                self.put_pixel(x, y, color);
            }
        }
    }

    /// Fills a circle of `diameter` pixels whose bounding box has its
    /// top-left corner at `(x0, y0)`.
    pub fn fill_circle(&mut self, x0: i32, y0: i32, diameter: i32, color: Color) {
        let r = diameter as f32 / 2.0;
        for dy in 0..diameter {
            for dx in 0..diameter {
                let (px, py) = (dx as f32 + 0.5 - r, dy as f32 + 0.5 - r);
                if px * px + py * py <= r * r {
                    self.put_pixel(x0 + dx, y0 + dy, color);
                }
            }
        }
    }

    /// Draws `text` with its top-left corner at `(x0, y0)`, each font pixel
    /// scaled up to a `scale x scale` block. Returns the width drawn, in pixels.
    pub fn draw_text(&mut self, x0: i32, y0: i32, text: &str, scale: i32, color: Color) -> i32 {
        let advance = (GLYPH_WIDTH as i32 + 1) * scale;
        let mut cursor_x = x0;
        for ch in text.chars() {
            let pixels = glyph_pixels(ch);
            for (gy, row) in pixels.iter().enumerate() {
                for (gx, &on) in row.iter().enumerate() {
                    if on {
                        self.fill_rect(
                            cursor_x + gx as i32 * scale,
                            y0 + gy as i32 * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
            cursor_x += advance;
        }
        cursor_x - x0
    }

    pub fn text_width(text: &str, scale: i32) -> i32 {
        text.chars().count() as i32 * (GLYPH_WIDTH as i32 + 1) * scale
    }
}
