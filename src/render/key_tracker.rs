//! Key-state overlay for diagnosing stuck or phantom keys (see
//! [`crate::config::KeyTrackerSettings`]). The keyboard filter in
//! `input_handler::keyboard` calls [`KeyTracker::record`] for every key
//! event *before* any shortcut/launcher handling, so what's shown is what
//! libinput/xkb reported, not what a client or binding did with it. Each
//! tracked key gets a keycap icon (raised outline = up, pressed-in solid =
//! down) and its name; every state change is also logged.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::keyboard::Keysym,
    utils::{Logical, Point, Size, Transform},
};
use tracing::{info, warn};
use xkbcommon::xkb;

use crate::{
    config::{KeyTrackerSettings, OverlayPosition},
    font::{Canvas, GLYPH_HEIGHT},
};

const FONT_SCALE: i32 = 2;
const PADDING: i32 = 10;
const ROW_GAP: i32 = 6;
const ICON_W: i32 = 22;
const ICON_H: i32 = 18;
const ICON_GAP: i32 = 10;
const MARGIN: i32 = 16;
const COLOR_BACKGROUND: [u8; 4] = [20, 18, 16, 210];
const COLOR_UP: [u8; 4] = [140, 230, 140, 255];
const COLOR_DOWN: [u8; 4] = [235, 100, 100, 255];
const COLOR_EDGE: [u8; 4] = [90, 86, 82, 255];

#[derive(Debug)]
struct Tracked {
    /// Display label, e.g. `CONTROL L`.
    label: String,
    sym: Keysym,
    down: bool,
}

#[derive(Debug, Default)]
pub struct KeyTracker {
    enabled: bool,
    position: OverlayPosition,
    keys: Vec<Tracked>,
    buffer: Option<MemoryRenderBuffer>,
    dirty: bool,
}

impl KeyTracker {
    pub fn new(settings: &KeyTrackerSettings) -> Self {
        let keys = settings
            .keys
            .iter()
            .filter_map(|name| {
                let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
                if sym.raw() == 0 {
                    warn!(key = %name, "key_tracker: unknown keysym name, ignoring");
                    return None;
                }
                Some(Tracked {
                    label: name.replace('_', " ").to_uppercase(),
                    sym,
                    down: false,
                })
            })
            .collect();
        Self {
            enabled: settings.enabled,
            position: settings.position,
            keys,
            buffer: None,
            dirty: true,
        }
    }

    /// Notes a key event; `raw_syms` are the key's layout-level-0 symbols.
    pub fn record(&mut self, raw_syms: &[Keysym], pressed: bool) {
        if !self.enabled {
            return;
        }
        for key in &mut self.keys {
            if raw_syms.contains(&key.sym) && key.down != pressed {
                key.down = pressed;
                self.dirty = true;
                info!(key = %key.label, pressed, "key_tracker: key state changed");
            }
        }
    }

    fn size(&self) -> Size<i32, Logical> {
        let rows = self.keys.len() as i32;
        let row_h = ICON_H.max(GLYPH_HEIGHT as i32 * FONT_SCALE);
        let text_w = self
            .keys
            .iter()
            .map(|k| Canvas::text_width(&format!("{}  DOWN", k.label), FONT_SCALE))
            .max()
            .unwrap_or(0);
        Size::from((
            PADDING * 2 + ICON_W + ICON_GAP + text_w,
            PADDING * 2 + rows * row_h + (rows - 1).max(0) * ROW_GAP,
        ))
    }

