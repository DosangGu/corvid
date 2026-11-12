use super::*;
use egui::{Align2, Rect, Response, Sense, Stroke, StrokeKind, Vec2, Widget};

pub(super) const BACKGROUND: Color32 = Color32::from_rgb(15, 19, 25);
pub(super) const PANEL: Color32 = Color32::from_rgb(21, 27, 35);
const TEXT: Color32 = Color32::from_rgb(229, 237, 241);
const MUTED: Color32 = Color32::from_rgb(133, 148, 163);
const MINT: Color32 = Color32::from_rgb(119, 216, 183);
const BLUE: Color32 = Color32::from_rgb(120, 189, 238);
const AMBER: Color32 = Color32::from_rgb(244, 192, 101);
const RED: Color32 = Color32::from_rgb(241, 133, 144);
pub(super) const GAP: f32 = 14.0;

pub(super) fn grid(width: f32) -> (usize, f32) {
    let columns = ((width + GAP) / (164.0 + GAP)).floor().clamp(1.0, 6.0) as usize;
    let edge = ((width - GAP * (columns - 1) as f32) / columns as f32).clamp(100.0, 212.0);
    (columns, edge)
}

struct Status {
    label: &'static str,
    color: Color32,
}

fn status(run: &PresentedRun, cached: bool) -> Status {
    if let Some(label) =
        availability_label(RunAvailability::try_from(run.availability).ok(), cached)
    {
        return Status {
            label,
            color: MUTED,
        };
    }
    if run.pending_requests == 0 && run.unverified_requests > 0 {
        return Status {
            label: "Check terminal",
            color: AMBER,
        };
    }
    let (label, color) = match AgentBadge::try_from(run.last_badge) {
        Ok(AgentBadge::Working) => ("Working", BLUE),
        Ok(AgentBadge::NeedsInput) => ("Needs input", AMBER),
        Ok(AgentBadge::ResponseObserved) => ("New response", MINT),
        Ok(AgentBadge::Ready) => ("Ready", MINT),
        Ok(AgentBadge::Interrupted) => ("Interrupted", AMBER),
        Ok(AgentBadge::Error) => ("Error", RED),
        Ok(AgentBadge::Ended) => ("Ended", MUTED),
        _ => ("Unknown", MUTED),
    };
    Status { label, color }
}

pub(super) struct Key<'a> {
    pub number: u32,
    pub run: Option<&'a PresentedRun>,
    pub pinned: bool,
    pub queued: bool,
    pub edge: f32,
    pub cached: bool,
    pub can_focus: bool,
    pub compact: bool,
}

impl Widget for Key<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let sense = if self.run.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        };
        let size = if self.compact {
            egui::vec2(self.edge, crate::window::KEY_HEIGHT)
        } else {
            Vec2::splat(self.edge)
        };
        let (rect, response) = ui.allocate_exact_size(size, sense);
        let state = self.run.map(|run| status(run, self.cached));
        let label = match (self.run, &state) {
            (Some(run), Some(state)) => format!(
                "{} {}: {}, {}, {}. {}",
                if self.queued { "Queued agent" } else { "Slot" },
                self.number,
                provider(run).1,
                state.label,
                run.key.as_ref().map_or_else(
                    || "Unknown run".to_owned(),
                    |key| short_id(&key.agent_run_id)
                ),
                if self.can_focus {
                    "Open terminal"
                } else {
                    "Focus unavailable"
                },
            ),
            _ => format!("Empty key {}", self.number),
        };
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, self.can_focus, &label)
        });
        if ui.is_rect_visible(rect) {
            self.paint(ui, rect, &response, state.as_ref());
        }
        if self.can_focus {
            response.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            response
        }
    }
}

