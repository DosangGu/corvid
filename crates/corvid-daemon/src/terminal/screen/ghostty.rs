use std::cell::{OnceCell, RefCell};
use std::fmt::Write;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use corvid_core::Viewport;
use corvid_protocol::TERMINAL_HISTORY_MAX_ROWS;
use corvid_protocol::wire::{TerminalHistory, WheelEncoding};
use ghostty_vt::render::{CellIterator, RenderState, RowIterator};
use ghostty_vt::screen::{Cell, CellContentTag, CellWide, Screen as ActiveScreen};
use ghostty_vt::style::{Style, StyleColor, Underline};
use ghostty_vt::terminal::{
    ClipboardWritePolicy, Event, Mode, Options, Point, PointCoordinate, Terminal,
};
#[cfg(target_os = "linux")]
use ghostty_vt::terminal::{CompressionActivity, CompressionMode, CompressionResult};
use vt100::{Parser, Screen};

use super::history::{HistoryBuilder, HistoryLine};
use super::snapshot::{CellColor, CellStyle, CellWidth, ScreenCell};
use super::{DEVICE_ATTRIBUTES, MAX_REPLY_BYTES, is_bidi_control};

const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_CELL_CODEPOINTS: usize = 24;
#[cfg(target_os = "linux")]
const COMPRESSION_IDLE: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const COMPRESSION_INTERVAL: Duration = Duration::from_millis(250);

pub(super) struct Ghostty {
    terminal: Terminal,
    surface: OnceCell<Screen>,
    projection: RefCell<Option<Projection>>,
    #[cfg(target_os = "linux")]
    compression: Option<IdleCompression>,
    prompt_parser: vte::Parser,
    prompt: Prompt,
    pub(super) viewport: Viewport,
    pub(super) title: String,
}

impl Ghostty {
    pub(super) fn new(viewport: Viewport) -> Result<Self> {
        let mut terminal = Terminal::new(Options {
            cols: viewport.cols.get(),
            rows: viewport.rows.get(),
            max_scrollback: SCROLLBACK_BYTES,
        })?;
        terminal.set_scrollback_max_lines(Some(TERMINAL_HISTORY_MAX_ROWS))?;
        terminal.set_glyph_protocol_enabled(false)?;
        terminal.set_apc_max_bytes(Some(4096))?;
        terminal.set_clipboard_write_max_bytes(Some(0))?;
        terminal.set_clipboard_write_policy(ClipboardWritePolicy::Deny);
        if cfg!(windows) {
            terminal.set_resize_pull_scrollback(false)?;
        }
        terminal.resize(viewport.cols.get(), viewport.rows.get(), 0, 0)?;
        Ok(Self {
            terminal,
            surface: OnceCell::new(),
            projection: RefCell::new(None),
            #[cfg(target_os = "linux")]
            compression: Some(IdleCompression::new(Instant::now())),
            prompt_parser: vte::Parser::new(),
            prompt: Prompt::default(),
            viewport,
            title: String::new(),
        })
    }

