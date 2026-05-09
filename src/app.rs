use std::collections::HashSet;
use std::time::Duration;

use crate::actions::{
    execute_action, run_script_thread, ActionContext, AppState, ScriptResult,
};
use crate::config::{Config, KeyBindingMap, ModifierSet};
use crate::curve_editor::CurveEditor;
use crate::filters::{Filter, RotateFill};
use crate::gallery::{BackgroundColor, EditGallery, FilterPreset, PhotoCollection};
use crate::image_cache::{ImageCache, ImageHistogram, LoadState};
use crate::overlay::{fit_rect, OverlayState};
use crate::viewer::ViewerState;

pub struct GalerieApp {
    config: Config,
    keybindings: KeyBindingMap,
    collection: PhotoCollection,
    viewer: ViewerState,
    cache: ImageCache,
    overlay: OverlayState,
    app_state: AppState,
    script_tx: crossbeam_channel::Sender<crate::actions::ScriptRequest>,
    script_result_tx: crossbeam_channel::Sender<ScriptResult>,
    script_result_rx: crossbeam_channel::Receiver<ScriptResult>,
    // Gallery editing
    edit_galleries: Vec<EditGallery>,
    gallery_selector_open: bool,
    gallery_selector_cursor: usize,
    // Tiling cell size (logical px) from the last rendered frame, used to size thumbnails.
    last_tiling_cell_size: f32,
    /// Saved tiling `cols` so switching back from single mode restores the previous grid size.
    last_tiling_cols: usize,
    /// Current annotation search query.
    search_query: String,
    /// Maps display index → collection index; rebuilt whenever `search_query` changes.
    filtered_indices: Vec<usize>,
    /// Pending text for the annotation add-input box.
    annotation_input: String,
    /// Set to true when the annotation panel is opened — causes the text input to be focused.
    annotation_focus_pending: bool,
    /// Which per-photo filter rows are expanded, keyed by position index.
    filter_accordion_open: HashSet<usize>,
    /// Same for the gallery pre-filter stack.
    gallery_pre_accordion_open: HashSet<usize>,
    /// Same for the gallery post-filter stack.
    gallery_post_accordion_open: HashSet<usize>,
    /// Combined preset pool from all loaded galerie files, with source tags for save-back.
    presets: Vec<(FilterPreset, PresetSource)>,
    /// Pending name text in the "Save from stack" preset form.
    preset_save_name: String,
    /// Active find-similar state (Some while viewing a similarity result set).
    similar_state: Option<SimilarState>,
}

/// State retained while viewing a find-similar / find-similar-diverse result set.
struct SimilarState {
    namespace: String,
    count: usize,
    /// `Some(threshold)` for diverse mode, `None` for plain cosine similarity.
    threshold: Option<f32>,
    /// Collection index of the photo that was used as the query.
    query_ci: usize,
    /// Whether the parameter panel (sliders) is currently shown.
    panel_open: bool,
}

/// Identifies which galerie file owns a preset, so edits can be saved back.
#[derive(Clone)]
enum PresetSource {
    Collection(std::path::PathBuf),
    EditGallery(usize),
}

impl GalerieApp {
    pub fn new(
        _cc: &eframe::CreationContext,
        config: Config,
        collection: PhotoCollection,
        edit_galleries: Vec<EditGallery>,
    ) -> Self {
        let keybindings = config.build_keybinding_map().unwrap_or_else(|e| {
            log::error!("keybinding config error: {e}");
            Default::default()
        });

        let cache = ImageCache::new(config.general.cache_size);
        let viewer = ViewerState::new_tiling(config.general.tile_count);

        let (script_tx, script_rx) = crossbeam_channel::bounded(4);
        let (script_result_tx, script_result_rx) = crossbeam_channel::unbounded();

        std::thread::spawn(move || run_script_thread(script_rx));

        let mut app_state = AppState::default();
        app_state.background_color = collection
            .galerie_background()
            .unwrap_or(config.general.background_color);

        let initial_cols = {
            let cols = (config.general.tile_count as f64).sqrt().ceil() as usize;
            cols.max(1)
        };
        let n = collection.len();

        // Build the combined preset pool: collection galerie files first, then edit galleries.
        let mut presets: Vec<(FilterPreset, PresetSource)> = collection
            .all_presets()
            .map(|(p, path)| (p.clone(), PresetSource::Collection(path.to_path_buf())))
            .collect();
        for (idx, eg) in edit_galleries.iter().enumerate() {
            for p in &eg.presets {
                if !presets.iter().any(|(ep, _)| ep.name == p.name) {
                    presets.push((p.clone(), PresetSource::EditGallery(idx)));
                }
            }
        }

        Self {
            config,
            keybindings,
            collection,
            viewer,
            cache,
            overlay: OverlayState::default(),
            app_state,
            script_tx,
            script_result_tx,
            script_result_rx,
            edit_galleries,
            gallery_selector_open: false,
            gallery_selector_cursor: 0,
            last_tiling_cell_size: 200.0,
            last_tiling_cols: initial_cols,
            search_query: String::new(),
            filtered_indices: (0..n).collect(),
            annotation_input: String::new(),
            annotation_focus_pending: false,
            filter_accordion_open: HashSet::new(),
            gallery_pre_accordion_open: HashSet::new(),
            gallery_post_accordion_open: HashSet::new(),
            presets,
            preset_save_name: String::new(),
            similar_state: None,
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Drain deferred viewport commands (must be outside the input closure to avoid Wayland deadlocks).
        if let Some(fs) = self.app_state.fullscreen_pending.take() {
            log::debug!("handle_input: sending Fullscreen({fs})");
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fs));
            log::debug!("handle_input: Fullscreen({fs}) sent");
        }

        // Keep last_tiling_cols in sync so switching back from single restores the grid size.
        if let ViewerState::Tiling(s) = &self.viewer {
            self.last_tiling_cols = s.cols;
        }

