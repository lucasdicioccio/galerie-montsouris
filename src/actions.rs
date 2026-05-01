use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::filters::{self, Filter, RotateFill};
use crate::gallery::{BackgroundColor, PhotoCollection};
use crate::image_cache::ImageCache;
use crate::overlay::OverlayState;
use crate::viewer::{Direction, ModeTarget, ViewerState};

#[derive(Debug, Clone)]
pub enum Action {
    /// Move the selection/cursor by `count` photos in `direction`.
    Navigate { direction: Direction, count: usize },
    SwitchMode { mode: ModeTarget },
    ToggleSlideshow,
    ToggleFilterSidebar,
    Quit,
    CycleRating { values: Vec<u8> },
    RunScript {
        path: PathBuf,
        /// Argument templates. `%p`=photo path, `%d1`=width px, `%d2`=height px,
        /// `%r`=aspect ratio, `%%`=literal %. Default: `["%p"]`.
        args: Vec<String>,
        /// If true, the JSON-serialised filter stack is written to the script's stdin.
        pass_filters_stdin: bool,
    },
    /// Apply (and merge) one filter onto the focused photo's filter stack.
    ApplyFilter { filter: Filter },
    /// Change the tiling grid size. delta > 0 = zoom in (fewer tiles); delta < 0 = zoom out.
    ZoomTiling { delta: i32 },
    /// Cycle viewer background: black → gray → white → black.
    CycleBackground,
    /// Toggle per-channel RGB histogram overlay in single-photo mode.
    ToggleHistogram,
    /// In tiling mode, move the selection up or down by one full row (cols positions).
    NavigateTilingRow { direction: Direction },
    /// Adjust zoom in single-photo mode. delta > 0 = zoom in; delta < 0 = zoom out.
    /// In tiling mode, delegates to ZoomTiling.
    ZoomSingle { delta: f32 },
    /// Reset single-photo zoom to fit-to-screen and clear pan.
    ZoomSingleFit,
    /// Request zoom-to-1:1 pixels (computed on the next render frame).
    ZoomSingleToOne,
    /// Toggle full-screen (no borders) mode.
    ToggleFullscreen,
    /// Apply `filter` to every photo in the collection.
    ApplyFilterToAll { filter: Filter },
}

