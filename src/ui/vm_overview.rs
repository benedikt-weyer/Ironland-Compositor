//! VM overview: an overlay (`super+o` by default) listing the VMs that are
//! running with their resource use and how they reach the outside world, the
//! persistent VMs' state on disk, and every program that can be started in a
//! VM.
//!
//! Each running VM is in one of three network modes, chosen here with the
//! buttons (or `O`/`T`/`X`, or `N` to flip between online and offline):
//! *online* (through the host's network), *Tor* (only through the host's Tor
//! client, by way of the VM's transparent gateway) and *offline*. In Tor mode
//! a fourth button, "new Tor identity", resets the VM's connections and gives
//! it fresh circuits. For a persistent VM the mode is kept in its metadata
//! (see [`crate::shell::vm_state::PersistMeta::network`]) and applies the next
//! time it starts.
//!
//! The data comes from `AnvilState::refresh_vm_overview` (`crate::shell::vm`),
//! once a second while the overlay is open. Everything in this file is pure:
//! [`layout`] turns the data into lines of text and buttons, and
//! [`VmOverviewState`] rasterizes them into a `MemoryRenderBuffer` the way the
//! launcher does and answers where a click landed. Like the launcher, the
//! overlay grabs the keyboard while it is visible, and it grabs the pointer.

use std::{collections::HashMap, net::Ipv4Addr, time::Duration};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::{Logical, Point, Size, Transform},
};

use crate::{
    font::{Canvas, GLYPH_HEIGHT, GLYPH_WIDTH},
    shell::{vm_state::NetMode, vm_tor::TorStatus},
};

/// Overlay width in logical pixels.
const WIDTH: i32 = 1040;
const PADDING: i32 = 12;
const FONT_SCALE: i32 = 2;
const LINE_HEIGHT: i32 = 22;
/// A line of buttons is taller than a line of text.
const BUTTON_LINE_HEIGHT: i32 = 36;
const BUTTON_HEIGHT: i32 = 28;
const BUTTON_PADDING: i32 = 14;
const BUTTON_GAP: i32 = 10;
/// Indent of the button row, in pixels (two characters).
const BUTTON_INDENT: i32 = 2 * (GLYPH_WIDTH as i32 + 1) * FONT_SCALE;
/// Characters that fit on one line.
const LINE_CHARS: usize =
    ((WIDTH - 2 * PADDING) / ((GLYPH_WIDTH as i32 + 1) * FONT_SCALE)) as usize;

/// Most persisted VMs and programs listed before "+N more".
const MAX_PERSISTED_ROWS: usize = 5;
const MAX_PROGRAM_ROWS: usize = 4;
const PROGRAM_COLUMNS: usize = 2;

// Canvas pixels are B, G, R, A.
const fn rgba(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [b, g, r, a]
}

const COLOR_BACKGROUND: [u8; 4] = rgba(30, 34, 40, 255);
const COLOR_HEADER_BG: [u8; 4] = rgba(52, 60, 70, 255);
const COLOR_SELECTED_BG: [u8; 4] = rgba(40, 90, 120, 255);
const COLOR_BUTTON_ACTIVE: [u8; 4] = rgba(60, 130, 170, 255);
const COLOR_TEXT: [u8; 4] = rgba(235, 230, 225, 255);
const COLOR_MUTED: [u8; 4] = rgba(150, 140, 130, 255);
const COLOR_ONLINE: [u8; 4] = rgba(130, 205, 130, 255);
const COLOR_TOR: [u8; 4] = rgba(190, 150, 230, 255);
const COLOR_OFFLINE: [u8; 4] = rgba(240, 150, 100, 255);
const COLOR_ERROR: [u8; 4] = rgba(235, 120, 110, 255);

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// Where a running VM is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The guest is starting or the program has not opened a window yet.
    Booting,
    Running,
    /// A persistent VM's program exited and its state is being written.
    Saving,
}

/// A VM in Tor mode, as the overview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorLink {
    /// The host's Tor client is running.
    pub running: bool,
    /// Percent of Tor's start-up done; 100 once it can build circuits.
    pub bootstrap: u8,
    /// The last thing Tor complained about.
    pub problem: Option<String>,
    /// Connections the VM has open through Tor.
    pub flows: u32,
    /// Starts at 1; each "new Tor identity" adds one.
    pub identity: u32,
}

/// A running VM's connection to the outside world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Online,
    Offline,
    Tor(TorLink),
    /// The change to `to` has been asked for and is not done yet.
    Switching {
        to: NetMode,
    },
}

