//! Virtual desktops ("workspaces"), layered on top of [`super::tiling`].
//!
//! Each output keeps its own set of workspaces - a workspace is just a
//! workspace-indexed slot: tiled windows live in [`super::tiling::TilingState`]
//! (already indexed by workspace), and floating windows are tracked here in
//! a small per-workspace registry, since unlike tiled windows their position
//! isn't recomputable from a layout tree and has to be remembered across a
//! hide/show cycle.
//!
//! Two things sit on top of that per-output storage:
//!
//! - **Mode** ([`crate::config::WorkspaceMode`]): in `PerMonitor` mode,
//!   switching workspaces only touches the output the switch was requested
//!   on. In `Combined` mode, every output is switched to the same index at
//!   once, so all monitors always show "workspace N" together (GNOME-style).
//!   Moving a window to an adjacent workspace always targets the window's
//!   own output in both modes - workspaces don't relocate windows across
//!   monitors, only across slots on their own.
//! - **Dynamic growth**: if enabled, navigating or moving a window past the
//!   last workspace creates a new one on demand, and trailing empty
//!   workspaces are pruned back down again automatically (GNOME-style).
//!   Otherwise the workspace count is fixed.

use std::cell::{RefCell, RefMut};
use std::time::{Duration, Instant};

use smithay::{
    desktop::Space,
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER},
};

use crate::{
    config::{Config, WorkspaceMode, WorkspaceTransitionAxis},
    state::{AnvilState, Backend},
};

use super::{FullscreenSurface, WindowElement, tiling};

/// How long the workspace-dot overlay stays on screen after a switch.
pub const OVERLAY_DURATION_MS: u64 = 1200;

/// Per-window bookkeeping: which output + workspace a window is currently
/// homed to, and (floating windows only) the location to restore it to when
/// that workspace becomes visible again.
#[derive(Default)]
struct WindowHome {
    output: RefCell<Option<Output>>,
    index: RefCell<usize>,
    floating_pos: RefCell<Option<Point<i32, Logical>>>,
    /// Whether this window was the output's fullscreen surface at the last
    /// time its workspace was hidden - restored by [`show_workspace`].
    fullscreen: RefCell<bool>,
}

impl WindowHome {
    fn get(window: &WindowElement) -> &WindowHome {
        window.user_data().insert_if_missing(WindowHome::default);
        window.user_data().get::<WindowHome>().unwrap()
    }
}

/// Per-output workspace bookkeeping: which workspace is active, how many
/// exist, and the floating windows homed to each one.
#[derive(Default)]
pub struct WorkspaceState {
    active: RefCell<usize>,
    count: RefCell<usize>,
    floating: RefCell<Vec<Vec<WindowElement>>>,
}

impl WorkspaceState {
    pub fn get(output: &Output) -> &WorkspaceState {
        output
            .user_data()
            .insert_if_missing(WorkspaceState::default);
        let state = output.user_data().get::<WorkspaceState>().unwrap();
        // Lazily-created state (e.g. touched before `init_output` runs)
        // still needs at least one workspace to be usable.
        if *state.count.borrow() == 0 {
            *state.count.borrow_mut() = 1;
        }
        state
    }

    pub fn active(&self) -> usize {
        *self.active.borrow()
    }

    pub fn count(&self) -> usize {
        *self.count.borrow()
    }

    fn floating_slot(&self, idx: usize) -> RefMut<'_, Vec<WindowElement>> {
        let mut floating = self.floating.borrow_mut();
        if floating.len() <= idx {
            floating.resize_with(idx + 1, Vec::new);
        }
        RefMut::map(floating, |v| &mut v[idx])
    }

    fn floating_at(&self, idx: usize) -> Vec<WindowElement> {
        self.floating.borrow().get(idx).cloned().unwrap_or_default()
    }
}

