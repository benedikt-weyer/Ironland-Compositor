#![allow(clippy::too_many_arguments)]

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Color32F, ImportAll, ImportMem, Renderer, Texture,
            element::{
                AsRenderElements, Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::WaylandSurfaceRenderElement,
            },
        },
    },
    input::pointer::CursorImageStatus,
    render_elements,
    utils::{Logical, Physical, Point, Scale, Size, Transform},
};

use std::collections::HashSet;

use crate::{font::Canvas, launcher::DesktopEntry};
#[cfg(feature = "debug")]
use smithay::{
    backend::renderer::{
        Frame,
        element::{Element, Id, RenderElement},
        utils::CommitCounter,
    },
    utils::{Buffer, Rectangle, user_data::UserDataMap},
};

pub static CLEAR_COLOR: Color32F = Color32F::new(0.8, 0.8, 0.9, 1.0);
pub static CLEAR_COLOR_FULLSCREEN: Color32F = Color32F::new(0.0, 0.0, 0.0, 0.0);

pub struct PointerElement {
    buffer: Option<MemoryRenderBuffer>,
    status: CursorImageStatus,
}

impl Default for PointerElement {
    fn default() -> Self {
        Self {
            buffer: Default::default(),
            status: CursorImageStatus::default_named(),
        }
    }
}

impl PointerElement {
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }

    pub fn set_buffer(&mut self, buffer: MemoryRenderBuffer) {
        self.buffer = Some(buffer);
    }
}

render_elements! {
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer> std::fmt::Debug for PointerRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::Memory(arg0) => f.debug_tuple("Memory").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

impl<T: Texture + Clone + Send + 'static, R> AsRenderElements<R> for PointerElement
where
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
    type RenderElement = PointerRenderElement<R>;
    fn render_elements<E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        E: From<PointerRenderElement<R>>,
    {
        match &self.status {
            CursorImageStatus::Hidden => vec![],
            // Always render `Default` for a named shape.
            CursorImageStatus::Named(_) => {
                if let Some(buffer) = self.buffer.as_ref() {
                    vec![
                        PointerRenderElement::<R>::from(
                            MemoryRenderBufferRenderElement::from_buffer(
                                renderer,
                                location.to_f64(),
                                buffer,
                                None,
                                None,
                                None,
                                Kind::Cursor,
                            )
                            .expect("Lost system pointer buffer"),
                        )
                        .into(),
                    ]
                } else {
                    vec![]
                }
            }
            CursorImageStatus::Surface(surface) => {
                let elements: Vec<PointerRenderElement<R>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface,
                        location,
                        scale,
                        alpha,
                        Kind::Cursor,
                    );
                elements.into_iter().map(E::from).collect()
            }
        }
    }
}

#[cfg(feature = "debug")]
pub static FPS_NUMBERS_PNG: &[u8] = include_bytes!("../../resources/numbers.png");

#[cfg(feature = "debug")]
#[derive(Debug, Clone)]
pub struct FpsElement<T: Texture> {
    id: Id,
    value: u32,
    texture: T,
    commit_counter: CommitCounter,
}

#[cfg(feature = "debug")]
impl<T: Texture> FpsElement<T> {
    pub fn new(texture: T) -> Self {
        FpsElement {
            id: Id::new(),
            texture,
            value: 0,
            commit_counter: CommitCounter::default(),
        }
    }

    pub fn update_fps(&mut self, fps: u32) {
        if self.value != fps {
            self.value = fps;
            self.commit_counter.increment();
        }
    }
}

#[cfg(feature = "debug")]
impl<T> Element for FpsElement<T>
where
    T: Texture + 'static,
{
    fn id(&self) -> &Id {
        &self.id
    }

    fn location(&self, _scale: Scale<f64>) -> Point<i32, Physical> {
        (0, 0).into()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        let digits = if self.value < 10 {
            1
        } else if self.value < 100 {
            2
        } else {
            3
        };
        Rectangle::from_size((24 * digits, 35).into()).to_f64()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let digits = if self.value < 10 {
            1
        } else if self.value < 100 {
            2
        } else {
            3
        };
        Rectangle::from_size((24 * digits, 35).into()).to_physical_precise_round(scale)
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }
}

