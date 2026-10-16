//! Screen lock: while locked, the compositor draws nothing but a small
//! password panel on a blank background, swallows every keyboard/pointer/
//! touch event, and only unlocks once the user's password checks out
//! against PAM.
//!
//! Authentication runs on a worker thread (PAM can block for seconds - on a
//! wrong password `pam_unix` deliberately delays) and reports back through
//! the `calloop` channel `AnvilState::init` wires up to
//! [`AnvilState::finish_unlock_attempt`]. The password never leaves this
//! process except through PAM's own conversation callback.
//!
//! The PAM service name is [`PAM_SERVICE`]; the NixOS module registers it
//! (`security.pam.services.ironland-compositor`). On a distro without that
//! file in `/etc/pam.d`, `pam_start` falls back to the `other` stack, which
//! normally denies everything - install a matching `/etc/pam.d` file there.
//!
//! Limits worth knowing: this locks the compositor's own input and output,
//! it does not (yet) implement `ext-session-lock-v1`, so shell lock screens
//! can't take over, and a VT switch stays possible (the other VT is
//! authenticated by its own login).

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    reexports::calloop::channel::Sender,
    utils::{Logical, Size, Transform},
};
use tracing::{debug, error, info};

use crate::font::{Canvas, GLYPH_HEIGHT};

/// `/etc/pam.d/<this>` is the PAM stack used to check the password.
pub const PAM_SERVICE: &str = "ironland-compositor";

const PANEL_WIDTH: i32 = 460;
const PANEL_HEIGHT: i32 = 190;
const PANEL_PADDING: i32 = 24;
const FONT_SCALE: i32 = 2;
const MAX_PASSWORD_CHARS: usize = 256;

const COLOR_BACKGROUND: [u8; 4] = [40, 34, 30, 255];
const COLOR_FIELD_BG: [u8; 4] = [70, 60, 52, 255];
const COLOR_TEXT: [u8; 4] = [235, 230, 225, 255];
const COLOR_MUTED_TEXT: [u8; 4] = [150, 140, 130, 255];
const COLOR_ERROR_TEXT: [u8; 4] = [235, 110, 100, 255];

/// Everything the lock screen needs between frames.
#[derive(Debug)]
pub struct LockState {
    locked: bool,
    password: String,
    message: Option<String>,
    /// A PAM check is in flight; input is ignored until it reports back.
    authenticating: bool,
    username: String,
    result_tx: Sender<bool>,
    buffer: Option<MemoryRenderBuffer>,
    dirty: bool,
}

