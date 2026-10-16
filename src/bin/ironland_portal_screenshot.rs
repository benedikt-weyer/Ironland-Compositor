//! `org.freedesktop.impl.portal.Screenshot` backend for
//! `xdg-desktop-portal`, bridging it to `ext-image-capture-source-v1` +
//! `ext-image-copy-capture-v1` (both implemented server-side by Smithay
//! itself; the actual pixel capture is `crate::screencopy` - see its module
//! doc) over the compositor's *privileged* capture Wayland socket
//! (`$IRONLAND_CAPTURE_SOCKET`, set by the compositor and exported to
//! systemd/D-Bus activation environment the same way `$WAYLAND_DISPLAY` is
//! - see `crate::session`). That socket isn't itself what gates capture
//!   (see `crate::screencopy`'s module doc - the compositor gates every
//!   capturer, this binary included, by its own executable identity); this
//!   binary just also needs `ironland-permission-prompt-v1`, which *is*
//!   restricted to it.
//!
//! Like `ironland-portal-global-shortcuts`, this is a small standalone
//! Wayland client - not part of the compositor binary - meant to be D-Bus
//! activated by `xdg-desktop-portal`. Packaging:
//! `resources/ironland-screenshot.portal` registers this as the
//! `Screenshot` backend under its own bus name (a *different* bus name
//! than `ironland-portal-global-shortcuts` uses, since two processes can't
//! own the same one); the matching `.service` file's `Exec=` is generated
//! by `flake.nix`.
//!
//! ## Permission model
//!
//! `Screenshot()` requests are gated per requesting `app_id` *on top of*
//! the compositor's own per-executable gate on `crate::screencopy` - two
//! independent layers, since this binary can't vouch for who it's
//! capturing on behalf of any more than a client-supplied `app_id` string
//! can be trusted on its own:
//!
//! - A decision already on file in the persisted [`Store`]
//!   (`$XDG_CONFIG_HOME/ironland-compositor/screenshot-permissions.json`,
//!   `Allow`/`Deny`) is used directly, with no prompt - required so a
//!   scripted/automated caller doesn't get stuck waiting on a human every
//!   time.
//! - Otherwise, `ironland-permission-prompt-v1` asks the compositor to show
//!   an on-screen "`app_id` wants to capture your screen" prompt (see
//!   `crate::permission_prompt`) and blocks on the user's Enter/Escape
//!   answer, which is then persisted for next time.
//!
//! Even once this layer approves an `app_id`, the actual `capture` request
//! still has to clear the compositor's own gate on this binary's
//! executable path (see `crate::screencopy`'s module doc) - on a fresh
//! install that means the *very first* `Screenshot()` call ever, from any
//! app, fails with a compositor-drawn (or shell-rendered) prompt asking to
//! approve `ironland-portal-screenshot` itself; the caller has to be asked
//! again afterward; every call after that first approval only needs the
//! per-`app_id` layer above.
//!
//! `app_id` here is whatever xdg-desktop-portal passes us, which for a
//! non-sandboxed caller can be empty - such callers share one persisted
//! decision, the same caveat `ironland-portal-global-shortcuts` documents
//! for its own per-app_id store.
//!
//! ## Scope
//!
//! Always captures the first advertised output as a whole (no monitor
//! picker, no interactive region/window selection - `options.interactive`
//! is accepted but ignored). `PickColor` isn't implemented.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output::WlOutput, wl_registry, wl_shm};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

/// Generated client bindings for `ironland-permission-prompt-v1` (see
/// `protocols/ironland-permission-prompt-v1.xml`) - not in the
/// `wayland-protocols` crate since it's our own, generated here the same
/// way `ironland-portal-global-shortcuts` generates its shortcuts bindings.
mod permission_prompt_protocol {
    #![allow(
        dead_code,
        non_camel_case_types,
        unused_imports,
        missing_docs,
        clippy::all
    )]

    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/ironland-permission-prompt-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocols/ironland-permission-prompt-v1.xml");
}
use permission_prompt_protocol::ironland_permission_prompt_manager_v1::IronlandPermissionPromptManagerV1;
use permission_prompt_protocol::ironland_permission_prompt_v1::{self, IronlandPermissionPromptV1};

const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.ironland.screenshot";

/// How long a single `Screenshot()` request will wait on the compositor
/// (through capture negotiation, or through a human answering the
/// permission prompt) before giving up and reporting failure.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------
// Persisted per-app_id grants.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Grant {
    Allow,
    Deny,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(flatten)]
    grants: HashMap<String, Grant>,
}

fn store_path() -> PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string())).join(".config")
        });
    config_home
        .join("ironland-compositor")
        .join("screenshot-permissions.json")
}

