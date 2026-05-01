mod actions;
mod app;
mod config;
mod curve_editor;
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

    /// Export all photos as PNG with filters baked in, then exit (no GUI).
    /// Output files are named `{index:04}_{original-stem}.png`.
    /// EXIF metadata is not written to the exported files.
    #[arg(long, value_name = "DIR")]
    export_to: Option<PathBuf>,

    /// Create a new .galerie file populated with every photo found in the given paths,
    /// then exit (no GUI). Fails if the output file already exists.
    #[arg(long, value_name = "FILE")]
    init_gallery: Option<PathBuf>,
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
        }).collect(),
        background_color: None,
        pre_filters: vec![],
        post_filters: vec![],
    };
    gallery::save_gallery_file(dest, &gallery)?;
    eprintln!("Initialized gallery {} ({} photos)", dest.display(), gallery.photos.len());
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    if let Some(Cmd::ApplyFilter { spec, input, output }) = &cli.command {
        return run_apply_filter(spec, input, output);
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

    if let Some(out_dir) = &cli.export_to {
        return run_export(&collection, out_dir);
    }

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
                filters: vec![],
            }).collect(),
            background_color: None,
            pre_filters: vec![],
            post_filters: vec![],
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