impl Link {
    /// The mode this link is in or going to.
    fn mode(&self) -> NetMode {
        match self {
            Link::Online => NetMode::Online,
            Link::Offline => NetMode::Offline,
            Link::Tor(_) => NetMode::Tor,
            Link::Switching { to } => *to,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunningVm {
    pub id: u64,
    pub name: String,
    pub persistent: bool,
    pub phase: Phase,
    pub uptime: Duration,
    /// Percent of one core.
    pub cpu_percent: f32,
    /// Host memory the VM's QEMU process occupies.
    pub rss_bytes: u64,
    /// Guest RAM the VM was given, in MiB.
    pub memory_mb: u32,
    pub link: Link,
    /// Loopback port forwarded to the guest's ssh server.
    pub ssh_port: u16,
}

/// A persistent VM's directory under the state root.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedEntry {
    pub app: String,
    pub disk_bytes: u64,
    /// The overlay holds a saved state to resume from.
    pub has_state: bool,
    /// The mode the VM starts in; `None` for state without readable metadata.
    pub network: Option<NetMode>,
    /// Kept aside after the program's image was replaced.
    pub stale: bool,
    pub running: bool,
}

/// A program that can be started in a VM.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramEntry {
    pub name: String,
    /// Size of the program's guest image.
    pub image_bytes: u64,
    /// How many VMs of it are running.
    pub running: usize,
}

/// How traffic leaves the host: the interface and gateway of its default route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostRoute {
    pub uplink: Option<String>,
    pub gateway: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VmOverviewData {
    pub running: Vec<RunningVm>,
    pub persisted: Vec<PersistedEntry>,
    pub programs: Vec<ProgramEntry>,
    pub route: HostRoute,
    /// The host's Tor client.
    pub tor: TorStatus,
    pub max_instances: u32,
}

/// The host's default route from the text of `/proc/net/route`: of the routes
/// to everywhere (destination 0.0.0.0), the one with the lowest metric.
pub fn parse_default_route(text: &str) -> HostRoute {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [
                iface,
                destination,
                gateway,
                _flags,
                _refcnt,
                _use,
                metric,
                ..,
            ] = fields[..]
            else {
                return None;
            };
            if destination != "00000000" {
                return None;
            }
            let gateway = u32::from_str_radix(gateway, 16).ok()?;
            let metric: u32 = metric.parse().ok()?;
            Some((metric, iface, gateway))
        })
        .min_by_key(|&(metric, ..)| metric)
        .map(|(_, iface, gateway)| HostRoute {
            uplink: Some(iface.to_string()),
            // The kernel prints the address as a host-order number of the
            // bytes in memory, i.e. least significant byte first.
            gateway: (gateway != 0).then(|| Ipv4Addr::from(gateway.to_le_bytes())),
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// "1.2G", "340M", "12K": the biggest unit that keeps the number short.
pub fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{}M", bytes / MIB)
    } else {
        format!("{}K", bytes.div_ceil(1024))
    }
}

/// "42S", "12M05S", "2H03M".
pub fn format_uptime(uptime: Duration) -> String {
    let secs = uptime.as_secs();
    match secs {
        0..60 => format!("{secs}S"),
        60..3600 => format!("{}M{:02}S", secs / 60, secs % 60),
        _ => format!("{}H{:02}M", secs / 3600, secs % 3600 / 60),
    }
}

/// `text` padded with spaces to `width` characters, or cut to fit.
fn fit(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    let len = out.chars().count();
    out.extend(std::iter::repeat_n(' ', width - len));
    out
}

/// `text` cut to `max` characters, ending in a dot if anything was dropped.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('.');
    out
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Title,
    Header,
    Text,
    Muted,
    Online,
    Tor,
    Offline,
    Error,
}

impl Style {
    fn color(self) -> [u8; 4] {
        match self {
            Style::Title | Style::Header | Style::Text => COLOR_TEXT,
            Style::Muted => COLOR_MUTED,
            Style::Online => COLOR_ONLINE,
            Style::Tor => COLOR_TOR,
            Style::Offline => COLOR_OFFLINE,
            Style::Error => COLOR_ERROR,
        }
    }
}

/// What a button does to the selected VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Mode(NetMode),
    /// Fresh Tor circuits (only in Tor mode).
    NewIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Button {
    label: String,
    action: Action,
    /// The mode the VM is in (or going to).
    active: bool,
    enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    text: String,
    style: Style,
    /// Part of the selected VM.
    selected: bool,
    /// The running VM the line belongs to, so a click on it selects it.
    vm: Option<u64>,
    /// A row of buttons instead of text.
    buttons: Vec<Button>,
}

impl Line {
    fn new(style: Style, text: impl Into<String>) -> Self {
        Self {
            text: clip(&text.into(), LINE_CHARS),
            style,
            selected: false,
            vm: None,
            buttons: Vec::new(),
        }
    }

    fn blank() -> Self {
        Self::new(Style::Text, "")
    }

    fn height(&self) -> i32 {
        if self.buttons.is_empty() {
            LINE_HEIGHT
        } else {
            BUTTON_LINE_HEIGHT
        }
    }
}

/// A rectangle in the overlay's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, p: Point<i32, Logical>) -> bool {
        (self.x..self.x + self.w).contains(&p.x) && (self.y..self.y + self.h).contains(&p.y)
    }
}

/// Top of each line, in the overlay's coordinates.
fn line_tops(lines: &[Line]) -> Vec<i32> {
    let mut y = PADDING;
    lines
        .iter()
        .map(|line| {
            let top = y;
            y += line.height();
            top
        })
        .collect()
}

fn total_height(lines: &[Line]) -> i32 {
    2 * PADDING + lines.iter().map(Line::height).sum::<i32>()
}

/// Where each button of `line` (whose top is `top`) is drawn.
fn button_rects(line: &Line, top: i32) -> Vec<(Rect, &Button)> {
    let mut x = PADDING + BUTTON_INDENT;
    line.buttons
        .iter()
        .map(|button| {
            let w = Canvas::text_width(&button.label, FONT_SCALE) + 2 * BUTTON_PADDING;
            let rect = Rect {
                x,
                y: top + (line.height() - BUTTON_HEIGHT) / 2,
                w,
                h: BUTTON_HEIGHT,
            };
            x += w + BUTTON_GAP;
            (rect, button)
        })
        .collect()
}

