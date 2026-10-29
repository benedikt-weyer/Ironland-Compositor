//! VM overview: an overlay (`super+o` by default) listing the VMs that are
//! running with their resource use and how they reach the internet, the
//! persistent VMs' state on disk, and every program that can be started in a
//! VM. The internet of a running VM can be switched off (and on again) from
//! here; for a persistent VM the choice is kept in its metadata (see
//! [`crate::shell::vm_state::PersistMeta::network_enabled`]) and applies the
//! next time it starts.
//!
//! The data comes from `AnvilState::refresh_vm_overview` (`crate::shell::vm`),
//! once a second while the overlay is open. Everything in this file is pure:
//! [`layout`] turns the data into lines of text, [`VmOverviewState`] rasterizes
//! them into a `MemoryRenderBuffer` the way the launcher does. Like the
//! launcher, the overlay grabs the keyboard while it is visible.

use std::{collections::HashMap, net::Ipv4Addr, time::Duration};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::{Logical, Size, Transform},
};

use crate::font::{Canvas, GLYPH_HEIGHT, GLYPH_WIDTH};

/// Overlay width in logical pixels.
const WIDTH: i32 = 1040;
const PADDING: i32 = 12;
const FONT_SCALE: i32 = 2;
const LINE_HEIGHT: i32 = 22;
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
const COLOR_TEXT: [u8; 4] = rgba(235, 230, 225, 255);
const COLOR_MUTED: [u8; 4] = rgba(150, 140, 130, 255);
const COLOR_ONLINE: [u8; 4] = rgba(130, 205, 130, 255);
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

/// A running VM's connection to the internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Online,
    Offline,
    /// The change to `online` has been asked for and is not done yet.
    Switching {
        online: bool,
    },
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
    /// Whether the VM gets the internet when it starts; `None` for state
    /// without readable metadata.
    pub internet: Option<bool>,
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
    Offline,
    Error,
}

impl Style {
    fn color(self) -> [u8; 4] {
        match self {
            Style::Title | Style::Header | Style::Text => COLOR_TEXT,
            Style::Muted => COLOR_MUTED,
            Style::Online => COLOR_ONLINE,
            Style::Offline => COLOR_OFFLINE,
            Style::Error => COLOR_ERROR,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    text: String,
    style: Style,
    /// Part of the selected VM.
    selected: bool,
}

impl Line {
    fn new(style: Style, text: impl Into<String>) -> Self {
        Self {
            text: clip(&text.into(), LINE_CHARS),
            style,
            selected: false,
        }
    }

    fn blank() -> Self {
        Self::new(Style::Text, "")
    }
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
        ..Line::new(Style::Text, text)
    }
}

/// The line under a running VM saying how it reaches the internet.
fn route_line(vm: &RunningVm, route: &HostRoute, selected: bool) -> Line {
    let ssh = format!(":{}", vm.ssh_port);
    let (style, text) = match vm.link {
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
        Link::Switching { online } => (
            Style::Muted,
            format!("    NET SWITCHING {}...", if online { "ON" } else { "OFF" }),
        ),
    };
    Line {
        selected,
        ..Line::new(style, text)
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
    let net = match entry.internet {
        Some(true) => "NET ON",
        Some(false) => "NET OFF",
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
    let style = if entry.internet == Some(false) {
        Style::Offline
    } else {
        Style::Text
    };
    Line::new(style, text)
}

/// Everything the overlay says, top to bottom. `selected` is the index of the
/// highlighted running VM and `status` replaces the key hints at the bottom.
fn layout(data: &VmOverviewData, selected: usize, status: Option<&str>) -> Vec<Line> {
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
        format!("HOST UPLINK: {uplink}. VMS REACH IT THROUGH QEMU'S NAT."),
    ));

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
        Some(message) => Line::new(Style::Error, message),
        None if data.running.is_empty() => Line::new(Style::Muted, "ESC: CLOSE"),
        None => Line::new(
            Style::Muted,
            "UP/DOWN: SELECT   N OR ENTER: INTERNET ON/OFF   ESC: CLOSE",
        ),
    });
    lines
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct VmOverviewState {
    visible: bool,
    data: VmOverviewData,
    /// The VM the selection follows as the list changes.
    selected_id: Option<u64>,
    /// Shown in place of the key hints until the next key.
    status: Option<String>,
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

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
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

    pub fn selected(&self) -> Option<&RunningVm> {
        self.data.running.get(self.selected_index())
    }

    fn lines(&self) -> Vec<Line> {
        layout(&self.data, self.selected_index(), self.status.as_deref())
    }

    /// The logical size of the overlay, so callers can center it on the output.
    pub fn logical_size(&self) -> Size<i32, Logical> {
        Size::from((WIDTH, self.lines().len() as i32 * LINE_HEIGHT + 2 * PADDING))
    }

    fn rasterize(&self) -> Canvas {
        let lines = self.lines();
        let size =
            Size::<i32, Logical>::from((WIDTH, lines.len() as i32 * LINE_HEIGHT + 2 * PADDING));
        let mut canvas = Canvas::new(size.w as usize, size.h as usize, COLOR_BACKGROUND);
        for (i, line) in lines.iter().enumerate() {
            let y = PADDING + i as i32 * LINE_HEIGHT;
            match line.style {
                Style::Header | Style::Title => {
                    canvas.fill_rect(0, y, size.w, LINE_HEIGHT, COLOR_HEADER_BG)
                }
                _ if line.selected => {
                    canvas.fill_rect(0, y, size.w, LINE_HEIGHT, COLOR_SELECTED_BG)
                }
                _ => {}
            }
            canvas.draw_text(
                PADDING,
                y + (LINE_HEIGHT - GLYPH_HEIGHT as i32 * FONT_SCALE) / 2,
                &line.text,
                FONT_SCALE,
                line.style.color(),
            );
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
                internet: Some(true),
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
    fn the_overview_lists_persisted_state_and_available_programs() {
        let all = text(&layout(&data(), 0, None));
        assert!(all.contains("PERSISTED VMS (1)   1.7G ON DISK"), "{all}");
        assert!(all.contains("STATE SAVED"), "{all}");
        assert!(all.contains("AVAILABLE AS VM (2)"), "{all}");
        assert!(all.contains("2.7G  1 RUNNING"), "{all}");
    }

    #[test]
    fn a_persisted_vm_without_internet_says_so() {
        let mut entry = data().persisted.remove(0);
        entry.internet = Some(false);
        let line = persisted_line(&entry);
        assert!(line.text.contains("NET OFF"), "{}", line.text);
        assert_eq!(line.style, Style::Offline);
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
    }

    #[test]
    fn a_status_replaces_the_key_hints() {
        let lines = layout(&data(), 0, Some("COULD NOT SWITCH"));
        let last = lines.last().unwrap();
        assert_eq!(last.text, "COULD NOT SWITCH");
        assert_eq!(last.style, Style::Error);
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
        for line in layout(&d, 0, Some(&"X".repeat(200))) {
            assert!(line.text.chars().count() <= LINE_CHARS, "{:?}", line.text);
        }
    }

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
