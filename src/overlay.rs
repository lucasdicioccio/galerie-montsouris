use std::time::{Duration, Instant};

const TOAST_DURATION: Duration = Duration::from_secs(3);
const RATING_CHARS: [&str; 5] = ["★", "★★", "★★★", "★★★★", "★★★★★"];

pub struct OverlayState {
    pub show_filename: bool,
    pub show_rating: bool,
    toast: Option<Toast>,
}

struct Toast {
    message: String,
    born_at: Instant,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            show_filename: true,
            show_rating: true,
            toast: None,
        }
    }
}

impl OverlayState {
    pub fn push_toast(&mut self, message: String) {
        self.toast = Some(Toast { message, born_at: Instant::now() });
    }

    #[allow(dead_code)]
    pub fn is_toast_alive(&self) -> bool {
        self.toast
            .as_ref()
            .map(|t| t.born_at.elapsed() < TOAST_DURATION)
            .unwrap_or(false)
    }

    /// Render a filename strip at top of `rect`. Call after painting the image.
    pub fn render_filename(ui: &egui::Ui, rect: egui::Rect, name: &str) {
        let painter = ui.painter();
        let strip_h = 22.0;
        let strip_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.max.x, rect.min.y + strip_h),
        );
        painter.rect_filled(strip_rect, 0.0, egui::Color32::from_black_alpha(160));
        painter.text(
            strip_rect.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(13.0),
            egui::Color32::WHITE,
        );
    }

    /// Render a rating badge in bottom-left of `rect`. `rating` is 1–5.
    pub fn render_rating(ui: &egui::Ui, rect: egui::Rect, rating: Option<u8>) {
        let Some(r) = rating else { return };
        let r = r.clamp(1, 5) as usize;
        let stars = RATING_CHARS[r - 1];
        let painter = ui.painter();
        let badge_h = 22.0;
        let badge_w = 10.0 + r as f32 * 14.0;
        let badge_rect = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, rect.max.y - badge_h),
            egui::pos2(rect.min.x + badge_w, rect.max.y),
        );
        painter.rect_filled(badge_rect, 4.0, egui::Color32::from_rgba_premultiplied(0, 0, 0, 180));
        painter.text(
            badge_rect.center(),
            egui::Align2::CENTER_CENTER,
            stars,
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgb(255, 215, 0), // gold
        );
    }

    /// Render the active toast (script output or status message). Returns true if still alive.
    pub fn render_toast(&mut self, ctx: &egui::Context) -> bool {
        let toast = match &self.toast {
            Some(t) if t.born_at.elapsed() < TOAST_DURATION => t,
            _ => {
                self.toast = None;
                return false;
            }
        };

        // Schedule repaint when the toast should disappear
        let remaining = TOAST_DURATION - toast.born_at.elapsed();
        ctx.request_repaint_after(remaining);

        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -40.0])
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(egui::Color32::from_rgba_premultiplied(30, 30, 30, 230))
                    .corner_radius(6)
                    .inner_margin(egui::Margin { left: 12, right: 12, top: 8, bottom: 8 })
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(&toast.message)
                                .color(egui::Color32::WHITE)
                                .size(14.0),
                        );
                    });
            });

        true
    }
}

/// Compute the largest letterbox-fit rect inside `container` for an image with `ratio = w/h`.
pub fn fit_rect(container: egui::Rect, ratio: f32) -> egui::Rect {
    let cw = container.width();
    let ch = container.height();
    let (fw, fh) = if cw / ch > ratio {
        (ch * ratio, ch)
    } else {
        (cw, cw / ratio)
    };
    let cx = container.center();
    egui::Rect::from_center_size(cx, egui::vec2(fw, fh))
}