fn running_line(vm: &RunningVm, selected: bool) -> Line {
    let kind = if vm.persistent {
        "PERSISTENT"
    } else {
        "THROW-AWAY"
    };
    let phase = match vm.phase {
        Phase::Booting => "STARTING",
        Phase::Running => "RUNNING",
        Phase::Saving => "SAVING",
    };
    let ram = format!(
        "{}/{}",
        human_bytes(vm.rss_bytes),
        human_bytes(u64::from(vm.memory_mb) * MIB)
    );
    let text = format!(
        "{} {} {} {} UP {} CPU {} RAM {}",
        if selected { ">" } else { " " },
        fit(&vm.name, 20),
        kind,
        fit(phase, 8),
        fit(&format_uptime(vm.uptime), 7),
        fit(&format!("{:.0}%", vm.cpu_percent), 4),
        ram,
    );
    Line {
        selected,
        vm: Some(vm.id),
        ..Line::new(Style::Text, text)
    }
}

/// The line under a running VM saying how it reaches the outside world.
fn route_line(vm: &RunningVm, route: &HostRoute, selected: bool) -> Line {
    let ssh = format!(":{}", vm.ssh_port);
    let (style, text) = match &vm.link {
        Link::Online => {
            let uplink = match (&route.uplink, route.gateway) {
                (Some(iface), Some(gateway)) => format!("{iface} VIA {gateway}"),
                (Some(iface), None) => iface.clone(),
                (None, _) => "NO HOST ROUTE".to_string(),
            };
            (
                Style::Online,
                format!(
                    "    NET ONLINE: {} > NAT > {uplink}   SSH {ssh}",
                    crate::shell::vm::GUEST_INTERNET_ADDR
                ),
            )
        }
        Link::Offline => (
            Style::Offline,
            format!("    NET OFFLINE: INTERNET LINK CUT, ONLY SSH {ssh} IS LEFT"),
        ),
        Link::Tor(tor) if !tor.running => (
            Style::Error,
            format!(
                "    NET TOR: TOR IS NOT RUNNING{}",
                tor.problem
                    .as_deref()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default()
            ),
        ),
        Link::Tor(tor) => {
            let (style, state) = if tor.bootstrap >= 100 {
                (Style::Tor, "TOR".to_string())
            } else {
                (Style::Muted, format!("TOR (STARTING {}%)", tor.bootstrap))
            };
            (
                style,
                format!(
                    "    NET TOR: {} > GATEWAY > {state}   ID {}   {} CONN   SSH {ssh}",
                    crate::shell::vm_gateway::GUEST_IP,
                    tor.identity,
                    tor.flows
                ),
            )
        }
        Link::Switching { to } => (
            Style::Muted,
            format!(
                "    NET SWITCHING TO {}...",
                to.label().to_ascii_uppercase()
            ),
        ),
    };
    Line {
        selected,
        vm: Some(vm.id),
        ..Line::new(style, text)
    }
}

/// The row of buttons for the selected VM.
fn button_line(vm: &RunningVm) -> Line {
    let current = vm.link.mode();
    let in_tor = matches!(vm.link, Link::Tor(_));
    let button = |label: &str, action: Action, enabled: bool| Button {
        label: label.to_string(),
        action,
        active: action == Action::Mode(current),
        enabled,
    };
    Line {
        buttons: vec![
            button("O  ONLINE", Action::Mode(NetMode::Online), true),
            button("T  TOR", Action::Mode(NetMode::Tor), true),
            button("X  OFFLINE", Action::Mode(NetMode::Offline), true),
            button("I  NEW TOR IDENTITY", Action::NewIdentity, in_tor),
        ],
        ..Line::blank()
    }
}

fn persisted_line(entry: &PersistedEntry) -> Line {
    let state = if entry.stale {
        "OLD IMAGE, KEPT ASIDE"
    } else if entry.has_state {
        "STATE SAVED"
    } else {
        "DISK ONLY"
    };
    let net = match entry.network {
        Some(NetMode::Online) => "NET ON",
        Some(NetMode::Tor) => "NET TOR",
        Some(NetMode::Offline) => "NET OFF",
        None => "NET ?",
    };
    let text = format!(
        "  {} {} {} {}{}",
        fit(&entry.app, 22),
        fit(&format!("{} ON DISK", human_bytes(entry.disk_bytes)), 14),
        fit(state, 22),
        net,
        if entry.running { "   RUNNING" } else { "" },
    );
    let style = match entry.network {
        Some(NetMode::Offline) => Style::Offline,
        Some(NetMode::Tor) => Style::Tor,
        _ => Style::Text,
    };
    Line::new(style, text)
}

/// The line about the host's Tor client, if it is or was in use.
fn tor_line(tor: &TorStatus) -> Option<Line> {
    if tor.running {
        let state = if tor.bootstrap >= 100 {
            "READY".to_string()
        } else {
            format!("STARTING, {}%", tor.bootstrap)
        };
        Some(Line::new(
            Style::Muted,
            format!("HOST TOR: RUNNING, {state}"),
        ))
    } else {
        tor.problem
            .as_ref()
            .map(|problem| Line::new(Style::Error, format!("HOST TOR: NOT RUNNING ({problem})")))
    }
}

