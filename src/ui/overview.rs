//! Workspace overview: a full-screen overlay showing every output as a row
//! of mini "virtual monitors" - one per workspace - with the windows living
//! on each. Windows can be dragged from one workspace (or monitor) onto
//! another; clicking a workspace or window switches to it. Opened by
//! the `toggle_overview` action (`super+space` by default), closed with
//! Escape, another Super tap, or after picking something.
//!
//! The overlay is software-rasterized into one `MemoryRenderBuffer` per
//! output (like the launcher and lock panel) and rebuilt only when what it
//! shows changes. While it is visible it grabs the keyboard and pointer:
//! `AnvilState::surface_under` reports nothing under the cursor, so no client
//! sees input, and button events are routed to [`AnvilState::overview_button`].

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportAll, ImportMem, Renderer,
            element::{memory::MemoryRenderBuffer, utils::CropRenderElement},
        },
    },
    desktop::space::{ConstrainBehavior, ConstrainReference, constrain_space_element},
    input::pointer::MotionEvent,
    output::Output,
    utils::{Logical, Point, Rectangle, Size, Transform},
};
use smithay::backend::renderer::element::utils::{ConstrainAlign, ConstrainScaleBehavior};

use crate::{
    config::WorkspaceTransitionAxis,
    font::{Canvas, GLYPH_HEIGHT, GLYPH_WIDTH},
    rounded_corners::GlesCapable,
    render::CustomRenderElements,
    shell::{
        WindowElement,
        workspace::{self, OverviewOutput, OverviewWindow},
    },
    state::{AnvilState, Backend},
};

/// A press that travels farther than this (logical pixels) is a drag, not a click.
const DRAG_THRESHOLD: f64 = 6.0;

const MARGIN: i32 = 40;
const TITLE_HEIGHT: i32 = 64;
/// When the grid has to scroll, thumbnails are never narrower than this.
const MIN_SCROLL_SLOT_WIDTH: i32 = 280;
const SLOT_GAP: i32 = 16;
const MIN_SLOT_WIDTH: i32 = 48;

/// Zoom slider: thumbnail size relative to the size that fits every
/// workspace (1.0), from [`ZOOM_MIN`] to [`ZOOM_MAX`].
pub const ZOOM_MIN: f64 = 0.5;
pub const ZOOM_MAX: f64 = 2.0;
const SLIDER_WIDTH: i32 = 220;
const SLIDER_TRACK_Y: i32 = 30;
const SLIDER_KNOB: i32 = 16;

// Canvas pixels are B, G, R, A.
const fn rgba(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [b, g, r, a]
}

const COLOR_DIM: [u8; 4] = rgba(0, 0, 0, 200);
const COLOR_TEXT: [u8; 4] = rgba(235, 230, 225, 255);
const COLOR_MUTED: [u8; 4] = rgba(150, 140, 130, 255);
const COLOR_SLOT: [u8; 4] = rgba(34, 30, 28, 255);
const COLOR_ACTIVE: [u8; 4] = rgba(137, 180, 250, 255);
const COLOR_DROP: [u8; 4] = rgba(250, 205, 120, 255);
const COLOR_WINDOW_BORDER: [u8; 4] = rgba(20, 18, 17, 255);
const WINDOW_COLORS: [[u8; 4]; 6] = [
    rgba(74, 98, 140, 255),
    rgba(98, 128, 92, 255),
    rgba(140, 100, 76, 255),
    rgba(112, 88, 138, 255),
    rgba(150, 128, 70, 255),
    rgba(78, 126, 128, 255),
];

/// A mouse press the overview is tracking until its release decides between
/// click and drag.
#[derive(Debug, Clone)]
struct Press {
    /// The window under the press, if any - it's what a drag carries.
    window: Option<WindowElement>,
    /// `(output name, workspace index)` of the slot under the press.
    slot: Option<(String, usize)>,
    /// Global pointer position at the press.
    start: Point<f64, Logical>,
    /// Where inside the window's thumbnail it was grabbed, and its size.
    grab: Point<i32, Logical>,
    size: Size<i32, Logical>,
}

#[derive(Debug)]
pub struct OverviewState {
    visible: bool,
    press: Option<Press>,
    /// Pixels the workspace strip is scrolled along its axis (clamped at layout time).
    scroll: i32,
    /// Thumbnail zoom set by the slider; kept across openings.
    zoom: f64,
    /// The zoom slider's knob is being dragged.
    slider_drag: bool,
    /// Per output name: the hash of what the cached buffer shows, and the buffer.
    cache: HashMap<String, (u64, MemoryRenderBuffer)>,
}

