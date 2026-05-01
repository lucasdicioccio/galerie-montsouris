use crate::gallery::PhotoEntry;

const MAX_COLS: usize = 7; // 49 tiles max

#[derive(Debug, Clone)]
pub enum ViewerState {
    Tiling(TilingState),
    Single(SingleState),
}

#[derive(Debug, Clone)]
pub struct TilingState {
    pub page: usize,
    pub cols: usize,     // grid dimension; tile_count = cols²
    pub selected: usize, // which tile within the current page is focused (0-based)
}

#[derive(Debug, Clone)]
pub struct SingleState {
    pub current_index: usize,
    /// Zoom multiplier relative to fit-to-screen (1.0 = fit, 2.0 = 2× bigger, …).
    pub zoom: f32,
    /// Pan offset in logical pixels from the viewport centre.
    pub pan: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy)]
pub enum ModeTarget {
    Tiling,
    Single,
    Toggle,
}

impl TilingState {
    pub fn new(tile_count: usize) -> Self {
        let cols = (tile_count as f64).sqrt().ceil() as usize;
        Self { page: 0, cols: cols.max(1), selected: 0 }
    }

    pub fn tile_count(&self) -> usize {
        self.cols * self.cols
    }

    /// Absolute photo index of the focused (selected) tile.
    pub fn focused_abs(&self) -> usize {
        self.page * self.tile_count() + self.selected
    }

    pub fn page_range(&self, total: usize) -> std::ops::Range<usize> {
        let start = self.page * self.tile_count();
        let end = (start + self.tile_count()).min(total);
        start..end
    }

    #[allow(dead_code)]
    pub fn page_entries<'a>(&self, entries: &'a [PhotoEntry]) -> &'a [PhotoEntry] {
        &entries[self.page_range(entries.len())]
    }

    #[allow(dead_code)]
    pub fn total_pages(&self, total: usize) -> usize {
        if self.tile_count() == 0 { return 0; }
        total.div_ceil(self.tile_count())
    }

    /// Move selection by `count` photos in `dir`. Wraps around the whole collection.
    pub fn move_by(&mut self, dir: Direction, count: usize, total: usize) {
        if total == 0 { return; }
        let tc = self.tile_count();
        let current = self.focused_abs().min(total - 1);
        let new_abs = match dir {
            Direction::Next => (current + count) % total,
            Direction::Prev => {
                // subtract with wrapping
                let c = count % total;
                if current < c { total - (c - current) } else { current - c }
            }
        };
        self.page = new_abs / tc;
        self.selected = new_abs % tc;
    }

    /// Clamp `selected` so it refers to an existing tile (used after zoom or page changes).
    #[allow(dead_code)]
    pub fn clamp_selected(&mut self, total: usize) {
        if total == 0 { self.selected = 0; return; }
        let page_start = self.page * self.tile_count();
        let tiles_on_page = self.tile_count().min(total.saturating_sub(page_start));
        if tiles_on_page == 0 {
            self.page = 0;
            self.selected = 0;
        } else {
            self.selected = self.selected.min(tiles_on_page - 1);
        }
    }
}

impl ViewerState {
    pub fn new_tiling(tile_count: usize) -> Self {
        Self::Tiling(TilingState::new(tile_count))
    }

    /// The absolute photo index that should be acted on (rating, script, filter, etc.).
    pub fn focused_index(&self) -> usize {
        match self {
            ViewerState::Tiling(s) => s.focused_abs(),
            ViewerState::Single(s) => s.current_index,
        }
    }

    /// Indices to prefetch this frame.
    pub fn visible_indices(&self, total: usize) -> Vec<usize> {
        if total == 0 { return vec![]; }
        match self {
            ViewerState::Tiling(s) => s.page_range(total).collect(),
            ViewerState::Single(s) => {
                let idx = s.current_index;
                let mut v = vec![idx, idx.saturating_sub(1), (idx + 1).min(total - 1)];
                v.dedup();
                v
            }
        }
    }

