//! Bridges the compositor's `ext-workspace-v1` global (see
//! `crate::ext_workspace`) - plus, best-effort, its own
//! `ironland-workspace-windows-v1` (see `crate::workspace_windows`) - to
//! line-delimited JSON on stdin/stdout.
//!
//! This exists because Quickshell (what molunga-shell is built on) has no
//! built-in support for `ext-workspace-v1` as of the version this was
//! written against - unlike, say, its Hyprland IPC integration. Rather than
//! bake compositor-specific glue into the shell, this is a small standalone
//! Wayland client any shell can spawn and talk newline-JSON to.
//!
//! ## Output (one JSON object per line, written whenever either protocol's `done` fires)
//!
//! ```json
//! {"outputs":[{"name":"eDP-1","workspaces":[{"index":0,"name":"1","active":true,"windows":[{"title":"~","appId":"foot"}]},{"index":1,"name":"2","active":false,"windows":[]}]}]}
//! ```
//!
//! `windows` is empty on a compositor that doesn't support
//! `ironland-workspace-windows-v1`, and is matched to its workspace by
//! title/app id best-effort (see that protocol's doc for why) rather than a
//! stable id.
//!
//! ## Input (one JSON object per line, read from stdin)
//!
//! ```json
//! {"activate":{"output":"eDP-1","index":1}}
//! {"setFloating":{"output":"eDP-1","index":1,"title":"~","appId":"foot","floating":true}}
//! ```
//!
//! `setFloating` is a no-op on a compositor that doesn't support
//! `ironland-workspace-windows-v1` (see the `windows` field above), and
//! matches the target window the same best-effort way that field does.
//!
//! Unknown/malformed lines and commands naming an output that doesn't
//! currently exist are silently ignored. An `activate` naming an index one
//! past the last known workspace on its output requests a new trailing
//! workspace (`ext_workspace_group_handle_v1.create_workspace`) and
//! activates it instead - a no-op compositor-side unless `workspaces.dynamic`
//! is set - and any other out-of-range index is ignored.
//!
//! Known limitation: outputs that connect *after* this process starts won't
//! be picked up, since `wl_output` globals are only bound once at startup
//! (see the registry burst below). Good enough for a panel that's expected
//! to be restarted along with (or shortly after) the compositor session.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use calloop::EventLoop;
use calloop::channel::{self, Channel};
use calloop_wayland_source::WaylandSource;
use serde::{Deserialize, Serialize};
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::{
    self, ExtWorkspaceGroupHandleV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    self, ExtWorkspaceHandleV1, State as WsState,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::{
    self, ExtWorkspaceManagerV1,
};

/// Generated client bindings for `ironland-workspace-windows-v1` (see
/// `protocols/ironland-workspace-windows-v1.xml`) - not in the
/// `wayland-protocols` crate since it's our own, so generated here the same
/// way `crate::ironland_protocols` generates the compositor's server side
/// of it (and the other two protocols in that module).
mod workspace_windows_protocol {
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
        wayland_scanner::generate_interfaces!("./protocols/ironland-workspace-windows-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocols/ironland-workspace-windows-v1.xml");
}
use workspace_windows_protocol::ironland_workspace_windows_v1::{self, IronlandWorkspaceWindowsV1};

#[derive(Deserialize)]
struct ActivateCommand {
    output: String,
    index: usize,
}

#[derive(Deserialize)]
struct SetFloatingCommand {
    output: String,
    index: usize,
    title: String,
    #[serde(rename = "appId")]
    app_id: String,
    floating: bool,
}

#[derive(Deserialize)]
enum Command {
    #[serde(rename = "activate")]
    Activate(ActivateCommand),
    #[serde(rename = "setFloating")]
    SetFloating(SetFloatingCommand),
}

#[derive(Clone, Serialize)]
struct WindowJson {
    title: String,
    #[serde(rename = "appId")]
    app_id: String,
    floating: bool,
}

#[derive(Serialize)]
struct WorkspaceJson {
    index: usize,
    name: String,
    active: bool,
    /// Best-effort: from `ironland-workspace-windows-v1` (see that
    /// protocol's doc for why this is title/app-id keyed rather than a
    /// stable id), keyed to this workspace by `(output name, index)`.
    /// Empty until at least one `done` of that protocol has arrived.
    windows: Vec<WindowJson>,
}

#[derive(Serialize)]
struct OutputJson {
    name: String,
    workspaces: Vec<WorkspaceJson>,
}

#[derive(Serialize)]
struct StateJson {
    outputs: Vec<OutputJson>,
}

struct WorkspaceEntry {
    handle: ExtWorkspaceHandleV1,
    index: usize,
    name: String,
    active: bool,
}

struct GroupEntry {
    handle: ExtWorkspaceGroupHandleV1,
    output_name: Option<String>,
    workspaces: Vec<WorkspaceEntry>,
}

struct App {
    manager: Option<ExtWorkspaceManagerV1>,
    /// Kept alive (rather than just bound and dropped) so [`App::handle_command`]
    /// can send `set_floating` requests through it later. `None` on a
    /// compositor that doesn't support the protocol at all.
    workspace_windows: Option<IronlandWorkspaceWindowsV1>,
    output_names: HashMap<ObjectId, String>,
    groups: Vec<GroupEntry>,
    /// Settled snapshot from the last `ironland-workspace-windows-v1` done,
    /// keyed by (output name, workspace index).
    windows_by_workspace: HashMap<(String, u32), Vec<WindowJson>>,
    /// Accumulates `window` events between two `done`s, then replaces
    /// `windows_by_workspace` wholesale on `done` - the same snapshot
    /// pattern `ext-workspace-v1` itself uses.
    pending_windows: HashMap<(String, u32), Vec<WindowJson>>,
}

impl App {
    fn emit_state(&self) {
        let outputs: Vec<OutputJson> = self
            .groups
            .iter()
            .filter_map(|g| {
                let name = g.output_name.clone()?;
                Some(OutputJson {
                    workspaces: g
                        .workspaces
                        .iter()
                        .map(|w| WorkspaceJson {
                            index: w.index,
                            name: w.name.clone(),
                            active: w.active,
                            windows: self
                                .windows_by_workspace
                                .get(&(name.clone(), w.index as u32))
                                .cloned()
                                .unwrap_or_default(),
                        })
                        .collect(),
                    name,
                })
            })
            .collect();

        let line = serde_json::to_string(&StateJson { outputs }).unwrap();
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, "{line}");
        let _ = lock.flush();
    }

    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Activate(ActivateCommand { output, index }) => {
                let Some(group) = self
                    .groups
                    .iter()
                    .find(|g| g.output_name.as_deref() == Some(output.as_str()))
                else {
                    return;
                };
                if let Some(workspace) = group.workspaces.iter().find(|w| w.index == index) {
                    workspace.handle.activate();
                } else if index == group.workspaces.len() {
                    // One past the last known workspace: there's no handle
                    // to activate yet, so ask the compositor to grow the
                    // output by one trailing workspace and activate it (see
                    // `ext_workspace::create_workspace` server-side; a no-op
                    // there unless `workspaces.dynamic` is set).
                    group.handle.create_workspace((index + 1).to_string());
                } else {
                    return;
                }
                if let Some(manager) = &self.manager {
                    manager.commit();
                }
            }
            Command::SetFloating(SetFloatingCommand {
                output,
                index,
                title,
                app_id,
                floating,
            }) => {
                if let Some(manager) = &self.workspace_windows {
                    manager.set_floating(output, index as u32, title, app_id, floating as u32);
                }
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for App {
    fn event(
        _state: &mut App,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &wayland_client::globals::GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        // Outputs and the manager global are all bound once at startup (see
        // `main`); see this module's doc comment for why hotplugged outputs
        // aren't picked up.
    }
}

impl Dispatch<WlOutput, ()> for App {
    fn event(
        state: &mut App,
        proxy: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_names.insert(proxy.id(), name);
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for App {
    fn event(
        state: &mut App,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.groups.push(GroupEntry {
                    handle: workspace_group,
                    output_name: None,
                    workspaces: Vec::new(),
                });
            }
            // Workspaces are unassigned to a group until `WorkspaceEnter`
            // arrives on one; that's where we file it away (see the
            // `ExtWorkspaceGroupHandleV1` dispatch impl below), so there's
            // nothing to do with the bare handle here.
            ext_workspace_manager_v1::Event::Workspace { .. } => {}
            ext_workspace_manager_v1::Event::Done => state.emit_state(),
            ext_workspace_manager_v1::Event::Finished => std::process::exit(0),
            _ => {}
        }
    }

    event_created_child!(App, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, PendingWorkspace::default()),
    ]);
}