impl Default for OverviewState {
    fn default() -> Self {
        Self {
            visible: false,
            press: None,
            scroll: 0,
            zoom: 1.0,
            slider_drag: false,
            cache: HashMap::new(),
        }
    }
}

impl OverviewState {
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    fn open(&mut self) {
        self.visible = true;
        self.press = None;
        self.slider_drag = false;
        self.scroll = 0;
        self.cache.clear();
    }

    fn close(&mut self) {
        self.visible = false;
        self.press = None;
        self.slider_drag = false;
        self.scroll = 0;
        self.cache.clear();
    }
}

/// A live window thumbnail: the window, the output-local rectangle it is
/// scaled into, and the part of that rectangle that may be drawn.
pub type Thumb = (WindowElement, Rectangle<i32, Logical>, Rectangle<i32, Logical>);

/// One workspace thumbnail's place on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    output: usize,
    workspace: usize,
    rect: Rectangle<i32, Logical>,
}

#[derive(Debug, Default)]
struct Layout {
    slots: Vec<Slot>,
    /// How far (pixels) the strip is scrolled along its axis, and the most it can be.
    scroll: i32,
    max_scroll: i32,
}

/// What the pointer is over.
#[derive(Default)]
struct Hit {
    /// `(output index, workspace index)`.
    slot: Option<(usize, usize)>,
    window: Option<(OverviewWindow, Rectangle<i32, Logical>)>,
}

/// Arranges `outputs[0]`'s workspaces as a single strip of thumbnails along
/// `axis` (the axis workspace switches slide on: a row for horizontal, a
/// column for vertical), as large as fit entirely - or, when even the
/// smallest comfortable thumbnails don't, at that size with the rest
/// reachable by scrolling. `scroll` (pixels along the axis) is clamped to the
/// content and returned in [`Layout::scroll`]. Thumbnails keep the output's
/// aspect ratio.
fn compute_layout(
    outputs: &[OverviewOutput],
    view: Size<i32, Logical>,
    scroll: i32,
    axis: WorkspaceTransitionAxis,
    zoom: f64,
) -> Layout {
    let mut layout = Layout::default();
    let Some(out) = outputs.first() else {
        return layout;
    };
    let n = out.count.max(1) as i32;
    let aspect = out.size.w.max(1) as f64 / out.size.h.max(1) as f64;
    let avail_w = (view.w - 2 * MARGIN).max(MIN_SLOT_WIDTH);
    let avail_h = (view.h - TITLE_HEIGHT - MARGIN).max(1);
    let horizontal = axis == WorkspaceTransitionAxis::Horizontal;

    // Thumbnail width: the largest that fits every slot along the axis,
    // never below the readable minimum (the strip scrolls instead), scaled by
    // `zoom` but never past what fits across the axis.
    let fit_along = if horizontal {
        (avail_w - (n - 1) * SLOT_GAP) / n
    } else {
        (((avail_h - (n - 1) * SLOT_GAP) / n) as f64 * aspect).round() as i32
    };
    let fit_across = if horizontal {
        (avail_h as f64 * aspect).round() as i32
    } else {
        avail_w
    };
    let fitted = fit_along.min(fit_across).max(MIN_SCROLL_SLOT_WIDTH);
    let width = ((fitted as f64 * zoom.clamp(ZOOM_MIN, ZOOM_MAX)).round() as i32)
        .min(fit_across)
        .min(avail_w)
        .max(MIN_SLOT_WIDTH);
    let height = (width as f64 / aspect).round() as i32;

    let (step, avail) = if horizontal {
        (width + SLOT_GAP, avail_w)
    } else {
        (height + SLOT_GAP, avail_h)
    };
    let content = n * step - SLOT_GAP;
    layout.max_scroll = (content - avail).max(0);
    layout.scroll = scroll.clamp(0, layout.max_scroll);

    // Centered along the axis when it all fits, and across it in the view.
    let along0 = if horizontal {
        MARGIN + (avail - content).max(0) / 2
    } else {
        TITLE_HEIGHT + (avail - content).max(0) / 2
    } - layout.scroll;
    let across0 = if horizontal {
        TITLE_HEIGHT + ((avail_h - height) / 2).max(0)
    } else {
        (view.w - width) / 2
    };
    for ws in 0..out.count as i32 {
        let along = along0 + ws * step;
        let loc = if horizontal {
            Point::from((along, across0))
        } else {
            Point::from((across0, along))
        };
        layout.slots.push(Slot {
            output: 0,
            workspace: ws as usize,
            rect: Rectangle::new(loc, Size::from((width, height))),
        });
    }
    layout
}

