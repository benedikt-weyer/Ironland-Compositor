//! Server side of the `ironland-vm-launch-v1` protocol (see
//! `crate::ironland_protocols::vm_launch` for the generated bindings and
//! `protocols/ironland-vm-launch-v1.xml` for the wire format).
//!
//! Lets the shell's launcher start a desktop entry in a VM (see
//! `crate::shell::vm`) the same way the compositor's own launcher overlay does
//! with its VM tabs. A client names an application by desktop entry
//! id; the compositor resolves that against the entries installed on the host
//! ([`crate::launcher::find_entry`]) and runs the entry's own `Exec` in the
//! guest, so the protocol can't be used to run arbitrary commands.
//!
//! Each `launch` request creates an `ironland_vm_launch_v1` object that is
//! handed to the VM as a [`Watcher`], which `shell::vm` pokes as the VM
//! progresses (first window, state being saved, end). The VM never depends on
//! the object: destroying it, or the client disconnecting, leaves the VM alone.
//!
//! The global is hidden from clients that run *inside* a VM (they would
//! otherwise be able to start more VMs on the host) and from sandboxed
//! (security-context) clients.

use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    backend::{ClientId, GlobalId},
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::{
    ClientState,
    ironland_protocols::vm_launch::{
        ironland_vm_launch_v1::{self, Error, FinishReason, IronlandVmLaunchV1},
        ironland_vm_launcher_v1::{self, IronlandVmLauncherV1, Mode},
    },
    shell::vm::{VmError, VmId},
};

/// Implemented by the compositor state so this module can start VMs without
/// depending on `AnvilState` (and its `Backend` type parameter) directly.
pub trait VmLaunchHandler: 'static {
    fn vm_launch_state(&mut self) -> &mut VmLaunchState;
    /// The programs (desktop entry ids without `.desktop`) that have a VM set
    /// up, i.e. can be launched.
    fn vm_launch_programs(&self) -> Vec<String>;
    /// Starts the desktop entry `entry_id` in a VM.
    fn vm_launch_entry(&mut self, entry_id: &str, persistent: bool) -> Result<VmId, Refusal>;
    /// Hands `watcher` to the running VM `vm`, to be told how it goes.
    fn vm_watch(&mut self, vm: VmId, watcher: Watcher);
}

/// Why a launch was refused, in the protocol's terms.
#[derive(Debug)]
pub struct Refusal {
    pub error: Error,
    pub message: String,
}

impl Refusal {
    pub fn unknown_entry(entry_id: &str) -> Self {
        Refusal {
            error: Error::UnknownEntry,
            message: format!("no installed application called {entry_id}"),
        }
    }
}

impl From<&VmError> for Refusal {
    fn from(err: &VmError) -> Self {
        let error = match err {
            VmError::NotConfigured(_) => Error::NotConfigured,
            VmError::ImageMissing(_) => Error::ImageMissing,
            VmError::TooManyInstances(_) => Error::TooMany,
            VmError::NoCommand => Error::NoCommand,
            VmError::AlreadyRunning(_) => Error::AlreadyRunning,
            VmError::State(_) => Error::State,
            VmError::Io { .. } => Error::Other,
        };
        // The short message is meant for a one-line display; the shell shows
        // it as is.
        Refusal {
            error,
            message: err.short(),
        }
    }
}

/// How a VM ended, as reported to a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Closed,
    Saved,
    Failed,
    TimedOut,
}

/// A client's view of one launched VM: tells it about progress.
#[derive(Debug, Clone)]
pub struct Watcher(IronlandVmLaunchV1);

impl Watcher {
    pub fn running(&self) {
        self.0.running();
    }

    pub fn saving(&self) {
        self.0.saving();
    }

    pub fn finished(&self, finish: Finish) {
        self.0.finished(match finish {
            Finish::Closed => FinishReason::Closed,
            Finish::Saved => FinishReason::Saved,
            Finish::Failed => FinishReason::Failed,
            Finish::TimedOut => FinishReason::TimedOut,
        });
    }
}

#[derive(Debug)]
pub struct VmLaunchState {
    global: GlobalId,
    managers: Vec<IronlandVmLauncherV1>,
    /// The list last announced to the clients (sorted), to tell when it
    /// changed.
    programs: Vec<String>,
}

