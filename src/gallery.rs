use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::filters::Filter;

const SUPPORTED_EXT: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff"];

/// A named filter stack stored in a galerie file and referenceable from any photo stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterPreset {
    pub name: String,
    /// Physical filters only — no nested Preset variants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Annotation {
    Note { text: String },
    Embedding {
        namespace: String,
        /// Base64-encoded little-endian IEEE 754 f32 bytes.
        data: String,
    },
    ClusterAssignment {
        namespace: String,
        cluster_id: u32,
    },
    /// Catches unknown annotation kinds so future variants don't break loading.
    #[serde(other)]
    Unknown,
}

impl Annotation {
    pub fn embedding(namespace: &str, vec: &[f32]) -> Self {
        use base64::Engine as _;
        let bytes: Vec<u8> = vec.iter().flat_map(|v| v.to_le_bytes()).collect();
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Annotation::Embedding { namespace: namespace.to_owned(), data }
    }

    pub fn decode_embedding(&self, ns: &str) -> Option<Vec<f32>> {
        use base64::Engine as _;
        if let Annotation::Embedding { namespace, data } = self {
            if namespace != ns { return None; }
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
            if bytes.len() % 4 != 0 { return None; }
            Some(bytes.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect())
        } else {
            None
        }
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundColor {
    #[default]
    Black,
    Gray,
    White,
}

impl BackgroundColor {
    pub fn cycle(self) -> Self {
        match self {
            Self::Black => Self::Gray,
            Self::Gray  => Self::White,
            Self::White => Self::Black,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Gray  => "gray",
            Self::White => "white",
        }
    }
}
const HASH_READ_BYTES: usize = 131_072; // 128 KB

// ---------- public types ----------

#[derive(Debug, Clone)]
pub struct PhotoEntry {
    #[allow(dead_code)]
    pub index: usize,
    pub path:  PathBuf,
    /// 16-char lowercase hex (seahash of first 128 KB). Stable across renames.
    pub hash:  String,
    pub data:  PhotoData,
    /// Set when this photo was loaded from a `.galerie` file (for per-galerie filter storage).
    pub galerie_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhotoData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Annotation>,
}

/// One entry inside a `.galerie` named-gallery file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryEntry {
    pub path: PathBuf,
    /// Content hash (seahash of first 128 KB). Empty string if not yet computed (legacy).
    #[serde(default)]
    pub hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Annotation>,
}

/// On-disk format for a `.galerie` named-gallery file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryFile {
    pub name: String,
    #[serde(default)]
    pub photos: Vec<GalleryEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<BackgroundColor>,
    /// Filters applied to every photo *before* its own per-photo filters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_filters: Vec<Filter>,
    /// Filters applied to every photo *after* its own per-photo filters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_filters: Vec<Filter>,
    /// Named preset filter stacks defined in this galerie file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<FilterPreset>,
}

/// What a single CLI argument resolves to.
pub enum InputArg {
    Directory(PathBuf),
    GalleryFile(PathBuf),
}

/// A gallery file open for interactive editing. Membership is tracked in memory and flushed
/// on every toggle so the file is always current even if the app exits unexpectedly.
pub struct EditGallery {
    pub path: PathBuf,
    pub name: String,
    entries: Vec<GalleryEntry>,    // ordered, matches on-disk sequence
    member_paths: HashSet<PathBuf>, // canonical absolute paths for O(1) lookup
    /// Background color setting from the galerie file; preserved on save.
    pub background_color: Option<BackgroundColor>,
    /// Preset definitions stored in this galerie file.
    pub presets: Vec<FilterPreset>,
}

