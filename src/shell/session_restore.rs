//! Session save and restore: the desktop layout survives a logout or reboot.
//!
//! While the session runs, [`AnvilState::session_tick`] (once a second)
//! captures a [`Snapshot`] (every output's workspaces, every window's place -
//! output, workspace, tiled or floating and where - and how to start its
//! app) and, once it has stayed the same for `session_restore.save_delay_ms`,
//! writes it to `$XDG_STATE_HOME/ironland-compositor/session.json`. At the
//! next start [`AnvilState::begin_session_restore`] reads it back, restores
//! the active workspaces, starts the apps and hands every window that shows
//! up to the place it had.
//!
//! ## How a window finds its place
//!
//! Nothing here can know in advance which window an app will open, so a
//! started app *claims* a saved window ([`Restore::pick`]) when its window
//! appears. In order of preference:
//!
//! 1. the `xdg-session-management-v1` name the client restores itself with
//!    ([`crate::xdg_session`]) - exact, no guessing;
//! 2. the VM the window's client belongs to (persistent VMs);
//! 3. the process: the window's client is the process the compositor started
//!    or a descendant of it (wrapper scripts, Electron, ...);
//! 4. the app id, for apps that hand over to an already-running instance
//!    (the started process exits) and for X11 windows, which have no pid.
//!
//! Windows made by one process are saved as one *group* and the app is
//! started once per group, not once per window: a browser with three windows
//! is one process, and starting it three times would give three browsers'
//! worth of windows. A slot nobody claims within `claim_timeout_secs` is
//! dropped.
//!
//! ## What is deliberately not saved
//!
//! Dialogs (windows with a parent), apps that can't be started again, and
//! throw-away VMs. Nothing is written while a restore is under way (the
//! half-built layout would replace the good one), nor once the compositor is
//! shutting down, and a layout with no windows at all replaces one with
//! windows only after it has been that way for [`EMPTY_HOLD`] - the clients
//! of a dying session close together, which must not look like the user
//! closing everything.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Child,
    sync::atomic::Ordering,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use smithay::{
    desktop::space::SpaceElement,
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Point, Rectangle},
};
use tracing::{debug, info, warn};

use super::{WindowElement, tiling, tiling::TreeShape, vm::VmId, workspace};
use crate::{
    launcher::{self, DesktopEntry},
    state::{AnvilState, Backend},
};

/// Version of the file format; a file of another version is left alone.
pub const SNAPSHOT_VERSION: u32 = 1;

/// How often the layout is looked at.
const TICK: Duration = Duration::from_secs(1);

/// How long a layout without any window has to persist before it replaces a
/// saved one that has windows (see the module doc).
pub const EMPTY_HOLD: Duration = Duration::from_secs(30);

/// Upper bound on remembered `xdg-session-management-v1` sessions; the least
/// recently used ones go first.
const MAX_XDG_SESSIONS: usize = 128;

/// The most parent processes looked at when deciding whether a window's
/// process descends from one the compositor started.
const MAX_ANCESTORS: usize = 64;

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

/// A window's rectangle in the global space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Where a window lives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// Connector name of the output (`eDP-1`).
    pub output: String,
    pub workspace: usize,
    /// `Some`: floating there. `None`: tiled (its slot is in [`Snapshot::trees`]).
    pub floating: Option<Geometry>,
}

/// How an app is started again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Launch {
    /// A desktop entry (`org.mozilla.firefox.desktop`): immune to the store
    /// path of the program changing with a system update.
    Entry { id: String },
    /// The command the window's process was started with, for apps without
    /// a desktop entry.
    Command { argv: Vec<String> },
    /// A persistent VM running the entry's program.
    Vm { entry: String },
}

/// What an `xdg-session-management-v1` client calls a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XdgRef {
    pub session: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSnap {
    pub app_id: String,
    pub launch: Launch,
    /// Windows with the same group came from one process; the app is started
    /// once for the whole group.
    pub group: usize,
    #[serde(flatten)]
    pub placement: Placement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xdg: Option<XdgRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSnap {
    pub name: String,
    pub active: usize,
    pub count: usize,
}

/// The tiling tree of one workspace; leaves are indexes into
/// [`Snapshot::windows`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeSnap {
    pub output: String,
    pub workspace: usize,
    pub shape: TreeShape<usize>,
}

/// An `xdg-session-management-v1` session: what the compositor remembers of
/// a client's toplevels so the client can get them back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XdgSessionRecord {
    pub id: String,
    pub toplevels: BTreeMap<String, Placement>,
    /// Seconds since the epoch; decides what is forgotten first.
    pub last_used: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub outputs: Vec<OutputSnap>,
    pub windows: Vec<WindowSnap>,
    pub trees: Vec<TreeSnap>,
    #[serde(default)]
    pub sessions: Vec<XdgSessionRecord>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            outputs: Vec::new(),
            windows: Vec::new(),
            trees: Vec::new(),
            sessions: Vec::new(),
        }
    }
}

/// Where the snapshot lives: `$IRONLAND_SESSION_FILE`, else
/// `$XDG_STATE_HOME/ironland-compositor/session.json`.
pub fn session_file() -> PathBuf {
    if let Some(path) = std::env::var_os("IRONLAND_SESSION_FILE") {
        return PathBuf::from(path);
    }
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    state_home.join("ironland-compositor/session.json")
}

/// Reads the snapshot at `path`. `Ok(None)` when there is none (or it is of
/// another version).
pub fn read_snapshot(path: &Path) -> Result<Option<Snapshot>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("reading {}: {err}", path.display())),
    };
    let snapshot: Snapshot =
        serde_json::from_str(&text).map_err(|err| format!("parsing {}: {err}", path.display()))?;
    if snapshot.version != SNAPSHOT_VERSION {
        warn!(
            version = snapshot.version,
            "Ignoring a saved session of another format version"
        );
        return Ok(None);
    }
    Ok(Some(snapshot))
}

/// Writes the snapshot atomically (a crash leaves the old file or the new
/// one, never half of one).
pub fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|err| format!("creating {}: {err}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(snapshot).map_err(|err| err.to_string())?;
    // Command lines can carry URLs and tokens: for the user's eyes only.
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|err| format!("writing {}: {err}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("writing {}: {err}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|err| format!("replacing {}: {err}", path.display()))
}

// ---------------------------------------------------------------------------
// Deciding when to write
// ---------------------------------------------------------------------------

/// Debounces snapshots: a layout is written once it has stayed the same for
/// the delay.
#[derive(Debug, Default)]
pub struct Saver {
    written: Option<Snapshot>,
    candidate: Option<(Snapshot, Instant)>,
}