/// User data for a freshly-created `ext_workspace_handle_v1`, before we know
/// which group it belongs to (that only arrives via `workspace_enter` on the
/// group). Holds what we learn about it in the meantime so nothing is lost
/// once it does get filed into a [`GroupEntry`].
#[derive(Default)]
struct PendingWorkspace {
    name: std::sync::Mutex<Option<String>>,
    active: std::sync::Mutex<bool>,
}

impl Dispatch<ExtWorkspaceHandleV1, PendingWorkspace> for App {
    fn event(
        state: &mut App,
        proxy: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        data: &PendingWorkspace,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        // Once a workspace has been filed into a group (see
        // `ExtWorkspaceGroupHandleV1::WorkspaceEnter` below), further events
        // for it are handled by mutating the `WorkspaceEntry` directly
        // rather than through this `PendingWorkspace`, since by then we know
        // its group/index. Find it first; if it's not filed yet, stash into
        // `data` instead.
        let existing = state
            .groups
            .iter_mut()
            .flat_map(|g| g.workspaces.iter_mut())
            .find(|w| w.handle == *proxy);

        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                if let Some(entry) = existing {
                    entry.name = name;
                } else {
                    *data.name.lock().unwrap() = Some(name);
                }
            }
            ext_workspace_handle_v1::Event::State {
                state: WEnum::Value(bits),
            } => {
                let active = bits.contains(WsState::Active);
                if let Some(entry) = existing {
                    entry.active = active;
                } else {
                    *data.active.lock().unwrap() = active;
                }
            }
            ext_workspace_handle_v1::Event::Removed => proxy.destroy(),
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for App {
    fn event(
        state: &mut App,
        proxy: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        let Some(group) = state.groups.iter_mut().find(|g| g.handle == *proxy) else {
            return;
        };
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                group.output_name = state.output_names.get(&output.id()).cloned();
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                let (name, active) = workspace
                    .data::<PendingWorkspace>()
                    .map(|p| (p.name.lock().unwrap().clone(), *p.active.lock().unwrap()))
                    .unwrap_or_default();
                group.workspaces.push(WorkspaceEntry {
                    index: group.workspaces.len(),
                    name: name.unwrap_or_default(),
                    active,
                    handle: workspace,
                });
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                group.workspaces.retain(|w| w.handle != workspace);
                for (i, w) in group.workspaces.iter_mut().enumerate() {
                    w.index = i;
                }
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                proxy.destroy();
                let id = proxy.id();
                state.groups.retain(|g| g.handle.id() != id);
            }
            _ => {}
        }
    }
}

