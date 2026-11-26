//! Screen data and ANSI encoding for acknowledged frames and pane composition.

use std::fmt::Write;
use std::num::NonZeroU16;

use anyhow::{Result, ensure};
use corvid_core::Viewport;
use corvid_protocol::ansi_screen::{Cell, Decoder, Hyperlink, Screen};
use unicode_width::UnicodeWidthStr;

const RESET_DRAWING: &str = "\x1b[?6l\x1b[r\x1b[?7h\x1b[0m\x1b[2J";
const MAX_FRAME_BYTES: usize = corvid_protocol::RENDER_LIMIT;

#[derive(Clone)]
pub(crate) struct ScreenSnapshot(pub(super) Screen);

impl ScreenSnapshot {
    pub(crate) fn from_ansi(viewport: Viewport, ansi: &[u8], clip: Option<Viewport>) -> Self {
        let mut decoder = Decoder::new(viewport.rows, viewport.cols);
        decoder.process(ansi);
        let mut screen = decoder.screen().clone();
        if let Some(clip) = clip.filter(|clip| *clip != viewport) {
            screen.clip(clip.rows, clip.cols);
        }
        Self(screen)
    }

    pub(crate) fn matches(&self, previous: &Self) -> bool {
        self.0.cursor == previous.0.cursor
            && self.size() == previous.size()
            && (0..self.size().0.get()).all(|row| self.0.same_row(&previous.0, row))
    }

    #[cfg(all(test, windows))]
    pub(crate) fn contents(&self) -> String {
        self.0.contents()
    }

    pub(crate) fn size(&self) -> (NonZeroU16, NonZeroU16) {
        self.0.size()
    }

    pub(crate) fn cursor_position(&self) -> (u16, u16) {
        self.0.cursor_position()
    }

    pub(crate) fn application_cursor(&self) -> bool {
        self.0.application_cursor
    }

    pub(crate) fn application_keypad(&self) -> bool {
        self.0.application_keypad
    }

    pub(crate) fn hide_cursor(&self) -> bool {
        self.0.hide_cursor()
    }

    pub(crate) fn cell(&self, row: u16, column: u16) -> Option<&Cell> {
        self.0.cell(row, column)
    }

    pub(super) fn encode(
        &self,
        previous: Option<&Self>,
        outer_paste: bool,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        let (rows, cols) = self.size();
        let previous = previous.filter(|screen| screen.size() == self.size());
        let mut frame = String::new();
        if previous.is_none() {
            frame.push_str(RESET_DRAWING);
            if rows < outer.rows {
                write!(frame, "\x1b[1;{rows}r")?;
            }
        }
        for row in 0..rows.get() {
            if previous.is_some_and(|baseline| self.0.same_row(&baseline.0, row)) {
                continue;
            }
            let cells = self.0.row(row).unwrap();
            let old = previous.and_then(|baseline| baseline.0.row(row));
            encode_row(&mut frame, row, cells, old, cols == outer.cols)?;
            ensure!(
                frame.len() <= MAX_FRAME_BYTES,
                "terminal redraw exceeds frame limit"
            );
        }

        if let Some((row, ansi)) = footer {
            write!(frame, "\x1b[?7l\x1b[{row};1H{ansi}\x1b[0m\x1b[?7h")?;
        }
        self.encode_cursor(&mut frame, cols == outer.cols)?;
        let cursor_keys = if self.application_cursor() { 'h' } else { 'l' };
        let paste = if outer_paste { 'h' } else { 'l' };
        let keypad = if self.application_keypad() { '=' } else { '>' };
        write!(frame, "\x1b[?1{cursor_keys}\x1b[?2004{paste}\x1b{keypad}")?;
        ensure!(
            frame.len() <= MAX_FRAME_BYTES,
            "terminal redraw exceeds frame limit"
        );
        Ok(frame)
    }

