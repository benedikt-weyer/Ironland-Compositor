//! Server side of the `ironland-cinema-mode-v1` protocol (see
//! `crate::ironland_protocols::cinema_mode` for the generated bindings and
//! `protocols/ironland-cinema-mode-v1.xml` for the wire format).
//!
//! A client (the shell's cinema mode toggle) asks for one output to stay lit
//! while all the others go dark, optionally with a glow on the dark ones. The
//! renderers read the outcome back through [`CinemaModeState::current`] each
//! frame and draw it with `crate::cinema`. Like the display filter, the mode
//! lives exactly as long as the protocol object that requested it, so a
//! crashed client can't leave the screens black, and nothing here asks for
//! permission: the effect is visual only and undone by the same client. Only
//! clients running inside a VM (`ClientState::vm`) don't see the global.

use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    backend::{ClientId, GlobalId},
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::{
    ClientState,
    ironland_protocols::cinema_mode::ironland_cinema_mode_v1::{self, IronlandCinemaModeV1},
};

/// How long the dark outputs take to fade in or out.
pub const FADE: Duration = Duration::from_millis(600);
/// Strongest glow a client can ask for; larger values are clamped.
pub const MAX_GLOW: u32 = 100;

/// Implemented by the compositor state so this module can store cinema mode
/// requests without depending on `AnvilState` directly.
pub trait CinemaModeHandler: 'static {
    fn cinema_mode_state(&mut self) -> &mut CinemaModeState;
    /// Name of the output the user is working on, for `enable` requests that
    /// don't name one.
    fn cinema_mode_default_output(&self) -> Option<String>;
}

#[derive(Debug)]
struct Entry {
    resource: IronlandCinemaModeV1,
    /// `None` until this object's first request, so merely binding the
    /// global never overrides another client's cinema mode.
    request: Option<Request>,
}

/// What one object has asked for so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Request {
    /// Request order, so the most recent request wins when several live
    /// objects disagree.
    serial: u64,
    /// The output that stays lit, `None` while the mode is off.
    lit_output: Option<String>,
    glow: u32,
}

/// The configuration cinema mode is currently showing (or fading out).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    lit_output: String,
    glow: u32,
}

/// What the renderers draw this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Cinema {
    /// Name of the output that stays lit.
    pub lit_output: String,
    /// Glow strength, 0 (none) to 1.
    pub glow: f32,
    /// How far the dark outputs have faded to black, 0 to 1.
    pub level: f32,
}

#[derive(Debug)]
pub struct CinemaModeState {
    global: GlobalId,
    entries: Vec<Entry>,
    next_serial: u64,
    fade: Fade,
    /// Kept while fading out, when nobody requests anything any more.
    shown: Option<Shown>,
}

/// A linear fade of the blackout level between 0 and 1.
#[derive(Debug, Default)]
struct Fade {
    level: f32,
    updated: Option<Instant>,
}

impl Fade {
    /// Moves the level towards `on`'s end by the time passed since the last
    /// call and returns it.
    fn advance(&mut self, on: bool, now: Instant) -> f32 {
        let step = self.updated.map_or(0.0, |at| {
            now.saturating_duration_since(at).as_secs_f32() / FADE.as_secs_f32()
        });
        self.updated = Some(now);
        self.level = if on {
            (self.level + step).min(1.0)
        } else {
            (self.level - step).max(0.0)
        };
        self.level
    }
}

impl CinemaModeState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<IronlandCinemaModeV1, GlobalData> + 'static,
    {
        let global = dh.create_global::<D, IronlandCinemaModeV1, _>(1, GlobalData);
        CinemaModeState {
            global,
            entries: Vec::new(),
            next_serial: 0,
            fade: Fade::default(),
            shown: None,
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// What to render right now, or `None` when every output shows normally:
    /// the requested mode eased in and out over [`FADE`]. Call once per
    /// frame; it is cheap and safe to call per output.
    pub fn current(&mut self) -> Option<Cinema> {
        self.current_at(Instant::now())
    }

    fn current_at(&mut self, now: Instant) -> Option<Cinema> {
        let wanted = self.wanted();
        let level = self.fade.advance(wanted.is_some(), now);
        if wanted.is_some() {
            self.shown = wanted;
        } else if level <= 0.0 {
            self.shown = None;
        }
        self.shown
            .as_ref()
            .filter(|_| level > 0.0)
            .map(|shown| Cinema {
                lit_output: shown.lit_output.clone(),
                glow: shown.glow as f32 / MAX_GLOW as f32,
                level,
            })
    }

    /// The mode the clients ask for: the latest request of the live objects,
    /// if it switched cinema mode on.
    fn wanted(&self) -> Option<Shown> {
        latest(self.entries.iter().filter_map(|e| e.request.as_ref())).and_then(|request| {
            Some(Shown {
                lit_output: request.lit_output.clone()?,
                glow: request.glow,
            })
        })
    }

    /// Applies `change` to the request of `resource`'s object, as the newest.
    fn update(&mut self, resource: &IronlandCinemaModeV1, change: impl FnOnce(&mut Request)) {
        // Settle the fade at the old target first, so a request after a quiet
        // spell starts fading from where it was instead of jumping.
        let wanted = self.wanted().is_some();
        self.fade.advance(wanted, Instant::now());

        let serial = self.next_serial;
        self.next_serial += 1;
        if let Some(entry) = self.entries.iter_mut().find(|e| &e.resource == resource) {
            let request = entry.request.get_or_insert_with(Request::default);
            change(request);
            request.serial = serial;
        }
    }
}

/// The request that was made last among `requests`.
fn latest<'a>(requests: impl Iterator<Item = &'a Request>) -> Option<&'a Request> {
    requests.max_by_key(|r| r.serial)
}