impl Saver {
    /// Starts out knowing what is on disk, so an unchanged layout isn't
    /// written again.
    pub fn with_written(written: Option<Snapshot>) -> Self {
        Self { written, candidate: None }
    }

    /// Takes the current layout; returns the one to write now, if any.
    pub fn observe(&mut self, snapshot: Snapshot, now: Instant, delay: Duration) -> Option<Snapshot> {
        if self.written.as_ref() == Some(&snapshot) {
            self.candidate = None;
            return None;
        }
        match &self.candidate {
            Some((candidate, _)) if *candidate == snapshot => {}
            _ => self.candidate = Some((snapshot, now)),
        }
        let (candidate, since) = self.candidate.as_ref()?;
        let dropping_all_windows =
            candidate.windows.is_empty() && self.written.as_ref().is_some_and(|w| !w.windows.is_empty());
        let hold = if dropping_all_windows { delay.max(EMPTY_HOLD) } else { delay };
        if now.saturating_duration_since(*since) < hold {
            return None;
        }
        let snapshot = candidate.clone();
        self.written = Some(snapshot.clone());
        self.candidate = None;
        Some(snapshot)
    }
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

/// The parent pid in the contents of `/proc/<pid>/stat`.
pub fn parse_ppid(stat: &str) -> Option<u32> {
    // The command name (field 2) is in parentheses and may contain anything,
    // spaces and parentheses included: everything after the *last* `)` is
    // plain fields - state, then the parent pid.
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn parent_pid(pid: u32) -> Option<u32> {
    parse_ppid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Whether `pid` is `ancestor` or has it among its parents.
pub fn descends_from(pid: u32, ancestor: u32, parent_of: impl Fn(u32) -> Option<u32>) -> bool {
    let mut current = pid;
    for _ in 0..MAX_ANCESTORS {
        if current == ancestor {
            return true;
        }
        match parent_of(current) {
            Some(parent) if parent > 1 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// Whether `pid` was started by systemd itself (a service's main process, as
/// opposed to something another program started): the desktop shell, an app
/// an autostart unit runs. Systemd brings those back by itself, so restoring
/// them too would run them twice - the shell most of all.
pub fn is_service_process(
    pid: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    comm_of: impl Fn(u32) -> Option<String>,
) -> bool {
    let Some(parent) = parent_of(pid) else {
        return false;
    };
    parent == 1 || comm_of(parent).is_some_and(|comm| comm.trim() == "systemd")
}

fn comm_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()
}

/// The command line of `pid`, from the contents of `/proc/<pid>/cmdline`.
pub fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let argv = parse_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
    (!argv.is_empty()).then_some(argv)
}

// ---------------------------------------------------------------------------
// Matching windows to saved slots
// ---------------------------------------------------------------------------

/// What is known about a window that just appeared.
#[derive(Debug, Default, Clone)]
pub struct ClaimQuery<'a> {
    pub pid: Option<u32>,
    pub vm: Option<VmId>,
    /// Empty while the app id isn't known yet.
    pub app_id: &'a str,
    pub xdg: Option<&'a XdgRef>,
}

/// One app started for a restore.
#[derive(Debug)]
pub struct Group {
    /// Indexes into [`Snapshot::windows`].
    pub members: Vec<usize>,
    pub launch: Launch,
    pub pid: Option<u32>,
    pub vm: Option<VmId>,
    pub child: Option<Child>,
    /// The started process is gone (an app that hands over to a running
    /// instance, or one that died).
    pub exited: bool,
    pub deadline: Instant,
    /// Given up on: it never showed its windows in time (or failed to start).
    pub expired: bool,
}

/// A restore under way.
#[derive(Debug)]
pub struct Restore<W = WindowElement> {
    pub snapshot: Snapshot,
    pub groups: Vec<Group>,
    /// Saved window index -> the window that took its place.
    pub claimed: HashMap<usize, W>,
}

impl<W> Restore<W> {
    /// The saved window `query` belongs to, if any (see the module doc for
    /// the order of preference). Doesn't mark it claimed.
    pub fn pick(&self, query: &ClaimQuery<'_>, parent_of: impl Fn(u32) -> Option<u32> + Copy) -> Option<usize> {
        let open = |slot: &&usize| !self.claimed.contains_key(*slot);
        let live = |group: &&Group| !group.expired;
        let first_open = |group: &Group| group.members.iter().filter(open).copied().next();

        if let Some(xdg) = query.xdg
            && let Some(slot) = (0..self.snapshot.windows.len())
                .filter(|slot| !self.claimed.contains_key(slot))
                .find(|slot| self.snapshot.windows[*slot].xdg.as_ref() == Some(xdg))
        {
            return Some(slot);
        }
        if let Some(vm) = query.vm {
            return self
                .groups
                .iter()
                .filter(live)
                .find(|g| g.vm == Some(vm))
                .and_then(first_open);
        }
        if let Some(pid) = query.pid
            && let Some(slot) = self
                .groups
                .iter()
                .filter(live)
                .filter(|g| g.pid.is_some_and(|started| descends_from(pid, started, parent_of)))
                .find_map(first_open)
        {
            return Some(slot);
        }
        if query.app_id.is_empty() {
            return None;
        }
        // By app id: only groups whose process can't be told apart by pid -
        // it exited, or the window has no pid to compare. Otherwise a window
        // the user opened by hand would take the place of one that is still
        // on its way.
        self.groups
            .iter()
            .filter(live)
            .filter(|g| g.vm.is_none() && (g.exited || g.pid.is_none() || query.pid.is_none()))
            .find_map(|group| {
                group
                    .members
                    .iter()
                    .filter(open)
                    .copied()
                    .find(|slot| self.snapshot.windows[*slot].app_id.eq_ignore_ascii_case(query.app_id))
            })
    }

    /// Whether every window of every group has been claimed or given up on.
    pub fn is_finished(&self) -> bool {
        self.groups
            .iter()
            .all(|g| g.expired || g.members.iter().all(|m| self.claimed.contains_key(m)))
    }
}

/// Numbers `keys` so equal keys share a number, in order of first appearance.
pub fn assign_groups<K: Eq + std::hash::Hash + Clone>(keys: &[K]) -> Vec<usize> {
    let mut seen: HashMap<K, usize> = HashMap::new();
    keys.iter()
        .map(|key| {
            let next = seen.len();
            *seen.entry(key.clone()).or_insert(next)
        })
        .collect()
}

/// `geometry` moved so its top-left corner lies inside `bounds` with at least
/// `margin` pixels of the window still on screen (the monitor may have
/// changed since the layout was saved).
pub fn clamp_into(geometry: Geometry, bounds: Rectangle<i32, Logical>, margin: i32) -> (i32, i32) {
    let max_x = (bounds.loc.x + bounds.size.w - margin).max(bounds.loc.x);
    let max_y = (bounds.loc.y + bounds.size.h - margin).max(bounds.loc.y);
    (geometry.x.clamp(bounds.loc.x, max_x), geometry.y.clamp(bounds.loc.y, max_y))
}

// ---------------------------------------------------------------------------
// xdg-session-management-v1 registry
// ---------------------------------------------------------------------------

/// A window a client has put in a session.
#[derive(Debug, Clone)]
struct Binding {
    session: String,
    name: String,
    surface: WlSurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// Another window of the session already has this name.
    NameInUse,
    /// The window is already in a session.
    AlreadyAdded,
}

/// The sessions `xdg-session-management-v1` clients keep, and which live
/// windows are bound to them. Persisted in the snapshot.
#[derive(Debug, Default)]
pub struct XdgRegistry {
    sessions: BTreeMap<String, XdgSessionRecord>,
    bindings: Vec<Binding>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_session_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

impl XdgRegistry {
    pub fn load(&mut self, records: &[XdgSessionRecord]) {
        for record in records {
            self.sessions.insert(record.id.clone(), record.clone());
        }
    }

    pub fn records(&self) -> Vec<XdgSessionRecord> {
        self.sessions.values().cloned().collect()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    /// Opens the session `id`; an unknown (or absent) id starts a new one.
    /// Returns the id to use and whether earlier state was found.
    pub fn open(&mut self, id: Option<&str>) -> (String, bool) {
        if let Some(id) = id
            && let Some(record) = self.sessions.get_mut(id)
        {
            record.last_used = unix_now();
            return (id.to_string(), true);
        }
        let id = new_session_id();
        self.sessions.insert(
            id.clone(),
            XdgSessionRecord { id: id.clone(), toplevels: BTreeMap::new(), last_used: unix_now() },
        );
        while self.sessions.len() > MAX_XDG_SESSIONS {
            let oldest = self
                .sessions
                .values()
                .filter(|r| r.id != id)
                .min_by_key(|r| r.last_used)
                .map(|r| r.id.clone());
            match oldest {
                Some(oldest) => {
                    self.sessions.remove(&oldest);
                }
                None => break,
            }
        }
        (id, false)
    }

    fn live_bindings(&mut self) -> &mut Vec<Binding> {
        self.bindings.retain(|b| b.surface.is_alive());
        &mut self.bindings
    }

    /// Whether `name` in `session` is held by a window that still exists.
    pub fn name_taken(&mut self, session: &str, name: &str) -> bool {
        self.live_bindings().iter().any(|b| b.session == session && b.name == name)
    }

    pub fn bind(&mut self, session: &str, name: &str, surface: &WlSurface) -> Result<(), BindError> {
        let bindings = self.live_bindings();
        if bindings.iter().any(|b| &b.surface == surface) {
            return Err(BindError::AlreadyAdded);
        }
        if bindings.iter().any(|b| b.session == session && b.name == name) {
            return Err(BindError::NameInUse);
        }
        bindings.push(Binding { session: session.to_string(), name: name.to_string(), surface: surface.clone() });
        Ok(())
    }

    pub fn rename(&mut self, session: &str, old: &str, new: &str) -> Result<(), BindError> {
        if old == new {
            return Ok(());
        }
        if self.name_taken(session, new) {
            return Err(BindError::NameInUse);
        }
        for binding in self.bindings.iter_mut().filter(|b| b.session == session && b.name == old) {
            binding.name = new.to_string();
        }
        if let Some(record) = self.sessions.get_mut(session)
            && let Some(placement) = record.toplevels.remove(old)
        {
            record.toplevels.insert(new.to_string(), placement);
        }
        Ok(())
    }

    /// The session's windows stop being tracked (the session object was
    /// destroyed); what was remembered stays.
    pub fn unbind_session(&mut self, session: &str) {
        self.bindings.retain(|b| b.session != session);
    }

    pub fn unbind_surface(&mut self, surface: &WlSurface) {
        self.bindings.retain(|b| &b.surface != surface);
    }

    pub fn remove_toplevel(&mut self, session: &str, name: &str) {
        self.bindings.retain(|b| !(b.session == session && b.name == name));
        if let Some(record) = self.sessions.get_mut(session) {
            record.toplevels.remove(name);
        }
    }

    /// Forgets the session altogether.
    pub fn remove(&mut self, session: &str) {
        self.unbind_session(session);
        self.sessions.remove(session);
    }

    pub fn placement(&self, session: &str, name: &str) -> Option<&Placement> {
        self.sessions.get(session)?.toplevels.get(name)
    }

    pub fn set_placement(&mut self, session: &str, name: &str, placement: Placement) {
        if let Some(record) = self.sessions.get_mut(session) {
            record.toplevels.insert(name.to_string(), placement);
        }
    }

    /// The live windows in sessions: `(session, name, surface)`.
    pub fn bound(&mut self) -> Vec<(String, String, WlSurface)> {
        self.live_bindings()
            .iter()
            .map(|b| (b.session.clone(), b.name.clone(), b.surface.clone()))
            .collect()
    }

    pub fn reference_for(&mut self, surface: &WlSurface) -> Option<XdgRef> {
        self.live_bindings()
            .iter()
            .find(|b| &b.surface == surface)
            .map(|b| XdgRef { session: b.session.clone(), name: b.name.clone() })
    }
}

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// The session isn't up yet: nothing is saved, nothing restored.
    #[default]
    Waiting,
    /// Apps are being started and their windows put in place.
    Restoring,
    /// Normal operation: the layout is saved as it changes.
    Running,
}

#[derive(Debug, Default)]
pub struct SessionRestore {
    pub phase: Phase,
    pub restore: Option<Restore>,
    pub saver: Saver,
    pub xdg: XdgRegistry,
    pub service_started: bool,
}

/// Session bookkeeping kept on the window itself.
#[derive(Default)]
struct WindowInfo {
    /// The saved window this one took the place of.
    claimed: Cell<Option<usize>>,
    /// `(app id it was worked out for, how to start it)`.
    launch: RefCell<Option<(String, Option<Launch>)>>,
}

impl WindowInfo {
    fn get(window: &WindowElement) -> &WindowInfo {
        window.user_data().insert_if_missing(WindowInfo::default);
        window.user_data().get::<WindowInfo>().unwrap()
    }
}

/// What makes windows belong to one app instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GroupKey {
    Pid(u32),
    Vm(VmId),
    /// A window of its own (X11: no pid to group by).
    Alone(usize),
}

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    /// Starts the once-a-second layout tracking and schedules the restore.
    /// Call once the session is up (the shell has been started).
    pub fn start_session_restore(&mut self) {
        if self.session_restore.service_started || !self.config.session_restore.enabled {
            return;
        }
        self.session_restore.service_started = true;
        let delay = Duration::from_millis(u64::from(self.config.session_restore.restore_delay_ms));
        let begin = self
            .handle
            .insert_source(Timer::from_duration(delay), |_, _, data| {
                data.begin_session_restore();
                TimeoutAction::Drop
            });
        let tick = self.handle.insert_source(Timer::from_duration(TICK), |_, _, data| {
            data.session_tick();
            TimeoutAction::ToDuration(TICK)
        });
        if let Err(err) = begin.and(tick) {
            warn!(%err, "Could not start the session restore timers");
        }
    }

    /// Reads the saved session and sets about bringing it back.
    pub fn begin_session_restore(&mut self) {
        if !self.config.session_restore.enabled || self.session_restore.phase != Phase::Waiting {
            return;
        }
        let path = session_file();
        let snapshot = match read_snapshot(&path) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                debug!(path = %path.display(), "No saved session to restore");
                self.session_restore.phase = Phase::Running;
                return;
            }
            Err(err) => {
                warn!(%err, "Could not read the saved session");
                self.session_restore.phase = Phase::Running;
                return;
            }
        };
        self.session_restore.xdg.load(&snapshot.sessions);
        self.session_restore.saver = Saver::with_written(Some(snapshot.clone()));

        // Where the user was.
        for saved in &snapshot.outputs {
            let Some(output) = self.all_outputs().find(|o| o.name() == saved.name).cloned() else {
                continue;
            };
            workspace::ensure_workspaces(&output, saved.count.max(saved.active + 1));
            workspace::activate_workspace(self, &output, saved.active);
        }

        // One start per group of windows.
        let timeout = Duration::from_secs(u64::from(self.config.session_restore.claim_timeout_secs.max(1)));
        let entries = launcher::scan_desktop_entries();
        let envs: Vec<_> = self.compositor_envs().collect();
        let mut groups: Vec<Group> = Vec::new();
        let group_count = snapshot.windows.iter().map(|w| w.group + 1).max().unwrap_or(0);
        for index in 0..group_count {
            let members: Vec<usize> = (0..snapshot.windows.len())
                .filter(|slot| snapshot.windows[*slot].group == index)
                .collect();
            let Some(first) = members.first() else { continue };
            let launch = snapshot.windows[*first].launch.clone();
            let mut group = Group {
                members,
                launch: launch.clone(),
                pid: None,
                vm: None,
                child: None,
                exited: false,
                deadline: Instant::now() + timeout,
                expired: false,
            };
            match self.start_group(&launch, &entries, &envs) {
                Ok(Started::Process(child)) => {
                    group.pid = Some(child.id());
                    group.child = Some(child);
                }
                Ok(Started::Vm(vm)) => group.vm = Some(vm),
                Err(err) => {
                    warn!(?launch, %err, "Could not start an app of the saved session");
                    group.expired = true;
                }
            }
            groups.push(group);
        }
        info!(
            windows = snapshot.windows.len(),
            apps = groups.len(),
            "Restoring the saved session"
        );
        let restore = Restore { snapshot, groups, claimed: HashMap::new() };
        if restore.is_finished() {
            self.session_restore.phase = Phase::Running;
        } else {
            self.session_restore.phase = Phase::Restoring;
            self.session_restore.restore = Some(restore);
        }
    }

