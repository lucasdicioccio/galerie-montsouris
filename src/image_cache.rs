use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lru::LruCache;

use crate::filters::{self, Filter, RotateFill};

const WORKER_COUNT: usize = 4;

pub enum LoadState {
    NotRequested,
    Pending,
    Ready(egui::TextureHandle),
    Error(String),
}

/// Per-channel pixel histogram (0–255 buckets, Rec. 601 luma + R/G/B).
#[derive(Debug, Clone)]
pub struct ImageHistogram {
    pub luma: [u32; 256],
    pub r: [u32; 256],
    pub g: [u32; 256],
    pub b: [u32; 256],
}

struct CachedTexture {
    handle: egui::TextureHandle,
    max_size: Option<u32>, // None = full resolution
}

struct LoadRequest {
    index: usize,
    path: PathBuf,
    filters: Vec<Filter>,
    max_size: Option<u32>,
    ctx: egui::Context,
}

struct LoadResult {
    index: usize,
    max_size: Option<u32>,
    result: Result<egui::ColorImage, String>,
    histogram: Option<ImageHistogram>,
}

pub struct ImageCache {
    lru: LruCache<usize, CachedTexture>,
    pending: HashMap<usize, Option<u32>>, // index → max_size of in-flight request
    errors: HashMap<usize, String>,
    histograms: HashMap<usize, ImageHistogram>,
    req_tx: crossbeam_channel::Sender<LoadRequest>,
    res_rx: crossbeam_channel::Receiver<LoadResult>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

impl ImageCache {
    pub fn new(capacity: usize) -> Self {
        let cap = std::num::NonZeroUsize::new(capacity.max(1)).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::bounded::<LoadRequest>(64);
        let (res_tx, res_rx) = crossbeam_channel::unbounded::<LoadResult>();

        let workers = (0..WORKER_COUNT)
            .map(|_| {
                let req_rx = req_rx.clone();
                let res_tx = res_tx.clone();
                std::thread::spawn(move || worker_loop(req_rx, res_tx))
            })
            .collect();

        Self {
            lru: LruCache::new(cap),
            pending: HashMap::new(),
            errors: HashMap::new(),
            histograms: HashMap::new(),
            req_tx,
            res_rx,
            _workers: workers,
        }
    }

    /// Drain the result channel; upload new textures to GPU.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(res) = self.res_rx.try_recv() {
            self.pending.remove(&res.index);
            match res.result {
                Ok(color_image) => {
                    // Never replace a full-res entry with a thumbnail.
                    let should_store = match self.lru.peek(&res.index) {
                        None => true,
                        Some(cached) => is_better_quality(res.max_size, cached.max_size),
                    };
                    if should_store {
                        let name = format!("photo_{}", res.index);
                        let handle =
                            ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR);
                        self.lru.put(res.index, CachedTexture { handle, max_size: res.max_size });
                        self.errors.remove(&res.index);
                    }
                    if let Some(hist) = res.histogram {
                        self.histograms.insert(res.index, hist);
                    }
                }
                Err(e) => {
                    self.errors.insert(res.index, e);
                }
            }
        }
    }

    /// Drop any cached or pending state for `index` so the next `get_or_request`
    /// re-decodes from disk (call this after changing a photo's filter stack).
    pub fn invalidate(&mut self, index: usize) {
        self.lru.pop(&index);
        self.pending.remove(&index);
        self.errors.remove(&index);
        self.histograms.remove(&index);
    }

    /// Flush the entire cache (call this after changing gallery-level filters).
    pub fn invalidate_all(&mut self) {
        self.lru.clear();
        self.pending.clear();
        self.errors.clear();
        self.histograms.clear();
    }

    pub fn get_histogram(&self, index: usize) -> Option<&ImageHistogram> {
        self.histograms.get(&index)
    }

    /// Return the current load state, triggering a (re-)decode if needed.
    ///
    /// `max_size`: `None` = full resolution; `Some(n)` = decode no larger than n×n px.
    /// If a thumbnail is cached but full-res is requested, the thumbnail is returned
    /// immediately (as a preview) while a full-res request is queued.
    pub fn get_or_request(
        &mut self,
        index: usize,
        path: &Path,
        filters: &[Filter],
        max_size: Option<u32>,
        ctx: &egui::Context,
    ) -> LoadState {
        // Check cache: clone out before mutably borrowing self again.
        let cached = self.lru.get(&index).map(|c| (c.handle.clone(), c.max_size));
        if let Some((handle, cached_max)) = cached {
            if is_adequate_quality(cached_max, max_size) {
                return LoadState::Ready(handle);
            }
            // Thumbnail cached, full-res needed: show thumbnail while queuing upgrade.
            if !matches!(self.pending.get(&index), Some(None)) {
                let req = LoadRequest {
                    index,
                    path: path.to_path_buf(),
                    filters: filters.to_vec(),
                    max_size: None,
                    ctx: ctx.clone(),
                };
                if self.req_tx.try_send(req).is_ok() {
                    self.pending.insert(index, None);
                }
            }
            return LoadState::Ready(handle);
        }

        if let Some(err) = self.errors.get(&index) {
            return LoadState::Error(err.clone());
        }

        if let Some(&pending_max) = self.pending.get(&index) {
            if is_adequate_quality(pending_max, max_size) {
                return LoadState::Pending;
            }
            // Pending thumbnail but full-res needed: upgrade the in-flight request.
            let req = LoadRequest {
                index,
                path: path.to_path_buf(),
                filters: filters.to_vec(),
                max_size: None,
                ctx: ctx.clone(),
            };
            if self.req_tx.try_send(req).is_ok() {
                self.pending.insert(index, None);
            }
            return LoadState::Pending;
        }

        let req = LoadRequest {
            index,
            path: path.to_path_buf(),
            filters: filters.to_vec(),
            max_size,
            ctx: ctx.clone(),
        };
        if self.req_tx.try_send(req).is_ok() {
            self.pending.insert(index, max_size);
        }
        LoadState::NotRequested
    }
}

