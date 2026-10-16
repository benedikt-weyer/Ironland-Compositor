//! Automatic BSP ("dwindle"-style) tiling, similar to Hyprland's default layout.
//!
//! Each output owns its own tree of splits. Every tiled window is a leaf; mapping a
//! new window splits the currently focused leaf in two, and closing/untiling a
//! window collapses its sibling back into the parent's slot. Windows can be
//! individually floated (`toggle_floating`) to opt out of the layout entirely.

use std::cell::{Ref, RefCell, RefMut};
use std::time::{Duration, Instant};

use smithay::{
    backend::input::InputTime,
    desktop::{Space, layer_map_for_output, space::SpaceElement},
    input::pointer::MotionEvent,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Size},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};

use crate::{
    focus::KeyboardFocusTarget,
    shell::workspace::WorkspaceState,
    state::{AnvilState, Backend},
};

use super::{FullscreenSurface, WindowElement};

/// How much a keyboard-driven resize changes a split's ratio per key press.
const RESIZE_STEP: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

enum Node {
    Leaf(WindowElement),
    Split {
        /// `true` = children sit side by side (a split along x); `false` = stacked (split along y).
        vertical: bool,
        /// Fraction of the area given to `a`.
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

#[derive(Default)]
pub struct TilingLayout {
    root: Option<Node>,
    /// The most recently inserted/targeted leaf, used as the split target when
    /// there is no focused window to split against.
    last: Option<WindowElement>,
}

impl TilingLayout {
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn contains(&self, window: &WindowElement) -> bool {
        fn walk(node: &Node, window: &WindowElement) -> bool {
            match node {
                Node::Leaf(w) => w == window,
                Node::Split { a, b, .. } => walk(a, window) || walk(b, window),
            }
        }
        self.root.as_ref().is_some_and(|n| walk(n, window))
    }

    pub fn windows(&self) -> Vec<WindowElement> {
        fn walk(node: &Node, out: &mut Vec<WindowElement>) {
            match node {
                Node::Leaf(w) => out.push(w.clone()),
                Node::Split { a, b, .. } => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk(root, &mut out);
        }
        out
    }

    /// Insert `window`, splitting the leaf for `target` (or the last-touched
    /// leaf, or an arbitrary leaf) in half.
    pub fn insert(&mut self, window: WindowElement, area: Rectangle<i32, Logical>, target: Option<&WindowElement>) {
        let Some(root) = self.root.take() else {
            self.root = Some(Node::Leaf(window.clone()));
            self.last = Some(window);
            return;
        };

        // Resolve the split target against the *previous* `last` - it must
        // not be overwritten with `window` before this, or a `None` target
        // on a non-empty tree would try to split `window` against itself,
        // which isn't in the tree yet, silently dropping it (`replace_leaf`
        // finds no matching leaf and hands the tree back unchanged).
        let target = target
            .filter(|t| Self::tree_contains(&root, t))
            .cloned()
            .or_else(|| self.last.clone())
            .unwrap_or_else(|| Self::first_leaf(&root).clone());
        self.last = Some(window.clone());

        // Gap doesn't matter for this orientation heuristic (only which
        // dimension is larger), so 0 avoids threading config through insert().
        let target_rect = Self::layout_rec_rect(&root, area, 0, &target).unwrap_or(area);
        let vertical = target_rect.size.w >= target_rect.size.h;

        let new_root = Self::replace_leaf(
            root,
            &target,
            Node::Split {
                vertical,
                ratio: 0.5,
                a: Box::new(Node::Leaf(target.clone())),
                b: Box::new(Node::Leaf(window)),
            },
        );
        self.root = Some(new_root);
    }

    /// Insert `window` as `target`'s new sibling, splitting `target`'s tile
    /// so `window` takes the half `direction` points to (`Left`/`Up` put it
    /// first, `Right`/`Down` second). Falls back to the generic [`insert`]
    /// heuristic if `target` isn't tiled here any more (e.g. closed
    /// mid-drag) - used to land a window dragged onto another one, with
    /// `direction` picked from where in `target`'s tile it was dropped.
    ///
    /// [`insert`]: Self::insert
    pub fn insert_beside(
        &mut self,
        window: WindowElement,
        area: Rectangle<i32, Logical>,
        target: &WindowElement,
        direction: Direction,
    ) {
        let Some(root) = self.root.take() else {
            self.root = Some(Node::Leaf(window.clone()));
            self.last = Some(window);
            return;
        };
        if !Self::tree_contains(&root, target) {
            self.root = Some(root);
            self.insert(window, area, None);
            return;
        }

        self.last = Some(window.clone());
        let vertical = matches!(direction, Direction::Left | Direction::Right);
        let window_first = matches!(direction, Direction::Left | Direction::Up);
        let new_leaf = Node::Leaf(window);
        let target_leaf = Node::Leaf(target.clone());
        let (a, b) = if window_first {
            (new_leaf, target_leaf)
        } else {
            (target_leaf, new_leaf)
        };
        self.root = Some(Self::replace_leaf(
            root,
            target,
            Node::Split {
                vertical,
                ratio: 0.5,
                a: Box::new(a),
                b: Box::new(b),
            },
        ));
    }

    fn tree_contains(node: &Node, window: &WindowElement) -> bool {
        match node {
            Node::Leaf(w) => w == window,
            Node::Split { a, b, .. } => Self::tree_contains(a, window) || Self::tree_contains(b, window),
        }
    }

    fn first_leaf(node: &Node) -> &WindowElement {
        match node {
            Node::Leaf(w) => w,
            Node::Split { a, .. } => Self::first_leaf(a),
        }
    }

    /// Replace the leaf holding `target` with `replacement`. `target` is assumed
    /// to be present in `node`.
    fn replace_leaf(node: Node, target: &WindowElement, replacement: Node) -> Node {
        match node {
            Node::Leaf(w) => {
                if &w == target {
                    replacement
                } else {
                    Node::Leaf(w)
                }
            }
            Node::Split { vertical, ratio, a, b } => {
                if Self::tree_contains(&a, target) {
                    Node::Split {
                        vertical,
                        ratio,
                        a: Box::new(Self::replace_leaf(*a, target, replacement)),
                        b,
                    }
                } else {
                    Node::Split {
                        vertical,
                        ratio,
                        a,
                        b: Box::new(Self::replace_leaf(*b, target, replacement)),
                    }
                }
            }
        }
    }

    /// Remove `window` from the tree, collapsing its sibling into its slot.
    pub fn remove(&mut self, window: &WindowElement) -> bool {
        if self.last.as_ref() == Some(window) {
            self.last = None;
        }
        let Some(root) = self.root.take() else {
            return false;
        };
        let (new_root, found) = Self::remove_rec(root, window);
        self.root = new_root;
        if found {
            set_tile_clip(window, None);
        }
        found
    }

    fn remove_rec(node: Node, target: &WindowElement) -> (Option<Node>, bool) {
        match node {
            Node::Leaf(w) => {
                if &w == target {
                    (None, true)
                } else {
                    (Some(Node::Leaf(w)), false)
                }
            }
            Node::Split { vertical, ratio, a, b } => {
                let (new_a, found_a) = Self::remove_rec(*a, target);
                if found_a {
                    return match new_a {
                        None => (Some(*b), true),
                        Some(na) => (Some(Node::Split { vertical, ratio, a: Box::new(na), b }), true),
                    };
                }
                // Not found under `a`; `remove_rec` hands back the subtree unchanged.
                let a = Box::new(new_a.expect("unchanged subtree is always returned"));
                let (new_b, found_b) = Self::remove_rec(*b, target);
                if found_b {
                    return match new_b {
                        None => (Some(*a), true),
                        Some(nb) => (Some(Node::Split { vertical, ratio, a, b: Box::new(nb) }), true),
                    };
                }
                let b = Box::new(new_b.expect("unchanged subtree is always returned"));
                (Some(Node::Split { vertical, ratio, a, b }), false)
            }
        }
    }

    /// Swap the positions of two tiled windows in the tree.
    pub fn swap(&mut self, a: &WindowElement, b: &WindowElement) {
        fn walk(node: &mut Node, a: &WindowElement, b: &WindowElement) {
            match node {
                Node::Leaf(w) => {
                    if w == a {
                        *w = b.clone();
                    } else if w == b {
                        *w = a.clone();
                    }
                }
                Node::Split { a: na, b: nb, .. } => {
                    walk(na, a, b);
                    walk(nb, a, b);
                }
            }
        }
        if let Some(root) = &mut self.root {
            walk(root, a, b);
        }
    }

    /// Grow (positive `delta`) or shrink (negative) the focused window's share
    /// of the nearest ancestor split matching `vertical`.
    pub fn adjust_ratio(&mut self, window: &WindowElement, vertical: bool, delta: f32) {
        fn walk(node: &mut Node, target: &WindowElement, vertical: bool, delta: f32) -> (bool, bool) {
            match node {
                Node::Leaf(w) => (&*w == target, false),
                Node::Split { vertical: v, ratio, a, b } => {
                    let (in_a, done_a) = walk(a, target, vertical, delta);
                    if in_a {
                        if !done_a && *v == vertical {
                            *ratio = (*ratio + delta).clamp(0.1, 0.9);
                            return (true, true);
                        }
                        return (true, done_a);
                    }
                    let (in_b, done_b) = walk(b, target, vertical, delta);
                    if in_b {
                        if !done_b && *v == vertical {
                            *ratio = (*ratio - delta).clamp(0.1, 0.9);
                            return (true, true);
                        }
                        return (true, done_b);
                    }
                    (false, false)
                }
            }
        }
        if let Some(root) = &mut self.root {
            walk(root, window, vertical, delta);
        }
    }

    /// Remove any windows that are no longer alive. Returns `true` if the tree changed.
    pub fn retain_alive(&mut self) -> bool {
        let dead: Vec<_> = self.windows().into_iter().filter(|w| !w.alive()).collect();
        let mut changed = false;
        for window in dead {
            changed |= self.remove(&window);
        }
        changed
    }

    /// Compute the on-screen rectangle for every tiled window within `area`,
    /// separated by `gap` logical pixels.
    pub fn layout(&self, area: Rectangle<i32, Logical>, gap: i32) -> Vec<(WindowElement, Rectangle<i32, Logical>)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            Self::layout_rec(root, area, gap, &mut out);
        }
        out
    }

    /// Split `area` into the two child areas a split with this orientation/ratio/gap
    /// produces. Shared by [`Self::layout_rec`] (rendering) and
    /// [`Self::resize_border`] (interactively dragging a split's border) so
    /// the two always agree on where a border actually sits on screen.
    fn split_areas(
        area: Rectangle<i32, Logical>,
        vertical: bool,
        ratio: f32,
        gap: i32,
    ) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
        let half_gap = gap / 2;
        if vertical {
            let wa = ((area.size.w as f32) * ratio) as i32;
            (
                Rectangle::new(area.loc, (wa - half_gap, area.size.h).into()),
                Rectangle::new(
                    Point::from((area.loc.x + wa + half_gap, area.loc.y)),
                    (area.size.w - wa - half_gap, area.size.h).into(),
                ),
            )
        } else {
            let ha = ((area.size.h as f32) * ratio) as i32;
            (
                Rectangle::new(area.loc, (area.size.w, ha - half_gap).into()),
                Rectangle::new(
                    Point::from((area.loc.x, area.loc.y + ha + half_gap)),
                    (area.size.w, area.size.h - ha - half_gap).into(),
                ),
            )
        }
    }

