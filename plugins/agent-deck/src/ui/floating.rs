use std::sync::Arc;

use super::*;
use crate::shell::{BAR_HEIGHT, GAP, GridLayout, KEY_WIDTH, Orientation, Preset};

impl DeckView {
    pub(super) fn keyboard_actions(&self) -> Vec<Option<FocusAction>> {
        if !self.window.expanded {
            return self
                .mini_tiles()
                .iter()
                .map(|tile| self.mini_action(tile))
                .collect();
        }
        let Some(snapshot) = &self.view.snapshot else {
            return Vec::new();
        };
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
        let layout = self.window.layout();
        let (columns, _) = layout.dimensions();
        for (row_index, cells) in self.mini_tiles().chunks(columns).enumerate() {
            let mut line = row().gap(px(GAP));
            for (column, tile) in cells.iter().enumerate() {
                let index = row_index * columns + column;
                line = line.child(match tile {
                    Tile::Slot(snapshot, slot) => self.card(snapshot, slot, index, KEY_WIDTH, cx),
                    Tile::Queued(snapshot, run) => {
                        self.run_card(snapshot, Some(run), None, index, KEY_WIDTH, cx)
                    }
                    Tile::Host(_, host, run) => {
                        self.host_card(host, run, index, self.mini_action(tile), cx)
                    }
                    Tile::Empty => self.attach_card_action(
                        super::deck::card_face(
                            None,
                            false,
                            false,
                            KEY_WIDTH,
                            crate::shell::KEY_HEIGHT,
                            None,
                        ),
                        index,
                        None,
                        cx,
                    ),
                });
            }
            grid = grid.child(line);
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

    fn mini_tiles(&self) -> Vec<Tile<'_>> {
        tiles(
            self.view
                .snapshot
                .as_ref()
                .filter(|snapshot| snapshot.page == 0),
            &self.catalog_view,
            self.session.catalog.1,
            self.window.layout().capacity(),
        )
    }

    fn mini_action(&self, tile: &Tile<'_>) -> Option<FocusAction> {
        if self.actions.is_pending() {
            return None;
        }
        match tile {
            Tile::Slot(snapshot, slot) => {
                self.focus_action(snapshot, Some(slot), slot.run.as_ref())
            }
            Tile::Queued(snapshot, run) => self.focus_action(snapshot, None, Some(run)),
            Tile::Host(id, host, run) => {
                let (controller, run) = host.focus_target(*id, run)?;
                Some(FocusAction::HostDesktop {
                    controller,
                    run: run.clone(),
                })
            }
            Tile::Empty => None,
        }
    }

    pub(super) fn footer(&self, cx: &mut Context<Self>) -> Div {
        let text = if let Some(error) = &self.window.error {
            error.clone()
        } else if self.actions.is_pending() {
            "Waiting for the action…".into()
        } else if let Some(notice) = self.actions.notice() {
            notice.message.clone()
        } else {
            connection_text(&self.view).unwrap_or_default().into()
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
                .child(muted("Columns × rows"))
                .children(Preset::ALL.into_iter().enumerate().map(|(index, preset)| {
                    let layout = GridLayout { preset, ..current };
                    button(("layout", index), layout.label(), true)
                        .when(current.preset == preset, |button| {
                            button.text_color(rgb(MINT))
                        })
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.window.set_layout(GridLayout {
                                preset,
                                ..view.window.layout()
                            });
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
    Slot(&'a Arc<Snapshot>, &'a PresentedSlot),
    Queued(&'a Arc<Snapshot>, &'a PresentedRun),
    Host(Uuid, &'a crate::catalog::Host, &'a PresentedRun),
    Empty,
}

fn tiles<'a>(
    current: Option<&'a Arc<Snapshot>>,
    catalog: &'a crate::catalog::View,
    current_host: Uuid,
    capacity: usize,
) -> Vec<Tile<'a>> {
    let mut tiles = Vec::new();
    if let Some(snapshot) = current {
        tiles.extend(
            snapshot
                .data
                .slots
                .iter()
                .filter(|slot| slot.run.is_some())
                .map(|slot| Tile::Slot(snapshot, slot)),
        );
        tiles.extend(
            snapshot
                .data
                .overflow
                .iter()
                .map(|run| Tile::Queued(snapshot, run)),
        );
    }
    for (&id, host) in &catalog.hosts {
        if id != current_host || current.is_none() {
            tiles.extend(host.runs.iter().map(|run| Tile::Host(id, host, run)));
        }
    }
    tiles.resize_with(tiles.len().max(capacity), || Tile::Empty);
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Host, View};
    use crate::hosts::Profile;

    #[test]
    fn the_main_grid_combines_hosts_without_repeating_its_own_slots() {
        let local = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let repeated_run = Uuid::new_v4().to_string();
        let repeated_epoch = Uuid::new_v4().to_string();
        let agent = |id: Uuid| PresentedRun {
            key: Some(PresentedRunKey {
                host_id: id.to_string(),
                remote_epoch: repeated_epoch.clone(),
                agent_run_id: repeated_run.clone(),
            }),
            ..Default::default()
        };
        let current = Arc::new(Snapshot {
            link: Uuid::new_v4(),
            host: local,
            page: 0,
            data: corvid_protocol::wire::PresentationSlots {
                slots: vec![PresentedSlot {
                    slot_id: 1,
                    revision: 42,
                    pinned: true,
                    run: Some(agent(local)),
                }],
                overflow: vec![agent(local); 7],
                ..Default::default()
            },
            #[cfg(windows)]
            can_pair_wezterm: false,
        });
        let catalog_host = |id, label: &str| {
            let mut host = Host::new(&Profile {
                controller: Uuid::new_v4(),
                address: Default::default(),
                label: label.into(),
            });
            host.runs.push(agent(id));
            Arc::new(host)
        };
        let mut catalog = View::default();
        catalog.hosts.insert(local, catalog_host(local, "Local"));
        catalog
            .hosts
            .insert(first, catalog_host(first, "SSH · first"));
        let before = tiles(Some(&current), &catalog, local, 6);
        assert_eq!(before.len(), 9, "the current host must not repeat");
        assert!(matches!(before[0], Tile::Slot(_, slot) if slot.pinned));
        assert!(matches!(before[7], Tile::Queued(_, _)));
        assert!(matches!(before[8], Tile::Host(id, _, _) if id == first));

        catalog
            .hosts
            .insert(second, catalog_host(second, "SSH · second"));
        let after = tiles(Some(&current), &catalog, local, 15);
        let mut rendered = after
            .iter()
            .filter_map(|tile| match tile {
                Tile::Host(id, _, run) => Some((*id, run.key.as_ref().unwrap().host_id.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        rendered.sort();
        let mut expected = vec![(first, first.to_string()), (second, second.to_string())];
        expected.sort();
        assert_eq!(rendered, expected);
        assert_eq!(after.len(), 15);
        assert!(after[10..].iter().all(|tile| matches!(tile, Tile::Empty)));
    }
}