impl Key<'_> {
    fn paint(&self, ui: &Ui, rect: Rect, response: &Response, state: Option<&Status>) {
        if self.compact {
            self.paint_compact(ui, rect, response, state);
            return;
        }
        let painter = ui.painter().with_clip_rect(rect);
        let occupied = self.run.is_some();
        let pressed = self.can_focus && response.is_pointer_button_down_on();
        let face = rect
            .shrink(1.0)
            .translate(egui::vec2(0.0, if pressed { 2.0 } else { 0.0 }));
        let face = Rect::from_min_max(face.min, face.max - egui::vec2(0.0, 5.0));
        painter.rect_filled(
            face.translate(egui::vec2(0.0, 4.0)),
            18,
            Color32::from_rgb(5, 8, 12),
        );
        let fill = if occupied && response.hovered() {
            Color32::from_rgb(39, 49, 62)
        } else if occupied {
            Color32::from_rgb(29, 37, 48)
        } else {
            Color32::from_rgb(18, 24, 32)
        };
        let border = if response.has_focus() {
            Stroke::new(2.0, MINT)
        } else {
            Stroke::new(1.0, Color32::from_rgb(47, 58, 71))
        };
        painter.rect(face, 17, fill, border, StrokeKind::Inside);
        let screen = face.shrink(7.0);
        painter.rect_filled(screen, 12, BACKGROUND);
        let number = format!("{:02}", self.number);
        painter.text(
            screen.left_top() + egui::vec2(12.0, 11.0),
            Align2::LEFT_TOP,
            number,
            FontId::monospace(11.0),
            MUTED,
        );
        if self.pinned {
            painter.text(
                screen.right_top() + egui::vec2(-12.0, 11.0),
                Align2::RIGHT_TOP,
                "PIN",
                FontId::monospace(10.0),
                MINT,
            );
        }
        if let (Some(run), Some(state)) = (self.run, state) {
            let (symbol, name) = provider(run);
            let center = screen.center();
            painter.text(
                center - egui::vec2(0.0, 25.0),
                Align2::CENTER_CENTER,
                symbol,
                FontId::monospace(33.0),
                state.color,
            );
            painter.text(
                center + egui::vec2(0.0, 10.0),
                Align2::CENTER_CENTER,
                name,
                FontId::proportional(16.0),
                TEXT,
            );
            if let Some(key) = &run.key {
                painter.text(
                    center + egui::vec2(0.0, 30.0),
                    Align2::CENTER_CENTER,
                    short_id(&key.agent_run_id),
                    FontId::monospace(10.0),
                    MUTED,
                );
            }
            let status = painter.layout_no_wrap(
                state.label.to_owned(),
                FontId::proportional(12.0),
                state.color,
            );
            let origin = egui::pos2(
                center.x - (status.size().x + 12.0) / 2.0,
                screen.bottom() - 18.0,
            );
            painter.circle_filled(origin + egui::vec2(3.0, 0.0), 3.0, state.color);
            painter.galley(
                origin + egui::vec2(12.0, -status.size().y / 2.0),
                status,
                state.color,
            );
            if run.unseen_attention > 0 {
                let count = if run.unseen_attention > 9 {
                    "9+".into()
                } else {
                    run.unseen_attention.to_string()
                };
                let badge = egui::pos2(screen.right() - 15.0, screen.top() + 43.0);
                painter.circle_filled(badge, 10.0, AMBER);
                painter.text(
                    badge,
                    Align2::CENTER_CENTER,
                    count,
                    FontId::monospace(10.0),
                    BACKGROUND,
                );
            }
        } else {
            let center = screen.center();
            for (x, y) in [(-10.0, -10.0), (3.0, -10.0), (-10.0, 3.0), (3.0, 3.0)] {
                painter.rect_stroke(
                    Rect::from_min_size(center + egui::vec2(x, y - 6.0), Vec2::splat(8.0)),
                    2,
                    Stroke::new(1.0, Color32::from_rgb(54, 66, 79)),
                    StrokeKind::Inside,
                );
            }
            painter.text(
                center + egui::vec2(0.0, 30.0),
                Align2::CENTER_CENTER,
                "Awaiting agent",
                FontId::proportional(11.0),
                MUTED,
            );
        }
    }

    fn paint_compact(&self, ui: &Ui, rect: Rect, response: &Response, state: Option<&Status>) {
        let painter = ui.painter().with_clip_rect(rect);
        let pressed = self.can_focus && response.is_pointer_button_down_on();
        let face = rect
            .shrink(1.0)
            .translate(egui::vec2(0.0, if pressed { 1.0 } else { 0.0 }));
        let face = Rect::from_min_max(face.min, face.max - egui::vec2(0.0, 3.0));
        painter.rect_filled(
            face.translate(egui::vec2(0.0, 3.0)),
            9,
            Color32::from_rgb(5, 8, 12),
        );
        let fill = if self.run.is_none() {
            BACKGROUND
        } else if response.hovered() {
            Color32::from_rgb(39, 49, 62)
        } else {
            PANEL
        };
        let border = if response.has_focus() {
            MINT
        } else {
            Color32::from_rgb(47, 58, 71)
        };
        painter.rect(face, 9, fill, Stroke::new(1.0, border), StrokeKind::Inside);
        let (Some(run), Some(state)) = (self.run, state) else {
            painter.circle_filled(face.center(), 1.5, Color32::from_rgb(47, 58, 71));
            return;
        };
        let number = format!(
            "{}{:02}{}",
            if self.queued { "Q" } else { "" },
            self.number,
            if self.pinned { " P" } else { "" }
        );
        painter.text(
            face.left_top() + egui::vec2(7.0, 5.0),
            Align2::LEFT_TOP,
            number,
            FontId::monospace(9.0),
            if self.pinned { MINT } else { MUTED },
        );
        painter.text(
            egui::pos2(face.center().x, face.top() + 25.0),
            Align2::CENTER_CENTER,
            provider(run).1,
            FontId::proportional(13.0),
            TEXT,
        );
        if let Some(key) = &run.key {
            painter.text(
                egui::pos2(face.center().x, face.top() + 41.0),
                Align2::CENTER_CENTER,
                short_id(&key.agent_run_id),
                FontId::monospace(9.0),
                MUTED,
            );
        }
        let label = match state.label {
            "Needs input" => "Input needed",
            "New response" => "Response",
            "Check terminal" => "Check terminal",
            label => label,
        };
        painter.text(
            egui::pos2(face.center().x, face.bottom() - 13.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(10.0),
            state.color,
        );
        painter.rect_filled(
            Rect::from_min_size(
                egui::pos2(face.left() + 8.0, face.bottom() - 4.0),
                egui::vec2(face.width() - 16.0, 2.0),
            ),
            1,
            state.color.gamma_multiply(0.7),
        );
        if run.unseen_attention > 0 {
            let badge = face.right_top() + egui::vec2(-10.0, 10.0);
            painter.circle_filled(badge, 7.0, AMBER);
            painter.text(
                badge,
                Align2::CENTER_CENTER,
                if run.unseen_attention > 9 {
                    "9+".into()
                } else {
                    run.unseen_attention.to_string()
                },
                FontId::monospace(8.0),
                BACKGROUND,
            );
        }
    }
}

fn provider(run: &PresentedRun) -> (&'static str, &'static str) {
    match AgentProvider::try_from(run.provider) {
        Ok(AgentProvider::Codex) => (">_", "Codex"),
        Ok(AgentProvider::Claude) => ("C", "Claude"),
        _ => ("?", "Unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_keys_do_not_present_last_known_activity_as_live() {
        let run = PresentedRun {
            availability: RunAvailability::Offline as i32,
            last_badge: AgentBadge::Working as i32,
            ..Default::default()
        };
        assert_eq!(status(&run, false).label, "Offline");
        let run = PresentedRun {
            availability: RunAvailability::Available as i32,
            ..run
        };
        assert_eq!(status(&run, true).label, "Cached");
        assert_eq!(status(&run, false).label, "Working");
    }

    #[test]
    fn unverified_input_requests_direct_the_user_to_the_terminal() {
        let run = PresentedRun {
            availability: RunAvailability::Available as i32,
            last_badge: AgentBadge::Unknown as i32,
            unverified_requests: 1,
            ..Default::default()
        };
        assert_eq!(status(&run, false).label, "Check terminal");
        assert_eq!(status(&run, true).label, "Cached");
    }
}