impl Action {
    pub fn from_binding(action: &str, args: &toml::Value) -> Result<Self> {
        let table = args.as_table();
        Ok(match action {
            "Navigate" => {
                let direction = table
                    .and_then(|t| t.get("direction"))
                    .and_then(|v| v.as_str())
                    .context("Navigate requires args.direction")?;
                let direction = match direction {
                    "next" => Direction::Next,
                    "prev" => Direction::Prev,
                    other => bail!("unknown direction {other:?}"),
                };
                let count = table
                    .and_then(|t| t.get("count"))
                    .and_then(|v| v.as_integer())
                    .unwrap_or(1)
                    .max(1) as usize;
                Action::Navigate { direction, count }
            }
            "SwitchMode" => {
                let mode = table
                    .and_then(|t| t.get("mode"))
                    .and_then(|v| v.as_str())
                    .context("SwitchMode requires args.mode")?;
                let mode = match mode {
                    "tiling" => ModeTarget::Tiling,
                    "single" => ModeTarget::Single,
                    "toggle" => ModeTarget::Toggle,
                    other => bail!("unknown mode {other:?}"),
                };
                Action::SwitchMode { mode }
            }
            "ToggleSlideshow" => Action::ToggleSlideshow,
            "ToggleFilterSidebar" => Action::ToggleFilterSidebar,
            "Quit" => Action::Quit,
            "CycleRating" => {
                let values: Vec<u8> = table
                    .and_then(|t| t.get("values"))
                    .and_then(|v| v.as_array())
                    .context("CycleRating requires args.values")?
                    .iter()
                    .map(|v| {
                        v.as_integer()
                            .context("values must be integers")
                            .map(|n| n as u8)
                    })
                    .collect::<Result<_>>()?;
                Action::CycleRating { values }
            }
            "RunScript" => {
                let path: PathBuf = table
                    .and_then(|t| t.get("path"))
                    .and_then(|v| v.as_str())
                    .context("RunScript requires args.path")?
                    .into();
                let args: Vec<String> = table
                    .and_then(|t| t.get("args"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["%p".to_owned()]);
                let pass_filters_stdin = table
                    .and_then(|t| t.get("pass_filters_stdin"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Action::RunScript { path, args, pass_filters_stdin }
            }
            "ApplyFilter" => {
                let kind = table
                    .and_then(|t| t.get("filter"))
                    .and_then(|v| v.as_str())
                    .context("ApplyFilter requires args.filter")?;
                let filter = match kind {
                    "RotateRight"     => Filter::Rotate { degrees: 90,  center: None, fill: RotateFill::Transparent },
                    "RotateLeft"      => Filter::Rotate { degrees: 270, center: None, fill: RotateFill::Transparent },
                    "Rotate180"       => Filter::Rotate { degrees: 180, center: None, fill: RotateFill::Transparent },
                    "Rotate" => {
                        let degrees = table
                            .and_then(|t| t.get("degrees"))
                            .and_then(|v| v.as_integer())
                            .context("Rotate filter requires args.degrees")? as i32;
                        Filter::Rotate { degrees, center: None, fill: RotateFill::Transparent }
                    }
                    "FlipHorizontal"  => Filter::FlipHorizontal,
                    "FlipVertical"    => Filter::FlipVertical,
                    "Exposure" => {
                        let stops = table
                            .and_then(|t| t.get("stops"))
                            .and_then(|v| v.as_float())
                            .context("Exposure filter requires args.stops")? as f32;
                        Filter::Exposure { stops }
                    }
                    "Contrast" => {
                        let factor = table
                            .and_then(|t| t.get("factor"))
                            .and_then(|v| v.as_float())
                            .context("Contrast filter requires args.factor")? as f32;
                        Filter::Contrast { factor }
                    }
                    "Scale" => {
                        let factor = table
                            .and_then(|t| t.get("factor"))
                            .and_then(|v| v.as_float())
                            .context("Scale filter requires args.factor")? as f32;
                        Filter::Scale { factor }
                    }
                    "Crop" => {
                        let x = table
                            .and_then(|t| t.get("x"))
                            .and_then(|v| v.as_float())
                            .unwrap_or(0.0) as f32;
                        let y = table
                            .and_then(|t| t.get("y"))
                            .and_then(|v| v.as_float())
                            .unwrap_or(0.0) as f32;
                        let width = table
                            .and_then(|t| t.get("width"))
                            .and_then(|v| v.as_float())
                            .context("Crop filter requires args.width")? as f32;
                        let height = table
                            .and_then(|t| t.get("height"))
                            .and_then(|v| v.as_float())
                            .context("Crop filter requires args.height")? as f32;
                        Filter::Crop { x, y, width, height }
                    }
                    "CapSize" => {
                        let max_px = table
                            .and_then(|t| t.get("max_px"))
                            .and_then(|v| v.as_integer())
                            .context("CapSize filter requires args.max_px")? as u32;
                        Filter::CapSize { max_px }
                    }
                    "Border" => {
                        let thickness = table
                            .and_then(|t| t.get("thickness"))
                            .and_then(|v| v.as_integer())
                            .context("Border filter requires args.thickness")? as u32;
                        let color_arr = table
                            .and_then(|t| t.get("color"))
                            .and_then(|v| v.as_array())
                            .context("Border filter requires args.color as [r,g,b,a]")?;
                        let color: [u8; 4] = color_arr
                            .iter()
                            .map(|v| v.as_integer().unwrap_or(0) as u8)
                            .collect::<Vec<u8>>()
                            .try_into()
                            .map_err(|_| anyhow::anyhow!("Border color must have exactly 4 components [r,g,b,a]"))?;
                        Filter::Border { thickness, color }
                    }
                    "Sharpen" => {
                        let amount = table
                            .and_then(|t| t.get("amount"))
                            .and_then(|v| v.as_float())
                            .context("Sharpen filter requires args.amount")? as f32;
                        Filter::Sharpen { amount }
                    }
                    "MicroContrast" => {
                        let amount = table
                            .and_then(|t| t.get("amount"))
                            .and_then(|v| v.as_float())
                            .context("MicroContrast filter requires args.amount")? as f32;
                        Filter::MicroContrast { amount }
                    }
                    "Curves" => Filter::Curves { r: vec![], g: vec![], b: vec![] },
                    other => bail!("unknown filter kind {other:?}"),
                };
                Action::ApplyFilter { filter }
            }
            "ZoomTiling" => {
                let delta = table
                    .and_then(|t| t.get("delta"))
                    .and_then(|v| v.as_integer())
                    .context("ZoomTiling requires args.delta")? as i32;
                Action::ZoomTiling { delta }
            }
            "CycleBackground" => Action::CycleBackground,
            "ToggleHistogram" => Action::ToggleHistogram,
            "ZoomSingle" => {
                let delta = table
                    .and_then(|t| t.get("delta"))
                    .and_then(|v| v.as_float())
                    .context("ZoomSingle requires args.delta")? as f32;
                Action::ZoomSingle { delta }
            }
            "ZoomSingleFit" => Action::ZoomSingleFit,
            "ZoomSingleToOne" => Action::ZoomSingleToOne,
            "ToggleFullscreen" => Action::ToggleFullscreen,
            "ApplyFilterToAll" => {
                let kind = table
                    .and_then(|t| t.get("filter"))
                    .and_then(|v| v.as_str())
                    .context("ApplyFilterToAll requires args.filter")?;
                let filter = match kind {
                    "RotateRight"     => Filter::Rotate { degrees: 90,  center: None, fill: RotateFill::Transparent },
                    "RotateLeft"      => Filter::Rotate { degrees: 270, center: None, fill: RotateFill::Transparent },
                    "Rotate180"       => Filter::Rotate { degrees: 180, center: None, fill: RotateFill::Transparent },
                    "FlipHorizontal"  => Filter::FlipHorizontal,
                    "FlipVertical"    => Filter::FlipVertical,
                    "Sharpen" => {
                        let amount = table
                            .and_then(|t| t.get("amount"))
                            .and_then(|v| v.as_float())
                            .unwrap_or(1.0) as f32;
                        Filter::Sharpen { amount }
                    }
                    "MicroContrast" => {
                        let amount = table
                            .and_then(|t| t.get("amount"))
                            .and_then(|v| v.as_float())
                            .unwrap_or(0.5) as f32;
                        Filter::MicroContrast { amount }
                    }
                    other => bail!("unknown filter kind for ApplyFilterToAll: {other:?}"),
                };
                Action::ApplyFilterToAll { filter }
            }
            "NavigateTilingRow" => {
                let direction = table
                    .and_then(|t| t.get("direction"))
                    .and_then(|v| v.as_str())
                    .context("NavigateTilingRow requires args.direction")?;
                let direction = match direction {
                    "next" => Direction::Next,
                    "prev" => Direction::Prev,
                    other => bail!("unknown direction {other:?}"),
                };
                Action::NavigateTilingRow { direction }
            }
            other => bail!("unknown action type {other:?}"),
        })
    }
}

pub struct ScriptRequest {
    pub script_path: PathBuf,
    /// Pre-expanded argument list ready to pass to the process.
    pub script_args: Vec<String>,
    /// If `Some`, pipe this content to the script's stdin.
    pub stdin_content: Option<String>,
    pub result_tx: crossbeam_channel::Sender<ScriptResult>,
    pub ctx: egui::Context,
}

pub struct ScriptResult {
    pub output: String,
    pub success: bool,
}

pub struct AppState {
    pub should_quit: bool,
    pub slideshow_active: bool,
    pub slideshow_last_at: std::time::Instant,
    pub script_running: bool,
    pub filter_sidebar_open: bool,
    pub background_color: BackgroundColor,
    pub show_histogram: bool,
    pub needs_scroll_to_selection: bool,
    pub is_fullscreen: bool,
    /// Signals render_single to set zoom so 1 image pixel = 1 screen pixel.
    pub zoom_to_one_pending: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            should_quit: false,
            slideshow_active: false,
            slideshow_last_at: std::time::Instant::now(),
            script_running: false,
            filter_sidebar_open: false,
            background_color: BackgroundColor::Black,
            show_histogram: false,
            needs_scroll_to_selection: false,
            is_fullscreen: false,
            zoom_to_one_pending: false,
        }
    }
}

pub struct ActionContext<'a> {
    pub collection: &'a mut PhotoCollection,
    pub viewer: &'a mut ViewerState,
    pub cache: &'a mut ImageCache,
    pub overlay: &'a mut OverlayState,
    pub app_state: &'a mut AppState,
    pub tile_count: usize,
    pub script_tx: &'a crossbeam_channel::Sender<ScriptRequest>,
    pub script_result_tx: crossbeam_channel::Sender<ScriptResult>,
    pub ctx: egui::Context,
}

