use super::*;
use crate::window::{BAR_HEIGHT, GAP, GridLayout, KEY_WIDTH, Orientation, Preset};

impl Desktop {
    pub(super) fn floating(&mut self, ctx: &egui::Context, tray: bool) {
        let escape = ctx.input(|input| input.key_pressed(egui::Key::Escape))
            && !egui::Popup::is_any_open(ctx);
        let panel = Frame::side_top_panel(&ctx.style()).inner_margin(Margin::symmetric(12, 6));
        egui::TopBottomPanel::top("mini-header")
            .exact_height(BAR_HEIGHT)
            .frame(panel)
            .show(ctx, |ui| self.floating_header(ui, tray));
        egui::TopBottomPanel::bottom("mini-status")
            .exact_height(BAR_HEIGHT)
            .frame(panel)
            .show(ctx, |ui| {
                self.floating_status(ui);
            });
        let content = Frame::central_panel(&ctx.style())
            .fill(deck::BACKGROUND)
            .inner_margin(Margin::symmetric(12, 8));
        egui::CentralPanel::default()
            .frame(content)
            .show(ctx, |ui| {
                let Some(snapshot) = self.view.snapshot.clone() else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(18.0);
                        ui.label("Connecting to agents…");
                        ui.weak("Open More to check the connection.");
                    });
                    return;
                };
                if snapshot.page != 0 {
                    ui.label("Loading agents…");
                    return;
                }
                self.floating_board(ui, &snapshot);
            });
        if escape && tray && !gesture_active(ctx) {
            self.window.hide(ctx);
        }
    }

    fn floating_board(&mut self, ui: &mut Ui, snapshot: &Snapshot) {
        let layout = self.window.layout();
        let tiles = tiles(
            &snapshot.data.slots,
            &snapshot.data.overflow,
            layout.capacity(),
        );
        let (columns, _) = layout.dimensions();
        let empty_hint = "Run Codex or Claude in a Corvid terminal to fill the deck.";
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
                let keys_width = ui.available_width() - GAP * (columns - 1) as f32;
                let width = KEY_WIDTH.min(keys_width / columns as f32);
                for (row_index, row) in tiles.chunks(columns).enumerate() {
                    ui.horizontal(|ui| {
                        for (column, tile) in row.iter().enumerate() {
                            let index = row_index * columns + column;
                            ui.push_id(("mini-key", index), |ui| match tile {
                                Tile::Slot(slot) => self.card(ui, snapshot, slot, width),
                                Tile::Queued(number, run) => {
                                    self.queued_key(ui, snapshot, *number, run, width);
                                }
                                Tile::Empty => {
                                    ui.add(deck::Key {
                                        number: index as u32 + 1,
                                        run: None,
                                        pinned: false,
                                        queued: false,
                                        edge: width,
                                        cached: false,
                                        can_focus: false,
                                        compact: true,
                                    })
                                    .on_hover_text(empty_hint);
                                }
                            });
                        }
                    });
                }
            });
    }

    fn queued_key(
        &mut self,
        ui: &mut Ui,
        snapshot: &Snapshot,
        number: u32,
        run: &PresentedRun,
        width: f32,
    ) {
        let key = snapshot.live_key(run);
        let can_focus = self.ready(snapshot) && self.parent.connected() && key.is_some();
        let cached = self.view.link != Link::Online || snapshot.generation().is_none();
        let response = ui.add(deck::Key {
            number,
            run: Some(run),
            pinned: false,
            queued: true,
            edge: width,
            cached,
            can_focus,
            compact: true,
        });
        if response.clicked() && can_focus {
            self.focus_run(key.expect("focusable queued agent").clone());
        }
        if run.latest_unseen_attention_id.is_some() {
            response.context_menu(|ui| {
                self.action(ui, snapshot, "Acknowledge alert", snapshot.acknowledge(run));
                if self.pending.is_some() {
                    ui.close();
                }
            });
        }
        response.on_hover_ui(|ui| {
            ui.weak("Queued agent");
            describe_run(ui, run, cached, None);
            ui.weak(if can_focus {
                "Click to open its terminal"
            } else {
                "Terminal focus unavailable"
            });
        });
    }

    fn layout_picker(&mut self, ui: &mut Ui) {
        ui.set_min_width(164.0);
        ui.strong("Deck layout");
        ui.weak(RichText::new("Columns × rows").size(11.0));
        let current = self.window.layout();
        let mut selected = current;
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut selected.orientation,
                Orientation::Landscape,
                "Landscape",
            );
            ui.selectable_value(&mut selected.orientation, Orientation::Portrait, "Portrait");
        });
        ui.separator();
        for preset in Preset::ALL {
            let candidate = GridLayout {
                preset,
                orientation: selected.orientation,
            };
            if layout_option(ui, candidate, selected.preset == preset).clicked() {
                selected.preset = preset;
                ui.close();
            }
        }
        if selected != current {
            self.window.set_layout(selected);
            ui.close();
        }
    }

    fn floating_header(&mut self, ui: &mut Ui, tray: bool) {
        ui.horizontal(|ui| {
            let title = ui
                .add(
                    egui::Label::new(RichText::new("Agent Deck").strong().size(13.0))
                        .sense(egui::Sense::drag()),
                )
                .on_hover_cursor(egui::CursorIcon::Grab)
                .on_hover_text("Drag to move the deck");
            if title.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let close_hint = if tray {
                    "Hide to tray"
                } else {
                    "Quit Agent Deck"
                };
                if ui.small_button("×").on_hover_text(close_hint).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                let more = ui
                    .small_button("…")
                    .on_hover_text("Queue, hosts, notifications and settings");
                more.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "More")
                });
                if more.clicked() {
                    self.window.expanded = true;
                }
                let hint = if tray {
                    "On: stays visible. Off: opens from the tray and hides when focus leaves."
                } else {
                    "Without a tray, turning this off keeps the window accessible."
                };
                let topmost = self.window.always_on_top();
                let mode = if topmost { "on" } else { "off" };
                let pin = topmost_button(ui, topmost)
                    .on_hover_text(format!("Always on top: {mode}\n{hint}"));
                if pin.clicked() {
                    self.window.toggle_topmost(ui.ctx(), tray);
                }
            });
        });
    }

    fn floating_status(&mut self, ui: &mut Ui) {
        let error = self.window.error.clone().or_else(|| {
            self.notice
                .as_ref()
                .filter(|notice| notice.failed)
                .map(|notice| notice.message.clone())
        });
        let more = self.view.snapshot.as_ref().map_or(0, |snapshot| {
            snapshot
                .data
                .overflow_count
                .saturating_sub(snapshot.data.overflow.len() as u32)
        });
        ui.horizontal(|ui| {
            let online = self.view.link == Link::Online
                && self
                    .view
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.generation().is_some());
            let status = if self.focus_pending.is_some() && self.pending.is_some() {
                "Opening…"
            } else if self.pending.is_some() {
                "Updating…"
            } else if online {
                "Connected"
            } else {
                "Offline"
            };
            let text = RichText::new(error.as_deref().unwrap_or(status))
                .size(11.0)
                .color(if error.is_some() {
                    ui.visuals().error_fg_color
                } else if online {
                    Color32::from_rgb(119, 216, 183)
                } else {
                    ui.visuals().warn_fg_color
                });
            let status_width =
                (ui.available_width() - 64.0 - if more > 0 { 48.0 } else { 0.0 }).max(20.0);
            ui.add_sized([status_width, 18.0], egui::Label::new(text).truncate())
                .on_hover_text(
                    error
                        .as_deref()
                        .unwrap_or_else(|| connection_text(&self.view)),
                );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(self.window.layout().label(), |ui| self.layout_picker(ui))
                    .response
                    .on_hover_text("Deck layout · columns × rows");
                if more > 0
                    && ui
                        .small_button(format!("+{more}"))
                        .on_hover_text("More queued agents")
                        .clicked()
                {
                    self.window.expanded = true;
                    self.section = Section::Overflow;
                }
            });
        });
    }
}