/// Global data for the `ironland_cinema_mode_v1` global (nothing to carry -
/// also reused as the per-object user data, since each object's state lives
/// in [`CinemaModeState::entries`]).
#[derive(Debug)]
pub struct GlobalData;

impl<D> GlobalDispatch2<IronlandCinemaModeV1, D> for GlobalData
where
    D: CinemaModeHandler + Dispatch<IronlandCinemaModeV1, GlobalData>,
{
    fn bind(
        &self,
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<IronlandCinemaModeV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let resource = data_init.init(resource, GlobalData);
        state.cinema_mode_state().entries.push(Entry {
            resource,
            request: None,
        });
    }

    fn can_view(&self, client: &Client) -> bool {
        // No permission is asked, but what runs inside a VM has no business
        // darkening the host's screens.
        client
            .get_data::<ClientState>()
            .is_none_or(|data| data.vm.is_none())
    }
}

impl<D: CinemaModeHandler> Dispatch2<IronlandCinemaModeV1, D> for GlobalData {
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &IronlandCinemaModeV1,
        request: ironland_cinema_mode_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ironland_cinema_mode_v1::Request::Destroy => {}
            ironland_cinema_mode_v1::Request::Enable { output } => {
                let lit_output = output
                    .as_ref()
                    .and_then(Output::from_resource)
                    .map(|output| output.name())
                    .or_else(|| state.cinema_mode_default_output());
                state
                    .cinema_mode_state()
                    .update(resource, |r| r.lit_output = lit_output);
            }
            ironland_cinema_mode_v1::Request::Disable => {
                state
                    .cinema_mode_state()
                    .update(resource, |r| r.lit_output = None);
            }
            ironland_cinema_mode_v1::Request::SetGlow { strength } => {
                state
                    .cinema_mode_state()
                    .update(resource, |r| r.glow = strength.min(MAX_GLOW));
            }
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &IronlandCinemaModeV1) {
        let cinema = state.cinema_mode_state();
        let wanted = cinema.wanted().is_some();
        cinema.fade.advance(wanted, Instant::now());
        cinema.entries.retain(|e| &e.resource != resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(serial: u64, lit_output: Option<&str>, glow: u32) -> Request {
        Request {
            serial,
            lit_output: lit_output.map(str::to_string),
            glow,
        }
    }

    #[test]
    fn the_latest_request_wins_as_a_whole() {
        let requests = [
            request(3, Some("DP-1"), 20),
            request(7, None, 80),
            request(5, Some("HDMI-A-1"), 0),
        ];
        // Serial 7 switched the mode off, whatever the others want.
        assert_eq!(latest(requests.iter()), Some(&requests[1]));
        assert_eq!(latest([].iter()), None);
    }

    #[test]
    fn fade_is_linear_and_clamped() {
        let t0 = Instant::now();
        let mut fade = Fade::default();
        // The first call only starts the clock.
        assert_eq!(fade.advance(true, t0), 0.0);
        let half = fade.advance(true, t0 + FADE / 2);
        assert!((half - 0.5).abs() < 1e-3, "{half}");
        assert_eq!(fade.advance(true, t0 + FADE * 3), 1.0);
        // Fading back out starts from where it is.
        let back = fade.advance(false, t0 + FADE * 3 + FADE / 4);
        assert!((back - 0.75).abs() < 1e-3, "{back}");
        assert_eq!(fade.advance(false, t0 + FADE * 9), 0.0);
    }

    #[test]
    fn settling_the_fade_before_a_request_keeps_the_next_fade_gradual() {
        // `CinemaModeState::update` advances the fade at the old target
        // right before applying a request, so after a long quiet spell the
        // first frame only sees the time since the request.
        let t0 = Instant::now();
        let mut fade = Fade::default();
        fade.advance(false, t0);
        fade.advance(false, t0 + Duration::from_secs(60));
        let level = fade.advance(true, t0 + Duration::from_secs(60) + FADE / 10);
        assert!(level > 0.0 && level < 0.2, "{level}");
    }
}

#[cfg(test)]
mod wire_tests {
    use std::{
        os::unix::net::UnixStream,
        sync::{Arc, mpsc},
        thread::JoinHandle,
    };

    use smithay::{delegate_dispatch2, reexports::wayland_server::Display};
    use wayland_client::{
        Connection, Dispatch as ClientDispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };

    use super::*;
    use crate::ClientState;

    /// Client-side bindings of the same protocol file.
    mod client {
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
            wayland_scanner::generate_interfaces!("./protocols/ironland-cinema-mode-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("./protocols/ironland-cinema-mode-v1.xml");
    }
    use client::ironland_cinema_mode_v1 as c_cinema;

    // ---- server side ----------------------------------------------------

    struct TestState {
        cinema: CinemaModeState,
    }

    impl CinemaModeHandler for TestState {
        fn cinema_mode_state(&mut self) -> &mut CinemaModeState {
            &mut self.cinema
        }
        fn cinema_mode_default_output(&self) -> Option<String> {
            Some("DP-2".to_string())
        }
    }

    delegate_dispatch2!(TestState);

    /// Runs a server with one client; `query` asks it for what
    /// [`CinemaModeState::wanted`] says right now.
    struct Server {
        query: mpsc::Sender<mpsc::Sender<Option<(String, u32)>>>,
        stop: mpsc::Sender<()>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        fn start(stream: UnixStream) -> Server {
            Self::start_with(stream, ClientState::default())
        }

        fn start_with(stream: UnixStream, client_state: ClientState) -> Server {
            let (query, queries) = mpsc::channel::<mpsc::Sender<Option<(String, u32)>>>();
            let (stop, stopped) = mpsc::channel::<()>();
            let thread = std::thread::spawn(move || {
                let mut display = Display::<TestState>::new().unwrap();
                let mut state = TestState {
                    cinema: CinemaModeState::new::<TestState>(&display.handle()),
                };
                display
                    .handle()
                    .insert_client(stream, Arc::new(client_state))
                    .unwrap();
                loop {
                    display.dispatch_clients(&mut state).unwrap();
                    while let Ok(reply) = queries.try_recv() {
                        let wanted = state.cinema.wanted();
                        let _ = reply.send(wanted.map(|s| (s.lit_output, s.glow)));
                    }
                    if stopped.try_recv().is_ok() {
                        return;
                    }
                    display.flush_clients().unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            Server {
                query,
                stop,
                thread: Some(thread),
            }
        }

        fn wanted(&self) -> Option<(String, u32)> {
            let (reply, answer) = mpsc::channel();
            self.query.send(reply).unwrap();
            answer.recv_timeout(Duration::from_secs(2)).unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    // ---- client side ----------------------------------------------------

    struct App;

    impl ClientDispatch<wl_registry::WlRegistry, GlobalListContents> for App {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl ClientDispatch<c_cinema::IronlandCinemaModeV1, ()> for App {
        fn event(
            _: &mut Self,
            _: &c_cinema::IronlandCinemaModeV1,
            _: c_cinema::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    struct Client {
        queue: wayland_client::EventQueue<App>,
        app: App,
        cinema: c_cinema::IronlandCinemaModeV1,
    }

    impl Client {
        fn bind(stream: UnixStream) -> Client {
            Self::try_bind(stream).expect("the global should be advertised")
        }

        fn try_bind(stream: UnixStream) -> Option<Client> {
            let conn = Connection::from_socket(stream).unwrap();
            let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
            let mut app = App;
            queue.roundtrip(&mut app).unwrap();
            let cinema = globals
                .bind::<c_cinema::IronlandCinemaModeV1, _, _>(&queue.handle(), 1..=1, ())
                .ok()?;
            Some(Client { queue, app, cinema })
        }

        /// Makes sure the server has handled everything sent so far.
        fn sync(&mut self) {
            self.queue.roundtrip(&mut self.app).unwrap();
        }
    }

    #[test]
    fn binding_requests_nothing() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let server = Server::start(server_end);
        let mut client = Client::bind(client_end);
        client.sync();
        assert_eq!(server.wanted(), None);
    }

    #[test]
    fn enable_without_an_output_uses_the_current_one_and_keeps_the_glow() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let server = Server::start(server_end);
        let mut client = Client::bind(client_end);

        client.cinema.set_glow(40);
        client.sync();
        // The glow alone doesn't switch the mode on.
        assert_eq!(server.wanted(), None);

        client.cinema.enable(None);
        client.sync();
        assert_eq!(server.wanted(), Some(("DP-2".to_string(), 40)));

        client.cinema.set_glow(500);
        client.sync();
        assert_eq!(server.wanted(), Some(("DP-2".to_string(), MAX_GLOW)));

        client.cinema.disable();
        client.sync();
        assert_eq!(server.wanted(), None);

        client.cinema.enable(None);
        client.sync();
        assert_eq!(server.wanted(), Some(("DP-2".to_string(), MAX_GLOW)));
    }

    #[test]
    fn destroying_the_object_withdraws_the_mode() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let server = Server::start(server_end);
        let mut client = Client::bind(client_end);

        client.cinema.enable(None);
        client.sync();
        assert!(server.wanted().is_some());

        client.cinema.destroy();
        client.sync();
        assert_eq!(server.wanted(), None);
    }

    #[test]
    fn vm_guests_do_not_see_the_global() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start_with(
            server_end,
            ClientState {
                vm: Some(1),
                ..ClientState::default()
            },
        );
        assert!(Client::try_bind(client_end).is_none());
    }
}
