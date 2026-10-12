//! Real-time output capture backing `ext-image-capture-source-v1`/
//! `ext-image-copy-capture-v1` (both implemented by Smithay itself - see
//! `smithay::wayland::image_capture_source`/`image_copy_capture` - this
//! module only supplies the parts Smithay leaves to the compositor: the
//! actual pixel copy, and this compositor's own permission model).
//!
//! ## Permission model
//!
//! The `ext_output_image_capture_source_manager_v1`/
//! `ext_image_copy_capture_manager_v1` globals themselves are unrestricted
//! (any client can bind them and negotiate a session - session setup alone
//! never reads screen content), *unlike* most other wlroots-style
//! compositors' `wlr-screencopy-v1`, which usually stops there: this
//! compositor gates the one point that actually matters instead, moving
//! consent from the protocol layer to the pixel layer.
//!
//! Every time a client's `ext_image_copy_capture_frame_v1.capture` request
//! would actually read screen content
//! ([`ImageCopyCaptureHandler::frame`][state] in `crate::state`), the
//! requesting client is identified by [`client_identity`] - the executable
//! path behind its Wayland connection, resolved via the kernel and never
//! anything the client itself asserts (see [`client_identity`]'s own doc
//! for exactly how, and why it's more involved than a plain `/proc/<pid>/
//! exe` read). That path is looked up in [`ScreencopyState`]'s grant table:
//!
//! - A prior **Allow** completes the capture immediately.
//! - A prior **Deny**, or no decision yet, fails the capture
//!   ([`CaptureFailureReason::Unknown`]) without reading any pixels. With
//!   no decision on file, it additionally queues an on-screen prompt (see
//!   `crate::permission_prompt`) naming that executable path; the client's
//!   *next* capture attempt (after the user answers - most callers,
//!   including this compositor's own shell's screenshot tool and
//!   `ironland-portal-screenshot`, simply retry on failure) succeeds or
//!   fails accordingly, and the decision is remembered from then on.
//!
//! This means every capturer - this compositor's own shell included - goes
//! through the exact same gate; nothing is special-cased.
//!
//! The grant table is **in-memory only, for this compositor process's own
//! lifetime** - not persisted to disk. A resolved executable path is only a
//! trustworthy, *stable* identity for as long as the binary it names hasn't
//! been replaced or moved since it was granted; nothing on a general-
//! purpose Linux system guarantees that across time (packages get
//! upgraded, distro-specific store/cache paths get rewritten on rebuild,
//! users move their own binaries around). Rather than chase that per
//! packaging scheme - or worse, silently paper over it with a weaker,
//! spoofable match like "same filename" - every grant simply expires when
//! the compositor restarts, which on most setups coincides with exactly
//! the moments (reboot, session restart) such changes tend to land anyway.
//! What's missing to do better is an *actual* kernel-backed persistent
//! identity for "this specific program, even after it's rebuilt/updated" -
//! not something available on Linux today.
//!
//! [state]: crate::state
//!
//! `ironland-portal-screenshot` (the `org.freedesktop.impl.portal.
//! Screenshot` xdg-desktop-portal backend used by sandboxed/third-party
//! apps) is just another caller of this same gate: it runs through its own
//! executable path like anyone else, with its *own*, separate per-`app_id`
//! prompt/grant layer on top (see that binary's module doc) for
//! distinguishing between the different apps it captures on behalf of.
//!
//! ## Pixel capture
//!
//! [`fulfill`] is the actual capture: given pixels already sitting in a
//! readable framebuffer, it reads them back via
//! [`ExportMem::copy_framebuffer`] and copies them into every pending
//! capture frame's client-provided shm buffer. Both backends call it
//! immediately after rendering an output's frame, but get there
//! differently:
//!
//! - `crate::winit`'s bound framebuffer is directly readable, so it calls
//!   [`fulfill`] straight away.
//! - `crate::udev`'s DRM/KMS compositor may instead have scanned a client's
//!   buffer out directly (direct scanout) without ever compositing it into
//!   a readable framebuffer, so `crate::udev::capture_udev_frame` first
//!   re-composites the same `RenderFrameResult` into an offscreen
//!   renderbuffer via `RenderFrameResult::blit_frame_result` (which blits
//!   the scanout plane's dmabuf in like any other element), then calls
//!   [`fulfill`] on that.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use rustix::process::{Pid, PidfdFlags, pidfd_open};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ExportMem, Renderer};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size, Transform};
use smithay::wayland::image_copy_capture::{CaptureFailureReason, Frame, Session, SessionRef};
use smithay::wayland::shm::with_buffer_contents_mut;