    pub fn navigate(&mut self, dir: Direction, count: usize, total: usize) {
        if total == 0 { return; }
        match self {
            ViewerState::Single(s) => {
                let prev = s.current_index;
                let c = count % total;
                s.current_index = match dir {
                    Direction::Next => (s.current_index + c) % total,
                    Direction::Prev => {
                        if s.current_index < c { total - (c - s.current_index) }
                        else { s.current_index - c }
                    }
                };
                if s.current_index != prev {
                    s.zoom = 1.0;
                    s.pan = [0.0, 0.0];
                }
            }
            ViewerState::Tiling(s) => s.move_by(dir, count, total),
        }
    }

    pub fn zoom_tiling(&mut self, delta: i32, total: usize) {
        if let ViewerState::Tiling(s) = self {
            let focused = s.focused_abs().min(total.saturating_sub(1));
            // delta > 0 → zoom in (fewer tiles, smaller cols)
            // delta < 0 → zoom out (more tiles, larger cols)
            let new_cols = (s.cols as i32 - delta).clamp(1, MAX_COLS as i32) as usize;
            let new_tc = new_cols * new_cols;
            s.cols = new_cols;
            s.page = focused / new_tc;
            s.selected = focused % new_tc;
        }
        // zooming is a no-op in single mode
    }

    pub fn switch_to_single(&mut self, index: usize) {
        *self = ViewerState::Single(SingleState { current_index: index, zoom: 1.0, pan: [0.0, 0.0] });
    }

    /// Adjust single-mode zoom multiplicatively. `delta` > 0 = zoom in, < 0 = zoom out.
    pub fn zoom_single(&mut self, delta: f32) {
        if let ViewerState::Single(s) = self {
            s.zoom = (s.zoom * 1.25_f32.powf(delta)).clamp(0.1, 20.0);
        }
    }

    /// Reset single-mode zoom to fit-to-screen.
    pub fn reset_single_zoom(&mut self) {
        if let ViewerState::Single(s) = self {
            s.zoom = 1.0;
            s.pan = [0.0, 0.0];
        }
    }

    pub fn switch_to_tiling(&mut self, tile_count: usize) {
        let focused = self.focused_index();
        let mut s = TilingState::new(tile_count);
        let tc = s.tile_count();
        s.page = focused / tc;
        s.selected = focused % tc;
        *self = ViewerState::Tiling(s);
    }