impl VmLaunchState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<IronlandVmLauncherV1, GlobalData> + 'static,
    {
        let global = dh.create_global::<D, IronlandVmLauncherV1, _>(2, GlobalData);
        VmLaunchState {
            global,
            managers: Vec::new(),
            programs: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Tells every bound client which programs can be launched in a VM, if
    /// that differs from what they were last told (the `[vm]` config changed,
    /// or VM images were installed or removed).
    pub fn update_programs(&mut self, mut programs: Vec<String>) {
        programs.sort();
        programs.dedup();
        if programs == self.programs {
            return;
        }
        self.programs = programs;
        for manager in &self.managers {
            announce(manager, &self.programs);
        }
    }
}

/// Sends `manager` the current state: whether anything is launchable, and
/// (from version 2) which programs.
fn announce(manager: &IronlandVmLauncherV1, programs: &[String]) {
    manager.availability(!programs.is_empty() as u32);
    if manager.version() >= 2 {
        for program in programs {
            manager.program(program.clone());
        }
        manager.programs_done();
    }
}

/// Global data for the `ironland_vm_launcher_v1` global (nothing to carry -
/// also reused as the per-object user data).
#[derive(Debug)]
pub struct GlobalData;

/// User data of an `ironland_vm_launch_v1` object (nothing to carry: the VM
/// holds the [`Watcher`]).
#[derive(Debug)]
pub struct LaunchData;

/// Whether `client` may see the global: not a guest application (see the
/// module doc) and not a sandboxed client.
fn may_launch(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_none_or(|data| data.vm.is_none() && data.security_context.is_none())
}

impl<D> GlobalDispatch2<IronlandVmLauncherV1, D> for GlobalData
where
    D: VmLaunchHandler + Dispatch<IronlandVmLauncherV1, GlobalData>,
{
    fn bind(
        &self,
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<IronlandVmLauncherV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let resource = data_init.init(resource, GlobalData);
        // What a client is told on binding is the live list, which may be
        // newer than the last one broadcast (images appear in /etc without
        // the config changing).
        let programs = state.vm_launch_programs();
        state.vm_launch_state().update_programs(programs);
        let known = state.vm_launch_state();
        announce(&resource, &known.programs);
        known.managers.push(resource);
    }

    fn can_view(&self, client: &Client) -> bool {
        may_launch(client)
    }
}

impl<D> Dispatch2<IronlandVmLauncherV1, D> for GlobalData
where
    D: VmLaunchHandler + Dispatch<IronlandVmLaunchV1, LaunchData>,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _resource: &IronlandVmLauncherV1,
        request: ironland_vm_launcher_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ironland_vm_launcher_v1::Request::Destroy => {}
            ironland_vm_launcher_v1::Request::Launch { id, entry, mode } => {
                // The object must be created whether or not the launch works:
                // a refusal is reported on it.
                let launch = data_init.init(id, LaunchData);
                let persistent = match mode {
                    WEnum::Value(Mode::Persistent) => true,
                    WEnum::Value(Mode::Throwaway) => false,
                    WEnum::Unknown(_) => {
                        launch.failed(Error::Other, "unknown launch mode".to_string());
                        return;
                    }
                };
                match state.vm_launch_entry(&entry, persistent) {
                    Ok(vm) => {
                        launch.starting(vm as u32);
                        state.vm_watch(vm, Watcher(launch));
                    }
                    Err(refusal) => launch.failed(refusal.error, refusal.message),
                }
            }
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &IronlandVmLauncherV1) {
        state
            .vm_launch_state()
            .managers
            .retain(|manager| manager != resource);
    }
}

