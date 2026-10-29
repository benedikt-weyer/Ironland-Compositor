//! Programs running inside QEMU virtual machines, shown as ordinary windows.
//!
//! Launching a desktop entry "as virtual" boots a fresh VM for it (one VM per
//! launch, never shared) and runs the program inside the guest through
//! `waypipe ssh`. Waypipe proxies the guest application's Wayland protocol
//! stream, so each guest window becomes a normal `xdg_toplevel` on this
//! compositor: it is tiled/floated/focused like any host window, and input,
//! clipboard, resizing and so on are carried across by the protocol itself
//! rather than by anything VM-specific.
//!
//! What makes a window *virtual* is the socket its client came in on. Every
//! VM gets its own private Wayland listening socket (see
//! [`AnvilState::launch_in_vm`]); `waypipe` connects to that one, and
//! everything accepted there is tagged with the VM's [`VmId`] in
//! [`ClientState::vm`](crate::ClientState::vm). That is sturdier than
//! guessing from pids or app ids, and it is what the corner badge
//! ([`crate::vm_badge`]) keys off via [`WindowElement::vm_id`].
//!
//! Lifecycle of one VM:
//!
//! 1. `qemu-system-x86_64` starts headless on a `-snapshot` of the program's
//!    own image (`vm.programs`: one program per VM, installed in that image;
//!    so guests never modify it and many can share it), with the guest's
//!    port 22 forwarded to a free loopback port.
//! 2. A probe thread waits for the guest's SSH banner on that port and then
//!    reports [`VmEvent::GuestReady`] over a calloop channel.
//! 3. The main thread starts `waypipe ssh ... <command>` against the VM's
//!    private socket.
//! 4. When the application exits (`waypipe` returns), when QEMU dies, or when
//!    no window shows up within `startup_timeout_secs`, the whole VM is torn
//!    down: both processes killed, the socket removed.
//!
//! Two flavours, chosen per launch:
//!
//! * **throw-away** (launcher's VM tab): as above, nothing survives the VM;
//! * **persistent** (launcher's persistent tab): the VM runs on a per-application
//!   disk overlay and, when its program exits cleanly, its disk *and memory*
//!   are saved (QMP `savevm`, see [`super::vm_state`]). The next persistent
//!   launch of that application restores them with `-loadvm`: no boot, and
//!   everything the last session wrote is still there. If the saved state
//!   can't be loaded (different machine settings, QEMU upgrade) the guest
//!   cold-boots from the overlay's disk instead.
//!
//! Processes are spawned from the main thread only (and with a parent-death
//! signal) so they can't outlive the compositor: `PR_SET_PDEATHSIG` fires
//! when the *thread* that spawned the child exits, not the process.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use smithay::{
    reexports::calloop::{
        RegistrationToken,
        channel::{Channel, Sender},
        timer::{TimeoutAction, Timer},
    },
    wayland::socket::ListeningSocketSource,
};
use tracing::{debug, error, info, warn};

use super::vm_state::{self, PersistMeta};
use crate::{
    ClientState,
    config::VmSettings,
    launcher::{self, DesktopEntry},
    state::{AnvilState, Backend, insert_client_with_identity},
    vm_launch::{Finish, Watcher},
    vm_overview::{
        self, HostRoute, Link, PersistedEntry, Phase, ProgramEntry, RunningVm, VmOverviewData,
    },
};

/// Identifies one running VM for as long as it exists. Never reused within a
/// compositor run.
pub type VmId = u64;

/// How often finished/stuck VMs are looked for.
const REAP_INTERVAL: Duration = Duration::from_secs(1);
/// How often the open VM overview is refreshed.
const OVERVIEW_INTERVAL: Duration = Duration::from_secs(1);
/// How often the probe thread knocks on the guest's SSH port.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// Log lines of a failed VM's output included in the compositor log.
const LOG_TAIL_LINES: usize = 12;

/// Messages from the SSH probe thread to the main loop.
#[derive(Debug)]
pub enum VmEvent {
    /// The guest's SSH server answered; time to start `waypipe`.
    GuestReady(VmId),
    /// Saving a persistent VM's state failed; the VM should be dropped.
    SaveFailed(VmId, String),
    /// A helper thread finished switching a VM's internet link (see
    /// [`AnvilState::set_vm_internet`]). `qemu_pid` names the QEMU it talked
    /// to, so a result from one that has since been replaced (a failed
    /// restore falls back to a cold boot) can be told from the current one.
    InternetSet {
        id: VmId,
        qemu_pid: u32,
        online: bool,
        result: Result<(), String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("{0} has no VM set up (add it under `vm.programs`, or `[vm.programs]` in the config)")]
    NotConfigured(String),
    #[error("VM image {0} does not exist")]
    ImageMissing(PathBuf),
    #[error("{0} VMs are already running (`max_instances` under [vm])")]
    TooManyInstances(u32),
    #[error("this entry has no command to run")]
    NoCommand,
    #[error("a persistent VM for {0} is already running")]
    AlreadyRunning(String),
    #[error("preparing the persistent VM state: {0}")]
    State(String),
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
}

impl VmError {
    /// A message short enough for the launcher's one-line footer (about 38
    /// characters at its font size; the full text goes to the log).
    pub fn short(&self) -> String {
        match self {
            VmError::NotConfigured(_) => "No VM set up for this app".into(),
            VmError::ImageMissing(_) => "VM image file not found".into(),
            VmError::TooManyInstances(max) => format!("Too many VMs running (max {max})"),
            VmError::NoCommand => "Nothing to run for this entry".into(),
            VmError::AlreadyRunning(_) => "Persistent VM already running".into(),
            VmError::State(_) => "Could not prepare VM state (see log)".into(),
            VmError::Io { .. } => "Could not start the VM (see log)".into(),
        }
    }

    fn io(context: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| VmError::Io { context, source }
    }
}

/// Why a VM is being shut down, for the log.
#[derive(Debug)]
enum EndReason {
    /// The application exited normally.
    Closed,
    QemuExited(ExitStatus),
    WaypipeFailed(ExitStatus),
    /// No window appeared in time (guest never booted, ssh refused us, or
    /// the program never opened a window).
    TimedOut,
    /// A persistent VM's state was saved and QEMU quit.
    Saved,
    SaveFailed(String),
    /// Saving took longer than [`vm_state::SAVE_TIMEOUT`].
    SaveTimedOut,
    /// QEMU exited right after starting from a saved state: the state could
    /// not be loaded. Not an end: the guest is cold-booted instead.
    RestoreFailed(ExitStatus),
    /// A VM that has to start without internet could not have its link taken
    /// down, so it was never let run.
    NetworkFailed(String),
}

impl std::fmt::Display for EndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EndReason::Closed => write!(f, "the application exited"),
            EndReason::QemuExited(status) => write!(f, "qemu exited ({status})"),
            EndReason::WaypipeFailed(status) => write!(f, "waypipe failed ({status})"),
            EndReason::TimedOut => write!(f, "no window appeared in time"),
            EndReason::Saved => write!(f, "state saved"),
            EndReason::SaveFailed(why) => write!(f, "saving the state failed: {why}"),
            EndReason::SaveTimedOut => write!(f, "saving the state took too long"),
            EndReason::RestoreFailed(status) => write!(f, "restoring the state failed ({status})"),
            EndReason::NetworkFailed(why) => {
                write!(f, "could not cut the internet link before starting: {why}")
            }
        }
    }
}

impl EndReason {
    /// How this ending is reported to protocol clients.
    fn finish(&self) -> Finish {
        match self {
            EndReason::Closed => Finish::Closed,
            EndReason::Saved => Finish::Saved,
            EndReason::TimedOut | EndReason::SaveTimedOut => Finish::TimedOut,
            EndReason::QemuExited(_)
            | EndReason::WaypipeFailed(_)
            | EndReason::SaveFailed(_)
            | EndReason::RestoreFailed(_)
            | EndReason::NetworkFailed(_) => Finish::Failed,
        }
    }

    fn is_failure(&self) -> bool {
        !matches!(self, EndReason::Closed | EndReason::Saved)
    }
}

/// What a persistent VM needs beyond a throw-away one.
#[derive(Debug)]
struct PersistState {
    /// [`vm_state::persist_key`] of the application; at most one running VM
    /// per key (two QEMUs can't share one overlay).
    key: String,
    meta_path: PathBuf,
    meta: PersistMeta,
    /// Monitor socket, for saving the state.
    qmp: PathBuf,
    /// Set once the program exited and the state is being saved: the deadline
    /// for that to finish.
    saving_until: Option<Instant>,
    /// QEMU arguments for a cold boot, for falling back when the saved state
    /// can't be loaded.
    cold_args: Vec<String>,
    /// Whether the running QEMU was started with `-loadvm`.
    loaded_state: bool,
}

#[derive(Debug)]
struct VmInstance {
    /// Display name of the launched application, for logs.
    name: String,
    /// Scratch directory holding this VM's logs.
    dir: PathBuf,
    qemu: Child,
    /// Started once the guest answers on SSH.
    waypipe: Option<Child>,
    ssh_port: u16,
    /// Program and arguments to run in the guest.
    command: Vec<String>,
    /// Name of the private Wayland socket (relative to `XDG_RUNTIME_DIR`).
    socket_name: String,
    socket_token: RegistrationToken,
    /// Tells the probe thread to give up.
    cancel: Arc<AtomicBool>,
    /// Whether any Wayland client has connected through this VM's socket,
    /// i.e. the guest application actually started talking to us.
    client_connected: bool,
    /// Past this, a VM with no connected client is considered stuck.
    deadline: Instant,
    /// Whether the guest has answered on SSH yet.
    ready_seen: bool,
    /// `Some` for persistent VMs.
    persist: Option<PersistState>,
    /// Protocol clients following this VM (see `crate::vm_launch`).
    watchers: Vec<Watcher>,
    started: Instant,
    /// Monitor socket every VM has (a persistent one saves its state through
    /// it, any one switches its internet link through it).
    qmp: PathBuf,
    internet: Internet,
    usage: Usage,
}

/// Whether a VM can reach the internet: the state of its internet NIC's link
/// (see [`qemu_args`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Internet {
    online: bool,
    /// A switch to this state is under way on a helper thread.
    pending: Option<bool>,
    /// Started with the CPUs stopped (`-S`) until the link is down; the guest
    /// has not run yet.
    held: bool,
}

/// CPU use of the VM's QEMU process, from two samples of its CPU time.
#[derive(Debug, Default)]
struct Usage {
    previous: Option<(Instant, u64)>,
    /// Percent of one core (200 = two cores busy).
    cpu_percent: f32,
}

/// All running VMs, plus the plumbing they share.
#[derive(Debug)]
pub struct VmManager {
    next_id: VmId,
    instances: HashMap<VmId, VmInstance>,
    events: Sender<VmEvent>,
    reaper_running: bool,
}

