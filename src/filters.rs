use std::path::Path;

use image::RgbaImage;
use serde::{Deserialize, Serialize};

/// What to do with pixels outside the source image after an arbitrary-angle rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RotateFill {
    /// Leave undefined pixels transparent (alpha = 0). Default; backwards-compatible.
    #[default]
    Transparent,
    /// Fill undefined pixels with a solid RGBA colour.
    Color([u8; 4]),
    /// Crop the output to the largest axis-aligned rectangle that lies fully inside the
    /// rotated image (no undefined pixels, smaller canvas).
    Crop,
}

impl RotateFill {
    fn is_transparent(&self) -> bool {
        matches!(self, RotateFill::Transparent)
    }
}

/// One entry in a photo's filter stack.
/// Tagged JSON: `{ "type": "Rotate", "degrees": 90 }`
/// Adjacent same-kind filters are merged (e.g. two consecutive 90° rotations → one 180°).
/// Non-adjacent same-kind filters are kept separate, enabling pipelines like [Crop, Rotate, Crop].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Filter {
    Rotate {
        degrees: i32,
        /// Rotation centre, normalised 0.0–1.0. `None` = image centre (lossless fast paths for ×90).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        center: Option<[f32; 2]>,
        /// What to put in undefined pixels after an arbitrary-angle rotation.
        #[serde(default, skip_serializing_if = "RotateFill::is_transparent")]
        fill: RotateFill,
    },
    FlipHorizontal,
    FlipVertical,
    Crop { x: f32, y: f32, width: f32, height: f32 }, // normalised 0.0–1.0
    Scale { factor: f32 },         // 1.0 = original size; < 1 = shrink
    Exposure { stops: f32 },       // EV stops; positive = brighter
    Contrast { factor: f32 },      // 1.0 = unchanged; > 1 = more contrast
    /// Shrink so the longest dimension is at most `max_px`; no-op if already smaller.
    CapSize { max_px: u32 },
    /// Extend the image with a solid-colour border. `color` is RGBA.
    Border { thickness: u32, color: [u8; 4] },
    /// Unsharp-mask sharpening. `amount` > 0 sharpens; typical range 0.5–3.0.
    Sharpen { amount: f32 },
    /// Clarity (local contrast / micro-contrast). `amount` > 0 enhances local contrast.
    MicroContrast { amount: f32 },
    /// Per-channel tone curves. Each channel is a list of `[input, output]` control points
    /// normalised 0.0–1.0, sorted by input. Empty list = identity (passthrough).
    Curves {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        r: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        g: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        b: Vec<[f32; 2]>,
    },
}