    fn start_group(
        &mut self,
        launch: &Launch,
        entries: &[DesktopEntry],
        envs: &[(&'static str, Option<String>)],
    ) -> Result<Started, String> {
        match launch {
            Launch::Entry { id } => {
                let entry = entries
                    .iter()
                    .find(|e| &e.id == id)
                    .ok_or_else(|| format!("no desktop entry {id}"))?;
                launcher::launch(entry, envs).map(Started::Process).map_err(|e| e.to_string())
            }
            Launch::Command { argv } => {
                launcher::launch_command(argv, envs).map(Started::Process).map_err(|e| e.to_string())
            }
            Launch::Vm { entry } => {
                if !self.config.session_restore.restore_vms {
                    return Err("restoring VMs is switched off".into());
                }
                let entry = entries
                    .iter()
                    .find(|e| &e.id == entry)
                    .ok_or_else(|| format!("no desktop entry {entry}"))?;
                self.launch_in_vm(entry, true, None).map(Started::Vm).map_err(|e| e.short())
            }
        }
    }

    /// Once a second: advances a restore, and saves the layout when it
    /// changed and settled.
    pub fn session_tick(&mut self) {
        if !self.config.session_restore.enabled {
            return;
        }
        self.restore_tick();
        self.xdg_session_tick();
        if self.session_restore.phase != Phase::Running || !self.running.load(Ordering::SeqCst) {
            return;
        }
        let snapshot = self.capture_snapshot();
        let delay = Duration::from_millis(u64::from(self.config.session_restore.save_delay_ms));
        if let Some(snapshot) = self.session_restore.saver.observe(snapshot, Instant::now(), delay) {
            match write_snapshot(&session_file(), &snapshot) {
                Ok(()) => debug!(windows = snapshot.windows.len(), "Saved the session layout"),
                Err(err) => warn!(%err, "Could not save the session layout"),
            }
        }
    }

    fn restore_tick(&mut self) {
        let Some(restore) = self.session_restore.restore.as_mut() else {
            return;
        };
        let now = Instant::now();
        let claimed: HashSet<usize> = restore.claimed.keys().copied().collect();
        for group in &mut restore.groups {
            if let Some(child) = group.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                group.exited = true;
                group.child = None;
                if !status.success() && group.members.iter().all(|m| !claimed.contains(m)) {
                    warn!(?group.launch, %status, "An app of the saved session exited without showing a window");
                    group.expired = true;
                }
            }
            if let Some(vm) = group.vm
                && !self.vm.contains(vm)
            {
                group.expired = true;
            }
            if !group.expired && now >= group.deadline && group.members.iter().any(|m| !claimed.contains(m)) {
                warn!(?group.launch, "An app of the saved session showed no window in time");
                group.expired = true;
            }
        }
        if restore.is_finished() {
            info!(restored = restore.claimed.len(), "Session restore finished");
            self.session_restore.restore = None;
            self.session_restore.phase = Phase::Running;
        }
    }

    // -- capturing ---------------------------------------------------------

    /// The pid of the process `window` belongs to (Wayland clients only).
    fn window_client_pid(&self, window: &WindowElement) -> Option<u32> {
        let surface = window.wl_surface()?;
        let credentials = surface.client()?.get_credentials(&self.display_handle).ok()?;
        u32::try_from(credentials.pid).ok()
    }

    /// Whether `window` is a dialog of another window; those are left to
    /// their app.
    fn is_transient(window: &WindowElement) -> bool {
        #[allow(irrefutable_let_patterns)]
        if let Some(toplevel) = window.0.toplevel() {
            return toplevel.parent().is_some();
        }
        #[cfg(feature = "xwayland")]
        if let Some(xsurface) = window.0.x11_surface() {
            return xsurface.is_transient_for().is_some();
        }
        false
    }

    /// How `window`'s app is started again, worked out once per app id.
    fn launch_for(&self, window: &WindowElement, app_id: &str, pid: Option<u32>) -> Option<Launch> {
        if let Some(vm) = window.vm_id() {
            let described = self.vm.describe(vm)?;
            // A throw-away VM holds nothing worth coming back to.
            return described.persistent.then_some(Launch::Vm { entry: described.entry_id });
        }
        let info = WindowInfo::get(window);
        if let Some((for_app, launch)) = &*info.launch.borrow()
            && for_app == app_id
        {
            return launch.clone();
        }
        if pid.is_some_and(|pid| is_service_process(pid, parent_pid, comm_of)) {
            *info.launch.borrow_mut() = Some((app_id.to_string(), None));
            return None;
        }
        let entries = launcher::scan_desktop_entries();
        let launch = launcher::entry_for_app_id(&entries, app_id)
            .map(|entry| Launch::Entry { id: entry.id.clone() })
            .or_else(|| {
                let pid = pid.filter(|pid| *pid != std::process::id())?;
                let argv = read_cmdline(pid)?;
                // A store path that is gone by the next boot can't be started.
                let program = &argv[0];
                (!program.contains('/') || Path::new(program).exists()).then_some(Launch::Command { argv })
            });
        *info.launch.borrow_mut() = Some((app_id.to_string(), launch.clone()));
        launch
    }

    /// Where `window` sits right now.
    fn placement_of(&self, window: &WindowElement) -> Option<Placement> {
        let (output, workspace_index) = workspace::window_home(window)?;
        let floating = workspace::is_floating(window).then(|| {
            let size = window.geometry().size;
            let loc = self
                .space
                .element_location(window)
                .or_else(|| workspace::floating_position(window))
                .unwrap_or_default();
            Geometry { x: loc.x, y: loc.y, w: size.w, h: size.h }
        });
        Some(Placement { output: output.name(), workspace: workspace_index, floating })
    }

    /// The layout as it is now.
    pub fn capture_snapshot(&mut self) -> Snapshot {
        let excluded: Vec<String> = self.config.session_restore.exclude_app_ids.clone();
        let mut outputs: Vec<Output> = self.all_outputs().cloned().collect();
        outputs.sort_by_key(|o| o.name());

        let mut snapshot = Snapshot::default();
        let mut keys: Vec<GroupKey> = Vec::new();
        // Window -> its index in `snapshot.windows`.
        let mut indexes: Vec<(WindowElement, usize)> = Vec::new();

        for output in &outputs {
            let (active, count) = workspace::overlay_info(output);
            snapshot.outputs.push(OutputSnap { name: output.name(), active, count });
            for window in workspace::windows_on_output(output) {
                if !window.alive() || Self::is_transient(&window) {
                    continue;
                }
                let (_, app_id) = crate::foreign_toplevel::title_and_app_id(&window.0);
                if app_id.is_empty() || excluded.iter().any(|e| e.eq_ignore_ascii_case(&app_id)) {
                    continue;
                }
                let pid = self.window_client_pid(&window);
                let Some(launch) = self.launch_for(&window, &app_id, pid) else {
                    continue;
                };
                let Some(placement) = self.placement_of(&window) else {
                    continue;
                };
                let key = match (window.vm_id(), pid) {
                    (Some(vm), _) => GroupKey::Vm(vm),
                    (None, Some(pid)) => GroupKey::Pid(pid),
                    (None, None) => GroupKey::Alone(indexes.len()),
                };
                let xdg = window
                    .wl_surface()
                    .and_then(|surface| self.session_restore.xdg.reference_for(&surface));
                indexes.push((window, snapshot.windows.len()));
                keys.push(key);
                snapshot.windows.push(WindowSnap { app_id, launch, group: 0, placement, xdg });
            }
        }
        for (window, group) in snapshot.windows.iter_mut().zip(assign_groups(&keys)) {
            window.group = group;
        }
        for output in &outputs {
            let (_, count) = workspace::overlay_info(output);
            for index in 0..count {
                let tree = tiling::TilingState::tree(output, index);
                let shape = tree.shape(|window| {
                    indexes
                        .iter()
                        .find(|(w, _)| w == window)
                        .map(|(_, slot)| *slot)
                        .filter(|slot| snapshot.windows[*slot].placement.floating.is_none())
                });
                if let Some(shape) = shape {
                    snapshot.trees.push(TreeSnap { output: output.name(), workspace: index, shape });
                }
            }
        }
        snapshot.sessions = self.session_restore.xdg.records();
        snapshot
    }

    // -- claiming ----------------------------------------------------------

    /// A window was just created (its app id isn't known yet): hands it to
    /// the saved window of the app that was started for it, if any.
    pub fn session_window_created(&mut self, window: &WindowElement) {
        self.try_claim(window);
    }

    /// A window's app id became known (or it just mapped, for X11).
    pub fn session_window_identified(&mut self, window: &WindowElement) {
        self.try_claim(window);
    }

    fn try_claim(&mut self, window: &WindowElement) {
        if self.session_restore.restore.is_none() || WindowInfo::get(window).claimed.get().is_some() {
            return;
        }
        let pid = self.window_client_pid(window);
        let (_, app_id) = crate::foreign_toplevel::title_and_app_id(&window.0);
        let xdg = window
            .wl_surface()
            .and_then(|surface| self.session_restore.xdg.reference_for(&surface));
        let query = ClaimQuery { pid, vm: window.vm_id(), app_id: &app_id, xdg: xdg.as_ref() };
        let Some(restore) = self.session_restore.restore.as_mut() else {
            return;
        };
        let Some(slot) = restore.pick(&query, parent_pid) else {
            return;
        };
        restore.claimed.insert(slot, window.clone());
        let placement = restore.snapshot.windows[slot].placement.clone();
        WindowInfo::get(window).claimed.set(Some(slot));
        debug!(app_id, slot, "A window took its place in the restored session");
        self.place_restored(window, &placement);
    }

    /// Applies a saved `placement` to `window`.
    pub(crate) fn place_restored(&mut self, window: &WindowElement, placement: &Placement) {
        if !window.alive() {
            return;
        }
        let Some(output) = self
            .all_outputs()
            .find(|o| o.name() == placement.output)
            .cloned()
            .or_else(|| workspace::home_output(window))
        else {
            return;
        };
        workspace::ensure_workspaces(&output, placement.workspace + 1);
        workspace::move_window_to(self, window, &output, placement.workspace);

        match placement.floating {
            Some(geometry) => self.place_floating(window, &output, placement.workspace, geometry),
            None => {
                if workspace::is_floating(window) && tiling::should_tile(window) {
                    tiling::set_floating(self, window, false);
                }
                self.rebuild_restored_tree(&output, placement.workspace);
            }
        }
    }

    fn place_floating(&mut self, window: &WindowElement, output: &Output, index: usize, geometry: Geometry) {
        if !workspace::is_floating(window) {
            tiling::set_floating(self, window, true);
        }
        let bounds = self.space.output_geometry(output);
        let (x, y) = match bounds {
            Some(bounds) => clamp_into(geometry, bounds, 64),
            None => (geometry.x, geometry.y),
        };
        let position = Point::from((x, y));
        workspace::set_floating_position(window, position);
        #[allow(irrefutable_let_patterns)]
        if let Some(toplevel) = window.0.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some((geometry.w.max(1), geometry.h.max(1)).into()));
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
        if workspace::overlay_info(output).0 == index {
            self.space.map_element(window.clone(), position, false);
        }
    }