        // Don't process keybindings when a text field has keyboard focus.
        if ctx.wants_keyboard_input() {
            // Escape still closes the annotations panel even while typing.
            if self.app_state.show_annotations {
                ctx.input(|i| {
                    if i.key_pressed(egui::Key::Escape) {
                        self.app_state.show_annotations = false;
                    }
                });
            }
            return;
        }

        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                {
                    // ── Gallery selector: intercepts all keys while open ──────────
                    if self.gallery_selector_open {
                        let n = self.edit_galleries.len();
                        match key {
                            egui::Key::ArrowUp | egui::Key::K => {
                                if self.gallery_selector_cursor > 0 {
                                    self.gallery_selector_cursor -= 1;
                                }
                            }
                            egui::Key::ArrowDown | egui::Key::J => {
                                if self.gallery_selector_cursor + 1 < n {
                                    self.gallery_selector_cursor += 1;
                                }
                            }
                            egui::Key::Space | egui::Key::Enter => {
                                self.toggle_at_cursor();
                            }
                            egui::Key::T | egui::Key::Escape => {
                                self.gallery_selector_open = false;
                            }
                            _ => {}
                        }
                        continue; // do not propagate to keybindings
                    }

                    // ── T intercept when edit-galleries are present ───────────────
                    if *key == egui::Key::T
                        && !modifiers.ctrl
                        && !modifiers.alt
                        && !self.edit_galleries.is_empty()
                    {
                        self.handle_gallery_t_press();
                        continue;
                    }

                    // ── Escape closes annotations panel before anything else ──────
                    if *key == egui::Key::Escape && self.app_state.show_annotations {
                        self.app_state.show_annotations = false;
                        continue;
                    }

                    // ── Escape exits fullscreen before doing anything else ────────
                    if *key == egui::Key::Escape && self.app_state.is_fullscreen {
                        log::debug!("Escape: queuing Fullscreen(false) for next frame");
                        self.app_state.is_fullscreen = false;
                        self.app_state.fullscreen_pending = Some(false);
                        continue;
                    }

                    // ── Escape clears similarity/search filter when already in tiling ─
                    if *key == egui::Key::Escape
                        && matches!(self.viewer, crate::viewer::ViewerState::Tiling(_))
                        && self.filtered_indices.len() < self.collection.len()
                    {
                        self.search_query.clear();
                        self.similar_state = None;
                        self.update_filter();
                        continue;
                    }

                    // ── Normal keybinding dispatch ────────────────────────────────
                    let mods = ModifierSet {
                        ctrl: modifiers.ctrl,
                        // Plus (Shift+= on most keyboards) inherently carries shift;
                        // strip it so a binding with modifiers=[] fires naturally.
                        shift: modifiers.shift && *key != egui::Key::Plus,
                        alt: modifiers.alt,
                    };
                    let action = self.keybindings.iter().find_map(|(combo, action)| {
                        if combo.key == *key && combo.modifiers == mods {
                            Some(action.clone())
                        } else {
                            None
                        }
                    });
                    if let Some(action) = action {
                        if matches!(action, crate::actions::Action::CycleBackground) {
                            self.handle_cycle_background();
                        } else if matches!(action, crate::actions::Action::ToggleAnnotations) {
                            self.app_state.show_annotations = !self.app_state.show_annotations;
                            if self.app_state.show_annotations {
                                self.annotation_focus_pending = true;
                            }
                        } else if let crate::actions::Action::FindSimilar { namespace, count } = &action {
                            self.handle_find_similar(namespace, *count);
                        } else if let crate::actions::Action::FindSimilarDiverse { namespace, count, threshold } = &action {
                            self.handle_find_similar_diverse(namespace, *count, *threshold);
                        } else {
                            let tile_count = self.last_tiling_cols * self.last_tiling_cols;
                            let mut cx = ActionContext {
                                collection: &mut self.collection,
                                viewer: &mut self.viewer,
                                cache: &mut self.cache,
                                overlay: &mut self.overlay,
                                app_state: &mut self.app_state,
                                tile_count,
                                script_tx: &self.script_tx,
                                script_result_tx: self.script_result_tx.clone(),
                                ctx: ctx.clone(),
                                display_len: self.filtered_indices.len(),
                                display_to_collection: &self.filtered_indices,
                            };
                            execute_action(&action, &mut cx);
                        }
                    }
                }
            }
        });
    }

    fn handle_cycle_background(&mut self) {
        self.app_state.background_color = self.app_state.background_color.cycle();
        let color = self.app_state.background_color;

        let paths = self.collection.galerie_file_paths();
        if paths.len() == 1 {
            if let Err(e) = self.collection.set_galerie_background(&paths[0], color) {
                self.overlay.push_toast(format!("Background save error: {e}"));
                return;
            }
            for eg in &mut self.edit_galleries {
                if eg.path.canonicalize().ok().as_deref() == Some(&paths[0]) {
                    eg.background_color = Some(color);
                }
            }
        }

        self.overlay.push_toast(format!("Background: {}", color.label()));
    }

    /// Translate a display (viewer) index to a collection index.
    #[inline]
    fn ci(&self, display_idx: usize) -> usize {
        self.filtered_indices[display_idx]
    }

    #[inline]
    fn display_len(&self) -> usize {
        self.filtered_indices.len()
    }

    /// Rebuild `filtered_indices` from the current `search_query` and clamp the viewer.
    fn update_filter(&mut self) {
        use crate::gallery::Annotation;
        let query = self.search_query.trim().to_lowercase();

        let old_ci = self.filtered_indices.get(self.viewer.focused_index()).copied();

        if query.is_empty() {
            self.filtered_indices = (0..self.collection.len()).collect();
        } else {
            let tokens: Vec<&str> = query.split_whitespace().collect();
            self.filtered_indices = (0..self.collection.len())
                .filter(|&i| {
                    let anns = &self.collection.entries[i].data.annotations;
                    tokens.iter().all(|token| {
                        if let Some(id_str) = token.strip_prefix("cluster:") {
                            if let Ok(cid) = id_str.parse::<u32>() {
                                return anns.iter().any(|a| matches!(a,
                                    Annotation::ClusterAssignment { cluster_id, .. }
                                    if *cluster_id == cid
                                ));
                            }
                        }
                        anns.iter().any(|ann| {
                            if let Annotation::Note { text } = ann {
                                text.to_lowercase().contains(token)
                            } else {
                                false
                            }
                        })
                    })
                })
                .collect();
        }

        let new_len = self.filtered_indices.len();
        if new_len == 0 { return; }

        // Preserve the previously focused photo's position, or go to 0.
        let new_di = old_ci
            .and_then(|ci| self.filtered_indices.iter().position(|&x| x == ci))
            .unwrap_or(0);

        match &mut self.viewer {
            crate::viewer::ViewerState::Tiling(s) => {
                let tc = s.tile_count();
                s.page = new_di / tc;
                s.selected = new_di % tc;
            }
            crate::viewer::ViewerState::Single(s) => {
                s.current_index = new_di;
            }
        }
    }

    fn handle_find_similar(&mut self, namespace: &str, count: usize) {
        let display_len = self.filtered_indices.len();
        if display_len == 0 { return; }
        let query_ci = self.filtered_indices[self.viewer.focused_index().min(display_len - 1)];
        self.find_similar_from_ci(namespace, count, query_ci);
        if self.filtered_indices.len() > 0 {
            self.similar_state = Some(SimilarState {
                namespace: namespace.to_owned(),
                count,
                threshold: None,
                query_ci,
                panel_open: false,
            });
        }
    }

    fn handle_find_similar_diverse(&mut self, namespace: &str, count: usize, threshold: f32) {
        let display_len = self.filtered_indices.len();
        if display_len == 0 { return; }
        let query_ci = self.filtered_indices[self.viewer.focused_index().min(display_len - 1)];
        self.find_similar_diverse_from_ci(namespace, count, threshold, query_ci);
        if self.filtered_indices.len() > 0 {
            self.similar_state = Some(SimilarState {
                namespace: namespace.to_owned(),
                count,
                threshold: Some(threshold),
                query_ci,
                panel_open: false,
            });
        }
    }

    fn find_similar_from_ci(&mut self, namespace: &str, count: usize, query_ci: usize) {
        let query_vec = self.collection.entries[query_ci].data.annotations.iter()
            .find_map(|a| a.decode_embedding(namespace));
        let Some(query) = query_vec else {
            self.overlay.push_toast(format!("No embedding (namespace: {namespace:?}) for this photo"));
            return;
        };
        let query_norm = l2_normalise(&query);
        let mut scored: Vec<(usize, f32)> = self.collection.entries.iter().enumerate()
            .filter_map(|(i, e)| {
                e.data.annotations.iter()
                    .find_map(|a| a.decode_embedding(namespace))
                    .map(|v| (i, dot(&query_norm, &l2_normalise(&v))))
            })
            .collect();
        if scored.is_empty() {
            self.overlay.push_toast(format!("No photos with embeddings (namespace: {namespace:?})"));
            return;
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(count);
        let new_len = scored.len();
        self.filtered_indices = scored.into_iter().map(|(i, _)| i).collect();
        self.viewer = crate::viewer::ViewerState::new_tiling(self.last_tiling_cols * self.last_tiling_cols);
        self.overlay.push_toast(format!("Showing {new_len} most similar (namespace: {namespace:?})"));
    }

    fn find_similar_diverse_from_ci(&mut self, namespace: &str, count: usize, threshold: f32, query_ci: usize) {
        let query_vec = self.collection.entries[query_ci].data.annotations.iter()
            .find_map(|a| a.decode_embedding(namespace));
        let Some(query) = query_vec else {
            self.overlay.push_toast(format!("No embedding (namespace: {namespace:?}) for this photo"));
            return;
        };
        let query_norm = l2_normalise(&query);
        let mut candidates: Vec<(usize, Vec<f32>, f32)> = self.collection.entries.iter().enumerate()
            .filter_map(|(i, e)| {
                e.data.annotations.iter()
                    .find_map(|a| a.decode_embedding(namespace))
                    .map(|v| {
                        let norm = l2_normalise(&v);
                        let sim = dot(&query_norm, &norm);
                        (i, norm, sim)
                    })
            })
            .collect();
        if candidates.is_empty() {
            self.overlay.push_toast(format!("No photos with embeddings (namespace: {namespace:?})"));
            return;
        }
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let min_sq = threshold * threshold;
        let mut selected: Vec<usize> = Vec::with_capacity(count);
        let mut selected_vecs: Vec<Vec<f32>> = Vec::with_capacity(count);
        for (ci, norm, _) in &candidates {
            if selected.len() >= count { break; }
            if !selected_vecs.iter().any(|sv| sq_dist(sv, norm) < min_sq) {
                selected.push(*ci);
                selected_vecs.push(norm.clone());
            }
        }
        if selected.is_empty() {
            self.overlay.push_toast("No diverse photos found (threshold may be too high)".to_owned());
            return;
        }
        let new_len = selected.len();
        self.filtered_indices = selected;
        self.viewer = crate::viewer::ViewerState::new_tiling(self.last_tiling_cols * self.last_tiling_cols);
        self.overlay.push_toast(format!("Showing {new_len} diverse similar (namespace: {namespace:?})"));
    }

    fn bg_color(&self) -> egui::Color32 {
        match self.app_state.background_color {
            BackgroundColor::Black => egui::Color32::from_gray(15),
            BackgroundColor::Gray  => egui::Color32::from_gray(128),
            BackgroundColor::White => egui::Color32::from_gray(245),
        }
    }

    // ── Gallery editing helpers ──────────────────────────────────────────────────

    fn handle_gallery_t_press(&mut self) {
        if self.edit_galleries.is_empty() || self.filtered_indices.is_empty() {
            return;
        }
        if self.edit_galleries.len() == 1 {
            // Direct toggle without selector
            let di = self.viewer.focused_index();
            let idx = self.ci(di);
            let path = self.collection.entries[idx].path.clone();
            let hash = self.collection.entries[idx].hash.clone();
            let added = self.edit_galleries[0].toggle(&path, &hash);
            let save_result = self.edit_galleries[0].save();
            let name = self.edit_galleries[0].name.clone();
            if let Err(e) = save_result {
                self.overlay.push_toast(format!("Save error: {e}"));
            } else {
                self.overlay.push_toast(if added {
                    format!("Added to {name}")
                } else {
                    format!("Removed from {name}")
                });
            }
        } else {
            // Multiple galleries: open/close the selector
            self.gallery_selector_open = !self.gallery_selector_open;
            if self.gallery_selector_open {
                self.gallery_selector_cursor = 0;
            }
        }
    }

    fn toggle_at_cursor(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let cursor = self.gallery_selector_cursor;
        if cursor >= self.edit_galleries.len() {
            return;
        }
        let di = self.viewer.focused_index();
        let idx = self.ci(di);
        let path = self.collection.entries[idx].path.clone();
        let hash = self.collection.entries[idx].hash.clone();
        self.edit_galleries[cursor].toggle(&path, &hash);
        let save_result = self.edit_galleries[cursor].save();
        if let Err(e) = save_result {
            self.overlay.push_toast(format!("Save error: {e}"));
        }
    }

    /// Render the floating gallery-membership selector (when open).
    fn render_similar_panel(&mut self, ctx: &egui::Context) {
        let Some(state) = &self.similar_state else { return; };
        if !state.panel_open { return; }

        // Snapshot params as locals — avoids borrow conflicts with self inside the closure.
        let mut count = state.count;
        let mut threshold = state.threshold;
        let namespace = state.namespace.clone();
        let query_ci = state.query_ci;
        let is_diverse = threshold.is_some();

        let mut changed = false;

        egui::Window::new("Similar params")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 30.0))
            .show(ctx, |ui| {
                egui::Grid::new("similar_panel_grid")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Count");
                        let mut c = count as i32;
                        if ui.add(egui::Slider::new(&mut c, 1..=200).integer()).changed() {
                            count = c as usize;
                            changed = true;
                        }
                        ui.end_row();

                        if let Some(ref mut t) = threshold {
                            ui.label("Threshold");
                            if ui.add(
                                egui::Slider::new(t, 0.0f32..=1.5)
                                    .fixed_decimals(2)
                                    .step_by(0.01),
                            ).changed() {
                                changed = true;
                            }
                            ui.end_row();
                        }
                    });
            });

        // Write changed params back and re-run if needed.
        if let Some(state) = &mut self.similar_state {
            state.count = count;
            if let (Some(ref mut st), Some(new_t)) = (&mut state.threshold, threshold) {
                *st = new_t;
            }
        }

        if changed {
            if is_diverse {
                let th = threshold.unwrap_or(0.3);
                self.find_similar_diverse_from_ci(&namespace, count, th, query_ci);
            } else {
                self.find_similar_from_ci(&namespace, count, query_ci);
            }
        }
    }

    fn render_gallery_selector(&mut self, ctx: &egui::Context) {
        if !self.gallery_selector_open || self.filtered_indices.is_empty() {
            return;
        }
        let focused_ci = self.ci(self.viewer.focused_index());
        let focused_path = self.collection.entries[focused_ci].path.clone();
        let filename = focused_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();

        // Collect display data before the closure to avoid borrow conflicts.
        let items: Vec<(String, bool)> = self
            .edit_galleries
            .iter()
            .map(|eg| (eg.name.clone(), eg.contains(&focused_path)))
            .collect();
        let cursor = self.gallery_selector_cursor;

        let mut toggle_at: Option<usize> = None;

        egui::Window::new("Edit galleries")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(&filename)
                        .size(11.0)
                        .color(egui::Color32::GRAY),
                );
                ui.separator();
                for (i, (name, is_member)) in items.iter().enumerate() {
                    let mark = if *is_member { "✓" } else { "  " };
                    let resp = ui.selectable_label(i == cursor, format!("{mark}  {name}"));
                    if resp.clicked() {
                        toggle_at = Some(i);
                    }
                }
                ui.separator();
                ui.label(
                    egui::RichText::new("↑↓  Space toggle  T/Esc close")
                        .size(10.0)
                        .color(egui::Color32::DARK_GRAY),
                );
            });

        if let Some(i) = toggle_at {
            self.gallery_selector_cursor = i;
            self.toggle_at_cursor();
        }
    }

    // ── Slideshow / prefetch / rendering ────────────────────────────────────────

    fn tick_slideshow(&mut self, ctx: &egui::Context) {
        if !self.app_state.slideshow_active {
            return;
        }
        let interval =
            Duration::from_secs_f64(self.config.general.slideshow_interval_secs);
        let elapsed = self.app_state.slideshow_last_at.elapsed();
        if elapsed >= interval {
            self.viewer
                .navigate(crate::viewer::Direction::Next, 1, self.display_len());
            self.app_state.slideshow_last_at = std::time::Instant::now();
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(interval - elapsed);
        }
    }

    /// Compose the effective filter stack for photo at display index:
    /// gallery pre-filters → per-photo stack (with Preset refs expanded) → gallery post-filters.
    fn effective_filters(&self, display_idx: usize) -> Vec<Filter> {
        let data = &self.collection.entries[self.ci(display_idx)].data;
        let gal = self.collection.galerie_filters();
        let (pre, post) = gal.as_ref()
            .map(|(a, b)| (a.as_slice(), b.as_slice()))
            .unwrap_or((&[], &[]));

        let mut out = Vec::new();
        out.extend_from_slice(pre);
        for item in &data.filters {
            match item {
                Filter::Preset { name } => {
                    if let Some((p, _)) = self.presets.iter().find(|(p, _)| p.name == *name) {
                        out.extend(p.filters.iter().cloned());
                    }
                    // unknown preset name → silently skip
                }
                f => out.push(f.clone()),
            }
        }
        out.extend_from_slice(post);
        out
    }

    /// Invalidate cache for every photo whose stack contains a reference to `preset_name`.
    fn invalidate_preset_dependents(&mut self, preset_name: &str) {
        for i in 0..self.collection.entries.len() {
            if self.collection.entries[i].data.filters.iter()
                .any(|f| matches!(f, Filter::Preset { name } if name == preset_name))
            {
                self.cache.invalidate(i);
            }
        }
    }

    #[allow(dead_code)]
    /// Rebuild the preset pool from all current galerie sources.
    fn rebuild_presets(&mut self) {
        let mut presets: Vec<(FilterPreset, PresetSource)> = self.collection
            .all_presets()
            .map(|(p, path)| (p.clone(), PresetSource::Collection(path.to_path_buf())))
            .collect();
        for (idx, eg) in self.edit_galleries.iter().enumerate() {
            for p in &eg.presets {
                if !presets.iter().any(|(ep, _)| ep.name == p.name) {
                    presets.push((p.clone(), PresetSource::EditGallery(idx)));
                }
            }
        }
        self.presets = presets;
    }

    fn prefetch_visible(&mut self, ctx: &egui::Context) {
        let total = self.display_len();
        let max_size = match &self.viewer {
            ViewerState::Single(_) => None,
            ViewerState::Tiling(_) => {
                let phys = (self.last_tiling_cell_size * ctx.pixels_per_point()).round() as u32;
                Some(phys.max(64))
            }
        };
        for di in self.viewer.visible_indices(total) {
            let ci = self.ci(di);
            let path = self.collection.entries[ci].path.clone();
            let filters = self.effective_filters(di);
            self.cache.get_or_request(ci, &path, &filters, max_size, ctx);
        }
    }

    fn render_single(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.filtered_indices.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(if self.collection.is_empty() { "No images found." } else { "No results." });
            });
            return;
        }

        let di = self.viewer.focused_index();
        let idx = self.ci(di);
        let path = self.collection.entries[idx].path.clone();
        let rating = self.collection.entries[idx].data.rating;
        let filters = self.effective_filters(di);
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();

        let avail = ui.available_rect_before_wrap();

        // Interact for zoom (scroll) and pan (drag) — allocated over the full panel.
        let resp = ui.interact(avail, egui::Id::new("single_view"), egui::Sense::click_and_drag());
        if resp.dragged() {
            if let ViewerState::Single(s) = &mut self.viewer {
                let d = resp.drag_delta();
                s.pan[0] += d.x;
                s.pan[1] += d.y;
            }
        }
        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll_y != 0.0 {
            let delta = scroll_y / 100.0;
            self.viewer.zoom_single(delta);
        }

        // Context menu (right-click).
        let path_clone = path.clone();
        let filters_clone = filters.clone();
        resp.context_menu(|ui| {
            if ui.button("Copy source path").clicked() {
                ui.ctx().copy_text(path_clone.to_string_lossy().to_string());
                ui.close_menu();
            }
            if ui.button("Copy image (PNG)").clicked() {
                let _ = copy_image_to_clipboard(&path_clone, &filters_clone);
                ui.close_menu();
            }
            if ui.button("Export (apply filters) & copy path").clicked() {
                match export_to_temp(&path_clone, &filters_clone) {
                    Ok(tmp) => ui.ctx().copy_text(tmp.to_string_lossy().to_string()),
                    Err(e) => { let _ = e; }
                }
                ui.close_menu();
            }
        });

        match self.cache.get_or_request(idx, &path, &filters, None, ctx) {
            LoadState::Ready(tex) => {
                let size = tex.size();
                let ratio = size[0] as f32 / size[1] as f32;
                let fit = fit_rect(avail, ratio);

                // Handle zoom-to-one: set zoom so 1 image pixel = 1 screen pixel.
                if self.app_state.zoom_to_one_pending {
                    let scale = size[0] as f32 / fit.width();
                    if let ViewerState::Single(s) = &mut self.viewer {
                        s.zoom = scale;
                        s.pan = [0.0, 0.0];
                    }
                    self.app_state.zoom_to_one_pending = false;
                }

                let (zoom, pan) = if let ViewerState::Single(s) = &self.viewer {
                    (s.zoom, egui::vec2(s.pan[0], s.pan[1]))
                } else {
                    (1.0, egui::Vec2::ZERO)
                };

                let draw_rect = if (zoom - 1.0).abs() < 1e-4 && pan == egui::Vec2::ZERO {
                    fit
                } else {
                    egui::Rect::from_center_size(avail.center() + pan, fit.size() * zoom)
                };

                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                ui.painter().image(tex.id(), draw_rect, uv, egui::Color32::WHITE);
                if self.overlay.show_filename {
                    OverlayState::render_filename(ui, draw_rect, &filename);
                }
                if self.overlay.show_rating {
                    OverlayState::render_rating(ui, draw_rect, rating);
                }
                // Gallery membership badge (bottom-left, above filename if shown)
                if !self.edit_galleries.is_empty() {
                    self.render_membership_badge(ui, draw_rect, &path);
                }
                // Histogram overlay (bottom-right, anchored to viewport not image)
                if self.app_state.show_histogram {
                    if let Some(hist) = self.cache.get_histogram(idx) { // idx is already collection idx
                        render_histogram_overlay(ui, avail, hist);
                    }
                }
            }
            LoadState::Pending | LoadState::NotRequested => {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                });
            }
            LoadState::Error(e) => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Error: {e}"))
                            .color(egui::Color32::RED),
                    );
                });
            }
        }
    }

    /// Draw a small membership list at the bottom-left of `rect` in single mode.
    fn render_membership_badge(&self, ui: &egui::Ui, rect: egui::Rect, path: &std::path::Path) {
        let painter = ui.painter();
        let font = egui::FontId::proportional(11.0);
        let line_h = 14.0;
        let pad = 4.0;

        let mut y = rect.max.y - pad;
        for eg in &self.edit_galleries {
            let (mark, color) = if eg.contains(path) {
                ("✓", egui::Color32::from_rgb(255, 200, 60))
            } else {
                ("·", egui::Color32::from_rgba_unmultiplied(200, 200, 200, 120))
            };
            y -= line_h;
            painter.text(
                egui::pos2(rect.min.x + pad, y),
                egui::Align2::LEFT_TOP,
                format!("{mark} {}", eg.name),
                font.clone(),
                color,
            );
        }
    }

    fn render_tiling(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.filtered_indices.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(if self.collection.is_empty() { "No images found." } else { "No results." });
            });
            return;
        }

        let total = self.display_len();
        let (page, cols, selected_in_page) = match &self.viewer {
            ViewerState::Tiling(s) => (s.page, s.cols, s.selected),
            ViewerState::Single(_) => unreachable!(),
        };
        let tile_count = cols * cols;
        let start = page * tile_count;
        let end = (start + tile_count).min(total);
        let focused_di = (start + selected_in_page).min(total - 1);
        let focused_ci = self.ci(focused_di);

        // Header
        ui.horizontal(|ui| {
            let total_pages = total.div_ceil(tile_count);
            let fname = self.collection.entries[focused_ci]
                .path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
            let search_indicator = if !self.search_query.is_empty() {
                format!(" (filtered: {total}/{} total)", self.collection.len())
            } else { String::new() };
            ui.label(
                egui::RichText::new(format!(
                    "Page {}/{total_pages}  ·  {focused_di}/{total}  ·  {fname}{search_indicator}",
                    page + 1
                ))
                .size(12.0)
                .color(egui::Color32::GRAY),
            );
        });

        let available_w = ui.available_width();
        let spacing = 2.0;
        let cell_size = (available_w - spacing * (cols as f32 - 1.0)) / cols as f32;
        self.last_tiling_cell_size = cell_size;
        let tile_max_size = Some(((cell_size * ui.ctx().pixels_per_point()).round() as u32).max(64));
        let cell_sz = egui::vec2(cell_size, cell_size);

        // Collect tile data before the grid (avoids borrow conflicts).
        let gal_info = self.collection.galerie_filters();
        let entries: Vec<_> = (start..end)
            .map(|di| {
                let ci = self.ci(di);
                let e = &self.collection.entries[ci];
                let in_edit = self.edit_galleries.iter().any(|eg| eg.contains(&e.path));
                let filters = compose_filters(&e.data.filters, gal_info.as_ref());
                (ci, e.path.clone(), e.data.rating, filters, in_edit)
            })
            .collect();

        let mut clicked_di: Option<usize> = None;
        let mut selected_rect: Option<egui::Rect> = None;

        egui::ScrollArea::vertical()
            .id_salt("tile_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("tile_grid")
                    .num_columns(cols)
                    .spacing([spacing, spacing])
                    .show(ui, |ui| {
                        for (tile_in_page, (ci, path, rating, filters, in_edit)) in entries.into_iter().enumerate() {
                            let is_selected = tile_in_page == selected_in_page;
                            let load_state = self.cache.get_or_request(ci, &path, &filters, tile_max_size, ctx);
                            let response = self.render_tile(ui, load_state, rating, cell_sz, is_selected, in_edit);
                            if is_selected {
                                selected_rect = Some(response.rect);
                            }
                            if response.clicked() {
                                clicked_di = Some(start + tile_in_page);
                            }
                            if (tile_in_page + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });

                if self.app_state.needs_scroll_to_selection {
                    if let Some(rect) = selected_rect {
                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    }
                    self.app_state.needs_scroll_to_selection = false;
                }
            });

        if let Some(di) = clicked_di {
            self.viewer.switch_to_single(di);
        }
    }

    fn render_tile(
        &mut self,
        ui: &mut egui::Ui,
        load_state: LoadState,
        rating: Option<u8>,
        cell_sz: egui::Vec2,
        is_selected: bool,
        in_edit_gallery: bool,
    ) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(cell_sz, egui::Sense::click());

        ui.painter().rect_filled(rect, 0.0, self.bg_color());

        match load_state {
            LoadState::Ready(tex) => {
                let size = tex.size();
                let ratio = size[0] as f32 / size[1] as f32;
                let fit = fit_rect(rect, ratio);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                ui.painter().image(tex.id(), fit, uv, egui::Color32::WHITE);
                if self.overlay.show_rating {
                    OverlayState::render_rating(ui, rect, rating);
                }
            }
            LoadState::Pending | LoadState::NotRequested => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "⏳",
                    egui::FontId::proportional(20.0),
                    egui::Color32::GRAY,
                );
            }
            LoadState::Error(_) => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "✗",
                    egui::FontId::proportional(20.0),
                    egui::Color32::RED,
                );
            }
        }

        // Gallery membership: small amber dot in the top-right corner.
        if in_edit_gallery {
            let dot_size = (cell_sz.x * 0.08).clamp(5.0, 12.0);
            let dot_pos = egui::pos2(rect.max.x - dot_size - 2.0, rect.min.y + 2.0);
            ui.painter().circle_filled(
                dot_pos + egui::vec2(dot_size / 2.0, dot_size / 2.0),
                dot_size / 2.0,
                egui::Color32::from_rgb(255, 190, 30),
            );
        }

        // Selection / hover border (drawn on top of everything else).
        if is_selected {
            ui.painter().rect_stroke(
                rect, 0.0,
                egui::Stroke::new(3.0, egui::Color32::from_rgb(100, 200, 255)),
                egui::StrokeKind::Middle,
            );
        } else if response.hovered() {
            ui.painter().rect_stroke(
                rect, 0.0,
                egui::Stroke::new(2.0, egui::Color32::WHITE),
                egui::StrokeKind::Middle,
            );
        }

        response
    }
    // ── Annotations panel ───────────────────────────────────────────────────────

    fn render_annotations_panel(&mut self, ctx: &egui::Context) {
        if !self.app_state.show_annotations || self.filtered_indices.is_empty() {
            return;
        }

        let di = self.viewer.focused_index();
        let idx = self.ci(di);
        let filename = self.collection.entries[idx]
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let current = self.collection.entries[idx].data.annotations.clone();

        let mut to_remove: Option<usize> = None;
        let mut add_text: Option<String> = None;
        let mut close = false;

        let mut open = self.app_state.show_annotations;
        egui::Window::new("Annotations")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .min_width(280.0)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(&filename)
                        .size(11.0)
                        .color(egui::Color32::GRAY),
                );
                ui.add_space(4.0);

                if current.is_empty() {
                    ui.label(
                        egui::RichText::new("No annotations yet.")
                            .italics()
                            .color(egui::Color32::GRAY),
                    );
                } else {
                    use crate::gallery::Annotation;
                    for (i, ann) in current.iter().enumerate() {
                        let Annotation::Note { text } = ann else { continue };
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("✎").color(egui::Color32::GRAY));
                            ui.label(text);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("×").on_hover_text("Remove").clicked() {
                                    to_remove = Some(i);
                                }
                            });
                        });
                    }
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);

                let has_galerie = self.collection.entries[idx].galerie_source.is_some();
                if has_galerie {
                    ui.horizontal(|ui| {
                        let input = ui.add(
                            egui::TextEdit::singleline(&mut self.annotation_input)
                                .hint_text("Add note…")
                                .desired_width(ui.available_width() - 50.0),
                        );
                        if self.annotation_focus_pending {
                            input.request_focus();
                            self.annotation_focus_pending = false;
                        }
                        let submit = ui.button("Add");
                        let pressed_enter = input.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (submit.clicked() || pressed_enter) && !self.annotation_input.trim().is_empty() {
                            add_text = Some(self.annotation_input.trim().to_owned());
                            self.annotation_input.clear();
                            input.request_focus();
                        }
                    });
                } else {
                    self.annotation_focus_pending = false;
                    ui.label(
                        egui::RichText::new("Open a .galerie file to save annotations.")
                            .italics()
                            .color(egui::Color32::GRAY),
                    );
                }
            });

        if !open {
            close = true;
        }
        if close {
            self.app_state.show_annotations = false;
        }

        if to_remove.is_some() || add_text.is_some() {
            use crate::gallery::Annotation;
            let mut new_data = self.collection.entries[idx].data.clone();
            if let Some(i) = to_remove {
                new_data.annotations.remove(i);
            }
            if let Some(text) = add_text {
                new_data.annotations.push(Annotation::Note { text });
            }
            if let Err(e) = self.collection.update_data(idx, new_data) {
                self.overlay.push_toast(format!("Annotation error: {e}"));
            } else if !self.search_query.is_empty() {
                self.update_filter();
            }
        }
    }

    // ── Filter sidebar ──────────────────────────────────────────────────────────

    fn render_filter_sidebar(&mut self, ctx: &egui::Context) {
        if !self.app_state.filter_sidebar_open { return; }
        let is_single = matches!(&self.viewer, ViewerState::Single(_));
        if self.collection.is_empty() { return; }

        // ── Gallery-level filter state ──────────────────────────────────────────
        let gal_path = self.collection.single_galerie_path();
        let (mut gal_pre, mut gal_post) = self.collection
            .galerie_filters()
            .unwrap_or_else(|| (vec![], vec![]));
        let mut gal_changed = false;
        // (tag, index, open) — tag distinguishes pre ("pre") from post ("post")
        let mut gal_toggle_req: Option<(&str, usize, bool)> = None;
        let mut gal_pre_remove: Option<usize> = None;
        let mut gal_post_remove: Option<usize> = None;
        let mut gal_pre_add: Option<Filter> = None;
        let mut gal_post_add: Option<Filter> = None;

        // ── Per-photo filter state ──────────────────────────────────────────────
        // idx is display index; ci is collection index used for data and cache access.
        let idx = if is_single { Some(self.viewer.focused_index()) } else { None };
        let ci = idx.map(|di| self.ci(di));
        let photo_histogram: Option<ImageHistogram> = ci.and_then(|i| self.cache.get_histogram(i).cloned());
        let mut photo_stack = ci
            .map(|i| self.collection.entries[i].data.filters.clone())
            .unwrap_or_default();
        let presets_snap: Vec<(FilterPreset, PresetSource)> = self.presets.clone();
        let mut photo_changed = false;
        let mut photo_toggle_req: Option<(usize, bool)> = None;
        let mut photo_to_remove: Option<usize> = None;
        let mut photo_to_add: Option<Filter> = None;
        // Preset-specific intents
        let mut preset_inline_changed: Option<(usize, Vec<Filter>)> = None; // (presets_snap idx, new filters)
        let mut preset_insert: Option<usize> = None;   // insert presets_snap[i] into photo stack
        let mut preset_lib_delete: Option<usize> = None;
        let mut save_stack_as_preset = false;

        let acc = self.filter_accordion_open.clone();
        let acc_pre = self.gallery_pre_accordion_open.clone();
        let acc_post = self.gallery_post_accordion_open.clone();
        let mut preset_save_name = self.preset_save_name.clone();

        egui::SidePanel::right("filter_sidebar")
            .resizable(true)
            .min_width(210.0)
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {

                        // ── Gallery pre-filters ─────────────────────────────────
                        if gal_path.is_some() {
                            ui.strong("Before photo filters");
                            ui.label(egui::RichText::new("Applied before each photo's own filters").color(egui::Color32::GRAY).small());
                            ui.add_space(2.0);
                            if gal_pre.is_empty() {
                                ui.label(egui::RichText::new("(none)").color(egui::Color32::GRAY).italics());
                            }
                            for (i, filter) in gal_pre.iter_mut().enumerate() {
                                let kind = filter_kind_name(filter);
                                let is_open = acc_pre.contains(&i);
                                let has_params = filter_has_params(filter);
                                ui.horizontal(|ui| {
                                    if has_params {
                                        if ui.small_button(if is_open { "▼" } else { "▶" }).clicked() {
                                            gal_toggle_req = Some(("pre", i, !is_open));
                                        }
                                    } else { ui.add_space(18.0); }
                                    ui.strong(kind);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("×").on_hover_text("Remove").clicked() { gal_pre_remove = Some(i); }
                                    });
                                });
                                if is_open && has_params {
                                    ui.indent(egui::Id::new(("gpre", i)), |ui| {
                                        if render_filter_params(ui, filter, None) { gal_changed = true; }
                                    });
                                }
                                ui.add_space(2.0);
                            }
                            ui.label(egui::RichText::new("Add:").color(egui::Color32::GRAY).small());
                            ui.horizontal_wrapped(|ui| {
                                for (label, default) in filter_add_list() {
                                    if ui.small_button(label).clicked() { gal_pre_add = Some(default); }
                                }
                            });

                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(4.0);
                        }

                        // ── Per-photo filters ───────────────────────────────────
                        if is_single {
                            ui.strong("Photo filters");
                            ui.add_space(2.0);
                            if photo_stack.is_empty() {
                                ui.label(egui::RichText::new("(none)").color(egui::Color32::GRAY).italics());
                            }
                            for (i, filter) in photo_stack.iter_mut().enumerate() {
                                let is_open = acc.contains(&i);
                                match filter {
                                    Filter::Preset { name } => {
                                        let preset_snap_idx = presets_snap.iter().position(|(p, _)| p.name == *name);
                                        ui.horizontal(|ui| {
                                            if preset_snap_idx.is_some() {
                                                if ui.small_button(if is_open { "▼" } else { "▶" }).clicked() {
                                                    photo_toggle_req = Some((i, !is_open));
                                                }
                                            } else {
                                                ui.add_space(18.0);
                                            }
                                            ui.strong(format!("Preset: {name}"));
                                            if preset_snap_idx.is_none() {
                                                ui.label(egui::RichText::new("(missing)").color(egui::Color32::RED).small());
                                            }
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.small_button("×").on_hover_text("Remove from stack").clicked() {
                                                    photo_to_remove = Some(i);
                                                }
                                            });
                                        });
                                        if is_open {
                                            if let Some(psi) = preset_snap_idx {
                                                let (preset, _) = &presets_snap[psi];
                                                let mut sub_filters = preset.filters.clone();
                                                let mut sub_changed = false;
                                                ui.indent(egui::Id::new(("preset_inline", i)), |ui| {
                                                    for (j, sub) in sub_filters.iter_mut().enumerate() {
                                                        let sub_kind = filter_kind_name(sub);
                                                        ui.label(egui::RichText::new(&sub_kind).strong().small());
                                                        if filter_has_params(sub) {
                                                            if render_filter_params(ui, sub, photo_histogram.as_ref()) {
                                                                sub_changed = true;
                                                            }
                                                        }
                                                        let _ = j;
                                                        ui.add_space(2.0);
                                                    }
                                                });
                                                if sub_changed {
                                                    preset_inline_changed = Some((psi, sub_filters));
                                                }
                                            }
                                        }
                                    }
                                    physical => {
                                        let kind = filter_kind_name(physical);
                                        let has_params = filter_has_params(physical);
                                        ui.horizontal(|ui| {
                                            if has_params {
                                                if ui.small_button(if is_open { "▼" } else { "▶" }).clicked() {
                                                    photo_toggle_req = Some((i, !is_open));
                                                }
                                            } else { ui.add_space(18.0); }
                                            ui.strong(kind);
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.small_button("×").on_hover_text("Remove").clicked() { photo_to_remove = Some(i); }
                                            });
                                        });
                                        if is_open && has_params {
                                            ui.indent(egui::Id::new(("fp", i)), |ui| {
                                                if render_filter_params(ui, physical, photo_histogram.as_ref()) { photo_changed = true; }
                                            });
                                        }
                                    }
                                }
                                ui.add_space(2.0);
                            }
                            ui.label(egui::RichText::new("Add:").color(egui::Color32::GRAY).small());
                            ui.horizontal_wrapped(|ui| {
                                for (label, default) in filter_add_list() {
                                    if ui.small_button(label).clicked() { photo_to_add = Some(default); }
                                }
                            });

                            // ── Presets library ─────────────────────────────────
                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(2.0);
                            ui.strong("Presets");
                            if presets_snap.is_empty() {
                                ui.label(egui::RichText::new("(none saved)").color(egui::Color32::GRAY).italics());
                            }
                            for (pi, (preset, _)) in presets_snap.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.small_button("Insert").clicked() { preset_insert = Some(pi); }
                                    ui.label(&preset.name);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("×").on_hover_text("Delete preset").clicked() { preset_lib_delete = Some(pi); }
                                    });
                                });
                            }
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut preset_save_name)
                                    .hint_text("Preset name")
                                    .desired_width(120.0));
                                if ui.small_button("Save from stack").clicked() && !preset_save_name.is_empty() {
                                    save_stack_as_preset = true;
                                }
                            });
                        } else if gal_path.is_none() {
                            ui.label(egui::RichText::new("Open single-photo view to edit photo filters.").color(egui::Color32::GRAY).italics());
                        }

                        // ── Gallery post-filters ────────────────────────────────
                        if gal_path.is_some() {
                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(4.0);
                            ui.strong("After photo filters");
                            ui.label(egui::RichText::new("Applied after each photo's own filters").color(egui::Color32::GRAY).small());
                            ui.add_space(2.0);
                            if gal_post.is_empty() {
                                ui.label(egui::RichText::new("(none)").color(egui::Color32::GRAY).italics());
                            }
                            for (i, filter) in gal_post.iter_mut().enumerate() {
                                let kind = filter_kind_name(filter);
                                let is_open = acc_post.contains(&i);
                                let has_params = filter_has_params(filter);
                                ui.horizontal(|ui| {
                                    if has_params {
                                        if ui.small_button(if is_open { "▼" } else { "▶" }).clicked() {
                                            gal_toggle_req = Some(("post", i, !is_open));
                                        }
                                    } else { ui.add_space(18.0); }
                                    ui.strong(kind);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("×").on_hover_text("Remove").clicked() { gal_post_remove = Some(i); }
                                    });
                                });
                                if is_open && has_params {
                                    ui.indent(egui::Id::new(("gpost", i)), |ui| {
                                        if render_filter_params(ui, filter, None) { gal_changed = true; }
                                    });
                                }
                                ui.add_space(2.0);
                            }
                            ui.label(egui::RichText::new("Add:").color(egui::Color32::GRAY).small());
                            ui.horizontal_wrapped(|ui| {
                                for (label, default) in filter_add_list() {
                                    if ui.small_button(label).clicked() { gal_post_add = Some(default); }
                                }
                            });
                        }
                    });
            });

        self.preset_save_name = preset_save_name;

        // ── Apply preset mutations ──────────────────────────────────────────────

        // Inline edit of a preset's physical filters → save back to source, invalidate
        if let Some((psi, new_sub_filters)) = preset_inline_changed {
            let (preset, source) = &self.presets[psi];
            let preset_name = preset.name.clone();
            let source = source.clone();
            self.presets[psi].0.filters = new_sub_filters.clone();
            match source {
                PresetSource::Collection(ref path) => {
                    let mut ps = self.collection.galerie_presets(path);
                    if let Some(p) = ps.iter_mut().find(|p| p.name == preset_name) {
                        p.filters = new_sub_filters;
                    }
                    if let Err(e) = self.collection.set_galerie_presets(path, ps) {
                        self.overlay.push_toast(format!("Preset save error: {e}"));
                    }
                }
                PresetSource::EditGallery(idx) => {
                    if let Some(eg) = self.edit_galleries.get_mut(idx) {
                        if let Some(p) = eg.presets.iter_mut().find(|p| p.name == preset_name) {
                            p.filters = new_sub_filters;
                        }
                        if let Err(e) = eg.save() {
                            self.overlay.push_toast(format!("Preset save error: {e}"));
                        }
                    }
                }
            }
            self.invalidate_preset_dependents(&preset_name);
        }

        // Insert a preset reference into the current photo's stack
        if let (Some(pi), Some(collection_idx)) = (preset_insert, ci) {
            let name = self.presets[pi].0.name.clone();
            let mut new_data = self.collection.entries[collection_idx].data.clone();
            new_data.filters.push(Filter::Preset { name });
            if let Err(e) = self.collection.update_data(collection_idx, new_data) {
                self.overlay.push_toast(format!("Filter error: {e}"));
            } else {
                self.cache.invalidate(collection_idx);
            }
        }

        // Save physical (non-Preset) filters from current photo stack as a new preset
        if save_stack_as_preset {
            let name = self.preset_save_name.clone();
            if !name.is_empty() {
                let physical_filters: Vec<Filter> = photo_stack.iter()
                    .filter(|f| !matches!(f, Filter::Preset { .. }))
                    .cloned()
                    .collect();
                let new_preset = FilterPreset { name: name.clone(), filters: physical_filters };
                // Target: first Collection galerie, or first EditGallery
                let saved = if let Some(path) = &gal_path {
                    let path = path.clone();
                    let mut presets = self.collection.galerie_presets(&path);
                    presets.retain(|p| p.name != name);
                    presets.push(new_preset.clone());
                    match self.collection.set_galerie_presets(&path, presets) {
                        Ok(()) => { self.presets.retain(|(p, _)| p.name != name); self.presets.push((new_preset, PresetSource::Collection(path))); true }
                        Err(e) => { self.overlay.push_toast(format!("Preset save error: {e}")); false }
                    }
                } else if let Some(eg) = self.edit_galleries.first_mut() {
                    eg.presets.retain(|p| p.name != name);
                    eg.presets.push(new_preset.clone());
                    let eg_idx = 0;
                    match eg.save() {
                        Ok(()) => { self.presets.retain(|(p, _)| p.name != name); self.presets.push((new_preset, PresetSource::EditGallery(eg_idx))); true }
                        Err(e) => { self.overlay.push_toast(format!("Preset save error: {e}")); false }
                    }
                } else {
                    self.overlay.push_toast("No galerie file loaded — cannot save preset.".to_string());
                    false
                };
                if saved { self.preset_save_name.clear(); self.invalidate_preset_dependents(&name); }
            }
        }

        // Delete a preset from its source galerie
        if let Some(pi) = preset_lib_delete {
            let (preset, source) = self.presets.remove(pi);
            let preset_name = preset.name.clone();
            match source {
                PresetSource::Collection(ref path) => {
                    let updated: Vec<_> = self.collection.galerie_presets(path)
                        .into_iter()
                        .filter(|p| p.name != preset_name)
                        .collect();
                    if let Err(e) = self.collection.set_galerie_presets(path, updated) {
                        self.overlay.push_toast(format!("Preset delete error: {e}"));
                    }
                }
                PresetSource::EditGallery(idx) => {
                    if let Some(eg) = self.edit_galleries.get_mut(idx) {
                        eg.presets.retain(|p| p.name != preset_name);
                        if let Err(e) = eg.save() {
                            self.overlay.push_toast(format!("Preset delete error: {e}"));
                        }
                    }
                }
            }
            self.invalidate_preset_dependents(&preset_name);
        }

        // ── Apply gallery-filter changes ────────────────────────────────────────
        if let Some((tag, i, open)) = gal_toggle_req {
            let set = if tag == "pre" { &mut self.gallery_pre_accordion_open } else { &mut self.gallery_post_accordion_open };
            if open { set.insert(i); } else { set.remove(&i); }
        }
        if let Some(i) = gal_pre_remove {
            gal_pre.remove(i);
            self.gallery_pre_accordion_open = accordion_after_remove(&self.gallery_pre_accordion_open, i);
            gal_changed = true;
        }
        if let Some(i) = gal_post_remove {
            gal_post.remove(i);
            self.gallery_post_accordion_open = accordion_after_remove(&self.gallery_post_accordion_open, i);
            gal_changed = true;
        }
        if let Some(f) = gal_pre_add {
            self.gallery_pre_accordion_open.insert(gal_pre.len());
            gal_pre.push(f);
            gal_changed = true;
        }
        if let Some(f) = gal_post_add {
            self.gallery_post_accordion_open.insert(gal_post.len());
            gal_post.push(f);
            gal_changed = true;
        }
        if gal_changed {
            if let Some(ref path) = gal_path {
                if let Err(e) = self.collection.set_galerie_filters(path, gal_pre, gal_post) {
                    self.overlay.push_toast(format!("Gallery filter error: {e}"));
                } else {
                    self.cache.invalidate_all();
                }
            }
        }

        // ── Apply per-photo filter changes ──────────────────────────────────────
        if let Some((i, open)) = photo_toggle_req {
            if open { self.filter_accordion_open.insert(i); } else { self.filter_accordion_open.remove(&i); }
        }
        if let Some(i) = photo_to_remove {
            photo_stack.remove(i);
            self.filter_accordion_open = accordion_after_remove(&self.filter_accordion_open, i);
            photo_changed = true;
        }
        if let Some(f) = photo_to_add {
            self.filter_accordion_open.insert(photo_stack.len());
            photo_stack.push(f);
            photo_changed = true;
        }
        if photo_changed {
            if let Some(collection_idx) = ci {
                let mut new_data = self.collection.entries[collection_idx].data.clone();
                new_data.filters = photo_stack;
                if let Err(e) = self.collection.update_data(collection_idx, new_data) {
                    self.overlay.push_toast(format!("Filter error: {e}"));
                } else {
                    self.cache.invalidate(collection_idx);
                }
            }
        }
    }
}