impl Store {
    fn load() -> Self {
        match std::fs::read_to_string(store_path()) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Store::default(),
        }
    }

    fn save(&self) {
        let path = store_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

fn screenshots_dir() -> PathBuf {
    let cache_home = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
                .join(".cache")
        });
    cache_home.join("ironland-compositor").join("screenshots")
}

// ---------------------------------------------------------------------
// Wayland side: one fresh connection per request, driven synchronously by
// blocking dispatch - simplest correct thing for a client that only ever
// does one short-lived exchange at a time.
// ---------------------------------------------------------------------

#[derive(Default)]
struct SessionState {
    buffer_size: Option<(u32, u32)>,
    shm_formats: Vec<wl_shm::Format>,
    done: bool,
    stopped: bool,
}

#[derive(Default)]
struct FrameState {
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct PromptState {
    answered: Option<bool>,
}

struct App {
    session: SessionState,
    frame: FrameState,
    prompt: PromptState,
}

wayland_client::delegate_noop!(App: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(App: ignore wayland_client::protocol::wl_buffer::WlBuffer);
wayland_client::delegate_noop!(App: ignore WlOutput);
wayland_client::delegate_noop!(App: ignore ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(App: ignore ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(App: ignore ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(App: ignore IronlandPermissionPromptManagerV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.session.buffer_size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: wayland_client::WEnum::Value(format),
            } => {
                state.session.shm_formats.push(format);
            }
            ext_image_copy_capture_session_v1::Event::Done => state.session.done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => state.session.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.frame.ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => state.frame.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<IronlandPermissionPromptV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &IronlandPermissionPromptV1,
        event: ironland_permission_prompt_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ironland_permission_prompt_v1::Event::Allowed => state.prompt.answered = Some(true),
            ironland_permission_prompt_v1::Event::Denied => state.prompt.answered = Some(false),
        }
    }
}

/// Blocking-dispatches `queue` until `done` returns `true` or `deadline`
/// passes, returning whether it finished in time.
fn wait_until(
    queue: &mut EventQueue<App>,
    app: &mut App,
    deadline: Instant,
    mut done: impl FnMut(&App) -> bool,
) -> bool {
    while !done(app) {
        if Instant::now() >= deadline {
            return false;
        }
        if queue.blocking_dispatch(app).is_err() {
            return false;
        }
    }
    true
}

/// Asks the compositor to show a permission prompt and blocks for the
/// user's answer. Returns `None` on any protocol/timeout failure (treated
/// as a denial by the caller).
fn ask_permission(app_id: &str, reason: &str) -> Option<bool> {
    let conn = connect_capture_socket()?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn).ok()?;
    let qh = queue.handle();
    let manager = globals
        .bind::<IronlandPermissionPromptManagerV1, _, _>(&qh, 1..=1, ())
        .ok()?;

    let mut app = App {
        session: SessionState::default(),
        frame: FrameState::default(),
        prompt: PromptState::default(),
    };
    manager.prompt(app_id.to_string(), reason.to_string(), &qh, ());

    let deadline = Instant::now() + REQUEST_TIMEOUT;
    if !wait_until(&mut queue, &mut app, deadline, |a| {
        a.prompt.answered.is_some()
    }) {
        return None;
    }
    app.prompt.answered
}

fn connect_capture_socket() -> Option<Connection> {
    let socket_name = std::env::var("IRONLAND_CAPTURE_SOCKET").ok()?;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let path = PathBuf::from(runtime_dir).join(socket_name);
    let stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    Connection::from_socket(stream).ok()
}

/// Creates an anonymous (create + immediately unlink) file of `size` bytes,
/// for use as an `wl_shm` pool - avoids depending on a `memfd`/`tempfile`
/// crate for what's otherwise a two-line trick.
fn anon_file(size: u64) -> std::io::Result<File> {
    let path = std::env::temp_dir().join(format!(
        "ironland-screenshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    file.set_len(size)?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

/// Captures the first output and returns the path to a saved PNG.
fn capture_screenshot() -> Result<PathBuf, String> {
    let conn = connect_capture_socket().ok_or("no privileged capture socket available")?;
    let (globals, mut queue) =
        registry_queue_init::<App>(&conn).map_err(|err| format!("registry init failed: {err}"))?;
    let qh = queue.handle();

    let shm = globals
        .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
        .map_err(|err| format!("no wl_shm: {err}"))?;
    let output = globals
        .bind::<WlOutput, _, _>(&qh, 1..=4, ())
        .map_err(|err| format!("no wl_output: {err}"))?;
    let output_source_manager = globals
        .bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|err| format!("compositor doesn't support ext-image-capture-source-v1: {err}"))?;
    let copy_capture_manager = globals
        .bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|err| format!("compositor doesn't support ext-image-copy-capture-v1: {err}"))?;

    let mut app = App {
        session: SessionState::default(),
        frame: FrameState::default(),
        prompt: PromptState::default(),
    };

    let source = output_source_manager.create_source(&output, &qh, ());
    let session = copy_capture_manager.create_session(&source, Options::empty(), &qh, ());

    let deadline = Instant::now() + REQUEST_TIMEOUT;
    if !wait_until(&mut queue, &mut app, deadline, |a| {
        a.session.done || a.session.stopped
    }) {
        return Err("timed out waiting for capture session constraints".into());
    }
    if app.session.stopped {
        return Err("capture session was stopped by the compositor".into());
    }
    let (width, height) = app.session.buffer_size.ok_or("no buffer_size advertised")?;
    if !app.session.shm_formats.contains(&wl_shm::Format::Argb8888) {
        return Err("compositor didn't offer Argb8888 shm capture".into());
    }

    let stride = width * 4;
    let pool_size = stride as u64 * height as u64;
    let file = anon_file(pool_size).map_err(|err| format!("failed to create shm buffer: {err}"))?;
    let pool = shm.create_pool(file.as_fd(), pool_size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );

    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    let deadline = Instant::now() + REQUEST_TIMEOUT;
    if !wait_until(&mut queue, &mut app, deadline, |a| {
        a.frame.ready || a.frame.failed
    }) {
        return Err("timed out waiting for capture to complete".into());
    }
    if app.frame.failed {
        return Err("compositor reported capture failure".into());
    }

    let mut raw = vec![0u8; pool_size as usize];
    let mut file = file;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut raw))
        .map_err(|err| format!("failed to read back captured pixels: {err}"))?;

