use std::ops::Range;

use corvid_protocol::ansi_screen::{Cell, Color, Screen};
use corvid_protocol::{
    DEFAULT_ANSI_PALETTE_16, DEFAULT_TERMINAL_BACKGROUND_RGB, DEFAULT_TERMINAL_FOREGROUND_RGB,
};
use gpui::{
    App, Bounds, FontStyle, FontWeight, PathBuilder, Pixels, Point, SharedString, TextRun,
    UnderlineStyle, Window, fill, point, px, rgb, size,
};

#[cfg(test)]
use crate::terminal_fonts::DEFAULT_FONT;
use crate::terminal_session::Grid;

mod graphic;

pub const BACKGROUND: u32 = DEFAULT_TERMINAL_BACKGROUND_RGB;
const FOREGROUND: u32 = DEFAULT_TERMINAL_FOREGROUND_RGB;
const DIM_FOREGROUND: u32 = 0x8b8b95;

#[cfg(test)]
mod performance;

#[derive(Clone, Copy)]
pub struct Metrics {
    column: Pixels,
    pub row: Pixels,
    font_size: Pixels,
    baseline: Pixels,
    underline_offset: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window, family: &SharedString, font_size: Pixels) -> Self {
        let line = window.text_system().shape_line(
            "M".into(),
            font_size,
            &[text_run(1, Style::default(), family)],
            None,
        );
        Self::from_reference(line.width, line.ascent, line.descent, font_size)
    }

    fn from_reference(width: Pixels, ascent: Pixels, descent: Pixels, font_size: Pixels) -> Self {
        let row = (ascent + descent).max(font_size);
        Self {
            column: width.max(px(1.)),
            row,
            font_size,
            baseline: centered_baseline(row, ascent, descent),
            underline_offset: descent * 0.618,
        }
    }

    pub fn grid(self, bounds: Bounds<Pixels>) -> Grid {
        Grid::new(
            (f32::from(bounds.size.height) / f32::from(self.row)) as u16,
            (f32::from(bounds.size.width) / f32::from(self.column)) as u16,
        )
    }

    pub fn cell(self, origin: Point<Pixels>, row: u16, col: u16, count: u16) -> Bounds<Pixels> {
        Bounds {
            origin: origin + point(self.column * f32::from(col), self.row * f32::from(row)),
            size: size(self.column * f32::from(count), self.row),
        }
    }

    pub fn position(self, bounds: Bounds<Pixels>, point: Point<Pixels>) -> (u16, u16) {
        let grid = self.grid(bounds);
        let relative = point - bounds.origin;
        let row = ((relative.y / self.row) as u16).min(grid.rows.get() - 1);
        let col = ((relative.x / self.column) as u16).min(grid.cols.get() - 1);
        (row, col)
    }
}

#[derive(Default)]
pub struct Frame {
    spans: Vec<Span>,
    cursor: Option<(u16, u16)>,
}

struct Span {
    row: u16,
    col: u16,
    width: u16,
    style: Style,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: u32,
    bg: u32,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: FOREGROUND,
            bg: BACKGROUND,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

impl Style {
    fn from_cell(cell: &Cell) -> Self {
        let mut fg = color(cell.style.fg, FOREGROUND);
        let mut bg = color(cell.style.bg, BACKGROUND);
        if cell.style.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.style.dim {
            fg = if !cell.style.inverse
                && cell.style.fg == Color::Default
                && cell.style.bg == Color::Default
            {
                DIM_FOREGROUND
            } else {
                dim_foreground(fg, bg)
            };
        }
        Self {
            fg,
            bg,
            bold: cell.style.bold,
            italic: cell.style.italic,
            underline: cell.style.underline,
        }
    }
}

fn dim_foreground(fg: u32, bg: u32) -> u32 {
    let [_, fr, fg, fb] = fg.to_be_bytes();
    let [_, br, bg, bb] = bg.to_be_bytes();
    u32::from_be_bytes([0, fr.midpoint(br), fg.midpoint(bg), fb.midpoint(bb)])
}

impl Frame {
    pub fn new(screen: &Screen, grid: Grid) -> Self {
        Self::selected(screen, grid, None)
    }

