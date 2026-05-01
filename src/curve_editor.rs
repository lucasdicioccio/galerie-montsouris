use egui::{Color32, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};

const POINT_RADIUS: f32 = 5.5;
const SNAP_RADIUS_PX: f32 = 13.0;
const GRAD_STRIP_W: f32 = 10.0;

/// Persistent state for one curve editor (which point is being dragged).
#[derive(Clone, Default)]
struct EditorMemory {
    dragging: Option<usize>,
}

/// A square interactive tone-curve editor widget.
///
/// Usage:
/// ```ignore
/// let changed = ui.add(
///     CurveEditor::new("curve_r", &mut r_points, Color32::from_rgb(220, 80, 80))
///         .histogram(&hist_r)
/// ).changed();
/// ```
pub struct CurveEditor<'a> {
    id: Id,
    points: &'a mut Vec<[f32; 2]>,
    color: Color32,
    histogram: Option<&'a [u32; 256]>,
}

impl<'a> CurveEditor<'a> {
    pub fn new(id: impl Into<Id>, points: &'a mut Vec<[f32; 2]>, color: Color32) -> Self {
        Self { id: id.into(), points, color, histogram: None }
    }

    /// Show a per-channel histogram behind the curve.
    pub fn histogram(mut self, hist: &'a [u32; 256]) -> Self {
        self.histogram = Some(hist);
        self
    }
}

impl<'a> egui::Widget for CurveEditor<'a> {
    fn ui(self, ui: &mut Ui) -> Response {
        // Square widget, slightly inset to leave room for the left gradient strip.
        let avail = ui.available_width();
        let total_size = avail.min(240.0).max(80.0);
        let curve_size = total_size - GRAD_STRIP_W - 2.0; // 2px gap

        // Allocate total rect (gradient strip + gap + curve square)
        let (total_rect, mut response) =
            ui.allocate_exact_size(Vec2::new(total_size, total_size), Sense::click_and_drag());

        let curve_rect = Rect::from_min_size(
            Pos2::new(total_rect.min.x + GRAD_STRIP_W + 2.0, total_rect.min.y),
            Vec2::splat(curve_size),
        );
        let grad_rect = Rect::from_min_size(
            total_rect.min,
            Vec2::new(GRAD_STRIP_W, curve_size),
        );

        let id = self.id;
        let mut mem: EditorMemory = ui.memory(|m| m.data.get_temp(id)).unwrap_or_default();

        // Only respond to interactions inside the curve area.
        let curve_response = ui.interact(curve_rect, id.with("curve"), Sense::click_and_drag());
        if process_interaction(&curve_response, curve_rect, self.points, &mut mem) {
            response.mark_changed();
        }

        ui.memory_mut(|m| m.data.insert_temp(id, mem.clone()));

        if ui.is_rect_visible(total_rect) {
            let painter = ui.painter().with_clip_rect(total_rect.expand(1.0));
            render_grad_strip(&painter, grad_rect);
            render_curve_area(&painter, curve_rect, self.points, self.color, self.histogram, &mem);
        }

        response
    }
}

// ── Interaction ─────────────────────────────────────────────────────────────

/// Returns `true` if points were modified.
fn process_interaction(
    response: &Response,
    rect: Rect,
    points: &mut Vec<[f32; 2]>,
    mem: &mut EditorMemory,
) -> bool {
    let mut changed = false;

    // Secondary click → remove nearest point.
    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(i) = nearest_within(points, pos, rect) {
                points.remove(i);
                if let Some(d) = mem.dragging {
                    match d.cmp(&i) {
                        std::cmp::Ordering::Equal => mem.dragging = None,
                        std::cmp::Ordering::Greater => mem.dragging = Some(d - 1),
                        _ => {}
                    }
                }
                changed = true;
            }
        }
    }

    // Primary drag start → select existing or add new point.
    if response.drag_started_by(egui::PointerButton::Primary) {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some(i) = nearest_within(points, pos, rect) {
                mem.dragging = Some(i);
            } else {
                let [x, y] = screen_to_curve(pos, rect);
                points.push([x, y]);
                sort_points(points);
                // Find index of the just-added point.
                let new_i = points
                    .iter()
                    .position(|p| (p[0] - x).abs() < 1e-5 && (p[1] - y).abs() < 1e-5)
                    .unwrap_or(0);
                mem.dragging = Some(new_i);
                changed = true;
            }
        }
    }

    // While dragging: move the selected point.
    if let Some(i) = mem.dragging {
        if response.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = response.interact_pointer_pos() {
                let [x, y] = screen_to_curve(pos, rect);
                let n = points.len();
                // Endpoints keep their x fixed; interior points are bounded by neighbours.
                let new_x = if i == 0 {
                    0.0
                } else if i + 1 == n {
                    1.0
                } else {
                    let x_min = points[i - 1][0] + 0.005;
                    let x_max = points[i + 1][0] - 0.005;
                    x.clamp(x_min, x_max)
                };
                points[i] = [new_x, y.clamp(0.0, 1.0)];
                changed = true;
            }
        } else {
            mem.dragging = None;
        }
    }

    // Plain click (no drag) on empty space → add point.
    if response.clicked_by(egui::PointerButton::Primary) {
        if let Some(pos) = response.interact_pointer_pos() {
            if nearest_within(points, pos, rect).is_none() {
                let [x, y] = screen_to_curve(pos, rect);
                points.push([x, y]);
                sort_points(points);
                changed = true;
            }
        }
    }

    changed
}

