//! Server side of the `ironland-permission-prompt-v1` protocol (see
//! `crate::ironland_protocols::permission_prompt` for the generated
//! bindings and `protocols/ironland-permission-prompt-v1.xml` for the wire
//! format).
//!
//! Two kinds of prompt share this one queue, distinguished by
//! [`PromptTarget`]:
//!
//! - **Wire-requested**: a privileged *requester* client (in practice,
//!   `ironland-portal-screenshot`) calls the protocol's `prompt` request and
//!   waits on the returned object's `allowed`/`denied` event.
//! - **Internal**: the compositor itself queues one via
//!   [`PermissionPromptManagerState::queue_internal`], with no requester
//!   object at all - tagged with a [`PromptKind`] so [`PermissionPromptHandler::internal_prompt_resolved`]
//!   knows which in-memory grant table (`crate::screencopy`'s, gating
//!   direct non-portal screen capture, or `crate::clipboard`'s, gating
//!   clipboard-history access) the answer belongs to. Resolving one of
//!   these calls back via [`PermissionPromptHandler::internal_prompt_resolved`]
//!   instead of firing a protocol event.
//!
//! Every prompt, of either kind, is drawn by this compositor itself
//! (reusing the bitmap font from `crate::font`/`crate::drawing`, the same
//! way the launcher does) as a clickable Allow/Deny pair - answered either
//! by clicking one (`crate::input_handler::on_pointer_button`, hit-tested
//! against [`PermissionPromptManagerState::hit_test`]) or by the user's
//! Enter/Y (allow) or Escape/N (deny) (`crate::input_handler`'s keyboard path) - there is
//! deliberately no way for any client to take over rendering a prompt or
//! to answer on the user's behalf. An earlier version of this module let a
//! second privileged client (a *renderer*, in practice this compositor's
//! companion shell) do exactly that; it was removed because the only
//! thing gating who could claim that role was "connected through the
//! privileged capture socket", which any local process can do - letting
//! it silently self-approve every prompt, its own capture request
//! included, with no UI ever shown to the user. See
//! `protocols/ironland-permission-prompt-v1.xml`'s own doc for the same
//! history from the wire format's side.
//!
//! The prompt is always composited as the frontmost of this compositor's
//! own overlay elements (`crate::winit`/`crate::udev` push it right after
//! the pointer/dnd icon, ahead of the launcher, FPS overlay and workspace
//! switcher) - and those overlay elements are themselves always drawn
//! above every window and every client's layer-shell surface (bars,
//! popups, notifications included - see `crate::render::output_elements`),
//! so nothing can visually cover the prompt while it's showing.

use std::collections::VecDeque;

use smithay::backend::allocator::Fourcc;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    backend::{ClientId, GlobalId},
};
use smithay::utils::{Logical, Point, Transform};
use smithay::wayland::Dispatch2;
use smithay::wayland::GlobalDispatch2;
use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer,
    utils::{Rectangle, Size},
};

use crate::font::Canvas;
use crate::ironland_protocols::permission_prompt::{
    ironland_permission_prompt_manager_v1::{self, IronlandPermissionPromptManagerV1},
    ironland_permission_prompt_v1::{self, IronlandPermissionPromptV1},
};

/// Implemented by the compositor state so this module can queue/answer
/// prompts without depending on `AnvilState` directly.
pub trait PermissionPromptHandler: 'static {
    fn permission_prompt_state(&mut self) -> &mut PermissionPromptManagerState;

    /// Called when an internally-queued prompt (see
    /// [`PermissionPromptManagerState::queue_internal`]) is resolved, with
    /// the same `kind`/`subject` it was queued under. There's no requester
    /// object to notify for these, unlike wire-requested prompts, so this is
    /// the only way to learn the answer.
    fn internal_prompt_resolved(&mut self, kind: PromptKind, subject: String, allowed: bool) {
        let _ = (kind, subject, allowed);
    }
}