#[cfg(feature = "debug")]
impl<R> RenderElement<R> for FpsElement<R::TextureId>
where
    R: Renderer + ImportAll,
    R::TextureId: 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        // FIXME: respect the src for cropping
        let scale = dst.size.to_f64() / self.src().size;
        let value_str = std::cmp::min(self.value, 999).to_string();
        let mut offset: Point<f64, Physical> = Point::from((0.0, 0.0));
        for digit in value_str.chars().map(|d| d.to_digit(10).unwrap()) {
            let digit_location = dst.loc.to_f64() + offset;
            let digit_size = Size::<i32, Logical>::from((22, 35)).to_f64().to_physical(scale);
            let dst = Rectangle::new(
                digit_location.to_i32_round(),
                ((digit_size.to_point() + digit_location).to_i32_round() - digit_location.to_i32_round())
                    .to_size(),
            );
            let damage = damage
                .iter()
                .cloned()
                .flat_map(|x| x.intersection(dst))
                .map(|mut x| {
                    x.loc -= dst.loc;
                    x
                })
                .collect::<Vec<_>>();
            let texture_src: Rectangle<i32, Buffer> = match digit {
                9 => Rectangle::from_size((22, 35).into()),
                6 => Rectangle::new((22, 0).into(), (22, 35).into()),
                3 => Rectangle::new((44, 0).into(), (22, 35).into()),
                1 => Rectangle::new((66, 0).into(), (22, 35).into()),
                8 => Rectangle::new((0, 35).into(), (22, 35).into()),
                0 => Rectangle::new((22, 35).into(), (22, 35).into()),
                2 => Rectangle::new((44, 35).into(), (22, 35).into()),
                7 => Rectangle::new((0, 70).into(), (22, 35).into()),
                4 => Rectangle::new((22, 70).into(), (22, 35).into()),
                5 => Rectangle::new((44, 70).into(), (22, 35).into()),
                _ => unreachable!(),
            };

            frame.render_texture_from_to(
                &self.texture,
                texture_src.to_f64(),
                dst,
                &damage,
                &[],
                Transform::Normal,
                1.0,
            )?;
            offset += Point::from((24.0, 0.0)).to_physical(scale);
        }

        Ok(())
    }
}

const LAUNCHER_WIDTH: i32 = 480;
const LAUNCHER_ROW_HEIGHT: i32 = 26;
const LAUNCHER_HEADER_HEIGHT: i32 = 36;
const LAUNCHER_TAB_HEIGHT: i32 = 28;
const LAUNCHER_PADDING: i32 = 10;
const LAUNCHER_FOOTER_LINE_HEIGHT: i32 = 22;
const LAUNCHER_FOOTER_PADDING: i32 = 6;
const LAUNCHER_MAX_ROWS: usize = 8;
const LAUNCHER_FONT_SCALE: i32 = 2;

const COLOR_BACKGROUND: [u8; 4] = [40, 34, 30, 255];
const COLOR_HEADER_BG: [u8; 4] = [70, 60, 52, 255];
const COLOR_SELECTED_BG: [u8; 4] = [120, 90, 40, 255];
const COLOR_TEXT: [u8; 4] = [235, 230, 225, 255];
const COLOR_MUTED_TEXT: [u8; 4] = [150, 140, 130, 255];
const COLOR_ERROR_TEXT: [u8; 4] = [235, 120, 110, 255];

/// The launcher's views, switched with the Left/Right arrow keys: ordinary
/// applications in the middle, with the programs set up for throw-away VMs to
/// the left and for persistent VMs to the right. The VM views list only the
/// programs that have a VM configured (`vm.programs`, see
/// [`ironland_config::VmSettings`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LauncherTab {
    /// Every installed application; Enter starts it on the host.
    #[default]
    Apps,
    /// Programs with a VM set up; Enter starts one in a throw-away VM.
    Vm,
    /// Programs with a VM set up; Enter starts one in a persistent VM.
    Persistent,
}

