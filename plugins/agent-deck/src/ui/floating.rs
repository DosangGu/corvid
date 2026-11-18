use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use corvid_companion::{
    PRESENTATION_PAGE_SIZE,
    priority::{Priority, prioritize},
};

use super::*;
use crate::shell::{BAR_HEIGHT, GAP, GridLayout, KEY_WIDTH, Orientation, Preset};

impl DeckView {
    pub(super) fn keyboard_actions(&self) -> Vec<Option<FocusAction>> {
        if !self.window.expanded {
            return self
                .mini_tiles()
                .iter()
                .map(|tile| self.tile_action(*tile))
                .collect();
        }
        match self.section {
            Section::Slots => self
                .view
                .snapshot
                .iter()
                .flat_map(|snapshot| {
                    snapshot
                        .data
                        .slots
                        .iter()
                        .map(|slot| self.focus_action(snapshot, Some(slot), slot.run.as_ref()))
                })
                .collect(),
            Section::Overflow => self
                .queue_tiles()
                .iter()
                .map(|tile| self.tile_action(*tile))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn floating(&self, cx: &mut Context<Self>) -> Div {
        let mut grid = column().px(px(12.)).py(px(8.)).gap(px(GAP));
        let layout = self.window.layout();
        let (columns, _) = layout.dimensions();
        let tiles = self.mini_tiles();
        for (row_index, cells) in tiles.chunks(columns).enumerate() {
            let mut line = row().gap(px(GAP));
            for (column, tile) in cells.iter().enumerate() {
                let index = row_index * columns + column;
                line = line.child(self.tile_card(*tile, index, KEY_WIDTH, cx));
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
        let capacity = self.window.layout().capacity();
        let mut tiles = self.ordered_tiles(0, capacity);
        tiles.resize(capacity, Tile::Empty);
        tiles
    }

    pub(super) fn queue_tiles(&self) -> Vec<Tile<'_>> {
        let size = PRESENTATION_PAGE_SIZE as usize;
        self.ordered_tiles(
            self.window.layout().capacity() + self.page as usize * size,
            size,
        )
    }

    pub(super) fn queue_len(&self) -> usize {
        self.deck_order
            .len()
            .saturating_sub(self.window.layout().capacity())
    }

    pub(super) fn queue_pages(&self) -> u32 {
        let suppressed = self
            .view
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.data.suppressed_count);
        (self.queue_len() as u32)
            .max(suppressed)
            .div_ceil(PRESENTATION_PAGE_SIZE)
            .max(1)
    }

    fn ordered_tiles(&self, offset: usize, count: usize) -> Vec<Tile<'_>> {
        let available: BTreeMap<_, _> = self.available_tiles().into_iter().collect();
        self.deck_order
            .iter()
            .skip(offset)
            .take(count)
            .filter_map(|key| available.get(key).copied())
            .collect()
    }

    fn available_tiles(&self) -> Vec<(RunKey, Tile<'_>)> {
        tiles(
            self.view.snapshot.as_ref(),
            &self.catalog_view,
            self.session.catalog.1,
        )
    }

    pub(super) fn refresh_deck_order(&mut self) {
        let tiles = self.available_tiles();
        let by_key: BTreeMap<_, _> = tiles.iter().copied().collect();
        let capacity = self.window.layout().capacity();
        let now = now_ms();
        let online = self.view.link == Link::Online;
        let order = project_deck(&self.deck_order, &tiles, &by_key, capacity, now, online);
        self.queue_waiting = order
            .iter()
            .skip(capacity)
            .filter(|key| by_key[key].waiting(online))
            .count();
        self.deck_order = order;
    }

    pub(super) fn tile_action(&self, tile: Tile<'_>) -> Option<FocusAction> {
        if self.actions.is_pending() {
            return None;
        }
        match tile {
            Tile::Slot(snapshot, slot) => {
                self.focus_action(snapshot, Some(slot), slot.run.as_ref())
            }
            Tile::Queued(snapshot, run) => self.focus_action(snapshot, None, Some(run)),
            Tile::Host(id, host, run) => {
                let (controller, run) = host.focus_target(id, run)?;
                Some(FocusAction::HostDesktop {
                    controller,
                    run: run.clone(),
                })
            }
            Tile::Empty => None,
        }
    }

    pub(super) fn tile_card(
        &self,
        tile: Tile<'_>,
        index: usize,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        match tile {
            Tile::Slot(snapshot, slot) => self.card(snapshot, slot, index, width, cx),
            Tile::Queued(snapshot, run) => {
                self.run_card(snapshot, Some(run), None, index, width, cx)
            }
            Tile::Host(_, host, run) => {
                self.host_card(host, run, index, width, self.tile_action(tile), cx)
            }
            Tile::Empty => self.attach_card_action(
                super::deck::card_face(None, false, false, width, crate::shell::KEY_HEIGHT, None),
                index,
                None,
                cx,
            ),
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
        let overflow = self.queue_len();
        row()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .px(px(12.))
            .overflow_hidden()
            .bg(rgb(PANEL))
            .child(muted(text).flex_1().min_w_0().overflow_hidden())
            .when(!self.window.expanded && overflow > 0, |footer| {
                let pending = self.queue_waiting;
                let label = if pending == 0 {
                    format!("+{overflow}")
                } else {
                    format!("+{overflow} · {pending} waiting")
                };
                footer.child(button("open-queue", label, true).on_click(
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

#[derive(Clone, Copy)]
pub(super) enum Tile<'a> {
    Slot(&'a Arc<Snapshot>, &'a PresentedSlot),
    Queued(&'a Arc<Snapshot>, &'a PresentedRun),
    Host(Uuid, &'a crate::catalog::Host, &'a PresentedRun),
    Empty,
}

impl<'a> Tile<'a> {
    fn run(self) -> Option<&'a PresentedRun> {
        match self {
            Self::Slot(_, slot) => slot.run.as_ref(),
            Self::Queued(_, run) | Self::Host(_, _, run) => Some(run),
            Self::Empty => None,
        }
    }

    fn online(self, local_online: bool) -> bool {
        match self {
            Self::Slot(snapshot, _) | Self::Queued(snapshot, _) => {
                local_online && snapshot.generation().is_some()
            }
            Self::Host(_, host, _) => host.online,
            Self::Empty => false,
        }
    }

    fn pinned(self, key: &RunKey) -> bool {
        match self {
            Self::Slot(_, slot) => slot.pinned,
            Self::Host(_, host, _) => host.pins.contains(key),
            _ => false,
        }
    }

    fn waiting(self, local_online: bool) -> bool {
        self.online(local_online)
            && self.run().is_some_and(|run| {
                run.availability == RunAvailability::Available as i32 && run.pending_requests > 0
            })
    }
}

fn project_deck(
    previous: &[RunKey],
    tiles: &[(RunKey, Tile<'_>)],
    by_key: &BTreeMap<RunKey, Tile<'_>>,
    capacity: usize,
    now: u64,
    online: bool,
) -> Vec<RunKey> {
    let mut seen = BTreeSet::new();
    let mut order: Vec<_> = previous
        .iter()
        .copied()
        .filter(|key| by_key.contains_key(key))
        .chain(tiles.iter().map(|(key, _)| *key))
        .filter(|key| seen.insert(*key))
        .collect();
    prioritize(&mut order, capacity, now, |key, now| {
        let tile = by_key[key];
        let priority = tile.run().map_or_else(Priority::default, |run| {
            Priority::for_run(run, tile.online(online), now)
        });
        (tile.pinned(key), priority)
    });
    order
}

fn tiles<'a>(
    current: Option<&'a Arc<Snapshot>>,
    catalog: &'a crate::catalog::View,
    current_host: Uuid,
) -> Vec<(RunKey, Tile<'a>)> {
    let mut tiles = Vec::new();
    let collect = |tile: Tile<'a>| Some((RunKey::try_from(tile.run()?.key.as_ref()?).ok()?, tile));
    let current_catalog = catalog.hosts.get(&current_host).filter(|host| {
        host.loaded
            && current
                .is_none_or(|snapshot| host.matches_epoch(snapshot.data.remote_epoch.as_deref()))
    });
    let mut local = Vec::new();
    if let Some(snapshot) = current {
        local.extend(
            snapshot
                .data
                .slots
                .iter()
                .map(|slot| Tile::Slot(snapshot, slot))
                .filter_map(collect),
        );
        local.extend(
            snapshot
                .data
                .overflow
                .iter()
                .map(|run| Tile::Queued(snapshot, run))
                .filter_map(collect),
        );
    }
    local.retain(|(key, _)| key.host == current_host);
    if let Some(host) = current_catalog {
        let local: BTreeMap<_, _> = local.into_iter().collect();
        tiles.extend(host.runs.iter().filter_map(|run| {
            let (key, catalog_tile) = collect(Tile::Host(current_host, host, run))?;
            if key.host != current_host {
                return None;
            }
            // Keep exact slot actions when the local poll has the same card.
            let tile = local
                .get(&key)
                .copied()
                .filter(|tile| {
                    tile.run() == Some(run) && tile.pinned(&key) == host.pins.contains(&key)
                })
                .unwrap_or(catalog_tile);
            Some((key, tile))
        }));
    } else {
        tiles.extend(local);
    }
    for (&id, host) in &catalog.hosts {
        if id == current_host {
            continue;
        }
        tiles.extend(
            host.runs
                .iter()
                .map(|run| Tile::Host(id, host, run))
                .filter_map(collect)
                .filter(|(key, _)| key.host == id),
        );
    }
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
                overflow: (1..=7)
                    .map(|run_id| {
                        let mut run = agent(local);
                        run.key.as_mut().unwrap().agent_run_id =
                            Uuid::from_u128(run_id).to_string();
                        run
                    })
                    .collect(),
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
        let before = tiles(Some(&current), &catalog, local);
        assert_eq!(before.len(), 9, "the current host must not repeat");
        assert!(matches!(before[0].1, Tile::Slot(_, slot) if slot.pinned));
        assert!(matches!(before[7].1, Tile::Queued(_, _)));
        assert!(matches!(before[8].1, Tile::Host(id, _, _) if id == first));

        catalog
            .hosts
            .insert(second, catalog_host(second, "SSH · second"));
        let after = tiles(Some(&current), &catalog, local);
        let mut rendered = after
            .iter()
            .filter_map(|(_, tile)| match tile {
                Tile::Host(id, _, run) => Some((*id, run.key.as_ref().unwrap().host_id.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        rendered.sort();
        let mut expected = vec![(first, first.to_string()), (second, second.to_string())];
        expected.sort();
        assert_eq!(rendered, expected);
        assert_eq!(after.len(), 10);
    }

    #[test]
    fn a_remote_request_displaces_the_last_worker_and_the_queue_counts_runs() {
        let local = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let mut catalog = View::default();
        let run = |host: Uuid, id: u128, pending: u32| PresentedRun {
            key: Some(PresentedRunKey {
                host_id: host.to_string(),
                remote_epoch: epoch.to_string(),
                agent_run_id: Uuid::from_u128(id).to_string(),
            }),
            availability: RunAvailability::Available as i32,
            pending_requests: pending,
            pending_since_unix_ms: (pending > 0).then_some(1),
            ..Default::default()
        };
        for (id, runs) in [
            (
                local,
                vec![run(local, 1, 0), run(local, 2, 0), run(local, 3, 0)],
            ),
            (
                remote,
                vec![run(remote, 1, 2), run(remote, 2, 1), run(remote, 3, 1)],
            ),
        ] {
            let mut host = Host::new(&Profile {
                controller: Uuid::new_v4(),
                address: Default::default(),
                label: id.to_string(),
            });
            host.online = true;
            host.loaded = true;
            host.runs = runs;
            catalog.hosts.insert(id, Arc::new(host));
        }
        let tiles = tiles(None, &catalog, local);
        let by_key: BTreeMap<_, _> = tiles.iter().copied().collect();
        let order = project_deck(&[], &tiles, &by_key, 2, 1, true);
        assert_eq!(order[0].host, remote);
        assert_eq!(order[1].host, remote);
        assert_eq!(order[2], tiles[2].0);
        assert_eq!(order[4..], [tiles[0].0, tiles[1].0]);
        assert_eq!(
            order[2..]
                .iter()
                .filter(|key| by_key[key].waiting(true))
                .count(),
            1
        );
    }
}