enum Tile<'a> {
    Slot(&'a PresentedSlot),
    Queued(u32, &'a PresentedRun),
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
    tiles.extend(
        overflow
            .iter()
            .enumerate()
            .map(|(index, run)| Tile::Queued(index as u32 + 1, run)),
    );
    tiles.resize_with(tiles.len().max(capacity), || Tile::Empty);
    tiles
}

fn layout_option(ui: &mut Ui, layout: GridLayout, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(164.0, 29.0), egui::Sense::click());
    let label = format!("{} · {} keys", layout.label(), layout.capacity());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &label)
    });
    if selected || response.hovered() || response.has_focus() {
        let fill = if selected {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().widgets.hovered.bg_fill
        };
        ui.painter().rect_filled(rect, 4, fill);
    }
    let color = if selected {
        Color32::from_rgb(119, 216, 183)
    } else {
        ui.visuals().text_color()
    };
    let (columns, rows) = layout.dimensions();
    let origin = egui::pos2(
        rect.left() + 21.0 - columns as f32 * 3.0,
        rect.center().y - rows as f32 * 2.5,
    );
    for row in 0..rows {
        for column in 0..columns {
            let cell = egui::Rect::from_min_size(
                origin + egui::vec2(column as f32 * 6.0, row as f32 * 5.0),
                egui::vec2(4.0, 3.0),
            );
            ui.painter().rect_filled(cell, 1, color);
        }
    }
    ui.painter().text(
        egui::pos2(rect.left() + 43.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        color,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn topmost_button(ui: &mut Ui, selected: bool) -> egui::Response {
    let response = ui.add(
        egui::Button::new("")
            .selected(selected)
            .min_size(egui::vec2(24.0, 18.0)),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, selected, "Always on top")
    });
    let color = if selected {
        Color32::from_rgb(119, 216, 183)
    } else {
        ui.visuals().text_color()
    };
    let center = response.rect.center();
    let stroke = egui::Stroke::new(1.3, color);
    ui.painter().rect_stroke(
        egui::Rect::from_center_size(center - egui::vec2(0.0, 2.0), egui::vec2(6.0, 5.0)),
        1,
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().line_segment(
        [
            center + egui::vec2(-5.0, 1.0),
            center + egui::vec2(5.0, 1.0),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [center + egui::vec2(0.0, 1.0), center + egui::vec2(0.0, 6.0)],
        stroke,
    );
    response
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
        assert!(matches!(large[9], Tile::Queued(9, run) if std::ptr::eq(run, &overflow[8])));
        assert!(large[10..].iter().all(|tile| matches!(tile, Tile::Empty)));
        let small = tiles(&slots, &overflow, 6);
        assert_eq!(
            small.len(),
            10,
            "smaller grids retain extra agents for scrolling"
        );
    }
}