impl LockState {
    pub fn new(result_tx: Sender<bool>) -> Self {
        Self {
            locked: false,
            password: String::new(),
            message: None,
            authenticating: false,
            username: current_username(),
            result_tx,
            buffer: None,
            dirty: true,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn lock(&mut self) {
        self.locked = true;
        self.reset_input();
        self.message = None;
        self.authenticating = false;
        self.username = current_username();
    }

    pub fn unlock(&mut self) {
        self.locked = false;
        self.reset_input();
        self.message = None;
        self.authenticating = false;
        self.buffer = None;
    }

    fn reset_input(&mut self) {
        // Best-effort scrub before the allocation is released.
        // SAFETY: zero bytes are valid UTF-8.
        unsafe { self.password.as_bytes_mut().fill(0) };
        self.password.clear();
        self.dirty = true;
    }

    pub fn push_char(&mut self, c: char) {
        if self.authenticating || self.password.chars().count() >= MAX_PASSWORD_CHARS {
            return;
        }
        self.password.push(c);
        self.message = None;
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if self.authenticating {
            return;
        }
        self.password.pop();
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        if !self.authenticating {
            self.reset_input();
        }
    }

    /// Starts checking the typed password in the background. The outcome
    /// arrives through the channel given to [`LockState::new`].
    pub fn submit(&mut self) {
        if self.authenticating || self.password.is_empty() {
            return;
        }
        let Ok(password) = CString::new(self.password.as_str()) else {
            self.reset_input();
            self.message = Some("Invalid password".into());
            return;
        };
        self.reset_input();
        self.authenticating = true;
        self.message = Some("Checking...".into());

        let username = self.username.clone();
        let tx = self.result_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("ironland-lock-pam".into())
            .spawn(move || {
                let ok = match check_password(PAM_SERVICE, &username, &password) {
                    Ok(()) => true,
                    Err(code) => {
                        info!(user = username, code, "Unlock attempt failed");
                        false
                    }
                };
                let _ = tx.send(ok);
            });
        if let Err(err) = spawned {
            error!(%err, "Failed to start PAM authentication thread");
            self.authenticating = false;
            self.message = Some("Authentication unavailable".into());
        }
    }

    /// Records a failed attempt; a success is handled by the caller via
    /// [`LockState::unlock`].
    pub fn attempt_failed(&mut self) {
        self.authenticating = false;
        self.message = Some("Wrong password".into());
        self.dirty = true;
    }

    pub fn logical_size(&self) -> Size<i32, Logical> {
        Size::from((PANEL_WIDTH, PANEL_HEIGHT))
    }

    fn rasterize(&self) -> Canvas {
        let mut canvas = Canvas::new(PANEL_WIDTH as usize, PANEL_HEIGHT as usize, COLOR_BACKGROUND);
        let line = GLYPH_HEIGHT as i32 * FONT_SCALE;

        canvas.draw_text(PANEL_PADDING, PANEL_PADDING, "Locked", FONT_SCALE * 2, COLOR_TEXT);
        canvas.draw_text(
            PANEL_PADDING,
            PANEL_PADDING + GLYPH_HEIGHT as i32 * FONT_SCALE * 2 + 8,
            &self.username,
            FONT_SCALE,
            COLOR_MUTED_TEXT,
        );

        let field_y = PANEL_HEIGHT / 2 + 4;
        let field_h = line + 16;
        canvas.fill_rect(PANEL_PADDING, field_y, PANEL_WIDTH - PANEL_PADDING * 2, field_h, COLOR_FIELD_BG);
        let dot = line - 4;
        let advance = dot + 8;
        let inner_w = PANEL_WIDTH - PANEL_PADDING * 2 - 24;
        let max_dots = ((inner_w + 8) / advance).max(1) as usize;
        let count = self.password.chars().count();
        // Show only the tail so the field never overflows.
        let shown = count.min(max_dots) as i32;
        for i in 0..shown {
            canvas.fill_circle(
                PANEL_PADDING + 12 + i * advance,
                field_y + (field_h - dot) / 2,
                dot,
                COLOR_TEXT,
            );
        }

        let (text, color) = match &self.message {
            Some(m) if self.authenticating => (m.as_str(), COLOR_MUTED_TEXT),
            Some(m) => (m.as_str(), COLOR_ERROR_TEXT),
            None => ("Enter password to unlock", COLOR_MUTED_TEXT),
        };
        canvas.draw_text(PANEL_PADDING, field_y + field_h + 14, text, FONT_SCALE, color);

        canvas
    }

    /// The panel to draw centered on each output, rebuilt when the state
    /// changed. `None` while unlocked.
    pub fn ensure_buffer(&mut self) -> Option<&MemoryRenderBuffer> {
        if !self.locked {
            return None;
        }
        if self.dirty || self.buffer.is_none() {
            let canvas = self.rasterize();
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

/// The login name to authenticate as: whoever runs the compositor.
fn current_username() -> String {
    let uid = rustix::process::getuid().as_raw();
    // The passwd entry is authoritative; `$USER` can be stale or spoofed.
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let mut fields = line.split(':');
            if let (Some(name), Some(_), Some(entry_uid)) =
                (fields.next(), fields.next(), fields.next())
                && entry_uid.parse::<u32>() == Ok(uid)
            {
                return name.to_string();
            }
        }
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

// --- minimal libpam binding -------------------------------------------------

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: extern "C" fn(
        num_msg: c_int,
        msg: *mut *const PamMessage,
        resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int,
    appdata_ptr: *mut c_void,
}

enum PamHandle {}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const PamConv,
        pamh: *mut *mut PamHandle,
    ) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut PamHandle, pam_status: c_int) -> c_int;
}

unsafe extern "C" {
    fn calloc(nmemb: usize, size: usize) -> *mut c_void;
    fn strdup(s: *const c_char) -> *mut c_char;
    fn free(ptr: *mut c_void);
}

/// Answers every password prompt with `appdata_ptr` (a `*const CStr`'s data);
/// informational/error messages need no answer.
extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
        return PAM_CONV_ERR;
    }
    let count = num_msg as usize;
    // SAFETY: PAM hands us `num_msg` valid message pointers; the response
    // array is calloc'd so PAM can free() it (and the strings in it).
    unsafe {
        let responses = calloc(count, size_of::<PamResponse>()) as *mut PamResponse;
        if responses.is_null() {
            return PAM_BUF_ERR;
        }
        for i in 0..count {
            let message = &**msg.add(i);
            match message.msg_style {
                PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                    let answer = strdup(appdata_ptr as *const c_char);
                    if answer.is_null() {
                        for j in 0..i {
                            let r = &mut *responses.add(j);
                            if !r.resp.is_null() {
                                free(r.resp as *mut c_void);
                            }
                        }
                        free(responses as *mut c_void);
                        return PAM_BUF_ERR;
                    }
                    (*responses.add(i)).resp = answer;
                }
                _ => {}
            }
        }
        *resp = responses;
    }
    PAM_SUCCESS
}