impl VmManager {
    /// Creates the manager and the channel the main loop must listen on for
    /// its [`VmEvent`]s.
    pub fn new() -> (Self, Channel<VmEvent>) {
        let (events, channel) = smithay::reexports::calloop::channel::channel();
        (
            Self {
                next_id: 1,
                instances: HashMap::new(),
                events,
                reaper_running: false,
            },
            channel,
        )
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Whether `id` names a VM that is still running.
    pub fn contains(&self, id: VmId) -> bool {
        self.instances.contains_key(&id)
    }

    /// Whether `id` is a persistent VM.
    fn is_persistent(&self, id: VmId) -> bool {
        self.instances
            .get(&id)
            .is_some_and(|vm| vm.persist.is_some())
    }

    /// Records that a Wayland client connected through `id`'s socket.
    fn mark_client_connected(&mut self, id: VmId) {
        if let Some(vm) = self.instances.get_mut(&id)
            && !std::mem::replace(&mut vm.client_connected, true)
        {
            vm.watchers.iter().for_each(Watcher::running);
        }
    }

    /// Lets `watcher` follow the VM `id`'s progress.
    pub fn add_watcher(&mut self, id: VmId, watcher: Watcher) {
        if let Some(vm) = self.instances.get_mut(&id) {
            vm.watchers.push(watcher);
        }
    }

    /// Looks for VMs that are over, returning each with why. Does not remove
    /// them; [`AnvilState::end_vm`] does that.
    fn find_finished(&mut self, now: Instant) -> Vec<(VmId, EndReason)> {
        let mut finished = Vec::new();
        for (&id, vm) in &mut self.instances {
            // Saving state: the only thing to wait for is QEMU quitting, which
            // it does by itself right after a successful save (so exiting
            // cleanly *is* the success signal).
            if let Some(until) = vm.persist.as_ref().and_then(|p| p.saving_until) {
                match vm.qemu.try_wait() {
                    Ok(Some(status)) if status.success() => finished.push((id, EndReason::Saved)),
                    Ok(Some(status)) => finished.push((id, EndReason::QemuExited(status))),
                    _ if now >= until => finished.push((id, EndReason::SaveTimedOut)),
                    _ => {}
                }
                continue;
            }
            if let Ok(Some(status)) = vm.qemu.try_wait() {
                let restore_failed =
                    !vm.ready_seen && vm.persist.as_ref().is_some_and(|p| p.loaded_state);
                finished.push((
                    id,
                    if restore_failed {
                        EndReason::RestoreFailed(status)
                    } else {
                        EndReason::QemuExited(status)
                    },
                ));
                continue;
            }
            if let Some(waypipe) = vm.waypipe.as_mut()
                && let Ok(Some(status)) = waypipe.try_wait()
            {
                let reason = if status.success() {
                    EndReason::Closed
                } else {
                    EndReason::WaypipeFailed(status)
                };
                finished.push((id, reason));
                continue;
            }
            // `waypipe` neither exits nor connects if its ssh failed (it
            // keeps waiting for the guest side), so a deadline is the only
            // way to notice.
            if !vm.client_connected && now >= vm.deadline {
                finished.push((id, EndReason::TimedOut));
            }
        }
        finished
    }
}

impl VmManager {
    /// The running VMs as the VM overview shows them, oldest first, with
    /// their resource use sampled now.
    fn running(&mut self, now: Instant, memory_mb: u32) -> Vec<RunningVm> {
        let mut running: Vec<RunningVm> = self
            .instances
            .iter_mut()
            .map(|(&id, vm)| {
                vm.sample_cpu(now);
                RunningVm {
                    id,
                    name: vm.name.clone(),
                    persistent: vm.persist.is_some(),
                    phase: if vm.persist.as_ref().is_some_and(|p| p.saving_until.is_some()) {
                        Phase::Saving
                    } else if vm.client_connected {
                        Phase::Running
                    } else {
                        Phase::Booting
                    },
                    uptime: now.saturating_duration_since(vm.started),
                    cpu_percent: vm.usage.cpu_percent,
                    rss_bytes: process_rss_bytes(vm.qemu.id()),
                    memory_mb,
                    link: match vm.internet.pending {
                        Some(online) => Link::Switching { online },
                        None if vm.internet.online => Link::Online,
                        None => Link::Offline,
                    },
                    ssh_port: vm.ssh_port,
                }
            })
            .collect();
        running.sort_by_key(|vm| vm.id);
        running
    }

    /// Persistent keys with a VM running.
    fn running_persist_keys(&self) -> Vec<&str> {
        self.instances
            .values()
            .filter_map(|vm| vm.persist.as_ref().map(|p| p.key.as_str()))
            .collect()
    }
}

impl Drop for VmManager {
    fn drop(&mut self) {
        for (_, vm) in self.instances.drain() {
            vm.kill();
        }
    }
}

/// CPU time (user + system, in clock ticks) from the text of `/proc/<pid>/stat`.
/// The command name in it may hold spaces and parentheses, so the fields are
/// counted from the last `)`.
fn parse_cpu_ticks(stat: &str) -> Option<u64> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    let mut fields = after_name.split_whitespace();
    // `state` is the first field after the name; `utime` and `stime` are the
    // 12th and 13th.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

/// Resident set size in pages, from the text of `/proc/<pid>/statm`.
fn parse_rss_pages(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

fn process_cpu_ticks(pid: u32) -> Option<u64> {
    parse_cpu_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn process_rss_bytes(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|statm| parse_rss_pages(&statm))
        .map_or(0, |pages| pages * rustix::param::page_size() as u64)
}

impl VmInstance {
    /// Updates [`Usage::cpu_percent`] from the CPU time QEMU used since the
    /// previous sample.
    fn sample_cpu(&mut self, now: Instant) {
        let Some(ticks) = process_cpu_ticks(self.qemu.id()) else {
            return;
        };
        if let Some((at, before)) = self.usage.previous {
            let secs = now.saturating_duration_since(at).as_secs_f32();
            if secs > 0.2 {
                let used = ticks.saturating_sub(before) as f32
                    / rustix::param::clock_ticks_per_second() as f32;
                self.usage.cpu_percent = used / secs * 100.0;
                self.usage.previous = Some((now, ticks));
            }
        } else {
            self.usage.previous = Some((now, ticks));
        }
    }

    /// Stops everything belonging to this VM. Safe to call on one whose
    /// processes already exited.
    fn kill(mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(mut waypipe) = self.waypipe.take() {
            // `waypipe` spawns the ssh client; kill the whole group.
            if let Some(pid) = rustix::process::Pid::from_raw(waypipe.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = waypipe.kill();
            let _ = waypipe.wait();
        }
        let _ = self.qemu.kill();
        let _ = self.qemu.wait();
    }
}

// ---------------------------------------------------------------------------
// Command line construction. Pure functions, so they're unit-testable.
// ---------------------------------------------------------------------------

/// The QEMU binary run for every VM.
const QEMU_BINARY: &str = "qemu-system-x86_64";

/// Expands a leading `~/` to `$HOME`, so config paths can be written the way
/// users expect.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// The guest-side name of the credential carrying the authorized keys; the
/// guest's sshd reads `/run/credentials/@system/<name>` (see `nix/vm-guest.nix`).
const KEYS_CREDENTIAL: &str = "ironland.authorized_keys";

/// The lines of `text` that look like public keys (`ssh-ed25519 AAAA...`),
/// which skips comments, blank lines and the ssh-agent's "no identities".
fn parse_public_keys(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| {
            let kind = line.split_whitespace().next().unwrap_or("");
            kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-")
        })
        .map(str::to_string)
        .collect()
}

/// Public keys currently held by the ssh-agent (`$SSH_AUTH_SOCK`), empty if
/// there is none. Bounded: a stuck agent must not stall the compositor.
fn agent_public_keys() -> Vec<String> {
    if std::env::var_os("SSH_AUTH_SOCK").is_none() {
        return Vec::new();
    }
    let Ok(mut child) = Command::new("ssh-add")
        .arg("-L")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut text);
    }
    parse_public_keys(&text)
}

/// Public keys the guest should accept for `settings.user`, in order of
/// preference: the explicit `public_key`/`public_key_file`; else the
/// `<ssh_key>.pub` next to the configured private key; else
/// `~/.ssh/id_ed25519.pub` plus every key the ssh-agent holds (`agent` is only
/// called then). Empty means the guest has to have a key baked in.
pub fn authorized_keys(settings: &VmSettings, agent: impl FnOnce() -> Vec<String>) -> Vec<String> {
    let set = |value: &Option<String>| value.clone().filter(|v| !v.trim().is_empty());
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => parse_public_keys(&text),
        Err(err) => {
            warn!(path = %path.display(), %err, "Could not read the VM public key file");
            Vec::new()
        }
    };

    let mut keys = Vec::new();
    if let Some(key) = set(&settings.public_key) {
        keys.extend(parse_public_keys(&key));
    }
    if let Some(file) = set(&settings.public_key_file) {
        keys.extend(read(&expand_tilde(&file)));
    }
    if keys.is_empty()
        && let Some(private) = set(&settings.ssh_key)
    {
        let mut path = expand_tilde(&private).into_os_string();
        path.push(".pub");
        keys.extend(read(Path::new(&path)));
    }
    if keys.is_empty() {
        let default = expand_tilde("~/.ssh/id_ed25519.pub");
        if let Ok(text) = std::fs::read_to_string(default) {
            keys.extend(parse_public_keys(&text));
        }
        keys.extend(agent());
    }
    let mut seen = std::collections::HashSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

/// Standard base64 (with padding).
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// QEMU arguments delivering `keys` to the guest as a systemd credential in
/// an SMBIOS OEM string (the guest's PID 1 imports those into
/// `/run/credentials/@system`). Base64, so newlines and commas survive
/// QEMU's option parsing. Empty for no keys.
pub fn authorized_keys_args(keys: &[String]) -> Vec<String> {
    if keys.is_empty() {
        return Vec::new();
    }
    let value = base64(format!("{}\n", keys.join("\n")).as_bytes());
    vec![
        "-smbios".into(),
        format!("type=11,value=io.systemd.credential.binary:{KEYS_CREDENTIAL}={value}"),
    ]
}

/// What a VM boots from.
pub enum DiskMode<'a> {
    /// A throw-away snapshot of the base image.
    Snapshot {
        image: &'a Path,
        /// Monitor socket QEMU listens on, for switching the internet link.
        qmp: &'a Path,
    },
    /// A persistent per-application overlay (see [`vm_state`]).
    Persistent {
        overlay: &'a Path,
        /// Monitor socket QEMU listens on, for saving the state and
        /// switching the internet link.
        qmp: &'a Path,
        /// Guest-agent socket QEMU listens on, for setting the clock.
        qga: &'a Path,
        /// Restore the saved state instead of booting.
        load_state: bool,
    },
}

/// Subnet of the guest's ssh-only NIC (see [`qemu_args`]); the internet NIC
/// keeps QEMU's default 10.0.2.0/24.
const MGMT_NET: &str = "10.0.3.0/24";
/// The guest's address on the internet NIC and the gateway (QEMU's NAT, which
/// reaches the internet through the host) it talks to.
pub const GUEST_INTERNET_ADDR: &str = "10.0.2.15";
pub const GUEST_INTERNET_GATEWAY: &str = "10.0.2.2";