    /// Rebuilds the tiling tree of `output`'s workspace `index` from the saved
    /// shape, with the windows that have taken their places so far.
    fn rebuild_restored_tree(&mut self, output: &Output, index: usize) {
        let Some(restore) = self.session_restore.restore.as_ref() else {
            return;
        };
        let Some(shape) = restore
            .snapshot
            .trees
            .iter()
            .find(|t| t.output == output.name() && t.workspace == index)
            .map(|t| t.shape.clone())
        else {
            return;
        };
        let placed: HashMap<usize, WindowElement> = restore
            .claimed
            .iter()
            .filter(|(slot, window)| {
                let placement = &restore.snapshot.windows[**slot].placement;
                window.alive()
                    && placement.floating.is_none()
                    && placement.output == output.name()
                    && placement.workspace == index
                    && tiling::TilingState::tree(output, index).contains(window)
            })
            .map(|(slot, window)| (*slot, window.clone()))
            .collect();
        let area = tiling::tiling_area(&self.space, output, self.config.gaps.outer as i32);
        tiling::TilingState::tree_mut(output, index).restore_shape(&shape, &placed, area);
        if workspace::overlay_info(output).0 == index {
            tiling::apply_layout_animated(self, output);
        }
    }

    // -- xdg-session-management-v1 ----------------------------------------