pub fn execute_action(action: &Action, cx: &mut ActionContext) {
    match action {
        Action::Navigate { direction, count } => {
            cx.viewer.navigate(*direction, *count, cx.collection.len());
            cx.app_state.slideshow_last_at = std::time::Instant::now();
            if matches!(cx.viewer, ViewerState::Tiling(_)) {
                cx.app_state.needs_scroll_to_selection = true;
            }
        }
        Action::ZoomTiling { delta } => {
            if matches!(cx.viewer, ViewerState::Single(_)) {
                cx.viewer.zoom_single(*delta as f32);
            } else {
                cx.viewer.zoom_tiling(*delta, cx.collection.len());
            }
        }
        Action::SwitchMode { mode } => match mode {
            ModeTarget::Tiling => cx.viewer.switch_to_tiling(cx.tile_count),
            ModeTarget::Single => {
                let idx = cx.viewer.focused_index();
                cx.viewer.switch_to_single(idx);
            }
            ModeTarget::Toggle => cx.viewer.toggle(cx.tile_count),
        },
        Action::ToggleSlideshow => {
            cx.app_state.slideshow_active = !cx.app_state.slideshow_active;
            cx.app_state.slideshow_last_at = std::time::Instant::now();
            let msg = if cx.app_state.slideshow_active {
                "Slideshow started"
            } else {
                "Slideshow paused"
            };
            cx.overlay.push_toast(msg.to_owned());
        }
        Action::ToggleFilterSidebar => {
            cx.app_state.filter_sidebar_open = !cx.app_state.filter_sidebar_open;
        }
        Action::Quit => cx.app_state.should_quit = true,
        Action::CycleRating { values } => {
            handle_cycle_rating(values, cx);
        }
        Action::RunScript { path, args, pass_filters_stdin } => {
            handle_run_script(path, args, *pass_filters_stdin, cx);
        }
        Action::ApplyFilter { filter } => {
            handle_apply_filter(filter.clone(), cx);
        }
        Action::CycleBackground => {}
        Action::ToggleHistogram => {
            cx.app_state.show_histogram = !cx.app_state.show_histogram;
        }
        Action::ZoomSingle { delta } => {
            cx.viewer.zoom_single(*delta);
        }
        Action::ZoomSingleFit => {
            cx.viewer.reset_single_zoom();
        }
        Action::ZoomSingleToOne => {
            cx.app_state.zoom_to_one_pending = true;
        }
        Action::ToggleFullscreen => {
            cx.app_state.is_fullscreen = !cx.app_state.is_fullscreen;
            cx.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(cx.app_state.is_fullscreen));
        }
        Action::ApplyFilterToAll { filter } => {
            handle_apply_filter_to_all(filter.clone(), cx);
        }
        Action::NavigateTilingRow { direction } => {
            let total = cx.collection.len();
            if total == 0 {
                return;
            }
            let (cols, current) = match &*cx.viewer {
                ViewerState::Tiling(s) => (s.cols, s.focused_abs()),
                ViewerState::Single(_) => return,
            };
            let new_abs = match direction {
                Direction::Next => (current + cols).min(total - 1),
                Direction::Prev => current.saturating_sub(cols),
            };
            if let ViewerState::Tiling(s) = &mut *cx.viewer {
                let tc = s.tile_count();
                s.page = new_abs / tc;
                s.selected = new_abs % tc;
            }
            cx.app_state.needs_scroll_to_selection = true;
        }
    }
}