/// Everything the overlay says, top to bottom. `selected` is the index of the
/// highlighted running VM; `status` replaces the key hints at the bottom, as
/// an error or (`true`) a plain notice.
fn layout(data: &VmOverviewData, selected: usize, status: Option<(&str, bool)>) -> Vec<Line> {
    let mut lines = Vec::new();
    let max = data.max_instances.max(1);
    lines.push(Line::new(
        Style::Title,
        format!("VM OVERVIEW   {}/{} RUNNING", data.running.len(), max),
    ));
    let uplink = match (&data.route.uplink, data.route.gateway) {
        (Some(iface), Some(gateway)) => format!("{iface} VIA {gateway}"),
        (Some(iface), None) => iface.clone(),
        (None, _) => "NONE (THE HOST IS OFFLINE)".to_string(),
    };
    lines.push(Line::new(
        Style::Muted,
        format!("HOST UPLINK: {uplink}. ONLINE VMS REACH IT THROUGH QEMU'S NAT."),
    ));
    lines.extend(tor_line(&data.tor));

    lines.push(Line::blank());
    lines.push(Line::new(Style::Header, "RUNNING VMS"));
    if data.running.is_empty() {
        lines.push(Line::new(
            Style::Muted,
            "  NONE. START ONE FROM THE LAUNCHER (LEFT/RIGHT TABS).",
        ));
    }
    for (i, vm) in data.running.iter().enumerate() {
        lines.push(running_line(vm, i == selected));
        lines.push(route_line(vm, &data.route, i == selected));
    }
    if let Some(vm) = data.running.get(selected) {
        lines.push(Line::new(
            Style::Muted,
            format!("  NETWORK OF {}:", clip(&vm.name.to_ascii_uppercase(), 40)),
        ));
        lines.push(button_line(vm));
    }

    lines.push(Line::blank());
    let total: u64 = data.persisted.iter().map(|p| p.disk_bytes).sum();
    lines.push(Line::new(
        Style::Header,
        format!(
            "PERSISTED VMS ({})   {} ON DISK",
            data.persisted.len(),
            human_bytes(total)
        ),
    ));
    if data.persisted.is_empty() {
        lines.push(Line::new(
            Style::Muted,
            "  NONE. THE LAUNCHER'S RIGHT TAB STARTS A PERSISTENT VM.",
        ));
    }
    lines.extend(
        data.persisted
            .iter()
            .take(MAX_PERSISTED_ROWS)
            .map(persisted_line),
    );
    if data.persisted.len() > MAX_PERSISTED_ROWS {
        lines.push(Line::new(
            Style::Muted,
            format!("  +{} MORE", data.persisted.len() - MAX_PERSISTED_ROWS),
        ));
    }

    lines.push(Line::blank());
    lines.push(Line::new(
        Style::Header,
        format!("AVAILABLE AS VM ({})", data.programs.len()),
    ));
    if data.programs.is_empty() {
        lines.push(Line::new(
            Style::Muted,
            "  NONE SET UP (SEE VM.PROGRAMS IN THE CONFIG).",
        ));
    }
    let shown = (MAX_PROGRAM_ROWS * PROGRAM_COLUMNS).min(data.programs.len());
    let cell = (LINE_CHARS - 2) / PROGRAM_COLUMNS;
    for row in data.programs[..shown].chunks(PROGRAM_COLUMNS) {
        let text: String = row
            .iter()
            .map(|p| {
                let running = if p.running > 0 {
                    format!("  {} RUNNING", p.running)
                } else {
                    String::new()
                };
                fit(
                    &format!(
                        "{} {}{running}",
                        fit(&p.name, 24),
                        human_bytes(p.image_bytes)
                    ),
                    cell,
                )
            })
            .collect();
        lines.push(Line::new(Style::Text, format!("  {}", text.trim_end())));
    }
    if data.programs.len() > shown {
        lines.push(Line::new(
            Style::Muted,
            format!("  +{} MORE", data.programs.len() - shown),
        ));
    }

    lines.push(Line::blank());
    lines.push(match status {
        Some((message, true)) => Line::new(Style::Tor, message),
        Some((message, false)) => Line::new(Style::Error, message),
        None if data.running.is_empty() => Line::new(Style::Muted, "ESC: CLOSE"),
        None => Line::new(
            Style::Muted,
            "UP/DOWN: SELECT   O/T/X: ONLINE/TOR/OFFLINE   I: NEW IDENTITY   ESC: CLOSE",
        ),
    });
    lines
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// What a click on the overlay hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A running VM's rows.
    Select(u64),
    /// A button (only enabled ones are reported).
    Button(Action),
    /// The panel, but nothing in it.
    Panel,
    /// Outside the panel.
    Outside,
}

#[derive(Debug, Default)]
pub struct VmOverviewState {
    visible: bool,
    data: VmOverviewData,
    /// The VM the selection follows as the list changes.
    selected_id: Option<u64>,
    /// Shown in place of the key hints until the next key: the message, and
    /// whether it is a notice rather than an error.
    status: Option<(String, bool)>,
    /// Display names by program id (desktop entry id without `.desktop`),
    /// looked up once when the overlay opens.
    names: HashMap<String, String>,
    /// The refresh timer (see `AnvilState::open_vm_overview`) is running.
    pub(crate) timer_running: bool,
    buffer: Option<MemoryRenderBuffer>,
    dirty: bool,
}

impl VmOverviewState {
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub(crate) fn open(&mut self, names: HashMap<String, String>) {
        self.visible = true;
        self.names = names;
        self.status = None;
        self.selected_id = None;
        self.dirty = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.buffer = None;
        self.dirty = true;
    }