    fn layout_rec(
        node: &Node,
        area: Rectangle<i32, Logical>,
        gap: i32,
        out: &mut Vec<(WindowElement, Rectangle<i32, Logical>)>,
    ) {
        match node {
            Node::Leaf(w) => out.push((w.clone(), area)),
            Node::Split { vertical, ratio, a, b } => {
                let (area_a, area_b) = Self::split_areas(area, *vertical, *ratio, gap);
                Self::layout_rec(a, area_a, gap, out);
                Self::layout_rec(b, area_b, gap, out);
            }
        }
    }

    /// Live-drag the border of the nearest ancestor split of `vertical`
    /// orientation whose border actually touches the edge being dragged -
    /// the split where `window` sits on the `want_a` side (`true` = the
    /// left/top `a` side, whose *far* edge is the border; `false` = the
    /// right/bottom `b` side, whose *near* edge is the border). Splits of
    /// matching orientation where `window` sits on the other side are
    /// skipped over (their border is the *other* edge of `window`, not this
    /// one) rather than stopping the search, so it walks out through the
    /// tree until it finds the split this edge is actually shared with.
    ///
    /// `coord` is the pointer's position along the resized axis (x for a
    /// vertical split, y for horizontal); `area`/`gap` must match whatever
    /// [`Self::layout`] was last called with. Returns `false` (changing
    /// nothing) if no such split exists - the dragged edge faces the output
    /// boundary rather than a sibling tile, so there is nothing to push.
    pub fn resize_border(
        &mut self,
        window: &WindowElement,
        vertical: bool,
        want_a: bool,
        area: Rectangle<i32, Logical>,
        gap: i32,
        coord: f64,
    ) -> bool {
        fn walk(
            node: &mut Node,
            area: Rectangle<i32, Logical>,
            gap: i32,
            target: &WindowElement,
            vertical: bool,
            want_a: bool,
            coord: f64,
        ) -> (bool, bool) {
            match node {
                Node::Leaf(w) => (&*w == target, false),
                Node::Split { vertical: v, ratio, a, b } => {
                    let (area_a, area_b) = TilingLayout::split_areas(area, *v, *ratio, gap);
                    let (in_a, done_a) = walk(a, area_a, gap, target, vertical, want_a, coord);
                    if in_a {
                        if !done_a && *v == vertical && want_a {
                            *ratio = TilingLayout::border_ratio(area, vertical, coord);
                            return (true, true);
                        }
                        return (true, done_a);
                    }
                    let (in_b, done_b) = walk(b, area_b, gap, target, vertical, want_a, coord);
                    if in_b {
                        if !done_b && *v == vertical && !want_a {
                            *ratio = TilingLayout::border_ratio(area, vertical, coord);
                            return (true, true);
                        }
                        return (true, done_b);
                    }
                    (false, false)
                }
            }
        }
        let Some(root) = &mut self.root else {
            return false;
        };
        walk(root, area, gap, window, vertical, want_a, coord).1
    }

