use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::filters::Filter;
use crate::gallery::{self, Annotation};

pub struct EmbedConfig {
    pub namespace: String,
    /// Command template — `%p` is replaced with the photo path(s).
    /// In single mode (batch_size = 1), `%p` is substituted anywhere in the template string.
    /// In batch mode (batch_size > 1), `%p` must appear as a standalone word and is
    /// expanded to all paths in the batch as separate arguments.
    /// The command must print to stdout:
    ///   - Single mode: raw little-endian f32 bytes, or a single base64-encoded line.
    ///   - Batch mode: one base64-encoded line per image, in the same order as the paths.
    pub command_template: String,
    /// Number of images to pass to the command per invocation.
    pub batch_size: usize,
    /// Re-embed photos that already have an embedding for this namespace.
    pub force: bool,
    /// Filter stack applied to each image before embedding. Empty = pass original path.
    pub filters: Vec<Filter>,
    pub galerie_path: PathBuf,
    /// Number of preprocessing threads (image decode + filter + temp PNG write).
    pub concurrency: usize,
    /// How many prepared batches to buffer ahead of the embedding command.
    pub prefetch_batches: usize,
}

struct PreparedBatch {
    indices: Vec<usize>,
    paths: Vec<PathBuf>,
}

pub fn run_embed(cfg: EmbedConfig) -> Result<()> {
    let EmbedConfig {
        namespace,
        command_template,
        batch_size,
        force,
        filters,
        galerie_path,
        concurrency,
        prefetch_batches,
    } = cfg;

    let mut gf = gallery::load_gallery_file(&galerie_path)?;
    let total = gf.photos.len();
    let batch_size = batch_size.max(1);
    let use_filters = !filters.is_empty();

    let mut pending: Vec<usize> = Vec::new();
    for (i, entry) in gf.photos.iter().enumerate() {
        let has_embedding = entry.annotations.iter().any(|a| {
            matches!(a, Annotation::Embedding { namespace: ns, .. } if ns == &namespace)
        });
        if !force && has_embedding {
            let label = entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            eprintln!("[{}/{total}] {label} … skip", i + 1);
        } else {
            pending.push(i);
        }
    }

    // Collect batch inputs (just PathBuf clones) before spawning the producer.
    let batches: Vec<(Vec<usize>, Vec<PathBuf>)> = pending
        .chunks(batch_size)
        .map(|chunk| {
            let indices = chunk.to_vec();
            let paths = chunk.iter().map(|&i| gf.photos[i].path.clone()).collect();
            (indices, paths)
        })
        .collect();

    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(concurrency)
            .build()
            .context("building preprocessing thread pool")?,
    );

    let tmp_dir: Option<Arc<tempfile::TempDir>> = if use_filters {
        Some(Arc::new(
            tempfile::tempdir().context("creating temp directory for pre-processed images")?,
        ))
    } else {
        None
    };

    let (tx, rx) = crossbeam_channel::bounded::<Result<PreparedBatch, String>>(prefetch_batches);

    // Producer: preprocesses batches and sends them into the bounded channel.
    let pool_prod = Arc::clone(&pool);
    let tmp_prod = tmp_dir.clone();
    std::thread::spawn(move || {
        for (batch_num, (indices, orig_paths)) in batches.into_iter().enumerate() {
            let result = if use_filters {
                let tmp = tmp_prod.as_ref().unwrap();
                preprocess_batch_parallel(&orig_paths, &filters, tmp.path(), &pool_prod, batch_num)
                    .map(|paths| PreparedBatch { indices, paths })
                    .map_err(|e| e.to_string())
            } else {
                Ok(PreparedBatch { indices, paths: orig_paths })
            };
            if tx.send(result).is_err() {
                break; // consumer dropped (e.g. early exit)
            }
        }
    });

    // Consumer: receives prepared batches and runs the embedding command.
    for result in rx {
        let batch = match result {
            Ok(b) => b,
            Err(e) => { eprintln!("error pre-processing batch: {e}"); continue; }
        };

        let chunk = &batch.indices;
        if batch_size == 1 {
            let label = gf.photos[chunk[0]].path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            eprint!("[{}/{total}] {label} … ", chunk[0] + 1);
        } else {
            let first = chunk[0] + 1;
            let last = chunk[chunk.len() - 1] + 1;
            eprint!("[{first}-{last}/{total}] batch of {} … ", chunk.len());
        }

        let paths: Vec<&PathBuf> = batch.paths.iter().collect();
        let results = match run_batch_command(&command_template, &paths) {
            Ok(v) => v,
            Err(e) => { eprintln!("error: {e}"); continue; }
        };

        if results.len() != chunk.len() {
            eprintln!("error: expected {} embeddings, got {}", chunk.len(), results.len());
            continue;
        }

        for (&idx, floats) in chunk.iter().zip(results.iter()) {
            let entry = &mut gf.photos[idx];
            entry.annotations.retain(|a| {
                !matches!(a, Annotation::Embedding { namespace: ns, .. } if ns == &namespace)
            });
            entry.annotations.push(Annotation::embedding(&namespace, floats));
        }

        gallery::save_gallery_file(&galerie_path, &gf)
            .with_context(|| format!("saving {galerie_path:?}"))?;

        if results.len() == 1 {
            eprintln!("ok ({}d)", results[0].len());
        } else {
            eprintln!("ok ({} embeddings, {}d each)", results.len(), results[0].len());
        }
    }

    Ok(())
}