/// Sets up workspace state for a newly connected output: how many
/// workspaces it starts with, and, in `Combined` mode, syncing its active
/// index/count to whatever the other outputs are already showing so a
/// hot-plugged monitor joins the same virtual desktop.
///
/// Takes `config`/`space` rather than `&AnvilState` so it can be called from
/// backend code that's already holding a disjoint mutable borrow of another
/// `AnvilState` field (e.g. a backend device entry) at the call site.
pub fn init_output(config: &Config, space: &Space<WindowElement>, output: &Output) {
    let settings = &config.workspaces;
    // In dynamic mode workspaces are meant to grow on demand and prune back
    // down to a minimum of one when empty (see `prune_trailing_empty`), so a
    // freshly connected output starts at that floor rather than at
    // `settings.count` - otherwise, since pruning only runs on a workspace
    // switch, it would sit stuck showing `count` empty workspaces (e.g. in
    // molunga-shell's workspace indicator) until the user first navigated
    // away and back.
    let initial_count = if settings.dynamic { 1 } else { settings.count.max(1) };
    let (active, count) = if settings.mode == WorkspaceMode::Combined {
        space
            .outputs()
            .find(|o| *o != output)
            .map(|o| {
                let other = WorkspaceState::get(o);
                (other.active(), other.count())
            })
            .unwrap_or((0, initial_count))
    } else {
        (0, initial_count)
    };
    let ws = WorkspaceState::get(output);
    *ws.active.borrow_mut() = active;
    *ws.count.borrow_mut() = count;
}

/// Applies changed workspace settings to every connected output. When a
/// fixed count shrinks, windows from removed slots are preserved by moving
/// them into the new last workspace before the public workspace list is
/// updated.
pub fn apply_config<B: Backend>(state: &mut AnvilState<B>) {
    let outputs: Vec<Output> = state.space.outputs().cloned().collect();
    let count = state.config.workspaces.count.max(1);

    for output in &outputs {
        let old_count = WorkspaceState::get(output)
            .count()
            .max(tiling::TilingState::len(output));
        let destination = count - 1;
        let area = tiling::tiling_area(&state.space, output, state.config.gaps.outer as i32);

        for idx in count..old_count {
            let tiled = tiling::TilingState::tree(output, idx).windows();
            for window in tiled {
                tiling::TilingState::tree_mut(output, idx).remove(&window);
                tiling::TilingState::tree_mut(output, destination).insert(
                    window.clone(),
                    area,
                    None,
                );
                *WindowHome::get(&window).index.borrow_mut() = destination;
            }

            let floating = WorkspaceState::get(output).floating_at(idx);
            WorkspaceState::get(output).floating_slot(idx).clear();
            for window in floating {
                *WindowHome::get(&window).index.borrow_mut() = destination;
                let mut destination_slot = WorkspaceState::get(output).floating_slot(destination);
                if !destination_slot.contains(&window) {
                    destination_slot.push(window);
                }
            }
        }
        *WorkspaceState::get(output).count.borrow_mut() = count;
    }

    let combined_active = outputs
        .first()
        .map(|output| WorkspaceState::get(output).active().min(count - 1))
        .unwrap_or(0);
    for output in &outputs {
        let target = if state.config.workspaces.mode == WorkspaceMode::Combined {
            combined_active
        } else {
            WorkspaceState::get(output).active().min(count - 1)
        };
        set_active(state, output, target);
        // `set_active` returns early if the index was already right; make
        // sure a reduced destination workspace is reflowed either way.
        tiling::apply_layout(state, output);
    }
}

/// Registers a newly mapped window (tiled or floating) as belonging to
/// `output`'s currently active workspace.
pub fn assign_new_window(window: &WindowElement, output: &Output, floating: bool) {
    let idx = WorkspaceState::get(output).active();
    let home = WindowHome::get(window);
    *home.output.borrow_mut() = Some(output.clone());
    *home.index.borrow_mut() = idx;
    if floating {
        let mut slot = WorkspaceState::get(output).floating_slot(idx);
        if !slot.contains(window) {
            slot.push(window.clone());
        }
    }
}

/// Records that `window` just became floating, having been pulled out of
/// `output`'s workspace `idx` tiling tree. Tracks its current on-screen
/// position (if mapped) so it reappears there next time that workspace is shown.
pub fn mark_floating<B: Backend>(
    state: &AnvilState<B>,
    window: &WindowElement,
    output: &Output,
    idx: usize,
) {
    let home = WindowHome::get(window);
    *home.output.borrow_mut() = Some(output.clone());
    *home.index.borrow_mut() = idx;
    {
        let mut slot = WorkspaceState::get(output).floating_slot(idx);
        if !slot.contains(window) {
            slot.push(window.clone());
        }
    }
    if let Some(loc) = state.space.element_location(window) {
        *home.floating_pos.borrow_mut() = Some(loc);
    }
}