    pub(super) fn process(&mut self, bytes: &[u8], replies: &mut Vec<u8>) -> Result<()> {
        self.surface.take();
        #[cfg(target_os = "linux")]
        if let Some(compression) = &mut self.compression {
            compression.defer(Instant::now());
        }
        self.prompt_parser.advance(&mut self.prompt, bytes);
        self.terminal.vt_write(bytes);
        for event in self.terminal.events() {
            match event {
                Event::TitleChanged(title) => self.title = corvid_core::layout::clean_title(&title),
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
        self.prompt.0
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

    pub(super) fn screen(&self) -> Result<&Screen> {
        if let Some(screen) = self.surface.get() {
            return Ok(screen);
        }
        let mut projection = self.projection.borrow_mut();
        if projection.is_none() {
            *projection = Some(Projection::new(self.viewport)?);
        }
        let screen = match projection.as_mut().unwrap().update(&self.terminal) {
            Ok(screen) => screen,
            Err(error) => {
                projection.take();
                return Err(error);
            }
        };
        Ok(self.surface.get_or_init(|| screen))
    }

    pub(super) fn history(&mut self) -> Result<TerminalHistory> {
        #[cfg(target_os = "linux")]
        if let Some(compression) = &mut self.compression {
            compression.defer(Instant::now());
        }
        let columns = self.viewport.cols.get();
        let rows = self.terminal.total_rows()?;
        let mut history = HistoryBuilder::new(columns);
        let mut cells = Vec::with_capacity(usize::from(columns));
        for y in (0..u32::try_from(rows)?).rev() {
            cells.clear();
            for x in 0..columns {
                let reference = self
                    .terminal
                    .grid_ref(Point::Screen(PointCoordinate { x, y }))?;
                let cell = reference.cell()?;
                let raw_style = reference.style()?;
                let style = cell_style(cell, cell.content_tag()?, raw_style)?;
                let mut codepoints = ['\0'; MAX_CELL_CODEPOINTS];
                let text = if raw_style.invisible {
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
                cells.push(OwnedCell {
                    text,
                    style,
                    width: cell_width(cell)?,
                });
            }
            if !history.push(HistoryLine::from_cells(cells.iter().map(OwnedCell::borrow))) {
                break;
            }
        }
        let mut snapshot = history.finish();
        snapshot.alternate_screen = Some(self.alternate_screen());
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
    parser: Parser,
}

impl Projection {
    fn new(viewport: Viewport) -> Result<Self> {
        Ok(Self {
            render: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            parser: Parser::new(viewport.rows, viewport.cols, 0),
        })
    }

    fn update(&mut self, terminal: &Terminal) -> Result<Screen> {
        // This cache holds the current grid. Clients still compare their own
        // acknowledged snapshots with that grid in the ANSI encoder.
        let snapshot = self.render.update(terminal)?;
        let mut rows = self.rows.update(&snapshot)?;
        let mut ansi = String::new();
        let mut text = String::new();
        while let Some((row_index, row)) = rows.next_dirty() {
            ansi.clear();
            let y = row_index + 1;
            write!(ansi, "\x1b[{y};1H\x1b[0m\x1b[2K")?;
            let mut cells = self.cells.update(row)?;
            let mut x = 0;
            let mut next_x = 1;
            let mut painted = CellStyle::default();
            while let Some(cell) = cells.next() {
                x += 1;
                let raw = cell.raw_cell()?;
                let tag = raw.content_tag()?;
                let codepoint = raw.codepoint()?;
                let styled = raw.has_styling()?
                    || matches!(
                        tag,
                        CellContentTag::BgColorRgb | CellContentTag::BgColorPalette
                    );
                if codepoint == 0 && !styled {
                    continue;
                }
                if cell_width(raw)? == CellWidth::Continuation {
                    continue;
                }
                let (style, invisible) = if styled {
                    let raw_style = cell.style()?;
                    (cell_style(raw, tag, raw_style)?, raw_style.invisible)
                } else {
                    (CellStyle::default(), false)
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
                if text.is_empty() && style == CellStyle::default() {
                    continue;
                }
                if x != next_x {
                    write!(ansi, "\x1b[{y};{x}H")?;
                }
                if painted != style {
                    style.paint(&mut ansi);
                    painted = style;
                }
                if text.is_empty() {
                    ansi.push(' ');
                } else {
                    ansi.push_str(&text);
                }
                // ASCII occupies one column. Other cells may use different
                // widths in the projection parser, so address the next one.
                next_x = if text.is_ascii() && text.len() <= 1 {
                    x + 1
                } else {
                    0
                };
            }
            self.parser.process(ansi.as_bytes());
        }
        snapshot.clean()?;
        Self::project_cursor(terminal, &mut self.parser)?;
        Ok(self.parser.screen().clone())
    }

    fn project_cursor(terminal: &Terminal, parser: &mut Parser) -> Result<()> {
        let row = terminal.cursor_y()?;
        let mut column = terminal.cursor_x()?;
        let mut ansi = String::new();
        let screen = parser.screen();
        if terminal.is_cursor_pending_wrap()? {
            if screen
                .cell(row, column)
                .is_some_and(vt100::Cell::is_wide_continuation)
            {
                column = column.saturating_sub(1);
            }
            if let Some(cell) = screen.cell(row, column).map(ScreenCell::from) {
                write!(ansi, "\x1b[{};{}H", row + 1, column + 1)?;
                cell.style.paint(&mut ansi);
                ansi.push_str(if cell.text.is_empty() { " " } else { cell.text });
            }
        } else {
            write!(ansi, "\x1b[{};{}H", row + 1, column + 1)?;
        }
        let style = terminal.cursor_style()?;
        convert_style(style, style.bg_color.into()).paint(&mut ansi);
        let cursor = if terminal.is_cursor_visible()? {
            'h'
        } else {
            'l'
        };
        let keys = if terminal.mode(Mode::DECCKM)? {
            'h'
        } else {
            'l'
        };
        let keypad = if terminal.mode(Mode::KEYPAD_KEYS)? {
            '='
        } else {
            '>'
        };
        write!(ansi, "\x1b[?25{cursor}\x1b[?1{keys}\x1b{keypad}")?;
        parser.process(ansi.as_bytes());
        Ok(())
    }
}

struct OwnedCell {
    text: String,
    style: CellStyle,
    width: CellWidth,
}

impl OwnedCell {
    fn borrow(&self) -> ScreenCell<'_> {
        ScreenCell {
            text: &self.text,
            width: self.width,
            style: self.style,
        }
    }
}

fn cell_style(cell: Cell, tag: CellContentTag, style: Style) -> Result<CellStyle> {
    let bg = match tag {
        CellContentTag::BgColorRgb => StyleColor::Rgb(cell.bg_color_rgb()?),
        CellContentTag::BgColorPalette => StyleColor::Palette(cell.bg_color_palette()?),
        _ => style.bg_color,
    };
    Ok(convert_style(style, bg.into()))
}

fn convert_style(style: Style, bg: CellColor) -> CellStyle {
    CellStyle {
        fg: style.fg_color.into(),
        bg,
        flags: [
            style.bold,
            style.faint,
            style.italic,
            style.underline != Underline::None,
            style.inverse,
        ],
    }
}

impl From<StyleColor> for CellColor {
    fn from(color: StyleColor) -> Self {
        match color {
            StyleColor::None => Self::Default,
            StyleColor::Palette(index) => Self::Indexed(index.0),
            StyleColor::Rgb(rgb) => Self::Rgb(rgb.r, rgb.g, rgb.b),
        }
    }
}

fn cell_width(cell: Cell) -> Result<CellWidth> {
    Ok(match cell.wide()? {
        CellWide::Wide => CellWidth::Wide,
        CellWide::SpacerTail => CellWidth::Continuation,
        CellWide::Narrow | CellWide::SpacerHead => CellWidth::Single,
    })
}

fn profile_reply(bytes: &[u8]) -> Option<&[u8]> {
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

fn numeric_fields(bytes: &[u8], count: Option<usize>) -> bool {
    let fields = bytes.split(|&byte| byte == b';');
    count.is_none_or(|count| fields.clone().count() == count)
        && fields
            .into_iter()
            .all(|field| !field.is_empty() && field.iter().all(u8::is_ascii_digit))
}

#[derive(Default)]
struct Prompt(Option<bool>);

impl vte::Perform for Prompt {
    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        if let [b"133" | b"633", phase, ..] = params {
            match *phase {
                b"A" | b"B" => self.0 = Some(true),
                b"C" => self.0 = Some(false),
                b"D" => self.0 = None,
                _ => {}
            }
        }
    }
}