// ── Coordinate helpers ───────────────────────────────────────────────────────

fn screen_to_curve(pos: Pos2, rect: Rect) -> [f32; 2] {
    let x = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
    let y = (1.0 - (pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
    [x, y]
}

fn curve_to_screen(pt: [f32; 2], rect: Rect) -> Pos2 {
    Pos2::new(
        rect.min.x + pt[0] * rect.width(),
        rect.min.y + (1.0 - pt[1]) * rect.height(),
    )
}

fn nearest_within(points: &[[f32; 2]], pos: Pos2, rect: Rect) -> Option<usize> {
    let (idx, dist) = points
        .iter()
        .enumerate()
        .map(|(i, &p)| (i, (curve_to_screen(p, rect) - pos).length()))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?;
    (dist <= SNAP_RADIUS_PX).then_some(idx)
}

fn sort_points(points: &mut Vec<[f32; 2]>) {
    points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
}

// ── Curve math ───────────────────────────────────────────────────────────────

/// Fritsch-Carlson monotone cubic tangents for a sorted point list.
fn build_tangents(pts: &[[f32; 2]]) -> Vec<f32> {
    let n = pts.len();
    if n < 2 {
        return vec![0.0; n];
    }

    // Secant slopes for each interval.
    let slopes: Vec<f32> = (0..n - 1)
        .map(|i| {
            let dx = pts[i + 1][0] - pts[i][0];
            if dx.abs() < 1e-10 { 0.0 } else { (pts[i + 1][1] - pts[i][1]) / dx }
        })
        .collect();

    // Initial tangent estimates.
    let mut m = vec![0.0f32; n];
    m[0] = slopes[0];
    m[n - 1] = slopes[n - 2];
    for i in 1..n - 1 {
        if slopes[i - 1] * slopes[i] <= 0.0 {
            m[i] = 0.0;
        } else {
            m[i] = (slopes[i - 1] + slopes[i]) / 2.0;
        }
    }

    // Monotonicity constraint (Fritsch-Carlson).
    for i in 0..n - 1 {
        let s = slopes[i];
        if s.abs() < 1e-10 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
            continue;
        }
        let alpha = m[i] / s;
        let beta = m[i + 1] / s;
        let a2b2 = alpha * alpha + beta * beta;
        if a2b2 > 9.0 {
            let tau = 3.0 / a2b2.sqrt();
            m[i] *= tau;
            m[i + 1] *= tau;
        }
    }
    m
}

/// Evaluate the monotone cubic curve at `x` using precomputed tangents.
fn eval_at(pts: &[[f32; 2]], tangents: &[f32], x: f32) -> f32 {
    let n = pts.len();
    if n == 0 {
        return x;
    }
    if n == 1 {
        return pts[0][1];
    }
    if x <= pts[0][0] {
        return pts[0][1];
    }
    if x >= pts[n - 1][0] {
        return pts[n - 1][1];
    }

    let seg = pts.partition_point(|p| p[0] <= x).saturating_sub(1).min(n - 2);
    let x0 = pts[seg][0];
    let x1 = pts[seg + 1][0];
    let y0 = pts[seg][1];
    let y1 = pts[seg + 1][1];
    let h = x1 - x0;
    if h.abs() < 1e-10 {
        return y0;
    }

    let t = (x - x0) / h;
    let t2 = t * t;
    let t3 = t2 * t;

    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;

    (h00 * y0 + h10 * h * tangents[seg] + h01 * y1 + h11 * h * tangents[seg + 1])
        .clamp(0.0, 1.0)
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// Vertical output-level gradient strip (black at bottom, white at top).
fn render_grad_strip(painter: &Painter, rect: Rect) {
    const STEPS: usize = 32;
    let step_h = rect.height() / STEPS as f32;
    for i in 0..STEPS {
        let luma = (255 * (STEPS - 1 - i) / (STEPS - 1)) as u8;
        let y = rect.min.y + i as f32 * step_h;
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(rect.min.x, y), Vec2::new(rect.width(), step_h + 0.5)),
            0.0,
            Color32::from_gray(luma),
        );
    }
}