impl eframe::App for GalerieApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Poll image cache
        self.cache.poll(ctx);

        // 2. Poll script results
        while let Ok(res) = self.script_result_rx.try_recv() {
            let msg = if res.success {
                res.output
            } else {
                format!("Script failed: {}", res.output)
            };
            self.overlay.push_toast(msg);
            self.app_state.script_running = false;
        }

        // 3. Handle keyboard input
        self.handle_input(ctx);

        // 4. Slideshow advance
        self.tick_slideshow(ctx);

        // 5. Quit
        if self.app_state.should_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // 6. Prefetch visible images
        self.prefetch_visible(ctx);

        // 7. Render top bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let mode_label = match &self.viewer {
                    ViewerState::Tiling(s) => format!("⊞ {}×{}", s.cols, s.cols),
                    ViewerState::Single(_) => "⬛ Single".to_owned(),
                };
                ui.label(egui::RichText::new(mode_label).size(12.0).color(egui::Color32::LIGHT_GRAY));
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("{} photos", self.collection.len()))
                        .size(12.0)
                        .color(egui::Color32::GRAY),
                );
                ui.separator();
                let search_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .hint_text("search annotations… (cluster:N)")
                        .desired_width(160.0)
                        .font(egui::FontId::proportional(12.0)),
                );
                if search_resp.changed() {
                    self.update_filter();
                }
                if !self.search_query.is_empty() && ui.small_button("×").on_hover_text("Clear search").clicked() {
                    self.search_query.clear();
                    self.update_filter();
                }
                if !self.edit_galleries.is_empty() {
                    ui.separator();
                    let label = if self.edit_galleries.len() == 1 {
                        format!("✏ {}", self.edit_galleries[0].name)
                    } else {
                        format!("✏ {} galleries", self.edit_galleries.len())
                    };
                    ui.label(
                        egui::RichText::new(label)
                            .size(12.0)
                            .color(egui::Color32::from_rgb(255, 190, 30)),
                    );
                }
                if self.app_state.slideshow_active {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("▶")
                            .size(12.0)
                            .color(egui::Color32::GREEN),
                    );
                    let mut interval = self.config.general.slideshow_interval_secs as f32;
                    let resp = ui.add(
                        egui::DragValue::new(&mut interval)
                            .range(0.5f32..=60.0)
                            .speed(0.1)
                            .suffix("s"),
                    );
                    if resp.changed() {
                        self.config.general.slideshow_interval_secs = interval as f64;
                    }
                }
                if self.app_state.script_running {
                    ui.separator();
                    ui.spinner();
                    ui.label(egui::RichText::new("Running script…").size(12.0).color(egui::Color32::YELLOW));
                }
                if matches!(&self.viewer, ViewerState::Tiling(_)) {
                    if let Some(ref mut ss) = self.similar_state {
                        ui.separator();
                        let label = if ss.threshold.is_some() { "≈∙" } else { "≈" };
                        let btn = egui::Button::new(
                            egui::RichText::new(label).size(13.0).color(egui::Color32::from_rgb(100, 200, 255))
                        ).selected(ss.panel_open);
                        let hover = if ss.threshold.is_some() {
                            "Similar (diverse) — click to adjust params"
                        } else {
                            "Similar — click to adjust params"
                        };
                        if ui.add(btn).on_hover_text(hover).clicked() {
                            ss.panel_open = !ss.panel_open;
                        }
                    }
                }

                // Right-aligned controls
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.filtered_indices.is_empty() {
                        let ann_count = self.collection.entries[self.ci(self.viewer.focused_index())]
                            .data.annotations.len();
                        let ann_label = if ann_count > 0 {
                            format!("✎ {ann_count}")
                        } else {
                            "✎".to_owned()
                        };
                        let btn = egui::Button::new(egui::RichText::new(ann_label).size(12.0))
                            .selected(self.app_state.show_annotations);
                        if ui.add(btn).on_hover_text("Toggle annotations (N)").clicked() {
                            self.app_state.show_annotations = !self.app_state.show_annotations;
                            if self.app_state.show_annotations {
                                self.annotation_focus_pending = true;
                            }
                        }
                    }
                    if matches!(&self.viewer, ViewerState::Single(_)) {
                        let btn = egui::Button::new(
                            egui::RichText::new("Hist").size(12.0),
                        )
                        .selected(self.app_state.show_histogram);
                        if ui.add(btn).on_hover_text("Toggle histogram (I)").clicked() {
                            self.app_state.show_histogram = !self.app_state.show_histogram;
                        }
                    }
                });
            });
        });

        // 8. Filter sidebar (must precede CentralPanel)
        self.render_filter_sidebar(ctx);

        // 8b. Annotations panel (floating window)
        self.render_annotations_panel(ctx);

        // 9. Main content
        let bg = self.bg_color();
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ctx.style().as_ref()).fill(bg))
            .show(ctx, |ui| {
            let viewer_clone = self.viewer.clone();
            match viewer_clone {
                ViewerState::Tiling(_) => self.render_tiling(ui, ctx),
                ViewerState::Single(_) => self.render_single(ui, ctx),
            }
        });

        // 9. Gallery selector overlay (rendered as a floating window on top)
        self.render_gallery_selector(ctx);

        // 9b. Find-similar parameter panel
        self.render_similar_panel(ctx);

        // 10. Toast overlay
        self.overlay.render_toast(ctx);
    }
}

