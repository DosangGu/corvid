use std::ops::Range;

use gpui::{
    App, Bounds, FontStyle, FontWeight, Pixels, Point, SharedString, TextRun, UnderlineStyle,
    Window, fill, font, point, px, rgb, size,
};
use vt100::{Cell, Color, Screen};

#[cfg(test)]
use crate::terminal_fonts::DEFAULT_FONT;
use crate::terminal_session::Grid;

mod graphic;

pub const BACKGROUND: u32 = 0x12151a;
const FOREGROUND: u32 = 0xe2e8f0;

#[cfg(test)]
mod performance;

#[derive(Clone, Copy)]
pub struct Metrics {
    column: Pixels,
    pub row: Pixels,
    font_size: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window, family: &SharedString, font_size: Pixels) -> Self {
        let line = window.text_system().shape_line(
            "M".into(),
            font_size,
            &[text_run(1, Style::default(), family)],
            None,
        );
        Self {
            column: line.width.max(px(1.)),
            row: (line.ascent + line.descent).max(font_size),
            font_size,
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
        let mut fg = color(cell.fgcolor(), FOREGROUND);
        let mut bg = color(cell.bgcolor(), BACKGROUND);
        if cell.inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.dim() {
            fg = dim_foreground(fg, bg);
        }
        Self {
            fg,
            bg,
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
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
                    style.bg = 0x315773;
                    style.fg = FOREGROUND;
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
        let cursor = (selection.is_none()
            && !screen.hide_cursor()
            && screen.scrollback() == 0
            && row < grid.rows.get())
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
        window.paint_layer(bounds, |window| {
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
        window.paint_layer(bounds, |window| {
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
                    paint_text(
                        span.text,
                        span.style,
                        cell,
                        family,
                        metrics.font_size,
                        window,
                    );
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
                    rgb(0x8ae3c4),
                ));
            } else {
                let style = Style {
                    fg: 0x8ae3c4,
                    bg: 0x20332f,
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
                let _ = marked.paint(cell.origin, metrics.row, window, cx);
            }
        }
    }
}

fn paint_text(
    text: String,
    style: Style,
    cell: Bounds<Pixels>,
    family: &SharedString,
    font_size: Pixels,
    window: &mut Window,
) {
    if text.trim().is_empty() {
        return;
    }
    let run = text_run(text.len(), style, family);
    let line = window
        .text_system()
        .shape_line(text.into(), font_size, &[run], None);
    let baseline = cell.origin
        + point(
            px(0.),
            (cell.size.height - line.ascent - line.descent) / 2. + line.ascent,
        );
    for run in &line.runs {
        for glyph in &run.glyphs {
            let position = baseline + point(glyph.position.x, px(0.));
            if glyph.is_emoji {
                let _ = window.paint_emoji(position, run.font_id, glyph.id, font_size);
            } else {
                let _ = window.paint_glyph(
                    position,
                    run.font_id,
                    glyph.id,
                    font_size,
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
            baseline + point(first_x, line.descent * 0.618),
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
    let mut font = font(family.clone());
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
    const ANSI: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
        0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    let rgb = |r: u8, g: u8, b: u8| u32::from_be_bytes([0, r, g, b]);
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => rgb(r, g, b),
        Color::Idx(index @ 0..=15) => ANSI[usize::from(index)],
        Color::Idx(index @ 16..=231) => {
            let level = [0, 95, 135, 175, 215, 255];
            let index = usize::from(index - 16);
            rgb(level[index / 36], level[index / 6 % 6], level[index % 6])
        }
        Color::Idx(index) => {
            let gray = 8 + (index - 232) * 10;
            rgb(gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_text_against_the_displayed_background_until_intensity_resets() {
        let grid = Grid::new(1, 6);
        let mut parser = vt100::Parser::new(grid.rows, grid.cols, 0);
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
                ("A", 0x7a7e85, BACKGROUND),
                ("B", 0x888480, 0x204060),
                ("C", 0x888480, 0xf0c8a0),
                ("D", 0x204060, 0xf0c8a0),
            ]
        );
    }
}
