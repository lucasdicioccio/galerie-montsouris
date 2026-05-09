mod actions;
mod app;
mod cluster;
mod config;
mod curve_editor;
mod embed;
mod filters;
mod gallery;
mod image_cache;
mod overlay;
mod viewer;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gallery::{EditGallery, GalleryEntry, GalleryFile, InputArg};

#[derive(Parser)]
#[command(name = "galerie-montsouris", about = "Personal photo gallery viewer and filter tool")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Directories to scan or .galerie files to load (viewer mode).
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Write discovered photo paths to this .galerie file, then launch the viewer.
    #[arg(long, value_name = "FILE")]
    save_as: Option<PathBuf>,

    /// Open a .galerie file for interactive editing (can be repeated for multiple galleries).
    /// The file is created empty if it doesn't exist yet.
    #[arg(long = "edit-gallery", value_name = "FILE", action = clap::ArgAction::Append)]
    edit_galleries: Vec<PathBuf>,

    /// Create a new .galerie file populated with every photo found in the given paths,
    /// then exit (no GUI). Fails if the output file already exists.
    #[arg(long, value_name = "FILE")]
    init_gallery: Option<PathBuf>,

    /// Enable debug logging to stderr.
    #[arg(long)]
    debug: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Apply a JSON filter spec to an image file and write the result.
    ///
    /// SPEC is a JSON file containing either a single Filter object or an array of Filter
    /// objects (same format as the `filters` field in .galerie.json sidecars).
    /// EXIF auto-rotation is applied before the spec filters, consistent with the viewer.
    ///
    /// Examples:
    ///   galerie-montsouris apply-filter rotate90.json photo.jpg out.png
    ///   galerie-montsouris apply-filter '[{"type":"Rotate","degrees":90}]' - -  # (file paths only)
    ApplyFilter {
        /// JSON filter spec file (single Filter or array of Filters).
        spec: PathBuf,
        /// Input image file.
        input: PathBuf,
        /// Output image file (format inferred from extension; .png recommended).
        output: PathBuf,
    },

    /// Compute embeddings for all photos in a .galerie file and store them as annotations.
    ///
    /// The command template receives the photo path via `%p` and must print to stdout
    /// either raw little-endian f32 bytes or a base64-encoded version of those bytes.
    ///
    /// Example (NumPy):
    ///   galerie-montsouris embed --namespace clip --command "my-embedder %p" gallery.galerie
    Embed {
        /// Embedding namespace (e.g. "clip", "custom-v1").
        #[arg(long)]
        namespace: String,
        /// Command template to run per photo. `%p` is replaced with the photo path(s).
        /// In single mode (default), `%p` is substituted anywhere in the string.
        /// In batch mode (--batch-size > 1), `%p` must be a standalone word and is expanded
        /// to all paths in the batch as separate arguments; the command must print one
        /// base64-encoded embedding per line, in the same order as the input paths.
        #[arg(long, value_name = "CMD")]
        command: String,
        /// Number of images to pass to the command per invocation (default: 1).
        /// Use with a command that can embed multiple images in one call (e.g. to amortise
        /// GPU/model loading costs). Output must be one base64 line per image.
        #[arg(long, default_value_t = 1, value_name = "N")]
        batch_size: usize,
        /// Re-embed photos that already have an embedding for this namespace.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Optional JSON filter spec file (single Filter object or array of Filters).
        /// Each image is pre-processed through this filter stack before being passed to
        /// the embedding command. Useful for e.g. `{"type":"CapSize","max_px":512}` to
        /// embed scaled-down thumbnails instead of full-resolution images.
        #[arg(long, value_name = "FILE")]
        filter_file: Option<PathBuf>,
        /// The .galerie file to process.
        galerie: PathBuf,
    },

    /// Cluster photos by embedding similarity and store cluster assignments as annotations.
    ///
    /// Requires that embeddings have already been computed with `embed` for the same namespace.
    ///
    /// Example:
    ///   galerie-montsouris cluster --namespace clip --clusters 8 gallery.galerie
    Cluster {
        /// Embedding namespace to read (must match the one used with `embed`).
        #[arg(long)]
        namespace: String,
        /// Number of clusters (k in k-means).
        #[arg(long = "clusters", value_name = "K")]
        k: usize,
        /// The .galerie file to process.
        galerie: PathBuf,
    },

    /// Export all photos as PNG with filters baked in, then exit (no GUI).
    ///
    /// Output files are named `{index:04}_{original-stem}.png`.
    /// EXIF metadata is not written to the exported files.
    ///
    /// Example:
    ///   galerie-montsouris export ./out gallery.galerie
    ///   galerie-montsouris export ./out ~/Pictures/2024/
    Export {
        /// Output directory (created if it does not exist).
        out_dir: PathBuf,
        /// Directories to scan or .galerie files to load.
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
}

fn classify_args(paths: &[PathBuf]) -> Vec<InputArg> {
    paths
        .iter()
        .map(|p| {
            if p.extension().and_then(|e| e.to_str()) == Some("galerie") {
                InputArg::GalleryFile(p.clone())
            } else {
                InputArg::Directory(p.clone())
            }
        })
        .collect()
}

fn run_apply_filter(spec: &Path, input: &Path, output: &Path) -> Result<()> {
    let text = std::fs::read_to_string(spec)
        .with_context(|| format!("reading filter spec {spec:?}"))?;

    // Accept either a JSON array of filters or a single filter object.
    let filter_stack: Vec<filters::Filter> =
        if let Ok(arr) = serde_json::from_str::<Vec<filters::Filter>>(&text) {
            arr
        } else {
            let f = serde_json::from_str::<filters::Filter>(&text)
                .with_context(|| format!("parsing {spec:?}: expected a Filter object or array"))?;
            vec![f]
        };

    let rgba = image_cache::load_and_process(input, &filter_stack)
        .map_err(|e| anyhow::anyhow!("processing {}: {e}", input.display()))?;

    rgba.save(output)
        .with_context(|| format!("saving to {output:?}"))?;

    Ok(())
}