/// Arguments for `qemu-system-x86_64`: a headless guest with guest port 22
/// reachable on `127.0.0.1:ssh_port` and its serial console written to
/// `serial_log`.
///
/// The machine definition is the same for both [`DiskMode`]s apart from the
/// extra devices persistent VMs carry; and for one application it must stay the
/// same from save to restore (a saved state can only be loaded into an
/// identical machine - see [`vm_state::machine_fingerprint`]).
pub fn qemu_args(
    settings: &VmSettings,
    id: VmId,
    disk: DiskMode<'_>,
    ssh_port: u16,
    serial_log: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|s| s.to_string()));

    push(&["-name", &format!("ironland-vm-{id}")]);
    if settings.kvm {
        push(&["-enable-kvm", "-cpu", "host"]);
    } else {
        push(&["-cpu", "max"]);
    }
    push(&["-m", &settings.memory_mb.to_string()]);
    push(&["-smp", &settings.cpus.max(1).to_string()]);
    match &disk {
        // `snapshot=on` keeps every guest write in a temporary overlay that
        // is thrown away on exit: the image stays pristine and any number of
        // VMs can boot from it at once.
        DiskMode::Snapshot { image, .. } => push(&[
            "-drive",
            &format!("file={},if=virtio,snapshot=on", image.display()),
        ]),
        DiskMode::Persistent {
            overlay,
            qga,
            load_state,
            ..
        } => {
            push(&[
                "-drive",
                &format!("file={},if=virtio,format=qcow2", overlay.display()),
            ]);
            push(&[
                "-chardev",
                &format!("socket,id=qga,path={},server=on,wait=off", qga.display()),
                "-device",
                "virtio-serial-pci",
                "-device",
                "virtserialport,chardev=qga,name=org.qemu.guest_agent.0",
            ]);
            if *load_state {
                push(&["-loadvm", vm_state::STATE_TAG]);
            }
        }
    }
    let (DiskMode::Snapshot { qmp, .. } | DiskMode::Persistent { qmp, .. }) = &disk;
    push(&[
        "-qmp",
        &format!("unix:{},server=on,wait=off", qmp.display()),
    ]);
    // Two NICs, so the internet can be cut without cutting the compositor off
    // from the guest: taking a NIC's link down (QMP `set_link`) stops all
    // traffic through it, including the ssh session `waypipe` runs over.
    //
    // * `inet` (first, so the guest prefers it for its default route): NAT
    //   onto the host's network. This is the one the VM overview switches.
    // * `mgmt`: `restrict=on` lets the guest reach nothing outside; only the
    //   forwarded ssh port works. A separate subnet keeps the two apart.
    push(&[
        "-netdev",
        &format!("user,id={}", vm_state::INTERNET_NETDEV),
        "-device",
        &format!(
            "virtio-net-pci,netdev={},id=nic_inet",
            vm_state::INTERNET_NETDEV
        ),
    ]);
    push(&[
        "-netdev",
        &format!(
            "user,id=mgmt,net={MGMT_NET},restrict=on,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"
        ),
        "-device",
        "virtio-net-pci,netdev=mgmt,id=nic_mgmt",
    ]);
    push(&["-device", "virtio-rng-pci"]);
    push(&["-display", "none"]);
    push(&["-serial", &format!("file:{}", serial_log.display())]);
    push(&["-no-reboot"]);

    args.extend(settings.extra_qemu_args.iter().cloned());
    args
}

/// Quotes `s` for a POSIX shell: wrapped in single quotes, with embedded
/// single quotes spelled `'\''`.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '+' | ',')
        })
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Environment forcing Wayland toolkits to use the forwarded display rather
/// than probing for X11, mirroring what host-launched apps get (see
/// `compositor_envs` in the keyboard handler).
const GUEST_ENV: [(&str, &str); 6] = [
    ("GDK_BACKEND", "wayland"),
    ("QT_QPA_PLATFORM", "wayland"),
    ("SDL_VIDEODRIVER", "wayland"),
    ("CLUTTER_BACKEND", "wayland"),
    ("MOZ_ENABLE_WAYLAND", "1"),
    ("NIXOS_OZONE_WL", "1"),
];

/// The single shell-command string handed to `ssh`/`waypipe server`. `ssh`
/// joins its trailing arguments with spaces and lets the guest's shell
/// re-parse them, so the words have to be quoted into one string up front.
pub fn remote_command(command: &[String]) -> String {
    let mut words = vec!["env".to_string()];
    words.extend(GUEST_ENV.iter().map(|(k, v)| format!("{k}={v}")));
    words.extend(command.iter().map(|word| shell_quote(word)));
    words.join(" ")
}

/// Arguments for `waypipe`, which connects to the guest on
/// `127.0.0.1:ssh_port` and runs `command` there. The host display it
/// forwards to is taken from `WAYLAND_DISPLAY` in its environment.
pub fn waypipe_args(settings: &VmSettings, ssh_port: u16, command: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        // One application per VM; `waypipe` exits when it disconnects.
        "--oneshot",
        // The guest has no GPU, and the "wire" is a local loopback.
        "--no-gpu",
        "--compress",
        "none",
        "ssh",
        // The guest is a throw-away VM on a loopback-only forward, so there
        // is no host key worth pinning (it changes on every boot).
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    if let Some(key) = settings.ssh_key.as_deref().filter(|k| !k.trim().is_empty()) {
        args.push("-i".into());
        args.push(expand_tilde(key).display().to_string());
        args.push("-o".into());
        args.push("IdentitiesOnly=yes".into());
    }
    args.push("-p".into());
    args.push(ssh_port.to_string());
    args.push(format!("{}@127.0.0.1", settings.user));
    args.push(remote_command(command));
    args
}

// ---------------------------------------------------------------------------
// Process and filesystem plumbing.
// ---------------------------------------------------------------------------

/// Directory VM scratch dirs live under: `$XDG_RUNTIME_DIR/ironland-vm`.
fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ironland-vm")
}

/// Picks a currently-free loopback TCP port. Racy by nature (the port is
/// released before QEMU binds it), but a collision only makes QEMU fail to
/// start, which is reported like any other failed launch.
fn free_loopback_port() -> io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Makes `cmd`'s child die with SIGKILL if the compositor does, so a crash
/// can't leave VMs running.
fn die_with_parent(cmd: &mut Command) {
    // Safety: runs between fork and exec; `prctl(PR_SET_PDEATHSIG)` is
    // async-signal-safe and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(io::Error::from)
        });
    }
}

/// Opens `path` as a child's stdout+stderr sink.
fn log_stdio(path: &Path) -> Result<(Stdio, Stdio), VmError> {
    let out = File::create(path).map_err(VmError::io("creating a VM log file"))?;
    let err = out
        .try_clone()
        .map_err(VmError::io("creating a VM log file"))?;
    Ok((Stdio::from(out), Stdio::from(err)))
}

/// Whether something speaking SSH answers on `127.0.0.1:port`. QEMU's port
/// forward accepts connections before the guest is up and then drops them,
/// so merely connecting proves nothing: wait for the banner.
fn ssh_ready(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut banner = [0u8; 4];
    stream.read_exact(&mut banner).is_ok() && &banner == b"SSH-"
}

/// Polls for the guest's SSH server until it answers, `cancel` is set, or
/// `deadline` passes.
///
/// `sync_clock` is the guest-agent socket to set the guest's clock through
/// once it answers (for a VM restored from saved state, whose clock is stale);
/// failing to is only logged, since an image without the agent still works.
fn spawn_probe(
    id: VmId,
    port: u16,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    events: Sender<VmEvent>,
    sync_clock: Option<PathBuf>,
) {
    let spawned = std::thread::Builder::new()
        .name(format!("vm-probe-{id}"))
        .spawn(move || {
            while !cancel.load(Ordering::SeqCst) && Instant::now() < deadline {
                if ssh_ready(port) {
                    if let Some(qga) = sync_clock
                        && let Err(err) = vm_state::guest_set_time(&qga)
                    {
                        warn!(id, %err, "Could not set the restored guest's clock (is qemu-guest-agent running in it?)");
                    }
                    let _ = events.send(VmEvent::GuestReady(id));
                    return;
                }
                std::thread::sleep(PROBE_INTERVAL);
            }
        });
    if let Err(err) = spawned {
        // Not fatal to the compositor; the VM just times out.
        error!(id, %err, "Failed to start the VM readiness probe");
    }
}

/// Switches the internet link of the QEMU `qemu_pid` listening on `qmp` on a
/// helper thread (the monitor can take a moment to answer) and reports the
/// outcome as [`VmEvent::InternetSet`]. With `resume`, also lets a QEMU that
/// was started paused (`-S`) run.
fn spawn_set_internet(
    id: VmId,
    qemu_pid: u32,
    qmp: &Path,
    online: bool,
    resume: bool,
    events: Sender<VmEvent>,
) {
    let qmp = qmp.to_path_buf();
    let reply = events.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("vm-net-{id}"))
        .spawn(move || {
            let result = vm_state::set_internet_over_socket(&qmp, online, resume);
            let _ = reply.send(VmEvent::InternetSet {
                id,
                qemu_pid,
                online,
                result,
            });
        });
    if let Err(err) = spawned {
        let _ = events.send(VmEvent::InternetSet {
            id,
            qemu_pid,
            online,
            result: Err(err.to_string()),
        });
    }
}

/// The last few lines of `path`, for putting a failed VM's output in the log.
fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
}

/// Starts QEMU with `args`, logging its output to `qemu.log` in `dir`.
fn spawn_qemu(args: &[String], dir: &Path) -> Result<Child, VmError> {
    let (stdout, stderr) = log_stdio(&dir.join("qemu.log"))?;
    let mut cmd = Command::new(QEMU_BINARY);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    die_with_parent(&mut cmd);
    cmd.spawn()
        .map_err(VmError::io("starting qemu-system-x86_64"))
}