impl LauncherTab {
    /// Left to right, as drawn.
    const ALL: [LauncherTab; 3] = [LauncherTab::Vm, LauncherTab::Apps, LauncherTab::Persistent];

    fn label(self) -> &'static str {
        match self {
            LauncherTab::Vm => "VM",
            LauncherTab::Apps => "Apps",
            LauncherTab::Persistent => "Persistent",
        }
    }

    fn position(self) -> usize {
        Self::ALL.iter().position(|&tab| tab == self).unwrap_or(1)
    }

    /// The neighbouring tab `delta` steps to the right (negative: left),
    /// stopping at the ends.
    fn shifted(self, delta: i32) -> LauncherTab {
        let last = Self::ALL.len() as i32 - 1;
        Self::ALL[(self.position() as i32 + delta).clamp(0, last) as usize]
    }

    fn lists_only_vm_programs(self) -> bool {
        self != LauncherTab::Apps
    }
}

/// One line of the launcher's footer, and in which style.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FooterLine {
    /// Key hints (normal colour).
    Hint(&'static str),
    /// Key hints with VM launching unavailable (no VM programs; muted).
    HintNoVm(&'static str),
    /// Why the last launch attempt failed (error colour).
    Error(String),
}

/// The launcher's bottom lines: a pending error beats the key hints, and the
/// hints say plainly when VM launching can't work yet. Each line must fit the
/// overlay (about 38 characters).
fn footer_lines(tab: LauncherTab, vm_available: bool, status: Option<&str>) -> Vec<FooterLine> {
    match status {
        Some(message) => vec![FooterLine::Error(message.to_string())],
        None if !vm_available => vec![FooterLine::HintNoVm("Enter: launch   VM: none set up")],
        None => match tab {
            LauncherTab::Apps => vec![
                FooterLine::Hint("Enter: launch on this computer"),
                FooterLine::Hint("Left: VM   Right: persistent VM"),
            ],
            LauncherTab::Vm => vec![
                FooterLine::Hint("Enter: launch in a throw-away VM"),
                FooterLine::Hint("Right: back to apps"),
            ],
            LauncherTab::Persistent => vec![
                FooterLine::Hint("Enter: launch in a persistent VM"),
                FooterLine::Hint("Left: back to apps"),
            ],
        },
    }
}

/// State for the built-in application launcher: a search query over the
/// user's XDG desktop entries, rendered as an on-screen overlay.
#[derive(Debug)]
pub struct LauncherState {
    pub visible: bool,
    query: String,
    entries: Vec<DesktopEntry>,
    filtered: Vec<usize>,
    selected: usize,
    tab: LauncherTab,
    /// Programs (desktop entry ids without `.desktop`) that have a VM set up:
    /// all the VM tabs list. Set by the caller when opening.
    vm_programs: HashSet<String>,
    /// Error from the last launch attempt, shown instead of the key hints
    /// until the next keystroke.
    status: Option<String>,
    buffer: Option<MemoryRenderBuffer>,
    dirty: bool,
}

impl Default for LauncherState {
    fn default() -> Self {
        Self {
            visible: false,
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            tab: LauncherTab::Apps,
            vm_programs: HashSet::new(),
            status: None,
            buffer: None,
            dirty: true,
        }
    }
}

impl LauncherState {
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Opens the launcher, (re-)scanning desktop entries so newly installed
    /// applications show up without restarting the compositor.
    pub fn open(&mut self) {
        self.entries = crate::launcher::scan_desktop_entries();
        self.query.clear();
        self.tab = LauncherTab::Apps;
        self.refresh_filter();
        self.visible = true;
    }

    /// Sets which programs have a VM, from the config (call before
    /// [`Self::open`]). Without any, the VM tabs are unavailable.
    pub fn set_vm_programs(&mut self, programs: impl IntoIterator<Item = String>) {
        self.vm_programs = programs.into_iter().collect();
    }

    /// Whether any program can be started in a VM, i.e. the VM tabs exist.
    pub fn vm_available(&self) -> bool {
        !self.vm_programs.is_empty()
    }

    pub fn tab(&self) -> LauncherTab {
        self.tab
    }

    /// Moves to the tab `delta` places to the right (negative: left). Does
    /// nothing while no program has a VM.
    pub fn switch_tab(&mut self, delta: i32) {
        if !self.vm_available() {
            return;
        }
        let tab = self.tab.shifted(delta);
        if tab != self.tab {
            self.tab = tab;
            self.refresh_filter();
        }
    }

    fn is_vm_program(&self, entry: &DesktopEntry) -> bool {
        self.vm_programs.contains(ironland_config::program_id(&entry.id))
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.buffer = None;
        self.dirty = true;
    }

    pub fn toggle(&mut self) {
        if self.visible {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refresh_filter();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.refresh_filter();
    }

    fn refresh_filter(&mut self) {
        let mut filtered = crate::launcher::filter_entries(&self.entries, &self.query);
        if self.tab.lists_only_vm_programs() {
            filtered.retain(|&i| self.is_vm_program(&self.entries[i]));
        }
        self.filtered = filtered;
        self.selected = 0;
        self.status = None;
        self.dirty = true;
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        self.status = None;
        let len = self.filtered.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
        self.dirty = true;
    }

    /// Closes the launcher and returns the currently selected entry, if any.
    pub fn activate(&mut self) -> Option<DesktopEntry> {
        let entry = self
            .filtered
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
            .cloned();
        self.close();
        entry
    }

    /// The currently highlighted entry, leaving the launcher open.
    pub fn selected_entry(&self) -> Option<DesktopEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
            .cloned()
    }

    /// Shows `message` in place of the key hints (until the next keystroke),
    /// e.g. why a launch failed.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.dirty = true;
    }

    fn footer_lines(&self) -> Vec<FooterLine> {
        footer_lines(self.tab, self.vm_available(), self.status.as_deref())
    }

    /// The tab strip is only there once there is a VM tab to switch to.
    fn tab_strip_height(&self) -> i32 {
        if self.vm_available() { LAUNCHER_TAB_HEIGHT } else { 0 }
    }

    fn footer_height(&self) -> i32 {
        self.footer_lines().len() as i32 * LAUNCHER_FOOTER_LINE_HEIGHT + LAUNCHER_FOOTER_PADDING
    }

    /// The logical size of the overlay, so callers can center it on the output.
    pub fn logical_size(&self) -> Size<i32, Logical> {
        let rows = self.filtered.len().clamp(1, LAUNCHER_MAX_ROWS);
        let height = LAUNCHER_HEADER_HEIGHT
            + self.tab_strip_height()
            + rows as i32 * LAUNCHER_ROW_HEIGHT
            + LAUNCHER_PADDING
            + self.footer_height();
        Size::from((LAUNCHER_WIDTH, height))
    }

    fn rasterize(&self) -> Canvas {
        let size = self.logical_size();
        let mut canvas = Canvas::new(size.w as usize, size.h as usize, COLOR_BACKGROUND);

        canvas.fill_rect(0, 0, size.w, LAUNCHER_HEADER_HEIGHT, COLOR_HEADER_BG);
        let prompt = format!("> {}", self.query);
        canvas.draw_text(
            LAUNCHER_PADDING,
            (LAUNCHER_HEADER_HEIGHT - GLYPH_LINE_HEIGHT * LAUNCHER_FONT_SCALE) / 2,
            &prompt,
            LAUNCHER_FONT_SCALE,
            COLOR_TEXT,
        );

        let list_top = LAUNCHER_HEADER_HEIGHT + self.tab_strip_height();
        if self.vm_available() {
            let tab_width = size.w / LauncherTab::ALL.len() as i32;
            for (i, tab) in LauncherTab::ALL.into_iter().enumerate() {
                let x = i as i32 * tab_width;
                let active = tab == self.tab;
                if active {
                    canvas.fill_rect(x, LAUNCHER_HEADER_HEIGHT, tab_width, LAUNCHER_TAB_HEIGHT, COLOR_SELECTED_BG);
                }
                let label = tab.label();
                let text_width = label.chars().count() as i32
                    * (crate::font::GLYPH_WIDTH as i32 + 1)
                    * LAUNCHER_FONT_SCALE;
                canvas.draw_text(
                    x + (tab_width - text_width) / 2,
                    LAUNCHER_HEADER_HEIGHT
                        + (LAUNCHER_TAB_HEIGHT - GLYPH_LINE_HEIGHT * LAUNCHER_FONT_SCALE) / 2,
                    label,
                    LAUNCHER_FONT_SCALE,
                    if active { COLOR_TEXT } else { COLOR_MUTED_TEXT },
                );
            }
        }

        if self.filtered.is_empty() {
            let message = match (self.tab, self.vm_programs.is_empty()) {
                (LauncherTab::Apps, _) => "No matching applications",
                (_, true) => "No programs set up for VMs",
                _ => "No matching VM programs",
            };
            canvas.draw_text(
                LAUNCHER_PADDING,
                list_top + LAUNCHER_PADDING,
                message,
                LAUNCHER_FONT_SCALE,
                COLOR_MUTED_TEXT,
            );
        } else {
            for (row, &entry_idx) in self.filtered.iter().take(LAUNCHER_MAX_ROWS).enumerate() {
                let row_y = list_top + row as i32 * LAUNCHER_ROW_HEIGHT;
                if row == self.selected {
                    canvas.fill_rect(0, row_y, size.w, LAUNCHER_ROW_HEIGHT, COLOR_SELECTED_BG);
                }
                let name = &self.entries[entry_idx].name;
                let max_chars = ((size.w - LAUNCHER_PADDING * 2)
                    / ((crate::font::GLYPH_WIDTH as i32 + 1) * LAUNCHER_FONT_SCALE))
                    as usize;
                let display = truncate(name, max_chars);
                canvas.draw_text(
                    LAUNCHER_PADDING,
                    row_y + (LAUNCHER_ROW_HEIGHT - GLYPH_LINE_HEIGHT * LAUNCHER_FONT_SCALE) / 2,
                    &display,
                    LAUNCHER_FONT_SCALE,
                    COLOR_TEXT,
                );
            }
        }

        let footer_height = self.footer_height();
        let footer_y = size.h - footer_height;
        canvas.fill_rect(0, footer_y, size.w, footer_height, COLOR_HEADER_BG);
        let max_chars = ((size.w - LAUNCHER_PADDING * 2)
            / ((crate::font::GLYPH_WIDTH as i32 + 1) * LAUNCHER_FONT_SCALE)) as usize;
        for (i, line) in self.footer_lines().into_iter().enumerate() {
            let (text, color) = match line {
                FooterLine::Hint(text) => (text.to_string(), COLOR_TEXT),
                FooterLine::HintNoVm(text) => (text.to_string(), COLOR_MUTED_TEXT),
                FooterLine::Error(message) => (message, COLOR_ERROR_TEXT),
            };
            canvas.draw_text(
                LAUNCHER_PADDING,
                footer_y
                    + LAUNCHER_FOOTER_PADDING / 2
                    + i as i32 * LAUNCHER_FOOTER_LINE_HEIGHT
                    + (LAUNCHER_FOOTER_LINE_HEIGHT - GLYPH_LINE_HEIGHT * LAUNCHER_FONT_SCALE) / 2,
                &truncate(&text, max_chars),
                LAUNCHER_FONT_SCALE,
                color,
            );
        }

        canvas
    }

    /// Returns the memory buffer to render, rebuilding it if the launcher
    /// state has changed since the last frame.
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

const GLYPH_LINE_HEIGHT: i32 = crate::font::GLYPH_HEIGHT as i32;

const WORKSPACE_DOT_SIZE: i32 = 12;
const WORKSPACE_DOT_GAP: i32 = 10;
const WORKSPACE_DOT_PADDING: i32 = 10;
const WORKSPACE_DOT_ACTIVE: [u8; 4] = [235, 230, 225, 255];
const WORKSPACE_DOT_INACTIVE: [u8; 4] = [110, 100, 92, 255];
const WORKSPACE_DOT_BACKGROUND: [u8; 4] = [30, 26, 22, 220];

/// The logical size of the workspace dot overlay for `count` workspaces.
pub fn workspace_overlay_size(count: usize) -> Size<i32, Logical> {
    let count = count.max(1) as i32;
    let width = WORKSPACE_DOT_PADDING * 2 + count * WORKSPACE_DOT_SIZE + (count - 1).max(0) * WORKSPACE_DOT_GAP;
    let height = WORKSPACE_DOT_PADDING * 2 + WORKSPACE_DOT_SIZE;
    Size::from((width, height))
}

/// Rasterizes a row of dots - one per workspace, the active one highlighted -
/// like GNOME's workspace switcher overlay.
pub fn workspace_overlay_buffer(active: usize, count: usize) -> MemoryRenderBuffer {
    let size = workspace_overlay_size(count);
    let mut canvas = Canvas::new(size.w as usize, size.h as usize, WORKSPACE_DOT_BACKGROUND);
    for i in 0..count.max(1) {
        let x = WORKSPACE_DOT_PADDING + i as i32 * (WORKSPACE_DOT_SIZE + WORKSPACE_DOT_GAP);
        let color = if i == active { WORKSPACE_DOT_ACTIVE } else { WORKSPACE_DOT_INACTIVE };
        canvas.fill_rect(x, WORKSPACE_DOT_PADDING, WORKSPACE_DOT_SIZE, WORKSPACE_DOT_SIZE, color);
    }
    MemoryRenderBuffer::from_slice(
        &canvas.pixels,
        Fourcc::Argb8888,
        (canvas.width as i32, canvas.height as i32),
        1,
        Transform::Normal,
        None,
    )
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(2);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("..");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint_text(line: &FooterLine) -> &str {
        match line {
            FooterLine::Hint(t) | FooterLine::HintNoVm(t) => t,
            FooterLine::Error(t) => t,
        }
    }

    #[test]
    fn footer_hints_explain_each_tab() {
        let text = |tab| {
            footer_lines(tab, true, None)
                .iter()
                .map(hint_text)
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        let apps = text(LauncherTab::Apps);
        assert!(apps.contains("enter") && apps.contains("left") && apps.contains("right"));
        assert!(apps.contains("persistent"));
        let vm = text(LauncherTab::Vm);
        assert!(vm.contains("throw-away") && vm.contains("right"));
        let persistent = text(LauncherTab::Persistent);
        assert!(persistent.contains("persistent") && persistent.contains("left"));
        for tab in LauncherTab::ALL {
            assert!(footer_lines(tab, true, None).iter().all(|l| matches!(l, FooterLine::Hint(_))));
        }
    }

    #[test]
    fn footer_says_when_no_vm_is_set_up() {
        let lines = footer_lines(LauncherTab::Apps, false, None);
        assert!(matches!(lines.as_slice(), [FooterLine::HintNoVm(_)]));
    }

    #[test]
    fn a_launch_error_replaces_the_hints() {
        assert_eq!(
            footer_lines(LauncherTab::Vm, true, Some("boom")),
            vec![FooterLine::Error("boom".into())]
        );
    }

    #[test]
    fn footer_lines_fit_the_launcher_width() {
        let per_char = (crate::font::GLYPH_WIDTH as i32 + 1) * LAUNCHER_FONT_SCALE;
        let max_chars = ((LAUNCHER_WIDTH - LAUNCHER_PADDING * 2) / per_char) as usize;
        for tab in LauncherTab::ALL {
            for vm_available in [true, false] {
                for line in &footer_lines(tab, vm_available, None) {
                    assert!(hint_text(line).chars().count() <= max_chars, "{line:?} is too wide");
                }
            }
        }
    }

    #[test]
    fn tab_labels_fit_their_third_of_the_overlay() {
        let per_char = (crate::font::GLYPH_WIDTH as i32 + 1) * LAUNCHER_FONT_SCALE;
        for tab in LauncherTab::ALL {
            assert!(tab.label().chars().count() as i32 * per_char < LAUNCHER_WIDTH / 3);
        }
    }

    fn entry(id: &str, name: &str) -> DesktopEntry {
        DesktopEntry { id: id.into(), name: name.into(), exec: name.into(), terminal: false }
    }

    /// A launcher showing Alacritty, Firefox and foot, of which only
    /// Alacritty and foot have a VM set up.
    fn launcher_with_vm_programs() -> LauncherState {
        let mut launcher = LauncherState {
            entries: vec![
                entry("Alacritty.desktop", "Alacritty"),
                entry("firefox.desktop", "Firefox"),
                entry("foot.desktop", "foot"),
            ],
            ..LauncherState::default()
        };
        launcher.set_vm_programs(["Alacritty".to_string(), "foot".to_string()]);
        launcher.refresh_filter();
        launcher
    }

    fn names(launcher: &LauncherState) -> Vec<String> {
        launcher
            .filtered
            .iter()
            .map(|&i| launcher.entries[i].name.clone())
            .collect()
    }

    #[test]
    fn the_vm_tabs_list_only_programs_that_have_a_vm() {
        let mut launcher = launcher_with_vm_programs();
        assert_eq!(launcher.tab(), LauncherTab::Apps);
        assert_eq!(names(&launcher), ["Alacritty", "Firefox", "foot"]);

        launcher.switch_tab(-1);
        assert_eq!(launcher.tab(), LauncherTab::Vm);
        assert_eq!(names(&launcher), ["Alacritty", "foot"]);

        launcher.switch_tab(1);
        launcher.switch_tab(1);
        assert_eq!(launcher.tab(), LauncherTab::Persistent);
        assert_eq!(names(&launcher), ["Alacritty", "foot"]);
    }

    #[test]
    fn arrows_walk_vm_then_apps_then_persistent_and_stop_at_the_ends() {
        let mut launcher = launcher_with_vm_programs();
        launcher.switch_tab(-1);
        launcher.switch_tab(-1);
        assert_eq!(launcher.tab(), LauncherTab::Vm);
        launcher.switch_tab(1);
        assert_eq!(launcher.tab(), LauncherTab::Apps);
        launcher.switch_tab(1);
        launcher.switch_tab(1);
        assert_eq!(launcher.tab(), LauncherTab::Persistent);
    }

    #[test]
    fn typing_filters_within_the_current_tab() {
        let mut launcher = launcher_with_vm_programs();
        launcher.switch_tab(-1);
        launcher.push_char('f');
        assert_eq!(names(&launcher), ["foot"]);
        launcher.switch_tab(1);
        assert_eq!(names(&launcher), ["Firefox", "foot"]);
    }

    #[test]
    fn without_vm_programs_there_are_no_tabs() {
        let mut launcher = launcher_with_vm_programs();
        launcher.set_vm_programs([]);
        assert!(!launcher.vm_available());
        launcher.switch_tab(-1);
        assert_eq!(launcher.tab(), LauncherTab::Apps);
        assert_eq!(names(&launcher).len(), 3);
    }

    #[test]
    fn the_overlay_grows_by_the_tab_strip_once_a_vm_program_exists() {
        let mut launcher = LauncherState::default();
        let plain = launcher.logical_size().h;
        launcher.set_vm_programs(["foot".to_string()]);
        assert!(launcher.logical_size().h > plain);
    }

    #[test]
    fn status_is_cleared_by_typing_and_navigation() {
        let mut launcher = LauncherState::default();
        launcher.set_status("nope");
        launcher.push_char('a');
        assert!(launcher.status.is_none());

        launcher.entries = vec![entry("a", "A"), entry("b", "B")];
        launcher.filtered = vec![0, 1];
        launcher.set_status("nope");
        launcher.move_selection(1);
        assert!(launcher.status.is_none());
        assert_eq!(launcher.selected_entry().unwrap().name, "B");
    }
}