    fn encode_cursor(&self, frame: &mut String, outer_wrap: bool) -> Result<()> {
        let cursor = self.0.cursor;
        let col = cursor.col.min(self.size().1.get() - 1);
        if cursor.wrap_pending && outer_wrap {
            // CUP alone cannot recreate the delayed wrap at the right edge.
            // Repaint the last glyph from its lead cell if it spans two columns.
            let start = if self
                .cell(cursor.row, col)
                .is_some_and(Cell::is_wide_continuation)
            {
                col.saturating_sub(1)
            } else {
                col
            };
            write!(frame, "\x1b[{};{}H", cursor.row + 1, start + 1)?;
            if let Some(cell) = self.cell(cursor.row, start) {
                if let Some(link) = cell.hyperlink() {
                    write_osc8(frame, Some(link));
                }
                cell.style.paint(frame);
                frame.push_str(if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                });
                if cell.hyperlink().is_some() {
                    write_osc8(frame, None);
                }
            }
        } else {
            write!(frame, "\x1b[{};{}H", cursor.row + 1, col + 1)?;
        }
        cursor.style.paint(frame);
        let visibility = if cursor.hidden { 'l' } else { 'h' };
        write!(frame, "\x1b[?25{visibility}")?;
        Ok(())
    }
}

fn encode_row(
    frame: &mut String,
    row: u16,
    cells: &[Cell],
    previous: Option<&[Cell]>,
    full_width: bool,
) -> Result<()> {
    let empty = Cell::default();
    let changed = |index: usize| cells[index] != *previous.map_or(&empty, |old| &old[index]);
    let Some(mut start) = (0..cells.len()).find(|&index| changed(index)) else {
        return Ok(());
    };
    let mut end = (start..cells.len()).rfind(|&index| changed(index)).unwrap() + 1;
    if cells[start].is_wide_continuation()
        || previous.is_some_and(|old| old[start].is_wide_continuation())
    {
        start = start.saturating_sub(1);
    }
    if end < cells.len() && cells[end].is_wide_continuation() {
        end += 1;
    }
    let mut style = None;
    let mut active_link: Option<&Hyperlink> = None;
    let mut column = start;
    write!(frame, "\x1b[{};{}H", row + 1, column + 1)?;
    while column < end {
        let cell = &cells[column];
        if cell.is_wide_continuation() {
            column += 1;
            continue;
        }
        let next_link = cell.hyperlink();
        if next_link != active_link {
            write_osc8(frame, next_link);
            active_link = next_link;
        }
        if style != Some(cell.style) {
            cell.style.paint(frame);
            style = Some(cell.style);
        }
        if cell.has_contents() {
            let text = cell.contents();
            frame.push_str(text);
            let width = 1 + usize::from(cell.is_wide());
            column += width;
            // Keep ordinary text contiguous and correct known width mismatches.
            if !text.is_ascii() && text.width() != width && column < end {
                write!(frame, "\x1b[{};{}H", row + 1, column + 1)?;
            }
            continue;
        }

        let blank_end = (column + 1..end)
            .find(|&index| cells[index] != *cell)
            .unwrap_or(end);
        // EL and ECH erase a cell without inserting a printable space.
        if full_width && blank_end == cells.len() {
            frame.push_str("\x1b[K");
        } else {
            write!(frame, "\x1b[{}X", blank_end - column)?;
        }
        column = blank_end;
        if column < end {
            write!(frame, "\x1b[{};{}H", row + 1, column + 1)?;
        }
    }
    if active_link.is_some() {
        write_osc8(frame, None);
    }
    Ok(())
}

pub(super) fn write_osc8(out: &mut String, link: Option<&Hyperlink>) {
    match link {
        None => out.push_str("\x1b]8;;\x1b\\"),
        Some(link) => {
            out.push_str("\x1b]8;");
            if let Some(id) = &link.id {
                out.push_str("id=");
                out.push_str(id);
            }
            out.push(';');
            out.push_str(&link.uri);
            out.push_str("\x1b\\");
        }
    }
}