fn handle_cycle_rating(values: &[u8], cx: &mut ActionContext) {
    if cx.collection.is_empty() || values.is_empty() {
        return;
    }
    let idx = cx.viewer.focused_index();
    let current_rating = cx.collection.entries[idx].data.rating;
    let next_rating = next_cycle_value(current_rating, values);
    let mut new_data = cx.collection.entries[idx].data.clone();
    new_data.rating = next_rating;
    if let Err(e) = cx.collection.update_data(idx, new_data) {
        cx.overlay.push_toast(format!("Rating error: {e}"));
    }
}

fn handle_apply_filter_to_all(incoming: Filter, cx: &mut ActionContext) {
    if cx.collection.is_empty() {
        return;
    }
    let n = cx.collection.len();
    for idx in 0..n {
        let mut new_data = cx.collection.entries[idx].data.clone();
        filters::apply_to_stack(&mut new_data.filters, incoming.clone());
        if let Err(e) = cx.collection.update_data(idx, new_data) {
            cx.overlay.push_toast(format!("Filter error at {idx}: {e}"));
            return;
        }
        cx.cache.invalidate(idx);
    }
    cx.overlay.push_toast(format!("Applied filter to {n} photos"));
}

fn handle_apply_filter(incoming: Filter, cx: &mut ActionContext) {
    if cx.collection.is_empty() {
        return;
    }
    let idx = cx.viewer.focused_index();
    let mut new_data = cx.collection.entries[idx].data.clone();
    filters::apply_to_stack(&mut new_data.filters, incoming);

    // Describe what happened for the toast
    let desc = filter_description(&new_data.filters);

    if let Err(e) = cx.collection.update_data(idx, new_data) {
        cx.overlay.push_toast(format!("Filter error: {e}"));
        return;
    }

    // Drop the cached texture so it is re-decoded with the new filter stack.
    cx.cache.invalidate(idx);
    cx.overlay.push_toast(desc);
}