impl EditGallery {
    /// Load an existing `.galerie` file or start a new empty one if the path doesn't exist yet.
    pub fn load_or_create(path: PathBuf) -> Result<Self> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("gallery")
            .to_owned();
        let (entries, member_paths, background_color, presets) = if path.exists() {
            let gf = load_gallery_file(&path)?;
            let member_paths = gf.photos
                .iter()
                .filter_map(|e| e.path.canonicalize().ok())
                .collect();
            let entries = gf.photos.into_iter().map(|mut e| {
                if let Ok(p) = e.path.canonicalize() { e.path = p; }
                e
            }).collect();
            (entries, member_paths, gf.background_color, gf.presets)
        } else {
            (Vec::new(), HashSet::new(), None, Vec::new())
        };
        Ok(Self { path, name, entries, member_paths, background_color, presets })
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.member_paths.contains(path)
    }

    /// Toggle `path` in/out. Returns `true` if added, `false` if removed.
    /// `hash` is the content hash of the photo (stored so the file is rename-portable).
    pub fn toggle(&mut self, path: &Path, hash: &str) -> bool {
        if self.member_paths.remove(path) {
            self.entries.retain(|e| e.path != path);
            false
        } else {
            self.member_paths.insert(path.to_path_buf());
            self.entries.push(GalleryEntry {
                path: path.to_path_buf(),
                hash: hash.to_string(),
                filters: vec![],
                rating: None,
                annotations: vec![],
            });
            true
        }
    }

    /// Write the current member set (and presets) to the gallery file.
    /// Pre/post filters are read from disk first to avoid clobbering them
    /// (they are managed by PhotoCollection, not EditGallery).
    pub fn save(&self) -> Result<()> {
        let (pre_filters, post_filters) = if self.path.exists() {
            let gf = load_gallery_file(&self.path)?;
            (gf.pre_filters, gf.post_filters)
        } else {
            (vec![], vec![])
        };
        save_gallery_file(
            &self.path,
            &GalleryFile {
                name: self.name.clone(),
                photos: self.entries.clone(),
                background_color: self.background_color,
                pre_filters,
                post_filters,
                presets: self.presets.clone(),
            },
        )
    }
}

// ---------- PhotoCollection ----------

pub struct PhotoCollection {
    pub entries:   Vec<PhotoEntry>,
    /// In-memory state for loaded galerie files; updated and flushed on data changes.
    galerie_files: HashMap<PathBuf, GalleryFile>,
}

impl PhotoCollection {
    /// Convenience: scan a list of directories.
    #[allow(dead_code)]
    pub fn scan(dirs: &[PathBuf]) -> Result<Self> {
        let args: Vec<InputArg> = dirs.iter().map(|d| InputArg::Directory(d.clone())).collect();
        Self::from_args(&args)
    }

    /// Returns true if any galerie file is loaded (rating/annotation persistence is available).
    #[allow(dead_code)]
    pub fn has_galerie_source(&self) -> bool {
        !self.galerie_files.is_empty()
    }

    /// Load from a mixed list of directories and `.galerie` gallery files.
    pub fn from_args(args: &[InputArg]) -> Result<Self> {
        let mut col = Self {
            entries: Vec::new(),
            galerie_files: HashMap::new(),
        };
        for arg in args {
            match arg {
                InputArg::Directory(dir)    => col.add_directory(dir)?,
                InputArg::GalleryFile(path) => col.add_from_gallery_file(path)?,
            }
        }
        Ok(col)
    }

    fn add_directory(&mut self, dir: &PathBuf) -> Result<()> {
        let dir = dir.canonicalize().with_context(|| format!("resolving {dir:?}"))?;

        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading directory {dir:?}"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_supported_image(p))
            .collect();
        paths.sort();

        for path in paths {
            let hash = match hash_photo_file(&path) {
                Ok(h)  => h,
                Err(e) => { log::warn!("Failed to hash {path:?}: {e}"); continue; }
            };
            let index = self.entries.len();
            self.entries.push(PhotoEntry { index, path, hash, data: PhotoData::default(), galerie_source: None });
        }
        Ok(())
    }

    fn add_from_gallery_file(&mut self, path: &PathBuf) -> Result<()> {
        let mut gallery = load_gallery_file(path)?;
        let galerie_abs = path.canonicalize().unwrap_or_else(|_| path.clone());
        let mut needs_save = false;

        for ge in &mut gallery.photos {
            let photo_path = match ge.path.canonicalize() {
                Ok(p)  => p,
                Err(e) => { log::warn!("Gallery photo {:?} not accessible: {e}", ge.path); continue; }
            };
            if !photo_path.is_file() {
                log::warn!("Gallery photo {photo_path:?} is not a file, skipping");
                continue;
            }

            // Use stored hash if valid; otherwise compute it (legacy entry).
            let hash = if is_hash_key(&ge.hash) {
                ge.hash.clone()
            } else {
                match hash_photo_file(&photo_path) {
                    Ok(h)  => { ge.hash = h.clone(); needs_save = true; h }
                    Err(e) => { log::warn!("Failed to hash {photo_path:?}: {e}"); continue; }
                }
            };

            let data = PhotoData {
                rating: ge.rating,
                filters: ge.filters.clone(),
                annotations: ge.annotations.clone(),
            };

            let index = self.entries.len();
            self.entries.push(PhotoEntry {
                index,
                path: photo_path,
                hash,
                data,
                galerie_source: Some(galerie_abs.clone()),
            });
        }

        if needs_save {
            if let Err(e) = save_gallery_file(path, &gallery) {
                log::warn!("Could not save migrated galerie file {path:?}: {e}");
            }
        }

        self.galerie_files.insert(galerie_abs, gallery);
        Ok(())
    }