// ---------------------------------------------------------------------------
// Compositor glue.
// ---------------------------------------------------------------------------

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    /// Boots a new VM and runs `entry` inside it, returning once QEMU has been
    /// *started* - the window appears later, when the guest is up and the
    /// program has opened it.
    ///
    /// With `persistent`, the VM runs on the application's own overlay and
    /// restores the state saved when it last exited cleanly (if any); see the
    /// module doc.
    pub fn launch_in_vm(
        &mut self,
        entry: &DesktopEntry,
        persistent: bool,
    ) -> Result<VmId, VmError> {
        let mut settings = self.config.vm.clone();
        // One program per VM: the program's own image, with only it installed.
        let image = match settings.image_for(&entry.id) {
            Some(image) => expand_tilde(&image),
            None => return Err(VmError::NotConfigured(entry.name.clone())),
        };
        if !image.exists() {
            return Err(VmError::ImageMissing(image));
        }
        if self.vm.len() >= settings.max_instances.max(1) as usize {
            return Err(VmError::TooManyInstances(settings.max_instances.max(1)));
        }
        // The guest's login key travels with the machine definition (an extra
        // QEMU argument), so a persistent VM saved under another key is
        // cold-booted - and picks up the new key - instead of restored.
        let keys = authorized_keys(&settings, agent_public_keys);
        if keys.is_empty() {
            warn!(
                "No public key found for the VM guest (no ssh-agent keys, no ~/.ssh/id_ed25519.pub); login relies on a key baked into the image"
            );
        }
        settings.extra_qemu_args.extend(authorized_keys_args(&keys));
        let terminal = settings
            .terminal
            .as_deref()
            .unwrap_or(&self.config.terminal);
        let command = launcher::guest_command(entry, terminal).ok_or(VmError::NoCommand)?;

        // Two QEMUs can't share an overlay, so one persistent VM per app.
        let key = vm_state::persist_key(&entry.id);
        if persistent
            && self
                .vm
                .instances
                .values()
                .any(|vm| vm.persist.as_ref().is_some_and(|p| p.key == key))
        {
            return Err(VmError::AlreadyRunning(entry.name.clone()));
        }
        let prepared = if persistent {
            let dir = vm_state::state_root(&settings).join(&key);
            Some(vm_state::prepare(&dir, &image, &settings).map_err(VmError::State)?)
        } else {
            None
        };

        let id = self.vm.next_id;
        self.vm.next_id += 1;

        let dir = runtime_root().join(format!("{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(VmError::io("creating the VM scratch directory"))?;
        let ssh_port =
            free_loopback_port().map_err(VmError::io("finding a free port for the guest's ssh"))?;

        // The VM's private Wayland socket: anything that connects here is
        // this VM's application.
        let socket_name = format!("ironland-vm-{}-{id}", std::process::id());
        let source = ListeningSocketSource::with_name(&socket_name).map_err(|err| VmError::Io {
            context: "creating the VM's wayland socket",
            source: io::Error::other(err),
        })?;
        let socket_token = self
            .handle
            .insert_source(source, move |stream, _, data| {
                data.vm.mark_client_connected(id);
                let client_state = ClientState {
                    vm: Some(id),
                    ..ClientState::default()
                };
                insert_client_with_identity(&mut data.display_handle, stream, client_state);
            })
            .map_err(|err| VmError::Io {
                context: "registering the VM's wayland socket",
                source: io::Error::other(err.to_string()),
            })?;

        let serial_log = dir.join("serial.log");
        let (qmp, qga) = (dir.join("qmp.sock"), dir.join("qga.sock"));
        let args_for = |load_state: bool| match &prepared {
            Some(p) => qemu_args(
                &settings,
                id,
                DiskMode::Persistent {
                    overlay: &p.overlay,
                    qmp: &qmp,
                    qga: &qga,
                    load_state,
                },
                ssh_port,
                &serial_log,
            ),
            None => qemu_args(
                &settings,
                id,
                DiskMode::Snapshot {
                    image: &image,
                    qmp: &qmp,
                },
                ssh_port,
                &serial_log,
            ),
        };
        let load_state = prepared.as_ref().is_some_and(|p| p.load_state);
        let (mut args, mut cold_args) = (args_for(load_state), args_for(false));
        // A VM whose internet was switched off starts with its CPUs stopped,
        // so the link can be taken down before the guest runs a single
        // instruction (see `start_offline`).
        let online = prepared.as_ref().is_none_or(|p| p.meta.network_enabled);
        if !online {
            args.push("-S".into());
            cold_args.push("-S".into());
        }
        let qemu = match spawn_qemu(&args, &dir) {
            Ok(child) => child,
            Err(err) => {
                self.handle.remove(socket_token);
                let _ = std::fs::remove_dir_all(&dir);
                return Err(err);
            }
        };

        let deadline =
            Instant::now() + Duration::from_secs(settings.startup_timeout_secs.max(1) as u64);
        let cancel = Arc::new(AtomicBool::new(false));
        spawn_probe(
            id,
            ssh_port,
            cancel.clone(),
            deadline,
            self.vm.events.clone(),
            load_state.then(|| qga.clone()),
        );

        info!(
            id,
            app = entry.name,
            ssh_port,
            ?image,
            persistent,
            restoring = load_state,
            online,
            "Started VM"
        );
        if !online {
            spawn_set_internet(id, qemu.id(), &qmp, false, true, self.vm.events.clone());
        }
        let persist = prepared.map(|p| PersistState {
            key,
            meta_path: p.meta_path,
            meta: p.meta,
            qmp: qmp.clone(),
            saving_until: None,
            cold_args,
            loaded_state: load_state,
        });
        self.vm.instances.insert(
            id,
            VmInstance {
                name: entry.name.clone(),
                dir,
                qemu,
                waypipe: None,
                ssh_port,
                command,
                socket_name,
                socket_token,
                cancel,
                client_connected: false,
                deadline,
                ready_seen: false,
                persist,
                watchers: Vec::new(),
                started: Instant::now(),
                qmp,
                internet: Internet {
                    online,
                    pending: (!online).then_some(false),
                    held: !online,
                },
                usage: Usage::default(),
            },
        );
        self.ensure_vm_reaper();
        Ok(id)
    }

    /// Launcher action: runs the highlighted entry in a new VM. Closes the
    /// launcher on success; on failure it stays open with the reason in its
    /// footer, so the user can see what's wrong without digging through logs.
    pub fn launch_selected_in_vm(&mut self, persistent: bool) {
        let Some(entry) = self.launcher.selected_entry() else {
            return;
        };
        match self.launch_in_vm(&entry, persistent) {
            Ok(_) => self.launcher.close(),
            Err(err) => {
                warn!(app = entry.name, %err, "Could not launch in a VM");
                self.launcher.set_status(err.short());
            }
        }
    }

    /// Opens the VM overview (see [`crate::vm_overview`]), refreshed every
    /// second for as long as it is open.
    pub fn open_vm_overview(&mut self) {
        self.launcher.close();
        let names = self
            .config
            .vm
            .all_programs()
            .into_keys()
            .map(|id| {
                let name = launcher::find_entry(&id).map_or_else(|| id.clone(), |e| e.name);
                (id, name)
            })
            .collect();
        self.vm_overview.open(names);
        self.refresh_vm_overview();
        if std::mem::replace(&mut self.vm_overview.timer_running, true) {
            return;
        }
        let inserted = self
            .handle
            .insert_source(Timer::from_duration(OVERVIEW_INTERVAL), |_, _, data| {
                if data.vm_overview.is_visible() {
                    data.refresh_vm_overview();
                    TimeoutAction::ToDuration(OVERVIEW_INTERVAL)
                } else {
                    data.vm_overview.timer_running = false;
                    TimeoutAction::Drop
                }
            });
        if let Err(err) = inserted {
            self.vm_overview.timer_running = false;
            error!(%err, "Failed to start the VM overview refresh timer");
        }
    }

    pub fn toggle_vm_overview(&mut self) {
        if self.vm_overview.is_visible() {
            self.vm_overview.close();
        } else {
            self.open_vm_overview();
        }
    }

    /// Gathers what the VM overview shows: running VMs with their resource
    /// use, the state persistent VMs keep on disk, and the programs that can
    /// be started in a VM.
    pub fn refresh_vm_overview(&mut self) {
        if !self.vm_overview.is_visible() {
            return;
        }
        let settings = &self.config.vm;
        let running = self.vm.running(Instant::now(), settings.memory_mb);
        let programs = settings.all_programs();
        let names = self.vm_overview.names();
        let name_of = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());

        // The state directory of a program is named after its desktop entry
        // id, with or without the `.desktop` suffix it came with.
        let app_of_key = |key: &str| {
            let key = key.split(".stale-").next().unwrap_or(key);
            programs
                .keys()
                .find(|id| {
                    vm_state::persist_key(id) == key
                        || vm_state::persist_key(&format!("{id}.desktop")) == key
                })
                .map_or_else(|| key.to_string(), |id| name_of(id))
        };
        let live_keys = self.vm.running_persist_keys();
        let persisted = vm_state::list_persisted(settings)
            .into_iter()
            .map(|p| PersistedEntry {
                app: app_of_key(&p.key),
                disk_bytes: p.disk_bytes,
                has_state: p.meta.as_ref().is_some_and(|m| m.has_state),
                internet: p.meta.as_ref().map(|m| m.network_enabled),
                stale: p.is_stale(),
                running: live_keys.contains(&p.key.as_str()),
            })
            .collect();
        let programs = programs
            .iter()
            .map(|(id, image)| ProgramEntry {
                name: name_of(id),
                image_bytes: std::fs::metadata(expand_tilde(image)).map_or(0, |m| m.len()),
                running: running.iter().filter(|vm| vm.name == name_of(id)).count(),
            })
            .collect();
        let route = std::fs::read_to_string("/proc/net/route")
            .map(|text| vm_overview::parse_default_route(&text))
            .unwrap_or_else(|_| HostRoute::default());

        let data = VmOverviewData {
            running,
            persisted,
            programs,
            route,
            max_instances: settings.max_instances,
        };
        self.vm_overview.set_data(data);
    }

    /// VM overview action: switches the selected VM's internet on or off.
    pub fn vm_overview_toggle_internet(&mut self) {
        let Some(vm) = self.vm_overview.selected() else {
            return;
        };
        let (id, name) = (vm.id, vm.name.clone());
        let online = match vm.link {
            Link::Online => false,
            Link::Offline => true,
            Link::Switching { .. } => {
                self.vm_overview.set_status("The network is still switching");
                return;
            }
        };
        match self.set_vm_internet(id, online) {
            Ok(()) => self.vm_overview.clear_status(),
            Err(why) => {
                warn!(id, app = name, %why, "Could not switch the VM's internet");
                self.vm_overview.set_status(why);
            }
        }
        self.refresh_vm_overview();
    }

    /// Starts `waypipe` for a VM whose guest just answered on SSH.
    pub fn vm_guest_ready(&mut self, id: VmId) {
        let settings = self.config.vm.clone();
        let Some(vm) = self.vm.instances.get_mut(&id) else {
            return;
        };
        vm.ready_seen = true;
        if vm.waypipe.is_some() {
            return;
        }
        debug!(id, "Guest is up, starting waypipe");

        let args = waypipe_args(&settings, vm.ssh_port, &vm.command);
        let spawned = log_stdio(&vm.dir.join("waypipe.log")).and_then(|(stdout, stderr)| {
            let mut cmd = Command::new("waypipe");
            cmd.args(&args)
                .env("WAYLAND_DISPLAY", &vm.socket_name)
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr)
                // Own process group, so teardown also reaches the ssh client
                // waypipe starts.
                .process_group(0);
            die_with_parent(&mut cmd);
            cmd.spawn().map_err(VmError::io("starting waypipe"))
        });
        match spawned {
            Ok(child) => vm.waypipe = Some(child),
            Err(err) => {
                error!(id, %err, "Failed to start waypipe for VM");
                self.end_vm(id, EndReason::TimedOut);
            }
        }
    }

    /// A persistent VM's state could not be saved: drop the VM. (The previous
    /// saved state, if any, is untouched, so the next launch still restores it.)
    pub fn vm_save_failed(&mut self, id: VmId, why: String) {
        self.end_vm(id, EndReason::SaveFailed(why));
    }

    /// Switches the internet of the running VM `id` on or off, which is what
    /// the VM overview's toggle does. The link changes on a helper thread; the
    /// outcome arrives as [`VmEvent::InternetSet`]. For a persistent VM the
    /// new state is also written to its metadata then, so it holds the next
    /// time the VM starts.
    pub fn set_vm_internet(&mut self, id: VmId, online: bool) -> Result<(), String> {
        let events = self.vm.events.clone();
        let vm = self.vm.instances.get_mut(&id).ok_or("That VM is gone")?;
        if vm.persist.as_ref().is_some_and(|p| p.saving_until.is_some()) {
            return Err("The VM is saving its state".into());
        }
        if vm.internet.pending.is_some() {
            return Err("The network is still switching".into());
        }
        if vm.internet.online == online {
            return Ok(());
        }
        vm.internet.pending = Some(online);
        spawn_set_internet(id, vm.qemu.id(), &vm.qmp, online, false, events);
        Ok(())
    }

    /// A helper thread finished switching a VM's internet link.
    pub fn vm_internet_set(
        &mut self,
        id: VmId,
        qemu_pid: u32,
        online: bool,
        result: Result<(), String>,
    ) {
        let Some(vm) = self.vm.instances.get_mut(&id) else {
            return;
        };
        if vm.qemu.id() != qemu_pid {
            return;
        }
        let held = std::mem::replace(&mut vm.internet.held, false);
        vm.internet.pending = None;
        match result {
            Ok(()) => {
                vm.internet.online = online;
                info!(id, app = vm.name, online, "Switched the VM's internet");
                if let Some(persist) = vm.persist.as_mut()
                    && let Err(err) =
                        vm_state::set_network_enabled(&mut persist.meta, &persist.meta_path, online)
                {
                    warn!(id, %err, "Switched the VM's internet but could not record it; it will not hold after a restart");
                    self.vm_overview.set_status("Could not save the setting (see log)");
                }
            }
            Err(why) if held => {
                // The guest never ran; better to stop it than to let it
                // loose with a network the user turned off.
                self.end_vm(id, EndReason::NetworkFailed(why));
            }
            Err(why) => {
                warn!(id, app = vm.name, online, %why, "Could not switch the VM's internet");
                self.vm_overview.set_status("Could not switch the network (see log)");
            }
        }
        self.refresh_vm_overview();
    }

    /// Reaps VMs that finished or got stuck. Run from a timer that exists only
    /// while VMs do (see [`ensure_vm_reaper`](Self::ensure_vm_reaper)).
    fn reap_vms(&mut self) {
        for (id, reason) in self.vm.find_finished(Instant::now()) {
            match reason {
                // A persistent VM isn't over when its program is: its state
                // gets saved first.
                EndReason::Closed if self.vm.is_persistent(id) => self.begin_save(id),
                EndReason::RestoreFailed(status) => self.restart_cold(id, status),
                EndReason::Saved => {
                    self.record_saved(id);
                    self.end_vm(id, EndReason::Saved);
                }
                other => self.end_vm(id, other),
            }
        }
    }

    /// The program of a persistent VM exited: ask QEMU to save the guest state
    /// and quit, on a helper thread (writing all of the guest's RAM takes a
    /// while). Success is noticed by QEMU exiting cleanly (see `find_finished`).
    fn begin_save(&mut self, id: VmId) {
        let events = self.vm.events.clone();
        let Some(vm) = self.vm.instances.get_mut(&id) else {
            return;
        };
        let Some(persist) = vm.persist.as_mut() else {
            return;
        };
        // waypipe is done; reap it so it isn't reported again.
        if let Some(mut waypipe) = vm.waypipe.take() {
            let _ = waypipe.wait();
        }
        persist.saving_until = Some(Instant::now() + vm_state::SAVE_TIMEOUT);
        vm.watchers.iter().for_each(Watcher::saving);
        let qmp = persist.qmp.clone();
        info!(id, app = vm.name, "Saving the VM's state");
        let spawned = std::thread::Builder::new()
            .name(format!("vm-save-{id}"))
            .spawn(move || {
                if let Err(why) = vm_state::save_state_over_socket(&qmp) {
                    let _ = events.send(VmEvent::SaveFailed(id, why));
                }
            });
        if let Err(err) = spawned {
            self.end_vm(id, EndReason::SaveFailed(err.to_string()));
        }
    }

    /// Writes down that the overlay now holds a state to restore.
    fn record_saved(&mut self, id: VmId) {
        let settings = self.config.vm.clone();
        let Some(persist) = self
            .vm
            .instances
            .get(&id)
            .and_then(|vm| vm.persist.as_ref())
        else {
            return;
        };
        if let Err(err) = vm_state::mark_saved(&persist.meta, &persist.meta_path, &settings) {
            warn!(id, %err, "Saved the VM state but could not record it; the next launch will cold-boot");
        }
    }

    /// QEMU died while loading a saved state: boot from the overlay's disk
    /// instead. The probe thread is still waiting on the same SSH port.
    fn restart_cold(&mut self, id: VmId, status: ExitStatus) {
        let Some(vm) = self.vm.instances.get_mut(&id) else {
            return;
        };
        let Some(persist) = vm.persist.as_mut() else {
            return;
        };
        warn!(id, app = vm.name, %status, logs = %vm.dir.display(), "Could not restore the saved VM state; cold-booting");
        match spawn_qemu(&persist.cold_args, &vm.dir) {
            Ok(child) => {
                vm.qemu = child;
                persist.loaded_state = false;
                // The new QEMU waits for its link to be cut as well (its
                // arguments carry `-S`), and the first one's helper is gone.
                if vm.internet.held {
                    spawn_set_internet(id, vm.qemu.id(), &vm.qmp, false, true, self.vm.events.clone());
                }
            }
            Err(err) => {
                error!(id, %err, "Failed to restart qemu");
                self.end_vm(id, EndReason::QemuExited(status));
            }
        }
    }

    fn end_vm(&mut self, id: VmId, reason: EndReason) {
        let Some(vm) = self.vm.instances.remove(&id) else {
            return;
        };
        self.handle.remove(vm.socket_token);
        let dir = vm.dir.clone();
        let name = vm.name.clone();
        let failed = reason.is_failure();

        if failed {
            warn!(id, app = name, %reason, logs = %dir.display(), "VM ended abnormally");
            for log in ["waypipe.log", "qemu.log"] {
                let tail = log_tail(&dir.join(log));
                if !tail.is_empty() {
                    warn!(id, log, "{tail}");
                }
            }
        } else {
            info!(id, app = name, %reason, "VM closed");
        }
        let finish = reason.finish();
        vm.watchers.iter().for_each(|w| w.finished(finish));
        vm.kill();

        // Logs of a failed VM are kept for debugging; everything else goes.
        if !failed {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Makes sure the once-a-second reaper timer is running. It stops itself
    /// when the last VM is gone, so an idle compositor has no extra wakeups.
    fn ensure_vm_reaper(&mut self) {
        if self.vm.reaper_running {
            return;
        }
        self.vm.reaper_running = true;
        let inserted =
            self.handle
                .insert_source(Timer::from_duration(REAP_INTERVAL), |_, _, data| {
                    data.reap_vms();
                    if data.vm.is_empty() {
                        data.vm.reaper_running = false;
                        TimeoutAction::Drop
                    } else {
                        TimeoutAction::ToDuration(REAP_INTERVAL)
                    }
                });
        if let Err(err) = inserted {
            self.vm.reaper_running = false;
            error!(%err, "Failed to start the VM reaper timer");
        }
    }

    /// Whether `id` still names a running VM.
    pub fn vm_is_running(&self, id: VmId) -> bool {
        self.vm.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> DiskMode<'static> {
        DiskMode::Snapshot {
            image: Path::new("/i"),
            qmp: Path::new("/q"),
        }
    }

    fn settings() -> VmSettings {
        VmSettings::default()
    }

    #[test]
    fn short_errors_fit_the_launcher_footer() {
        let errors = [
            VmError::NotConfigured("A Very Long Application Name".into()),
            VmError::ImageMissing("/some/very/long/path/guest.qcow2".into()),
            VmError::TooManyInstances(8),
            VmError::NoCommand,
            VmError::Io {
                context: "x",
                source: io::Error::other("y"),
            },
        ];
        for err in errors {
            assert!(
                err.short().chars().count() <= 38,
                "{:?} is too long",
                err.short()
            );
        }
    }

    #[test]
    fn shell_quote_leaves_plain_words_alone() {
        assert_eq!(shell_quote("firefox"), "firefox");
        assert_eq!(shell_quote("--new-window"), "--new-window");
        assert_eq!(shell_quote("/usr/bin/x"), "/usr/bin/x");
    }

    #[test]
    fn shell_quote_protects_everything_the_shell_would_interpret() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
    }

    #[test]
    fn remote_command_forces_wayland_and_quotes_each_word() {
        let cmd = remote_command(&["my app".into(), "--title=a b".into()]);
        assert!(cmd.starts_with("env GDK_BACKEND=wayland "), "{cmd}");
        assert!(cmd.contains("MOZ_ENABLE_WAYLAND=1"));
        assert!(cmd.ends_with("'my app' '--title=a b'"), "{cmd}");
    }

    #[test]
    fn qemu_args_boot_a_headless_snapshot_with_ssh_forwarded() {
        let args = qemu_args(
            &settings(),
            7,
            DiskMode::Snapshot {
                image: Path::new("/img/guest.qcow2"),
                qmp: Path::new("/run/s.qmp"),
            },
            2222,
            Path::new("/run/s.log"),
        );
        let joined = args.join(" ");
        assert!(
            joined.contains("-drive file=/img/guest.qcow2,if=virtio,snapshot=on"),
            "{joined}"
        );
        assert!(
            joined.contains("hostfwd=tcp:127.0.0.1:2222-:22"),
            "{joined}"
        );
        assert!(joined.contains("-display none"));
        assert!(joined.contains("-serial file:/run/s.log"));
        assert!(joined.contains("-enable-kvm"));
        assert!(joined.contains("-m 2048"));
        assert!(joined.contains("-smp 2"));
        assert!(joined.contains("-name ironland-vm-7"));
    }

    #[test]
    fn qemu_args_give_the_guest_an_internet_nic_and_a_restricted_ssh_nic() {
        let args = qemu_args(&settings(), 1, snapshot(), 2222, Path::new("/l"));
        let joined = args.join(" ");
        // The internet NIC comes first: the guest prefers the first NIC for
        // its default route, so the ssh-only one never carries internet
        // traffic while the first is up.
        let inet = joined.find("netdev=inet,id=nic_inet").expect(&joined);
        let mgmt = joined.find("netdev=mgmt,id=nic_mgmt").expect(&joined);
        assert!(inet < mgmt, "{joined}");
        assert!(joined.contains("-netdev user,id=inet "), "{joined}");
        // The ssh forward lives on the restricted NIC, so cutting the
        // internet NIC's link leaves the compositor's way in intact.
        assert!(
            joined.contains("user,id=mgmt,net=10.0.3.0/24,restrict=on,hostfwd=tcp:127.0.0.1:2222-:22"),
            "{joined}"
        );
        assert!(!joined.contains("id=inet,hostfwd"), "{joined}");
    }

    #[test]
    fn every_vm_gets_a_monitor_socket_for_switching_its_link() {
        let args = qemu_args(&settings(), 1, snapshot(), 1, Path::new("/l"));
        assert!(
            args.join(" ").contains("-qmp unix:/q,server=on,wait=off"),
            "{args:?}"
        );
    }

    #[test]
    fn qemu_args_without_kvm_uses_emulation_and_appends_extras_last() {
        let mut s = settings();
        s.kvm = false;
        s.extra_qemu_args = vec!["-bios".into(), "/fw/OVMF.fd".into()];
        let args = qemu_args(&s, 1, snapshot(), 1, Path::new("/l"));
        assert!(!args.contains(&"-enable-kvm".to_string()));
        assert_eq!(&args[args.len() - 2..], ["-bios", "/fw/OVMF.fd"]);
    }

    #[test]
    fn qemu_args_never_ask_for_zero_cpus() {
        let mut s = settings();
        s.cpus = 0;
        let args = qemu_args(&s, 1, snapshot(), 1, Path::new("/l"));
        let at = args.iter().position(|a| a == "-smp").unwrap();
        assert_eq!(args[at + 1], "1");
    }

    #[test]
    fn waypipe_args_connect_over_the_forwarded_port_and_end_with_the_command() {
        let args = waypipe_args(&settings(), 2222, &["foot".into()]);
        let ssh_at = args.iter().position(|a| a == "ssh").unwrap();
        assert!(args[..ssh_at].contains(&"--oneshot".to_string()));
        let port_at = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[port_at + 1], "2222");
        assert!(args.contains(&"user@127.0.0.1".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(args.last().unwrap().ends_with(" foot"));
    }

    #[test]
    fn waypipe_args_pass_the_configured_key_and_user() {
        let mut s = settings();
        s.user = "dev".into();
        s.ssh_key = Some("/keys/vm".into());
        let args = waypipe_args(&s, 1, &["x".into()]);
        let at = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[at + 1], "/keys/vm");
        assert!(args.contains(&"IdentitiesOnly=yes".to_string()));
        assert!(args.contains(&"dev@127.0.0.1".to_string()));
    }

    #[test]
    fn tilde_expands_only_as_a_leading_home_prefix() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_tilde("~/vm.qcow2"),
            PathBuf::from(home).join("vm.qcow2")
        );
        assert_eq!(expand_tilde("/abs/~/x"), PathBuf::from("/abs/~/x"));
        assert_eq!(expand_tilde("rel"), PathBuf::from("rel"));
    }

    #[test]
    fn ssh_ready_requires_the_ssh_banner() {
        use std::io::Write;

        // A listener that speaks SSH, one that accepts and hangs up (what
        // QEMU's forward does while the guest is still booting), and a
        // closed port.
        let ssh = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let ssh_port = ssh.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = ssh.accept() {
                let _ = s.write_all(b"SSH-2.0-test\r\n");
            }
        });
        assert!(ssh_ready(ssh_port));

        let mute = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mute_port = mute.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = mute.accept();
        });
        assert!(!ssh_ready(mute_port));

        let closed = free_loopback_port().unwrap();
        assert!(!ssh_ready(closed));
    }

    // --- resource use ----------------------------------------------------

    #[test]
    fn cpu_ticks_are_user_plus_system_time() {
        let stat = "123 (qemu-system-x86) S 1 123 123 0 -1 4194560 1000 0 0 0 250 50 0 0 20 0 5 0 99 1 2";
        assert_eq!(parse_cpu_ticks(stat), Some(300));
    }

    #[test]
    fn cpu_ticks_survive_odd_command_names() {
        let stat = "123 (a) b (c)) S 1 123 123 0 -1 4194560 1000 0 0 0 7 3 0 0 20 0 5 0 99 1 2";
        assert_eq!(parse_cpu_ticks(stat), Some(10));
        assert_eq!(parse_cpu_ticks("garbage"), None);
        assert_eq!(parse_cpu_ticks("1 (x) S 1 2"), None);
    }

    #[test]
    fn resident_pages_are_the_second_statm_field() {
        assert_eq!(parse_rss_pages("5000 1234 99 1 0 4000 0"), Some(1234));
        assert_eq!(parse_rss_pages("5000"), None);
    }

    #[test]
    fn a_vms_cpu_use_is_measured_between_samples() {
        let mut vm = instance(sleeper(), None, far_future(), true);
        let t0 = Instant::now();
        vm.sample_cpu(t0);
        assert!(vm.usage.previous.is_some());
        // A sleeping process uses (almost) no CPU.
        vm.sample_cpu(t0 + Duration::from_secs(1));
        assert!(vm.usage.cpu_percent < 50.0, "{}", vm.usage.cpu_percent);
    }

    #[test]
    fn running_vms_report_their_internet_link() {
        let mut vm = instance(sleeper(), None, far_future(), true);
        vm.internet.online = false;
        let mut manager = manager_with(vm);
        let running = manager.running(Instant::now(), 2048);
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].link, Link::Offline);
        assert_eq!(running[0].phase, Phase::Running);
        assert!(!running[0].persistent);
        assert!(running[0].rss_bytes > 0);

        manager.instances.get_mut(&1).unwrap().internet.pending = Some(true);
        assert_eq!(
            manager.running(Instant::now(), 2048)[0].link,
            Link::Switching { online: true }
        );
    }

    // --- lifecycle -------------------------------------------------------

    /// A real calloop registration token (they can't be constructed directly).
    fn registration_token() -> RegistrationToken {
        let event_loop = smithay::reexports::calloop::EventLoop::<()>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(
                Timer::from_duration(Duration::from_secs(3600)),
                |_, _, _| TimeoutAction::Drop,
            )
            .unwrap()
    }

    fn spawn(program: &str, args: &[&str]) -> Child {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.process_group(0);
        cmd.spawn().unwrap()
    }

    fn sleeper() -> Child {
        spawn("sleep", &["60"])
    }

    fn instance(
        qemu: Child,
        waypipe: Option<Child>,
        deadline: Instant,
        connected: bool,
    ) -> VmInstance {
        VmInstance {
            name: "test".into(),
            dir: PathBuf::from("/nonexistent"),
            qemu,
            waypipe,
            ssh_port: 1,
            command: vec!["x".into()],
            socket_name: "s".into(),
            socket_token: registration_token(),
            cancel: Arc::new(AtomicBool::new(false)),
            client_connected: connected,
            deadline,
            ready_seen: true,
            persist: None,
            watchers: Vec::new(),
            started: Instant::now(),
            qmp: PathBuf::from("/nonexistent/qmp.sock"),
            internet: Internet {
                online: true,
                pending: None,
                held: false,
            },
            usage: Usage::default(),
        }
    }

    fn manager_with(vm: VmInstance) -> VmManager {
        let (mut manager, _channel) = VmManager::new();
        manager.instances.insert(1, vm);
        manager
    }

    /// Polls `find_finished` for up to two seconds, for a child that exits shortly.
    fn finished_within_2s(manager: &mut VmManager) -> Vec<(VmId, EndReason)> {
        for _ in 0..40 {
            let finished = manager.find_finished(Instant::now());
            if !finished.is_empty() {
                return finished;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Vec::new()
    }

    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn a_running_vm_is_left_alone() {
        let mut manager = manager_with(instance(sleeper(), Some(sleeper()), far_future(), true));
        assert!(manager.find_finished(Instant::now()).is_empty());
    }

    #[test]
    fn qemu_dying_ends_the_vm() {
        let mut manager = manager_with(instance(spawn("false", &[]), None, far_future(), false));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::QemuExited(_))]),
            "{finished:?}"
        );
    }

    #[test]
    fn the_application_exiting_cleanly_is_a_normal_close() {
        let mut manager = manager_with(instance(
            sleeper(),
            Some(spawn("true", &[])),
            far_future(),
            true,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::Closed)]),
            "{finished:?}"
        );
        assert!(!finished[0].1.is_failure());
    }

    #[test]
    fn waypipe_failing_is_a_failure() {
        let mut manager = manager_with(instance(
            sleeper(),
            Some(spawn("false", &[])),
            far_future(),
            true,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::WaypipeFailed(_))]),
            "{finished:?}"
        );
        assert!(finished[0].1.is_failure());
    }

    #[test]
    fn no_window_by_the_deadline_times_the_vm_out() {
        let past = Instant::now() - Duration::from_secs(1);
        let mut manager = manager_with(instance(sleeper(), Some(sleeper()), past, false));
        let finished = manager.find_finished(Instant::now());
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::TimedOut)]),
            "{finished:?}"
        );
    }

    #[test]
    fn a_connected_client_cancels_the_deadline() {
        let past = Instant::now() - Duration::from_secs(1);
        let mut manager = manager_with(instance(sleeper(), Some(sleeper()), past, false));
        manager.mark_client_connected(1);
        assert!(manager.find_finished(Instant::now()).is_empty());
    }

    #[test]
    fn killing_a_vm_stops_both_processes_and_the_probe() {
        let (qemu, waypipe) = (sleeper(), sleeper());
        let pids = [qemu.id(), waypipe.id()];
        let vm = instance(qemu, Some(waypipe), far_future(), true);
        let cancel = vm.cancel.clone();
        vm.kill();
        assert!(cancel.load(Ordering::SeqCst));
        for pid in pids {
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "process {pid} survived"
            );
        }
    }

    #[test]
    fn dropping_the_manager_kills_every_vm() {
        let qemu = sleeper();
        let pid = qemu.id();
        drop(manager_with(instance(qemu, None, far_future(), false)));
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn ids_are_looked_up_through_the_manager() {
        let manager = manager_with(instance(sleeper(), None, far_future(), false));
        assert!(manager.contains(1));
        assert!(!manager.contains(2));
        assert_eq!(manager.len(), 1);
    }

    // --- persistence -----------------------------------------------------

    fn persistent_args(load_state: bool) -> Vec<String> {
        qemu_args(
            &settings(),
            3,
            DiskMode::Persistent {
                overlay: Path::new("/state/app/disk.qcow2"),
                qmp: Path::new("/run/vm/qmp.sock"),
                qga: Path::new("/run/vm/qga.sock"),
                load_state,
            },
            2222,
            Path::new("/run/vm/serial.log"),
        )
    }

    #[test]
    fn persistent_vms_boot_the_overlay_directly_without_snapshot_mode() {
        let joined = persistent_args(false).join(" ");
        assert!(
            joined.contains("file=/state/app/disk.qcow2,if=virtio,format=qcow2"),
            "{joined}"
        );
        assert!(!joined.contains("snapshot=on"), "{joined}");
        assert!(
            joined.contains("-qmp unix:/run/vm/qmp.sock,server=on,wait=off"),
            "{joined}"
        );
        assert!(
            joined.contains("path=/run/vm/qga.sock,server=on,wait=off"),
            "{joined}"
        );
        assert!(joined.contains("name=org.qemu.guest_agent.0"), "{joined}");
        assert!(!joined.contains("-loadvm"), "{joined}");
    }

    #[test]
    fn restoring_adds_loadvm_and_keeps_the_machine_identical() {
        let (cold, warm) = (persistent_args(false), persistent_args(true));
        assert!(warm.join(" ").contains("-loadvm ironland"));
        // The saved state only loads into the same machine, so the two
        // command lines may differ by nothing but the -loadvm pair.
        let stripped: Vec<_> = warm
            .iter()
            .filter(|a| *a != "-loadvm" && *a != "ironland")
            .cloned()
            .collect();
        assert_eq!(stripped, cold);
    }

    fn persist_state(saving_until: Option<Instant>, loaded_state: bool) -> PersistState {
        PersistState {
            key: "app".into(),
            meta_path: PathBuf::from("/nonexistent/meta.json"),
            meta: PersistMeta {
                image: "/i".into(),
                image_len: 1,
                image_mtime: 1,
                machine: String::new(),
                has_state: false,
                network_enabled: true,
            },
            qmp: PathBuf::from("/nonexistent/qmp.sock"),
            saving_until,
            cold_args: Vec::new(),
            loaded_state,
        }
    }

    fn persistent(
        qemu: Child,
        waypipe: Option<Child>,
        persist: PersistState,
        ready: bool,
    ) -> VmInstance {
        let mut vm = instance(qemu, waypipe, far_future(), true);
        vm.persist = Some(persist);
        vm.ready_seen = ready;
        vm
    }

    #[test]
    fn a_persistent_vm_is_reported_as_persistent() {
        let manager = manager_with(persistent(
            sleeper(),
            None,
            persist_state(None, false),
            true,
        ));
        assert!(manager.is_persistent(1));
        let plain = manager_with(instance(sleeper(), None, far_future(), false));
        assert!(!plain.is_persistent(1));
    }

    #[test]
    fn qemu_quitting_cleanly_while_saving_means_the_state_was_saved() {
        let saving = Some(far_future());
        let mut manager = manager_with(persistent(
            spawn("true", &[]),
            None,
            persist_state(saving, false),
            true,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::Saved)]),
            "{finished:?}"
        );
        assert!(!finished[0].1.is_failure());
    }

    #[test]
    fn qemu_crashing_while_saving_is_a_failure() {
        let saving = Some(far_future());
        let mut manager = manager_with(persistent(
            spawn("false", &[]),
            None,
            persist_state(saving, false),
            true,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::QemuExited(_))]),
            "{finished:?}"
        );
    }

    #[test]
    fn a_save_that_never_finishes_times_out() {
        let past = Some(Instant::now() - Duration::from_secs(1));
        let mut manager = manager_with(persistent(
            sleeper(),
            None,
            persist_state(past, false),
            true,
        ));
        let finished = manager.find_finished(Instant::now());
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::SaveTimedOut)]),
            "{finished:?}"
        );
    }

    #[test]
    fn a_vm_that_is_saving_ignores_the_startup_deadline_and_waypipe() {
        // Past the startup deadline with no client, waypipe long gone: none of
        // that matters while the state is being written.
        let mut vm = persistent(
            sleeper(),
            None,
            persist_state(Some(far_future()), false),
            true,
        );
        vm.client_connected = false;
        vm.deadline = Instant::now() - Duration::from_secs(1);
        let mut manager = manager_with(vm);
        assert!(manager.find_finished(Instant::now()).is_empty());
    }

    #[test]
    fn qemu_dying_before_the_guest_answers_after_a_restore_means_the_restore_failed() {
        let mut manager = manager_with(persistent(
            spawn("false", &[]),
            None,
            persist_state(None, true),
            false,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::RestoreFailed(_))]),
            "{finished:?}"
        );
        assert!(finished[0].1.is_failure());
    }

    #[test]
    fn qemu_dying_after_the_guest_was_up_is_an_ordinary_exit_even_if_restored() {
        let mut manager = manager_with(persistent(
            spawn("false", &[]),
            None,
            persist_state(None, true),
            true,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::QemuExited(_))]),
            "{finished:?}"
        );
    }

    #[test]
    fn a_cold_booted_persistent_vm_dying_early_is_not_a_restore_failure() {
        let mut manager = manager_with(persistent(
            spawn("false", &[]),
            None,
            persist_state(None, false),
            false,
        ));
        let finished = finished_within_2s(&mut manager);
        assert!(
            matches!(finished.as_slice(), [(1, EndReason::QemuExited(_))]),
            "{finished:?}"
        );
    }

    #[test]
    fn persistence_errors_fit_the_launcher_footer() {
        for err in [
            VmError::AlreadyRunning("A Very Long Application Name Indeed".into()),
            VmError::State("x".into()),
        ] {
            assert!(err.short().chars().count() <= 38, "{:?}", err.short());
        }
    }

    // --- live (needs a real guest image) ---------------------------------

    /// Runs `command` in the guest over ssh and returns its stdout.
    fn guest_run(port: u16, key: &str, command: &str) -> String {
        let out = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
            ])
            .args([
                "-o",
                "LogLevel=ERROR",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
            ])
            .args([
                "-i",
                key,
                "-p",
                &port.to_string(),
                "user@127.0.0.1",
                command,
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ssh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn wait_for_ssh(port: u16) {
        for _ in 0..180 {
            if ssh_ready(port) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        panic!("guest never answered on ssh");
    }

    /// End to end against a real QEMU and guest image: cold boot, leave a file
    /// and a running process behind, save the state, restore it, and check the
    /// file, the process (i.e. the memory) and the clock.
    ///
    /// Run with
    /// `IRONLAND_TEST_VM_IMAGE=... IRONLAND_TEST_VM_KEY=... cargo test --lib live_ -- --ignored --nocapture`
    /// (the image from `scripts/build-vm-guest`; needs /dev/kvm).
    #[test]
    #[ignore = "boots a real VM"]
    fn live_persistent_vm_saves_and_restores_disk_memory_and_clock() {
        let image = std::env::var("IRONLAND_TEST_VM_IMAGE").expect("IRONLAND_TEST_VM_IMAGE");
        let key = std::env::var("IRONLAND_TEST_VM_KEY").expect("IRONLAND_TEST_VM_KEY");
        let image = Path::new(&image);
        // Unix socket paths are limited to ~108 bytes: keep this short.
        let root = std::env::temp_dir().join(format!("ivt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = VmSettings {
            memory_mb: 1024,
            ..VmSettings::default()
        };
        settings.ssh_key = Some(key.clone());
        // Images log in with the key handed over at boot, as `launch_in_vm` does.
        let keys = authorized_keys(&settings, Vec::new);
        settings.extra_qemu_args.extend(authorized_keys_args(&keys));

        let boot = |load_state_expected: bool| {
            let prepared = vm_state::prepare(&root.join("state"), image, &settings).unwrap();
            assert_eq!(prepared.load_state, load_state_expected);
            let port = free_loopback_port().unwrap();
            let (qmp, qga) = (root.join("qmp.sock"), root.join("qga.sock"));
            let args = qemu_args(
                &settings,
                1,
                DiskMode::Persistent {
                    overlay: &prepared.overlay,
                    qmp: &qmp,
                    qga: &qga,
                    load_state: prepared.load_state,
                },
                port,
                &root.join("serial.log"),
            );
            let started = Instant::now();
            let qemu = spawn_qemu(&args, &root).unwrap();
            wait_for_ssh(port);
            println!(
                "ssh up after {:?} (restoring: {})",
                started.elapsed(),
                prepared.load_state
            );
            (prepared, qemu, port, qmp, qga)
        };

        // 1. Cold boot; leave a file and a long-running process behind.
        let (prepared, mut qemu, port, qmp, _qga) = boot(false);
        guest_run(port, &key, "echo persisted > ~/marker");
        let pid = guest_run(
            port,
            &key,
            "setsid sleep 100000 </dev/null >/dev/null 2>&1 & echo $!",
        );
        let guest_time_at_save: i64 = guest_run(port, &key, "date +%s").parse().unwrap();

        // 2. Save and quit, exactly as the compositor does.
        vm_state::save_state_over_socket(&qmp).unwrap();
        let status = qemu.wait().unwrap();
        assert!(
            status.success(),
            "qemu should exit cleanly after saving: {status}"
        );
        vm_state::mark_saved(&prepared.meta, &prepared.meta_path, &settings).unwrap();

        // 3. Let the host clock move on, so a stale guest clock would show.
        std::thread::sleep(Duration::from_secs(25));

        // 4. Restore.
        let (_prepared, mut qemu, port, _qmp, qga) = boot(true);
        let guest_time_before_sync: i64 = guest_run(port, &key, "date +%s").parse().unwrap();
        println!(
            "guest clock just restored: {} s behind the host",
            host_epoch() - guest_time_before_sync
        );
        vm_state::guest_set_time(&qga).expect("guest agent should set the time");
        let guest_time: i64 = guest_run(port, &key, "date +%s").parse().unwrap();

        assert_eq!(
            guest_run(port, &key, "cat ~/marker"),
            "persisted",
            "disk state"
        );
        assert_eq!(
            guest_run(port, &key, &format!("kill -0 {pid} && echo alive")),
            "alive",
            "memory state: the process started before the save must still be running"
        );
        assert!(
            guest_time > guest_time_at_save + 20,
            "the guest clock must have moved on"
        );
        assert!(
            (host_epoch() - guest_time).abs() <= 3,
            "clock should match the host after syncing"
        );

        let _ = qemu.kill();
        let _ = qemu.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Whether the guest can open a TCP connection to the host's loopback
    /// port `port` through QEMU's NAT (the gateway address stands for the
    /// host), i.e. whether its internet works. Reaching it is a stand-in for
    /// reaching the internet that needs no internet.
    fn guest_reaches_host(port_ssh: u16, key: &str, host_port: u16) -> bool {
        let command = format!(
            "timeout 5 bash -c 'echo > /dev/tcp/{GUEST_INTERNET_GATEWAY}/{host_port}' 2>/dev/null && echo yes || echo no"
        );
        guest_run(port_ssh, key, &command) == "yes"
    }

    /// Cutting a VM's internet over QMP must leave the ssh link `waypipe` runs
    /// over alone, and a VM started offline must never have been online.
    ///
    /// `IRONLAND_TEST_VM_IMAGE=... IRONLAND_TEST_VM_KEY=... cargo test --lib live_internet -- --ignored --nocapture`
    /// (the key's `.pub` is handed to the guest the way `launch_in_vm` does).
    #[test]
    #[ignore = "boots a real VM"]
    fn live_internet_can_be_cut_without_losing_the_ssh_link() {
        let image = std::env::var("IRONLAND_TEST_VM_IMAGE").expect("IRONLAND_TEST_VM_IMAGE");
        let key = std::env::var("IRONLAND_TEST_VM_KEY").expect("IRONLAND_TEST_VM_KEY");
        let root = std::env::temp_dir().join(format!("ivn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = VmSettings {
            memory_mb: 1024,
            ssh_key: Some(key.clone()),
            ..VmSettings::default()
        };
        let keys = authorized_keys(&settings, Vec::new);
        assert!(!keys.is_empty(), "no public key next to {key}");
        settings.extra_qemu_args.extend(authorized_keys_args(&keys));

        // A host service for the guest to reach through the NAT.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host_port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || for _ in listener.incoming() {});

        let boot = |name: &str, paused: bool| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let port = free_loopback_port().unwrap();
            let qmp = dir.join("qmp.sock");
            let mut args = qemu_args(
                &settings,
                1,
                DiskMode::Snapshot {
                    image: Path::new(&image),
                    qmp: &qmp,
                },
                port,
                &dir.join("serial.log"),
            );
            if paused {
                args.push("-S".into());
            }
            let qemu = spawn_qemu(&args, &dir).unwrap();
            (qemu, port, qmp)
        };

        // 1. Online: the default route is the internet NIC's, the host is reachable.
        let (mut qemu, port, qmp) = boot("online", false);
        wait_for_ssh(port);
        println!("routes:\n{}", guest_run(port, &key, "ip -4 route; ip -o -4 addr"));
        let default = guest_run(port, &key, "ip -4 route show default");
        assert!(default.contains(GUEST_INTERNET_GATEWAY), "default route: {default}");
        assert!(guest_reaches_host(port, &key, host_port), "online guest cannot reach the host");

        // 2. Cut: ssh keeps working, the host (and with it the internet) is gone.
        vm_state::set_internet_over_socket(&qmp, false, false).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(guest_run(port, &key, "echo still-here"), "still-here");
        assert!(
            !guest_reaches_host(port, &key, host_port),
            "guest still reaches the host with its internet cut"
        );
        println!("after cut:\n{}", guest_run(port, &key, "ip -4 route; ip -o link"));

        // 3. Restore: the guest comes back online.
        vm_state::set_internet_over_socket(&qmp, true, false).unwrap();
        let mut back = false;
        for _ in 0..30 {
            if guest_reaches_host(port, &key, host_port) {
                back = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(back, "guest never got its internet back");
        let _ = qemu.kill();
        let _ = qemu.wait();

        // 4. Started offline: paused, link cut, resumed. Never online.
        let (mut qemu, port, qmp) = boot("offline", true);
        vm_state::set_internet_over_socket(&qmp, false, true).unwrap();
        wait_for_ssh(port);
        assert_eq!(guest_run(port, &key, "echo offline-start"), "offline-start");
        assert!(
            !guest_reaches_host(port, &key, host_port),
            "a VM started offline reached the host"
        );
        let _ = qemu.kill();
        let _ = qemu.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A persistent VM whose internet was switched off is saved offline and
    /// comes back offline: restored paused (`-S -loadvm`), link cut, resumed.
    /// Its memory (a process started before the save) and disk survive, and
    /// the internet can be switched back on afterwards.
    #[test]
    #[ignore = "boots a real VM"]
    fn live_persistent_vm_without_internet_restores_without_internet() {
        let image = std::env::var("IRONLAND_TEST_VM_IMAGE").expect("IRONLAND_TEST_VM_IMAGE");
        let key = std::env::var("IRONLAND_TEST_VM_KEY").expect("IRONLAND_TEST_VM_KEY");
        let image = Path::new(&image);
        let root = std::env::temp_dir().join(format!("ivp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = VmSettings {
            memory_mb: 1024,
            ssh_key: Some(key.clone()),
            ..VmSettings::default()
        };
        let keys = authorized_keys(&settings, Vec::new);
        assert!(!keys.is_empty(), "no public key next to {key}");
        settings.extra_qemu_args.extend(authorized_keys_args(&keys));

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host_port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || for _ in listener.incoming() {});

        let boot = |expect_restore: bool, paused: bool| {
            let prepared = vm_state::prepare(&root.join("state"), image, &settings).unwrap();
            assert_eq!(prepared.load_state, expect_restore);
            let port = free_loopback_port().unwrap();
            let (qmp, qga) = (root.join("qmp.sock"), root.join("qga.sock"));
            let mut args = qemu_args(
                &settings,
                1,
                DiskMode::Persistent {
                    overlay: &prepared.overlay,
                    qmp: &qmp,
                    qga: &qga,
                    load_state: prepared.load_state,
                },
                port,
                &root.join("serial.log"),
            );
            if paused {
                args.push("-S".into());
            }
            let qemu = spawn_qemu(&args, &root).unwrap();
            (prepared, qemu, port, qmp)
        };

        // 1. Cold boot, leave a file and a process behind, then cut the internet
        //    and record that in the metadata, as the overview does.
        let (mut prepared, mut qemu, port, qmp) = boot(false, false);
        wait_for_ssh(port);
        guest_run(port, &key, "echo persisted > ~/marker");
        let pid = guest_run(
            port,
            &key,
            "setsid sleep 100000 </dev/null >/dev/null 2>&1 & echo $!",
        );
        assert!(guest_reaches_host(port, &key, host_port));
        vm_state::set_internet_over_socket(&qmp, false, false).unwrap();
        vm_state::set_network_enabled(&mut prepared.meta, &prepared.meta_path, false).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert!(!guest_reaches_host(port, &key, host_port));

        // 2. Save and quit; the save keeps the recorded network setting.
        vm_state::save_state_over_socket(&qmp).unwrap();
        assert!(qemu.wait().unwrap().success());
        vm_state::mark_saved(&prepared.meta, &prepared.meta_path, &settings).unwrap();

        // 3. Restore paused, cut the link before the guest runs, resume.
        let (prepared, mut qemu, port, qmp) = boot(true, true);
        assert!(!prepared.meta.network_enabled, "the setting was lost");
        vm_state::set_internet_over_socket(&qmp, false, true).unwrap();
        wait_for_ssh(port);
        assert_eq!(guest_run(port, &key, "cat ~/marker"), "persisted");
        assert_eq!(
            guest_run(port, &key, &format!("kill -0 {pid} && echo alive")),
            "alive"
        );
        assert!(
            !guest_reaches_host(port, &key, host_port),
            "the restored VM reached the host although its internet was off"
        );

        // 4. Switching it back on works.
        vm_state::set_internet_over_socket(&qmp, true, false).unwrap();
        let mut back = false;
        for _ in 0..30 {
            if guest_reaches_host(port, &key, host_port) {
                back = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(back, "internet did not come back after being switched on");
        let _ = qemu.kill();
        let _ = qemu.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cold-boot fallback (`EndReason::RestoreFailed`) relies on QEMU
    /// *exiting* when asked to load a state into a differently-configured
    /// machine, rather than starting a broken guest. Check that it does.
    #[test]
    #[ignore = "boots a real VM"]
    fn live_loading_a_state_into_a_different_machine_makes_qemu_exit() {
        let image = std::env::var("IRONLAND_TEST_VM_IMAGE").expect("IRONLAND_TEST_VM_IMAGE");
        let key = std::env::var("IRONLAND_TEST_VM_KEY").expect("IRONLAND_TEST_VM_KEY");
        let image = Path::new(&image);
        let root = std::env::temp_dir().join(format!("ivt2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let settings = VmSettings {
            memory_mb: 1024,
            ssh_key: Some(key),
            ..VmSettings::default()
        };
        let prepared = vm_state::prepare(&root.join("state"), image, &settings).unwrap();
        let (qmp, qga) = (root.join("qmp.sock"), root.join("qga.sock"));
        let args = |settings: &VmSettings, load_state: bool| {
            qemu_args(
                settings,
                1,
                DiskMode::Persistent {
                    overlay: &prepared.overlay,
                    qmp: &qmp,
                    qga: &qga,
                    load_state,
                },
                free_loopback_port().unwrap(),
                &root.join("serial.log"),
            )
        };

        let mut qemu = spawn_qemu(&args(&settings, false), &root).unwrap();
        // The monitor socket exists as soon as QEMU is up; no need to boot far.
        vm_state::save_state_over_socket(&qmp).unwrap();
        assert!(qemu.wait().unwrap().success());

        let bigger = VmSettings {
            memory_mb: 2048,
            ..settings.clone()
        };
        let mut qemu = spawn_qemu(&args(&bigger, true), &root).unwrap();
        let mut status = None;
        for _ in 0..60 {
            if let Some(st) = qemu.try_wait().unwrap() {
                status = Some(st);
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = qemu.kill();
        let _ = qemu.wait();
        let status = status.expect("qemu should refuse to load the state, not keep running");
        assert!(!status.success(), "{status}");
        println!("qemu log: {}", log_tail(&root.join("qemu.log")));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn host_epoch() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample me@host";

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn only_key_lines_are_kept() {
        let text = format!("# comment\n\n{ED}\nThe agent has no identities.\n");
        assert_eq!(parse_public_keys(&text), vec![ED.to_string()]);
    }

    #[test]
    fn explicit_keys_skip_the_agent() {
        let settings = VmSettings {
            public_key: Some(ED.into()),
            ..VmSettings::default()
        };
        let keys = authorized_keys(&settings, || panic!("agent must not be asked"));
        assert_eq!(keys, vec![ED.to_string()]);
    }

    #[test]
    fn public_key_file_and_private_key_neighbour_are_read() {
        let dir = std::env::temp_dir().join(format!("ironland-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("some.pub");
        std::fs::write(&file, format!("{ED}\n")).unwrap();
        let from_file = VmSettings {
            public_key_file: Some(file.display().to_string()),
            ..VmSettings::default()
        };
        assert_eq!(authorized_keys(&from_file, Vec::new), vec![ED.to_string()]);

        let private = dir.join("some");
        let from_private = VmSettings {
            ssh_key: Some(private.display().to_string()),
            ..VmSettings::default()
        };
        assert_eq!(
            authorized_keys(&from_private, Vec::new),
            vec![ED.to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_falls_back_to_the_agent_and_dedups() {
        let keys = authorized_keys(&VmSettings::default(), || vec![ED.into(), ED.into()]);
        assert_eq!(keys.iter().filter(|k| *k == ED).count(), 1);
    }

    #[test]
    fn keys_become_one_smbios_credential() {
        assert!(authorized_keys_args(&[]).is_empty());
        let args = authorized_keys_args(&[ED.to_string()]);
        assert_eq!(args[0], "-smbios");
        assert!(
            args[1].starts_with(
                "type=11,value=io.systemd.credential.binary:ironland.authorized_keys="
            )
        );
        assert!(!args[1].contains(' '));
    }
}
