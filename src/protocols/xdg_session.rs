//! Server side of the staging `xdg-session-management-v1` protocol: lets a
//! client keep its toplevels' places across restarts - of the client, of the
//! compositor, of the whole login.
//!
//! The state itself lives in [`crate::shell::session_restore`] (the same file
//! that holds the rest of the saved layout): this module is the wire. A
//! client asks for a session (`get_session`), gets the id to store back as
//! `created`, and names each of its toplevels (`add_toplevel`). The next time
//! it passes the id and `restore_toplevel`s the same names before the first
//! commit; the compositor then puts the window where it was - on its
//! workspace, tiled in its slot or floating at its position - and answers
//! `restored` ahead of the first configure.
//!
//! Smithay doesn't implement this protocol; the generated bindings come from
//! `wayland-protocols` (staging).

use std::{collections::HashMap, sync::Mutex};

use smithay::{
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
            backend::{ClientId, GlobalId},
            protocol::wl_surface::WlSurface,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2, compositor, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{
    ironland_protocols::xdg_session_management::{
        xdg_session_manager_v1::{self, XdgSessionManagerV1},
        xdg_session_v1::{self, XdgSessionV1},
        xdg_toplevel_session_v1::{self, XdgToplevelSessionV1},
    },
    shell::session_restore::{BindError, XdgRegistry},
};

/// Implemented by the compositor state so this module doesn't depend on
/// `AnvilState` and its backend type parameter.
pub trait XdgSessionHandler: 'static {
    fn xdg_session_state(&mut self) -> &mut XdgSessionState;
    /// The surface of `toplevel`.
    fn xdg_toplevel_surface(&mut self, toplevel: &XdgToplevel) -> Option<WlSurface>;
    /// The sessions and which windows are bound to them.
    fn xdg_registry(&mut self) -> &mut XdgRegistry;
    /// Applies the saved state of `session`/`name` to the window of
    /// `surface`; whether there was any to apply.
    fn xdg_restore_surface(&mut self, surface: &WlSurface, session: &str, name: &str) -> bool;
}

#[derive(Debug)]
pub struct XdgSessionState {
    _global: GlobalId,
    /// The session object currently managing each session id.
    live: HashMap<String, XdgSessionV1>,
}

impl XdgSessionState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<XdgSessionManagerV1, ManagerData> + 'static,
    {
        Self {
            _global: dh.create_global::<D, XdgSessionManagerV1, _>(1, ManagerData),
            live: HashMap::new(),
        }
    }
}

/// Data of the manager global and its objects (nothing to carry).
#[derive(Debug)]
pub struct ManagerData;

/// Data of an `xdg_session_v1`.
#[derive(Debug)]
pub struct SessionData {
    id: String,
    /// Taken over by another client, or destroyed: requests do nothing.
    inert: Mutex<bool>,
}

impl SessionData {
    fn is_inert(&self) -> bool {
        *self.inert.lock().unwrap()
    }
}

/// Data of an `xdg_toplevel_session_v1`.
#[derive(Debug)]
pub struct ToplevelSessionData {
    session: XdgSessionV1,
    session_id: String,
    name: Mutex<String>,
}

/// Whether the toplevel's surface already got its initial configure, i.e. is
/// too far along to be restored (`already_mapped`).
fn already_mapped(surface: &WlSurface) -> bool {
    compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .is_some_and(|data| data.lock().unwrap().initial_configure_sent)
    })
}

impl<D> GlobalDispatch2<XdgSessionManagerV1, D> for ManagerData
where
    D: XdgSessionHandler + Dispatch<XdgSessionManagerV1, ManagerData>,
{
    fn bind(
        &self,
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<XdgSessionManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ManagerData);
    }
}