// ── Filter sidebar helpers ──────────────────────────────────────────────────────

/// Rebuild an index-keyed accordion set after removing the entry at `removed`,
/// shifting all higher indices down by one.
fn accordion_after_remove(set: &HashSet<usize>, removed: usize) -> HashSet<usize> {
    set.iter()
        .filter(|&&j| j != removed)
        .map(|&j| if j > removed { j - 1 } else { j })
        .collect()
}

/// Compose effective filter stack: pre → photo → post.
fn compose_filters(photo: &[Filter], gallery: Option<&(Vec<Filter>, Vec<Filter>)>) -> Vec<Filter> {
    match gallery {
        None => photo.to_vec(),
        Some((pre, post)) => {
            let mut v = Vec::with_capacity(pre.len() + photo.len() + post.len());
            v.extend_from_slice(pre);
            v.extend_from_slice(photo);
            v.extend_from_slice(post);
            v
        }
    }
}

fn filter_kind_name(filter: &Filter) -> String {
    match filter {
        Filter::Rotate { .. }        => "Rotate".to_string(),
        Filter::FlipHorizontal       => "Flip H".to_string(),
        Filter::FlipVertical         => "Flip V".to_string(),
        Filter::Crop { .. }          => "Crop".to_string(),
        Filter::Scale { .. }         => "Scale".to_string(),
        Filter::Exposure { .. }      => "Exposure".to_string(),
        Filter::Contrast { .. }      => "Contrast".to_string(),
        Filter::CapSize { .. }       => "Cap Size".to_string(),
        Filter::Border { .. }        => "Border".to_string(),
        Filter::Sharpen { .. }       => "Sharpen".to_string(),
        Filter::MicroContrast { .. } => "Clarity".to_string(),
        Filter::Curves { .. }        => "Curves".to_string(),
        Filter::Preset { name }      => format!("Preset: {name}"),
    }
}

