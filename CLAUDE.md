# galerie-montsouris

Personal photo gallery desktop app. Rust + egui/eframe (immediate-mode GUI, wgpu backend).

```
galerie-montsouris <dir1> <dir2> ...
```

## Build & test

```bash
cargo build              # debug
cargo build --release    # optimised
cargo test               # 33 unit tests, all pure logic (no GPU needed)
DISPLAY=:1 cargo run -- ~/Pictures/
```

## Module map

| File | Responsibility |
|------|----------------|
| `main.rs` | CLI (`clap`), config load, gallery scan, `eframe::run_native` |
| `config.rs` | `Config` (TOML), `KeyBindingMap`, key-name → `egui::Key` table |
| `gallery.rs` | `PhotoCollection` (flat dir scan), `PhotoEntry`, `PhotoData`, `.galerie.json` sidecar I/O |
| `filters.rs` | `Filter` enum, `apply_to_stack`, EXIF rotation reader, `rotate_rgba` |
| `viewer.rs` | `ViewerState { Tiling(TilingState), Single(SingleState) }`, navigation, zoom |
| `image_cache.rs` | LRU `TextureHandle` cache, 4-worker thread pool, `decode_image` (EXIF + filters) |
| `overlay.rs` | Toast, rating badge, filename strip, `fit_rect` |
| `actions.rs` | `Action` enum, `execute_action` dispatcher, `AppState`, script runner thread |
| `app.rs` | `GalerieApp` (`eframe::App`), render loop, tiling/single renderers |

## Viewer modes

**Tiling** (`TilingState`): `cols` is the primary control (`tile_count = cols²`). `selected` is the focused tile index within the current page (0-based). Absolute photo index = `page * tile_count + selected`.

**Single** (`SingleState`): `current_index` is the absolute photo index.

`ViewerState::navigate(dir, count, total)` works on absolute photo index in both modes — in tiling, it moves the selection and crosses page boundaries naturally. No special-casing at boundaries.

`ViewerState::zoom_tiling(delta, total)`: `delta > 0` = zoom in (fewer tiles, smaller `cols`); `delta < 0` = zoom out. Clamps `cols` to `[1, 7]`. Preserves the focused photo across zoom by recomputing `page`/`selected` from the saved absolute index.

## Image loading pipeline

```
disk → image::open → into_rgba8
     → exif auto-rotation (EXIF tag, transparent)
     → user filter stack (net rotation degrees)
     → egui::ColorImage
     → ctx.load_texture → egui::TextureHandle  (stored in LRU cache)
```

4 worker threads decode in parallel via `crossbeam-channel`. Workers call `ctx.request_repaint()` on completion to wake the render loop. `ImageCache::invalidate(idx)` drops the cached texture and clears pending state — call it whenever a photo's filter stack changes.

## Filter system (`filters.rs`)

`Filter` is a `#[serde(tag = "type")]` enum stored in `.galerie.json`:

```json
{ "type": "Rotate", "degrees": 90 }
```

`apply_to_stack(stack, incoming)` merges with the **last** element only if it's the same kind (adjacent merge). Non-adjacent same-kind filters coexist, enabling pipelines like `[Crop, Rotate, Crop]`. Identity results (e.g. 360° rotation) remove the entry.

To add a new filter type:
1. Add a variant to `Filter` in `filters.rs` with `#[serde(tag = "type")]`.
2. Add an arm to `filters::net_*` helper or a new apply function as needed.
3. Add a new `"FilterName"` arm to `Action::from_binding` in `actions.rs`.
4. Handle it in `execute_action` → `handle_apply_filter` (the merge logic in `apply_to_stack` handles the rest).

## Action system (`actions.rs`)

`Action` enum is the single extension point for new behaviours. To add an action:

1. Add variant to `Action`.
2. Add a parsing arm in `Action::from_binding` (maps TOML `args` to the variant).
3. Add a dispatch arm in `execute_action`.
4. Wire a key binding in `~/.config/galerie-montsouris/config.toml` or the default list in `config.rs`.