/// Which in-memory grant table an internally-queued prompt (see
/// [`PermissionPromptManagerState::queue_internal`]) belongs to - carried
/// through to [`PermissionPromptHandler::internal_prompt_resolved`] so the
/// one shared prompt queue can back more than one such table without them
/// interfering (in particular, so a still-pending prompt for one table
/// doesn't dedupe-suppress a same-named subject's prompt for another).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Gates `crate::screencopy`'s direct, non-portal screen-capture grant
    /// table.
    Capture,
    /// Gates `crate::clipboard`'s clipboard-history access grant table.
    ClipboardHistory,
}

/// Who resolving a [`PendingPrompt`] notifies - see the module doc.
#[derive(Debug)]
enum PromptTarget {
    Requester(IronlandPermissionPromptV1),
    Internal(PromptKind),
}

#[derive(Debug)]
struct PendingPrompt {
    /// For a wire-requested prompt, the requesting `app_id` as chosen by
    /// the (trusted) requester. For an internal one (see
    /// [`PromptTarget::Internal`]), doubles as the subject
    /// [`PermissionPromptHandler::internal_prompt_resolved`] is called back
    /// with - see [`PermissionPromptManagerState::queue_internal`].
    app_id: String,
    reason: String,
    target: PromptTarget,
}

const PROMPT_WIDTH: i32 = 520;
const PROMPT_PADDING: i32 = 18;
const FONT_SCALE: i32 = 2;
const LINE_HEIGHT: i32 = crate::font::GLYPH_HEIGHT as i32 * FONT_SCALE + 10;

// Button row layout - see `button_rects`/`button_height`, the one source
// of truth both drawing (`rasterize`) and pointer hit-testing (`hit_test`)
// read from, so they can never drift apart.
const BUTTON_PADDING_X: i32 = 18;
const BUTTON_PADDING_Y: i32 = 9;
const BUTTON_GAP: i32 = 16;
const BUTTON_ROW_TOP_GAP: i32 = 14;

/// Button labels, with the keyboard shortcut that answers each (see
/// `crate::input_handler`'s keyboard path).
const DENY_LABEL: &str = "Deny (N)";
const ALLOW_LABEL: &str = "Allow (Y)";

const COLOR_BACKGROUND: [u8; 4] = [40, 30, 30, 245];
const COLOR_TITLE: [u8; 4] = [235, 230, 225, 255];
const COLOR_DENY_BUTTON: [u8; 4] = [110, 50, 50, 255];
const COLOR_ALLOW_BUTTON: [u8; 4] = [70, 110, 70, 255];
const COLOR_BUTTON_TEXT: [u8; 4] = [245, 240, 235, 255];

/// Height, in logical pixels, of an Allow/Deny button - just the glyph
/// height at [`FONT_SCALE`] plus vertical padding, same for both since
/// they share a row.
fn button_height() -> i32 {
    crate::font::GLYPH_HEIGHT as i32 * FONT_SCALE + BUTTON_PADDING_Y * 2
}

/// The Deny and Allow buttons' rectangles, in prompt-local logical pixels
/// (i.e. relative to the prompt's own top-left corner - add
/// [`PermissionPromptManagerState::origin_in`] to place them on an
/// output). Centered as a pair under the `title_lines` title lines; sized to fit each
/// label plus [`BUTTON_PADDING_X`] on every side. Takes no prompt-specific
/// input - the labels are fixed strings ([`DENY_LABEL`]/[`ALLOW_LABEL`]), so the layout never changes.
fn button_rects(title_lines: usize) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
    let height = button_height();
    let deny_width = Canvas::text_width(DENY_LABEL, FONT_SCALE) + BUTTON_PADDING_X * 2;
    let allow_width = Canvas::text_width(ALLOW_LABEL, FONT_SCALE) + BUTTON_PADDING_X * 2;
    let content_width = PROMPT_WIDTH - PROMPT_PADDING * 2;
    let row_width = deny_width + BUTTON_GAP + allow_width;
    let row_x = PROMPT_PADDING + (content_width - row_width) / 2;
    let row_y = PROMPT_PADDING + title_lines as i32 * LINE_HEIGHT + BUTTON_ROW_TOP_GAP;

    let deny = Rectangle::new((row_x, row_y).into(), (deny_width, height).into());
    let allow = Rectangle::new(
        (row_x + deny_width + BUTTON_GAP, row_y).into(),
        (allow_width, height).into(),
    );
    (deny, allow)
}