fn run_export(collection: &gallery::PhotoCollection, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory {out_dir:?}"))?;

    let total = collection.len();
    let mut errors = 0usize;

    // Compose gallery pre/post filters once; they are the same for every photo.
    let (gal_pre, gal_post) = collection.galerie_filters().unwrap_or_default();

    for (i, entry) in collection.entries.iter().enumerate() {
        let stem = entry.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        let out_name = format!("{:04}_{stem}.png", i + 1);
        let out_path = out_dir.join(&out_name);

        eprint!("[{}/{}] {} … ", i + 1, total, entry.path.display());

        let effective: Vec<filters::Filter> = gal_pre.iter()
            .chain(entry.data.filters.iter())
            .chain(gal_post.iter())
            .cloned()
            .collect();

        match image_cache::load_and_process(&entry.path, &effective) {
            Ok(rgba) => match rgba.save(&out_path) {
                Ok(()) => eprintln!("ok"),
                Err(e) => { eprintln!("ERROR saving: {e}"); errors += 1; }
            },
            Err(e) => { eprintln!("ERROR decoding: {e}"); errors += 1; }
        }
    }

    if errors > 0 {
        anyhow::bail!("{errors}/{total} photos failed to export");
    }
    eprintln!("Exported {total} photos to {}", out_dir.display());
    Ok(())
}

fn run_init_gallery(dest: &Path, collection: &gallery::PhotoCollection) -> Result<()> {
    if dest.exists() {
        anyhow::bail!("output file already exists: {}", dest.display());
    }
    let gallery = GalleryFile {
        name: dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("gallery")
            .to_owned(),
        photos: collection.entries.iter().map(|e| GalleryEntry {
            path: e.path.clone(),
            hash: e.hash.clone(),
            filters: vec![],
            rating: None,
            annotations: vec![],
        }).collect(),
        background_color: None,
        pre_filters: vec![],
        post_filters: vec![],
        presets: vec![],
    };
    gallery::save_gallery_file(dest, &gallery)?;
    eprintln!("Initialized gallery {} ({} photos)", dest.display(), gallery.photos.len());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    env_logger::Builder::new()
        .filter_level(if cli.debug { log::LevelFilter::Debug } else { log::LevelFilter::Warn })
        .init();

    if let Some(Cmd::ApplyFilter { spec, input, output }) = &cli.command {
        return run_apply_filter(spec, input, output);
    }

    if let Some(Cmd::Embed { namespace, command, batch_size, force, filter_file, galerie }) = &cli.command {
        let filters = match filter_file {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading filter file {path:?}"))?;
                if let Ok(arr) = serde_json::from_str::<Vec<filters::Filter>>(&text) {
                    arr
                } else {
                    let f = serde_json::from_str::<filters::Filter>(&text)
                        .with_context(|| format!("parsing {path:?}: expected a Filter object or array"))?;
                    vec![f]
                }
            }
            None => vec![],
        };
        return embed::run_embed(embed::EmbedConfig {
            namespace: namespace.clone(),
            command_template: command.clone(),
            batch_size: *batch_size,
            force: *force,
            filters,
            galerie_path: galerie.clone(),
        });
    }

    if let Some(Cmd::Cluster { namespace, k, galerie }) = &cli.command {
        return cluster::run_cluster(cluster::ClusterConfig {
            namespace: namespace.clone(),
            k: *k,
            galerie_path: galerie.clone(),
        });
    }

    if let Some(Cmd::Export { out_dir, paths }) = &cli.command {
        let args = classify_args(paths);
        let collection = gallery::PhotoCollection::from_args(&args)?;
        return run_export(&collection, out_dir);
    }

    if cli.paths.is_empty() && cli.init_gallery.is_none() {
        // No subcommand and no paths: print help and exit.
        use std::io::Write;
        let mut cmd = <Cli as clap::CommandFactory>::command();
        cmd.print_help()?;
        writeln!(std::io::stderr())?;
        std::process::exit(1);
    }

    let config = config::Config::load()?;
    let args = classify_args(&cli.paths);
    let collection = gallery::PhotoCollection::from_args(&args)?;

    log::info!("Loaded {} photos", collection.len());

    if let Some(dest) = &cli.init_gallery {
        return run_init_gallery(dest, &collection);
    }

    if let Some(dest) = &cli.save_as {
        let gallery = GalleryFile {
            name: dest
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("gallery")
                .to_owned(),
            photos: collection.entries.iter().map(|e| GalleryEntry {
                path: e.path.clone(),
                hash: e.hash.clone(),
                filters: e.data.filters.clone(),
                rating: e.data.rating,
                annotations: e.data.annotations.clone(),
            }).collect(),
            background_color: None,
            pre_filters: vec![],
            post_filters: vec![],
            presets: vec![],
        };
        gallery::save_gallery_file(dest, &gallery)?;
        log::info!("Saved gallery to {dest:?} ({} photos)", gallery.photos.len());
    }

    let edit_galleries: Vec<EditGallery> = cli
        .edit_galleries
        .into_iter()
        .map(EditGallery::load_or_create)
        .collect::<Result<_>>()?;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("galerie-montsouris")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        "galerie-montsouris",
        native_options,
        Box::new(|cc| Ok(Box::new(app::GalerieApp::new(cc, config, collection, edit_galleries)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