/// The zoom slider's hit area in a view of width `view_w`: the track plus
/// room for the knob, in the title band.
fn slider_rect(view_w: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((view_w - MARGIN - SLIDER_WIDTH - SLIDER_KNOB / 2, SLIDER_TRACK_Y - SLIDER_KNOB)),
        Size::from((SLIDER_WIDTH + SLIDER_KNOB, SLIDER_KNOB * 2)),
    )
}

/// The zoom a pointer at view-local `x` selects on the slider.
fn zoom_at(view_w: i32, x: i32) -> f64 {
    let left = view_w - MARGIN - SLIDER_WIDTH;
    let t = ((x - left) as f64 / SLIDER_WIDTH as f64).clamp(0.0, 1.0);
    ZOOM_MIN + t * (ZOOM_MAX - ZOOM_MIN)
}

/// `window`'s rectangle inside `slot`'s thumbnail.
fn window_rect_in(slot: &Slot, output: &OverviewOutput, window: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let scale = slot.rect.size.w as f64 / output.size.w.max(1) as f64;
    let loc = Point::<i32, Logical>::from((
        slot.rect.loc.x + (window.loc.x as f64 * scale).round() as i32,
        slot.rect.loc.y + (window.loc.y as f64 * scale).round() as i32,
    ));
    let size = Size::from((
        ((window.size.w as f64 * scale).round() as i32).max(4),
        ((window.size.h as f64 * scale).round() as i32).max(4),
    ));
    Rectangle::new(loc, size)
}

impl Layout {
    fn hit(&self, outputs: &[OverviewOutput], point: Point<i32, Logical>) -> Hit {
        if point.y < TITLE_HEIGHT - 4 {
            return Hit::default();
        }
        let Some(slot) = self.slots.iter().find(|s| s.rect.contains(point)) else {
            return Hit::default();
        };
        let mut hit = Hit {
            slot: Some((slot.output, slot.workspace)),
            window: None,
        };
        if let Some(windows) = outputs[slot.output].workspaces.get(slot.workspace) {
            // Later windows are drawn on top, so test them first.
            hit.window = windows.iter().rev().find_map(|w| {
                let rect = window_rect_in(slot, &outputs[slot.output], w.rect);
                rect.contains(point).then(|| (w.clone(), rect))
            });
        }
        hit
    }
}

fn window_key(window: &WindowElement) -> u64 {
    use smithay::reexports::wayland_server::Resource;
    window
        .wl_surface()
        .map(|s| u64::from(s.id().protocol_id()))
        .unwrap_or(0)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(2)).collect();
    out.push_str("..");
    out
}

fn border(canvas: &mut Canvas, rect: Rectangle<i32, Logical>, thickness: i32, color: [u8; 4]) {
    let (x, y, w, h) = (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
    canvas.fill_rect(x, y, w, thickness, color);
    canvas.fill_rect(x, y + h - thickness, w, thickness, color);
    canvas.fill_rect(x, y, thickness, h, color);
    canvas.fill_rect(x + w - thickness, y, thickness, h, color);
}

fn plural(count: usize) -> String {
    format!("{count} workspace{}", if count == 1 { "" } else { "s" })
}

fn draw_window(canvas: &mut Canvas, rect: Rectangle<i32, Logical>, window: &OverviewWindow, alpha_dim: bool) {
    let mut color = WINDOW_COLORS[(window_key(&window.window) % WINDOW_COLORS.len() as u64) as usize];
    if alpha_dim {
        color = [color[0] / 2, color[1] / 2, color[2] / 2, 255];
        // The live thumbnail covers the rect itself, so highlight just outside it.
        let outer = Rectangle::new(rect.loc - Point::from((2, 2)), rect.size + Size::from((4, 4)));
        border(canvas, outer, 2, COLOR_ACTIVE);
    }
    canvas.fill_rect(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h, color);
    border(canvas, rect, 1, COLOR_WINDOW_BORDER);
    let advance = GLYPH_WIDTH as i32 + 1;
    let max_chars = ((rect.size.w - 6) / advance).max(0) as usize;
    if max_chars >= 3 && rect.size.h >= GLYPH_HEIGHT as i32 + 6 {
        canvas.draw_text(
            rect.loc.x + 3,
            rect.loc.y + 3,
            &truncate(&window.title, max_chars),
            1,
            COLOR_TEXT,
        );
    }
}

/// Everything [`rasterize`] needs besides the layout.
struct Scene<'a> {
    outputs: &'a [OverviewOutput],
    layout: &'a Layout,
    view: Size<i32, Logical>,
    /// Pointer in this view's local coordinates, when it is on this output.
    pointer: Option<Point<i32, Logical>>,
    press: Option<&'a Press>,
    dragging: bool,
    hover: &'a Hit,
    zoom: f64,
}