impl<D: VmLaunchHandler> Dispatch2<IronlandVmLaunchV1, D> for LaunchData {
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &IronlandVmLaunchV1,
        request: ironland_vm_launch_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let ironland_vm_launch_v1::Request::Destroy = request;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vm_error_maps_to_a_distinct_protocol_error_with_a_short_message() {
        let errors = [
            (VmError::NotConfigured("A".into()), Error::NotConfigured),
            (VmError::ImageMissing("/x".into()), Error::ImageMissing),
            (VmError::TooManyInstances(2), Error::TooMany),
            (VmError::NoCommand, Error::NoCommand),
            (VmError::AlreadyRunning("A".into()), Error::AlreadyRunning),
            (VmError::State("s".into()), Error::State),
            (
                VmError::Io {
                    context: "c",
                    source: std::io::Error::other("e"),
                },
                Error::Other,
            ),
        ];
        for (vm_error, expected) in errors {
            let refusal = Refusal::from(&vm_error);
            assert_eq!(refusal.error, expected, "{vm_error:?}");
            assert_eq!(refusal.message, vm_error.short());
        }
    }

    #[test]
    fn unknown_entries_are_refused_by_name() {
        let refusal = Refusal::unknown_entry("nope.desktop");
        assert_eq!(refusal.error, Error::UnknownEntry);
        assert!(refusal.message.contains("nope.desktop"));
    }
}