    /// The split ratio that puts the border at `coord` within `area` along
    /// the resized axis.
    fn border_ratio(area: Rectangle<i32, Logical>, vertical: bool, coord: f64) -> f32 {
        let (origin, span) = if vertical {
            (area.loc.x as f64, area.size.w.max(1) as f64)
        } else {
            (area.loc.y as f64, area.size.h.max(1) as f64)
        };
        (((coord - origin) / span) as f32).clamp(0.1, 0.9)
    }

    fn layout_rec_rect(
        node: &Node,
        area: Rectangle<i32, Logical>,
        gap: i32,
        target: &WindowElement,
    ) -> Option<Rectangle<i32, Logical>> {
        let mut rects = Vec::new();
        Self::layout_rec(node, area, gap, &mut rects);
        rects.into_iter().find(|(w, _)| w == target).map(|(_, r)| r)
    }
}

/// One window's in-flight "grow in" (newly tiled) or "reflow" (a sibling
/// resizing/moving because another tile was inserted or removed) animation:
/// purely a render-time transform (see `render::output_elements`) - the
/// window's real, protocol-level size is always its final settled rect from
/// the moment `apply_layout_animated` maps it; only how it's *drawn* eases
/// from `from` to `to` in the meantime.
#[derive(Debug, Clone)]
struct RectAnim {
    from: Rectangle<i32, Logical>,
    to: Rectangle<i32, Logical>,
    start: Instant,
    duration: Duration,
    /// True only for a window that wasn't mapped anywhere before (a
    /// brand-new tile, not just a reflowing sibling) - fades in on top of
    /// the shrink/grow, so a resizing sibling doesn't flicker translucent.
    fade_in: bool,
}

/// The tile rect a tiled window is currently confined to, stored in its
/// user data so the render path (which only sees the `Space`) can crop
/// whatever the client draws beyond it - a client may refuse to shrink below
/// its own minimum size, or lag behind a configure.
#[derive(Default)]
struct TileClip(RefCell<Option<Rectangle<i32, Logical>>>);

fn set_tile_clip(window: &WindowElement, rect: Option<Rectangle<i32, Logical>>) {
    window.user_data().insert_if_missing(TileClip::default);
    if let Some(clip) = window.user_data().get::<TileClip>() {
        *clip.0.borrow_mut() = rect;
    }
}

/// The global-logical rect `window` must not draw outside of, if it's tiled.
pub(crate) fn tile_clip(window: &WindowElement) -> Option<Rectangle<i32, Logical>> {
    window.user_data().get::<TileClip>().and_then(|clip| *clip.0.borrow())
}

/// Per-output registry of [`RectAnim`]s, stored the same way as
/// [`crate::shell::workspace::WorkspaceTransitionSlot`] - interior-mutable
/// output user data, so it's reachable from `render.rs`'s otherwise-`&self`
/// render path.
#[derive(Default)]
struct TilingAnimSlot(RefCell<Vec<(WindowElement, RectAnim)>>);

impl TilingAnimSlot {
    fn get(output: &Output) -> &TilingAnimSlot {
        output.user_data().insert_if_missing(TilingAnimSlot::default);
        output.user_data().get::<TilingAnimSlot>().unwrap()
    }
}

/// A slightly-shrunk, centered version of `rect` - the starting point for a
/// brand-new tile's "grow in" animation.
fn shrink_centered(rect: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    const START_FRACTION: f32 = 0.82;
    let new_w = ((rect.size.w as f32) * START_FRACTION).round() as i32;
    let new_h = ((rect.size.h as f32) * START_FRACTION).round() as i32;
    let dx = (rect.size.w - new_w) / 2;
    let dy = (rect.size.h - new_h) / 2;
    Rectangle::new(Point::from((rect.loc.x + dx, rect.loc.y + dy)), (new_w, new_h).into())
}

/// Eases out with no overshoot - used for the fade-in alpha of a brand-new
/// tile, where any overshoot (a bounce past fully opaque) isn't meaningful.
fn ease_out_cubic(t: f32) -> f32 {
    let t = t - 1.0;
    t * t * t + 1.0
}

/// Eases out with a bit of overshoot past the target before settling
/// exactly on it at `t = 1.0` - the standard "back out" curve, tuned down
/// from its usual default (`1.70158`) for a subtler bounce.
fn ease_out_back(t: f32) -> f32 {
    const C1: f32 = 1.4;
    const C3: f32 = C1 + 1.0;
    let t = t - 1.0;
    1.0 + C3 * t * t * t + C1 * t * t
}

fn lerp_rect(from: Rectangle<i32, Logical>, to: Rectangle<i32, Logical>, t: f32) -> Rectangle<i32, Logical> {
    let lerp = |a: i32, b: i32| (a as f32 + (b - a) as f32 * t).round() as i32;
    Rectangle::new(
        Point::from((lerp(from.loc.x, to.loc.x), lerp(from.loc.y, to.loc.y))),
        (lerp(from.size.w, to.size.w), lerp(from.size.h, to.size.h)).into(),
    )
}

/// Registers (or refreshes) `window`'s reflow animation on `output`: it just
/// moved/resized from `previous` (`None` for a window that wasn't mapped
/// anywhere before - it grows in from a shrunk version of `to` instead) to
/// `to`. A no-op if `previous == Some(to)` already (nothing actually
/// changed - the overwhelming common case, so this stays cheap).
fn start_reflow(
    output: &Output,
    window: &WindowElement,
    previous: Option<Rectangle<i32, Logical>>,
    to: Rectangle<i32, Logical>,
    duration: Duration,
) {
    let (from, fade_in) = match previous {
        Some(from) if from == to => return,
        Some(from) => (from, false),
        None => (shrink_centered(to), true),
    };
    let mut anims = TilingAnimSlot::get(output).0.borrow_mut();
    anims.retain(|(w, _)| w != window);
    anims.push((
        window.clone(),
        RectAnim {
            from,
            to,
            start: Instant::now(),
            duration,
            fade_in,
        },
    ));
}

/// `window`'s current eased (rect, alpha) if it has an in-flight animation
/// on `output`, `None` otherwise (the common case, once the animation's
/// `duration` has elapsed or it was never registered) - `render.rs` renders
/// it scaled/relocated into `rect` at `alpha` instead of at its real,
/// settled position when this returns `Some`. Also prunes any entry that
/// has finished or gone stale (dead window) as a side effect, so nothing
/// needs to sweep this separately once a frame.
pub(crate) fn animated_rect(output: &Output, window: &WindowElement) -> Option<(Rectangle<i32, Logical>, f32)> {
    let mut anims = TilingAnimSlot::get(output).0.borrow_mut();
    anims.retain(|(w, a)| w.alive() && a.start.elapsed() < a.duration);
    let (_, anim) = anims.iter().find(|(w, _)| w == window)?;
    let t = (anim.start.elapsed().as_secs_f32() / anim.duration.as_secs_f32().max(0.001)).clamp(0.0, 1.0);
    let rect = lerp_rect(anim.from, anim.to, ease_out_back(t));
    let alpha = if anim.fade_in { ease_out_cubic(t) } else { 1.0 };
    Some((rect, alpha))
}