impl<D> Dispatch2<XdgSessionManagerV1, D> for ManagerData
where
    D: XdgSessionHandler + Dispatch<XdgSessionV1, SessionData>,
{
    fn request(
        &self,
        state: &mut D,
        client: &Client,
        resource: &XdgSessionManagerV1,
        request: xdg_session_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_session_manager_v1::Request::Destroy => {}
            xdg_session_manager_v1::Request::GetSession { id, reason, session_id } => {
                if let WEnum::Unknown(value) = reason {
                    resource.post_error(
                        xdg_session_manager_v1::Error::InvalidReason,
                        format!("{value} is not a reason"),
                    );
                    return;
                }
                // The same client opening a session it already has open is a
                // mistake; another client doing so takes it over.
                if let Some(wanted) = &session_id
                    && let Some(current) = state.xdg_session_state().live.get(wanted).cloned()
                {
                    if current.client().is_some_and(|c| c.id() == client.id()) {
                        resource.post_error(
                            xdg_session_manager_v1::Error::InUse,
                            format!("session {wanted} is already in use by this client"),
                        );
                        return;
                    }
                    if let Some(data) = current.data::<SessionData>() {
                        *data.inert.lock().unwrap() = true;
                    }
                    current.replaced();
                    state.xdg_registry().unbind_session(wanted);
                    state.xdg_session_state().live.remove(wanted);
                }

                let (session, known) = state.xdg_registry().open(session_id.as_deref());
                let object = data_init.init(
                    id,
                    SessionData { id: session.clone(), inert: Mutex::new(false) },
                );
                if known {
                    object.restored();
                } else {
                    object.created(session.clone());
                }
                state.xdg_session_state().live.insert(session, object);
            }
        }
    }
}

impl<D> Dispatch2<XdgSessionV1, D> for SessionData
where
    D: XdgSessionHandler + Dispatch<XdgToplevelSessionV1, ToplevelSessionData>,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &XdgSessionV1,
        request: xdg_session_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_session_v1::Request::Destroy => {
                if !self.is_inert() {
                    state.xdg_registry().unbind_session(&self.id);
                }
            }
            xdg_session_v1::Request::Remove => {
                if !self.is_inert() {
                    state.xdg_registry().remove(&self.id);
                }
            }
            xdg_session_v1::Request::AddToplevel { id, toplevel, name } => {
                data_init.init(
                    id,
                    ToplevelSessionData {
                        session: resource.clone(),
                        session_id: self.id.clone(),
                        name: Mutex::new(name.clone()),
                    },
                );
                if self.is_inert() {
                    return;
                }
                let Some(surface) = state.xdg_toplevel_surface(&toplevel) else { return };
                if let Err(err) = state.xdg_registry().bind(&self.id, &name, &surface) {
                    post_bind_error(resource, err, &name);
                }
            }
            xdg_session_v1::Request::RestoreToplevel { id, toplevel, name } => {
                let object = data_init.init(
                    id,
                    ToplevelSessionData {
                        session: resource.clone(),
                        session_id: self.id.clone(),
                        name: Mutex::new(name.clone()),
                    },
                );
                if self.is_inert() {
                    return;
                }
                let Some(surface) = state.xdg_toplevel_surface(&toplevel) else { return };
                if already_mapped(&surface) {
                    resource.post_error(
                        xdg_session_v1::Error::AlreadyMapped,
                        format!("toplevel {name} was already mapped when restored"),
                    );
                    return;
                }
                if let Err(err) = state.xdg_registry().bind(&self.id, &name, &surface) {
                    post_bind_error(resource, err, &name);
                    return;
                }
                if state.xdg_restore_surface(&surface, &self.id, &name) {
                    object.restored();
                }
            }
            xdg_session_v1::Request::RemoveToplevel { name } => {
                if !self.is_inert() {
                    state.xdg_registry().remove_toplevel(&self.id, &name);
                }
            }
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &XdgSessionV1) {
        // Only forget the object if it is still the one managing the id (a
        // replaced one has long been swapped out).
        let live = &mut state.xdg_session_state().live;
        if live.get(&self.id) == Some(resource) {
            live.remove(&self.id);
        }
    }
}