/// Pre-process a batch of images through the filter stack in parallel, writing each to a temp PNG.
fn preprocess_batch_parallel(
    paths: &[PathBuf],
    filters: &[Filter],
    tmp_dir: &std::path::Path,
    pool: &rayon::ThreadPool,
    batch_num: usize,
) -> Result<Vec<PathBuf>> {
    pool.install(|| {
        paths
            .par_iter()
            .enumerate()
            .map(|(i, src)| -> Result<PathBuf> {
                let rgba = crate::image_cache::load_and_process(src, filters)
                    .map_err(|e| anyhow::anyhow!("processing {}: {e}", src.display()))?;
                let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("img");
                let out = tmp_dir.join(format!("{batch_num}_{i}_{stem}.png"));
                rgba.save(&out)
                    .with_context(|| format!("saving temp image {}", out.display()))?;
                Ok(out)
            })
            .collect::<Result<Vec<_>>>()
    })
}

/// Run the command for one or more images and return their float embeddings.
fn run_batch_command(template: &str, paths: &[&PathBuf]) -> Result<Vec<Vec<f32>>> {
    if paths.len() == 1 {
        let cmd_str = template.replace("%p", &paths[0].to_string_lossy());
        return run_command_capture_floats(&cmd_str).map(|f| vec![f]);
    }

    let (prog, args) = build_batch_args(template, paths);
    if prog.is_empty() {
        anyhow::bail!("empty command");
    }

    let output = Command::new(&prog)
        .args(&args)
        .output()
        .with_context(|| format!("running {prog:?}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("exit {}: {}", output.status, stderr.trim());
    }

    parse_batch_output(&output.stdout, paths.len())
}

/// Build (program, args) for a batch invocation.
fn build_batch_args(template: &str, paths: &[&PathBuf]) -> (String, Vec<String>) {
    let mut parts = shell_words(template);
    if parts.is_empty() {
        return (String::new(), vec![]);
    }
    let prog = parts.remove(0);
    let mut args = Vec::with_capacity(parts.len() + paths.len());
    let mut found = false;
    for part in parts {
        if part == "%p" {
            found = true;
            for path in paths {
                args.push(path.to_string_lossy().into_owned());
            }
        } else {
            args.push(part);
        }
    }
    if !found {
        for path in paths {
            args.push(path.to_string_lossy().into_owned());
        }
    }
    (prog, args)
}

fn parse_batch_output(stdout: &[u8], expected: usize) -> Result<Vec<Vec<f32>>> {
    use base64::Engine as _;
    let text = std::str::from_utf8(stdout).context("batch output is not valid UTF-8")?;
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if lines.len() != expected {
        anyhow::bail!(
            "expected {expected} output lines (one base64 embedding per image), got {}",
            lines.len()
        );
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(line)
                .with_context(|| format!("failed to base64-decode line {}", i + 1))?;
            bytes_to_floats(&bytes)
        })
        .collect()
}

fn run_command_capture_floats(cmd_str: &str) -> Result<Vec<f32>> {
    let mut parts = shell_words(cmd_str);
    if parts.is_empty() {
        anyhow::bail!("empty command");
    }
    let prog = parts.remove(0);
    let output = Command::new(&prog)
        .args(&parts)
        .output()
        .with_context(|| format!("running {prog:?}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("exit {}: {}", output.status, stderr.trim());
    }

    parse_floats_from_stdout(&output.stdout)
}

fn parse_floats_from_stdout(stdout: &[u8]) -> Result<Vec<f32>> {
    use base64::Engine as _;
    if let Ok(text) = std::str::from_utf8(stdout) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
                return bytes_to_floats(&bytes);
            }
        }
    }
    bytes_to_floats(stdout)
}

fn bytes_to_floats(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.is_empty() {
        anyhow::bail!("empty output");
    }
    if bytes.len() % 4 != 0 {
        anyhow::bail!("output length {} is not a multiple of 4", bytes.len());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn shell_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(|w| w.to_owned()).collect()
}