fn render_curve_area(
    painter: &Painter,
    rect: Rect,
    points: &[[f32; 2]],
    color: Color32,
    histogram: Option<&[u32; 256]>,
    mem: &EditorMemory,
) {
    // Background
    painter.rect_filled(rect, 2.0, Color32::from_gray(22));

    // Grid (4×4 divisions)
    let grid = Color32::from_rgba_unmultiplied(70, 70, 70, 100);
    for i in 1..4 {
        let t = i as f32 / 4.0;
        let x = rect.min.x + t * rect.width();
        let y = rect.min.y + t * rect.height();
        painter.line_segment(
            [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
            Stroke::new(1.0, grid),
        );
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(1.0, grid),
        );
    }

    // Histogram bars
    if let Some(hist) = histogram {
        let max_val = hist.iter().copied().max().unwrap_or(1).max(1) as f64;
        let max_log = (max_val + 1.0).ln();
        let bar_color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 55);
        let bar_w = rect.width() / 256.0;
        for (i, &count) in hist.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let norm_h = ((count as f64 + 1.0).ln() / max_log) as f32;
            let bar_h = norm_h * rect.height();
            let x = rect.min.x + i as f32 * bar_w;
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(x, rect.max.y - bar_h),
                    Pos2::new(x + bar_w + 0.5, rect.max.y),
                ),
                0.0,
                bar_color,
            );
        }
    }

    // Dashed identity line
    let dash_color = Color32::from_rgba_unmultiplied(160, 160, 160, 120);
    let n_dashes = 20usize;
    for i in 0..n_dashes {
        let t0 = i as f32 / n_dashes as f32;
        let t1 = (i as f32 + 0.45) / n_dashes as f32;
        painter.line_segment(
            [
                Pos2::new(rect.min.x + t0 * rect.width(), rect.max.y - t0 * rect.height()),
                Pos2::new(rect.min.x + t1 * rect.width(), rect.max.y - t1 * rect.height()),
            ],
            Stroke::new(1.0, dash_color),
        );
    }

    // Smooth curve
    if !points.is_empty() {
        let mut sorted = points.to_vec();
        sort_points(&mut sorted);
        let tangents = build_tangents(&sorted);
        const N: usize = 128;
        let curve_pts: Vec<Pos2> = (0..=N)
            .map(|i| {
                let x = i as f32 / N as f32;
                let y = eval_at(&sorted, &tangents, x);
                curve_to_screen([x, y], rect)
            })
            .collect();
        painter.add(Shape::line(
            curve_pts,
            Stroke::new(2.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 230)),
        ));
    }

    // Control points
    let pointer_pos = painter
        .ctx()
        .input(|i| i.pointer.hover_pos());

    for (i, &pt) in points.iter().enumerate() {
        let pos = curve_to_screen(pt, rect);
        let is_dragging = mem.dragging == Some(i);
        let is_hovered = pointer_pos
            .map(|p| (p - pos).length() <= SNAP_RADIUS_PX)
            .unwrap_or(false);
        let r = if is_dragging || is_hovered { POINT_RADIUS + 1.5 } else { POINT_RADIUS };
        let fill = if is_dragging {
            Color32::from_gray(80)
        } else {
            Color32::from_gray(45)
        };
        painter.circle(pos, r, fill, Stroke::new(2.0, Color32::WHITE));
    }

    // Border
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, Color32::from_white_alpha(40)),
        egui::StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_curve_passthrough() {
        let pts = vec![[0.0f32, 0.0], [1.0, 1.0]];
        let t = build_tangents(&pts);
        assert!((eval_at(&pts, &t, 0.0) - 0.0).abs() < 1e-4);
        assert!((eval_at(&pts, &t, 0.5) - 0.5).abs() < 1e-3);
        assert!((eval_at(&pts, &t, 1.0) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn s_curve_monotone() {
        let pts = vec![[0.0, 0.0], [0.25, 0.1], [0.75, 0.9], [1.0, 1.0]];
        let t = build_tangents(&pts);
        let mut prev = eval_at(&pts, &t, 0.0);
        for i in 1..=20 {
            let x = i as f32 / 20.0;
            let y = eval_at(&pts, &t, x);
            assert!(y >= prev - 1e-4, "curve went backwards at x={x}: {prev} → {y}");
            prev = y;
        }
    }

    #[test]
    fn empty_curve_returns_input() {
        let pts: Vec<[f32; 2]> = vec![];
        let t = build_tangents(&pts);
        assert!((eval_at(&pts, &t, 0.5) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn sort_points_orders_by_x() {
        let mut pts = vec![[0.8, 0.5], [0.2, 0.3], [0.5, 0.7]];
        sort_points(&mut pts);
        assert_eq!(pts[0][0], 0.2);
        assert_eq!(pts[1][0], 0.5);
        assert_eq!(pts[2][0], 0.8);
    }
}