/// End-to-end tests of the wire protocol: a real `wayland-client` talks to this
/// module's server side over a socket pair, so the XML, the generated
/// bindings, the dispatching and the global's visibility rules are all
/// exercised together.
#[cfg(test)]
mod wire_tests {
    use std::{
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, mpsc},
        thread::JoinHandle,
        time::Duration,
    };

    use smithay::{delegate_dispatch2, reexports::wayland_server::Display};
    use wayland_client::{
        Connection, Dispatch as ClientDispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };

    use super::*;

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
            wayland_scanner::generate_interfaces!("./protocols/ironland-vm-launch-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("./protocols/ironland-vm-launch-v1.xml");
    }
    use client::{ironland_vm_launch_v1 as c_launch, ironland_vm_launcher_v1 as c_launcher};

    // ---- server side ----------------------------------------------------

    /// What the test harness tells the server thread to do.
    enum Command {
        Running,
        Saving,
        Finished(Finish),
        SetPrograms(Vec<&'static str>),
        Stop,
    }

    struct TestState {
        vm: VmLaunchState,
        programs: Vec<String>,
        launches: Arc<Mutex<Vec<(String, bool)>>>,
        watchers: Vec<Watcher>,
    }

    impl VmLaunchHandler for TestState {
        fn vm_launch_state(&mut self) -> &mut VmLaunchState {
            &mut self.vm
        }
        fn vm_launch_programs(&self) -> Vec<String> {
            self.programs.clone()
        }
        fn vm_launch_entry(&mut self, entry_id: &str, persistent: bool) -> Result<VmId, Refusal> {
            self.launches
                .lock()
                .unwrap()
                .push((entry_id.to_string(), persistent));
            match entry_id {
                "missing" => Err(Refusal::unknown_entry(entry_id)),
                "busy" => Err(Refusal::from(&VmError::AlreadyRunning("Busy".into()))),
                _ => Ok(7),
            }
        }
        fn vm_watch(&mut self, _vm: VmId, watcher: Watcher) {
            self.watchers.push(watcher);
        }
    }

    delegate_dispatch2!(TestState);

    struct Server {
        commands: mpsc::Sender<Command>,
        launches: Arc<Mutex<Vec<(String, bool)>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        /// Starts a server with one client connection per entry of `clients`.
        fn start(clients: Vec<(UnixStream, ClientState)>) -> Server {
            let (commands, rx) = mpsc::channel::<Command>();
            let launches = Arc::new(Mutex::new(Vec::new()));
            let launches_in_thread = launches.clone();
            let thread = std::thread::spawn(move || {
                let mut display = Display::<TestState>::new().unwrap();
                let mut state = TestState {
                    vm: VmLaunchState::new::<TestState>(&display.handle()),
                    programs: vec!["foot".to_string(), "Alacritty".to_string()],
                    launches: launches_in_thread,
                    watchers: Vec::new(),
                };
                let mut handle = display.handle();
                for (stream, client_state) in clients {
                    handle
                        .insert_client(stream, Arc::new(client_state))
                        .unwrap();
                }
                loop {
                    display.dispatch_clients(&mut state).unwrap();
                    while let Ok(command) = rx.try_recv() {
                        match command {
                            Command::Running => state.watchers.iter().for_each(Watcher::running),
                            Command::Saving => state.watchers.iter().for_each(Watcher::saving),
                            Command::Finished(f) => {
                                state.watchers.iter().for_each(|w| w.finished(f))
                            }
                            Command::SetPrograms(programs) => {
                                state.programs = programs.iter().map(|p| p.to_string()).collect();
                                state.vm.update_programs(state.programs.clone());
                            }
                            Command::Stop => return,
                        }
                    }
                    display.flush_clients().unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            Server {
                commands,
                launches,
                thread: Some(thread),
            }
        }

        fn send(&self, command: Command) {
            self.commands.send(command).unwrap();
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

    /// An enum argument by name (`Unknown(n)` for a value the client's
    /// bindings don't know).
    fn name<T: std::fmt::Debug>(value: WEnum<T>) -> String {
        match value {
            WEnum::Value(v) => format!("{v:?}"),
            WEnum::Unknown(n) => format!("?{n}"),
        }
    }

    #[derive(Default)]
    struct App {
        /// Everything the client was told, as readable strings.
        log: Vec<String>,
    }

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

    impl ClientDispatch<c_launcher::IronlandVmLauncherV1, ()> for App {
        fn event(
            app: &mut Self,
            _: &c_launcher::IronlandVmLauncherV1,
            event: c_launcher::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            app.log.push(match event {
                c_launcher::Event::Availability { configured } => {
                    format!("availability {configured}")
                }
                c_launcher::Event::Program { entry } => format!("program {entry}"),
                c_launcher::Event::ProgramsDone => "programs_done".to_string(),
            });
        }
    }

    impl ClientDispatch<c_launch::IronlandVmLaunchV1, ()> for App {
        fn event(
            app: &mut Self,
            _: &c_launch::IronlandVmLaunchV1,
            event: c_launch::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            app.log.push(match event {
                c_launch::Event::Failed { error, message } => {
                    format!("failed {} {message}", name(error))
                }
                c_launch::Event::Starting { vm } => format!("starting {vm}"),
                c_launch::Event::Running => "running".to_string(),
                c_launch::Event::Saving => "saving".to_string(),
                c_launch::Event::Finished { reason } => format!("finished {}", name(reason)),
            });
        }
    }

    struct Client {
        conn: Connection,
        queue: wayland_client::EventQueue<App>,
        app: App,
        launcher: c_launcher::IronlandVmLauncherV1,
    }

    impl Client {
        /// Connects over `stream` and binds the launcher global.
        fn bind(stream: UnixStream) -> Option<Client> {
            Self::bind_versions(stream, 1..=2)
        }

        /// Like [`Self::bind`], but binding a version in `versions` (the
        /// newest the server offers is used).
        fn bind_versions(
            stream: UnixStream,
            versions: std::ops::RangeInclusive<u32>,
        ) -> Option<Client> {
            let conn = Connection::from_socket(stream).unwrap();
            let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
            let mut app = App::default();
            queue.roundtrip(&mut app).unwrap();
            let launcher = globals
                .bind::<c_launcher::IronlandVmLauncherV1, _, _>(&queue.handle(), versions, ())
                .ok()?;
            Some(Client {
                conn,
                queue,
                app,
                launcher,
            })
        }

        /// Waits (briefly) until the log has `n` entries, returning it.
        fn wait_for_log(&mut self, n: usize) -> Vec<String> {
            for _ in 0..200 {
                self.queue.roundtrip(&mut self.app).unwrap();
                if self.app.log.len() >= n {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.app.log.clone()
        }

        fn launch(&self, entry: &str, mode: c_launcher::Mode) -> c_launch::IronlandVmLaunchV1 {
            self.launcher
                .launch(entry.to_string(), mode, &self.queue.handle(), ())
        }
    }

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    /// What a version 2 client is told on binding, with the test server's
    /// programs (sorted).
    const HELLO: [&str; 4] = [
        "availability 1",
        "program Alacritty",
        "program foot",
        "programs_done",
    ];

    #[test]
    fn binding_reports_the_programs_and_a_launch_reports_progress() {
        let (client_end, server_end) = pair();
        let server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind(client_end).expect("the global should be advertised");
        assert_eq!(client.wait_for_log(4), HELLO);

        let _launch = client.launch("org.mozilla.firefox", c_launcher::Mode::Persistent);
        assert_eq!(client.wait_for_log(5)[4], "starting 7");
        assert_eq!(
            *server.launches.lock().unwrap(),
            [("org.mozilla.firefox".to_string(), true)]
        );

        server.send(Command::Running);
        server.send(Command::Saving);
        server.send(Command::Finished(Finish::Saved));
        assert_eq!(
            client.wait_for_log(8)[5..],
            ["running", "saving", "finished Saved"]
        );
    }

    #[test]
    fn a_version_1_client_only_hears_availability() {
        let (client_end, server_end) = pair();
        let _server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind_versions(client_end, 1..=1).unwrap();
        assert_eq!(client.wait_for_log(1), ["availability 1"]);
        std::thread::sleep(Duration::from_millis(50));
        client.queue.roundtrip(&mut client.app).unwrap();
        assert_eq!(client.app.log, ["availability 1"]);
    }

    #[test]
    fn throwaway_mode_is_passed_through() {
        let (client_end, server_end) = pair();
        let server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind(client_end).unwrap();
        let _launch = client.launch("foot", c_launcher::Mode::Throwaway);
        client.wait_for_log(5);
        assert_eq!(
            *server.launches.lock().unwrap(),
            [("foot".to_string(), false)]
        );
    }

    #[test]
    fn a_refused_launch_reports_why_on_its_own_object() {
        let (client_end, server_end) = pair();
        let _server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind(client_end).unwrap();

        let _a = client.launch("missing", c_launcher::Mode::Throwaway);
        let _b = client.launch("busy", c_launcher::Mode::Persistent);
        let log = client.wait_for_log(6);
        assert!(log[4].starts_with("failed UnknownEntry"), "{log:?}");
        assert!(log[4].contains("missing"), "{log:?}");
        assert!(log[5].starts_with("failed AlreadyRunning"), "{log:?}");
        assert_eq!(log.iter().filter(|l| l.starts_with("starting")).count(), 0);
    }

    #[test]
    fn the_program_list_follows_the_configuration() {
        let (client_end, server_end) = pair();
        let server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind(client_end).unwrap();
        client.wait_for_log(4);
        server.send(Command::SetPrograms(vec![]));
        server.send(Command::SetPrograms(vec!["htop"]));
        assert_eq!(
            client.wait_for_log(9)[4..],
            [
                "availability 0",
                "programs_done",
                "availability 1",
                "program htop",
                "programs_done"
            ]
        );
    }

    #[test]
    fn an_unchanged_program_list_is_not_announced_again() {
        let (client_end, server_end) = pair();
        let server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind(client_end).unwrap();
        client.wait_for_log(4);
        // Same programs as the server started with, in another order.
        server.send(Command::SetPrograms(vec!["foot", "Alacritty"]));
        std::thread::sleep(Duration::from_millis(50));
        client.queue.roundtrip(&mut client.app).unwrap();
        assert_eq!(client.app.log, HELLO);
    }

    #[test]
    fn destroying_the_manager_stops_program_updates() {
        let (client_end, server_end) = pair();
        let server = Server::start(vec![(server_end, ClientState::default())]);
        let mut client = Client::bind(client_end).unwrap();
        client.wait_for_log(4);
        client.launcher.destroy();
        client.conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        server.send(Command::SetPrograms(vec![]));
        std::thread::sleep(Duration::from_millis(50));
        client.queue.roundtrip(&mut client.app).unwrap();
        assert_eq!(client.app.log, HELLO);
    }

    #[test]
    fn the_global_is_hidden_from_guest_applications() {
        let (guest_end, guest_server_end) = pair();
        let (host_end, host_server_end) = pair();
        let _server = Server::start(vec![
            (
                guest_server_end,
                ClientState {
                    vm: Some(3),
                    ..ClientState::default()
                },
            ),
            (host_server_end, ClientState::default()),
        ]);
        assert!(
            Client::bind(guest_end).is_none(),
            "a VM guest must not see the global"
        );
        assert!(Client::bind(host_end).is_some(), "a host client must");
    }
}
