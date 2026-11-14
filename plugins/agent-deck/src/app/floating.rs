use super::*;
use crate::window::{BAR_HEIGHT, GAP, GridLayout, KEY_WIDTH, Orientation, Preset};

impl Desktop {
    pub(super) fn keyboard_actions(&self) -> Vec<Option<FocusAction>> {
        let Some(snapshot) = &self.view.snapshot else {
            return Vec::new();
        };
        if !self.window.expanded {
            return tiles(
                &snapshot.data.slots,
                &snapshot.data.overflow,
                self.window.layout().capacity(),
            )
            .into_iter()
            .map(|tile| match tile {
                Tile::Slot(slot) => self.focus_action(snapshot, Some(slot), slot.run.as_ref()),
                Tile::Queued(run) => self.focus_action(snapshot, None, Some(run)),
                Tile::Empty => None,
            })
            .collect();
        }
        match self.section {
            Section::Slots => snapshot
                .data
                .slots
                .iter()
                .map(|slot| self.focus_action(snapshot, Some(slot), slot.run.as_ref()))
                .collect(),
            Section::Overflow => snapshot
                .data
                .overflow
                .iter()
                .map(|run| self.focus_action(snapshot, None, Some(run)))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn floating(&self, cx: &mut Context<Self>) -> Div {
        let mut grid = column().px(px(12.)).py(px(8.)).gap(px(GAP));
        if let Some(snapshot) = &self.view.snapshot {
            if snapshot.page == 0 {
                let layout = self.window.layout();
                let tiles = tiles(
                    &snapshot.data.slots,
                    &snapshot.data.overflow,
                    layout.capacity(),
                );
                let (columns, _) = layout.dimensions();
                for (row_index, cells) in tiles.chunks(columns).enumerate() {
                    let mut line = row().gap(px(GAP));
                    for (column, tile) in cells.iter().enumerate() {
                        let index = row_index * columns + column;
                        line = line.child(match tile {
                            Tile::Slot(slot) => self.card(snapshot, slot, index, KEY_WIDTH, cx),
                            Tile::Queued(run) => {
                                self.run_card(snapshot, Some(run), None, index, KEY_WIDTH, cx)
                            }
                            Tile::Empty => {
                                self.run_card(snapshot, None, None, index, KEY_WIDTH, cx)
                            }
                        });
                    }
                    grid = grid.child(line);
                }
            } else {
                grid = grid.child(muted("Loading agents…"));
            }
        } else {
            grid = grid.child(muted("Connecting to agents…"));
        }
        column()
            .size_full()
            .gap_0()
            .child(self.window_header(cx))
            .child(
                div()
                    .id("mini-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(grid),
            )
            .child(self.footer(cx))
    }

    pub(super) fn footer(&self, cx: &mut Context<Self>) -> Div {
        let text = if let Some(error) = &self.window.error {
            error.clone()
        } else if self.pending.is_some() {
            "Waiting for the action…".into()
        } else if let Some(notice) = &self.notice {
            notice.message.clone()
        } else {
            connection_text(&self.view).into()
        };
        let overflow = self
            .view
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.data.overflow_count);
        row()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .px(px(12.))
            .overflow_hidden()
            .bg(rgb(PANEL))
            .child(muted(text).flex_1().min_w_0().overflow_hidden())
            .when(!self.window.expanded && overflow > 0, |footer| {
                footer.child(button("open-queue", format!("+{overflow}"), true).on_click(
                    cx.listener(|view, _, window, cx| view.expand(Section::Overflow, window, cx)),
                ))
            })
    }

    pub(super) fn layout_controls(&self, cx: &mut Context<Self>) -> Div {
        let current = self.window.layout();
        column().child(
            row()
                .flex_wrap()
                .child(muted("Deck layout"))
                .children(Preset::ALL.into_iter().enumerate().map(|(index, preset)| {
                    let layout = GridLayout { preset, ..current };
                    button(("layout", index), layout.label(), true)
                        .when(current.preset == preset, |button| {
                            button.text_color(rgb(MINT))
                        })
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.window.set_layout(layout);
                            view.update(window, cx);
                        }))
                }))
                .child(
                    button(
                        "orientation",
                        if current.orientation == Orientation::Landscape {
                            "Landscape"
                        } else {
                            "Portrait"
                        },
                        true,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        let current = view.window.layout();
                        let orientation = match current.orientation {
                            Orientation::Landscape => Orientation::Portrait,
                            Orientation::Portrait => Orientation::Landscape,
                        };
                        view.window.set_layout(GridLayout {
                            orientation,
                            ..current
                        });
                        view.update(window, cx);
                    })),
                ),
        )
    }
}

enum Tile<'a> {
    Slot(&'a PresentedSlot),
    Queued(&'a PresentedRun),
    Empty,
}

fn tiles<'a>(
    slots: &'a [PresentedSlot],
    overflow: &'a [PresentedRun],
    capacity: usize,
) -> Vec<Tile<'a>> {
    let mut tiles: Vec<_> = slots
        .iter()
        .filter(|slot| slot.run.is_some())
        .map(Tile::Slot)
        .collect();
    // Queue entries keep their exact run identity. They are never fabricated
    // slot tokens, and changing the visual grid does not edit companion policy.
    tiles.extend(overflow.iter().map(Tile::Queued));
    tiles.resize_with(tiles.len().max(capacity), || Tile::Empty);
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn larger_grids_show_queued_runs_without_fabricating_slot_tokens() {
        let slots = vec![PresentedSlot {
            slot_id: 1,
            revision: 42,
            pinned: true,
            run: Some(PresentedRun::default()),
        }];
        let overflow = vec![PresentedRun::default(); 9];
        let large = tiles(&slots, &overflow, 15);
        assert_eq!(large.len(), 15);
        assert!(
            matches!(large[0], Tile::Slot(slot) if std::ptr::eq(slot, &slots[0]) && slot.pinned)
        );
        assert!(matches!(large[9], Tile::Queued(run) if std::ptr::eq(run, &overflow[8])));
        assert!(large[10..].iter().all(|tile| matches!(tile, Tile::Empty)));
        let small = tiles(&slots, &overflow, 6);
        assert_eq!(
            small.len(),
            10,
            "smaller grids retain extra agents for scrolling"
        );
    }
}