/// The prompt's title text, word-wrapped to the prompt's content width so a
/// long app id/executable path or reason grows the prompt downwards instead
/// of overflowing it. A single word wider than a line (a long path, say) is
/// broken across lines. Always at least one line.
fn wrap_title(app_id: &str, reason: &str) -> Vec<String> {
    let title = format!("{app_id} wants to {reason}");
    let char_width = Canvas::text_width("x", FONT_SCALE);
    let max_chars = ((PROMPT_WIDTH - PROMPT_PADDING * 2) / char_width).max(1) as usize;

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in title.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        while word.len() > max_chars {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            lines.push(word.drain(..max_chars).collect());
        }
        let word: String = word.into_iter().collect();
        if word.is_empty() {
            continue;
        }
        let needed = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if needed > max_chars {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&word);
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Total height of a prompt whose title takes `title_lines` lines.
fn prompt_height(title_lines: usize) -> i32 {
    PROMPT_PADDING * 2 + title_lines as i32 * LINE_HEIGHT + BUTTON_ROW_TOP_GAP + button_height()
}

/// State of the `ironland_permission_prompt_manager_v1` global.
#[derive(Debug)]
pub struct PermissionPromptManagerState {
    global: GlobalId,
    queue: VecDeque<PendingPrompt>,
    buffer: Option<MemoryRenderBuffer>,
    dirty: bool,
}

impl PermissionPromptManagerState {
    /// Registers the global, visible only to clients for which `filter`
    /// returns `true` - in practice, clients connected through the
    /// privileged capture socket (see `crate::screencopy`'s module doc),
    /// the same gate `ext-image-capture-source-v1`/
    /// `ext-image-copy-capture-v1` use.
    pub fn new<D, F>(dh: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<IronlandPermissionPromptManagerV1, ManagerGlobalData> + 'static,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = dh.create_global::<D, IronlandPermissionPromptManagerV1, _>(
            1,
            ManagerGlobalData {
                filter: Box::new(filter),
            },
        );
        PermissionPromptManagerState {
            global,
            queue: VecDeque::new(),
            buffer: None,
            dirty: false,
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Queues a prompt with no wire requester behind it - see
    /// [`PromptTarget::Internal`] and the module doc. A no-op if `subject`
    /// already has an internal prompt of the same `kind` in flight, so a
    /// storm of requests from the same still-undecided caller (e.g.
    /// repeated capture attempts - see `crate::screencopy`) doesn't queue a
    /// prompt per attempt.
    pub fn queue_internal(&mut self, kind: PromptKind, subject: String, reason: String) {
        let already_queued = self.queue.iter().any(
            |p| matches!(p.target, PromptTarget::Internal(k) if k == kind) && p.app_id == subject,
        );
        if already_queued {
            return;
        }

        self.dirty = true;
        self.queue.push_back(PendingPrompt {
            app_id: subject,
            reason,
            target: PromptTarget::Internal(kind),
        });
    }

    /// Whether the compositor's own prompt overlay should be showing.
    pub fn is_visible(&self) -> bool {
        !self.queue.is_empty()
    }

    /// How many lines the showing prompt's title wraps to (1 when nothing is
    /// showing, so sizes stay well-defined).
    fn front_title_lines(&self) -> usize {
        self.queue
            .front()
            .map_or(1, |front| wrap_title(&front.app_id, &front.reason).len())
    }

    /// The logical size of the prompt overlay, for centering by callers.
    /// Grows with the wrapped title.
    pub fn logical_size(&self) -> Size<i32, Logical> {
        Size::from((PROMPT_WIDTH, prompt_height(self.front_title_lines())))
    }

    /// Where this prompt is drawn on an output whose logical size is
    /// `output_size` - top-centered, a fixed distance down from the top
    /// edge. The one place this placement formula lives; `crate::winit`
    /// and `crate::udev` call it to position the render element, and
    /// [`Self::hit_test`] calls it to translate a pointer click into
    /// prompt-local coordinates, so the two can never disagree about where
    /// the prompt actually is.
    pub fn origin_in(&self, output_size: Size<i32, Logical>) -> Point<i32, Logical> {
        let prompt_size = self.logical_size();
        Point::from(((output_size.w - prompt_size.w) / 2, 24))
    }

    /// Tests a pointer click at `point` (logical coordinates on an output
    /// of `output_size`, both in that output's own space) against the
    /// currently-showing prompt's Allow/Deny buttons. `Some(true)` for a
    /// hit on Allow, `Some(false)` for Deny, `None` if the click missed
    /// both or nothing is showing.
    pub fn hit_test(&self, output_size: Size<i32, Logical>, point: Point<i32, Logical>) -> Option<bool> {
        if !self.is_visible() {
            return None;
        }
        let local = point - self.origin_in(output_size);
        let (deny, allow) = button_rects(self.front_title_lines());
        if allow.contains(local) {
            Some(true)
        } else if deny.contains(local) {
            Some(false)
        } else {
            None
        }
    }

    fn rasterize(front: &PendingPrompt) -> Canvas {
        let width = PROMPT_WIDTH;
        let lines = wrap_title(&front.app_id, &front.reason);
        let height = prompt_height(lines.len());
        let mut canvas = Canvas::new(width as usize, height as usize, COLOR_BACKGROUND);

        for (i, line) in lines.iter().enumerate() {
            canvas.draw_text(
                PROMPT_PADDING,
                PROMPT_PADDING + i as i32 * LINE_HEIGHT,
                line,
                FONT_SCALE,
                COLOR_TITLE,
            );
        }

        let (deny, allow) = button_rects(lines.len());
        canvas.fill_rect(deny.loc.x, deny.loc.y, deny.size.w, deny.size.h, COLOR_DENY_BUTTON);
        canvas.fill_rect(
            allow.loc.x,
            allow.loc.y,
            allow.size.w,
            allow.size.h,
            COLOR_ALLOW_BUTTON,
        );

        let deny_label_width = Canvas::text_width(DENY_LABEL, FONT_SCALE);
        canvas.draw_text(
            deny.loc.x + (deny.size.w - deny_label_width) / 2,
            deny.loc.y + BUTTON_PADDING_Y,
            DENY_LABEL,
            FONT_SCALE,
            COLOR_BUTTON_TEXT,
        );
        let allow_label_width = Canvas::text_width(ALLOW_LABEL, FONT_SCALE);
        canvas.draw_text(
            allow.loc.x + (allow.size.w - allow_label_width) / 2,
            allow.loc.y + BUTTON_PADDING_Y,
            ALLOW_LABEL,
            FONT_SCALE,
            COLOR_BUTTON_TEXT,
        );

        canvas
    }

    /// Returns the prompt overlay buffer, rebuilding it if the showing
    /// prompt changed since the last frame. `None` whenever the queue is
    /// empty.
    pub fn ensure_buffer(&mut self) -> Option<&MemoryRenderBuffer> {
        let front = self.queue.front()?;
        if self.dirty || self.buffer.is_none() {
            let canvas = Self::rasterize(front);
            self.buffer = Some(MemoryRenderBuffer::from_slice(
                &canvas.pixels,
                Fourcc::Argb8888,
                (canvas.width as i32, canvas.height as i32),
                1,
                Transform::Normal,
                None,
            ));
            self.dirty = false;
        }
        self.buffer.as_ref()
    }
}

/// Notifies `prompt`'s target of `allow` - firing a protocol event for a
/// wire-requested prompt, or calling back into `state` for an internal one.
/// A free function (rather than a method) so it can reach
/// [`PermissionPromptHandler::internal_prompt_resolved`] on `state`.
fn send_answer<D: PermissionPromptHandler>(state: &mut D, prompt: PendingPrompt, allow: bool) {
    match prompt.target {
        PromptTarget::Requester(resource) => {
            if allow {
                resource.allowed();
            } else {
                resource.denied();
            }
        }
        PromptTarget::Internal(kind) => state.internal_prompt_resolved(kind, prompt.app_id, allow),
    }
}

/// Answers the currently-showing prompt (the compositor's own overlay, via
/// the user's Enter/Y or Escape/N - see `crate::input_handler`) and shows the next
/// queued one, if any. Returns whether a prompt was actually answered, so
/// callers know whether to consume the key.
pub fn answer<D: PermissionPromptHandler>(state: &mut D, allow: bool) -> bool {
    let prompt_state = state.permission_prompt_state();
    let Some(prompt) = prompt_state.queue.pop_front() else {
        return false;
    };
    prompt_state.dirty = true;
    prompt_state.buffer = None;
    send_answer(state, prompt, allow);
    true
}

/// Global data for the `ironland_permission_prompt_manager_v1` global: the
/// client filter it's gated behind.
#[allow(missing_debug_implementations)]
pub struct ManagerGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for ManagerGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerGlobalData").finish_non_exhaustive()
    }
}