/// Removes `window`'s registration from whichever floating slot [`mark_floating`]
/// last put it in, without otherwise touching its tiled/floating status -
/// used when a tiling drag-and-drop re-tiles a window that briefly went
/// through [`mark_floating`] when the drag grab started.
pub(crate) fn unregister_floating(window: &WindowElement) {
    let home = WindowHome::get(window);
    if let Some(output) = home.output.borrow().clone() {
        let idx = *home.index.borrow();
        WorkspaceState::get(&output).floating_slot(idx).retain(|w| w != window);
    }
}

/// Drops dead windows from every output's floating registry. Tiled windows
/// are cleaned up by [`tiling::cleanup_dead`] (which calls this too).
/// Returns whether any window was removed.
pub fn cleanup_dead<B: Backend>(state: &AnvilState<B>) -> bool {
    let mut changed = false;
    for output in state.space.outputs() {
        let ws = WorkspaceState::get(output);
        for slot in ws.floating.borrow_mut().iter_mut() {
            let old_len = slot.len();
            slot.retain(|w| w.alive());
            changed |= slot.len() != old_len;
        }
    }
    changed
}

/// The next workspace index `delta` steps from `cur`, or `None` if that step
/// is out of bounds (below zero, or past the last workspace in fixed mode).
fn target_index(cur: usize, count: usize, delta: i32, dynamic: bool) -> Option<usize> {
    let next = cur as i32 + delta;
    if next < 0 {
        return None;
    }
    let next = next as usize;
    if !dynamic && next >= count {
        return None;
    }
    Some(next)
}

fn show_overlay<B: Backend>(state: &mut AnvilState<B>) {
    if state.config.workspaces.overlay {
        state.workspace_overlay_shown = Some(Instant::now());
    }
}

/// Unmaps every window (tiled or floating) belonging to `output`'s
/// workspace `idx`, remembering floating windows' positions first.
///
/// If one of them is `output`'s current fullscreen surface, that's also
/// cleared here - otherwise `render::output_elements` (which checks
/// `FullscreenSurface` before anything else) would keep drawing the now
/// unmapped window's last frame over whatever workspace replaces it.
/// [`show_workspace`] restores it if the window is still shown again later.
fn hide_workspace<B: Backend>(state: &mut AnvilState<B>, output: &Output, idx: usize) {
    let current_fullscreen = output
        .user_data()
        .get::<FullscreenSurface>()
        .and_then(|f| f.get());
    let mut unfullscreened = false;

    let tiled = tiling::TilingState::tree(output, idx).windows();
    for window in &tiled {
        if window.alive() {
            unfullscreened |= hide_window(state, output, window, current_fullscreen.as_ref());
        }
    }

    let floating = WorkspaceState::get(output).floating_at(idx);
    for window in &floating {
        if !window.alive() {
            continue;
        }
        if let Some(loc) = state.space.element_location(window) {
            *WindowHome::get(window).floating_pos.borrow_mut() = Some(loc);
        }
        unfullscreened |= hide_window(state, output, window, current_fullscreen.as_ref());
    }

    if unfullscreened {
        state.backend_data.reset_buffers(output);
        crate::foreign_toplevel::sync(state);
        crate::ext_workspace::ext_workspace_sync(state);
        crate::workspace_windows::sync(state);
    }
}

/// Unmaps a single window, recording (and clearing) whether it was
/// `output`'s fullscreen surface. Returns whether it was.
fn hide_window<B: Backend>(
    state: &mut AnvilState<B>,
    output: &Output,
    window: &WindowElement,
    current_fullscreen: Option<&WindowElement>,
) -> bool {
    state.space.unmap_elem(window);

    let was_fullscreen = current_fullscreen == Some(window);
    *WindowHome::get(window).fullscreen.borrow_mut() = was_fullscreen;
    if was_fullscreen
        && let Some(fullscreen) = output.user_data().get::<FullscreenSurface>()
    {
        fullscreen.clear();
    }
    was_fullscreen
}