/// Checks `password` for `user` against the PAM stack `service`. `Err`
/// carries the PAM error code.
fn check_password(service: &str, user: &str, password: &CStr) -> Result<(), c_int> {
    let (Ok(service), Ok(user)) = (CString::new(service), CString::new(user)) else {
        return Err(-1);
    };
    let conv = PamConv {
        conv: conversation,
        appdata_ptr: password.as_ptr() as *mut c_void,
    };
    let mut handle: *mut PamHandle = ptr::null_mut();
    // SAFETY: all pointers outlive the calls below; `conv` and `password`
    // are only borrowed for the duration of this function.
    unsafe {
        let mut status = pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle);
        if status != PAM_SUCCESS {
            return Err(status);
        }
        status = pam_authenticate(handle, 0);
        if status == PAM_SUCCESS {
            // Rejects expired/locked accounts that still know their password.
            status = pam_acct_mgmt(handle, 0);
        }
        debug!(status, "PAM finished");
        pam_end(handle, status);
        if status == PAM_SUCCESS { Ok(()) } else { Err(status) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::calloop::channel::channel;

    fn state() -> LockState {
        LockState::new(channel::<bool>().0)
    }

    #[test]
    fn typing_edits_the_password_only_while_locked_and_idle() {
        let mut lock = state();
        assert!(!lock.is_locked());
        assert!(lock.ensure_buffer().is_none());

        lock.lock();
        assert!(lock.ensure_buffer().is_some());
        lock.push_char('a');
        lock.push_char('b');
        lock.backspace();
        assert_eq!(lock.password, "a");

        lock.authenticating = true;
        lock.push_char('x');
        lock.backspace();
        assert_eq!(lock.password, "a");
    }

    #[test]
    fn failed_attempt_keeps_the_lock_and_shows_a_message() {
        let mut lock = state();
        lock.lock();
        lock.authenticating = true;
        lock.attempt_failed();
        assert!(lock.is_locked());
        assert!(!lock.authenticating);
        assert_eq!(lock.message.as_deref(), Some("Wrong password"));

        lock.unlock();
        assert!(!lock.is_locked());
    }

    #[test]
    fn empty_password_is_not_submitted() {
        let mut lock = state();
        lock.lock();
        lock.submit();
        assert!(!lock.authenticating);
    }
}