    /// The window whose toplevel surface is `surface`.
    pub(crate) fn any_window_for_surface(&self, surface: &WlSurface) -> Option<WindowElement> {
        workspace::all_windows(self)
            .into_iter()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
    }

    /// `restore_toplevel`: the client asks for `session`/`name`'s state to be
    /// applied to `window`. Returns whether anything was restored.
    pub(crate) fn xdg_restore_toplevel(&mut self, window: &WindowElement, session: &str, name: &str) -> bool {
        let reference = XdgRef { session: session.to_string(), name: name.to_string() };
        // Part of a restore we are running: the saved layout slot goes with it.
        // The window may already have been given a slot by its process (that
            // happens when it is created, before the client names it): the name
            // is the better evidence, so it takes over.
        if let Some(restore) = self.session_restore.restore.as_mut()
            && let Some(slot) = (0..restore.snapshot.windows.len())
                .filter(|slot| !restore.claimed.contains_key(slot))
                .find(|slot| restore.snapshot.windows[*slot].xdg.as_ref() == Some(&reference))
        {
            if let Some(previous) = WindowInfo::get(window).claimed.get() {
                restore.claimed.remove(&previous);
            }
            restore.claimed.insert(slot, window.clone());
            let placement = restore.snapshot.windows[slot].placement.clone();
            WindowInfo::get(window).claimed.set(Some(slot));
            self.place_restored(window, &placement);
            return true;
        }
        let Some(placement) = self.session_restore.xdg.placement(session, name).cloned() else {
            return false;
        };
        self.place_restored(window, &placement);
        true
    }

