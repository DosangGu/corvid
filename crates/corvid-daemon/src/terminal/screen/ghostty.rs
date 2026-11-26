use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use corvid_core::Viewport;
use corvid_protocol::ansi_screen::{
    Cell as FrameCell, Color, Cursor, Hyperlink, MAX_HYPERLINK_URI_BYTES, Screen,
    Style as FrameStyle, Width, sanitize_hyperlink,
};
use corvid_protocol::wire::{
    HistoryCommand, ReportedDirectory, TerminalColors, TerminalHistory, WheelEncoding,
};
use corvid_protocol::{
    DEFAULT_ANSI_PALETTE_16, DEFAULT_TERMINAL_BACKGROUND_RGB, DEFAULT_TERMINAL_FOREGROUND_RGB,
    TERMINAL_HISTORY_MAX_COMMANDS, TERMINAL_HISTORY_MAX_ROWS,
};
use ghostty_vt::render::{CellIterator, RenderState, RowIterator};
use ghostty_vt::screen::{Cell, CellContentTag, CellWide, Screen as ActiveScreen, TrackedGridRef};
use ghostty_vt::style::{Palette, RgbColor, Style, StyleColor, Underline};
use ghostty_vt::terminal::{
    ClipboardWritePolicy, Event, Mode, Options, Point, PointCoordinate, PointSpace, Terminal,
};
#[cfg(target_os = "linux")]
use ghostty_vt::terminal::{CompressionActivity, CompressionMode, CompressionResult};

use super::history::{ColumnOffsets, HistoryBuilder, HistoryLine};
use super::{DEVICE_ATTRIBUTES, MAX_REPLY_BYTES, is_bidi_control};

const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_CELL_CODEPOINTS: usize = 24;
#[cfg(target_os = "linux")]
const COMPRESSION_IDLE: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const COMPRESSION_INTERVAL: Duration = Duration::from_millis(250);

pub(super) struct Ghostty {
    // TrackedGridRefs hold non-owning pointers to `terminal` and must drop first.
    commands: VecDeque<TrackedCommand>,
    terminal: Terminal,
    surface: OnceCell<Screen>,
    projection: RefCell<Option<Projection>>,
    #[cfg(target_os = "linux")]
    compression: Option<IdleCompression>,
    prompt_parser: vte::Parser,
    prompt: Prompt,
    pending_st_marker: Option<ShellMarker>,
    pub(super) viewport: Viewport,
    pub(super) title: String,
    pub(super) reported_directory: Option<ReportedDirectory>,
}

struct TrackedAnchor {
    grid: TrackedGridRef,
    inclusive: bool,
}

struct TrackedCommand {
    prompt: Option<TrackedGridRef>,
    output_start: Option<TrackedAnchor>,
    output_end: Option<TrackedAnchor>,
    finished: bool,
}

#[derive(Clone, Copy)]
enum ShellMarker {
    PromptStart { continuation: bool },
    PromptEnd,
    CommandStart,
    CommandEnd,
    Reset,
}

impl Ghostty {
    pub(super) fn new(viewport: Viewport) -> Result<Self> {
        let mut terminal = Terminal::new(Options {
            cols: viewport.cols.get(),
            rows: viewport.rows.get(),
            max_scrollback: SCROLLBACK_BYTES,
        })?;
        // Ghostty prunes whole pages. Keep enough rows to report when the
        // smaller exported history limit truncates a snapshot.
        terminal.set_scrollback_max_lines(Some(TERMINAL_HISTORY_MAX_ROWS * 2))?;
        // The text-only render protocol cannot present or acknowledge images.
        terminal.set_kitty_image_storage_limit(0)?;
        terminal.set_glyph_protocol_enabled(false)?;
        terminal.set_apc_max_bytes(Some(4096))?;
        terminal.set_clipboard_write_max_bytes(Some(0))?;
        terminal.set_clipboard_write_policy(ClipboardWritePolicy::Deny);
        if cfg!(windows) {
            terminal.set_resize_pull_scrollback(false)?;
        }
        // TODO: Combine OSC 7 reported directory with OSC 3008 execution context stacks (#413)
        // Tracked as a follow-up in https://github.com/DosangGu/corvid/issues/413.
        // TODO: Synchronize dynamic runtime OSC 10/11/4 color writes and mid-session theme changes across attached clients
        // Tracked alongside Desktop light appearance (#193) and the general OSC roadmap (#413).
        terminal.set_default_fg_color(Some(rgb_color(DEFAULT_TERMINAL_FOREGROUND_RGB)))?;
        terminal.set_default_bg_color(Some(rgb_color(DEFAULT_TERMINAL_BACKGROUND_RGB)))?;
        terminal.set_default_color_palette(Some(default_palette(None)))?;
        terminal.resize(viewport.cols.get(), viewport.rows.get(), 0, 0)?;
        Ok(Self {
            commands: VecDeque::new(),
            terminal,
            surface: OnceCell::new(),
            projection: RefCell::new(None),
            #[cfg(target_os = "linux")]
            compression: Some(IdleCompression::new(Instant::now())),
            prompt_parser: vte::Parser::new(),
            prompt: Prompt::default(),
            pending_st_marker: None,
            viewport,
            title: String::new(),
            reported_directory: None,
        })
    }