fn filter_description(filters: &[Filter]) -> String {
    if filters.is_empty() {
        return "Filters cleared".to_owned();
    }
    let mut parts = Vec::new();
    for f in filters {
        let s = match f {
            Filter::Rotate { degrees, center: None, .. } => format!("Rotated {degrees}°"),
            Filter::Rotate { degrees, center: Some([cx, cy]), .. } => {
                format!("Rotated {degrees}° around ({:.0}%,{:.0}%)", cx * 100.0, cy * 100.0)
            }
            Filter::FlipHorizontal    => "Flipped H".to_owned(),
            Filter::FlipVertical      => "Flipped V".to_owned(),
            Filter::Crop { x, y, width, height } => {
                format!("Crop {:.0}%×{:.0}% at ({:.0}%,{:.0}%)",
                    width * 100.0, height * 100.0, x * 100.0, y * 100.0)
            }
            Filter::Scale { factor }  => format!("Scale {:.0}%", factor * 100.0),
            Filter::Exposure { stops } => {
                if *stops >= 0.0 { format!("+{stops:.2} EV") } else { format!("{stops:.2} EV") }
            }
            Filter::Contrast { factor } => format!("Contrast ×{factor:.2}"),
            Filter::CapSize { max_px } => format!("Cap {max_px}px"),
            Filter::Border { thickness, color } => {
                format!("Border {thickness}px #{:02x}{:02x}{:02x}", color[0], color[1], color[2])
            }
            Filter::Sharpen { amount } => format!("Sharpen {amount:.2}"),
            Filter::MicroContrast { amount } => format!("Clarity {amount:.2}"),
            Filter::Curves { r, g, b } => {
                format!("Curves ({}/{}/{} pts)", r.len(), g.len(), b.len())
            }
        };
        parts.push(s);
    }
    parts.join(", ")
}

fn next_cycle_value(current: Option<u8>, values: &[u8]) -> Option<u8> {
    match current {
        None => Some(values[0]),
        Some(v) => {
            let pos = values.iter().position(|&x| x == v);
            match pos {
                None => Some(values[0]),
                Some(i) => {
                    if i + 1 >= values.len() {
                        None // cycle back to unrated
                    } else {
                        Some(values[i + 1])
                    }
                }
            }
        }
    }
}

fn handle_run_script(path: &PathBuf, arg_templates: &[String], pass_filters_stdin: bool, cx: &mut ActionContext) {
    if cx.app_state.script_running {
        cx.overlay.push_toast("Script already running".to_owned());
        return;
    }

    if cx.collection.is_empty() {
        cx.overlay.push_toast("No photo selected".to_owned());
        return;
    }

    let idx = cx.viewer.focused_index();
    let photo_path = cx.collection.entries[idx].path.clone();

    // Compose effective filters: gallery pre → per-photo → gallery post.
    let effective_filters: Vec<filters::Filter> = {
        let photo = &cx.collection.entries[idx].data.filters;
        match cx.collection.galerie_filters() {
            None => photo.clone(),
            Some((pre, post)) => {
                let mut v = Vec::with_capacity(pre.len() + photo.len() + post.len());
                v.extend_from_slice(&pre);
                v.extend_from_slice(photo);
                v.extend_from_slice(&post);
                v
            }
        }
    };

    let (d1, d2) = displayed_dimensions(&photo_path, &effective_filters);
    let script_args: Vec<String> = arg_templates
        .iter()
        .map(|t| expand_template(t, &photo_path, d1, d2))
        .collect();

    let stdin_content = if pass_filters_stdin {
        serde_json::to_string(&effective_filters).ok()
    } else {
        None
    };

    let req = ScriptRequest {
        script_path: expand_tilde(path),
        script_args,
        stdin_content,
        result_tx: cx.script_result_tx.clone(),
        ctx: cx.ctx.clone(),
    };

    if cx.script_tx.send(req).is_ok() {
        cx.app_state.script_running = true;
    } else {
        cx.overlay.push_toast("Failed to dispatch script".to_owned());
    }
}