fn rasterize(scene: &Scene<'_>) -> Canvas {
    let view = scene.view;
    let mut canvas = Canvas::new(view.w.max(1) as usize, view.h.max(1) as usize, COLOR_DIM);

    let carried = scene
        .press
        .filter(|_| scene.dragging)
        .and_then(|p| p.window.as_ref());
    let drop_slot = scene.dragging.then_some(scene.hover.slot).flatten();

    for slot in &scene.layout.slots {
        let out = &scene.outputs[slot.output];
        let rect = slot.rect;
        canvas.fill_rect(
            rect.loc.x,
            rect.loc.y,
            rect.size.w,
            rect.size.h,
            COLOR_SLOT,
        );

        if let Some(windows) = out.workspaces.get(slot.workspace) {
            for window in windows {
                if carried == Some(&window.window) {
                    continue;
                }
                let wrect = window_rect_in(slot, out, window.rect);
                let hovered = !scene.dragging
                    && scene
                        .hover
                        .window
                        .as_ref()
                        .is_some_and(|(h, _)| h.window == window.window);
                draw_window(&mut canvas, wrect, window, hovered);
            }
            canvas.draw_text(rect.loc.x + 6, rect.loc.y + 6, &(slot.workspace + 1).to_string(), 2, COLOR_TEXT);
        }

        let is_drop = drop_slot == Some((slot.output, slot.workspace));
        if is_drop {
            border(&mut canvas, rect, 3, COLOR_DROP);
        } else if slot.workspace == out.active {
            border(&mut canvas, rect, 3, COLOR_ACTIVE);
        } else {
            border(&mut canvas, rect, 1, COLOR_MUTED);
        }
    }

    // Slots scrolled under the title area are covered by it.
    canvas.fill_rect(0, 0, view.w, TITLE_HEIGHT - 4, COLOR_DIM);
    let total: usize = scene.outputs.iter().map(|o| o.count).sum();
    canvas.draw_text(MARGIN, 18, &plural(total), 3, COLOR_TEXT);
    canvas.draw_text(
        MARGIN,
        18 + GLYPH_HEIGHT as i32 * 3 + 6,
        "Drag windows between workspaces - scroll for more - click to switch - Esc to close",
        1,
        COLOR_MUTED,
    );

    // Zoom slider, right-aligned in the title band.
    let left = view.w - MARGIN - SLIDER_WIDTH;
    let t = (scene.zoom - ZOOM_MIN) / (ZOOM_MAX - ZOOM_MIN);
    canvas.fill_rect(left, SLIDER_TRACK_Y - 2, SLIDER_WIDTH, 4, COLOR_MUTED);
    let knob_x = left + (t.clamp(0.0, 1.0) * SLIDER_WIDTH as f64).round() as i32;
    canvas.fill_rect(
        left,
        SLIDER_TRACK_Y - 2,
        knob_x - left,
        4,
        COLOR_ACTIVE,
    );
    canvas.fill_rect(
        knob_x - SLIDER_KNOB / 2,
        SLIDER_TRACK_Y - SLIDER_KNOB / 2,
        SLIDER_KNOB,
        SLIDER_KNOB,
        COLOR_TEXT,
    );
    canvas.draw_text(
        left,
        SLIDER_TRACK_Y + SLIDER_KNOB,
        &format!("Zoom {}%", (scene.zoom * 100.0).round() as i32),
        1,
        COLOR_MUTED,
    );

    // The window being dragged follows the pointer, drawn last (on top).
    if let (Some(press), Some(pointer), true) = (scene.press, scene.pointer, scene.dragging)
        && let Some(window) = press.window.as_ref()
        && let Some(info) = scene
            .outputs
            .iter()
            .flat_map(|o| o.workspaces.iter().flatten())
            .find(|w| &w.window == window)
    {
        let rect = Rectangle::new(pointer - press.grab, press.size);
        draw_window(&mut canvas, rect, info, false);
        border(&mut canvas, rect, 2, COLOR_DROP);
    }

    canvas
}