fn filter_has_params(filter: &Filter) -> bool {
    !matches!(filter, Filter::FlipHorizontal | Filter::FlipVertical | Filter::Preset { .. })
}

/// Default instances for the "Add filter" buttons, in display order.
fn filter_add_list() -> Vec<(&'static str, Filter)> {
    vec![
        ("Rotate",    Filter::Rotate { degrees: 0, center: None, fill: RotateFill::Transparent }),
        ("Flip H",    Filter::FlipHorizontal),
        ("Flip V",    Filter::FlipVertical),
        ("Crop",      Filter::Crop { x: 0.0, y: 0.0, width: 1.0, height: 1.0 }),
        ("Scale",     Filter::Scale { factor: 1.0 }),
        ("Exposure",  Filter::Exposure { stops: 0.0 }),
        ("Contrast",  Filter::Contrast { factor: 1.0 }),
        ("Sharpen",   Filter::Sharpen { amount: 1.0 }),
        ("Clarity",   Filter::MicroContrast { amount: 0.5 }),
        ("Curves",    Filter::Curves {
            r: vec![[0.0, 0.0], [1.0, 1.0]],
            g: vec![[0.0, 0.0], [1.0, 1.0]],
            b: vec![[0.0, 0.0], [1.0, 1.0]],
        }),
        ("Cap Size",  Filter::CapSize { max_px: 1024 }),
        ("Border",    Filter::Border { thickness: 20, color: [255, 255, 255, 255] }),
    ]
}

