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
//! 1. `qemu-system-x86_64` starts headless on a `-snapshot` of the configured
//!    image (so guests never modify it and many can share it), with the
//!    guest's port 22 forwarded to a free loopback port.
//! 2. A probe thread waits for the guest's SSH banner on that port and then
//!    reports [`VmEvent::GuestReady`] over a calloop channel.
//! 3. The main thread starts `waypipe ssh ... <command>` against the VM's
//!    private socket.
//! 4. When the application exits (`waypipe` returns), when QEMU dies, or when
//!    no window shows up within `startup_timeout_secs`, the whole VM is torn
//!    down: both processes killed, the socket removed.
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

use crate::{
    ClientState,
    config::VmSettings,
    launcher::{self, DesktopEntry},
    state::{AnvilState, Backend, insert_client_with_identity},
};

/// Identifies one running VM for as long as it exists. Never reused within a
/// compositor run.
pub type VmId = u64;

/// How often finished/stuck VMs are looked for.
const REAP_INTERVAL: Duration = Duration::from_secs(1);
/// How often the probe thread knocks on the guest's SSH port.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// Log lines of a failed VM's output included in the compositor log.
const LOG_TAIL_LINES: usize = 12;

/// Messages from the SSH probe thread to the main loop.
#[derive(Debug)]
pub enum VmEvent {
    /// The guest's SSH server answered; time to start `waypipe`.
    GuestReady(VmId),
}

#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("no VM image configured (set `image` under [vm] in the config)")]
    NotConfigured,
    #[error("VM image {0} does not exist")]
    ImageMissing(PathBuf),
    #[error("{0} VMs are already running (`max_instances` under [vm])")]
    TooManyInstances(u32),
    #[error("this entry has no command to run")]
    NoCommand,
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
            VmError::NotConfigured => "VM not set up (no image in config)".into(),
            VmError::ImageMissing(_) => "VM image file not found".into(),
            VmError::TooManyInstances(max) => format!("Too many VMs running (max {max})"),
            VmError::NoCommand => "Nothing to run for this entry".into(),
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
}

impl std::fmt::Display for EndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EndReason::Closed => write!(f, "the application exited"),
            EndReason::QemuExited(status) => write!(f, "qemu exited ({status})"),
            EndReason::WaypipeFailed(status) => write!(f, "waypipe failed ({status})"),
            EndReason::TimedOut => write!(f, "no window appeared in time"),
        }
    }
}

impl EndReason {
    fn is_failure(&self) -> bool {
        !matches!(self, EndReason::Closed)
    }
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

    /// Records that a Wayland client connected through `id`'s socket.
    fn mark_client_connected(&mut self, id: VmId) {
        if let Some(vm) = self.instances.get_mut(&id) {
            vm.client_connected = true;
        }
    }