/// User data attached to a bound `ironland_permission_prompt_manager_v1`
/// resource (nothing to carry).
#[derive(Debug)]
pub struct ManagerToken;

/// User data attached to an `ironland_permission_prompt_v1` resource
/// (nothing to carry beyond what's already queued).
#[derive(Debug)]
pub struct PromptToken;

impl<D> GlobalDispatch2<IronlandPermissionPromptManagerV1, D> for ManagerGlobalData
where
    D: PermissionPromptHandler
        + Dispatch<IronlandPermissionPromptManagerV1, ManagerToken>
        + Dispatch<IronlandPermissionPromptV1, PromptToken>,
{
    fn bind(
        &self,
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<IronlandPermissionPromptManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ManagerToken);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<IronlandPermissionPromptManagerV1, D> for ManagerToken
where
    D: PermissionPromptHandler + Dispatch<IronlandPermissionPromptV1, PromptToken>,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _manager: &IronlandPermissionPromptManagerV1,
        request: ironland_permission_prompt_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ironland_permission_prompt_manager_v1::Request::Prompt { id, app_id, reason } => {
                let resource = data_init.init(id, PromptToken);
                let prompt_state = state.permission_prompt_state();
                prompt_state.dirty = true;
                prompt_state.queue.push_back(PendingPrompt {
                    app_id,
                    reason,
                    target: PromptTarget::Requester(resource),
                });
            }
            ironland_permission_prompt_manager_v1::Request::Destroy => {}
        }
    }
}