/// Maps `output`'s workspace `idx` floating windows back into the space at
/// their remembered positions (first dropping any that died while hidden).
/// Returns them, for callers that need the full window list alongside the
/// tiled ones.
fn map_floating<B: Backend>(state: &mut AnvilState<B>, output: &Output, idx: usize) -> Vec<WindowElement> {
    let ws = WorkspaceState::get(output);
    if let Some(slot) = ws.floating.borrow_mut().get_mut(idx) {
        slot.retain(|w| w.alive());
    }
    let floating = ws.floating_at(idx);
    for window in &floating {
        let pos = WindowHome::get(window)
            .floating_pos
            .borrow()
            .unwrap_or_default();
        state.space.map_element(window.clone(), pos, false);
    }
    floating
}

/// Maps every (alive) window belonging to `output`'s workspace `idx` back
/// into the space: tiled windows are reflowed, floating ones restored to
/// their last known position. Whichever of them (if any) was fullscreen when
/// [`hide_workspace`] hid this workspace becomes `output`'s fullscreen
/// surface again.
fn show_workspace<B: Backend>(state: &mut AnvilState<B>, output: &Output, idx: usize) {
    // `apply_layout` reflows `output`'s *active* workspace, which by the
    // time this runs is already `idx` (the caller updates `active` first).
    tiling::apply_layout(state, output);

    let tiled = tiling::TilingState::tree(output, idx).windows();
    let floating = map_floating(state, output, idx);

    if let Some(window) = tiled
        .iter()
        .chain(floating.iter())
        .find(|w| w.alive() && *WindowHome::get(w).fullscreen.borrow())
    {
        restore_fullscreen(state, output, &window.clone());
    }
}

