use gpui::{Bounds, PaintQuad, Pixels, Point, Rgba, Window, point, size};

pub(super) fn paint(text: &str, cell: Bounds<Pixels>, color: Rgba, window: &mut Window) -> bool {
    let mut chars = text.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }

    match ch {
        '\u{2800}'..='\u{28ff}' => {
            let mask = (u32::from(ch) - 0x2800) as u8;
            paint_braille(mask, cell, color, window);
        }
        _ => {
            let mut paint = |left, top, right, bottom| {
                let bounds = Bounds::from_corners(
                    cell.origin + point(cell.size.width * left, cell.size.height * top),
                    cell.origin + point(cell.size.width * right, cell.size.height * bottom),
                );
                window.paint_quad(
                    PaintQuad::new(snap(bounds, window.scale_factor())).background(color),
                );
            };
            if !block_regions(ch, &mut paint) {
                return false;
            }
        }
    }
    true
}

pub(super) fn snap(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    let edge = |point: Point<Pixels>| (point * scale).map(|p| p.round()) / scale;
    Bounds::from_corners(edge(bounds.origin), edge(bounds.bottom_right()))
}

fn block_regions(ch: char, mut rect: impl FnMut(f32, f32, f32, f32)) -> bool {
    match ch {
        '▀' => rect(0., 0., 1., 0.5),
        '▁'..='█' => {
            let height = (ch as u32 - '▁' as u32 + 1) as f32 / 8.;
            rect(0., 1. - height, 1., 1.);
        }
        '▉'..='▏' => {
            let width = ('▏' as u32 - ch as u32 + 1) as f32 / 8.;
            rect(0., 0., width, 1.);
        }
        '▐' => rect(0.5, 0., 1., 1.),
        '▔' => rect(0., 0., 1., 0.125),
        '▕' => rect(0.875, 0., 1., 1.),
        '▖'..='▟' => {
            // Bits mark the upper-left, upper-right, lower-left, and lower-right quadrants.
            const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
            let quadrants = QUADRANTS[(ch as u32 - '▖' as u32) as usize];
            for index in 0..4 {
                if quadrants & (1 << index) != 0 {
                    let left = (index % 2) as f32 * 0.5;
                    let top = (index / 2) as f32 * 0.5;
                    rect(left, top, left + 0.5, top + 0.5);
                }
            }
        }
        _ => return false,
    }
    true
}

fn braille_dots(mask: u8, cell: Bounds<Pixels>) -> impl Iterator<Item = Point<Pixels>> {
    // Unicode orders dots 1–3 and 4–6 down each column, then dots 7 and 8.
    const POSITIONS: [(f32, f32); 8] = [
        (0.25, 0.125),
        (0.25, 0.375),
        (0.25, 0.625),
        (0.75, 0.125),
        (0.75, 0.375),
        (0.75, 0.625),
        (0.25, 0.875),
        (0.75, 0.875),
    ];
    POSITIONS
        .into_iter()
        .enumerate()
        .filter(move |(bit, _)| mask & (1 << bit) != 0)
        .map(move |(_, (x, y))| cell.origin + point(cell.size.width * x, cell.size.height * y))
}

fn paint_braille(mask: u8, cell: Bounds<Pixels>, color: Rgba, window: &mut Window) {
    let radius = (cell.size.width / 2.).min(cell.size.height / 4.) * 0.27;
    let offset = point(radius, radius);
    let diameter = radius * 2.;
    for center in braille_dots(mask, cell) {
        let dot = Bounds::new(center - offset, size(diameter, diameter));
        window.paint_quad(PaintQuad::new(dot).background(color).corner_radii(radius));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn fills_full_and_complementary_half_blocks() {
        let mut regions = Vec::new();
        for ch in ['█', '▀', '▄', '▌', '▐'] {
            assert!(block_regions(ch, |l, t, r, b| regions.push((l, t, r, b))));
        }
        assert_eq!(
            regions,
            [
                (0., 0., 1., 1.),
                (0., 0., 1., 0.5),
                (0., 0.5, 1., 1.),
                (0., 0., 0.5, 1.),
                (0.5, 0., 1., 1.)
            ]
        );
    }

    #[test]
    fn snaps_adjacent_cells_to_the_same_edge_at_fractional_scale() {
        let cell = Bounds::new(point(px(4.2), px(3.6)), size(px(8.25), px(17.6)));
        let next = Bounds::new(cell.bottom_left(), cell.size);
        assert_eq!(snap(cell, 1.25).bottom_left(), snap(next, 1.25).origin);
    }

    #[test]
    fn spaces_braille_dots_evenly_across_cell_edges() {
        let cell = Bounds::new(point(px(0.), px(0.)), size(px(8.), px(16.)));
        let below = Bounds::new(cell.bottom_left(), cell.size);
        let right = Bounds::new(cell.top_right(), cell.size);
        let dots: Vec<_> = braille_dots(0xff, cell).collect();
        assert_eq!(dots[0], point(px(2.), px(2.)));
        assert_eq!(dots[6], point(px(2.), px(14.)));
        assert_eq!(dots[7], point(px(6.), px(14.)));
        assert_eq!(
            braille_dots(1, below).next().unwrap() - dots[6],
            point(px(0.), px(4.))
        );
        assert_eq!(
            braille_dots(1, right).next().unwrap() - dots[3],
            point(px(4.), px(0.))
        );
        assert_eq!(braille_dots(0, cell).count(), 0);
    }
}