    pub fn selected(screen: &Screen, grid: Grid, selection: Option<&Range<(u16, u16)>>) -> Self {
        let (rows, cols) = screen.size();
        let mut spans: Vec<Span> = Vec::new();
        for row in 0..rows.get().min(grid.rows.get()) {
            for col in 0..cols.get().min(grid.cols.get()) {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::from_cell(cell);
                if selection.is_some_and(|selection| selection.contains(&(row, col))) {
                    style.bg = 0x2e3642;
                    if style.fg == style.bg {
                        style.fg = FOREGROUND;
                    }
                }
                if !cell.has_contents() && style.bg == BACKGROUND {
                    continue;
                }
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                if text.len() == 1
                    && let Some(previous) = spans.last_mut()
                    && previous.row == row
                    && previous.col + previous.width == col
                    && previous.style == style
                    && previous.text.is_ascii()
                {
                    previous.text.push_str(text);
                    previous.width += 1;
                } else {
                    spans.push(Span {
                        row,
                        col,
                        width: if cell.is_wide() { 2 } else { 1 },
                        style,
                        text: text.to_owned(),
                    });
                }
            }
        }
        let (row, col) = screen.cursor_position();
        let cursor = (selection.is_none() && !screen.hide_cursor() && row < grid.rows.get())
            .then_some((row, col.min(grid.cols.get() - 1)));
        Self { spans, cursor }
    }

    pub fn paint(
        self,
        bounds: Bounds<Pixels>,
        metrics: Metrics,
        marked: Option<&str>,
        family: &SharedString,
        window: &mut Window,
        cx: &mut App,
    ) {
        // ShapedLine::paint creates a scene layer per span. A terminal can have
        // thousands of one-cell spans; inserting each into GPUI's bounds tree
        // is costly on the UI thread. Batch backgrounds and glyphs in separate layers.
        window.paint_layer(|window| {
            for span in &self.spans {
                let cell = metrics.cell(bounds.origin, span.row, span.col, span.width);
                if span.style.bg != BACKGROUND {
                    window.paint_quad(fill(
                        graphic::snap(cell, window.scale_factor()),
                        rgb(span.style.bg),
                    ));
                }
            }
        });
        window.paint_layer(|window| {
            for span in self.spans {
                let cell = metrics.cell(bounds.origin, span.row, span.col, span.width);
                if graphic::paint(&span.text, cell, rgb(span.style.fg), window) {
                    if span.style.underline {
                        window.paint_quad(fill(
                            Bounds::new(
                                cell.origin + point(px(0.), metrics.row - px(2.)),
                                size(cell.size.width, px(1.)),
                            ),
                            rgb(span.style.fg),
                        ));
                    }
                } else {
                    paint_text(span.text, span.style, cell, family, metrics, window);
                }
            }
        });
        if let Some(marked) = marked
            && let Some((row, col)) = self.cursor
        {
            let cell = metrics.cell(bounds.origin, row, col, 1);
            if marked.is_empty() {
                window.paint_quad(fill(
                    Bounds {
                        origin: cell.origin,
                        size: size(px(2.), metrics.row),
                    },
                    rgb(0xe4e4e7),
                ));
            } else {
                let style = Style {
                    fg: 0xededf0,
                    bg: 0x252529,
                    underline: true,
                    ..Style::default()
                };
                let marked = window.text_system().shape_line(
                    marked.to_owned().into(),
                    metrics.font_size,
                    &[text_run(marked.len(), style, family)],
                    None,
                );
                window.paint_quad(fill(
                    Bounds {
                        origin: cell.origin,
                        size: size(marked.width, metrics.row),
                    },
                    rgb(style.bg),
                ));
                let marked_baseline = centered_baseline(metrics.row, marked.ascent, marked.descent);
                let origin = cell.origin + point(px(0.), metrics.baseline - marked_baseline);
                let _ = marked.paint(origin, metrics.row, gpui::TextAlign::Left, None, window, cx);
            }
        }
    }
}

pub fn link_underline(
    bounds: Bounds<Pixels>,
    metrics: Metrics,
    spans: &[Range<(u16, u16)>],
    window: &mut Window,
) {
    window.paint_layer(|window| {
        for range in spans {
            for row in range.start.0..=range.end.0 {
                let start = if row == range.start.0 {
                    range.start.1
                } else {
                    0
                };
                let end = range.end.1;
                if end <= start {
                    continue;
                }
                let line = metrics.cell(bounds.origin, row, start, end - start);
                let mut path = PathBuilder::stroke(px(1.25)).dash_array(&[px(1.5), px(2.)]);
                let origin = line.origin + point(px(0.), metrics.row - px(2.));
                path.move_to(origin);
                path.line_to(origin + point(line.size.width, px(0.)));
                if let Ok(path) = path.build() {
                    window.paint_path(path, rgb(0x93b8e8));
                }
            }
        }
    });
}

fn centered_baseline(row: Pixels, ascent: Pixels, descent: Pixels) -> Pixels {
    (row - ascent - descent) / 2. + ascent
}

fn paint_text(
    text: String,
    style: Style,
    cell: Bounds<Pixels>,
    family: &SharedString,
    metrics: Metrics,
    window: &mut Window,
) {
    if text.trim().is_empty() {
        return;
    }
    let run = text_run(text.len(), style, family);
    let line = window
        .text_system()
        .shape_line(text.into(), metrics.font_size, &[run], None);
    // Text with different fallback faces still shares the row's baseline.
    let baseline = cell.origin + point(px(0.), metrics.baseline);
    for run in &line.runs {
        for glyph in &run.glyphs {
            let position = baseline + point(glyph.position.x, px(0.));
            if glyph.is_emoji {
                let _ = window.paint_emoji(position, run.font_id, glyph.id, metrics.font_size);
            } else {
                let _ = window.paint_glyph(
                    position,
                    run.font_id,
                    glyph.id,
                    metrics.font_size,
                    rgb(style.fg).into(),
                );
            }
        }
    }
    if style.underline {
        let first_x = line
            .runs
            .iter()
            .flat_map(|run| &run.glyphs)
            .next()
            .map_or(px(0.), |glyph| glyph.position.x);
        window.paint_underline(
            baseline + point(first_x, metrics.underline_offset),
            line.width,
            &UnderlineStyle {
                thickness: px(1.),
                color: Some(rgb(style.fg).into()),
                wavy: false,
            },
        );
    }
}

fn text_run(len: usize, style: Style, family: &SharedString) -> TextRun {
    let mut font = crate::desktop_font::with_symbols(family.clone());
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: rgb(style.fg).into(),
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        }),
        strikethrough: None,
    }
}