    pub(super) fn set_colors(&mut self, colors: &TerminalColors) -> Result<()> {
        if let Some(fg) = colors.foreground_rgb {
            self.terminal.set_default_fg_color(Some(rgb_color(fg)))?;
        }
        if let Some(bg) = colors.background_rgb {
            self.terminal.set_default_bg_color(Some(rgb_color(bg)))?;
        }
        if colors.ansi_palette.len() == 16 {
            self.terminal
                .set_default_color_palette(Some(default_palette(Some(&colors.ansi_palette))))?;
        }
        Ok(())
    }

    pub(super) fn process(&mut self, bytes: &[u8], replies: &mut Vec<u8>) -> Result<()> {
        self.surface.take();
        #[cfg(target_os = "linux")]
        if let Some(compression) = &mut self.compression {
            compression.defer(Instant::now());
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if let Some(marker) = self.pending_st_marker.take() {
                let (first, rest) = remaining.split_at(1);
                self.prompt_parser.advance(&mut self.prompt, first);
                self.terminal.vt_write(first);
                self.drain_events(replies)?;
                if first == b"\\" {
                    self.record_marker(marker);
                }
                remaining = rest;
                continue;
            }
            let consumed = self
                .prompt_parser
                .advance_until_terminated(&mut self.prompt, remaining);
            let (chunk, rest) = remaining.split_at(consumed);
            self.terminal.vt_write(chunk);
            self.drain_events(replies)?;
            if let Some(dir) = self.prompt.pending_directory.take() {
                self.reported_directory = Some(dir);
            }
            if let Some(marker) = self.prompt.terminated.take() {
                if self.prompt.awaiting_st {
                    self.prompt.awaiting_st = false;
                    self.pending_st_marker = Some(marker);
                } else {
                    self.record_marker(marker);
                }
            }
            remaining = rest;
        }
        Ok(())
    }