/// Every one of an output's tiling trees, one per workspace, indexed by
/// workspace number. Grows lazily as higher workspace indices are touched;
/// entries are never removed (an empty [`TilingLayout`] is cheap and
/// `retain_alive`/pruning elsewhere deals with staleness).
#[derive(Default)]
pub struct TilingState(RefCell<Vec<TilingLayout>>);

impl TilingState {
    fn get(output: &Output) -> &TilingState {
        output.user_data().insert_if_missing(TilingState::default);
        output.user_data().get::<TilingState>().unwrap()
    }

    fn ensure_len(&self, idx: usize) {
        let mut v = self.0.borrow_mut();
        if v.len() <= idx {
            v.resize_with(idx + 1, TilingLayout::default);
        }
    }

    /// Borrows workspace `idx`'s tiling tree for `output`, growing storage as needed.
    pub fn tree(output: &Output, idx: usize) -> Ref<'_, TilingLayout> {
        let state = Self::get(output);
        state.ensure_len(idx);
        Ref::map(state.0.borrow(), |v| &v[idx])
    }

    /// Mutably borrows workspace `idx`'s tiling tree for `output`, growing storage as needed.
    pub fn tree_mut(output: &Output, idx: usize) -> RefMut<'_, TilingLayout> {
        let state = Self::get(output);
        state.ensure_len(idx);
        RefMut::map(state.0.borrow_mut(), |v| &mut v[idx])
    }

    /// Every workspace's tiling tree for `output`, in workspace-index order.
    pub fn all(output: &Output) -> Ref<'_, Vec<TilingLayout>> {
        Self::get(output).0.borrow()
    }

    /// How many workspace slots have been touched for `output` so far.
    pub fn len(output: &Output) -> usize {
        Self::get(output).0.borrow().len()
    }
}

pub(crate) fn tiling_area(space: &Space<WindowElement>, output: &Output, outer_gap: i32) -> Rectangle<i32, Logical> {
    let geo = space.output_geometry(output).unwrap_or_default();
    let map = layer_map_for_output(output);
    let zone = map.non_exclusive_zone();
    let outer_gap = outer_gap.max(0);
    let loc = geo.loc + zone.loc + Point::from((outer_gap, outer_gap));
    let size = Size::from((
        (zone.size.w - 2 * outer_gap).max(0),
        (zone.size.h - 2 * outer_gap).max(0),
    ));
    Rectangle::new(loc, size)
}

/// Finds which output's tiling tree (and at which workspace index) contains
/// `window`, searching every workspace, not just the active one - a window
/// can be tiled on a hidden workspace (e.g. its client closed it in the
/// background).
pub(crate) fn locate<BackendData: Backend>(
    state: &AnvilState<BackendData>,
    window: &WindowElement,
) -> Option<(Output, usize)> {
    for output in state.all_outputs() {
        for (idx, layout) in TilingState::all(output).iter().enumerate() {
            if layout.contains(window) {
                return Some((output.clone(), idx));
            }
        }
    }
    None
}

pub(crate) fn current_focused_window<BackendData: Backend>(
    state: &AnvilState<BackendData>,
) -> Option<WindowElement> {
    let keyboard = state.seat.get_keyboard()?;
    match keyboard.current_focus()? {
        KeyboardFocusTarget::Window(w) => Some(WindowElement(w)),
        _ => None,
    }
}

/// Re-applies a tiled window's own layout on every commit of its surface.
///
/// A client is supposed to honor a configure's suggested size, but not every
/// toolkit's very first (pre-map) commit reliably does - some render at
/// their own preferred/minimum content size regardless of what was
/// suggested, only picking up server-driven resizes correctly once they're
/// already mapped. Re-checking here, on each real commit, makes the tile the
/// window was assigned the source of truth continuously rather than only at
/// insert time: `apply_layout` already no-ops (no configure sent) once the
/// window's pending size already matches its tile, so this is cheap once
/// the window has caught up.
///
/// `xdg_toplevel`'s configure size is only ever advisory though - a client
/// is free to commit something else, and one that has advertised its own
/// `min_size` (via `set_min_size`, only known once it actually commits, so
/// this can't be caught any earlier e.g. at `should_tile` time) will never
/// honor a configure asking it to go smaller than that. Forcing such a
/// window to stay in a tile it has declared it can't fit would just have it
/// permanently overflow that tile's bounds, so it's untiled to float instead,
/// the closest the compositor can get to "hard limit to the screen" without
/// shrinking the window past a size it has said it cannot render at.
pub(crate) fn resync_committed_window<BackendData: Backend>(state: &mut AnvilState<BackendData>, surface: &WlSurface) {
    let Some(window) = state.window_for_surface(surface) else {
        return;
    };
    let Some((output, idx)) = locate(state, &window) else {
        return;
    };

    if let Some(assigned) = assigned_tile_rect(state, &window, &output, idx) {
        let min = window_min_size(&window);
        if (min.w > 0 && min.w > assigned.size.w) || (min.h > 0 && min.h > assigned.size.h) {
            untile_window(state, &window);
            center_on_output(state, &window, &output);
            return;
        }
    }

    apply_layout(state, &output);
}

/// The rect `window`'s tiling tree currently assigns it, if it's tiled.
fn assigned_tile_rect<BackendData: Backend>(
    state: &AnvilState<BackendData>,
    window: &WindowElement,
    output: &Output,
    idx: usize,
) -> Option<Rectangle<i32, Logical>> {
    let area = tiling_area(&state.space, output, state.config.gaps.outer as i32);
    TilingState::tree(output, idx)
        .layout(area, state.config.gaps.inner as i32)
        .into_iter()
        .find_map(|(w, rect)| (&w == window).then_some(rect))
}

fn window_min_size(window: &WindowElement) -> smithay::utils::Size<i32, Logical> {
    #[allow(irrefutable_let_patterns)]
    if let Some(toplevel) = window.0.toplevel() {
        return with_states(toplevel.wl_surface(), |states| {
            states.cached_state.get::<SurfaceCachedState>().current().min_size
        });
    }
    #[cfg(feature = "xwayland")]
    if let Some(xsurface) = window.0.x11_surface() {
        return xsurface.min_size().unwrap_or_default();
    }
    (0, 0).into()
}

/// Centers `window` (assumed already mapped, e.g. just untiled) on `output`,
/// clamped so it starts fully on-screen even when its own size is larger
/// than the output - the compositor can reposition it, it just can't force
/// it smaller than the minimum size the client itself has declared.
pub(crate) fn center_on_output<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    window: &WindowElement,
    output: &Output,
) {
    let Some(output_geo) = state.space.output_geometry(output) else {
        return;
    };
    let size = window.geometry().size;
    let x = output_geo.loc.x + ((output_geo.size.w - size.w) / 2).max(0);
    let y = output_geo.loc.y + ((output_geo.size.h - size.h) / 2).max(0);
    state.space.map_element(window.clone(), (x, y), false);
}

pub(crate) fn raise_and_focus<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) {
    raise_and_focus_impl(state, window);
    warp_pointer_to(state, window);
}