/// Marker in a capture source's user data: this session has already had its
/// first capture request queued (see `ImageCopyCaptureHandler::frame` in
/// `crate::state`).
pub struct FirstCaptureDone;

/// Compositor-side state for the image-copy-capture pipeline: sessions kept
/// alive for their duration, capture requests waiting on the next rendered
/// frame of their output, and the in-memory (see the module doc) per-
/// executable capture grants gating them.
#[derive(Debug, Default)]
pub struct ScreencopyState {
    /// Owned sessions, kept alive as long as the client's session object is;
    /// see `ImageCopyCaptureHandler::new_session`/`session_destroyed` in
    /// `crate::state`.
    pub sessions: Vec<Session>,
    /// Frame captures requested (`ext_image_copy_capture_frame_v1.capture`)
    /// but not yet fulfilled, keyed by output name. Drained by [`fulfill`]
    /// after the next successful render of that output.
    pending: HashMap<String, Vec<Frame>>,
    /// Per-executable-path capture decisions, for this compositor process's
    /// lifetime only; see [`Self::grant`]/[`Self::set_grant`] and the
    /// module doc for why these deliberately aren't persisted to disk.
    grants: HashMap<String, bool>,
}

impl ScreencopyState {
    pub fn queue_frame(&mut self, output_name: String, frame: Frame) {
        self.pending.entry(output_name).or_default().push(frame);
    }

    /// Takes every capture request still waiting on `output`'s next
    /// rendered frame, for [`fulfill`] to complete.
    pub fn take_pending(&mut self, output: &Output) -> Vec<Frame> {
        self.pending.remove(&output.name()).unwrap_or_default()
    }

    pub fn has_pending(&self, output: &Output) -> bool {
        self.pending
            .get(&output.name())
            .is_some_and(|frames| !frames.is_empty())
    }

    /// Drops the `Session` matching `destroyed` (if any is currently owned),
    /// letting its `Drop` impl stop it and fail any frames still active on
    /// it. Called from `ImageCopyCaptureHandler::session_destroyed`.
    pub fn forget_session(&mut self, destroyed: &SessionRef) {
        self.sessions.retain(|session| session != destroyed);
    }

    /// The decision on file for `subject` (an executable path from
    /// [`client_identity`]), if any.
    pub fn grant(&self, subject: &str) -> Option<bool> {
        self.grants.get(subject).copied()
    }

    /// Records a decision for `subject`, for the rest of this compositor
    /// process's lifetime (see the module doc). Called back via
    /// `PermissionPromptHandler::internal_prompt_resolved` once the user
    /// answers a prompt [`PermissionPromptManagerState::queue_internal`]
    /// queued (see `crate::state`'s impl of that trait).
    ///
    /// [`PermissionPromptManagerState::queue_internal`]: crate::permission_prompt::PermissionPromptManagerState::queue_internal
    pub fn set_grant(&mut self, subject: String, allowed: bool) {
        self.grants.insert(subject, allowed);
    }
}

/// Identifies the executable behind `client`'s Wayland connection, for
/// [`ScreencopyState`]'s grant table - see the module doc for why this
/// (rather than any client-supplied string) is what capture decisions are
/// keyed and prompted on. Falls back to a placeholder (still safe to
/// prompt/gate on, just not a meaningful path) if identification fails for
/// any reason - e.g. the process has already exited, or `/proc` isn't
/// available.
///
/// This is more than a plain `/proc/<pid>/exe` read because a raw pid
/// number is, on its own, an unsafe thing to resolve lazily: `SO_PEERCRED`
/// (via [`Client::get_credentials`]) gives a pid the kernel guarantees was
/// genuinely this client's *at connect time*, frozen from then on - but if
/// that original process has since exited, Linux is free to hand that same
/// pid number to a completely unrelated later process, and a naive `/proc/
/// <pid>/exe` read done long after connecting could silently resolve to
/// *that* process instead. Calling this immediately after a client
/// connects (see `crate::state::insert_client_with_identity`, this
/// function's only caller) rather than lazily on first capture already
/// closes almost all of that window; using a `pidfd` closes the rest:
/// [`pidfd_open`] on that pid, taken immediately before and after the
/// `/proc/<pid>/exe` read, returns a kernel-stable handle to *that specific
/// task* - reused pids included, since a pidfd is bound to the task, not
/// the number - so comparing the two pidfds' identities (their backing
/// inode, stable since Linux 5.9) confirms the same task was alive and
/// unchanged for the read's entire duration, or otherwise discards the
/// result rather than risk misattributing it.
pub fn client_identity(dh: &DisplayHandle, client: &Client) -> String {
    client
        .get_credentials(dh)
        .ok()
        .and_then(|creds| resolve_exe_via_pidfd(creds.pid))
        .unwrap_or_else(|| "an unidentified application".to_string())
}