fn post_bind_error(session: &XdgSessionV1, err: BindError, name: &str) {
    match err {
        BindError::NameInUse => session.post_error(
            xdg_session_v1::Error::NameInUse,
            format!("a toplevel named {name} is already in the session"),
        ),
        BindError::AlreadyAdded => session.post_error(
            xdg_session_v1::Error::AlreadyAdded,
            "the toplevel is already in a session".to_string(),
        ),
    }
}

impl<D> Dispatch2<XdgToplevelSessionV1, D> for ToplevelSessionData
where
    D: XdgSessionHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _resource: &XdgToplevelSessionV1,
        request: xdg_toplevel_session_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_session_v1::Request::Destroy => {}
            xdg_toplevel_session_v1::Request::Rename { name } => {
                let mut current = self.name.lock().unwrap();
                match state.xdg_registry().rename(&self.session_id, &current, &name) {
                    Ok(()) => *current = name,
                    Err(err) => post_bind_error(&self.session, err, &name),
                }
            }
        }
    }
}

/// Real wire tests: a `wayland-client` against this module (and Smithay's
/// compositor and xdg-shell) over a socketpair, so the requests, events and
/// protocol errors are exercised together.
#[cfg(test)]
mod wire_tests {
    use std::{
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, mpsc},
        thread::JoinHandle,
        time::Duration,
    };

    use smithay::{
        delegate_dispatch2,
        reexports::wayland_server::Display,
        utils::Serial,
        wayland::{
            compositor::{CompositorClientState, CompositorHandler, CompositorState},
            shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
    };
    use wayland_client::{
        Connection, Dispatch as ClientDispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_compositor, wl_registry, wl_surface},
    };
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    use super::*;
    use crate::{ClientState, shell::session_restore::Placement};

    /// Client-side bindings of the same protocol file.
    mod client {
        #![allow(dead_code, non_camel_case_types, unused_imports, missing_docs, clippy::all)]
        use wayland_client;
        use wayland_client::protocol::*;
        use wayland_protocols::xdg::shell::client::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            use wayland_protocols::xdg::shell::client::__interfaces::*;
            wayland_scanner::generate_interfaces!("./protocols/xdg-session-management-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("./protocols/xdg-session-management-v1.xml");
    }
    use client::{xdg_session_manager_v1 as c_manager, xdg_session_v1 as c_session, xdg_toplevel_session_v1 as c_top};

    // ---- server side ----------------------------------------------------

    struct TestState {
        compositor: CompositorState,
        xdg_shell: XdgShellState,
        session: XdgSessionState,
        registry: XdgRegistry,
        /// `(session, name)` of every restore the server applied.
        applied: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl CompositorHandler for TestState {
        fn compositor_state(&mut self) -> &mut CompositorState {
            &mut self.compositor
        }
        fn client_compositor_state<'a>(&self, client: &'a wayland_server::Client) -> &'a CompositorClientState {
            &client.get_data::<ClientState>().unwrap().compositor_state
        }
        fn commit(&mut self, surface: &WlSurface) {
            // What a real compositor does on the first commit.
            if let Some(toplevel) = self.xdg_shell.toplevel_surfaces().iter().find(|t| t.wl_surface() == surface)
                && !toplevel.is_initial_configure_sent()
            {
                toplevel.send_configure();
            }
        }
    }

    impl XdgShellHandler for TestState {
        fn xdg_shell_state(&mut self) -> &mut XdgShellState {
            &mut self.xdg_shell
        }
        fn new_toplevel(&mut self, _surface: ToplevelSurface) {}
        fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
        fn grab(&mut self, _surface: PopupSurface, _seat: wayland_server::protocol::wl_seat::WlSeat, _serial: Serial) {}
        fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}
    }

    impl XdgSessionHandler for TestState {
        fn xdg_session_state(&mut self) -> &mut XdgSessionState {
            &mut self.session
        }
        fn xdg_toplevel_surface(&mut self, toplevel: &XdgToplevel) -> Option<WlSurface> {
            self.xdg_shell.get_toplevel(toplevel).map(|t| t.wl_surface().clone())
        }
        fn xdg_registry(&mut self) -> &mut XdgRegistry {
            &mut self.registry
        }
        fn xdg_restore_surface(&mut self, _surface: &WlSurface, session: &str, name: &str) -> bool {
            let known = self.registry.placement(session, name).is_some();
            if known {
                self.applied.lock().unwrap().push((session.to_string(), name.to_string()));
            }
            known
        }
    }

    delegate_dispatch2!(TestState);

    enum Command {
        /// Remember a placement for `(session, name)`.
        Remember(String, String),
        Stop,
    }

    struct Server {
        commands: mpsc::Sender<Command>,
        applied: Arc<Mutex<Vec<(String, String)>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        fn start(clients: Vec<UnixStream>) -> Server {
            let (commands, rx) = mpsc::channel::<Command>();
            let applied = Arc::new(Mutex::new(Vec::new()));
            let applied_in_thread = applied.clone();
            let thread = std::thread::spawn(move || {
                let mut display = Display::<TestState>::new().unwrap();
                let dh = display.handle();
                let mut state = TestState {
                    compositor: CompositorState::new::<TestState>(&dh),
                    xdg_shell: XdgShellState::new::<TestState>(&dh),
                    session: XdgSessionState::new::<TestState>(&dh),
                    registry: XdgRegistry::default(),
                    applied: applied_in_thread,
                };
                let mut handle = display.handle();
                for stream in clients {
                    handle.insert_client(stream, Arc::new(ClientState::default())).unwrap();
                }
                loop {
                    display.dispatch_clients(&mut state).unwrap();
                    while let Ok(command) = rx.try_recv() {
                        match command {
                            Command::Remember(session, name) => {
                                state.registry.set_placement(
                                    &session,
                                    &name,
                                    Placement { output: "DP-1".into(), workspace: 1, floating: None },
                                );
                            }
                            Command::Stop => return,
                        }
                    }
                    display.flush_clients().unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            Server { commands, applied, thread: Some(thread) }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.commands.send(Command::Stop);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    // ---- client side ----------------------------------------------------

    #[derive(Default)]
    struct App {
        /// Events as readable strings, tagged with which object got them.
        log: Vec<String>,
    }

    macro_rules! ignore_events {
        ($($proxy:ty => $user:ty),* $(,)?) => {$(
            impl ClientDispatch<$proxy, $user> for App {
                fn event(
                    _: &mut Self,
                    _: &$proxy,
                    _: <$proxy as wayland_client::Proxy>::Event,
                    _: &$user,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*};
    }
    ignore_events!(
        wl_registry::WlRegistry => GlobalListContents,
        wl_compositor::WlCompositor => (),
        wl_surface::WlSurface => (),
        xdg_wm_base::XdgWmBase => (),
        xdg_surface::XdgSurface => (),
        xdg_toplevel::XdgToplevel => (),
        c_manager::XdgSessionManagerV1 => (),
    );

    /// Which session object an event came from, to tell two clients' apart.
    impl ClientDispatch<c_session::XdgSessionV1, &'static str> for App {
        fn event(
            app: &mut Self,
            _: &c_session::XdgSessionV1,
            event: c_session::Event,
            who: &&'static str,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            app.log.push(match event {
                c_session::Event::Created { session_id } => format!("{who} created {session_id}"),
                c_session::Event::Restored => format!("{who} restored"),
                c_session::Event::Replaced => format!("{who} replaced"),
            });
        }
    }

    impl ClientDispatch<c_top::XdgToplevelSessionV1, &'static str> for App {
        fn event(
            app: &mut Self,
            _: &c_top::XdgToplevelSessionV1,
            event: c_top::Event,
            who: &&'static str,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let c_top::Event::Restored = event;
            app.log.push(format!("{who} toplevel restored"));
        }
    }

    struct Client {
        queue: wayland_client::EventQueue<App>,
        app: App,
        manager: c_manager::XdgSessionManagerV1,
        compositor: wl_compositor::WlCompositor,
        wm_base: xdg_wm_base::XdgWmBase,
    }

    impl Client {
        fn connect(stream: UnixStream) -> Client {
            let conn = Connection::from_socket(stream).unwrap();
            let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
            let mut app = App::default();
            queue.roundtrip(&mut app).unwrap();
            let qh = queue.handle();
            Client {
                manager: globals.bind(&qh, 1..=1, ()).expect("the global should be advertised"),
                compositor: globals.bind(&qh, 1..=6, ()).unwrap(),
                wm_base: globals.bind(&qh, 1..=6, ()).unwrap(),
                queue,
                app,
            }
        }

        fn roundtrip(&mut self) -> Result<usize, wayland_client::backend::WaylandError> {
            self.queue.roundtrip(&mut self.app).map_err(|e| match e {
                wayland_client::DispatchError::Backend(e) => e,
                other => panic!("{other}"),
            })
        }

        fn session(&mut self, who: &'static str, id: Option<&str>) -> c_session::XdgSessionV1 {
            let session = self.manager.get_session(
                c_manager::Reason::Launch,
                id.map(str::to_string),
                &self.queue.handle(),
                who,
            );
            self.roundtrip().unwrap();
            session
        }

        /// A toplevel that hasn't been committed yet.
        fn toplevel(&mut self) -> (wl_surface::WlSurface, xdg_toplevel::XdgToplevel) {
            let qh = self.queue.handle();
            let surface = self.compositor.create_surface(&qh, ());
            let xdg = self.wm_base.get_xdg_surface(&surface, &qh, ());
            let toplevel = xdg.get_toplevel(&qh, ());
            self.roundtrip().unwrap();
            (surface, toplevel)
        }

        fn log(&mut self) -> Vec<String> {
            self.roundtrip().unwrap();
            self.app.log.clone()
        }
    }

    /// `(interface, error code)` of a protocol error the server raised.
    fn protocol_error(err: wayland_client::backend::WaylandError) -> (String, u32) {
        match err {
            wayland_client::backend::WaylandError::Protocol(e) => (e.object_interface, e.code),
            other => panic!("not a protocol error: {other}"),
        }
    }

    fn id_of(entry: &str) -> String {
        entry.rsplit(' ').next().unwrap().to_string()
    }

    #[test]
    fn a_new_session_is_created_and_a_known_one_restored() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);

        let first = client.session("a", None);
        let log = client.log();
        assert!(log[0].starts_with("a created "), "{log:?}");
        let id = id_of(&log[0]);
        assert_eq!(id.len(), 32);

        // Gone and back: the id is known, and it says so.
        first.destroy();
        client.roundtrip().unwrap();
        let _again = client.session("b", Some(&id));
        assert_eq!(client.log()[1], "b restored");

        // An id nobody handed out starts afresh.
        let _fresh = client.session("c", Some("never-issued"));
        let log = client.log();
        assert!(log[2].starts_with("c created ") && id_of(&log[2]) != "never-issued", "{log:?}");
    }

    #[test]
    fn another_client_taking_a_session_over_replaces_the_first() {
        let (a_end, a_server) = UnixStream::pair().unwrap();
        let (b_end, b_server) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![a_server, b_server]);
        let mut a = Client::connect(a_end);
        let mut b = Client::connect(b_end);

        let _a_session = a.session("a", None);
        let id = id_of(&a.log()[0]);

        let _b_session = b.session("b", Some(&id));
        assert_eq!(b.log(), ["b restored"]);
        assert_eq!(a.log()[1], "a replaced");
    }

    #[test]
    fn opening_a_session_twice_in_one_client_is_an_error() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let _session = client.session("a", None);
        let id = id_of(&client.log()[0]);

        let _again = client.manager.get_session(
            c_manager::Reason::Launch,
            Some(id),
            &client.queue.handle(),
            "b",
        );
        // manager error `in_use`
        assert_eq!(protocol_error(client.roundtrip().unwrap_err()), ("xdg_session_manager_v1".into(), 1));
    }

    #[test]
    fn a_toplevel_name_can_only_be_used_once_per_session() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let (_s1, top1) = client.toplevel();
        let (_s2, top2) = client.toplevel();

        let qh = client.queue.handle();
        let _first = session.add_toplevel(&top1, "main".into(), &qh, "t1");
        client.roundtrip().unwrap();
        let _second = session.add_toplevel(&top2, "main".into(), &qh, "t2");
        // session error `name_in_use`
        assert_eq!(protocol_error(client.roundtrip().unwrap_err()), ("xdg_session_v1".into(), 1));
    }

    #[test]
    fn a_toplevel_cannot_be_added_to_a_session_twice() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let (_s, top) = client.toplevel();

        let qh = client.queue.handle();
        let _first = session.add_toplevel(&top, "main".into(), &qh, "t1");
        let _again = session.add_toplevel(&top, "other".into(), &qh, "t2");
        // session error `already_added`
        assert_eq!(protocol_error(client.roundtrip().unwrap_err()), ("xdg_session_v1".into(), 4));
    }

    #[test]
    fn a_remembered_toplevel_is_restored_before_it_maps() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let id = id_of(&client.log()[0]);
        server.commands.send(Command::Remember(id.clone(), "main".into())).unwrap();
        std::thread::sleep(Duration::from_millis(30));

        let (_s, top) = client.toplevel();
        let _handle = session.restore_toplevel(&top, "main".into(), &client.queue.handle(), "t");
        assert_eq!(client.log()[1], "t toplevel restored");
        assert_eq!(*server.applied.lock().unwrap(), [(id, "main".to_string())]);
    }

    #[test]
    fn a_toplevel_the_session_does_not_know_is_just_added() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let (_s, top) = client.toplevel();

        let _handle = session.restore_toplevel(&top, "unknown".into(), &client.queue.handle(), "t");
        // No `restored`: only the session's own `created` is in the log.
        assert_eq!(client.log().len(), 1);
        assert!(server.applied.lock().unwrap().is_empty());
        // And the name is taken now, so adding another under it is refused.
        let (_s2, top2) = client.toplevel();
        let _other = session.add_toplevel(&top2, "unknown".into(), &client.queue.handle(), "t2");
        assert_eq!(protocol_error(client.roundtrip().unwrap_err()), ("xdg_session_v1".into(), 1));
    }

    #[test]
    fn restoring_a_toplevel_that_already_mapped_is_an_error() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let (surface, top) = client.toplevel();
        // The first commit gets the initial configure sent.
        surface.commit();
        client.roundtrip().unwrap();

        let _handle = session.restore_toplevel(&top, "main".into(), &client.queue.handle(), "t");
        // session error `already_mapped`
        assert_eq!(protocol_error(client.roundtrip().unwrap_err()), ("xdg_session_v1".into(), 2));
    }

    #[test]
    fn renaming_a_toplevel_keeps_its_state_and_refuses_a_taken_name() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let (_s1, top1) = client.toplevel();
        let (_s2, top2) = client.toplevel();
        let qh = client.queue.handle();
        let first = session.add_toplevel(&top1, "one".into(), &qh, "t1");
        let _second = session.add_toplevel(&top2, "two".into(), &qh, "t2");
        client.roundtrip().unwrap();

        first.rename("uno".into());
        client.roundtrip().unwrap();
        // The old name is free again, the new one taken.
        let (_s3, top3) = client.toplevel();
        let _reuse = session.add_toplevel(&top3, "one".into(), &qh, "t3");
        client.roundtrip().unwrap();
        first.rename("two".into());
        assert_eq!(protocol_error(client.roundtrip().unwrap_err()), ("xdg_session_v1".into(), 1));
    }

    #[test]
    fn removing_a_session_makes_it_unknown() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let _server = Server::start(vec![server_end]);
        let mut client = Client::connect(client_end);
        let session = client.session("a", None);
        let id = id_of(&client.log()[0]);
        session.remove();
        client.roundtrip().unwrap();

        let _gone = client.session("b", Some(&id));
        let log = client.log();
        assert!(log[1].starts_with("b created ") && id_of(&log[1]) != id, "{log:?}");
    }
}