    /// Keeps the remembered state of windows in sessions up to date.
    fn xdg_session_tick(&mut self) {
        for (session, name, surface) in self.session_restore.xdg.bound() {
            let Some(window) = self.any_window_for_surface(&surface) else {
                continue;
            };
            if let Some(placement) = self.placement_of(&window) {
                self.session_restore.xdg.set_placement(&session, &name, placement);
            }
        }
    }
}

enum Started {
    Process(Child),
    Vm(VmId),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(workspace: usize) -> Placement {
        Placement { output: "DP-1".into(), workspace, floating: None }
    }

    #[test]
    fn an_unknown_session_id_starts_a_new_session() {
        let mut registry = XdgRegistry::default();
        let (id, known) = registry.open(None);
        assert!(!known);
        assert_eq!(id.len(), 32);
        assert!(registry.contains(&id));
        // Asking for one that was never handed out is the same as asking for none.
        let (other, known) = registry.open(Some("made-up"));
        assert!(!known);
        assert_ne!(other, "made-up");
        assert_ne!(other, id);
        // The real one is found again.
        assert_eq!(registry.open(Some(&id)), (id, true));
    }

    #[test]
    fn sessions_remember_placements_until_removed() {
        let mut registry = XdgRegistry::default();
        let (id, _) = registry.open(None);
        assert_eq!(registry.placement(&id, "main"), None);
        registry.set_placement(&id, "main", placement(2));
        registry.set_placement(&id, "prefs", placement(3));
        assert_eq!(registry.placement(&id, "main"), Some(&placement(2)));

        registry.remove_toplevel(&id, "prefs");
        assert_eq!(registry.placement(&id, "prefs"), None);
        assert_eq!(registry.placement(&id, "main"), Some(&placement(2)));

        // Survives the file: what `records` hands out loads back.
        let mut reloaded = XdgRegistry::default();
        reloaded.load(&registry.records());
        assert_eq!(reloaded.placement(&id, "main"), Some(&placement(2)));

        registry.remove(&id);
        assert!(!registry.contains(&id));
        assert_eq!(registry.placement(&id, "main"), None);
    }

    #[test]
    fn placements_of_an_unknown_session_are_not_invented() {
        let mut registry = XdgRegistry::default();
        registry.set_placement("nope", "main", placement(1));
        assert!(!registry.contains("nope"));
        assert!(registry.records().is_empty());
    }