    // Argb8888 is little-endian byte order B,G,R,A in memory; `image`
    // wants R,G,B,A.
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let dir = screenshots_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create screenshots dir: {err}"))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = dir.join(format!("Screenshot-{timestamp}.png"));

    image::save_buffer(&path, &raw, width, height, image::ColorType::Rgba8)
        .map_err(|err| format!("failed to encode png: {err}"))?;

    session.destroy();
    source.destroy();

    Ok(path)
}

// ---------------------------------------------------------------------
// D-Bus side: `org.freedesktop.impl.portal.Screenshot`.
// ---------------------------------------------------------------------

struct ScreenshotIface {
    store: std::sync::Mutex<Store>,
}

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotIface {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    #[allow(clippy::too_many_arguments)]
    fn screenshot(
        &self,
        _handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let grant = {
            let store = self.store.lock().unwrap();
            store.grants.get(&app_id).copied()
        };

        let allowed = match grant {
            Some(Grant::Allow) => true,
            Some(Grant::Deny) => false,
            None => {
                let shown_id = if app_id.is_empty() {
                    "An application"
                } else {
                    &app_id
                };
                let answer = ask_permission(shown_id, "take a screenshot").unwrap_or(false);
                let mut store = self.store.lock().unwrap();
                store.grants.insert(
                    app_id.clone(),
                    if answer { Grant::Allow } else { Grant::Deny },
                );
                store.save();
                answer
            }
        };

        if !allowed {
            return (1, HashMap::new()); // cancelled/denied
        }

        match capture_screenshot() {
            Ok(path) => {
                let uri = format!("file://{}", path.display());
                let mut results = HashMap::new();
                results.insert("uri".to_string(), Value::from(uri).try_to_owned().unwrap());
                (0, results)
            }
            Err(err) => {
                eprintln!("ironland-portal-screenshot: capture failed: {err}");
                (2, HashMap::new()) // error
            }
        }
    }
}

fn main() {
    let store = Store::load();
    let iface = ScreenshotIface {
        store: std::sync::Mutex::new(store),
    };

    let connection =
        match async_io::block_on(async { zbus::connection::Builder::session()?.build().await }) {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("ironland-portal-screenshot: failed to connect to session bus: {err}");
                std::process::exit(1);
            }
        };

    if let Err(err) = async_io::block_on(connection.object_server().at(PORTAL_PATH, iface)) {
        eprintln!("ironland-portal-screenshot: failed to register Screenshot interface: {err}");
        std::process::exit(1);
    }

    // Claim the bus name only once the interface is exported: xdg-desktop-portal
    // reads this backend's properties (e.g. `AvailableSourceTypes`) once, when
    // the name appears, and never re-reads them, so an interface that shows up
    // after the name leaves it cached as empty.
    if let Err(err) = async_io::block_on(connection.request_name(BUS_NAME)) {
        eprintln!("ironland-portal-screenshot: failed to request bus name: {err}");
        std::process::exit(1);
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