    pub fn names(&self) -> &HashMap<String, String> {
        &self.names
    }

    pub fn set_data(&mut self, data: VmOverviewData) {
        if self.data == data {
            return;
        }
        self.data = data;
        if !self
            .selected_id
            .is_some_and(|id| self.data.running.iter().any(|vm| vm.id == id))
        {
            self.selected_id = self.data.running.first().map(|vm| vm.id);
        }
        self.dirty = true;
    }

    /// Shows an error in place of the key hints.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), false));
        self.dirty = true;
    }

    /// Shows a plain message in place of the key hints.
    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), true));
        self.dirty = true;
    }

    pub fn clear_status(&mut self) {
        if self.status.take().is_some() {
            self.dirty = true;
        }
    }

    fn selected_index(&self) -> usize {
        self.selected_id
            .and_then(|id| self.data.running.iter().position(|vm| vm.id == id))
            .unwrap_or(0)
    }

    /// Moves the selection `delta` VMs down (negative: up), wrapping around.
    pub fn move_selection(&mut self, delta: i32) {
        self.clear_status();
        let len = self.data.running.len() as i32;
        if len == 0 {
            return;
        }
        let next = (self.selected_index() as i32 + delta).rem_euclid(len) as usize;
        self.selected_id = Some(self.data.running[next].id);
        self.dirty = true;
    }

    /// Selects the running VM `id`, if it is running.
    pub fn select(&mut self, id: u64) {
        if self.data.running.iter().any(|vm| vm.id == id) {
            self.clear_status();
            self.selected_id = Some(id);
            self.dirty = true;
        }
    }

    pub fn selected(&self) -> Option<&RunningVm> {
        self.data.running.get(self.selected_index())
    }

    fn lines(&self) -> Vec<Line> {
        layout(
            &self.data,
            self.selected_index(),
            self.status
                .as_ref()
                .map(|(m, notice)| (m.as_str(), *notice)),
        )
    }

    /// The logical size of the overlay, so callers can center it on the output.
    pub fn logical_size(&self) -> Size<i32, Logical> {
        Size::from((WIDTH, total_height(&self.lines())))
    }

    /// What a click at `at` (relative to the overlay's top-left corner) hit.
    pub fn hit_test(&self, at: Point<i32, Logical>) -> Hit {
        let lines = self.lines();
        let size = Size::<i32, Logical>::from((WIDTH, total_height(&lines)));
        if at.x < 0 || at.y < 0 || at.x >= size.w || at.y >= size.h {
            return Hit::Outside;
        }
        for (line, top) in lines.iter().zip(line_tops(&lines)) {
            for (rect, button) in button_rects(line, top) {
                if rect.contains(at) {
                    return if button.enabled {
                        Hit::Button(button.action)
                    } else {
                        Hit::Panel
                    };
                }
            }
            if let Some(id) = line.vm
                && (top..top + line.height()).contains(&at.y)
            {
                return Hit::Select(id);
            }
        }
        Hit::Panel
    }

    fn rasterize(&self) -> Canvas {
        let lines = self.lines();
        let width = WIDTH;
        let height = total_height(&lines);
        let mut canvas = Canvas::new(width as usize, height as usize, COLOR_BACKGROUND);
        let text_y = |top: i32, h: i32| top + (h - GLYPH_HEIGHT as i32 * FONT_SCALE) / 2;
        for (line, top) in lines.iter().zip(line_tops(&lines)) {
            match line.style {
                Style::Header | Style::Title => {
                    canvas.fill_rect(0, top, width, line.height(), COLOR_HEADER_BG)
                }
                _ if line.selected => {
                    canvas.fill_rect(0, top, width, line.height(), COLOR_SELECTED_BG)
                }
                _ => {}
            }
            canvas.draw_text(
                PADDING,
                text_y(top, line.height()),
                &line.text,
                FONT_SCALE,
                line.style.color(),
            );
            for (rect, button) in button_rects(line, top) {
                let background = if button.active {
                    COLOR_BUTTON_ACTIVE
                } else {
                    COLOR_HEADER_BG
                };
                canvas.fill_rect(rect.x, rect.y, rect.w, rect.h, background);
                canvas.draw_text(
                    rect.x + BUTTON_PADDING,
                    text_y(rect.y, rect.h),
                    &button.label,
                    FONT_SCALE,
                    if button.enabled {
                        COLOR_TEXT
                    } else {
                        COLOR_MUTED
                    },
                );
            }
        }
        canvas
    }

    /// The buffer to render, rebuilt if anything changed since the last frame.
    pub fn ensure_buffer(&mut self) -> Option<&MemoryRenderBuffer> {
        if !self.visible {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(id: u64, name: &str, persistent: bool, link: Link) -> RunningVm {
        RunningVm {
            id,
            name: name.into(),
            persistent,
            phase: Phase::Running,
            uptime: Duration::from_secs(754),
            cpu_percent: 34.4,
            rss_bytes: 1_300_000_000,
            memory_mb: 2048,
            link,
            ssh_port: 43211,
        }
    }

    fn tor_link(bootstrap: u8) -> Link {
        Link::Tor(TorLink {
            running: true,
            bootstrap,
            problem: None,
            flows: 5,
            identity: 3,
        })
    }

    fn route() -> HostRoute {
        HostRoute {
            uplink: Some("wlp3s0".into()),
            gateway: Some(Ipv4Addr::new(192, 168, 1, 1)),
        }
    }

    fn data() -> VmOverviewData {
        VmOverviewData {
            running: vec![
                vm(1, "Firefox", true, Link::Online),
                vm(2, "Alacritty", false, Link::Offline),
            ],
            persisted: vec![PersistedEntry {
                app: "Firefox".into(),
                disk_bytes: 1_800_000_000,
                has_state: true,
                network: Some(NetMode::Online),
                stale: false,
                running: true,
            }],
            programs: vec![
                ProgramEntry {
                    name: "Firefox".into(),
                    image_bytes: 2_900_000_000,
                    running: 1,
                },
                ProgramEntry {
                    name: "Alacritty".into(),
                    image_bytes: 2_700_000_000,
                    running: 1,
                },
            ],
            route: route(),
            tor: TorStatus::default(),
            max_instances: 8,
        }
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn default_route_is_the_lowest_metric_route_to_everywhere() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
            eth0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            wlp3s0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            wlp3s0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        let route = parse_default_route(table);
        assert_eq!(route.uplink.as_deref(), Some("wlp3s0"));
        assert_eq!(route.gateway, Some(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn no_default_route_means_no_uplink() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
            lo\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\n";
        assert_eq!(parse_default_route(table), HostRoute::default());
        assert_eq!(parse_default_route(""), HostRoute::default());
    }

    #[test]
    fn sizes_use_the_biggest_short_unit() {
        assert_eq!(human_bytes(0), "0K");
        assert_eq!(human_bytes(1), "1K");
        assert_eq!(human_bytes(340 * MIB), "340M");
        assert_eq!(human_bytes(3 * 1024 * MIB / 2), "1.5G");
    }

    #[test]
    fn uptime_shows_the_two_biggest_units() {
        assert_eq!(format_uptime(Duration::from_secs(42)), "42S");
        assert_eq!(format_uptime(Duration::from_secs(725)), "12M05S");
        assert_eq!(format_uptime(Duration::from_secs(2 * 3600 + 185)), "2H03M");
    }

    #[test]
    fn fit_pads_and_cuts() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcdef", 4), "abcd");
        assert_eq!(clip("abcdef", 4), "abc.");
        assert_eq!(clip("abc", 4), "abc");
    }

    #[test]
    fn the_overview_names_every_part_of_a_running_vm() {
        let lines = layout(&data(), 0, None);
        let all = text(&lines);
        assert!(all.contains("2/8 RUNNING"), "{all}");
        assert!(all.contains("PERSISTENT"), "{all}");
        assert!(all.contains("THROW-AWAY"), "{all}");
        assert!(all.contains("CPU 34%"), "{all}");
        assert!(all.contains("RAM 1.2G/2.0G"), "{all}");
        assert!(all.contains("UP 12M34S"), "{all}");
        assert!(
            all.contains("10.0.2.15 > NAT > wlp3s0 VIA 192.168.1.1"),
            "{all}"
        );
        assert!(all.contains("NET OFFLINE"), "{all}");
        assert!(all.contains("SSH :43211"), "{all}");
    }

    #[test]
    fn a_vm_in_tor_mode_shows_its_path_identity_and_connections() {
        let mut d = data();
        d.running[0].link = tor_link(100);
        let all = text(&layout(&d, 0, None));
        assert!(
            all.contains("NET TOR: 10.0.4.15 > GATEWAY > TOR   ID 3   5 CONN   SSH :43211"),
            "{all}"
        );
        d.running[0].link = tor_link(40);
        let all = text(&layout(&d, 0, None));
        assert!(all.contains("TOR (STARTING 40%)"), "{all}");
    }

    #[test]
    fn a_vm_in_tor_mode_without_a_running_tor_says_so() {
        let mut d = data();
        d.running[0].link = Link::Tor(TorLink {
            running: false,
            bootstrap: 0,
            problem: Some("tor exited (exit status: 1)".into()),
            flows: 0,
            identity: 1,
        });
        let line = route_line(&d.running[0], &d.route, true);
        assert!(line.text.contains("TOR IS NOT RUNNING"), "{}", line.text);
        assert!(line.text.contains("tor exited"), "{}", line.text);
        assert_eq!(line.style, Style::Error);
    }

    #[test]
    fn a_vm_being_switched_says_where_to() {
        let mut d = data();
        d.running[0].link = Link::Switching { to: NetMode::Tor };
        let all = text(&layout(&d, 0, None));
        assert!(all.contains("NET SWITCHING TO TOR..."), "{all}");
    }

    #[test]
    fn the_host_tor_client_is_mentioned_when_it_runs_or_failed() {
        let mut d = data();
        assert!(!text(&layout(&d, 0, None)).contains("HOST TOR"));
        d.tor = TorStatus {
            running: true,
            bootstrap: 100,
            problem: None,
        };
        assert!(text(&layout(&d, 0, None)).contains("HOST TOR: RUNNING, READY"));
        d.tor.bootstrap = 30;
        assert!(text(&layout(&d, 0, None)).contains("HOST TOR: RUNNING, STARTING, 30%"));
        d.tor = TorStatus {
            running: false,
            bootstrap: 0,
            problem: Some("starting tor: not found".into()),
        };
        assert!(
            text(&layout(&d, 0, None)).contains("HOST TOR: NOT RUNNING (starting tor: not found)")
        );
    }

    #[test]
    fn the_overview_lists_persisted_state_and_available_programs() {
        let all = text(&layout(&data(), 0, None));
        assert!(all.contains("PERSISTED VMS (1)   1.7G ON DISK"), "{all}");
        assert!(all.contains("STATE SAVED"), "{all}");
        assert!(all.contains("AVAILABLE AS VM (2)"), "{all}");
        assert!(all.contains("2.7G  1 RUNNING"), "{all}");
    }

    #[test]
    fn a_persisted_vm_says_which_mode_it_starts_in() {
        let mut entry = data().persisted.remove(0);
        entry.network = Some(NetMode::Offline);
        let line = persisted_line(&entry);
        assert!(line.text.contains("NET OFF"), "{}", line.text);
        assert_eq!(line.style, Style::Offline);
        entry.network = Some(NetMode::Tor);
        let line = persisted_line(&entry);
        assert!(line.text.contains("NET TOR"), "{}", line.text);
        assert_eq!(line.style, Style::Tor);
    }

    #[test]
    fn the_selection_marks_exactly_the_selected_vms_two_lines() {
        let lines = layout(&data(), 1, None);
        let marked: Vec<_> = lines.iter().filter(|l| l.selected).collect();
        assert_eq!(marked.len(), 2);
        assert!(
            marked[0].text.starts_with("> ALACRITTY") || marked[0].text.starts_with("> Alacritty")
        );
    }

    #[test]
    fn an_empty_overview_says_what_to_do() {
        let all = text(&layout(&VmOverviewData::default(), 0, None));
        assert!(all.contains("0/1 RUNNING"), "{all}");
        assert!(all.contains("START ONE FROM THE LAUNCHER"), "{all}");
        assert!(all.contains("PERSISTED VMS (0)"), "{all}");
        assert!(all.contains("NONE (THE HOST IS OFFLINE)"), "{all}");
        assert!(
            !layout(&VmOverviewData::default(), 0, None)
                .iter()
                .any(|l| !l.buttons.is_empty()),
            "no VM, nothing to switch"
        );
    }

    #[test]
    fn a_status_replaces_the_key_hints_as_an_error_or_a_notice() {
        let lines = layout(&data(), 0, Some(("COULD NOT SWITCH", false)));
        let last = lines.last().unwrap();
        assert_eq!(last.text, "COULD NOT SWITCH");
        assert_eq!(last.style, Style::Error);
        let lines = layout(&data(), 0, Some(("NEW TOR IDENTITY", true)));
        assert_eq!(lines.last().unwrap().style, Style::Tor);
    }

    #[test]
    fn long_lists_are_cut_with_a_count() {
        let mut d = data();
        d.programs = (0..12)
            .map(|i| ProgramEntry {
                name: format!("App {i}"),
                image_bytes: 0,
                running: 0,
            })
            .collect();
        let all = text(&layout(&d, 0, None));
        assert!(all.contains("+4 MORE"), "{all}");
    }

    #[test]
    fn the_route_line_fits_with_a_long_interface_name() {
        let route = HostRoute {
            uplink: Some("enx0123456789ab".into()),
            gateway: Some(Ipv4Addr::new(192, 168, 100, 254)),
        };
        let mut vm = vm(1, "Firefox", true, Link::Online);
        vm.ssh_port = 65535;
        let line = route_line(&vm, &route, false);
        assert!(line.text.ends_with("SSH :65535"), "{:?}", line.text);
        vm.link = Link::Tor(TorLink {
            running: true,
            bootstrap: 100,
            problem: None,
            flows: 256,
            identity: 99,
        });
        let line = route_line(&vm, &route, false);
        assert!(line.text.ends_with("SSH :65535"), "{:?}", line.text);
    }

    #[test]
    fn no_line_is_wider_than_the_overlay() {
        let mut d = data();
        d.running[0].name = "A Very Long Application Name Indeed".into();
        d.running[0].rss_bytes = 15 * 1024 * MIB;
        d.running[0].memory_mb = 16384;
        d.running[0].cpu_percent = 1234.0;
        d.running[0].uptime = Duration::from_secs(99 * 3600);
        d.route.uplink = Some("enx0123456789ab".into());
        d.tor = TorStatus {
            running: false,
            bootstrap: 0,
            problem: Some("x".repeat(200)),
        };
        for line in layout(&d, 0, Some((&"X".repeat(200), false))) {
            assert!(line.text.chars().count() <= LINE_CHARS, "{:?}", line.text);
        }
    }

    // --- buttons and clicks -------------------------------------------------

    fn buttons_of(d: &VmOverviewData, selected: usize) -> Vec<Button> {
        layout(d, selected, None)
            .into_iter()
            .find(|l| !l.buttons.is_empty())
            .expect("a button row")
            .buttons
    }

    #[test]
    fn the_buttons_show_the_selected_vms_mode() {
        let mut d = data();
        d.running[0].link = tor_link(100);
        let active = |b: &[Button]| -> Vec<Action> {
            b.iter().filter(|b| b.active).map(|b| b.action).collect()
        };
        assert_eq!(active(&buttons_of(&d, 0)), [Action::Mode(NetMode::Tor)]);
        assert_eq!(active(&buttons_of(&d, 1)), [Action::Mode(NetMode::Offline)]);
        d.running[1].link = Link::Switching {
            to: NetMode::Online,
        };
        assert_eq!(active(&buttons_of(&d, 1)), [Action::Mode(NetMode::Online)]);
    }

    #[test]
    fn new_identity_only_works_in_tor_mode() {
        let mut d = data();
        let identity = |b: Vec<Button>| {
            b.into_iter()
                .find(|b| b.action == Action::NewIdentity)
                .unwrap()
        };
        assert!(!identity(buttons_of(&d, 0)).enabled);
        d.running[0].link = tor_link(100);
        assert!(identity(buttons_of(&d, 0)).enabled);
        // The mode buttons never are disabled.
        assert!(
            buttons_of(&d, 0)
                .iter()
                .filter(|b| b.action != Action::NewIdentity)
                .all(|b| b.enabled)
        );
    }

    /// The middle of a button of the selected VM.
    fn centre_of(state: &VmOverviewState, action: Action) -> Point<i32, Logical> {
        let lines = state.lines();
        let tops = line_tops(&lines);
        for (line, top) in lines.iter().zip(tops) {
            for (rect, button) in button_rects(line, top) {
                if button.action == action {
                    return Point::from((rect.x + rect.w / 2, rect.y + rect.h / 2));
                }
            }
        }
        panic!("no such button");
    }

    fn open_state(d: VmOverviewData) -> VmOverviewState {
        let mut state = VmOverviewState::default();
        state.open(HashMap::new());
        state.set_data(d);
        state
    }

    #[test]
    fn a_click_on_a_button_reports_its_action() {
        let state = open_state(data());
        for action in [
            Action::Mode(NetMode::Online),
            Action::Mode(NetMode::Tor),
            Action::Mode(NetMode::Offline),
        ] {
            assert_eq!(
                state.hit_test(centre_of(&state, action)),
                Hit::Button(action)
            );
        }
    }

    #[test]
    fn a_click_on_a_disabled_button_does_nothing() {
        let state = open_state(data());
        assert_eq!(
            state.hit_test(centre_of(&state, Action::NewIdentity)),
            Hit::Panel
        );
    }

    #[test]
    fn a_click_on_a_vms_rows_selects_it() {
        let state = open_state(data());
        let lines = state.lines();
        let tops = line_tops(&lines);
        let row_of = |id: u64| -> Vec<i32> {
            lines
                .iter()
                .zip(&tops)
                .filter(|(l, _)| l.vm == Some(id))
                .map(|(_, &top)| top + 3)
                .collect()
        };
        for id in [1, 2] {
            // Both of its lines count.
            for y in row_of(id) {
                assert_eq!(state.hit_test(Point::from((100, y))), Hit::Select(id));
            }
        }
    }

    #[test]
    fn a_click_elsewhere_is_the_panel_or_outside_it() {
        let state = open_state(data());
        // The title bar.
        assert_eq!(state.hit_test(Point::from((100, PADDING + 3))), Hit::Panel);
        assert_eq!(state.hit_test(Point::from((-1, 10))), Hit::Outside);
        assert_eq!(state.hit_test(Point::from((10, -1))), Hit::Outside);
        let size = state.logical_size();
        assert_eq!(state.hit_test(Point::from((size.w, 10))), Hit::Outside);
        assert_eq!(state.hit_test(Point::from((10, size.h))), Hit::Outside);
    }

    #[test]
    fn buttons_stay_inside_the_overlay() {
        let state = open_state(data());
        let lines = state.lines();
        for (line, top) in lines.iter().zip(line_tops(&lines)) {
            for (rect, _) in button_rects(line, top) {
                assert!(rect.x >= 0 && rect.x + rect.w <= WIDTH, "{rect:?}");
                assert!(
                    rect.y >= top && rect.y + rect.h <= top + line.height(),
                    "{rect:?}"
                );
            }
        }
    }

    // --- selection ------------------------------------------------------------

    #[test]
    fn selection_follows_the_vm_when_the_list_changes() {
        let mut state = VmOverviewState::default();
        state.set_data(data());
        state.move_selection(1);
        assert_eq!(state.selected().unwrap().id, 2);

        // A VM before the selected one goes away: still on the same VM.
        let mut d = data();
        d.running.remove(0);
        state.set_data(d);
        assert_eq!(state.selected().unwrap().id, 2);

        // The selected VM goes away: back to the first.
        let mut d = data();
        d.running.remove(1);
        state.set_data(d);
        assert_eq!(state.selected().unwrap().id, 1);
    }

    #[test]
    fn moving_the_selection_wraps_and_survives_an_empty_list() {
        let mut state = VmOverviewState::default();
        state.move_selection(1);
        assert!(state.selected().is_none());
        state.set_data(data());
        state.move_selection(-1);
        assert_eq!(state.selected().unwrap().id, 2);
        state.move_selection(1);
        assert_eq!(state.selected().unwrap().id, 1);
    }

    #[test]
    fn selecting_a_vm_that_is_not_running_is_ignored() {
        let mut state = open_state(data());
        state.select(2);
        assert_eq!(state.selected().unwrap().id, 2);
        state.select(99);
        assert_eq!(state.selected().unwrap().id, 2);
    }

    #[test]
    fn the_buffer_exists_only_while_the_overlay_is_open() {
        let mut state = VmOverviewState::default();
        assert!(state.ensure_buffer().is_none());
        state.open(HashMap::new());
        state.set_data(data());
        let size = state.logical_size();
        assert_eq!(size.w, WIDTH);
        assert!(state.ensure_buffer().is_some());
        state.close();
        assert!(state.ensure_buffer().is_none());
    }
}
