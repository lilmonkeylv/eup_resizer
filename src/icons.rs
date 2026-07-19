use eframe::egui::{self, Color32, Painter, Pos2, Rect, Sense, Stroke, Ui, Vec2};

fn draw(
    ui: &mut Ui,
    size: f32,
    sense: Sense,
    paint: impl FnOnce(&Painter, Rect),
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), sense);
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect);
    }
    response
}

fn p(rect: Rect, x: f32, y: f32) -> Pos2 {
    rect.min + Vec2::new(x, y) * rect.size()
}

pub fn pending(ui: &mut Ui, size: f32, color: Color32) {
    draw(ui, size, Sense::hover(), |painter, rect| {
        painter.circle_stroke(rect.center(), rect.width() * 0.32, Stroke::new(1.6, color));
    });
}

pub fn check(ui: &mut Ui, size: f32, color: Color32) {
    draw(ui, size, Sense::hover(), |painter, rect| {
        let stroke = Stroke::new(2.0, color);
        painter.line_segment([p(rect, 0.18, 0.55), p(rect, 0.42, 0.78)], stroke);
        painter.line_segment([p(rect, 0.42, 0.78), p(rect, 0.84, 0.24)], stroke);
    });
}

pub fn cross(ui: &mut Ui, size: f32, color: Color32) {
    draw(ui, size, Sense::hover(), |painter, rect| {
        let stroke = Stroke::new(2.0, color);
        painter.line_segment([p(rect, 0.22, 0.22), p(rect, 0.78, 0.78)], stroke);
        painter.line_segment([p(rect, 0.78, 0.22), p(rect, 0.22, 0.78)], stroke);
    });
}

pub fn folder(ui: &mut Ui, size: f32, color: Color32) {
    draw(ui, size, Sense::hover(), |painter, rect| {
        let stroke = Stroke::new(1.4, color);
        let body = Rect::from_min_max(p(rect, 0.1, 0.32), p(rect, 0.9, 0.8));
        painter.rect_stroke(body, 1.5, stroke, egui::StrokeKind::Outside);
        let tab = Rect::from_min_max(p(rect, 0.1, 0.2), p(rect, 0.45, 0.32));
        painter.rect_stroke(tab, 1.0, stroke, egui::StrokeKind::Outside);
    });
}

pub fn plus(ui: &mut Ui, size: f32, color: Color32) {
    draw(ui, size, Sense::hover(), |painter, rect| {
        let stroke = Stroke::new(2.0, color);
        painter.line_segment([p(rect, 0.5, 0.15), p(rect, 0.5, 0.85)], stroke);
        painter.line_segment([p(rect, 0.15, 0.5), p(rect, 0.85, 0.5)], stroke);
    });
}

pub fn spinner(ui: &mut Ui, size: f32, color: Color32) {
    ui.add(egui::Spinner::new().size(size).color(color));
}

pub fn close_button(ui: &mut Ui, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter().circle_filled(
                rect.center(),
                rect.width() * 0.5,
                ui.visuals().widgets.hovered.bg_fill,
            );
        }
        let color = if response.hovered() {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        let stroke = Stroke::new(1.6, color);
        ui.painter()
            .line_segment([p(rect, 0.28, 0.28), p(rect, 0.72, 0.72)], stroke);
        ui.painter()
            .line_segment([p(rect, 0.72, 0.28), p(rect, 0.28, 0.72)], stroke);
    }
    response
}

pub fn icon_button(
    ui: &mut Ui,
    draw_icon: impl FnOnce(&mut Ui, f32, Color32),
    label: &str,
) -> egui::Response {
    let icon_size = 13.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let color = ui.visuals().text_color();
        draw_icon(ui, icon_size, color);
        ui.button(label)
    })
    .inner
}