    /// Looks for VMs that are over, returning each with why. Does not remove
    /// them; [`AnvilState::end_vm`] does that.
    fn find_finished(&mut self, now: Instant) -> Vec<(VmId, EndReason)> {
        let mut finished = Vec::new();
        for (&id, vm) in &mut self.instances {
            if let Ok(Some(status)) = vm.qemu.try_wait() {
                finished.push((id, EndReason::QemuExited(status)));
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

impl Drop for VmManager {
    fn drop(&mut self) {
        for (_, vm) in self.instances.drain() {
            vm.kill();
        }
    }
}

impl VmInstance {
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

/// Arguments for `qemu-system-x86_64`: a headless guest booted from a
/// throw-away snapshot of `image`, with guest port 22 reachable on
/// `127.0.0.1:ssh_port` and its serial console written to `serial_log`.
pub fn qemu_args(
    settings: &VmSettings,
    id: VmId,
    image: &Path,
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
    // `snapshot=on` keeps every guest write in a temporary overlay that is
    // thrown away on exit: the image stays pristine and any number of VMs
    // can boot from it at once.
    push(&[
        "-drive",
        &format!("file={},if=virtio,snapshot=on", image.display()),
    ]);
    push(&[
        "-nic",
        &format!("user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
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
fn spawn_probe(
    id: VmId,
    port: u16,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    events: Sender<VmEvent>,
) {
    let spawned = std::thread::Builder::new()
        .name(format!("vm-probe-{id}"))
        .spawn(move || {
            while !cancel.load(Ordering::SeqCst) && Instant::now() < deadline {
                if ssh_ready(port) {
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

/// The last few lines of `path`, for putting a failed VM's output in the log.
fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
}

// ---------------------------------------------------------------------------
// Compositor glue.
// ---------------------------------------------------------------------------

impl<BackendData: Backend + 'static> AnvilState<BackendData> {
    /// Boots a new VM and runs `entry` inside it, returning once QEMU has been
    /// *started* - the window appears later, when the guest is up and the
    /// program has opened it.
    pub fn launch_in_vm(&mut self, entry: &DesktopEntry) -> Result<VmId, VmError> {
        let settings = self.config.vm.clone();
        let image = match settings.image.as_deref().filter(|i| !i.trim().is_empty()) {
            Some(image) => expand_tilde(image),
            None => return Err(VmError::NotConfigured),
        };
        if !image.exists() {
            return Err(VmError::ImageMissing(image));
        }
        if self.vm.len() >= settings.max_instances.max(1) as usize {
            return Err(VmError::TooManyInstances(settings.max_instances.max(1)));
        }
        let terminal = settings
            .terminal
            .as_deref()
            .unwrap_or(&self.config.terminal);
        let command = launcher::guest_command(entry, terminal).ok_or(VmError::NoCommand)?;

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

        let qemu = match self.spawn_qemu(&settings, id, &image, ssh_port, &dir) {
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
        );

        info!(id, app = entry.name, ssh_port, ?image, "Started VM");
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
            },
        );
        self.ensure_vm_reaper();
        Ok(id)
    }

    /// Launcher action: runs the highlighted entry in a new VM. Closes the
    /// launcher on success; on failure it stays open with the reason in its
    /// footer, so the user can see what's wrong without digging through logs.
    pub fn launch_selected_in_vm(&mut self) {
        let Some(entry) = self.launcher.selected_entry() else {
            return;
        };
        match self.launch_in_vm(&entry) {
            Ok(_) => self.launcher.close(),
            Err(err) => {
                warn!(app = entry.name, %err, "Could not launch in a VM");
                self.launcher.set_status(err.short());
            }
        }
    }

    fn spawn_qemu(
        &self,
        settings: &VmSettings,
        id: VmId,
        image: &Path,
        ssh_port: u16,
        dir: &Path,
    ) -> Result<Child, VmError> {
        let args = qemu_args(settings, id, image, ssh_port, &dir.join("serial.log"));
        let (stdout, stderr) = log_stdio(&dir.join("qemu.log"))?;
        let mut cmd = Command::new(QEMU_BINARY);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        die_with_parent(&mut cmd);
        cmd.spawn()
            .map_err(VmError::io("starting qemu-system-x86_64"))
    }

    /// Starts `waypipe` for a VM whose guest just answered on SSH.
    pub fn vm_guest_ready(&mut self, id: VmId) {
        let settings = self.config.vm.clone();
        let Some(vm) = self.vm.instances.get_mut(&id) else {
            return;
        };
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

    /// Reaps VMs that finished or got stuck. Run from a timer that exists only
    /// while VMs do (see [`ensure_vm_reaper`](Self::ensure_vm_reaper)).
    fn reap_vms(&mut self) {
        for (id, reason) in self.vm.find_finished(Instant::now()) {
            self.end_vm(id, reason);
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
            info!(id, app = name, "VM closed");
        }
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

    fn settings() -> VmSettings {
        VmSettings {
            image: Some("/img/guest.qcow2".into()),
            ..VmSettings::default()
        }
    }

    #[test]
    fn short_errors_fit_the_launcher_footer() {
        let errors = [
            VmError::NotConfigured,
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
            Path::new("/img/guest.qcow2"),
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
    fn qemu_args_without_kvm_uses_emulation_and_appends_extras_last() {
        let mut s = settings();
        s.kvm = false;
        s.extra_qemu_args = vec!["-bios".into(), "/fw/OVMF.fd".into()];
        let args = qemu_args(&s, 1, Path::new("/i"), 1, Path::new("/l"));
        assert!(!args.contains(&"-enable-kvm".to_string()));
        assert_eq!(&args[args.len() - 2..], ["-bios", "/fw/OVMF.fd"]);
    }

    #[test]
    fn qemu_args_never_ask_for_zero_cpus() {
        let mut s = settings();
        s.cpus = 0;
        let args = qemu_args(&s, 1, Path::new("/i"), 1, Path::new("/l"));
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
}
