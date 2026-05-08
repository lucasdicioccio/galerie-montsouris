use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::gallery::{self, Annotation};

pub struct EmbedConfig {
    pub namespace: String,
    /// Command template — `%p` is replaced with the photo path.
    /// The command must print to stdout either raw little-endian f32 bytes or a
    /// base64-encoded single line of those bytes.
    pub command_template: String,
    /// Re-embed photos that already have an embedding for this namespace.
    pub force: bool,
    pub galerie_path: PathBuf,
}

pub fn run_embed(cfg: EmbedConfig) -> Result<()> {
    let mut gf = gallery::load_gallery_file(&cfg.galerie_path)?;
    let total = gf.photos.len();

    for (i, entry) in gf.photos.iter_mut().enumerate() {
        let label = entry.path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");

        eprint!("[{}/{total}] {label} … ", i + 1);

        if !cfg.force && entry.annotations.iter().any(|a| matches!(a,
            Annotation::Embedding { namespace, .. } if namespace == &cfg.namespace))
        {
            eprintln!("skip");
            continue;
        }

        let cmd_str = cfg.command_template.replace("%p", &entry.path.to_string_lossy());
        let floats = match run_command_capture_floats(&cmd_str) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                continue;
            }
        };

        entry.annotations.retain(|a| !matches!(a,
            Annotation::Embedding { namespace, .. } if namespace == &cfg.namespace));
        entry.annotations.push(Annotation::embedding(&cfg.namespace, &floats));
        eprintln!("ok ({}d)", floats.len());
    }

    gallery::save_gallery_file(&cfg.galerie_path, &gf)
        .with_context(|| format!("saving {:?}", cfg.galerie_path))?;

    Ok(())
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
    // Try base64: valid UTF-8, trim whitespace, decode
    if let Ok(text) = std::str::from_utf8(stdout) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
                return bytes_to_floats(&bytes);
            }
        }
    }
    // Fall back to raw binary
    bytes_to_floats(stdout)
}

fn bytes_to_floats(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.is_empty() {
        anyhow::bail!("empty output");
    }
    if bytes.len() % 4 != 0 {
        anyhow::bail!("output length {} is not a multiple of 4", bytes.len());
    }
    Ok(bytes.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Minimal whitespace-based word splitter (no quoting support).
fn shell_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(|w| w.to_owned()).collect()
}