impl<D: PermissionPromptHandler> Dispatch2<IronlandPermissionPromptV1, D> for PromptToken {
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &IronlandPermissionPromptV1,
        request: ironland_permission_prompt_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let ironland_permission_prompt_v1::Request::Destroy = request;
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &IronlandPermissionPromptV1) {
        let prompt_state = state.permission_prompt_state();
        let Some(pos) = prompt_state.queue.iter().position(
            |p| matches!(&p.target, PromptTarget::Requester(r) if r == resource),
        ) else {
            return;
        };
        let was_front = pos == 0;
        prompt_state.queue.remove(pos);

        if was_front {
            prompt_state.dirty = true;
            prompt_state.buffer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn max_chars() -> usize {
        ((PROMPT_WIDTH - PROMPT_PADDING * 2) / Canvas::text_width("x", FONT_SCALE)) as usize
    }

    #[test]
    fn short_title_stays_on_one_line() {
        assert_eq!(wrap_title("foot", "capture your screen"), vec!["foot wants to capture your screen"]);
    }

    #[test]
    fn long_title_wraps_within_the_content_width() {
        let lines = wrap_title("an unidentified application with a very long name", "record your entire screen and more");
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= max_chars()));
    }

    #[test]
    fn overlong_word_is_broken_across_lines() {
        let path = format!("/nix/store/{}/bin/app", "a".repeat(120));
        let lines = wrap_title(&path, "capture your screen");
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|l| l.chars().count() <= max_chars()));
        assert_eq!(lines.join("").replace(' ', ""), format!("{path}wantstocaptureyourscreen"));
    }

    #[test]
    fn more_lines_make_a_taller_prompt() {
        assert!(prompt_height(3) > prompt_height(1));
        let (_, one) = button_rects(1);
        let (_, three) = button_rects(3);
        assert_eq!(three.loc.y - one.loc.y, 2 * LINE_HEIGHT);
    }
}