/// Like [`raise_and_focus`], but without the `mouse_follows_focus` pointer
/// warp - for callers that are themselves handling a pointer event (a drag
/// grab's `unset()`), where warping would re-enter the pointer's own
/// motion/frame dispatch and deadlock on its internal lock. See
/// [`warp_pointer_to`]'s doc for the invariant this preserves.
pub(crate) fn raise_and_focus_no_warp<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) {
    raise_and_focus_impl(state, window);
}

fn raise_and_focus_impl<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) {
    state.space.raise_element(window, true);
    if let Some(keyboard) = state.seat.get_keyboard() {
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(state, Some(window.clone().into()), serial);
    }
}

/// Moves the pointer to the center of `window`, if `focus.mouse_follows_focus`
/// is enabled. Every caller of [`raise_and_focus`] is a focus change that
/// didn't originate from the pointer itself (a workspace switch, cycling
/// windows, a newly mapped window taking focus, activating a window from the
/// dock) - `focus.follows_mouse` only ever runs from real pointer-motion
/// input events (see `input_handler.rs`), so the two settings can't end up
/// fighting each other through this. This also isn't just a style
/// preference: `pointer.motion()`/`pointer.frame()` below re-enter the
/// pointer's own (non-reentrant) internal lock, so calling this from code
/// that's itself running inside that lock - e.g. a drag grab's `unset()` -
/// deadlocks. Such callers must use [`raise_and_focus_no_warp`] instead.
fn warp_pointer_to<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) {
    if !state.config.focus.mouse_follows_focus {
        return;
    }
    let Some(geo) = state.space.element_geometry(window) else {
        return;
    };
    let center = Point::<i32, Logical>::from((geo.loc.x + geo.size.w / 2, geo.loc.y + geo.size.h / 2))
        .to_f64();

    let pointer = state.pointer.clone();
    pointer.set_location(center);
    let under = state.surface_under(center);
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location: center,
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(state);
}

/// Re-flow every tiled window on `output`'s *active* workspace to match its
/// current tree. Hidden workspaces are left untouched (and are up to date
/// whenever they're next shown by [`crate::shell::workspace`]).
pub fn apply_layout<BackendData: Backend>(state: &mut AnvilState<BackendData>, output: &Output) {
    apply_layout_impl(state, output, false);
}

/// Like [`apply_layout`], but also registers a bouncy grow-in/reflow
/// animation (see [`start_reflow`]) for every window whose rect actually
/// changed - used by the handful of call sites that correspond to a window
/// genuinely entering or leaving the tiling layout (as opposed to e.g. a
/// workspace switch, which already has its own dedicated slide transition -
/// see [`crate::shell::workspace::advance_transitions`] - and would fight
/// with this one if both applied to the same reflow).
pub(crate) fn apply_layout_animated<BackendData: Backend>(state: &mut AnvilState<BackendData>, output: &Output) {
    apply_layout_impl(state, output, true);
}

fn apply_layout_impl<BackendData: Backend>(state: &mut AnvilState<BackendData>, output: &Output, animate: bool) {
    let idx = WorkspaceState::get(output).active();
    let area = tiling_area(&state.space, output, state.config.gaps.outer as i32);
    let rects = TilingState::tree(output, idx).layout(area, state.config.gaps.inner as i32);
    let fullscreen = output
        .user_data()
        .get::<FullscreenSurface>()
        .and_then(|f| f.get());
    let anim_duration = (animate
        && state.config.window_animations.enabled
        && state.config.window_animations.duration_ms > 0)
        .then(|| Duration::from_millis(u64::from(state.config.window_animations.duration_ms)));
    for (window, rect) in rects {
        // A fullscreen window's toplevel size is driven by
        // `render::output_elements`/`shell::xdg`/`shell::x11`, not by its
        // tile slot - resizing it down here would fight whatever configure
        // made it fullscreen in the first place.
        set_tile_clip(&window, (fullscreen.as_ref() != Some(&window)).then_some(rect));
        if fullscreen.as_ref() != Some(&window) {
            if let Some(duration) = anim_duration {
                let previous = state.space.element_geometry(&window);
                start_reflow(output, &window, previous, rect, duration);
            }
            #[allow(irrefutable_let_patterns)]
            if let Some(toplevel) = window.0.toplevel() {
                let changed = toplevel.with_pending_state(|s| {
                    if s.size != Some(rect.size) {
                        s.size = Some(rect.size);
                        true
                    } else {
                        false
                    }
                });
                if changed && toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
            #[cfg(feature = "xwayland")]
            if let Some(xsurface) = window.0.x11_surface() {
                let _ = xsurface.configure(Some(rect));
            }
        }
        state.space.map_element(window, rect.loc, false);
    }
}

/// Should this newly-created toplevel participate in tiling at all?
/// Dialogs (windows with a parent) and fixed-size utility windows stay floating.
pub fn should_tile(window: &WindowElement) -> bool {
    #[allow(irrefutable_let_patterns)]
    let Some(toplevel) = window.0.toplevel() else {
        return false;
    };
    if toplevel.parent().is_some() {
        return false;
    }
    let (min, max) = with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let state = cached.current();
        (state.min_size, state.max_size)
    });
    let fixed_size = min.w > 0 && min.h > 0 && min == max;
    !fixed_size
}

/// [`should_tile`]'s X11 counterpart: XWayland windows have no xdg toplevel,
/// so the parent/transient-for relationship, fixed-size hint and window
/// type live on the `X11Surface` instead. Dialogs, utility/menu/splash-type
/// windows and fixed-size windows stay floating, same as their Wayland
/// counterparts.
#[cfg(feature = "xwayland")]
pub fn should_tile_x11(xsurface: &smithay::xwayland::X11Surface) -> bool {
    if xsurface.is_transient_for().is_some() {
        return false;
    }
    if !matches!(
        xsurface.window_type(),
        None | Some(smithay::xwayland::xwm::WmWindowType::Normal)
    ) {
        return false;
    }
    let min = xsurface.min_size();
    let max = xsurface.max_size();
    let fixed_size = matches!((min, max), (Some(min), Some(max)) if min.w > 0 && min.h > 0 && min == max);
    !fixed_size
}

/// Insert a newly mapped window into the tiling tree of the output it belongs on.
pub fn tile_new_window<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    window: &WindowElement,
    pointer_location: Point<f64, Logical>,
) {
    let Some(output) = state
        .space
        .output_under(pointer_location)
        .next()
        .or_else(|| state.space.outputs().next())
        .cloned()
    else {
        return;
    };

    let idx = WorkspaceState::get(&output).active();
    insert_into_tree(state, window, &output, idx);
}

/// Shared tail of [`tile_new_window`] and [`retile_at_home`]: inserts
/// `window` into `output`'s workspace `idx` tiling tree and reflows it.
fn insert_into_tree<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    window: &WindowElement,
    output: &Output,
    idx: usize,
) {
    let target = current_focused_window(state).filter(|w| TilingState::tree(output, idx).contains(w));

    crate::shell::workspace::assign_new_window(window, output, false);

    let area = tiling_area(&state.space, output, state.config.gaps.outer as i32);
    TilingState::tree_mut(output, idx).insert(window.clone(), area, target.as_ref());

    apply_layout_animated(state, output);
    raise_and_focus(state, window);
}