impl Dispatch<IronlandWorkspaceWindowsV1, ()> for App {
    fn event(
        state: &mut App,
        _proxy: &IronlandWorkspaceWindowsV1,
        event: ironland_workspace_windows_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        match event {
            ironland_workspace_windows_v1::Event::Window {
                output,
                workspace,
                title,
                app_id,
                floating,
            } => {
                state
                    .pending_windows
                    .entry((output, workspace))
                    .or_default()
                    .push(WindowJson {
                        title,
                        app_id,
                        floating: floating != 0,
                    });
            }
            ironland_workspace_windows_v1::Event::Done => {
                state.windows_by_workspace = std::mem::take(&mut state.pending_windows);
                state.emit_state();
            }
        }
    }
}

fn main() {
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("ironland-workspaces: failed to connect to Wayland display: {err}");
            std::process::exit(1);
        }
    };
    let (globals, event_queue) =
        registry_queue_init::<App>(&conn).expect("failed to init registry");
    let qh = event_queue.handle();

    let mut app = App {
        manager: None,
        workspace_windows: None,
        output_names: HashMap::new(),
        groups: Vec::new(),
        windows_by_workspace: HashMap::new(),
        pending_windows: HashMap::new(),
    };

    for global in globals.contents().clone_list() {
        if global.interface == WlOutput::interface().name {
            let _: WlOutput = globals.registry().bind(
                global.name,
                global.version.min(WlOutput::interface().version),
                &qh,
                (),
            );
        }
    }

    app.manager = match globals.bind::<ExtWorkspaceManagerV1, _, _>(&qh, 1..=1, ()) {
        Ok(manager) => Some(manager),
        Err(err) => {
            eprintln!("ironland-workspaces: compositor doesn't support ext-workspace-v1: {err}");
            std::process::exit(1);
        }
    };

    // Best-effort: a compositor without this protocol just never gets
    // per-workspace window data (every workspace's `windows` stays empty)
    // and `setFloating` becomes a no-op, rather than failing to start.
    match globals.bind::<IronlandWorkspaceWindowsV1, _, _>(&qh, 1..=2, ()) {
        Ok(proxy) => app.workspace_windows = Some(proxy),
        Err(err) => eprintln!(
            "ironland-workspaces: compositor doesn't support ironland-workspace-windows-v1, \
             per-workspace window lists will be empty and setFloating will be a no-op: {err}"
        ),
    }

    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("failed to create event loop");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .expect("failed to insert Wayland source");

    let (tx, rx): (_, Channel<Command>) = channel::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(&line) {
                Ok(cmd) => {
                    if tx.send(cmd).is_err() {
                        break;
                    }
                }
                Err(err) => eprintln!("ironland-workspaces: ignoring malformed command: {err}"),
            }
        }
    });
    loop_handle
        .insert_source(rx, |event, _, app| {
            if let channel::Event::Msg(cmd) = event {
                app.handle_command(cmd);
            }
        })
        .expect("failed to insert stdin command source");

    event_loop
        .run(None, &mut app, |_| {})
        .expect("event loop error");
}