/// Return the pixel dimensions of `photo_path` as they appear after EXIF auto-rotation
/// and the user's filter stack. Width is d1, height is d2.
fn displayed_dimensions(path: &std::path::Path, filters: &[crate::filters::Filter]) -> (u32, u32) {
    let (raw_w, raw_h) = image::image_dimensions(path).unwrap_or((0, 0));
    let exif_deg = crate::filters::exif_rotation_degrees(path);
    let user_deg = crate::filters::net_rotation(filters);
    let total_deg = (exif_deg + user_deg).rem_euclid(360);
    match total_deg {
        90 | 270 => (raw_h, raw_w),
        0 | 180  => (raw_w, raw_h),
        deg      => {
            let rad = (deg as f32).to_radians();
            let cos_a = rad.cos().abs();
            let sin_a = rad.sin().abs();
            let w = (raw_w as f32 * cos_a + raw_h as f32 * sin_a).round() as u32;
            let h = (raw_w as f32 * sin_a + raw_h as f32 * cos_a).round() as u32;
            (w, h)
        }
    }
}

/// Expand `%p`, `%d1`, `%d2`, `%r`, `%%` in a template string.
fn expand_template(template: &str, path: &std::path::Path, d1: u32, d2: u32) -> String {
    let ratio = if d2 > 0 { d1 as f64 / d2 as f64 } else { 0.0 };
    let mut out = String::with_capacity(template.len() + 16);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('p') => { chars.next(); out.push_str(&path.to_string_lossy()); }
            Some('d') => {
                chars.next();
                match chars.peek().copied() {
                    Some('1') => { chars.next(); out.push_str(&d1.to_string()); }
                    Some('2') => { chars.next(); out.push_str(&d2.to_string()); }
                    other => { out.push('%'); out.push('d'); if let Some(ch) = other { out.push(ch); chars.next(); } }
                }
            }
            Some('r') => { chars.next(); out.push_str(&format!("{:.4}", ratio)); }
            Some('%') => { chars.next(); out.push('%'); }
            _ => out.push('%'),
        }
    }
    out
}

fn expand_tilde(path: &PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path.clone()
}

pub fn run_script_thread(req_rx: crossbeam_channel::Receiver<ScriptRequest>) {
    for req in req_rx {
        let result = run_one_script(&req);
        let _ = req.result_tx.send(result);
        req.ctx.request_repaint();
    }
}