    #[test]
    fn the_least_recently_used_sessions_are_forgotten_first() {
        let mut registry = XdgRegistry::default();
        // A full registry whose first entry is by far the oldest.
        let mut records = Vec::new();
        for i in 0..MAX_XDG_SESSIONS {
            records.push(XdgSessionRecord {
                id: format!("s{i}"),
                toplevels: BTreeMap::new(),
                last_used: 1_000 + i as u64,
            });
        }
        registry.load(&records);
        let (fresh, _) = registry.open(None);
        assert!(!registry.contains("s0"), "the oldest went");
        assert!(registry.contains("s1"));
        assert!(registry.contains(&fresh), "the new one is never the one evicted");
        assert_eq!(registry.records().len(), MAX_XDG_SESSIONS);
    }

    fn window(app_id: &str, group: usize) -> WindowSnap {
        WindowSnap {
            app_id: app_id.into(),
            launch: Launch::Entry { id: format!("{app_id}.desktop") },
            group,
            placement: Placement { output: "DP-1".into(), workspace: 0, floating: None },
            xdg: None,
        }
    }

    fn group(members: &[usize], pid: Option<u32>) -> Group {
        Group {
            members: members.to_vec(),
            launch: Launch::Entry { id: "x.desktop".into() },
            pid,
            vm: None,
            child: None,
            exited: false,
            deadline: Instant::now() + Duration::from_secs(60),
            expired: false,
        }
    }

    fn restore(windows: Vec<WindowSnap>, groups: Vec<Group>) -> Restore<()> {
        Restore {
            snapshot: Snapshot { windows, ..Snapshot::default() },
            groups,
            claimed: HashMap::new(),
        }
    }