fn resolve_exe_via_pidfd(pid: i32) -> Option<String> {
    let rpid = Pid::from_raw(pid)?;
    let before = pidfd_open(rpid, PidfdFlags::empty()).ok()?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let after = pidfd_open(rpid, PidfdFlags::empty()).ok()?;

    let before_ino = std::fs::File::from(before).metadata().ok()?.ino();
    let after_ino = std::fs::File::from(after).metadata().ok()?.ino();
    (before_ino == after_ino).then(|| exe.to_string_lossy().into_owned())
}

/// The size (in buffer/physical pixels) `ext-image-copy-capture-v1` buffer
/// constraints and captures use for `output` - its current mode's size,
/// unaffected by logical scale.
pub fn output_buffer_size(output: &Output) -> Option<Size<i32, BufferCoord>> {
    let mode = output.current_mode()?;
    Some(
        mode.size
            .to_logical(1)
            .to_buffer(1, smithay::utils::Transform::Normal),
    )
}

/// Completes every frame in `pending` using the pixels currently in
/// `framebuffer`, which must already hold `size`'s worth of freshly
/// rendered, readable content (a backend calls this immediately after its
/// normal per-output render, while its renderer is still bound to that
/// output). No-ops (skips the readback entirely) if `pending` is empty.
pub fn fulfill<R>(
    pending: Vec<Frame>,
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    size: Size<i32, BufferCoord>,
    presented: Duration,
) where
    R: Renderer + ExportMem,
{
    if pending.is_empty() {
        return;
    }

    let region = Rectangle::from_size(size);
    let mapping = match renderer.copy_framebuffer(framebuffer, region, Fourcc::Argb8888) {
        Ok(mapping) => mapping,
        Err(_) => {
            for frame in pending {
                frame.fail(CaptureFailureReason::Unknown);
            }
            return;
        }
    };
    let bytes = match renderer.map_texture(&mapping) {
        Ok(bytes) => bytes,
        Err(_) => {
            for frame in pending {
                frame.fail(CaptureFailureReason::Unknown);
            }
            return;
        }
    };

    for frame in pending {
        if write_shm_argb8888(&frame.buffer(), bytes, size.w as u32, size.h as u32) {
            frame.success(Transform::Normal, None, presented);
        } else {
            frame.fail(CaptureFailureReason::BufferConstraints);
        }
    }
}

/// Copies a tightly-packed `Argb8888` source (as produced by
/// [`ExportMem::map_texture`]) into a client's shm-backed `wl_buffer`, row
/// by row to account for the two potentially differing strides. Returns
/// `false` (leaving the buffer untouched) if either is too small for
/// `width`x`height`.
fn write_shm_argb8888(buffer: &WlBuffer, src: &[u8], width: u32, height: u32) -> bool {
    let src_stride = width as usize * 4;
    let rows = height as usize;
    if src.len() < src_stride * rows {
        return false;
    }

    with_buffer_contents_mut(buffer, |ptr, len, data| {
        let dst_stride = data.stride as usize;
        let offset = data.offset as usize;
        if data.width as u32 != width
            || data.height as u32 != height
            || len < offset + dst_stride * rows
        {
            return false;
        }

        // Safety: `with_buffer_contents_mut` guarantees `ptr` is valid for
        // `len` bytes for the duration of this closure.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), len - offset) };
        for row in 0..rows {
            let src_row = &src[row * src_stride..row * src_stride + src_stride];
            let dst_row = &mut dst[row * dst_stride..row * dst_stride + src_stride];
            dst_row.copy_from_slice(src_row);
        }
        true
    })
    .unwrap_or(false)
}