    pub fn toggle(&mut self, tile_count: usize) {
        let focused = self.focused_index();
        match self {
            ViewerState::Tiling(_) => self.switch_to_single(focused),
            ViewerState::Single(_) => self.switch_to_tiling(tile_count),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_entries(n: usize) -> Vec<PhotoEntry> {
        (0..n)
            .map(|i| PhotoEntry {
                index: i,
                path: format!("/tmp/photo{i}.jpg").into(),
                hash: format!("{:016x}", i),
                data: Default::default(),
                galerie_source: None,
            })
            .collect()
    }

    #[test]
    fn single_navigate_wraps() {
        let mut v = ViewerState::Single(SingleState { current_index: 0, zoom: 1.0, pan: [0.0, 0.0] });
        v.navigate(Direction::Prev, 1, 3);
        assert_eq!(v.focused_index(), 2);
        v.navigate(Direction::Next, 1, 3);
        assert_eq!(v.focused_index(), 0);
    }

    #[test]
    fn single_navigate_by_count() {
        let mut v = ViewerState::Single(SingleState { current_index: 0, zoom: 1.0, pan: [0.0, 0.0] });
        v.navigate(Direction::Next, 10, 100);
        assert_eq!(v.focused_index(), 10);
        v.navigate(Direction::Prev, 3, 100);
        assert_eq!(v.focused_index(), 7);
    }

    #[test]
    fn tiling_page_entries() {
        let entries = dummy_entries(10);
        let s = TilingState::new(4);
        let page0 = s.page_entries(&entries);
        assert_eq!(page0.len(), 4);
        assert_eq!(page0[0].index, 0);
    }

    #[test]
    fn tiling_total_pages() {
        let s = TilingState::new(4);
        assert_eq!(s.total_pages(10), 3);
        assert_eq!(s.total_pages(8), 2);
        assert_eq!(s.total_pages(0), 0);
    }

    #[test]
    fn tiling_navigate_within_page() {
        // cols=3 → 9 tiles/page. Navigate within first page.
        let mut v = ViewerState::new_tiling(9);
        v.navigate(Direction::Next, 1, 20);
        assert_eq!(v.focused_index(), 1);
        v.navigate(Direction::Next, 1, 20);
        assert_eq!(v.focused_index(), 2);
    }

    #[test]
    fn tiling_navigate_crosses_page_boundary() {
        // cols=3 → 9 tiles/page, 20 total. Start at last tile of page 0 (index 8).
        let mut v = ViewerState::Tiling(TilingState { page: 0, cols: 3, selected: 8 });
        v.navigate(Direction::Next, 1, 20);
        // Should be on page 1, selected 0 (absolute index 9)
        assert_eq!(v.focused_index(), 9);
        if let ViewerState::Tiling(s) = &v {
            assert_eq!(s.page, 1);
            assert_eq!(s.selected, 0);
        }
    }

    #[test]
    fn tiling_navigate_by_10() {
        let mut v = ViewerState::new_tiling(9); // cols=3, 9 tiles/page
        v.navigate(Direction::Next, 10, 100);
        assert_eq!(v.focused_index(), 10);
        if let ViewerState::Tiling(s) = &v {
            assert_eq!(s.page, 1); // page 1 (indices 9-17)
            assert_eq!(s.selected, 1); // tile 1 on that page
        }
    }

    #[test]
    fn tiling_navigate_wraps_collection() {
        let mut v = ViewerState::Tiling(TilingState { page: 0, cols: 3, selected: 0 });
        v.navigate(Direction::Prev, 1, 10);
        assert_eq!(v.focused_index(), 9); // wraps to last photo
    }

    #[test]
    fn zoom_in_decreases_cols_keeps_focus() {
        let mut v = ViewerState::Tiling(TilingState { page: 0, cols: 3, selected: 5 });
        // focused = 5, cols=3 → tile_count=9
        v.zoom_tiling(1, 50); // zoom in → cols becomes 2, tile_count=4
        // 5 / 4 = page 1, 5 % 4 = selected 1
        assert_eq!(v.focused_index(), 5);
        if let ViewerState::Tiling(s) = &v {
            assert_eq!(s.cols, 2);
            assert_eq!(s.page, 1);
            assert_eq!(s.selected, 1);
        }
    }

    #[test]
    fn zoom_out_increases_cols_keeps_focus() {
        let mut v = ViewerState::Tiling(TilingState { page: 1, cols: 2, selected: 1 });
        // focused = 4*1 + 1 = 5, cols=2 → tile_count=4
        v.zoom_tiling(-1, 50); // zoom out → cols becomes 3, tile_count=9
        // 5 / 9 = page 0, 5 % 9 = selected 5
        assert_eq!(v.focused_index(), 5);
        if let ViewerState::Tiling(s) = &v {
            assert_eq!(s.cols, 3);
            assert_eq!(s.page, 0);
            assert_eq!(s.selected, 5);
        }
    }

    #[test]
    fn zoom_clamped_at_min() {
        let mut v = ViewerState::Tiling(TilingState { page: 0, cols: 1, selected: 0 });
        v.zoom_tiling(1, 10); // already at minimum
        if let ViewerState::Tiling(s) = &v { assert_eq!(s.cols, 1); }
    }

    #[test]
    fn zoom_clamped_at_max() {
        let mut v = ViewerState::Tiling(TilingState { page: 0, cols: MAX_COLS, selected: 0 });
        v.zoom_tiling(-1, 100); // already at maximum
        if let ViewerState::Tiling(s) = &v { assert_eq!(s.cols, MAX_COLS); }
    }

    #[test]
    fn visible_indices_single() {
        let v = ViewerState::Single(SingleState { current_index: 5, zoom: 1.0, pan: [0.0, 0.0] });
        let vis = v.visible_indices(10);
        assert!(vis.contains(&4));
        assert!(vis.contains(&5));
        assert!(vis.contains(&6));
    }

    #[test]
    fn toggle_switches_mode() {
        let mut v = ViewerState::new_tiling(9);
        v.toggle(9);
        assert!(matches!(v, ViewerState::Single(_)));
        v.toggle(9);
        assert!(matches!(v, ViewerState::Tiling(_)));
    }
}