/// Render editable parameters for `filter`. Returns `true` if any value changed.
fn render_filter_params(ui: &mut egui::Ui, filter: &mut Filter, histogram: Option<&ImageHistogram>) -> bool {
    match filter {
        Filter::Rotate { degrees, center, fill } => {
            let mut changed = false;
            ui.horizontal(|ui| {
                for d in [0i32, 90, 180, 270] {
                    if ui.radio_value(degrees, d, format!("{d}°")).changed() {
                        changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                changed |= ui
                    .add(egui::DragValue::new(degrees).suffix("°"))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(degrees, -180..=180).show_value(false))
                    .changed();
            });
            // Fill mode (only meaningful for non-90° angles)
            ui.label("Fill:");
            ui.horizontal(|ui| {
                if ui.radio(matches!(fill, RotateFill::Transparent), "Transparent").clicked() {
                    *fill = RotateFill::Transparent;
                    changed = true;
                }
                if ui.radio(matches!(fill, RotateFill::Crop), "Crop").clicked() {
                    *fill = RotateFill::Crop;
                    changed = true;
                }
                if ui.radio(matches!(fill, RotateFill::Color(_)), "Color").clicked() {
                    if !matches!(fill, RotateFill::Color(_)) {
                        *fill = RotateFill::Color([0, 0, 0, 255]);
                        changed = true;
                    }
                }
            });
            if let RotateFill::Color(c) = fill {
                let mut color = egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
                if ui.color_edit_button_srgba(&mut color).changed() {
                    let [r, g, b, a] = color.to_array();
                    *c = [r, g, b, a];
                    changed = true;
                }
            }
            // Custom centre of rotation
            let mut has_center = center.is_some();
            if ui.checkbox(&mut has_center, "Custom center").changed() {
                *center = if has_center { Some([0.5, 0.5]) } else { None };
                changed = true;
            }
            if let Some([cx, cy]) = center {
                changed |= ui
                    .add(egui::Slider::new(cx, 0.0f32..=1.0).text("X").step_by(0.01))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(cy, 0.0f32..=1.0).text("Y").step_by(0.01))
                    .changed();
            }
            changed
        }
        Filter::FlipHorizontal | Filter::FlipVertical => false,
        Filter::Crop { x, y, width, height } => {
            let mut changed = false;
            changed |= ui
                .add(egui::Slider::new(x, 0.0f32..=0.95).text("left").step_by(0.01))
                .changed();
            changed |= ui
                .add(egui::Slider::new(y, 0.0f32..=0.95).text("top").step_by(0.01))
                .changed();
            changed |= ui
                .add(egui::Slider::new(width, 0.05f32..=1.0).text("width").step_by(0.01))
                .changed();
            changed |= ui
                .add(egui::Slider::new(height, 0.05f32..=1.0).text("height").step_by(0.01))
                .changed();
            changed
        }
        Filter::Scale { factor } => ui
            .add(egui::Slider::new(factor, 0.05f32..=2.0).text("×").step_by(0.05))
            .changed(),
        Filter::Exposure { stops } => ui
            .add(egui::Slider::new(stops, -4.0f32..=4.0).text("EV").step_by(0.1))
            .changed(),
        Filter::Contrast { factor } => ui
            .add(egui::Slider::new(factor, 0.1f32..=3.0).text("×").step_by(0.05))
            .changed(),
        Filter::CapSize { max_px } => {
            let mut changed = false;
            ui.horizontal(|ui| {
                for &preset in &[120u32, 600, 800, 1200] {
                    if ui.small_button(preset.to_string()).clicked() {
                        *max_px = preset;
                        changed = true;
                    }
                }
            });
            changed |= ui
                .add(egui::Slider::new(max_px, 64u32..=4096).text("px").logarithmic(true))
                .changed();
            changed
        }
        Filter::Border { thickness, color } => {
            let mut changed = false;
            changed |= ui
                .add(egui::Slider::new(thickness, 1u32..=500).text("px"))
                .changed();
            let mut c = egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3]);
            if egui::color_picker::color_edit_button_srgba(
                ui,
                &mut c,
                egui::color_picker::Alpha::OnlyBlend,
            )
            .changed()
            {
                *color = c.to_array();
                changed = true;
            }
            changed
        }
        Filter::Sharpen { amount } => ui
            .add(egui::Slider::new(amount, -1.0f32..=5.0).text("amount").step_by(0.05))
            .changed(),
        Filter::MicroContrast { amount } => ui
            .add(egui::Slider::new(amount, -1.0f32..=2.0).text("clarity").step_by(0.05))
            .changed(),
        Filter::Curves { r, g, b } => {
            let mut changed = false;
            ui.label(egui::RichText::new("R").color(egui::Color32::from_rgb(220, 80, 80)).small());
            let r_hist = histogram.map(|h| &h.r);
            let mut w = CurveEditor::new("curve_r", r, egui::Color32::from_rgb(220, 80, 80));
            if let Some(h) = r_hist { w = w.histogram(h); }
            changed |= ui.add(w).changed();

            ui.label(egui::RichText::new("G").color(egui::Color32::from_rgb(80, 180, 80)).small());
            let g_hist = histogram.map(|h| &h.g);
            let mut w = CurveEditor::new("curve_g", g, egui::Color32::from_rgb(80, 180, 80));
            if let Some(h) = g_hist { w = w.histogram(h); }
            changed |= ui.add(w).changed();

            ui.label(egui::RichText::new("B").color(egui::Color32::from_rgb(80, 120, 220)).small());
            let b_hist = histogram.map(|h| &h.b);
            let mut w = CurveEditor::new("curve_b", b, egui::Color32::from_rgb(80, 120, 220));
            if let Some(h) = b_hist { w = w.histogram(h); }
            changed |= ui.add(w).changed();
            changed
        }
        Filter::Preset { .. } => unreachable!("Preset must be expanded before render_filter_params"),
    }
}