/// Like [`tile_new_window`], but for a window that's already floating
/// somewhere: re-tiles it onto whichever output/workspace it's currently
/// homed to (see [`crate::shell::workspace::window_home`]) rather than
/// wherever the pointer happens to be, so re-tiling a window on an inactive
/// workspace (e.g. from a dock context menu) doesn't relocate it onto the
/// output's active one. Falls back to [`tile_new_window`] at the pointer if
/// the window has no home yet.
fn retile_at_home<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) {
    match crate::shell::workspace::window_home(window) {
        Some((output, idx)) => insert_into_tree(state, window, &output, idx),
        None => tile_new_window(state, window, state.pointer.current_location()),
    }
}

/// Remove `window` from whichever output/workspace's tiling tree contains
/// it (wherever that is - it need not be the active/visible one), and
/// re-flow that output's active workspace if it was affected. The window
/// becomes floating, tracked at the workspace it was pulled out of. Returns
/// `true` if it was tiled.
pub fn untile_window<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) -> bool {
    let Some((output, idx)) = locate(state, window) else {
        return false;
    };
    let removed = TilingState::tree_mut(&output, idx).remove(window);
    if removed {
        crate::shell::workspace::mark_floating(state, window, &output, idx);
        if idx == WorkspaceState::get(&output).active() {
            apply_layout_animated(state, &output);
        }
    }
    removed
}

/// Toggle whether `window` participates in tiling.
pub fn toggle_floating<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement) {
    if untile_window(state, window) {
        return;
    }
    tile_new_window(state, window, state.pointer.current_location());
}

/// Set whether `window` participates in tiling, a no-op if it's already in
/// the requested state. Unlike [`toggle_floating`] (keybinding-driven, so it
/// always targets the pointer/focus), re-tiling here targets the window's
/// own home output/workspace (see [`retile_at_home`]) since this backs a
/// client request (e.g. a dock context menu) that isn't necessarily acting
/// on the window under the pointer.
pub fn set_floating<BackendData: Backend>(state: &mut AnvilState<BackendData>, window: &WindowElement, floating: bool) {
    let currently_tiled = locate(state, window).is_some();
    if floating && currently_tiled {
        untile_window(state, window);
    } else if !floating && !currently_tiled {
        retile_at_home(state, window);
    }
}

/// Which side of `rect` is closest to `point`, for [`drop_target`]'s
/// direction heuristic - the edge `point` has traveled furthest toward, as a
/// fraction of that edge's own dimension (not absolute logical pixels), so
/// it's just as sensitive on a tiny sliver of a tile as on a huge one.
fn edge_direction(rect: Rectangle<i32, Logical>, point: Point<f64, Logical>) -> Direction {
    let rel_x = ((point.x - rect.loc.x as f64) / rect.size.w.max(1) as f64).clamp(0.0, 1.0);
    let rel_y = ((point.y - rect.loc.y as f64) / rect.size.h.max(1) as f64).clamp(0.0, 1.0);
    [
        (rel_x, Direction::Left),
        (1.0 - rel_x, Direction::Right),
        (rel_y, Direction::Up),
        (1.0 - rel_y, Direction::Down),
    ]
    .into_iter()
    .min_by(|(a, _), (b, _)| a.total_cmp(b))
    .map(|(_, dir)| dir)
    .expect("non-empty")
}

/// The half of `rect` that [`TilingLayout::insert_beside`] would give a
/// window dropped toward `direction`.
fn half_rect(rect: Rectangle<i32, Logical>, direction: Direction) -> Rectangle<i32, Logical> {
    let half_w = rect.size.w / 2;
    let half_h = rect.size.h / 2;
    match direction {
        Direction::Left => Rectangle::new(rect.loc, (half_w, rect.size.h).into()),
        Direction::Right => Rectangle::new(
            Point::from((rect.loc.x + half_w, rect.loc.y)),
            (rect.size.w - half_w, rect.size.h).into(),
        ),
        Direction::Up => Rectangle::new(rect.loc, (rect.size.w, half_h).into()),
        Direction::Down => Rectangle::new(
            Point::from((rect.loc.x, rect.loc.y + half_h)),
            (rect.size.w, rect.size.h - half_h).into(),
        ),
    }
}

/// The tiled window a drag currently hovering `pointer` (on `output`'s
/// active workspace) would land beside if dropped now, which side of it,
/// and the rect a drop indicator should highlight to preview that. `None`
/// if `pointer` isn't over any tile (a gap, an empty output, or a
/// fullscreen/single-window workspace's own edges are still "over" that one
/// tile, so this is only `None` when there's nothing tiled there at all).
pub(crate) fn drop_target<BackendData: Backend>(
    state: &AnvilState<BackendData>,
    output: &Output,
    pointer: Point<f64, Logical>,
) -> Option<(WindowElement, Direction, Rectangle<i32, Logical>)> {
    let idx = WorkspaceState::get(output).active();
    let area = tiling_area(&state.space, output, state.config.gaps.outer as i32);
    let rects = TilingState::tree(output, idx).layout(area, state.config.gaps.inner as i32);
    let point = pointer.to_i32_round();
    let (window, rect) = rects.into_iter().find(|(_, r)| r.contains(point))?;
    let direction = edge_direction(rect, pointer);
    let indicator = half_rect(rect, direction);
    Some((window, direction, indicator))
}

/// Finishes a tiling drag-and-drop: re-tiles `window` (already pulled out of
/// the tree by [`untile_window`] when the drag grab started, and still
/// registered floating from that) into `output`'s active workspace, beside
/// `target` on the given side - or via the ordinary [`TilingLayout::insert`]
/// heuristic if there was no drop target (dropped over empty space) or it
/// died mid-drag.
pub(crate) fn tile_dropped_window<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    window: &WindowElement,
    output: &Output,
    target: Option<(WindowElement, Direction)>,
) {
    crate::shell::workspace::unregister_floating(window);
    let idx = WorkspaceState::get(output).active();
    let area = tiling_area(&state.space, output, state.config.gaps.outer as i32);
    {
        let mut tree = TilingState::tree_mut(output, idx);
        match target {
            Some((target_window, direction)) if tree.contains(&target_window) => {
                tree.insert_beside(window.clone(), area, &target_window, direction);
            }
            _ => tree.insert(window.clone(), area, None),
        }
    }
    crate::shell::workspace::assign_new_window(window, output, false);
    apply_layout_animated(state, output);
}

/// Re-flow every output's tiling tree, e.g. after output geometry changed
/// (scale, rotation, added/removed monitors).
pub fn retile_all_outputs<BackendData: Backend>(state: &mut AnvilState<BackendData>) {
    let outputs: Vec<Output> = state.space.outputs().cloned().collect();
    for output in outputs {
        apply_layout(state, &output);
    }
}