    fn rebuild(&self) -> MemoryRenderBuffer {
        let size = self.size();
        let mut canvas = Canvas::new(size.w as usize, size.h as usize, COLOR_BACKGROUND);
        let row_h = ICON_H.max(GLYPH_HEIGHT as i32 * FONT_SCALE);
        for (i, key) in self.keys.iter().enumerate() {
            let y = PADDING + i as i32 * (row_h + ROW_GAP);
            let color = if key.down { COLOR_DOWN } else { COLOR_UP };
            draw_keycap(&mut canvas, PADDING, y, key.down, color);
            let text = format!("{}  {}", key.label, if key.down { "DOWN" } else { "UP" });
            let text_y = y + (row_h - GLYPH_HEIGHT as i32 * FONT_SCALE) / 2;
            canvas.draw_text(PADDING + ICON_W + ICON_GAP, text_y, &text, FONT_SCALE, color);
        }
        MemoryRenderBuffer::from_slice(
            &canvas.pixels,
            Fourcc::Argb8888,
            (canvas.width as i32, canvas.height as i32),
            1,
            Transform::Normal,
            None,
        )
    }

    /// The overlay texture and its top-left position within an output of
    /// `output_size`, or `None` if disabled / nothing is tracked.
    pub fn overlay(
        &mut self,
        output_size: Size<i32, Logical>,
    ) -> Option<(&MemoryRenderBuffer, Point<i32, Logical>)> {
        if !self.enabled || self.keys.is_empty() {
            return None;
        }
        if self.dirty || self.buffer.is_none() {
            self.buffer = Some(self.rebuild());
            self.dirty = false;
        }
        let size = self.size();
        let (x, y) = match self.position {
            OverlayPosition::TopLeft => (MARGIN, MARGIN),
            OverlayPosition::TopRight => (output_size.w - size.w - MARGIN, MARGIN),
            OverlayPosition::BottomLeft => (MARGIN, output_size.h - size.h - MARGIN),
            OverlayPosition::BottomRight => {
                (output_size.w - size.w - MARGIN, output_size.h - size.h - MARGIN)
            }
        };
        self.buffer.as_ref().map(|b| (b, Point::from((x, y))))
    }
}

/// Raised outlined keycap with a base shadow when up; solid and pushed down
/// (no shadow) when pressed.
fn draw_keycap(canvas: &mut Canvas, x: i32, y: i32, down: bool, color: [u8; 4]) {
    let (cap_y, cap_h) = if down { (y + 3, ICON_H - 3) } else { (y, ICON_H - 3) };
    if !down {
        canvas.fill_rect(x, y + 3, ICON_W, ICON_H - 3, COLOR_EDGE);
    }
    canvas.fill_rect(x, cap_y, ICON_W, cap_h, color);
    if !down {
        canvas.fill_rect(x + 2, cap_y + 2, ICON_W - 4, cap_h - 4, COLOR_BACKGROUND);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(keys: &[&str]) -> KeyTracker {
        KeyTracker::new(&KeyTrackerSettings {
            enabled: true,
            keys: keys.iter().map(|k| k.to_string()).collect(),
            position: OverlayPosition::TopLeft,
        })
    }

    #[test]
    fn tracks_press_and_release_of_configured_key() {
        let mut t = tracker(&["Control_L"]);
        t.record(&[Keysym::Control_L], true);
        assert!(t.keys[0].down);
        t.record(&[Keysym::Shift_L], false);
        assert!(t.keys[0].down, "other keys don't affect it");
        t.record(&[Keysym::Control_L], false);
        assert!(!t.keys[0].down);
    }

    #[test]
    fn unknown_names_are_dropped_and_disabled_ignores_events() {
        let t = tracker(&["NotAKeysym", "Control_R"]);
        assert_eq!(t.keys.len(), 1);

        let mut off = KeyTracker::new(&KeyTrackerSettings::default());
        off.record(&[Keysym::Control_L], true);
        assert!(!off.keys[0].down);
    }

    #[test]
    fn overlay_is_anchored_to_the_configured_corner() {
        let mut t = tracker(&["Control_L"]);
        let (_, loc) = t.overlay(Size::from((1920, 1080))).unwrap();
        assert_eq!((loc.x, loc.y), (MARGIN, MARGIN));
    }
}