fn color(color: Color, default: u32) -> u32 {
    let rgb = |r: u8, g: u8, b: u8| u32::from_be_bytes([0, r, g, b]);
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => rgb(r, g, b),
        Color::Indexed(index @ 0..=15) => DEFAULT_ANSI_PALETTE_16[usize::from(index)],
        Color::Indexed(index @ 16..=231) => {
            let level = [0, 95, 135, 175, 215, 255];
            let index = usize::from(index - 16);
            rgb(level[index / 36], level[index / 6 % 6], level[index % 6])
        }
        Color::Indexed(index) => {
            let gray = 8 + (index - 232) * 10;
            rgb(gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_row_baseline_and_underline_from_the_reference_font() {
        let metrics = Metrics::from_reference(px(9.), px(12.), px(3.), px(17.));

        assert_eq!(metrics.row, px(17.));
        assert_eq!(metrics.baseline, px(13.));
        assert_eq!(metrics.underline_offset, px(3.) * 0.618);
    }

    #[test]
    fn dims_text_against_the_displayed_background_until_intensity_resets() {
        let grid = Grid::new(1, 6);
        let mut parser = corvid_protocol::ansi_screen::Decoder::new(grid.rows, grid.cols);
        parser.process(b"\x1b[2mA\x1b[38;2;240;200;160;48;2;32;64;96mB\x1b[7mC\x1b[22mD");

        let frame = Frame::new(parser.screen(), grid);
        let colors: Vec<_> = frame
            .spans
            .iter()
            .map(|span| (span.text.as_str(), span.style.fg, span.style.bg))
            .collect();
        assert_eq!(
            colors,
            [
                ("A", DIM_FOREGROUND, BACKGROUND),
                ("B", 0x888480, 0x204060),
                ("C", 0x888480, 0xf0c8a0),
                ("D", 0x204060, 0xf0c8a0),
            ]
        );
    }
}