/// Drop dead windows from every output's tiling tree (every workspace, not
/// just the active one) and re-flow whichever active workspaces changed. If
/// the currently focused window is among the casualties, focus moves to its
/// nearest surviving neighbor in the same tiling tree, falling back to
/// whatever else is left on that workspace. Returns whether any window was
/// removed.
pub fn cleanup_dead<BackendData: Backend>(state: &mut AnvilState<BackendData>) -> bool {
    let outputs: Vec<Output> = state.all_outputs().cloned().collect();
    let mut changed = false;

    let dead_focus = current_focused_window(state).filter(|w| !w.alive());
    let fallback = dead_focus.as_ref().and_then(|focused| {
        let (output, idx) = locate(state, focused)?;
        let area = tiling_area(&state.space, &output, state.config.gaps.outer as i32);
        let rects = TilingState::tree(&output, idx).layout(area, state.config.gaps.inner as i32);
        let replacement = nearest_neighbor(&rects, focused);
        Some((output, idx, replacement))
    });

    for output in &outputs {
        let active = WorkspaceState::get(output).active();
        let mut active_changed = false;
        for idx in 0..TilingState::len(output) {
            let workspace_changed = TilingState::tree_mut(output, idx).retain_alive();
            changed |= workspace_changed;
            active_changed |= workspace_changed && idx == active;
        }
        // A parked (disconnected) output has no geometry to lay out into; its
        // tree is re-flowed when it's restored.
        if active_changed && state.space.output_geometry(output).is_some() {
            apply_layout_animated(state, output);
        }
    }
    changed |= crate::shell::workspace::cleanup_dead(state);

    if dead_focus.is_some() {
        match fallback.as_ref().and_then(|(_, _, w)| w.clone()).filter(|w| w.alive()) {
            Some(target) => raise_and_focus(state, &target),
            None => match fallback {
                Some((output, idx, _)) => crate::shell::workspace::focus_first_in_workspace(state, &output, idx),
                None => {
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let serial = SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(state, None, serial);
                    }
                }
            },
        }
    }

    changed
}

/// The output keyboard-driven focus/move actions should treat as "current":
/// the focused window's output (tiled or floating), else the output under the
/// pointer (so an empty, unfocused monitor still works as a starting point).
fn current_output<BackendData: Backend>(
    state: &AnvilState<BackendData>,
    focused: Option<&WindowElement>,
) -> Option<Output> {
    if let Some(w) = focused {
        if let Some((o, _)) = locate(state, w) {
            return Some(o);
        }
        if let Some(o) = crate::shell::workspace::home_output(w) {
            return Some(o);
        }
    }
    state
        .space
        .output_under(state.pointer.current_location())
        .next()
        .or_else(|| state.space.outputs().next())
        .cloned()
}

/// Focus the window on `output`'s active workspace nearest to the edge that
/// `dir` enters through, lined up with `from_y`/`from_x` of the origin. An
/// empty monitor takes keyboard focus away from windows and receives the
/// pointer, so the next directional action starts from it.
fn focus_on_output<BackendData: Backend>(
    state: &mut AnvilState<BackendData>,
    output: &Output,
    dir: Direction,
    origin: (i32, i32),
) {
    let idx = WorkspaceState::get(output).active();
    let area = tiling_area(&state.space, output, state.config.gaps.outer as i32);
    let rects = TilingState::tree(output, idx).layout(area, state.config.gaps.inner as i32);

    const FAR: i32 = 1_000_000;
    let from = match dir {
        Direction::Left => (FAR, origin.1),
        Direction::Right => (-FAR, origin.1),
        Direction::Up => (origin.0, FAR),
        Direction::Down => (origin.0, -FAR),
    };
    let target = rects
        .iter()
        .filter(|(w, _)| w.alive())
        .filter_map(|(w, r)| directional_score(dir, from, center(r)).map(|s| (s, w.clone())))
        .min_by_key(|(s, _)| *s)
        .map(|(_, w)| w);

    match target {
        Some(window) => raise_and_focus(state, &window),
        None => {
            crate::shell::workspace::focus_first_in_workspace(state, output, idx);
            if current_focused_window(state).is_none()
                && let Some(geo) = state.space.output_geometry(output)
            {
                let c = center(&geo);
                state.pointer.set_location((c.0 as f64, c.1 as f64).into());
            }
        }
    }
}

/// Move keyboard focus to the tiled window neighboring the currently focused
/// one in `dir`. If there is no neighbor in that direction on the current
/// output (or the focused window is floating, or nothing is focused), hops to
/// the adjacent monitor in `dir` (per the monitor layout), focusing the
/// window nearest to the edge it enters through.
pub fn focus_direction<BackendData: Backend>(state: &mut AnvilState<BackendData>, dir: Direction) {
    let focused = current_focused_window(state);
    let Some(output) = current_output(state, focused.as_ref()) else {
        return;
    };
    let mut origin = state
        .space
        .output_geometry(&output)
        .map(|g| center(&g))
        .unwrap_or((0, 0));

    if let Some(focused) = &focused {
        if let Some((_, idx)) = locate(state, focused) {
            let area = tiling_area(&state.space, &output, state.config.gaps.outer as i32);
            let rects = TilingState::tree(&output, idx).layout(area, state.config.gaps.inner as i32);
            if let Some(target) = neighbor(&rects, focused, dir) {
                raise_and_focus(state, &target);
                return;
            }
        }
        if let Some(geo) = state.space.element_geometry(focused) {
            origin = center(&geo);
        }
    }

    if let Some(next_output) = output_in_direction(state, &output, dir) {
        focus_on_output(state, &next_output, dir, origin);
    }
}

/// Swap the currently focused tiled window with its neighbor in `dir`. If
/// there is no neighbor, moves the window to the adjacent monitor in `dir`
/// instead.
pub fn swap_direction<BackendData: Backend>(state: &mut AnvilState<BackendData>, dir: Direction) {
    let Some(focused) = current_focused_window(state) else {
        return;
    };
    let Some((output, idx)) = locate(state, &focused) else {
        return;
    };
    let area = tiling_area(&state.space, &output, state.config.gaps.outer as i32);
    let rects = TilingState::tree(&output, idx).layout(area, state.config.gaps.inner as i32);
    let Some(target) = neighbor(&rects, &focused, dir) else {
        crate::shell::workspace::move_focused_window_to_monitor(state, dir);
        return;
    };
    TilingState::tree_mut(&output, idx).swap(&focused, &target);
    apply_layout(state, &output);
    raise_and_focus(state, &focused);
}

/// Grow the currently focused tiled window towards `dir` by shrinking its neighbor.
pub fn resize_tiled<BackendData: Backend>(state: &mut AnvilState<BackendData>, dir: Direction) {
    let Some(focused) = current_focused_window(state) else {
        return;
    };
    let Some((output, idx)) = locate(state, &focused) else {
        return;
    };
    let vertical = matches!(dir, Direction::Left | Direction::Right);
    let grow = matches!(dir, Direction::Right | Direction::Down);
    let delta = if grow { RESIZE_STEP } else { -RESIZE_STEP };
    TilingState::tree_mut(&output, idx).adjust_ratio(&focused, vertical, delta);
    apply_layout(state, &output);
}

fn center(rect: &Rectangle<i32, Logical>) -> (i32, i32) {
    (rect.loc.x + rect.size.w / 2, rect.loc.y + rect.size.h / 2)
}