    fn drain_events(&mut self, replies: &mut Vec<u8>) -> Result<()> {
        for event in self.terminal.events() {
            match event {
                Event::TitleChanged(title) => self.title = corvid_core::layout::clean_title(&title),
                Event::PwdChanged(pwd) => {
                    if pwd.is_empty() {
                        self.reported_directory = None;
                    } else if let Some(dir) =
                        corvid_protocol::reported_directory::parse_osc7_or_pwd(&pwd)
                    {
                        self.reported_directory = Some(dir);
                    }
                }
                Event::PtyWrite(bytes) => {
                    if let Some(reply) = profile_reply(&bytes) {
                        ensure!(
                            replies.len() + reply.len() <= MAX_REPLY_BYTES,
                            "terminal query reply limit exceeded"
                        );
                        replies.extend_from_slice(reply);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn record_marker(&mut self, marker: ShellMarker) {
        match marker {
            ShellMarker::PromptStart { .. } | ShellMarker::PromptEnd => {
                self.prompt.phase = Some(true);
            }
            ShellMarker::CommandStart => {
                self.prompt.phase = Some(false);
            }
            ShellMarker::CommandEnd | ShellMarker::Reset => {
                self.prompt.phase = None;
            }
        }
        if matches!(marker, ShellMarker::Reset) {
            self.commands.clear();
            self.reported_directory = None;
            return;
        }
        if self.alternate_screen() {
            return;
        }
        let cols = self.viewport.cols.get();
        let rows = self.viewport.rows.get();
        let x = self
            .terminal
            .cursor_x()
            .unwrap_or(0)
            .min(cols.saturating_sub(1));
        let y = u32::from(
            self.terminal
                .cursor_y()
                .unwrap_or(0)
                .min(rows.saturating_sub(1)),
        );
        let pending_wrap = self.terminal.is_cursor_pending_wrap().unwrap_or(false);
        let track = |terminal: &Terminal| {
            terminal
                .track_grid_ref(Point::Active(PointCoordinate { x, y }))
                .ok()
        };

        match marker {
            ShellMarker::PromptStart { continuation } => {
                if continuation {
                    return;
                }
                let scrollback =
                    u32::try_from(self.terminal.scrollback_rows().unwrap_or(0)).unwrap_or(u32::MAX);
                let current_screen_y = scrollback.saturating_add(y);
                if let Some(last) = self.commands.back_mut() {
                    if last.output_start.is_none()
                        && last
                            .prompt
                            .as_ref()
                            .and_then(|grid| grid.point(PointSpace::Screen).ok().flatten())
                            .is_some_and(|point| point.y >= current_screen_y)
                    {
                        last.prompt = track(&self.terminal);
                        return;
                    }
                    if last.output_start.is_some() && last.output_end.is_none() {
                        last.output_end = track(&self.terminal).map(|grid| TrackedAnchor {
                            grid,
                            inclusive: false,
                        });
                        last.finished = true;
                    }
                }
                self.push_command(TrackedCommand {
                    prompt: track(&self.terminal),
                    output_start: None,
                    output_end: None,
                    finished: false,
                });
            }
            ShellMarker::PromptEnd | ShellMarker::Reset => {}
            ShellMarker::CommandStart => {
                let Some(grid) = track(&self.terminal) else {
                    return;
                };
                let anchor = TrackedAnchor {
                    grid,
                    inclusive: pending_wrap,
                };
                if let Some(last) = self.commands.back_mut()
                    && last.output_start.is_none()
                {
                    last.output_start = Some(anchor);
                } else {
                    self.push_command(TrackedCommand {
                        prompt: None,
                        output_start: Some(anchor),
                        output_end: None,
                        finished: false,
                    });
                }
            }
            ShellMarker::CommandEnd => {
                if let Some(last) = self.commands.back_mut()
                    && last.output_start.is_some()
                    && last.output_end.is_none()
                {
                    last.output_end = track(&self.terminal).map(|grid| TrackedAnchor {
                        grid,
                        inclusive: pending_wrap,
                    });
                    last.finished = true;
                }
            }
        }
    }

    fn push_command(&mut self, command: TrackedCommand) {
        while self
            .commands
            .front()
            .is_some_and(|cmd| !cmd.has_live_anchor())
        {
            self.commands.pop_front();
        }
        if self.commands.len() >= TERMINAL_HISTORY_MAX_COMMANDS {
            self.commands.pop_front();
        }
        self.commands.push_back(command);
    }

    pub(super) fn resize(&mut self, viewport: Viewport) -> Result<()> {
        self.terminal
            .resize(viewport.cols.get(), viewport.rows.get(), 0, 0)?;
        self.terminal.events().for_each(drop);
        self.viewport = viewport;
        self.surface.take();
        self.projection.get_mut().take();
        #[cfg(target_os = "linux")]
        if let Some(compression) = &mut self.compression {
            compression.defer(Instant::now());
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn compress_idle(&mut self, now: Instant) {
        if let Some(scheduler) = &mut self.compression
            && !scheduler.poll(&mut self.terminal, now)
        {
            self.compression = None;
        }
    }

    pub(super) fn alternate_screen(&self) -> bool {
        matches!(self.terminal.active_screen(), Ok(ActiveScreen::Alternate))
    }

    pub(super) fn bracketed_paste(&self) -> bool {
        self.mode(Mode::BRACKETED_PASTE)
    }

    pub(super) fn shell_prompt(&self) -> Option<bool> {
        self.prompt.phase
    }

    pub(super) fn wheel_encoding(&self) -> WheelEncoding {
        if ![
            Mode::X10_MOUSE,
            Mode::NORMAL_MOUSE,
            Mode::BUTTON_MOUSE,
            Mode::ANY_MOUSE,
        ]
        .into_iter()
        .any(|mode| self.mode(mode))
        {
            return WheelEncoding::Disabled;
        }
        if self.mode(Mode::SGR_MOUSE) {
            WheelEncoding::Sgr
        } else if self.mode(Mode::UTF8_MOUSE) {
            WheelEncoding::Utf8
        } else {
            WheelEncoding::Legacy
        }
    }

    pub(super) fn screen(&self) -> Result<Screen> {
        if let Some(screen) = self.surface.get() {
            return Ok(screen.clone());
        }
        let mut projection = self.projection.borrow_mut();
        if projection.is_none() {
            *projection = Some(Projection::new(self.viewport)?);
        }
        match projection.as_mut().unwrap().update(&self.terminal) {
            Ok(screen) => Ok(self.surface.get_or_init(|| screen).clone()),
            Err(error) => {
                projection.take();
                Err(error)
            }
        }
    }

    pub(super) fn history(&mut self) -> Result<TerminalHistory> {
        #[cfg(target_os = "linux")]
        if let Some(compression) = &mut self.compression {
            compression.defer(Instant::now());
        }
        let alternate_screen = self.alternate_screen();
        let resolved_commands: Vec<ResolvedCommand> = if alternate_screen {
            Vec::new()
        } else {
            while self
                .commands
                .front()
                .is_some_and(|cmd| !cmd.has_live_anchor())
            {
                self.commands.pop_front();
            }
            self.commands.iter().map(TrackedCommand::resolve).collect()
        };
        let mut needed_rows = HashMap::<u32, ColumnOffsets>::new();
        for cmd in &resolved_commands {
            if let Some((y, _, _)) = cmd.start {
                needed_rows.entry(y).or_insert_with(|| ColumnOffsets {
                    starts: Vec::new(),
                    ends: Vec::new(),
                });
            }
            if let Some((y, _, _)) = cmd.end {
                needed_rows.entry(y).or_insert_with(|| ColumnOffsets {
                    starts: Vec::new(),
                    ends: Vec::new(),
                });
            }
        }

        let columns = self.viewport.cols.get();
        let rows = u32::try_from(self.terminal.total_rows()?)?;
        let reserved_bytes = resolved_commands.len() * 32;
        let mut history = HistoryBuilder::new(columns, reserved_bytes);
        let mut cells = Vec::with_capacity(usize::from(columns));
        let mut link_buf = Vec::new();
        for y in (0..rows).rev() {
            cells.clear();
            let mut wrapped = false;
            let mut last_link: Option<Arc<Hyperlink>> = None;
            for x in 0..columns {
                let reference = self
                    .terminal
                    .grid_ref(Point::Screen(PointCoordinate { x, y }))?;
                if x == 0 {
                    wrapped = reference.row()?.is_wrapped()?;
                }
                let cell = reference.cell()?;
                let hyperlink = if cell.has_hyperlink()? {
                    read_hyperlink(&reference, &mut link_buf, &mut last_link)?
                } else {
                    last_link = None;
                    None
                };
                let raw_style = reference.style()?;
                let style = cell_style(cell, cell.content_tag()?, raw_style)?;
                let width = match cell.wide()? {
                    CellWide::Wide => Width::Wide,
                    CellWide::SpacerTail | CellWide::SpacerHead => Width::Continuation,
                    CellWide::Narrow => Width::Single,
                };
                let mut codepoints = ['\0'; MAX_CELL_CODEPOINTS];
                let text = if raw_style.invisible || width == Width::Continuation {
                    String::new()
                } else {
                    match reference.graphemes(&mut codepoints) {
                        Ok(count) => codepoints[..count].iter().copied().collect(),
                        Err(ghostty_vt::Error::OutOfSpace { .. }) => {
                            char::from_u32(cell.codepoint()?).into_iter().collect()
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                cells.push(FrameCell::new(&text, width, style).with_hyperlink(hyperlink));
            }
            let track = needed_rows.contains_key(&y);
            let (line, offsets) = HistoryLine::from_cells(&cells, wrapped, track);
            if !history.push(line, wrapped) {
                break;
            }
            if let Some(offsets) = offsets {
                needed_rows.insert(y, offsets);
            }
        }
        let (mut snapshot, exported_rows) = history.finish();
        let base_y = rows.saturating_sub(u32::try_from(exported_rows)?);
        for cmd in resolved_commands {
            if let Some(exported) = cmd.export(base_y, &needed_rows) {
                snapshot.commands.push(exported);
            }
        }
        snapshot.alternate_screen = Some(alternate_screen);
        Ok(snapshot)
    }

    fn mode(&self, mode: Mode) -> bool {
        self.terminal.mode(mode).unwrap_or(false)
    }
}

#[cfg(target_os = "linux")]
struct IdleCompression {
    activity: Option<CompressionActivity>,
    next: Instant,
    complete: bool,
}

#[cfg(target_os = "linux")]
impl IdleCompression {
    fn new(now: Instant) -> Self {
        Self {
            activity: None,
            next: now + COMPRESSION_IDLE,
            complete: false,
        }
    }

    fn defer(&mut self, now: Instant) {
        self.next = now + COMPRESSION_IDLE;
    }

    fn poll(&mut self, terminal: &mut Terminal, now: Instant) -> bool {
        if now < self.next {
            return true;
        }
        self.next = now + COMPRESSION_INTERVAL;
        let Ok(activity) = terminal.compression_activity() else {
            return false;
        };
        if self.activity.replace(activity) != Some(activity) {
            self.complete = false;
        }
        if self.complete {
            return true;
        }
        match terminal.compress(CompressionMode::Incremental) {
            Ok(CompressionResult::Pending) => true,
            Ok(CompressionResult::Complete) => {
                self.complete = true;
                true
            }
            Ok(CompressionResult::Unsupported) | Err(_) => false,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn idle_compression_preserves_history_and_restarts_after_scroll() -> Result<()> {
    let mut engine = Ghostty::new(Viewport::new(100, 30).unwrap())?;
    let text = b"\x1b[36ma repeated line in the scrollback buffer\x1b[0m\r\n".repeat(2600);
    engine.process(&text, &mut Vec::new())?;
    let before = engine.history()?;
    let first_deadline = engine.compression.as_ref().unwrap().next;
    engine.compress_idle(first_deadline - Duration::from_millis(1));
    assert!(engine.compression.as_ref().unwrap().activity.is_none());
    for step in 0..128 {
        engine.compress_idle(first_deadline + COMPRESSION_INTERVAL * step);
        let state = engine
            .compression
            .as_ref()
            .expect("Linux supports incremental compression");
        if state.complete {
            break;
        }
    }
    assert!(engine.compression.as_ref().unwrap().complete);
    let activity = engine.compression.as_ref().unwrap().activity;
    let after = engine.history()?;
    assert_eq!(after.lines, before.lines);
    assert_eq!(after.styles, before.styles);

    engine.process(b"\x1b[31mnew row\x1b[0m\r\n", &mut Vec::new())?;
    let deadline = engine.compression.as_ref().unwrap().next;
    engine.compress_idle(deadline - Duration::from_millis(1));
    assert_eq!(engine.compression.as_ref().unwrap().activity, activity);
    engine.compress_idle(deadline);
    assert_ne!(engine.compression.as_ref().unwrap().activity, activity);
    assert!(engine.history()?.lines.contains(&"new row".into()));
    Ok(())
}

struct Projection {
    render: RenderState,
    rows: RowIterator,
    cells: CellIterator,
    screen: Screen,
}

impl Projection {
    fn new(viewport: Viewport) -> Result<Self> {
        Ok(Self {
            render: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            screen: Screen::new(viewport.rows, viewport.cols),
        })
    }

    fn update(&mut self, terminal: &Terminal) -> Result<Screen> {
        // Refresh the owned grid using Ghostty damage. Each client's last
        // acknowledged grid remains the independent baseline for ANSI diffs.
        let snapshot = self.render.update(terminal)?;
        let mut rows = self.rows.update(&snapshot)?;
        let columns = usize::from(self.screen.size().1.get());
        let mut text = String::new();
        let mut link_buf = Vec::new();
        while let Some((index, row)) = rows.next_dirty() {
            let wrapped = row.raw_row()?.is_wrapped()?;
            let row_has_hyperlink = row.raw_row()?.has_hyperlink()?;
            let mut cells = self.cells.update(row)?;
            let mut line: Vec<FrameCell> = Vec::with_capacity(columns);
            let mut last_link: Option<Arc<Hyperlink>> = None;
            while let Some(cell) = cells.next() {
                let col = line.len() as u16;
                let raw = cell.raw_cell()?;
                let has_hyperlink = row_has_hyperlink && raw.has_hyperlink()?;
                let tag = raw.content_tag()?;
                let codepoint = raw.codepoint()?;
                let styled = raw.has_styling()?
                    || matches!(
                        tag,
                        CellContentTag::BgColorRgb | CellContentTag::BgColorPalette
                    );
                if codepoint == 0 && !styled && !has_hyperlink {
                    last_link = None;
                    // Only the cell immediately after a wide glyph can be an
                    // unstyled continuation. Other empty cells share defaults.
                    let cell = if line.last().is_some_and(FrameCell::is_wide) {
                        FrameCell::new("", Width::Continuation, FrameStyle::default())
                    } else {
                        FrameCell::default()
                    };
                    line.push(cell);
                    continue;
                }
                if !styled
                    && !has_hyperlink
                    && tag == CellContentTag::Codepoint
                    && (32..=126).contains(&codepoint)
                {
                    last_link = None;
                    let ascii = [codepoint as u8];
                    line.push(FrameCell::new(
                        std::str::from_utf8(&ascii).unwrap(),
                        Width::Single,
                        FrameStyle::default(),
                    ));
                    continue;
                }
                let hyperlink = if has_hyperlink {
                    let reference = terminal.grid_ref(Point::Viewport(PointCoordinate {
                        x: col,
                        y: u32::from(index),
                    }))?;
                    read_hyperlink(&reference, &mut link_buf, &mut last_link)?
                } else {
                    last_link = None;
                    None
                };
                let (style, invisible) = if styled {
                    let attributes = cell.style()?;
                    (cell_style(raw, tag, attributes)?, attributes.invisible)
                } else {
                    (FrameStyle::default(), false)
                };
                text.clear();
                if !invisible && codepoint != 0 {
                    if tag == CellContentTag::CodepointGrapheme
                        && cell.graphemes_len()? <= MAX_CELL_CODEPOINTS
                    {
                        cell.graphemes_utf8(&mut text)?;
                    } else if let Some(base) = char::from_u32(codepoint) {
                        text.push(base);
                    }
                }
                text.retain(|ch| !ch.is_control() && !is_bidi_control(ch));
                line.push(FrameCell::new(&text, cell_width(raw)?, style).with_hyperlink(hyperlink));
            }
            self.screen.replace_row(index, line, wrapped);
        }
        snapshot.clean()?;
        let attributes = terminal.cursor_style()?;
        self.screen.cursor = Cursor {
            row: terminal.cursor_y()?,
            col: terminal.cursor_x()?,
            wrap_pending: terminal.is_cursor_pending_wrap()?,
            hidden: !terminal.is_cursor_visible()?,
            style: convert_style(attributes, attributes.bg_color),
        };
        self.screen.application_cursor = terminal.mode(Mode::DECCKM)?;
        self.screen.application_keypad = terminal.mode(Mode::KEYPAD_KEYS)?;
        Ok(self.screen.clone())
    }
}

fn read_hyperlink(
    reference: &ghostty_vt::screen::GridRef<'_>,
    buf: &mut Vec<u8>,
    last_link: &mut Option<Arc<Hyperlink>>,
) -> Result<Option<Arc<Hyperlink>>> {
    if buf.is_empty() {
        buf.resize(256, 0);
    }
    let len = match reference.hyperlink_uri(buf) {
        Ok(0) => {
            *last_link = None;
            return Ok(None);
        }
        Ok(len) => len,
        Err(ghostty_vt::Error::OutOfSpace { required }) => {
            if required > MAX_HYPERLINK_URI_BYTES {
                *last_link = None;
                return Ok(None);
            }
            buf.resize(required, 0);
            reference.hyperlink_uri(buf)?
        }
        Err(error) => return Err(error.into()),
    };
    let Ok(uri) = std::str::from_utf8(&buf[..len]) else {
        *last_link = None;
        return Ok(None);
    };
    if let Some(prev) = last_link.as_ref().filter(|prev| prev.uri == uri) {
        return Ok(Some(Arc::clone(prev)));
    }
    let link = sanitize_hyperlink(uri, None).map(Arc::new);
    *last_link = link.clone();
    Ok(link)
}

fn cell_style(cell: Cell, tag: CellContentTag, style: Style) -> Result<FrameStyle> {
    let bg = match tag {
        CellContentTag::BgColorRgb => StyleColor::Rgb(cell.bg_color_rgb()?),
        CellContentTag::BgColorPalette => StyleColor::Palette(cell.bg_color_palette()?),
        _ => style.bg_color,
    };
    Ok(convert_style(style, bg))
}

fn convert_style(style: Style, bg: StyleColor) -> FrameStyle {
    FrameStyle {
        fg: color(style.fg_color),
        bg: color(bg),
        bold: style.bold,
        dim: style.faint,
        italic: style.italic,
        underline: style.underline != Underline::None,
        inverse: style.inverse,
    }
}

fn color(color: StyleColor) -> Color {
    match color {
        StyleColor::None => Color::Default,
        StyleColor::Palette(index) => Color::Indexed(index.0),
        StyleColor::Rgb(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn cell_width(cell: Cell) -> Result<Width> {
    Ok(match cell.wide()? {
        CellWide::Wide => Width::Wide,
        CellWide::SpacerTail => Width::Continuation,
        CellWide::Narrow | CellWide::SpacerHead => Width::Single,
    })
}

fn rgb_color(rgb: u32) -> RgbColor {
    RgbColor {
        r: ((rgb >> 16) & 0xff) as u8,
        g: ((rgb >> 8) & 0xff) as u8,
        b: (rgb & 0xff) as u8,
    }
}

fn default_palette(ansi_16: Option<&[u32]>) -> Palette {
    let mut colors = [RgbColor { r: 0, g: 0, b: 0 }; 256];
    let base_16 = ansi_16.unwrap_or(&DEFAULT_ANSI_PALETTE_16);
    for (slot, &rgb) in colors[..16].iter_mut().zip(base_16.iter()) {
        *slot = rgb_color(rgb);
    }
    let level = [0u8, 95, 135, 175, 215, 255];
    for (offset, slot) in colors[16..232].iter_mut().enumerate() {
        *slot = RgbColor {
            r: level[offset / 36],
            g: level[(offset / 6) % 6],
            b: level[offset % 6],
        };
    }
    for (offset, slot) in colors[232..256].iter_mut().enumerate() {
        let gray = 8 + (offset as u8) * 10;
        *slot = RgbColor {
            r: gray,
            g: gray,
            b: gray,
        };
    }
    Palette(colors)
}

fn profile_reply(bytes: &[u8]) -> Option<&[u8]> {
    if let Some(osc) = bytes.strip_prefix(b"\x1b]") {
        let body = osc
            .strip_suffix(b"\x07")
            .or_else(|| osc.strip_suffix(b"\x1b\\"))?;
        if let Some(rgb) = body
            .strip_prefix(b"10;rgb:")
            .or_else(|| body.strip_prefix(b"11;rgb:"))
        {
            return osc_rgb_triplet(rgb).then_some(bytes);
        }
        if let Some(rest) = body.strip_prefix(b"4;")
            && let Some((index, rgb)) = split_once_bytes(rest, b";rgb:")
            && valid_palette_index(index)
            && osc_rgb_triplet(rgb)
        {
            return Some(bytes);
        }
        return None;
    }

    let body = bytes.strip_prefix(b"\x1b[")?;
    if body == b"0n" {
        Some(bytes)
    } else if let Some(attributes) = body
        .strip_prefix(b"?")
        .and_then(|rest| rest.strip_suffix(b"c"))
    {
        numeric_fields(attributes, None).then_some(DEVICE_ATTRIBUTES.as_bytes())
    } else if let Some(cursor) = body.strip_suffix(b"R") {
        numeric_fields(cursor, Some(2)).then_some(bytes)
    } else if let Some(size) = body
        .strip_prefix(b"8;")
        .and_then(|rest| rest.strip_suffix(b"t"))
    {
        numeric_fields(size, Some(2)).then_some(bytes)
    } else {
        None
    }
}

fn split_once_bytes<'a>(bytes: &'a [u8], needle: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let pos = bytes
        .windows(needle.len())
        .position(|window| window == needle)?;
    Some((&bytes[..pos], &bytes[pos + needle.len()..]))
}

fn valid_palette_index(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.len() <= 3
        && bytes.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| s.parse::<u8>().ok())
            .is_some()
}

fn osc_rgb_triplet(bytes: &[u8]) -> bool {
    let mut parts = bytes.split(|&byte| byte == b'/');
    let Some(r) = parts.next() else { return false };
    let Some(g) = parts.next() else { return false };
    let Some(b) = parts.next() else { return false };
    parts.next().is_none()
        && [r, g, b]
            .into_iter()
            .all(|part| (1..=4).contains(&part.len()) && part.iter().all(u8::is_ascii_hexdigit))
}

fn numeric_fields(bytes: &[u8], count: Option<usize>) -> bool {
    let fields = bytes.split(|&byte| byte == b';');
    count.is_none_or(|count| fields.clone().count() == count)
        && fields
            .into_iter()
            .all(|field| !field.is_empty() && field.iter().all(u8::is_ascii_digit))
}

impl TrackedAnchor {
    fn resolve(&self) -> Option<(u32, u16, bool)> {
        let point = self.grid.point(PointSpace::Screen).ok().flatten()?;
        Some((point.y, point.x, self.inclusive))
    }
}

impl TrackedCommand {
    fn has_live_anchor(&self) -> bool {
        self.prompt.as_ref().is_some_and(TrackedGridRef::has_value)
            || self
                .output_start
                .as_ref()
                .is_some_and(|anchor| anchor.grid.has_value())
            || self
                .output_end
                .as_ref()
                .is_some_and(|anchor| anchor.grid.has_value())
    }

    fn resolve(&self) -> ResolvedCommand {
        ResolvedCommand {
            had_prompt: self.prompt.is_some(),
            prompt_y: self
                .prompt
                .as_ref()
                .and_then(|grid| grid.point(PointSpace::Screen).ok().flatten())
                .map(|point| point.y),
            had_start: self.output_start.is_some(),
            start: self.output_start.as_ref().and_then(TrackedAnchor::resolve),
            end: self.output_end.as_ref().and_then(TrackedAnchor::resolve),
            finished: self.finished,
        }
    }
}

struct ResolvedCommand {
    had_prompt: bool,
    prompt_y: Option<u32>,
    had_start: bool,
    start: Option<(u32, u16, bool)>,
    end: Option<(u32, u16, bool)>,
    finished: bool,
}

impl ResolvedCommand {
    fn export(self, base_y: u32, offsets: &HashMap<u32, ColumnOffsets>) -> Option<HistoryCommand> {
        if let Some((end_y, _, _)) = self.end
            && end_y < base_y
        {
            return None;
        }
        if !self.had_start && self.prompt_y.is_none_or(|prompt_y| prompt_y < base_y) {
            return None;
        }

        let prompt_row = self.prompt_y.and_then(|y| y.checked_sub(base_y));
        let mut truncated = self.had_prompt && prompt_row.is_none();

        let (output_start_row, output_start_offset) = if self.had_start {
            match self.start {
                Some((y, x, inclusive)) if y >= base_y => {
                    let row = y - base_y;
                    let offset = resolve_offset(offsets.get(&y), x, inclusive);
                    (Some(row), Some(offset))
                }
                _ => {
                    truncated = true;
                    (Some(0), Some(0))
                }
            }
        } else {
            (None, None)
        };

        let (output_end_row, output_end_offset) = if let Some((y, x, inclusive)) = self.end {
            if let (Some(sr), Some(so)) = (output_start_row, output_start_offset) {
                let row = y.checked_sub(base_y)?;
                let offset = resolve_offset(offsets.get(&y), x, inclusive);
                if (row, offset) < (sr, so) {
                    (Some(sr), Some(so))
                } else {
                    (Some(row), Some(offset))
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        Some(HistoryCommand {
            prompt_row,
            output_start_row,
            output_start_offset,
            output_end_row,
            output_end_offset,
            finished: self.finished,
            truncated,
        })
    }
}

fn resolve_offset(offsets: Option<&ColumnOffsets>, x: u16, inclusive: bool) -> u32 {
    let Some(offsets) = offsets else {
        return 0;
    };
    let slice = if inclusive {
        &offsets.ends
    } else {
        &offsets.starts
    };
    slice
        .get(usize::from(x))
        .copied()
        .unwrap_or_else(|| slice.last().copied().unwrap_or(0)) as u32
}

#[derive(Default)]
struct Prompt {
    phase: Option<bool>,
    terminated: Option<ShellMarker>,
    awaiting_st: bool,
    pending_directory: Option<ReportedDirectory>,
}

impl vte::Perform for Prompt {
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        if let [b"633", b"P", prop, ..] = params
            && let Some(raw) = prop.strip_prefix(b"Cwd=")
        {
            if let Ok(value) = std::str::from_utf8(raw) {
                self.pending_directory =
                    corvid_protocol::reported_directory::parse_osc7_or_pwd(value);
            }
            return;
        }
        let marker = match params {
            [b"133" | b"633", b"A", rest @ ..] => ShellMarker::PromptStart {
                continuation: is_continuation_prompt(rest),
            },
            [b"133" | b"633", b"B", ..] => ShellMarker::PromptEnd,
            [b"133" | b"633", b"C", ..] => ShellMarker::CommandStart,
            [b"133" | b"633", b"D", ..] => ShellMarker::CommandEnd,
            _ => return,
        };
        self.terminated = Some(marker);
        self.awaiting_st = !bell_terminated;
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.terminated = Some(ShellMarker::Reset);
            self.awaiting_st = false;
        }
    }

    fn terminated(&self) -> bool {
        self.terminated.is_some()
    }
}

fn is_continuation_prompt(rest: &[&[u8]]) -> bool {
    rest.iter().any(|param| {
        param
            .split(|&byte| byte == b':')
            .any(|part| matches!(part, b"k=c" | b"k=s"))
    })
}