/// Live, scaled-down render elements for the windows `thumbs` lists (from
/// [`AnvilState::overview_thumbnails`]), each fitted into its rectangle and
/// cropped to its clip (the part of it on screen). Windows on hidden
/// workspaces aren't mapped in the space, but a window's surface tree still
/// renders, so they show their latest frame too.
pub fn thumbnail_elements<R>(
    renderer: &mut R,
    output: &Output,
    thumbs: &[Thumb],
) -> Vec<CustomRenderElements<R>>
where
    R: Renderer + ImportAll + ImportMem + GlesCapable,
    R::TextureId: Clone + Send + 'static,
{
    let scale = output.current_scale().fractional_scale();
    let behavior = ConstrainBehavior {
        reference: ConstrainReference::BoundingBox,
        behavior: ConstrainScaleBehavior::Fit,
        align: ConstrainAlign::CENTER,
    };
    thumbs
        .iter()
        .flat_map(|(window, rect, clip)| {
            let clip = clip.to_f64().to_physical(scale).to_i32_round();
            constrain_space_element::<_, _, CropRenderElement<_>, _, _>(
                renderer,
                window,
                Point::<i32, Logical>::from((0, 0)),
                1.0,
                scale,
                *rect,
                behavior,
            )
            .filter_map(|element| CropRenderElement::from_element(element, scale, clip))
            .map(CustomRenderElements::Thumbnail)
            .collect::<Vec<_>>()
        })
        .collect()
}

impl<BackendData: Backend> AnvilState<BackendData> {
    /// The overview's snapshot restricted to `output`: each display's overlay
    /// shows only its own workspaces.
    fn overview_snapshot_for(&self, output: &Output) -> Vec<OverviewOutput> {
        let mut snapshot = workspace::overview_snapshot(self);
        snapshot.retain(|o| &o.output == output);
        snapshot
    }

    /// Scrolls the workspace strip by `pixels` (positive = toward later workspaces).
    pub(crate) fn overview_scroll(&mut self, pixels: f64) {
        self.overview.scroll = (self.overview.scroll + pixels.round() as i32).max(0);
        // Out-of-range values are clamped against the real content height the
        // next time the layout is computed; clamp here too so the stored
        // value never drifts past the end.
        let snapshot = workspace::overview_snapshot(self);
        let axis = self.config.workspaces.transition_axis;
        let zoom = self.overview.zoom;
        let max = self
            .space
            .outputs()
            .filter_map(|o| {
                let geo = self.space.output_geometry(o)?;
                let snap: Vec<_> = snapshot.iter().filter(|s| &s.output == o).cloned().collect();
                Some(compute_layout(&snap, geo.size, i32::MAX, axis, zoom).scroll)
            })
            .max()
            .unwrap_or(0);
        self.overview.scroll = self.overview.scroll.min(max);
        self.overview.cache.clear();
    }

    pub fn toggle_overview(&mut self) {
        if self.overview.is_visible() {
            self.close_overview();
        } else {
            self.open_overview();
        }
    }