    /// Update in-memory data and persist to the galerie file if this photo has one.
    /// Photos loaded from a directory (no galerie source) are updated in memory only.
    pub fn update_data(&mut self, index: usize, data: PhotoData) -> Result<()> {
        self.entries[index].data = data.clone();
        let galerie_source = self.entries[index].galerie_source.clone();

        if let Some(ref galerie_path) = galerie_source {
            if let Some(gf) = self.galerie_files.get_mut(galerie_path) {
                let entry_path = self.entries[index].path.clone();
                let entry_hash = self.entries[index].hash.clone();
                for ge in &mut gf.photos {
                    let ge_canon = ge.path.canonicalize().unwrap_or_else(|_| ge.path.clone());
                    if ge_canon == entry_path || ge.hash == entry_hash {
                        ge.filters = data.filters.clone();
                        ge.rating = data.rating;
                        ge.annotations = data.annotations.clone();
                        break;
                    }
                }
                save_gallery_file(galerie_path, gf)?;
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Returns the background color from the loaded galerie file, if exactly one was loaded.
    pub fn galerie_background(&self) -> Option<BackgroundColor> {
        if self.galerie_files.len() == 1 {
            self.galerie_files.values().next().and_then(|gf| gf.background_color)
        } else {
            None
        }
    }

    /// Paths of all galerie files loaded by this collection.
    pub fn galerie_file_paths(&self) -> Vec<PathBuf> {
        self.galerie_files.keys().cloned().collect()
    }

    /// Update and persist the background color for the given galerie file.
    pub fn set_galerie_background(&mut self, galerie_path: &Path, color: BackgroundColor) -> Result<()> {
        if let Some(gf) = self.galerie_files.get_mut(galerie_path) {
            gf.background_color = Some(color);
            save_gallery_file(galerie_path, gf)?;
        }
        Ok(())
    }

    /// Returns the gallery-level pre/post filter stacks when exactly one galerie file is loaded.
    pub fn galerie_filters(&self) -> Option<(Vec<Filter>, Vec<Filter>)> {
        if self.galerie_files.len() == 1 {
            self.galerie_files.values().next().map(|gf| (gf.pre_filters.clone(), gf.post_filters.clone()))
        } else {
            None
        }
    }

    /// Returns the path of the single loaded galerie file, if exactly one is loaded.
    pub fn single_galerie_path(&self) -> Option<PathBuf> {
        if self.galerie_files.len() == 1 {
            self.galerie_files.keys().next().cloned()
        } else {
            None
        }
    }

    /// Update and persist the gallery-level pre/post filter stacks for the given galerie file.
    pub fn set_galerie_filters(
        &mut self,
        galerie_path: &Path,
        pre_filters: Vec<Filter>,
        post_filters: Vec<Filter>,
    ) -> Result<()> {
        if let Some(gf) = self.galerie_files.get_mut(galerie_path) {
            gf.pre_filters = pre_filters;
            gf.post_filters = post_filters;
            save_gallery_file(galerie_path, gf)?;
        }
        Ok(())
    }

    /// Presets for a specific galerie file (cloned list for editing).
    pub fn galerie_presets(&self, path: &Path) -> Vec<FilterPreset> {
        self.galerie_files.get(path)
            .map(|gf| gf.presets.clone())
            .unwrap_or_default()
    }

    /// All presets from all loaded galerie files.
    pub fn all_presets(&self) -> impl Iterator<Item = (&FilterPreset, &Path)> {
        self.galerie_files.iter()
            .flat_map(|(path, gf)| gf.presets.iter().map(move |p| (p, path.as_path())))
    }

    /// Update the preset list for the given galerie file and flush to disk.
    pub fn set_galerie_presets(
        &mut self,
        galerie_path: &Path,
        presets: Vec<FilterPreset>,
    ) -> Result<()> {
        if let Some(gf) = self.galerie_files.get_mut(galerie_path) {
            gf.presets = presets;
            save_gallery_file(galerie_path, gf)?;
        }
        Ok(())
    }
}

// ---------- hashing ----------

pub fn hash_photo_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {path:?} for hashing"))?;
    let mut buf = vec![0u8; HASH_READ_BYTES];
    let n = f.read(&mut buf)
        .with_context(|| format!("reading {path:?} for hashing"))?;
    Ok(format!("{:016x}", seahash::hash(&buf[..n])))
}

fn is_hash_key(s: &str) -> bool {
    s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------- gallery file I/O ----------

pub fn load_gallery_file(path: &Path) -> Result<GalleryFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading gallery file {path:?}"))?;

    // Try new format (photos as objects with path/hash/filters).
    if let Ok(gf) = serde_json::from_str::<GalleryFile>(&text) {
        return Ok(gf);
    }

    // Fall back to legacy format (photos as plain path strings).
    #[derive(Deserialize)]
    struct Legacy {
        name: String,
        #[serde(default)]
        photos: Vec<PathBuf>,
    }
    let legacy: Legacy = serde_json::from_str(&text)
        .with_context(|| format!("parsing gallery file {path:?}"))?;
    let photos = legacy.photos.into_iter().map(|p| {
        GalleryEntry { path: p, hash: String::new(), filters: vec![], rating: None, annotations: vec![] }
    }).collect();
    Ok(GalleryFile { name: legacy.name, photos, background_color: None, pre_filters: vec![], post_filters: vec![], presets: vec![] })
}

pub fn save_gallery_file(path: &Path, gallery: &GalleryFile) -> Result<()> {
    let text = serde_json::to_string_pretty(gallery)?;
    std::fs::write(path, text).with_context(|| format!("writing gallery file {path:?}"))
}

fn is_supported_image(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| SUPPORTED_EXT.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_temp_jpeg(dir: &Path, name: &str) {
        let minimal_jpeg: &[u8] = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
            0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
            0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
            0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
            0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02,
            0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10,
            0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00,
            0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
            0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42,
            0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16,
            0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55,
            0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73,
            0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
            0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
            0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6,
            0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
            0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08,
            0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFB, 0xD7, 0xFF, 0xD9,
        ];
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(minimal_jpeg).unwrap();
    }

    #[test]
    fn scan_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let col = PhotoCollection::scan(&[tmp.path().to_path_buf()]).unwrap();
        assert!(col.is_empty());
    }