`ActionContext` carries mutable borrows of everything actions need. Adding a new action never requires changing the context structure unless the action needs access to something genuinely new.

## Configuration (`~/.config/galerie-montsouris/config.toml`)

```toml
[general]
tile_count              = 9      # initial cols = ceil(sqrt(tile_count))
cache_size              = 50     # max cached textures (each ~8 MB for 24 MP)
slideshow_interval_secs = 5.0

[[keybindings]]
key       = "ArrowRight"         # egui::Key variant name
modifiers = []                   # ["ctrl"], ["shift"], ["alt"], or []
action    = "Navigate"
args      = { direction = "next", count = 1 }
```

Key names are `egui::Key` variant names — see `parse_key()` in `config.rs` for the full table. Supported actions and their `args` shapes:

| `action` | `args` |
|----------|--------|
| `Navigate` | `{ direction = "next"\|"prev", count = 1 }` |
| `SwitchMode` | `{ mode = "tiling"\|"single"\|"toggle" }` |
| `ZoomTiling` | `{ delta = 1 }` (positive = zoom in) |
| `ToggleSlideshow` | `{}` |
| `Quit` | `{}` |
| `CycleRating` | `{ values = [1,2,3,4,5] }` |
| `ApplyFilter` | `{ filter = "RotateLeft"\|"RotateRight"\|"Rotate180" }` — 90°/270°/180° shortcuts |
| `ApplyFilter` | `{ filter = "Rotate", degrees = N }` — rotate by N degrees (positive or negative, merges adjacently) |
| `ApplyFilter` | `{ filter = "CapSize", max_px = 1024 }` — shrink longest dim to ≤ max_px |
| `ApplyFilter` | `{ filter = "Border", thickness = 10, color = [255,255,255,255] }` — RGBA border |
| `CycleBackground` | `{}` — cycle viewer background black → gray → white; persisted to galerie file if one is loaded |
| `ToggleHistogram` | `{}` — toggle per-channel RGB histogram overlay in single-photo mode |
| `RunScript` | `{ path = "~/script.sh", args = ["%p"], pass_filters_stdin = false }` |

## Sidecar data (`.galerie.json`)

One file per scanned directory. Format:

```json
{
  "photo.jpg": { "rating": 3, "filters": [{ "type": "Rotate", "degrees": 90 }] }
}
```

Both `rating` and `filters` are omitted when null/empty (backwards-compatible). `PhotoCollection::update_data(idx, data)` rewrites the whole sidecar for that photo's directory atomically on every mutation.

## Default key bindings

| Key | Action |
|-----|--------|
| `←` / `→` or `H` / `L` | Navigate ±1 (crosses page in tiling) |
| `PgUp` / `PgDn` | Navigate ±10 |
| `+` / `-` | Zoom in / out (tiling grid) |
| `T` | Toggle tiling ↔ single |
| `Escape` | Switch to tiling |
| `[` / `]` | Rotate photo left / right (persisted) |
| `R` | Cycle rating (1→2→3→4→5→unrated) |
| `B` | Cycle background black → gray → white (persisted in galerie file) |
| `I` | Toggle RGB histogram overlay (single-photo mode) |
| `S` | Toggle slideshow |
| `Q` | Quit |
| click tile | Open in single mode |

## Key invariants

- `TilingState.selected` is always `< min(tile_count, tiles_on_last_page)` — clamp after any operation that could violate this.
- `ImageCache::invalidate` must be called whenever `PhotoData.filters` changes.
- `handle_cycle_rating` and `handle_apply_filter` both clone the full `PhotoData` and mutate one field — never construct `PhotoData { .. }` directly (would silently zero other fields).
- EXIF rotation is applied transparently before user filters in `decode_image`; it is **not** stored in the sidecar.
- The script runner thread is a single long-lived thread; `AppState::script_running` prevents concurrent invocations of the same binding.