    pub fn open_overview(&mut self) {
        if self.overview.is_visible() || self.lock.is_locked() {
            return;
        }
        self.launcher.close();
        self.overview.open();

        // Clients stop seeing the pointer while the overview is up.
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: pointer.current_location(),
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: smithay::backend::input::InputTime::now(),
            },
        );
        pointer.frame(self);
    }

    pub fn close_overview(&mut self) {
        self.overview.close();
    }

    /// The overview's overlay buffer for `output`, rebuilt only when what it
    /// shows (workspaces, windows, hover, drag) has changed since last time.
    pub fn overview_buffer_for(&mut self, output: &Output) -> Option<MemoryRenderBuffer> {
        if !self.overview.is_visible() {
            return None;
        }
        let geo = self.space.output_geometry(output)?;
        let view = geo.size;
        let snapshot = self.overview_snapshot_for(output);
        let layout = compute_layout(&snapshot, view, self.overview.scroll, self.config.workspaces.transition_axis, self.overview.zoom);

        let pointer_global = self.pointer.current_location();
        let local = (pointer_global - geo.loc.to_f64()).to_i32_round();
        let on_output = Rectangle::from_size(view).contains(local);
        let hover = if on_output {
            layout.hit(&snapshot, local)
        } else {
            Hit::default()
        };
        let press = self.overview.press.clone();
        let dragging = press.as_ref().is_some_and(|p| {
            let d = pointer_global - p.start;
            p.window.is_some() && d.x.hypot(d.y) > DRAG_THRESHOLD
        });

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (view.w, view.h, dragging, layout.scroll, (self.overview.zoom * 1000.0) as i32).hash(&mut hasher);
        for out in &snapshot {
            (&out.name, out.size.w, out.size.h, out.active, out.count).hash(&mut hasher);
            for (idx, windows) in out.workspaces.iter().enumerate() {
                idx.hash(&mut hasher);
                for w in windows {
                    (window_key(&w.window), w.rect.loc.x, w.rect.loc.y, w.rect.size.w, w.rect.size.h, &w.title, w.floating)
                        .hash(&mut hasher);
                }
            }
        }
        hover.slot.hash(&mut hasher);
        hover.window.as_ref().map(|(w, _)| window_key(&w.window)).hash(&mut hasher);
        press.as_ref().map(|p| p.window.as_ref().map(window_key)).hash(&mut hasher);
        if dragging {
            (local.x, local.y).hash(&mut hasher);
        }
        let key = hasher.finish();

        let name = output.name();
        if let Some((cached_key, buffer)) = self.overview.cache.get(&name)
            && *cached_key == key
        {
            return Some(buffer.clone());
        }

        let canvas = rasterize(&Scene {
            outputs: &snapshot,
            layout: &layout,
            view,
            pointer: on_output.then_some(local),
            press: press.as_ref(),
            dragging,
            hover: &hover,
            zoom: self.overview.zoom,
        });
        let buffer = MemoryRenderBuffer::from_slice(
            &canvas.pixels,
            Fourcc::Argb8888,
            (canvas.width as i32, canvas.height as i32),
            1,
            Transform::Normal,
            None,
        );
        self.overview.cache.insert(name, (key, buffer.clone()));
        Some(buffer)
    }

    /// The windows to draw live inside the overview, top-most first, each
    /// with the output-local rectangle its thumbnail fills. The window being
    /// dragged comes first, at the pointer.
    pub fn overview_thumbnails(&self, output: &Output) -> Vec<Thumb> {
        if !self.overview.is_visible() {
            return Vec::new();
        }
        let Some(geo) = self.space.output_geometry(output) else {
            return Vec::new();
        };
        let snapshot = self.overview_snapshot_for(output);
        let layout = compute_layout(&snapshot, geo.size, self.overview.scroll, self.config.workspaces.transition_axis, self.overview.zoom);

        let pointer_global = self.pointer.current_location();
        let local = (pointer_global - geo.loc.to_f64()).to_i32_round();
        let carried = self.overview.press.as_ref().filter(|p| {
            let d = pointer_global - p.start;
            p.window.is_some() && d.x.hypot(d.y) > DRAG_THRESHOLD
        });

        let mut out = Vec::new();
        if let Some(press) = carried
            && let Some(window) = press.window.clone()
        {
            out.push((window, Rectangle::new(local - press.grab, press.size), Rectangle::from_size(geo.size)));
        }
        let viewport = Rectangle::new(
            Point::from((0, TITLE_HEIGHT - 4)),
            Size::from((geo.size.w, geo.size.h - TITLE_HEIGHT + 4)),
        );
        // Slots cut off by the viewport show the part that's on screen.
        for slot in &layout.slots {
            let Some(clip) = viewport.intersection(slot.rect) else {
                continue;
            };
            let info = &snapshot[slot.output];
            let Some(windows) = info.workspaces.get(slot.workspace) else {
                continue;
            };
            // Later windows are on top.
            for w in windows.iter().rev() {
                if carried.is_some_and(|p| p.window.as_ref() == Some(&w.window)) {
                    continue;
                }
                out.push((w.window.clone(), window_rect_in(slot, info, w.rect), clip));
            }
        }
        out
    }

    fn overview_set_zoom(&mut self, zoom: f64) {
        self.overview.zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self.overview.cache.clear();
    }

    /// Called after the pointer moves: keeps a zoom-slider drag following it.
    pub(crate) fn overview_pointer_moved(&mut self) {
        if !self.overview.is_visible() || !self.overview.slider_drag {
            return;
        }
        let location = self.pointer.current_location();
        let Some(output) = self.space.output_under(location).next().cloned() else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        let x = (location.x - geo.loc.x as f64).round() as i32;
        self.overview_set_zoom(zoom_at(geo.size.w, x));
    }

    /// Handles a mouse button while the overview is up: a press starts a
    /// click-or-drag, the release finishes it - dropping a dragged window
    /// onto a workspace moves it there, a plain click activates what's under
    /// it, and a click on empty space closes the overview.
    pub(crate) fn overview_button(&mut self, pressed: bool, left: bool) {
        if !left {
            return;
        }
        let location = self.pointer.current_location();
        let Some(output) = self.space.output_under(location).next().cloned() else {
            if !pressed {
                self.overview.press = None;
            }
            return;
        };
        let geo = self.space.output_geometry(&output).unwrap_or_default();
        let local = (location - geo.loc.to_f64()).to_i32_round();
        if pressed && slider_rect(geo.size.w).contains(local) {
            self.overview.slider_drag = true;
            self.overview_set_zoom(zoom_at(geo.size.w, local.x));
            return;
        }
        if !pressed && self.overview.slider_drag {
            self.overview.slider_drag = false;
            return;
        }
        let snapshot = self.overview_snapshot_for(&output);
        let layout = compute_layout(&snapshot, geo.size, self.overview.scroll, self.config.workspaces.transition_axis, self.overview.zoom);
        let hit = layout.hit(&snapshot, local);
        let slot_id = hit
            .slot
            .map(|(oi, ws)| (snapshot[oi].name.clone(), ws));

        if pressed {
            self.overview.press = Some(Press {
                window: hit.window.as_ref().map(|(w, _)| w.window.clone()),
                slot: slot_id,
                start: location,
                grab: hit
                    .window
                    .as_ref()
                    .map(|(_, rect)| local - rect.loc)
                    .unwrap_or_default(),
                size: hit.window.as_ref().map(|(_, rect)| rect.size).unwrap_or_default(),
            });
            return;
        }

        let Some(press) = self.overview.press.take() else {
            return;
        };
        let delta = location - press.start;
        let moved = delta.x.hypot(delta.y) > DRAG_THRESHOLD;

        match press.window {
            Some(window) if moved => {
                if let Some((oi, ws)) = hit.slot {
                    let target = snapshot[oi].output.clone();
                    workspace::move_window_to(self, &window, &target, ws);
                }
            }
            Some(window) => {
                if let Some((home, idx)) = workspace::window_home(&window)
                    && idx != workspace::WorkspaceState::get(&home).active()
                {
                    workspace::activate_workspace(self, &home, idx);
                }
                if smithay::utils::IsAlive::alive(&window) {
                    crate::shell::tiling::raise_and_focus_no_warp(self, &window);
                }
                crate::ext_workspace::ext_workspace_sync(self);
                self.close_overview();
            }
            None => match (press.slot, slot_id, hit.slot) {
                (Some(pressed_on), Some(released_on), Some((oi, ws))) if pressed_on == released_on => {
                    let target = snapshot[oi].output.clone();
                    workspace::activate_workspace(self, &target, ws);
                    crate::ext_workspace::ext_workspace_sync(self);
                    self.close_overview();
                }
                (None, None, _) => self.close_overview(),
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(count: usize, w: i32, h: i32) -> OverviewOutput {
        // `output` is never touched by the layout code under test.
        OverviewOutput {
            output: Output::new(
                "test".into(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: String::new(),
                    model: String::new(),
                    serial_number: String::new(),
                },
            ),
            name: "test".into(),
            size: Size::from((w, h)),
            active: 0,
            count,
            workspaces: vec![Vec::new(); count],
        }
    }

    #[test]
    fn one_slot_per_workspace() {
        let outputs = [out(3, 1920, 1080)];
        assert_eq!(compute_layout(&outputs, Size::from((1920, 1080)), 0, WorkspaceTransitionAxis::Horizontal, 1.0).slots.len(), 3);
    }

    #[test]
    fn slots_keep_their_outputs_aspect_ratio_and_fit_the_view() {
        let outputs = [out(3, 1920, 1080), out(2, 1080, 1920)];
        let view = Size::from((1280, 720));
        let layout = compute_layout(&outputs, view, 0, WorkspaceTransitionAxis::Horizontal, 1.0);
        for slot in &layout.slots {
            assert!(slot.rect.loc.x >= 0 && slot.rect.loc.x + slot.rect.size.w <= view.w);
            let o = &outputs[slot.output];
            let want = o.size.w as f64 / o.size.h as f64;
            let got = slot.rect.size.w as f64 / slot.rect.size.h as f64;
            assert!((want - got).abs() < 0.05, "{want} vs {got}");
        }
    }

    #[test]
    fn hit_finds_the_slot_under_a_point() {
        let outputs = [out(2, 1600, 900)];
        let layout = compute_layout(&outputs, Size::from((1600, 900)), 0, WorkspaceTransitionAxis::Horizontal, 1.0);
        let second = layout.slots[1].rect;
        let hit = layout.hit(&outputs, second.loc + Point::from((2, 2)));
        assert_eq!(hit.slot, Some((0, 1)));
        assert!(layout.hit(&outputs, Point::from((0, 0))).slot.is_none());
    }

    #[test]
    fn window_rects_scale_into_their_slot() {
        let outputs = [out(1, 1000, 500)];
        let slot = Slot {
            output: 0,
            workspace: 0,
            rect: Rectangle::new(Point::from((100, 50)), Size::from((200, 100))),
        };
        let rect = window_rect_in(
            &slot,
            &outputs[0],
            Rectangle::new(Point::from((500, 250)), Size::from((500, 250))),
        );
        assert_eq!(rect, Rectangle::new(Point::from((200, 100)), Size::from((100, 50))));
    }

    #[test]
    fn horizontal_axis_lays_workspaces_out_in_one_row() {
        let outputs = [out(5, 1920, 1080)];
        let layout = compute_layout(&outputs, Size::from((1920, 1080)), 0, WorkspaceTransitionAxis::Horizontal, 1.0);
        let y = layout.slots[0].rect.loc.y;
        assert!(layout.slots.iter().all(|s| s.rect.loc.y == y));
        assert!(layout.slots.windows(2).all(|w| w[0].rect.loc.x < w[1].rect.loc.x));
    }

    #[test]
    fn vertical_axis_lays_workspaces_out_in_one_column() {
        let outputs = [out(5, 1920, 1080)];
        let layout = compute_layout(&outputs, Size::from((1920, 1080)), 0, WorkspaceTransitionAxis::Vertical, 1.0);
        let x = layout.slots[0].rect.loc.x;
        assert!(layout.slots.iter().all(|s| s.rect.loc.x == x));
        assert!(layout.slots.windows(2).all(|w| w[0].rect.loc.y < w[1].rect.loc.y));
    }

    #[test]
    fn many_workspaces_scroll_along_the_axis() {
        let outputs = [out(30, 1920, 1080)];
        let view = Size::from((1280, 720));
        for axis in [WorkspaceTransitionAxis::Horizontal, WorkspaceTransitionAxis::Vertical] {
            let top = compute_layout(&outputs, view, 0, axis, 1.0);
            assert!(top.max_scroll > 0);
            assert!(top.slots.iter().all(|s| s.rect.size.w >= MIN_SCROLL_SLOT_WIDTH));
            let end = compute_layout(&outputs, view, i32::MAX, axis, 1.0);
            assert_eq!(end.scroll, end.max_scroll);
            let (a, b) = (top.slots[0].rect.loc, end.slots[0].rect.loc);
            match axis {
                WorkspaceTransitionAxis::Horizontal => assert!(b.x < a.x && b.y == a.y),
                WorkspaceTransitionAxis::Vertical => assert!(b.y < a.y && b.x == a.x),
            }
        }
    }

    #[test]
    fn zoom_scales_thumbnails_and_the_slider_maps_its_ends() {
        let outputs = [out(4, 1920, 1080)];
        let view = Size::from((1920, 1080));
        let axis = WorkspaceTransitionAxis::Horizontal;
        let w = |zoom| compute_layout(&outputs, view, 0, axis, zoom).slots[0].rect.size.w;
        assert!(w(0.5) < w(1.0) && w(1.0) < w(1.5));
        let slider = slider_rect(view.w);
        assert_eq!(zoom_at(view.w, slider.loc.x), ZOOM_MIN);
        assert_eq!(zoom_at(view.w, slider.loc.x + slider.size.w), ZOOM_MAX);
    }

    #[test]
    fn plural_labels() {
        assert_eq!(plural(1), "1 workspace");
        assert_eq!(plural(4), "4 workspaces");
    }
}