/// True when `incoming` is equal or higher quality than `existing`.
fn is_better_quality(incoming: Option<u32>, existing: Option<u32>) -> bool {
    match (incoming, existing) {
        (None, _) => true,       // full-res always wins
        (Some(_), None) => false, // thumbnail never replaces full-res
        (Some(i), Some(e)) => i >= e,
    }
}

/// True when `cached` quality satisfies the `requested` quality.
fn is_adequate_quality(cached: Option<u32>, requested: Option<u32>) -> bool {
    match (cached, requested) {
        (None, _) => true,       // full-res serves any request
        (Some(_), None) => false, // thumbnail doesn't serve full-res request
        (Some(c), Some(r)) => c >= r,
    }
}

fn worker_loop(
    req_rx: crossbeam_channel::Receiver<LoadRequest>,
    res_tx: crossbeam_channel::Sender<LoadResult>,
) {
    for req in req_rx {
        let (result, histogram) = decode_with_histogram(&req.path, &req.filters, req.max_size);
        let _ = res_tx.send(LoadResult { index: req.index, max_size: req.max_size, result, histogram });
        req.ctx.request_repaint();
    }
}

fn decode_with_histogram(
    path: &Path,
    user_filters: &[Filter],
    max_size: Option<u32>,
) -> (Result<egui::ColorImage, String>, Option<ImageHistogram>) {
    let mut rgba = match load_and_process(path, user_filters) {
        Ok(r) => r,
        Err(e) => return (Err(e), None),
    };

    let histogram = if max_size.is_none() {
        Some(compute_histogram(&rgba))
    } else {
        None
    };

    if let Some(max) = max_size {
        let (w, h) = rgba.dimensions();
        if w > max || h > max {
            rgba = image::DynamicImage::ImageRgba8(rgba)
                .resize(max, max, image::imageops::FilterType::Triangle)
                .into_rgba8();
        }
    }

    let (w, h) = rgba.dimensions();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    );
    (Ok(color_image), histogram)
}

fn compute_histogram(rgba: &image::RgbaImage) -> ImageHistogram {
    let mut hist = ImageHistogram { luma: [0; 256], r: [0; 256], g: [0; 256], b: [0; 256] };
    for pixel in rgba.pixels() {
        let luma = (0.299 * pixel[0] as f32
            + 0.587 * pixel[1] as f32
            + 0.114 * pixel[2] as f32)
            .round() as usize;
        hist.luma[luma.min(255)] += 1;
        hist.r[pixel[0] as usize] += 1;
        hist.g[pixel[1] as usize] += 1;
        hist.b[pixel[2] as usize] += 1;
    }
    hist
}

/// Load an image from disk, apply EXIF auto-rotation, then apply the user filter stack.
/// Returns the fully processed `RgbaImage` — use this for export or any non-GPU path.
pub fn load_and_process(path: &Path, user_filters: &[Filter]) -> Result<image::RgbaImage, String> {
    let img = image::open(path).map_err(|e| e.to_string())?;
    let mut rgba = img.into_rgba8();

    let exif_deg = filters::exif_rotation_degrees(path);
    if exif_deg != 0 {
        rgba = filters::rotate_rgba(rgba, exif_deg, &RotateFill::Transparent);
    }

    if !user_filters.is_empty() {
        rgba = filters::apply_all_filters(rgba, user_filters);
    }

    Ok(rgba)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_nonexistent_returns_error() {
        let result = load_and_process(Path::new("/nonexistent/image.jpg"), &[]);
        assert!(result.is_err());
    }
}
