use std::cell::OnceCell;
use std::fmt::Write;

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
use vt100::{Parser, Screen};

use super::history::{HistoryBuilder, HistoryLine};
use super::snapshot::{CellColor, CellStyle, CellWidth, ScreenCell};
use super::{DEVICE_ATTRIBUTES, MAX_REPLY_BYTES, is_bidi_control};

const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_CELL_CODEPOINTS: usize = 24;

pub(super) struct Ghostty {
    terminal: Terminal,
    surface: OnceCell<Screen>,
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
            prompt_parser: vte::Parser::new(),
            prompt: Prompt::default(),
            viewport,
            title: String::new(),
        })
    }

    pub(super) fn process(&mut self, bytes: &[u8], replies: &mut Vec<u8>) -> Result<()> {
        self.surface.take();
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
        Ok(())
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
        let screen = self.project_visible_screen()?;
        Ok(self.surface.get_or_init(|| screen))
    }

    pub(super) fn history(&self) -> Result<TerminalHistory> {
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
                let style = cell_style(cell, raw_style)?;
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

    fn project_visible_screen(&self) -> Result<Screen> {
        // The existing per-client ANSI encoder compares owned vt100 screens.
        // Only reconstructed cell text, rendition, and supported modes enter it.
        let mut render = RenderState::new()?;
        let snapshot = render.update(&self.terminal)?;
        let mut row_iter = RowIterator::new()?;
        let mut cell_iter = CellIterator::new()?;
        let mut rows = row_iter.update(&snapshot)?;
        let mut parser = Parser::new(self.viewport.rows, self.viewport.cols, 0);
        let mut ansi = String::new();
        let mut text = String::new();
        while let Some(row) = rows.next() {
            ansi.clear();
            let y = row.viewport_y()? + 1;
            let mut cells = cell_iter.update(row)?;
            let mut x = 0;
            let mut painted = CellStyle::default();
            while let Some(cell) = cells.next() {
                x += 1;
                let raw = cell.raw_cell()?;
                if cell_width(raw)? == CellWidth::Continuation {
                    continue;
                }
                let raw_style = cell.style()?;
                let style = cell_style(raw, raw_style)?;
                text.clear();
                if !raw_style.invisible {
                    if cell.graphemes_len()? <= MAX_CELL_CODEPOINTS {
                        cell.graphemes_utf8(&mut text)?;
                    } else if let Some(base) = char::from_u32(raw.codepoint()?) {
                        text.push(base);
                    }
                }
                text.retain(|ch| !ch.is_control() && !is_bidi_control(ch));
                if text.is_empty() && style == CellStyle::default() {
                    continue;
                }
                write!(ansi, "\x1b[{y};{x}H")?;
                if painted != style {
                    style.paint(&mut ansi);
                    painted = style;
                }
                if text.is_empty() {
                    ansi.push(' ');
                } else {
                    ansi.push_str(&text);
                }
            }
            ansi.push_str("\x1b[0m");
            parser.process(ansi.as_bytes());
        }
        self.project_cursor(&mut parser)?;
        Ok(parser.screen().clone())
    }

    fn project_cursor(&self, parser: &mut Parser) -> Result<()> {
        let row = self.terminal.cursor_y()?;
        let mut column = self.terminal.cursor_x()?;
        let mut ansi = String::new();
        let screen = parser.screen();
        if self.terminal.is_cursor_pending_wrap()? {
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
        let style = self.terminal.cursor_style()?;
        convert_style(style, style.bg_color.into()).paint(&mut ansi);
        if !self.terminal.is_cursor_visible()? {
            ansi.push_str("\x1b[?25l");
        }
        if self.mode(Mode::DECCKM) {
            ansi.push_str("\x1b[?1h");
        }
        if self.mode(Mode::KEYPAD_KEYS) {
            ansi.push_str("\x1b=");
        }
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

fn cell_style(cell: Cell, style: Style) -> Result<CellStyle> {
    let bg = match cell.content_tag()? {
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