/// Re-fullscreens `window` on `output` after its workspace becomes visible
/// again, undoing what [`hide_workspace`] did when it was hidden.
fn restore_fullscreen<B: Backend>(state: &mut AnvilState<B>, output: &Output, window: &WindowElement) {
    let Some(geometry) = state.space.output_geometry(output) else {
        return;
    };

    #[allow(irrefutable_let_patterns)]
    if let Some(toplevel) = window.0.toplevel() {
        toplevel.with_pending_state(|s| {
            s.states.set(xdg_toplevel::State::Fullscreen);
            s.size = Some(geometry.size);
            // See the comment on the same assignment in
            // `shell::xdg::fullscreen_request` - without this a client that
            // treats `bounds` as a hard cap leaves a margin at the
            // bottom/right, sized to whatever the shell reserves there.
            s.bounds = Some(geometry.size);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
    #[cfg(feature = "xwayland")]
    if let Some(x11) = window.0.x11_surface() {
        let _ = x11.set_fullscreen(true);
        let _ = x11.configure(geometry);
    }

    output.user_data().insert_if_missing(FullscreenSurface::default);
    output
        .user_data()
        .get::<FullscreenSurface>()
        .unwrap()
        .set(window.clone());

    crate::foreign_toplevel::sync(state);
    crate::ext_workspace::ext_workspace_sync(state);
    crate::workspace_windows::sync(state);
}

/// An in-flight slide animation for a workspace switch on one output, like a
/// filmstrip: outgoing windows stay mapped and slide away toward `-dir`,
/// while incoming windows (already mapped off-screen by [`start_transition`],
/// at `dir` distance away) slide the same distance in the same direction to
/// reach their resting position. Only positions animate - sizes are fixed
/// once, by the same [`tiling::apply_layout`] call [`show_workspace`] would
/// otherwise have made.
struct WorkspaceTransition {
    start: Instant,
    duration: Duration,
    /// `1` if `new_idx > old_idx` (incoming slides in from the trailing
    /// edge - the right on the horizontal axis, the bottom on the vertical
    /// one), `-1` otherwise.
    dir: i32,
    /// Axis the slide moves along.
    axis: WorkspaceTransitionAxis,
    /// Slide distance in logical pixels - `output`'s width (horizontal axis)
    /// or height (vertical axis).
    distance: i32,
    /// Each window's resting position before the switch, which it animates
    /// away from.
    outgoing: Vec<(WindowElement, Point<i32, Logical>)>,
    /// Each window's resting position after the switch, which it animates
    /// toward.
    incoming: Vec<(WindowElement, Point<i32, Logical>)>,
}

#[derive(Default)]
struct WorkspaceTransitionSlot(RefCell<Option<WorkspaceTransition>>);

impl WorkspaceTransitionSlot {
    fn get(output: &Output) -> &WorkspaceTransitionSlot {
        output
            .user_data()
            .insert_if_missing(WorkspaceTransitionSlot::default);
        output.user_data().get::<WorkspaceTransitionSlot>().unwrap()
    }
}

/// Starts an animated slide from `old_idx` to `new_idx` on `output`, mapping
/// `new_idx`'s windows off-screen so [`advance_transitions`] can animate
/// both sets in from there. Returns whether it did - on `false` (animation
/// disabled, nothing to animate, or a fullscreen window is involved), the
/// caller falls back to the instant [`hide_workspace`]/[`show_workspace`]
/// pair.
fn start_transition<B: Backend>(
    state: &mut AnvilState<B>,
    output: &Output,
    old_idx: usize,
    new_idx: usize,
) -> bool {
    let duration_ms = state.config.workspaces.transition_ms;
    if duration_ms == 0 {
        return false;
    }

    // Fullscreen rendering bypasses `space` positions entirely (see
    // `render::output_elements`, which draws the fullscreen surface at a
    // fixed (0, 0) regardless of where it's mapped), so animating a
    // position here would be invisible while skipping the fullscreen
    // bookkeeping `hide_workspace`/`show_workspace` do - simpler to just not
    // animate a switch that involves one.
    if output
        .user_data()
        .get::<FullscreenSurface>()
        .and_then(|f| f.get())
        .is_some()
    {
        return false;
    }

    let axis = state.config.workspaces.transition_axis;
    let Some(distance) = state.space.output_geometry(output).map(|g| match axis {
        WorkspaceTransitionAxis::Horizontal => g.size.w,
        WorkspaceTransitionAxis::Vertical => g.size.h,
    }) else {
        return false;
    };
    if distance == 0 {
        return false;
    }

    // A switch landing mid-animation of a previous one finishes that one
    // first, so its incoming windows' current (mid-slide) position isn't
    // mistaken for their resting one.
    finalize_transition(state, output);

    let outgoing: Vec<(WindowElement, Point<i32, Logical>)> = tiling::TilingState::tree(output, old_idx)
        .windows()
        .into_iter()
        .chain(WorkspaceState::get(output).floating_at(old_idx))
        .filter(|w| w.alive())
        .filter_map(|w| state.space.element_location(&w).map(|loc| (w, loc)))
        .collect();

    // `apply_layout` reflows `output`'s *active* workspace, which by the
    // time this runs is already `new_idx` (the caller updates `active`
    // first, same as `show_workspace` relies on).
    tiling::apply_layout(state, output);
    let tiled_incoming = tiling::TilingState::tree(output, new_idx).windows();
    let floating_incoming = map_floating(state, output, new_idx);

    let incoming: Vec<(WindowElement, Point<i32, Logical>)> = tiled_incoming
        .into_iter()
        .chain(floating_incoming)
        .filter(|w| w.alive())
        .filter_map(|w| state.space.element_location(&w).map(|loc| (w, loc)))
        .collect();

    if outgoing.is_empty() && incoming.is_empty() {
        return false;
    }

    let dir: i32 = if new_idx > old_idx { 1 } else { -1 };
    let offscreen = match axis {
        WorkspaceTransitionAxis::Horizontal => Point::from((dir * distance, 0)),
        WorkspaceTransitionAxis::Vertical => Point::from((0, dir * distance)),
    };
    for (window, target) in &incoming {
        state.space.map_element(window.clone(), *target + offscreen, false);
    }

    *WorkspaceTransitionSlot::get(output).0.borrow_mut() = Some(WorkspaceTransition {
        start: Instant::now(),
        duration: Duration::from_millis(duration_ms as u64),
        dir,
        axis,
        distance,
        outgoing,
        incoming,
    });

    true
}

/// Advances every in-flight workspace-switch animation on `output` by one
/// frame. Called from `AnvilState::pre_repaint`, so windows visibly slide
/// under both backends' render loops without either needing to know
/// animations exist.
pub(crate) fn advance_transitions<B: Backend>(state: &mut AnvilState<B>, output: &Output) {
    struct Snapshot {
        elapsed: Duration,
        duration: Duration,
        dir: i32,
        axis: WorkspaceTransitionAxis,
        distance: i32,
        outgoing: Vec<(WindowElement, Point<i32, Logical>)>,
        incoming: Vec<(WindowElement, Point<i32, Logical>)>,
    }

    let snapshot = {
        let transition = WorkspaceTransitionSlot::get(output).0.borrow();
        transition.as_ref().map(|t| Snapshot {
            elapsed: t.start.elapsed(),
            duration: t.duration,
            dir: t.dir,
            axis: t.axis,
            distance: t.distance,
            outgoing: t.outgoing.clone(),
            incoming: t.incoming.clone(),
        })
    };
    let Some(snapshot) = snapshot else {
        return;
    };

    if snapshot.elapsed >= snapshot.duration {
        finalize_transition(state, output);
        return;
    }

    let progress = snapshot.elapsed.as_secs_f32() / snapshot.duration.as_secs_f32();
    let eased = ease_out_cubic(progress.clamp(0.0, 1.0));
    // Outgoing and incoming slide the same way, like a filmstrip: incoming
    // starts off-screen at `dir * distance` (see `start_transition`) and
    // slides down to its resting position, while outgoing slides the same
    // distance in the same direction (`-dir`) away from its resting one.
    let outgoing_shift = (-snapshot.dir as f32 * snapshot.distance as f32 * eased).round() as i32;
    let incoming_shift = (snapshot.dir as f32 * snapshot.distance as f32 * (1.0 - eased)).round() as i32;
    let axis_point = |shift: i32| match snapshot.axis {
        WorkspaceTransitionAxis::Horizontal => Point::from((shift, 0)),
        WorkspaceTransitionAxis::Vertical => Point::from((0, shift)),
    };

    for (window, home) in &snapshot.outgoing {
        if window.alive() {
            state
                .space
                .map_element(window.clone(), *home + axis_point(outgoing_shift), false);
        }
    }
    for (window, target) in &snapshot.incoming {
        if window.alive() {
            state
                .space
                .map_element(window.clone(), *target + axis_point(incoming_shift), false);
        }
    }
}

/// Ends `output`'s in-flight transition (if any) immediately: unmaps the
/// outgoing windows and snaps the incoming ones to their exact resting
/// position - the same end state an instant
/// [`hide_workspace`]/[`show_workspace`] pair would have left behind.
fn finalize_transition<B: Backend>(state: &mut AnvilState<B>, output: &Output) {
    let Some(transition) = WorkspaceTransitionSlot::get(output).0.borrow_mut().take() else {
        return;
    };

    for (window, _) in &transition.outgoing {
        if window.alive() {
            state.space.unmap_elem(window);
        }
    }
    for (window, target) in &transition.incoming {
        if window.alive() {
            state.space.map_element(window.clone(), *target, false);
        }
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let f = t - 1.0;
    f * f * f + 1.0
}

pub(crate) fn focus_first_in_workspace<B: Backend>(state: &mut AnvilState<B>, output: &Output, idx: usize) {
    let candidate = tiling::TilingState::tree(output, idx)
        .windows()
        .into_iter()
        .find(|w| w.alive())
        .or_else(|| {
            WorkspaceState::get(output)
                .floating_at(idx)
                .into_iter()
                .find(|w| w.alive())
        });

    match candidate {
        Some(window) => tiling::raise_and_focus(state, &window),
        None => {
            if let Some(keyboard) = state.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(state, None, serial);
            }
        }
    }
}

/// Drops trailing empty workspaces (dynamic mode only), down to a minimum of
/// one and never below the active one.
fn prune_trailing_empty(output: &Output) {
    let ws = WorkspaceState::get(output);
    let active = ws.active();
    let mut count = ws.count();
    while count > 1 && count - 1 != active {
        let idx = count - 1;
        let tiled_empty = tiling::TilingState::tree(output, idx).is_empty();
        let floating_empty = ws.floating_at(idx).is_empty();
        if tiled_empty && floating_empty {
            count -= 1;
        } else {
            break;
        }
    }
    *ws.count.borrow_mut() = count;
}

fn set_active<B: Backend>(state: &mut AnvilState<B>, output: &Output, new_idx: usize) {
    let old_idx = WorkspaceState::get(output).active();
    if old_idx == new_idx {
        return;
    }

    let ws = WorkspaceState::get(output);
    *ws.active.borrow_mut() = new_idx;
    if new_idx + 1 > ws.count() {
        *ws.count.borrow_mut() = new_idx + 1;
    }

    // `start_transition` (like `show_workspace`) reflows `output`'s
    // *active* workspace via `apply_layout`, which is why `active` is
    // updated above before either runs.
    if !start_transition(state, output, old_idx, new_idx) {
        hide_workspace(state, output, old_idx);
        show_workspace(state, output, new_idx);
    }
    focus_first_in_workspace(state, output, new_idx);

    if state.config.workspaces.dynamic {
        prune_trailing_empty(output);
    }
}

/// Switches `output` (and, in `Combined` mode, every output) `delta`
/// workspaces over (-1 = previous, +1 = next). No-op if that would go out
/// of bounds.
pub fn switch_workspace<B: Backend>(state: &mut AnvilState<B>, output: &Output, delta: i32) {
    let ws = WorkspaceState::get(output);
    let Some(new_idx) = target_index(
        ws.active(),
        ws.count(),
        delta,
        state.config.workspaces.dynamic,
    ) else {
        return;
    };

    let outputs: Vec<Output> = if state.config.workspaces.mode == WorkspaceMode::Combined {
        state.space.outputs().cloned().collect()
    } else {
        vec![output.clone()]
    };
    for o in &outputs {
        set_active(state, o, new_idx);
    }

    show_overlay(state);
}

/// Moves the currently focused window `delta` workspaces over on its own
/// output (-1 = previous, +1 = next). No-op if nothing is focused or that
/// would go out of bounds. If `follow` is true, the active workspace
/// switches to the window's new one (see [`set_active`]), applying the same
/// `Combined`-mode fan-out as [`switch_workspace`]; otherwise the active
/// workspace stays put and focus returns to whatever else is left there.
pub fn move_focused_window<B: Backend>(state: &mut AnvilState<B>, delta: i32, follow: bool) {
    let Some(window) = tiling::current_focused_window(state) else {
        return;
    };

    let (output, was_tiled) = match tiling::locate(state, &window) {
        Some((o, _idx)) => (o, true),
        None => match WindowHome::get(&window).output.borrow().clone() {
            Some(o) => (o, false),
            None => return,
        },
    };

    let ws = WorkspaceState::get(&output);
    let active = ws.active();
    let Some(target_idx) = target_index(active, ws.count(), delta, state.config.workspaces.dynamic)
    else {
        return;
    };
    if target_idx == active {
        return;
    }

    // Pull the window out of its current (active) slot without leaving it
    // registered there.
    if was_tiled {
        tiling::TilingState::tree_mut(&output, active).remove(&window);
    } else {
        WorkspaceState::get(&output)
            .floating_slot(active)
            .retain(|w| w != &window);
    }

    let home = WindowHome::get(&window);
    *home.output.borrow_mut() = Some(output.clone());
    *home.index.borrow_mut() = target_idx;
    if target_idx + 1 > WorkspaceState::get(&output).count() {
        *WorkspaceState::get(&output).count.borrow_mut() = target_idx + 1;
    }

    if was_tiled {
        let area = tiling::tiling_area(&state.space, &output, state.config.gaps.outer as i32);
        tiling::TilingState::tree_mut(&output, target_idx).insert(window.clone(), area, None);
        // Reflows the source workspace (still active) to close the gap the
        // window left; the destination tree is applied whenever it's shown.
        tiling::apply_layout_animated(state, &output);
        state.space.unmap_elem(&window);
    } else {
        WorkspaceState::get(&output)
            .floating_slot(target_idx)
            .push(window.clone());
        if let Some(loc) = state.space.element_location(&window) {
            *home.floating_pos.borrow_mut() = Some(loc);
        }
        state.space.unmap_elem(&window);
    }

    if follow {
        let outputs: Vec<Output> = if state.config.workspaces.mode == WorkspaceMode::Combined {
            state.space.outputs().cloned().collect()
        } else {
            vec![output.clone()]
        };
        for o in &outputs {
            set_active(state, o, target_idx);
        }
        // In `Combined` mode `set_active` runs once per output, and each run
        // ends in `focus_first_in_workspace`. Every output other than the
        // window's own has an empty workspace at `target_idx` (a brand new
        // one, with dynamic workspaces), so its run clears keyboard focus
        // entirely - leaving no window focused. Refocus the moved window
        // last, so nothing after it can take focus away again.
        if window.alive() {
            tiling::raise_and_focus(state, &window);
        }
    } else {
        focus_first_in_workspace(state, &output, active);
        if state.config.workspaces.dynamic {
            prune_trailing_empty(&output);
        }
    }

    show_overlay(state);
}

/// Activates workspace `idx` on `output`, e.g. in response to the
/// `ext-workspace-v1` protocol's `activate` request. Applies the same
/// `Combined`-mode fan-out as [`switch_workspace`]. `idx` is expected to
/// name a workspace that already exists (protocol clients only ever hold
/// handles for workspaces we've told them about), so unlike
/// [`switch_workspace`]/[`move_focused_window`] this doesn't validate it
/// against `count`/`dynamic` first.
pub fn activate_workspace<B: Backend>(state: &mut AnvilState<B>, output: &Output, idx: usize) {
    let outputs: Vec<Output> = if state.config.workspaces.mode == WorkspaceMode::Combined {
        state.space.outputs().cloned().collect()
    } else {
        vec![output.clone()]
    };
    for o in &outputs {
        set_active(state, o, idx);
    }
    show_overlay(state);
}

/// `(active, count)` for `output`'s workspaces, for the dot overlay.
pub fn overlay_info(output: &Output) -> (usize, usize) {
    let ws = WorkspaceState::get(output);
    (ws.active(), ws.count())
}

/// Every window belonging to `output`, across *every* workspace it has (not
/// just the active one) - tiled and floating alike.
fn all_windows_on_output(output: &Output) -> Vec<WindowElement> {
    let mut windows: Vec<WindowElement> = tiling::TilingState::all(output)
        .iter()
        .flat_map(tiling::TilingLayout::windows)
        .collect();
    windows.extend(
        WorkspaceState::get(output)
            .floating
            .borrow()
            .iter()
            .flatten()
            .cloned(),
    );
    windows
}

/// Every window the compositor currently knows about, across every output
/// and workspace. Used by [`crate::foreign_toplevel`] to list every running
/// app regardless of which workspace currently hides it - unlike
/// `state.space.elements()`, which only sees the *visible* (active-workspace)
/// ones.
pub(crate) fn all_windows<B: Backend>(state: &AnvilState<B>) -> Vec<WindowElement> {
    state
        .space
        .outputs()
        .flat_map(all_windows_on_output)
        .collect()
}

/// The output+workspace `window` is currently homed to (tiled or floating),
/// if it's been assigned one yet (see [`assign_new_window`]/[`mark_floating`]).
pub(crate) fn window_home(window: &WindowElement) -> Option<(Output, usize)> {
    let home = WindowHome::get(window);
    let output = home.output.borrow().clone()?;
    Some((output, *home.index.borrow()))
}

/// Whether `window` is currently floating rather than tiled. `false` for a
/// window with no home yet.
pub(crate) fn is_floating(window: &WindowElement) -> bool {
    let Some((output, idx)) = window_home(window) else {
        return false;
    };
    WorkspaceState::get(&output).floating_at(idx).contains(window)
}

/// Switches to whichever workspace `window` is homed to (if it isn't
/// already the active one on its output) and focuses it. Backs the
/// `zwlr_foreign_toplevel_handle_v1.activate` request - see
/// [`crate::foreign_toplevel`].
pub(crate) fn activate_window<B: Backend>(state: &mut AnvilState<B>, window: &WindowElement) {
    if let Some((output, idx)) = window_home(window)
        && idx != WorkspaceState::get(&output).active()
    {
        activate_workspace(state, &output, idx);
    }
    if window.alive() {
        tiling::raise_and_focus(state, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_index_steps_within_bounds() {
        assert_eq!(target_index(1, 4, 1, false), Some(2));
        assert_eq!(target_index(1, 4, -1, false), Some(0));
    }

    #[test]
    fn target_index_fixed_mode_clamps_at_edges() {
        assert_eq!(target_index(0, 4, -1, false), None);
        assert_eq!(target_index(3, 4, 1, false), None);
    }

    #[test]
    fn target_index_dynamic_mode_grows_past_the_last_workspace() {
        assert_eq!(target_index(3, 4, 1, true), Some(4));
        // Still can't go negative even when dynamic.
        assert_eq!(target_index(0, 4, -1, true), None);
    }
}