impl Filter {
    /// Combine `self` with `incoming` of the same kind.
    /// Returns `Some(merged)` or `None` if they cancel to identity.
    fn merge_same(&self, incoming: &Filter) -> Option<Filter> {
        match (self, incoming) {
            (Filter::Rotate { degrees: a, center: ca, .. }, Filter::Rotate { degrees: b, center: cb, fill: fb }) => {
                if ca == cb {
                    // Same centre: add angles, cancel at identity.
                    let total = (a + b).rem_euclid(360);
                    if total == 0 { None } else { Some(Filter::Rotate { degrees: total, center: *cb, fill: fb.clone() }) }
                } else {
                    // Different centres can't be composed into one rotation; last wins.
                    if (*b).rem_euclid(360) == 0 { None } else { Some(Filter::Rotate { degrees: *b, center: *cb, fill: fb.clone() }) }
                }
            }
            // Double flip = identity
            (Filter::FlipHorizontal, Filter::FlipHorizontal) => None,
            (Filter::FlipVertical, Filter::FlipVertical) => None,
            // Crop and Scale: last value wins (user sets final framing, not accumulates)
            (Filter::Crop { .. }, Filter::Crop { x, y, width, height }) => {
                let is_identity = *x < 1e-4 && *y < 1e-4
                    && (*width - 1.0).abs() < 1e-4
                    && (*height - 1.0).abs() < 1e-4;
                if is_identity { None } else { Some(Filter::Crop { x: *x, y: *y, width: *width, height: *height }) }
            }
            (Filter::Scale { .. }, Filter::Scale { factor }) => {
                if (*factor - 1.0).abs() < 1e-4 { None } else { Some(Filter::Scale { factor: *factor }) }
            }
            // Exposure accumulates (adding EV stops)
            (Filter::Exposure { stops: a }, Filter::Exposure { stops: b }) => {
                let total = a + b;
                if total.abs() < 1e-4 { None } else { Some(Filter::Exposure { stops: total }) }
            }
            // Contrast accumulates (multiplying factors)
            (Filter::Contrast { factor: a }, Filter::Contrast { factor: b }) => {
                let total = a * b;
                if (total - 1.0).abs() < 1e-4 { None } else { Some(Filter::Contrast { factor: total }) }
            }
            // CapSize: keep the more restrictive (smaller) cap
            (Filter::CapSize { max_px: a }, Filter::CapSize { max_px: b }) => {
                Some(Filter::CapSize { max_px: (*a).min(*b) })
            }
            // Border: last wins (adjacent border replaces previous)
            (Filter::Border { .. }, Filter::Border { thickness, color }) => {
                if *thickness == 0 { None } else { Some(Filter::Border { thickness: *thickness, color: *color }) }
            }
            // Sharpen accumulates
            (Filter::Sharpen { amount: a }, Filter::Sharpen { amount: b }) => {
                let total = a + b;
                if total.abs() < 1e-4 { None } else { Some(Filter::Sharpen { amount: total }) }
            }
            // MicroContrast accumulates
            (Filter::MicroContrast { amount: a }, Filter::MicroContrast { amount: b }) => {
                let total = a + b;
                if total.abs() < 1e-4 { None } else { Some(Filter::MicroContrast { amount: total }) }
            }
            // Curves: last wins (full replacement)
            (Filter::Curves { .. }, Filter::Curves { r, g, b }) => {
                if r.is_empty() && g.is_empty() && b.is_empty() {
                    None
                } else {
                    Some(Filter::Curves { r: r.clone(), g: g.clone(), b: b.clone() })
                }
            }
            _ => unreachable!("merge_same called with different discriminants"),
        }
    }

    fn same_kind(&self, other: &Filter) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// Apply this filter to `img`, consuming it and returning the result.
    pub fn apply(&self, img: RgbaImage) -> RgbaImage {
        match self {
            Filter::Rotate { degrees, center: None, fill } => rotate_rgba(img, *degrees, fill),
            Filter::Rotate { degrees, center: Some([cx, cy]), fill } => {
                let (w, h) = img.dimensions();
                rotate_with_center_px(img, *degrees, cx * w as f32, cy * h as f32, fill)
            }
            Filter::FlipHorizontal => image::imageops::flip_horizontal(&img),
            Filter::FlipVertical => image::imageops::flip_vertical(&img),
            Filter::Crop { x, y, width, height } => apply_crop(img, *x, *y, *width, *height),
            Filter::Scale { factor } => apply_scale(img, *factor),
            Filter::Exposure { stops } => apply_exposure(img, *stops),
            Filter::Contrast { factor } => apply_contrast(img, *factor),
            Filter::CapSize { max_px } => apply_cap_size(img, *max_px),
            Filter::Border { thickness, color } => apply_border(img, *thickness, *color),
            Filter::Sharpen { amount } => apply_sharpen(img, *amount),
            Filter::MicroContrast { amount } => apply_micro_contrast(img, *amount),
            Filter::Curves { r, g, b } => apply_curves(img, r, g, b),
        }
    }
}

/// Apply `incoming` to `stack`, merging with the last element only if it's the same kind.
/// Non-adjacent same-kind filters are kept separate (e.g. [Crop, Rotate, Crop] is valid).
/// Adjacent merging still eliminates identity sequences (e.g. four 90° rotations cancel).
pub fn apply_to_stack(stack: &mut Vec<Filter>, incoming: Filter) {
    if let Some(last) = stack.last() {
        if last.same_kind(&incoming) {
            match last.merge_same(&incoming) {
                Some(merged) => { *stack.last_mut().unwrap() = merged; }
                None => { stack.pop(); }
            }
            return;
        }
    }
    stack.push(incoming);
}