fn run_one_script(req: &ScriptRequest) -> ScriptResult {
    use std::io::Write;
    use std::process::Stdio;

    let mut cmd = std::process::Command::new(&req.script_path);
    cmd.args(&req.script_args);

    if req.stdin_content.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ScriptResult { output: format!("Failed to run script: {e}"), success: false },
    };

    if let Some(ref content) = req.stdin_content {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(content.as_bytes());
            // stdin closes on drop, signalling EOF to the script
        }
    }

    match child.wait_with_output() {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            let out = if stdout.is_empty() && !stderr.is_empty() {
                format!("stderr: {stderr}")
            } else if stdout.is_empty() {
                "(no output)".to_owned()
            } else {
                stdout
            };
            ScriptResult { output: out, success: o.status.success() }
        }
        Err(e) => ScriptResult {
            output: format!("Failed to run script: {e}"),
            success: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_from_binding_navigate() {
        let args = toml::Value::Table(toml::toml! { direction = "next" });
        let action = Action::from_binding("Navigate", &args).unwrap();
        assert!(matches!(action, Action::Navigate { direction: Direction::Next, count: 1 }));
    }

    #[test]
    fn action_from_binding_cycle_rating() {
        let args = toml::Value::Table(toml::toml! { values = [1, 2, 3, 4, 5] });
        let action = Action::from_binding("CycleRating", &args).unwrap();
        if let Action::CycleRating { values } = action {
            assert_eq!(values, vec![1, 2, 3, 4, 5]);
        } else {
            panic!("wrong action");
        }
    }

    #[test]
    fn action_from_binding_zoom_tiling() {
        // Positive delta (zoom in)
        let args = toml::Value::Table(toml::toml! { delta = 1 });
        let action = Action::from_binding("ZoomTiling", &args).unwrap();
        assert!(matches!(action, Action::ZoomTiling { delta: 1 }));

        // Negative delta (zoom out) — round-trips via TOML file format
        let toml_str = "delta = -1\n";
        let tbl: toml::Table = toml::from_str(toml_str).unwrap();
        let args = toml::Value::Table(tbl);
        let action = Action::from_binding("ZoomTiling", &args).unwrap();
        assert!(matches!(action, Action::ZoomTiling { delta: -1 }));
    }

    #[test]
    fn action_from_binding_unknown() {
        let args = toml::Value::Table(Default::default());
        assert!(Action::from_binding("MadeUp", &args).is_err());
    }

    #[test]
    fn cycle_rating_progression() {
        let values = &[1u8, 2, 3, 4, 5];
        assert_eq!(next_cycle_value(None, values), Some(1));
        assert_eq!(next_cycle_value(Some(1), values), Some(2));
        assert_eq!(next_cycle_value(Some(5), values), None); // clears rating
        assert_eq!(next_cycle_value(Some(99), values), Some(1)); // unknown → reset
    }

    #[test]
    fn run_script_from_binding_defaults() {
        // Only path given — args should default to ["%p"], pass_filters_stdin to false.
        let toml_str = r#"path = "~/.local/bin/export.sh""#;
        let tbl: toml::Table = toml::from_str(toml_str).unwrap();
        let args = toml::Value::Table(tbl);
        let action = Action::from_binding("RunScript", &args).unwrap();
        if let Action::RunScript { path, args, pass_filters_stdin } = action {
            assert_eq!(path, std::path::PathBuf::from("~/.local/bin/export.sh"));
            assert_eq!(args, vec!["%p".to_owned()]);
            assert!(!pass_filters_stdin);
        } else {
            panic!("wrong action");
        }
    }

    #[test]
    fn run_script_from_binding_explicit_args() {
        let toml_str = r#"
            path = "~/bin/info.sh"
            args = ["%p", "--width=%d1", "--height=%d2", "--ratio=%r"]
            pass_filters_stdin = true
        "#;
        let tbl: toml::Table = toml::from_str(toml_str).unwrap();
        let action = Action::from_binding("RunScript", &toml::Value::Table(tbl)).unwrap();
        if let Action::RunScript { args, pass_filters_stdin, .. } = action {
            assert_eq!(args, vec!["%p", "--width=%d1", "--height=%d2", "--ratio=%r"]);
            assert!(pass_filters_stdin);
        } else {
            panic!("wrong action");
        }
    }

    #[test]
    fn expand_template_substitutions() {
        let path = std::path::Path::new("/photos/img.jpg");
        assert_eq!(expand_template("%p", path, 1920, 1080), "/photos/img.jpg");
        assert_eq!(expand_template("%d1", path, 1920, 1080), "1920");
        assert_eq!(expand_template("%d2", path, 1920, 1080), "1080");
        assert_eq!(expand_template("--width=%d1 --height=%d2", path, 800, 600), "--width=800 --height=600");
        assert_eq!(expand_template("%%", path, 0, 0), "%");
    }

    #[test]
    fn expand_template_aspect_ratio() {
        let path = std::path::Path::new("/x.jpg");
        let result = expand_template("%r", path, 1920, 1080);
        let ratio: f64 = result.parse().unwrap();
        assert!((ratio - 16.0 / 9.0).abs() < 1e-3);
    }

    #[test]
    fn expand_template_zero_height() {
        // Should not panic when height is 0.
        let path = std::path::Path::new("/x.jpg");
        let result = expand_template("%r", path, 100, 0);
        assert_eq!(result, "0.0000");
    }
}