fn render_histogram_overlay(ui: &egui::Ui, photo_rect: egui::Rect, hist: &ImageHistogram) {
    const HIST_W: f32 = 220.0;
    const HIST_H: f32 = 80.0;
    const PAD: f32 = 8.0;

    let rect = egui::Rect::from_min_max(
        egui::pos2(photo_rect.max.x - HIST_W - PAD, photo_rect.max.y - HIST_H - PAD),
        egui::pos2(photo_rect.max.x - PAD, photo_rect.max.y - PAD),
    );

    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(160));

    let max_val = [hist.r.iter(), hist.g.iter(), hist.b.iter()]
        .into_iter()
        .flatten()
        .copied()
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let max_log = (max_val + 1.0).ln();
    let bar_w = rect.width() / 256.0;

    for (channel, color) in [
        (&hist.r, egui::Color32::from_rgba_unmultiplied(255, 60, 60, 160)),
        (&hist.g, egui::Color32::from_rgba_unmultiplied(60, 220, 60, 140)),
        (&hist.b, egui::Color32::from_rgba_unmultiplied(60, 100, 255, 160)),
    ] {
        for (i, &count) in channel.iter().enumerate() {
            if count == 0 { continue; }
            let norm_h = ((count as f64 + 1.0).ln() / max_log) as f32;
            let bar_h = norm_h * rect.height();
            let x = rect.min.x + i as f32 * bar_w;
            let bar_rect = egui::Rect::from_min_max(
                egui::pos2(x, rect.max.y - bar_h),
                egui::pos2(x + bar_w, rect.max.y),
            );
            painter.rect_filled(bar_rect, 0.0, color);
        }
    }

    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(60)),
        egui::StrokeKind::Inside,
    );
}

/// Copy the filter-processed image to the system clipboard as image/png data.
fn copy_image_to_clipboard(path: &std::path::Path, filters: &[Filter]) -> Result<(), String> {
    let img = crate::image_cache::load_and_process(path, filters)?;
    let (w, h) = img.dimensions();
    let bytes = img.into_raw(); // RGBA, row-major
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(bytes),
    }).map_err(|e| e.to_string())
}

/// Apply the filter stack and save to a temp file. Returns the temp file path.
fn export_to_temp(path: &std::path::Path, filters: &[Filter]) -> Result<std::path::PathBuf, String> {
    let img = crate::image_cache::load_and_process(path, filters)?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("export");
    let tmp = std::env::temp_dir().join(format!("{stem}_galerie.png"));
    img.save(&tmp).map_err(|e| e.to_string())?;
    Ok(tmp)
}

fn l2_normalise(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < 1e-12 { return v.to_vec(); }
    v.iter().map(|x| x / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn sq_dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}
