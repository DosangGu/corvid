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
                .mini_tiles()
                .iter()
                .map(|tile| self.tile_action(*tile))
                .collect(),
            Section::Overflow => self
                .queue_tiles()
                .iter()
                .map(|tile| self.tile_action(*tile))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn floating(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut grid = column().px(px(12.)).py(px(8.)).gap(px(GAP));
        let layout = self.window.layout();
        let (columns, _) = layout.dimensions();
        let tiles = self.mini_tiles();
        for (row_index, cells) in tiles.chunks(columns).enumerate() {
            let mut line = row().gap(px(GAP));
            for (column, tile) in cells.iter().enumerate() {
                let index = row_index * columns + column;
                line = line.child(self.tile_card(*tile, index, KEY_WIDTH, window, cx));
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
            .when(!cfg!(target_os = "macos"), |view| {
                view.child(self.footer(cx))
            })
    }

    pub(super) fn mini_tiles(&self) -> Vec<Tile<'_>> {
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
        let suppressed = self.hidden_runs().count();
        (self.queue_len().max(suppressed) as u32)
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

    pub(super) fn hidden_runs(&self) -> impl Iterator<Item = (&Arc<Snapshot>, &PresentedRunKey)> {
        self.catalog_view
            .hosts
            .values()
            .flat_map(|host| host.pages.iter())
            .flat_map(|snapshot| {
                snapshot
                    .data
                    .suppressed
                    .iter()
                    .map(move |key| (snapshot, key))
            })
    }

    fn available_tiles(&self) -> Vec<(RunKey, Tile<'_>)> {
        tiles(&self.catalog_view)
    }

    pub(super) fn refresh_deck_order(&mut self) {
        let tiles = self.available_tiles();
        let by_key: BTreeMap<_, _> = tiles.iter().copied().collect();
        let capacity = self.window.layout().capacity();
        let now = now_ms();
        let order = project_deck(&self.deck_order, &tiles, &by_key, capacity, now);
        self.queue_waiting = order
            .iter()
            .skip(capacity)
            .filter(|key| by_key[key].waiting())
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
            Tile::Empty => None,
        }
    }

    pub(super) fn tile_card(
        &self,
        tile: Tile<'_>,
        index: usize,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = self.colors;
        match tile {
            Tile::Slot(snapshot, slot) => self.card(snapshot, slot, index, width, window, cx),
            Tile::Queued(snapshot, run) => {
                self.run_card(snapshot, Some(run), None, index, width, window, cx)
            }
            Tile::Empty => self.attach_card_action(
                colors.card_face(
                    None,
                    false,
                    false,
                    index,
                    width,
                    if self.window.expanded {
                        width
                    } else {
                        crate::shell::KEY_HEIGHT
                    },
                    window,
                ),
                index,
                None,
                cx,
            ),
        }
    }

    pub(super) fn status_message(&self) -> Option<&str> {
        if let Some(error) = &self.window.error {
            Some(error)
        } else if self.actions.is_pending() {
            Some("Waiting for the action…")
        } else if let Some(notice) = self.actions.notice() {
            Some(&notice.message)
        } else {
            connection_text(&self.view)
        }
    }

    pub(super) fn footer(&self, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let text = self.status_message().unwrap_or_default().to_owned();
        row()
            .h(px(BAR_HEIGHT))
            .flex_shrink_0()
            .px(px(12.))
            .overflow_hidden()
            .bg(colors.bar)
            .child(colors.muted(text).flex_1().min_w_0().overflow_hidden())
            .when(!self.window.expanded && self.queue_len() > 0, |footer| {
                footer.child(self.queue_button(false, cx))
            })
    }

    pub(super) fn layout_controls(&self, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let current = self.window.layout();
        column().child(
            row()
                .flex_wrap()
                .child(colors.muted("Columns × rows"))
                .children(Preset::ALL.into_iter().enumerate().map(|(index, preset)| {
                    let layout = GridLayout { preset, ..current };
                    colors
                        .button(("layout", index), layout.label(), true)
                        .when(current.preset == preset, |button| {
                            button.text_color(colors.accent)
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
                    colors
                        .button(
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
    Empty,
}

impl<'a> Tile<'a> {
    fn run(self) -> Option<&'a PresentedRun> {
        match self {
            Self::Slot(_, slot) => slot.run.as_ref(),
            Self::Queued(_, run) => Some(run),
            Self::Empty => None,
        }
    }

    fn pinned(self) -> bool {
        match self {
            Self::Slot(_, slot) => slot.pinned,
            _ => false,
        }
    }

    fn waiting(self) -> bool {
        self.run().is_some_and(|run| {
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
        let priority = tile
            .run()
            .map_or_else(Priority::default, |run| Priority::for_run(run, true, now));
        (tile.pinned(), priority)
    });
    order
}

fn tiles(catalog: &crate::catalog::View) -> Vec<(RunKey, Tile<'_>)> {
    let keyed = |tile: Tile<'_>| RunKey::try_from(tile.run()?.key.as_ref()?).ok();
    let mut tiles = Vec::new();
    for host in catalog
        .hosts
        .values()
        .filter(|host| host.link == Link::Online)
    {
        if let Some(snapshot) = host
            .pages
            .first()
            .filter(|snapshot| snapshot.generation().is_some())
        {
            tiles.extend(snapshot.data.slots.iter().filter_map(|slot| {
                let tile = Tile::Slot(snapshot, slot);
                Some((keyed(tile)?, tile))
            }));
            tiles.extend(host.pages.iter().flat_map(|page| {
                page.data.overflow.iter().filter_map(|run| {
                    let tile = Tile::Queued(page, run);
                    Some((keyed(tile)?, tile))
                })
            }));
        }
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Host, View};

    fn run(host: Uuid, epoch: u128, id: u128) -> PresentedRun {
        PresentedRun {
            key: Some(
                RunKey {
                    host,
                    epoch: Uuid::from_u128(epoch),
                    run: Uuid::from_u128(id),
                }
                .into(),
            ),
            availability: RunAvailability::Available as i32,
            ..Default::default()
        }
    }

    fn page(host: Uuid, page: u32, epoch: u128) -> Snapshot {
        Snapshot {
            link: Uuid::from_u128(1),
            host,
            page,
            data: corvid_protocol::wire::PresentationSlots {
                connection: CompanionConnection::Ready as i32,
                control_generation: Some(Uuid::new_v4().to_string()),
                remote_epoch: Some(Uuid::from_u128(epoch).to_string()),
                ..Default::default()
            },
            #[cfg(windows)]
            can_pair_wezterm: false,
        }
    }

    #[test]
    fn the_grid_uses_all_ready_host_pages_and_drops_disconnected_cards() {
        let local = Uuid::from_u128(1);
        let remote = Uuid::from_u128(2);
        let mut local_page = page(local, 0, 1);
        local_page.data.slots.push(PresentedSlot {
            run: Some(run(local, 1, 1)),
            ..Default::default()
        });
        let mut remote_page = page(remote, 0, 1);
        remote_page.data.slots.push(PresentedSlot {
            pinned: true,
            run: Some(run(remote, 1, 1)),
            ..Default::default()
        });
        let mut overflow = page(remote, 1, 1);
        overflow.data.slots = remote_page.data.slots.clone();
        overflow.data.control_generation = remote_page.data.control_generation.clone();
        overflow.data.overflow.push(run(remote, 1, 2));
        let mut catalog = View::default();
        catalog.hosts.insert(
            local,
            Arc::new(Host {
                label: "Local".into(),
                link: Link::Online,
                pages: vec![Arc::new(local_page)],
            }),
        );
        catalog.hosts.insert(
            remote,
            Arc::new(Host {
                label: "SSH".into(),
                link: Link::Online,
                pages: vec![Arc::new(remote_page), Arc::new(overflow)],
            }),
        );
        let (keys, values): (Vec<_>, Vec<_>) = tiles(&catalog).into_iter().unzip();
        assert_eq!(
            keys.iter().map(|key| key.host).collect::<Vec<_>>(),
            [local, remote, remote]
        );
        assert!(values[1].pinned());

        let host = Arc::make_mut(catalog.hosts.get_mut(&remote).unwrap());
        let old = host.pages[0].clone();
        host.link = Link::Unavailable;
        assert_eq!(tiles(&catalog).len(), 1);

        let mut replacement = page(remote, 0, 2);
        replacement.data.overflow.push(run(remote, 2, 1));
        let host = Arc::make_mut(catalog.hosts.get_mut(&remote).unwrap());
        host.link = Link::Online;
        host.pages = vec![Arc::new(replacement)];
        assert!(!host.accepts(&old));
        let ready = tiles(&catalog);
        let by_key = ready.iter().copied().collect();
        let order = project_deck(&keys, &ready, &by_key, 1, 0);
        assert_eq!(order.len(), 2);
        assert_eq!(order[1].epoch, Uuid::from_u128(2));
    }

    #[test]
    fn an_input_request_on_a_remote_host_promotes_the_card_into_the_grid() {
        let local = Uuid::from_u128(1);
        let remote = Uuid::from_u128(2);
        let mut catalog = View::default();
        for host in [local, remote] {
            let mut ready = page(host, 0, 1);
            for id in 1..=3 {
                let mut run = run(host, 1, id);
                if host == remote {
                    run.pending_requests = 1;
                    run.pending_since_unix_ms = Some(1);
                }
                ready.data.overflow.push(run);
            }
            catalog.hosts.insert(
                host,
                Arc::new(Host {
                    label: host.to_string(),
                    link: Link::Online,
                    pages: vec![Arc::new(ready)],
                }),
            );
        }
        let tiles = tiles(&catalog);
        let by_key: BTreeMap<_, _> = tiles.iter().copied().collect();
        let order = project_deck(&[], &tiles, &by_key, 2, 1);
        assert_eq!(order[0].host, remote);
        assert_eq!(order[1].host, remote);
        assert_eq!(order[2], tiles[2].0);
        assert_eq!(order[4..], [tiles[0].0, tiles[1].0]);
        assert_eq!(
            order[2..]
                .iter()
                .filter(|key| by_key[key].waiting())
                .count(),
            1
        );
    }
}