/// Apply every filter in `filters` to `img` in order.
pub fn apply_all_filters(mut img: RgbaImage, filters: &[Filter]) -> RgbaImage {
    for filter in filters {
        img = filter.apply(img);
    }
    img
}

/// Read the EXIF orientation tag and return the CW degrees needed to display the image upright.
/// Returns 0 on any error (no EXIF, not a JPEG, no orientation tag).
pub fn exif_rotation_degrees(path: &Path) -> i32 {
    let Ok(file) = std::fs::File::open(path) else { return 0 };
    let mut reader = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else { return 0 };
    let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) else { return 0 };
    let Some(v) = field.value.get_uint(0) else { return 0 };
    // EXIF orientation → CW degrees needed to correct it
    match v {
        1 => 0,
        3 => 180,
        6 => 90,  // shot in portrait, phone rotated CCW → correct with 90° CW
        8 => 270, // shot in portrait, phone rotated CW → correct with 270° CW
        _ => 0,   // flip variants (2,4,5,7) are rare; ignore
    }
}

/// Apply a CW rotation in degrees to an RGBA image.
/// Multiples of 90 use lossless pixel-exact paths; other angles use bilinear interpolation.
/// `fill` controls what happens to undefined pixels for non-90° rotations.
pub fn rotate_rgba(img: RgbaImage, degrees: i32, fill: &RotateFill) -> RgbaImage {
    match degrees.rem_euclid(360) {
        0   => img,
        90  => image::imageops::rotate90(&img),
        180 => image::imageops::rotate180(&img),
        270 => image::imageops::rotate270(&img),
        deg => rotate_arbitrary(img, deg, fill),
    }
}

/// Bilinear rotation around the image centre for angles that are not multiples of 90°.
fn rotate_arbitrary(img: RgbaImage, degrees: i32, fill: &RotateFill) -> RgbaImage {
    let (w, h) = img.dimensions();
    rotate_with_center_px(img, degrees, w as f32 / 2.0, h as f32 / 2.0, fill)
}