/// Score how well `to` sits in `dir` relative to `from`, lower being closer.
/// `None` if `to` isn't in that direction at all.
fn directional_score(dir: Direction, from: (i32, i32), to: (i32, i32)) -> Option<i64> {
    let (fx, fy) = from;
    let (tx, ty) = to;
    let matches_dir = match dir {
        Direction::Left => tx < fx,
        Direction::Right => tx > fx,
        Direction::Up => ty < fy,
        Direction::Down => ty > fy,
    };
    if !matches_dir {
        return None;
    }
    let (primary, secondary) = match dir {
        Direction::Left | Direction::Right => ((tx - fx).abs(), (ty - fy).abs()),
        Direction::Up | Direction::Down => ((ty - fy).abs(), (tx - fx).abs()),
    };
    Some(primary as i64 * 4 + secondary as i64)
}

fn neighbor(
    rects: &[(WindowElement, Rectangle<i32, Logical>)],
    focused: &WindowElement,
    dir: Direction,
) -> Option<WindowElement> {
    let focused_rect = rects.iter().find(|(w, _)| w == focused)?.1;
    let from = center(&focused_rect);

    let mut best: Option<(i64, WindowElement)> = None;
    for (w, r) in rects {
        if w == focused {
            continue;
        }
        let Some(score) = directional_score(dir, from, center(r)) else {
            continue;
        };
        if best.as_ref().is_none_or(|(best_score, _)| score < *best_score) {
            best = Some((score, w.clone()));
        }
    }
    best.map(|(_, w)| w)
}

/// The output adjacent to `from` in `dir`, per the outputs' arranged
/// positions in `state.space` (i.e. the monitor layout), if any.
pub(crate) fn output_in_direction<BackendData: Backend>(
    state: &AnvilState<BackendData>,
    from: &Output,
    dir: Direction,
) -> Option<Output> {
    let from_geo = state.space.output_geometry(from)?;
    let origin = center(&from_geo);

    let mut best: Option<(i64, Output)> = None;
    for output in state.space.outputs() {
        if output == from {
            continue;
        }
        let Some(geo) = state.space.output_geometry(output) else {
            continue;
        };
        let Some(score) = directional_score(dir, origin, center(&geo)) else {
            continue;
        };
        if best.as_ref().is_none_or(|(best_score, _)| score < *best_score) {
            best = Some((score, output.clone()));
        }
    }
    best.map(|(_, o)| o)
}

/// The tiled window in `rects` geometrically closest to `focused`, in any
/// direction. Used to pick a replacement focus when `focused` is about to
/// disappear (e.g. it was just closed).
fn nearest_neighbor(
    rects: &[(WindowElement, Rectangle<i32, Logical>)],
    focused: &WindowElement,
) -> Option<WindowElement> {
    let focused_rect = rects.iter().find(|(w, _)| w == focused)?.1;
    let (fx, fy) = center(&focused_rect);
    rects
        .iter()
        .filter(|(w, _)| w != focused)
        .min_by_key(|(_, r)| {
            let (cx, cy) = center(r);
            let dx = (cx - fx) as i64;
            let dy = (cy - fy) as i64;
            dx * dx + dy * dy
        })
        .map(|(w, _)| w.clone())
}

#[cfg(test)]
mod drop_indicator_tests {
    use super::{Direction, edge_direction, half_rect};
    use smithay::utils::{Point, Rectangle};

    fn rect() -> Rectangle<i32, smithay::utils::Logical> {
        Rectangle::new(Point::from((100, 200)), (300, 400).into())
    }

    #[test]
    fn edge_direction_picks_the_nearest_edge() {
        let r = rect();
        // Near the left edge, away from top/bottom.
        assert_eq!(edge_direction(r, Point::from((110.0, 400.0))), Direction::Left);
        // Near the right edge.
        assert_eq!(edge_direction(r, Point::from((390.0, 400.0))), Direction::Right);
        // Near the top edge.
        assert_eq!(edge_direction(r, Point::from((250.0, 210.0))), Direction::Up);
        // Near the bottom edge.
        assert_eq!(edge_direction(r, Point::from((250.0, 590.0))), Direction::Down);
    }

    #[test]
    fn edge_direction_clamps_points_outside_the_rect() {
        // Comfortably left of the rect entirely - still reads as "left".
        assert_eq!(edge_direction(rect(), Point::from((-500.0, 400.0))), Direction::Left);
    }

    #[test]
    fn half_rect_splits_evenly_and_covers_the_source_rect() {
        let r = rect();
        for dir in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            let half = half_rect(r, dir);
            assert!(r.contains_rect(half));
        }

        let left = half_rect(r, Direction::Left);
        let right = half_rect(r, Direction::Right);
        assert_eq!(left.size.w + right.size.w, r.size.w);
        assert_eq!(left.loc.x, r.loc.x);
        assert_eq!(right.loc.x + right.size.w, r.loc.x + r.size.w);

        let up = half_rect(r, Direction::Up);
        let down = half_rect(r, Direction::Down);
        assert_eq!(up.size.h + down.size.h, r.size.h);
        assert_eq!(up.loc.y, r.loc.y);
        assert_eq!(down.loc.y + down.size.h, r.loc.y + r.size.h);
    }
}

#[cfg(test)]
mod window_animation_tests {
    use super::{ease_out_back, ease_out_cubic, lerp_rect, shrink_centered};
    use smithay::utils::{Point, Rectangle};

    fn rect() -> Rectangle<i32, smithay::utils::Logical> {
        Rectangle::new(Point::from((100, 200)), (300, 400).into())
    }

    #[test]
    fn ease_out_back_starts_at_zero_and_settles_exactly_at_one() {
        assert!(ease_out_back(0.0).abs() < 1e-6);
        assert!((ease_out_back(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ease_out_back_overshoots_past_one_before_settling() {
        let max = (0..=100)
            .map(|i| ease_out_back(i as f32 / 100.0))
            .fold(0.0f32, f32::max);
        assert!(max > 1.0, "expected some overshoot, got a peak of {max}");
    }

    #[test]
    fn ease_out_cubic_has_no_overshoot() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            assert!(ease_out_cubic(t) <= 1.0 + f32::EPSILON);
        }
        assert!(ease_out_cubic(0.0).abs() < 1e-6);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn lerp_rect_hits_its_endpoints() {
        let from = Rectangle::new(Point::from((0, 0)), (100, 100).into());
        let to = rect();
        assert_eq!(lerp_rect(from, to, 0.0), from);
        assert_eq!(lerp_rect(from, to, 1.0), to);
    }

    #[test]
    fn shrink_centered_keeps_the_same_center() {
        let r = rect();
        let shrunk = shrink_centered(r);
        assert!(shrunk.size.w < r.size.w);
        assert!(shrunk.size.h < r.size.h);
        let center = |rect: Rectangle<i32, smithay::utils::Logical>| {
            (rect.loc.x + rect.size.w / 2, rect.loc.y + rect.size.h / 2)
        };
        // Rounding from odd sizes can shift the center by a pixel - not
        // exact equality, just "still roughly centered".
        let (cx, cy) = center(r);
        let (sx, sy) = center(shrunk);
        assert!((cx - sx).abs() <= 1);
        assert!((cy - sy).abs() <= 1);
    }
}