    #[test]
    fn scan_finds_images() {
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        make_temp_jpeg(tmp.path(), "b.jpeg");
        std::fs::write(tmp.path().join("readme.txt"), "ignore").unwrap();
        let col = PhotoCollection::scan(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(col.len(), 2);
        assert_eq!(col.entries[0].index, 0);
        assert_eq!(col.entries[1].index, 1);
    }

    #[test]
    fn dir_scan_no_persistence() {
        // Without a galerie file, update_data is in-memory only.
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        let mut col = PhotoCollection::scan(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(col.entries[0].data.rating, None);
        col.update_data(0, PhotoData { rating: Some(4), ..Default::default() }).unwrap();
        assert_eq!(col.entries[0].data.rating, Some(4)); // in-memory OK
        let col2 = PhotoCollection::scan(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(col2.entries[0].data.rating, None); // not persisted
    }

    #[test]
    fn hash_photo_file_is_stable() {
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        let path = tmp.path().join("a.jpg");
        let h1 = hash_photo_file(&path).unwrap();
        let h2 = hash_photo_file(&path).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_differs_for_different_content() {
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        std::fs::write(tmp.path().join("b.jpg"), b"clearly different content").unwrap();
        let ha = hash_photo_file(&tmp.path().join("a.jpg")).unwrap();
        let hb = hash_photo_file(&tmp.path().join("b.jpg")).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn is_hash_key_classification() {
        assert!(is_hash_key("a1b2c3d4e5f60718"));
        assert!(!is_hash_key("photo.jpg"));
        assert!(!is_hash_key("a1b2c3d4e5f6071"));   // 15 chars
        assert!(!is_hash_key("a1b2c3d4e5f607189"));  // 17 chars
        assert!(!is_hash_key("g1b2c3d4e5f60718"));   // 'g' not hex
    }

    #[test]
    fn gallery_file_round_trip() {
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        make_temp_jpeg(tmp.path(), "b.jpg");
        let abs_a = tmp.path().join("a.jpg").canonicalize().unwrap();
        let abs_b = tmp.path().join("b.jpg").canonicalize().unwrap();
        let gal = GalleryFile {
            name: "test".to_owned(),
            photos: vec![
                GalleryEntry { path: abs_a, hash: "a1b2c3d4e5f60718".to_string(), filters: vec![], rating: Some(3), annotations: vec![] },
                GalleryEntry { path: abs_b, hash: "b2c3d4e5f6071829".to_string(), filters: vec![], rating: None, annotations: vec![] },
            ],
            background_color: None,
            pre_filters: vec![],
            post_filters: vec![],
            presets: vec![],
        };
        let gal_path = tmp.path().join("test.galerie");
        save_gallery_file(&gal_path, &gal).unwrap();
        let gal2 = load_gallery_file(&gal_path).unwrap();
        assert_eq!(gal2.name, "test");
        assert_eq!(gal2.photos.len(), 2);
        assert_eq!(gal2.photos[0].hash, "a1b2c3d4e5f60718");
        assert_eq!(gal2.photos[0].rating, Some(3));
    }

    #[test]
    fn gallery_file_legacy_migration() {
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        let abs_a = tmp.path().join("a.jpg").canonicalize().unwrap();
        // Write old-format galerie file (plain path strings)
        let legacy_json = format!(r#"{{"name":"legacy","photos":[{:?}]}}"#, abs_a.to_string_lossy());
        let gal_path = tmp.path().join("legacy.galerie");
        std::fs::write(&gal_path, legacy_json).unwrap();
        let gf = load_gallery_file(&gal_path).unwrap();
        assert_eq!(gf.name, "legacy");
        assert_eq!(gf.photos.len(), 1);
        assert_eq!(gf.photos[0].path, abs_a);
        assert!(gf.photos[0].hash.is_empty()); // hash not yet computed
    }

    #[test]
    fn galerie_rating_annotation_round_trip() {
        // Rating and annotations persist in the galerie file and survive reload.
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        let abs_a = tmp.path().join("a.jpg").canonicalize().unwrap();
        let hash_a = hash_photo_file(&abs_a).unwrap();
        let gal = GalleryFile {
            name: "g".to_owned(),
            photos: vec![GalleryEntry { path: abs_a, hash: hash_a, filters: vec![], rating: None, annotations: vec![] }],
            background_color: None,
            pre_filters: vec![],
            post_filters: vec![],
            presets: vec![],
        };
        let gal_path = tmp.path().join("g.galerie");
        save_gallery_file(&gal_path, &gal).unwrap();

        let mut col = PhotoCollection::from_args(&[InputArg::GalleryFile(gal_path.clone())]).unwrap();
        col.update_data(0, PhotoData {
            rating: Some(4),
            annotations: vec![Annotation::Note { text: "lovely".to_owned() }],
            ..Default::default()
        }).unwrap();

        let col2 = PhotoCollection::from_args(&[InputArg::GalleryFile(gal_path)]).unwrap();
        assert_eq!(col2.entries[0].data.rating, Some(4));
        assert_eq!(col2.entries[0].data.annotations.len(), 1);
        let Annotation::Note { text } = &col2.entries[0].data.annotations[0] else { panic!("expected Note") };
        assert_eq!(text, "lovely");
    }

    #[test]
    fn galerie_filters_stored_per_galerie() {
        use crate::filters::{Filter, RotateFill};
        let tmp = TempDir::new().unwrap();
        make_temp_jpeg(tmp.path(), "a.jpg");
        let abs_a = tmp.path().join("a.jpg").canonicalize().unwrap();
        let hash_a = hash_photo_file(&abs_a).unwrap();

        // Create galerie file with a filter already set
        let filters = vec![Filter::Rotate { degrees: 90, center: None, fill: RotateFill::Transparent }];
        let gal = GalleryFile {
            name: "g".to_owned(),
            photos: vec![GalleryEntry { path: abs_a.clone(), hash: hash_a.clone(), filters: filters.clone(), rating: None, annotations: vec![] }],
            background_color: None,
            pre_filters: vec![],
            post_filters: vec![],
            presets: vec![],
        };
        let gal_path = tmp.path().join("g.galerie");
        save_gallery_file(&gal_path, &gal).unwrap();

        // Load and verify filters come from galerie file
        let col = PhotoCollection::from_args(&[InputArg::GalleryFile(gal_path.clone())]).unwrap();
        assert_eq!(col.entries[0].data.filters, filters);
        assert!(col.entries[0].galerie_source.is_some());

        // Apply new filter via update_data → should persist to galerie file
        let new_filters = vec![Filter::Rotate { degrees: 180, center: None, fill: RotateFill::Transparent }];
        let mut col = PhotoCollection::from_args(&[InputArg::GalleryFile(gal_path.clone())]).unwrap();
        col.update_data(0, PhotoData { filters: new_filters.clone(), ..Default::default() }).unwrap();

        // Reload and confirm new filters are in the galerie file
        let col2 = PhotoCollection::from_args(&[InputArg::GalleryFile(gal_path)]).unwrap();
        assert_eq!(col2.entries[0].data.filters, new_filters);
    }
}