    /// pid -> parent, as a lookup.
    fn tree(pairs: &[(u32, u32)]) -> impl Fn(u32) -> Option<u32> + Copy + '_ {
        move |pid| pairs.iter().find(|(p, _)| *p == pid).map(|(_, parent)| *parent)
    }

    fn query(pid: Option<u32>, app_id: &str) -> ClaimQuery<'_> {
        ClaimQuery { pid, vm: None, app_id, xdg: None }
    }

    #[test]
    fn a_snapshot_survives_the_file() {
        let dir = std::env::temp_dir().join(format!("ironland-session-{}", std::process::id()));
        let path = dir.join("nested/session.json");
        let mut snapshot = Snapshot::default();
        snapshot.outputs.push(OutputSnap { name: "DP-1".into(), active: 2, count: 4 });
        let mut floating = window("mpv", 1);
        floating.placement.floating = Some(Geometry { x: 10, y: 20, w: 640, h: 480 });
        floating.xdg = Some(XdgRef { session: "s".into(), name: "main".into() });
        snapshot.windows = vec![window("alacritty", 0), floating];
        snapshot.trees.push(TreeSnap {
            output: "DP-1".into(),
            workspace: 0,
            shape: TreeShape::Leaf { window: 0 },
        });

        assert_eq!(read_snapshot(&path), Ok(None), "no file yet");
        write_snapshot(&path, &snapshot).unwrap();
        assert_eq!(read_snapshot(&path), Ok(Some(snapshot)));
        assert!(!path.with_extension("json.tmp").exists(), "the temporary file is gone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_of_another_version_is_ignored_and_garbage_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ironland-session-v-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        std::fs::write(&path, r#"{"version":99,"outputs":[],"windows":[],"trees":[]}"#).unwrap();
        assert_eq!(read_snapshot(&path), Ok(None));
        std::fs::write(&path, "not json").unwrap();
        assert!(read_snapshot(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_layout_is_written_only_after_it_stays_the_same() {
        let delay = Duration::from_secs(3);
        let t0 = Instant::now();
        let mut saver = Saver::default();
        let mut one = Snapshot::default();
        one.windows.push(window("a", 0));
        let mut two = one.clone();
        two.windows.push(window("b", 1));

        assert_eq!(saver.observe(one.clone(), t0, delay), None, "just seen");
        assert_eq!(saver.observe(one.clone(), t0 + Duration::from_secs(2), delay), None);
        // It changed again: the wait starts over.
        assert_eq!(saver.observe(two.clone(), t0 + Duration::from_secs(4), delay), None);
        assert_eq!(saver.observe(two.clone(), t0 + Duration::from_secs(6), delay), None);
        assert_eq!(saver.observe(two.clone(), t0 + Duration::from_secs(7), delay), Some(two.clone()));
        // Written once, not again while nothing changes.
        assert_eq!(saver.observe(two, t0 + Duration::from_secs(20), delay), None);
    }

    #[test]
    fn closing_every_window_only_counts_after_a_long_hold() {
        let delay = Duration::from_secs(3);
        let t0 = Instant::now();
        let mut full = Snapshot::default();
        full.windows.push(window("a", 0));
        let mut saver = Saver::with_written(Some(full));
        let empty = Snapshot::default();

        // A logout closes every client at once; that must not erase the layout.
        assert_eq!(saver.observe(empty.clone(), t0, delay), None);
        assert_eq!(saver.observe(empty.clone(), t0 + Duration::from_secs(10), delay), None);
        assert_eq!(saver.observe(empty.clone(), t0 + EMPTY_HOLD, delay), Some(empty));
    }

    #[test]
    fn an_empty_layout_over_an_empty_one_needs_no_hold() {
        let mut saver = Saver::with_written(Some(Snapshot::default()));
        let mut outputs = Snapshot::default();
        outputs.outputs.push(OutputSnap { name: "DP-1".into(), active: 1, count: 2 });
        let t0 = Instant::now();
        assert_eq!(saver.observe(outputs.clone(), t0, Duration::from_secs(3)), None);
        assert_eq!(
            saver.observe(outputs.clone(), t0 + Duration::from_secs(3), Duration::from_secs(3)),
            Some(outputs)
        );
    }

    #[test]
    fn the_parent_pid_is_read_past_a_tricky_command_name() {
        assert_eq!(parse_ppid("123 (bash) S 45 123 123 0 -1"), Some(45));
        assert_eq!(parse_ppid("123 (my (weird) name) S 77 1 1 0"), Some(77));
        assert_eq!(parse_ppid("garbage"), None);
    }

    #[test]
    fn a_process_descends_from_its_ancestors() {
        let parents = [(300, 200), (200, 100), (100, 1)];
        let parent_of = tree(&parents);
        assert!(descends_from(300, 300, parent_of));
        assert!(descends_from(300, 200, parent_of));
        assert!(descends_from(300, 100, parent_of));
        assert!(!descends_from(200, 300, parent_of));
        assert!(!descends_from(300, 999, parent_of));
        // A loop in the data can't hang it.
        let looped = [(5, 6), (6, 5)];
        assert!(!descends_from(5, 9, tree(&looped)));
    }

    #[test]
    fn a_process_started_by_systemd_is_a_service_not_an_app_to_restore() {
        let parents = [(500, 1), (600, 400), (700, 450)];
        let comms = |pid: u32| match pid {
            400 => Some("systemd\n".to_string()),
            450 => Some("quickshell\n".to_string()),
            _ => None,
        };
        // Directly under init, or under the user's systemd: a service.
        assert!(is_service_process(500, tree(&parents), comms));
        assert!(is_service_process(600, tree(&parents), comms));
        // Started by another program (the shell, the compositor): an app.
        assert!(!is_service_process(700, tree(&parents), comms));
        // Unknown process.
        assert!(!is_service_process(999, tree(&parents), comms));
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ironland-session-perm-{}", std::process::id()));
        let path = dir.join("session.json");
        write_snapshot(&path, &Snapshot::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // And rewriting keeps it so.
        write_snapshot(&path, &Snapshot::default()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn command_lines_are_split_at_nul() {
        assert_eq!(parse_cmdline(b"/bin/foo\0--bar\0baz qux\0"), vec!["/bin/foo", "--bar", "baz qux"]);
        assert!(parse_cmdline(b"").is_empty());
    }

    #[test]
    fn windows_of_one_process_form_a_group() {
        assert_eq!(assign_groups(&["a", "b", "a", "c", "b"]), vec![0, 1, 0, 2, 1]);
        assert!(assign_groups::<u32>(&[]).is_empty());
    }

    #[test]
    fn a_window_of_a_started_process_takes_its_group_slot_by_pid() {
        let r = restore(
            vec![window("alacritty", 0), window("alacritty", 1)],
            vec![group(&[0], Some(100)), group(&[1], Some(200))],
        );
        let parents = [(250, 200), (200, 1), (150, 100), (100, 1)];
        // The terminal of the second process, a child of the one started.
        assert_eq!(r.pick(&query(Some(250), ""), tree(&parents)), Some(1));
        assert_eq!(r.pick(&query(Some(150), ""), tree(&parents)), Some(0));
        assert_eq!(r.pick(&query(Some(999), ""), tree(&parents)), None);
    }

    #[test]
    fn a_group_hands_out_its_slots_one_by_one() {
        let mut r = restore(
            vec![window("firefox", 0), window("firefox", 0)],
            vec![group(&[0, 1], Some(100))],
        );
        let none: [(u32, u32); 0] = [];
        let first = r.pick(&query(Some(100), ""), tree(&none)).unwrap();
        assert_eq!(first, 0);
        r.claimed.insert(first, ());
        assert_eq!(r.pick(&query(Some(100), ""), tree(&none)), Some(1));
    }

    #[test]
    fn the_app_id_finds_a_slot_when_the_started_process_handed_over() {
        let mut g = group(&[0], Some(100));
        g.exited = true;
        let r = restore(vec![window("firefox", 0)], vec![g]);
        let none: [(u32, u32); 0] = [];
        // Different pid (the already-running instance), same app id.
        assert_eq!(r.pick(&query(Some(5000), "Firefox"), tree(&none)), Some(0));
        assert_eq!(r.pick(&query(Some(5000), "other"), tree(&none)), None);
        assert_eq!(r.pick(&query(Some(5000), ""), tree(&none)), None, "no app id yet");
    }

    #[test]
    fn a_window_opened_by_hand_does_not_take_a_slot_of_a_running_process() {
        // The started process is alive and will show its own window; a
        // same-app window from elsewhere must not take its place.
        let r = restore(vec![window("alacritty", 0)], vec![group(&[0], Some(100))]);
        let parents = [(7000, 1), (100, 1)];
        assert_eq!(r.pick(&query(Some(7000), "alacritty"), tree(&parents)), None);
    }

    #[test]
    fn x11_windows_without_a_pid_match_by_app_id() {
        let r = restore(vec![window("Steam", 0)], vec![group(&[0], Some(100))]);
        let none: [(u32, u32); 0] = [];
        assert_eq!(r.pick(&query(None, "steam"), tree(&none)), Some(0));
    }

    #[test]
    fn an_xdg_name_beats_everything_else() {
        let reference = XdgRef { session: "s1".into(), name: "second".into() };
        let mut second = window("editor", 0);
        second.xdg = Some(reference.clone());
        let r = restore(vec![window("editor", 0), second], vec![group(&[0, 1], Some(100))]);
        let none: [(u32, u32); 0] = [];
        let q = ClaimQuery { pid: Some(100), vm: None, app_id: "editor", xdg: Some(&reference) };
        assert_eq!(r.pick(&q, tree(&none)), Some(1));
    }

    #[test]
    fn a_vm_window_belongs_to_its_vm_group_only() {
        let mut vm_group = group(&[0], None);
        vm_group.vm = Some(7);
        let r = restore(vec![window("keepassxc", 0)], vec![vm_group]);
        let none: [(u32, u32); 0] = [];
        let q = |vm: Option<VmId>, app_id| ClaimQuery { pid: Some(1234), vm, app_id, xdg: None };
        assert_eq!(r.pick(&q(Some(7), "keepassxc"), tree(&none)), Some(0));
        assert_eq!(r.pick(&q(Some(8), "keepassxc"), tree(&none)), None);
        // A host window with the same app id doesn't take the VM's slot.
        assert_eq!(r.pick(&q(None, "keepassxc"), tree(&none)), None);
    }

    #[test]
    fn given_up_groups_hand_out_nothing_and_a_restore_finishes() {
        let mut expired = group(&[0], Some(100));
        expired.expired = true;
        let mut r = restore(vec![window("a", 0), window("b", 1)], vec![expired, group(&[1], Some(200))]);
        let none: [(u32, u32); 0] = [];
        assert_eq!(r.pick(&query(Some(100), "a"), tree(&none)), None);
        assert!(!r.is_finished());
        r.claimed.insert(1, ());
        assert!(r.is_finished(), "the given-up group no longer holds it up");
    }

    #[test]
    fn a_floating_window_is_kept_on_a_screen_that_changed() {
        let bounds = Rectangle::new((0, 0).into(), (1920, 1080).into());
        let geometry = |x, y| Geometry { x, y, w: 800, h: 600 };
        assert_eq!(clamp_into(geometry(100, 50), bounds, 64), (100, 50));
        // Saved on a bigger monitor.
        assert_eq!(clamp_into(geometry(3000, 2000), bounds, 64), (1856, 1016));
        assert_eq!(clamp_into(geometry(-500, -40), bounds, 64), (0, 0));
        // A monitor not at the origin.
        let right = Rectangle::new((1920, 0).into(), (1280, 720).into());
        assert_eq!(clamp_into(geometry(10, 10), right, 64), (1920, 10));
    }
}