/// Bilinear rotation around an explicit pixel-coordinate centre `(cx_src, cy_src)`.
/// The output canvas is the tight axis-aligned bounding box of the rotated source corners.
/// `fill` controls undefined pixels: transparent, solid colour, or crop to inscribed rect.
pub fn rotate_with_center_px(img: RgbaImage, degrees: i32, cx_src: f32, cy_src: f32, fill: &RotateFill) -> RgbaImage {
    let rad = (degrees as f32).to_radians();
    let cos_a = rad.cos();
    let sin_a = rad.sin();

    let (src_w, src_h) = img.dimensions();

    // Forward-map the four image corners to find the destination bounding box.
    let corners = [
        (0.0f32, 0.0f32),
        (src_w as f32, 0.0),
        (0.0, src_h as f32),
        (src_w as f32, src_h as f32),
    ];
    let fwd = |x: f32, y: f32| -> (f32, f32) {
        let dx = x - cx_src;
        let dy = y - cy_src;
        (dx * cos_a + dy * sin_a, -dx * sin_a + dy * cos_a)
    };

    let mapped: Vec<(f32, f32)> = corners.iter().map(|&(x, y)| fwd(x, y)).collect();
    let min_x = mapped.iter().map(|&(x, _)| x).fold(f32::INFINITY, f32::min);
    let max_x = mapped.iter().map(|&(x, _)| x).fold(f32::NEG_INFINITY, f32::max);
    let min_y = mapped.iter().map(|&(_, y)| y).fold(f32::INFINITY, f32::min);
    let max_y = mapped.iter().map(|&(_, y)| y).fold(f32::NEG_INFINITY, f32::max);

    let dst_w = ((max_x - min_x).ceil() as u32).max(1);
    let dst_h = ((max_y - min_y).ceil() as u32).max(1);
    // Destination offset: the rotated source centre lands at (-min_x, -min_y) in dst.
    let cx_dst = -min_x;
    let cy_dst = -min_y;

    let bg = match fill {
        RotateFill::Color(c) => image::Rgba(*c),
        _ => image::Rgba([0, 0, 0, 0]),
    };
    let mut out = RgbaImage::from_pixel(dst_w, dst_h, bg);

    for y in 0..dst_h {
        for x in 0..dst_w {
            let rx = x as f32 - cx_dst;
            let ry = y as f32 - cy_dst;
            // Backward mapping: inverse (CCW) rotation back to source.
            let sx = rx * cos_a - ry * sin_a + cx_src;
            let sy = rx * sin_a + ry * cos_a + cy_src;

            if sx >= 0.0 && sy >= 0.0 && sx < src_w as f32 && sy < src_h as f32 {
                let x0 = sx.floor() as u32;
                let y0 = sy.floor() as u32;
                let x1 = (x0 + 1).min(src_w - 1);
                let y1 = (y0 + 1).min(src_h - 1);
                let fx = sx - x0 as f32;
                let fy = sy - y0 as f32;

                let p00 = img.get_pixel(x0, y0);
                let p10 = img.get_pixel(x1, y0);
                let p01 = img.get_pixel(x0, y1);
                let p11 = img.get_pixel(x1, y1);

                let bilerp = |a: u8, b: u8, c: u8, d: u8| -> u8 {
                    let top = a as f32 * (1.0 - fx) + b as f32 * fx;
                    let bot = c as f32 * (1.0 - fx) + d as f32 * fx;
                    (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8
                };

                out.put_pixel(x, y, image::Rgba([
                    bilerp(p00[0], p10[0], p01[0], p11[0]),
                    bilerp(p00[1], p10[1], p01[1], p11[1]),
                    bilerp(p00[2], p10[2], p01[2], p11[2]),
                    bilerp(p00[3], p10[3], p01[3], p11[3]),
                ]));
            }
        }
    }

    if let RotateFill::Crop = fill {
        let (cw, ch) = largest_inscribed_rect(src_w as f32, src_h as f32, degrees);
        let cw = (cw.round() as u32).min(dst_w);
        let ch = (ch.round() as u32).min(dst_h);
        let cx = (dst_w / 2).saturating_sub(cw / 2);
        let cy = (dst_h / 2).saturating_sub(ch / 2);
        out = image::imageops::crop_imm(&out, cx, cy, cw.max(1), ch.max(1)).to_image();
    }

    out
}

/// Compute the dimensions of the largest axis-aligned rectangle that fits entirely inside a
/// W×H image after CW rotation by `degrees`. Uses the standard largest-inscribed-rect formula.
/// Reference: <https://stackoverflow.com/a/16778797>
fn largest_inscribed_rect(w: f32, h: f32, degrees: i32) -> (f32, f32) {
    let rad = (degrees as f32).to_radians();
    // Reduce to 0..π/2 — the inscribed rect size is symmetric.
    let angle = rad.rem_euclid(std::f32::consts::PI);
    let angle = if angle > std::f32::consts::FRAC_PI_2 { std::f32::consts::PI - angle } else { angle };

    if angle < 1e-6 {
        return (w, h);
    }

    let sin_a = angle.sin();
    let cos_a = angle.cos();
    let width_is_longer = w >= h;
    let (side_long, side_short) = if width_is_longer { (w, h) } else { (h, w) };

    let (wr, hr) = if side_short <= 2.0 * sin_a * cos_a * side_long {
        let x = 0.5 * side_short;
        if width_is_longer { (x / sin_a, x / cos_a) } else { (x / cos_a, x / sin_a) }
    } else {
        let cos_2a = cos_a * cos_a - sin_a * sin_a;
        if cos_2a.abs() < 1e-6 {
            // 45° edge case: inscribed square
            let s = side_short / std::f32::consts::SQRT_2;
            (s, s)
        } else {
            ((w * cos_a - h * sin_a) / cos_2a, (h * cos_a - w * sin_a) / cos_2a)
        }
    };

    (wr.max(1.0), hr.max(1.0))
}

/// Compute the net CW rotation degrees from a filter stack (sum of all Rotate entries).
pub fn net_rotation(filters: &[Filter]) -> i32 {
    filters.iter().fold(0i32, |acc, f| match f {
        Filter::Rotate { degrees, .. } => (acc + degrees).rem_euclid(360),
        _ => acc,
    })
}

// ---------- pixel operations ----------

fn apply_crop(img: RgbaImage, x: f32, y: f32, width: f32, height: f32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let cx = (x * w as f32).round() as u32;
    let cy = (y * h as f32).round() as u32;
    let cw = (width * w as f32).round() as u32;
    let ch = (height * h as f32).round() as u32;
    // Clamp to image bounds
    let cx = cx.min(w.saturating_sub(1));
    let cy = cy.min(h.saturating_sub(1));
    let cw = cw.min(w - cx).max(1);
    let ch = ch.min(h - cy).max(1);
    image::imageops::crop_imm(&img, cx, cy, cw, ch).to_image()
}

fn apply_scale(img: RgbaImage, factor: f32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let nw = ((w as f32 * factor).round() as u32).max(1);
    let nh = ((h as f32 * factor).round() as u32).max(1);
    image::DynamicImage::ImageRgba8(img)
        .resize(nw, nh, image::imageops::FilterType::Triangle)
        .into_rgba8()
}

fn apply_exposure(mut img: RgbaImage, stops: f32) -> RgbaImage {
    let scale = 2f32.powf(stops);
    let lut: [u8; 256] = std::array::from_fn(|i| {
        ((i as f32 * scale).round().clamp(0.0, 255.0)) as u8
    });
    for pixel in img.pixels_mut() {
        pixel[0] = lut[pixel[0] as usize];
        pixel[1] = lut[pixel[1] as usize];
        pixel[2] = lut[pixel[2] as usize];
        // alpha unchanged
    }
    img
}

fn apply_contrast(mut img: RgbaImage, factor: f32) -> RgbaImage {
    // Scale each channel around the midpoint 128.
    let lut: [u8; 256] = std::array::from_fn(|i| {
        let v = (i as f32 - 128.0) * factor + 128.0;
        v.round().clamp(0.0, 255.0) as u8
    });
    for pixel in img.pixels_mut() {
        pixel[0] = lut[pixel[0] as usize];
        pixel[1] = lut[pixel[1] as usize];
        pixel[2] = lut[pixel[2] as usize];
    }
    img
}

fn apply_cap_size(img: RgbaImage, max_px: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest <= max_px {
        return img;
    }
    let scale = max_px as f32 / longest as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::DynamicImage::ImageRgba8(img)
        .resize(nw, nh, image::imageops::FilterType::Triangle)
        .into_rgba8()
}

fn apply_sharpen(img: RgbaImage, amount: f32) -> RgbaImage {
    if amount.abs() < 1e-4 { return img; }
    let blurred = image::DynamicImage::ImageRgba8(img.clone())
        .blur(1.5)
        .into_rgba8();
    let mut out = img;
    for (pix, blur_pix) in out.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let v = pix[c] as f32 + amount * (pix[c] as f32 - blur_pix[c] as f32);
            pix[c] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn apply_micro_contrast(img: RgbaImage, amount: f32) -> RgbaImage {
    if amount.abs() < 1e-4 { return img; }
    // Large-radius blur captures the regional mean; boost local deviations around it.
    let blurred = image::DynamicImage::ImageRgba8(img.clone())
        .blur(10.0)
        .into_rgba8();
    let mut out = img;
    for (pix, blur_pix) in out.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let mean = blur_pix[c] as f32;
            let v = mean + (1.0 + amount) * (pix[c] as f32 - mean);
            pix[c] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Build a 256-entry lookup table via piecewise-linear interpolation through `points`.
/// Points are `[input, output]` pairs normalised 0.0–1.0. Empty = identity.
fn build_curve_lut(points: &[[f32; 2]]) -> [u8; 256] {
    if points.is_empty() {
        return std::array::from_fn(|i| i as u8);
    }
    let mut pts: Vec<[f32; 2]> = points.to_vec();
    pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
    std::array::from_fn(|i| {
        let x = i as f32 / 255.0;
        let y = if x <= pts[0][0] {
            pts[0][1]
        } else if x >= pts[pts.len() - 1][0] {
            pts[pts.len() - 1][1]
        } else {
            let pos = pts.partition_point(|p| p[0] <= x);
            let lo = &pts[pos - 1];
            let hi = &pts[pos];
            let span = hi[0] - lo[0];
            if span < 1e-6 { lo[1] } else {
                let t = (x - lo[0]) / span;
                lo[1] * (1.0 - t) + hi[1] * t
            }
        };
        (y * 255.0).round().clamp(0.0, 255.0) as u8
    })
}

fn apply_curves(mut img: RgbaImage, r: &[[f32; 2]], g: &[[f32; 2]], b: &[[f32; 2]]) -> RgbaImage {
    let lut_r = build_curve_lut(r);
    let lut_g = build_curve_lut(g);
    let lut_b = build_curve_lut(b);
    for pixel in img.pixels_mut() {
        pixel[0] = lut_r[pixel[0] as usize];
        pixel[1] = lut_g[pixel[1] as usize];
        pixel[2] = lut_b[pixel[2] as usize];
    }
    img
}

fn apply_border(img: RgbaImage, thickness: u32, color: [u8; 4]) -> RgbaImage {
    if thickness == 0 {
        return img;
    }
    let (w, h) = img.dimensions();
    let new_w = w + 2 * thickness;
    let new_h = h + 2 * thickness;
    let mut out = RgbaImage::from_pixel(new_w, new_h, image::Rgba(color));
    image::imageops::overlay(&mut out, &img, thickness as i64, thickness as i64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(r: u8, g: u8, b: u8, a: u8, w: u32, h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() { *p = image::Rgba([r, g, b, a]); }
        img
    }

    #[test]
    fn apply_to_stack_accumulates() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent });
        apply_to_stack(&mut stack, Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent });
        assert_eq!(stack, vec![Filter::Rotate { degrees: 180, center: None, fill: RotateFill::Transparent }]);
    }

    #[test]
    fn apply_to_stack_cancels() {
        let mut stack = vec![Filter::Rotate { degrees: 270, center: None, fill: RotateFill::Transparent }];
        apply_to_stack(&mut stack, Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent });
        assert!(stack.is_empty());
    }

    #[test]
    fn apply_to_stack_full_cycle() {
        let mut stack = vec![];
        for _ in 0..4 {
            apply_to_stack(&mut stack, Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent });
        }
        assert!(stack.is_empty(), "4 × 90° = 360° should cancel");
    }

    #[test]
    fn apply_to_stack_non_adjacent_same_kind_kept_separate() {
        // [Crop, Rotate, Crop] — second Crop is not adjacent to first, so both survive.
        let crop1 = Filter::Crop { x: 0.0, y: 0.0, width: 0.8, height: 1.0 };
        let crop2 = Filter::Crop { x: 0.0, y: 0.0, width: 0.5, height: 0.5 };
        let mut stack = vec![];
        apply_to_stack(&mut stack, crop1.clone());
        apply_to_stack(&mut stack, Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent });
        apply_to_stack(&mut stack, crop2.clone());
        assert_eq!(stack.len(), 3);
        assert_eq!(stack[0], crop1);
        assert_eq!(stack[1], Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent });
        assert_eq!(stack[2], crop2);
    }

    #[test]
    fn net_rotation_sums() {
        let filters = vec![Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent }];
        assert_eq!(net_rotation(&filters), 90);
    }

    #[test]
    fn net_rotation_ignores_other_filters() {
        let filters = vec![Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent }, Filter::FlipHorizontal];
        assert_eq!(net_rotation(&filters), 90);
    }

    #[test]
    fn rotate_rgba_dimensions_swap() {
        let img = RgbaImage::new(100, 50);
        let rotated = rotate_rgba(img, 90, &RotateFill::Transparent);
        assert_eq!(rotated.dimensions(), (50, 100));
    }

    #[test]
    fn rotate_rgba_arbitrary_expands_canvas() {
        // 45° rotation of a square NxN image → canvas ≈ N*√2 × N*√2
        let n = 100u32;
        let img = solid_rgba(255, 0, 0, 255, n, n);
        let rotated = rotate_rgba(img, 45, &RotateFill::Transparent);
        let (w, h) = rotated.dimensions();
        let expected = (n as f32 * std::f32::consts::SQRT_2).round() as u32;
        assert!((w as i32 - expected as i32).abs() <= 2, "w={w} expected≈{expected}");
        assert!((h as i32 - expected as i32).abs() <= 2, "h={h} expected≈{expected}");
    }

    #[test]
    fn rotate_rgba_arbitrary_zero_is_identity() {
        // 0° falls through to the identity fast path, not rotate_arbitrary
        let img = solid_rgba(128, 64, 32, 255, 60, 40);
        let rotated = rotate_rgba(img.clone(), 0, &RotateFill::Transparent);
        assert_eq!(rotated.dimensions(), img.dimensions());
        assert_eq!(rotated.get_pixel(0, 0), img.get_pixel(0, 0));
    }

    #[test]
    fn flip_horizontal_is_self_inverse() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::FlipHorizontal);
        apply_to_stack(&mut stack, Filter::FlipHorizontal);
        assert!(stack.is_empty());
    }

    #[test]
    fn flip_vertical_is_self_inverse() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::FlipVertical);
        apply_to_stack(&mut stack, Filter::FlipVertical);
        assert!(stack.is_empty());
    }

    #[test]
    fn flip_horizontal_mirrors_pixels() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 0, 255, 255]));
        let flipped = Filter::FlipHorizontal.apply(img);
        assert_eq!(flipped.get_pixel(0, 0), &image::Rgba([0, 0, 255, 255]));
        assert_eq!(flipped.get_pixel(1, 0), &image::Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn exposure_brightens_pixels() {
        let img = solid_rgba(100, 100, 100, 255, 2, 2);
        let brighter = Filter::Exposure { stops: 1.0 }.apply(img); // +1 stop = ×2
        let p = brighter.get_pixel(0, 0);
        assert_eq!(p[0], 200);
        assert_eq!(p[3], 255); // alpha unchanged
    }

    #[test]
    fn exposure_clamps_at_255() {
        let img = solid_rgba(200, 200, 200, 255, 1, 1);
        let blown = Filter::Exposure { stops: 2.0 }.apply(img); // ×4
        assert_eq!(blown.get_pixel(0, 0)[0], 255);
    }

    #[test]
    fn exposure_accumulates_in_stack() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::Exposure { stops: 0.5 });
        apply_to_stack(&mut stack, Filter::Exposure { stops: 0.5 });
        assert_eq!(stack, vec![Filter::Exposure { stops: 1.0 }]);
    }

    #[test]
    fn exposure_cancels_in_stack() {
        let mut stack = vec![Filter::Exposure { stops: 1.0 }];
        apply_to_stack(&mut stack, Filter::Exposure { stops: -1.0 });
        assert!(stack.is_empty());
    }

    #[test]
    fn contrast_increases_difference() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgba([100, 0, 0, 255])); // below midpoint
        img.put_pixel(1, 0, image::Rgba([200, 0, 0, 255])); // above midpoint
        let high = Filter::Contrast { factor: 2.0 }.apply(img);
        // 100 → (100-128)*2+128 = -56+128 = 72  → darker
        // 200 → (200-128)*2+128 = 144+128 = 272 → clamped to 255
        assert!(high.get_pixel(0, 0)[0] < 100);
        assert_eq!(high.get_pixel(1, 0)[0], 255);
    }

    #[test]
    fn contrast_accumulates_in_stack() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::Contrast { factor: 1.2 });
        apply_to_stack(&mut stack, Filter::Contrast { factor: 1.2 });
        if let Some(Filter::Contrast { factor }) = stack.first() {
            assert!((*factor - 1.44).abs() < 1e-3);
        } else {
            panic!("expected Contrast in stack");
        }
    }

    #[test]
    fn crop_reduces_dimensions() {
        let img = solid_rgba(0, 0, 0, 255, 100, 80);
        let cropped = Filter::Crop { x: 0.1, y: 0.1, width: 0.5, height: 0.5 }.apply(img);
        assert_eq!(cropped.dimensions(), (50, 40));
    }

    #[test]
    fn crop_identity_removes_from_stack() {
        let mut stack = vec![Filter::Crop { x: 0.1, y: 0.0, width: 0.9, height: 1.0 }];
        apply_to_stack(&mut stack, Filter::Crop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
        assert!(stack.is_empty(), "full-image crop should be treated as identity");
    }

    #[test]
    fn scale_reduces_dimensions() {
        let img = solid_rgba(0, 0, 0, 255, 100, 80);
        let scaled = Filter::Scale { factor: 0.5 }.apply(img);
        assert_eq!(scaled.dimensions(), (50, 40));
    }

    #[test]
    fn scale_identity_removes_from_stack() {
        let mut stack = vec![Filter::Scale { factor: 0.5 }];
        apply_to_stack(&mut stack, Filter::Scale { factor: 1.0 });
        assert!(stack.is_empty(), "Scale(1.0) is identity and should be removed");
    }

    #[test]
    fn apply_all_filters_chains_correctly() {
        let img = RgbaImage::new(100, 50);
        let filters = vec![
            Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent }, // 50×100
            Filter::Scale { factor: 0.5 },  // 25×50
        ];
        let result = apply_all_filters(img, &filters);
        assert_eq!(result.dimensions(), (25, 50));
    }

    #[test]
    fn cap_size_shrinks_landscape() {
        let img = solid_rgba(0, 0, 0, 255, 4000, 3000);
        let capped = Filter::CapSize { max_px: 1000 }.apply(img);
        let (w, h) = capped.dimensions();
        assert_eq!(w, 1000);
        assert_eq!(h, 750); // 3000 * (1000/4000)
    }

    #[test]
    fn cap_size_shrinks_portrait() {
        let img = solid_rgba(0, 0, 0, 255, 600, 1200);
        let capped = Filter::CapSize { max_px: 300 }.apply(img);
        let (w, h) = capped.dimensions();
        assert_eq!(w, 150); // 600 * (300/1200)
        assert_eq!(h, 300);
    }

    #[test]
    fn cap_size_no_op_when_already_small() {
        let img = solid_rgba(0, 0, 0, 255, 200, 100);
        let capped = Filter::CapSize { max_px: 1024 }.apply(img);
        assert_eq!(capped.dimensions(), (200, 100));
    }

    #[test]
    fn cap_size_adjacent_takes_min() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::CapSize { max_px: 2048 });
        apply_to_stack(&mut stack, Filter::CapSize { max_px: 1024 });
        assert_eq!(stack, vec![Filter::CapSize { max_px: 1024 }]);

        let mut stack2 = vec![];
        apply_to_stack(&mut stack2, Filter::CapSize { max_px: 256 });
        apply_to_stack(&mut stack2, Filter::CapSize { max_px: 2048 });
        assert_eq!(stack2, vec![Filter::CapSize { max_px: 256 }]);
    }

    #[test]
    fn border_expands_dimensions() {
        let img = solid_rgba(0, 0, 0, 255, 100, 80);
        let bordered = Filter::Border { thickness: 10, color: [255, 255, 255, 255] }.apply(img);
        assert_eq!(bordered.dimensions(), (120, 100));
    }

    #[test]
    fn border_fills_color() {
        let img = solid_rgba(0, 0, 0, 255, 10, 10);
        let bordered = Filter::Border { thickness: 5, color: [255, 0, 0, 255] }.apply(img);
        // Corner pixel should be the border color
        assert_eq!(bordered.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        // Center pixel should be the original black
        assert_eq!(bordered.get_pixel(10, 10), &image::Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn border_zero_thickness_no_op() {
        let img = solid_rgba(128, 128, 128, 255, 50, 50);
        let out = Filter::Border { thickness: 0, color: [0, 0, 0, 255] }.apply(img);
        assert_eq!(out.dimensions(), (50, 50));
    }

    #[test]
    fn border_adjacent_last_wins() {
        let mut stack = vec![];
        apply_to_stack(&mut stack, Filter::Border { thickness: 10, color: [255, 255, 255, 255] });
        apply_to_stack(&mut stack, Filter::Border { thickness: 5, color: [0, 0, 0, 255] });
        assert_eq!(stack, vec![Filter::Border { thickness: 5, color: [0, 0, 0, 255] }]);
    }
}
