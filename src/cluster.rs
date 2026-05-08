use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::gallery::{self, Annotation};

pub struct ClusterConfig {
    pub namespace: String,
    pub k: usize,
    pub galerie_path: PathBuf,
}

pub fn run_cluster(cfg: ClusterConfig) -> Result<()> {
    let mut gf = gallery::load_gallery_file(&cfg.galerie_path)?;

    // Collect (photo index, embedding vector) for all photos with this namespace.
    let mut indexed: Vec<(usize, Vec<f32>)> = gf.photos.iter().enumerate()
        .filter_map(|(i, entry)| {
            entry.annotations.iter().find_map(|a| a.decode_embedding(&cfg.namespace))
                .map(|v| (i, v))
        })
        .collect();

    if indexed.len() < cfg.k {
        anyhow::bail!(
            "only {} photos have embeddings for namespace {:?}, need at least {}",
            indexed.len(), cfg.namespace, cfg.k
        );
    }

    // Verify dimension consistency.
    let dim = indexed[0].1.len();
    let mismatched: Vec<usize> = indexed.iter()
        .filter(|(_, v)| v.len() != dim)
        .map(|(i, _)| *i)
        .collect();
    if !mismatched.is_empty() {
        eprintln!("warning: {} photos have embedding length != {dim}, skipping them", mismatched.len());
        indexed.retain(|(i, _)| !mismatched.contains(i));
        if indexed.len() < cfg.k {
            anyhow::bail!("too few consistent embeddings after filtering mismatches");
        }
    }

    // L2-normalise all vectors (makes Euclidean k-means equivalent to cosine k-means).
    let mut points: Vec<Vec<f32>> = indexed.iter()
        .map(|(_, v)| l2_normalise(v))
        .collect();

    eprintln!("clustering {} photos into {} clusters (namespace: {:?})…",
        points.len(), cfg.k, cfg.namespace);

    let assignments = kmeans(&mut points, cfg.k);

    // Write cluster assignments back to the gallery file.
    for ((photo_idx, _), cluster_id) in indexed.iter().zip(assignments.iter()) {
        let entry = &mut gf.photos[*photo_idx];
        entry.annotations.retain(|a| !matches!(a,
            Annotation::ClusterAssignment { namespace, .. } if namespace == &cfg.namespace));
        entry.annotations.push(Annotation::ClusterAssignment {
            namespace: cfg.namespace.clone(),
            cluster_id: *cluster_id,
        });
    }

    gallery::save_gallery_file(&cfg.galerie_path, &gf)
        .with_context(|| format!("saving {:?}", cfg.galerie_path))?;

    // Print a cluster size summary.
    let mut counts = vec![0usize; cfg.k];
    for &id in &assignments { counts[id as usize] += 1; }
    for (i, c) in counts.iter().enumerate() {
        eprintln!("  cluster {i}: {c} photos");
    }

    Ok(())
}

fn l2_normalise(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < 1e-12 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn sq_dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// K-means++ initialisation: pick k diverse starting centroids.
fn kmeans_pp_init(points: &[Vec<f32>], k: usize) -> Vec<Vec<f32>> {
    // Deterministic seed: hash the first point's bytes via seahash.
    let seed_bytes: Vec<u8> = points[0].iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    let seed = seahash::hash(&seed_bytes) as usize;
    let mut centroids = vec![points[seed % points.len()].clone()];

    while centroids.len() < k {
        // For each point, compute min squared distance to any existing centroid.
        let dists: Vec<f32> = points.iter()
            .map(|p| centroids.iter().map(|c| sq_dist(p, c)).fold(f32::INFINITY, f32::min))
            .collect();
        let total: f32 = dists.iter().sum();
        // Pick next centroid proportional to distance squared.
        // Use seahash of centroid count as a deterministic "random" value.
        let threshold = (seahash::hash(&[centroids.len() as u8]) as f32 / u64::MAX as f32) * total;
        let mut cumsum = 0.0f32;
        let mut chosen = points.len() - 1;
        for (i, &d) in dists.iter().enumerate() {
            cumsum += d;
            if cumsum >= threshold {
                chosen = i;
                break;
            }
        }
        centroids.push(points[chosen].clone());
    }
    centroids
}

/// Lloyd's k-means algorithm on pre-normalised points. Returns cluster assignments (0..k).
fn kmeans(points: &[Vec<f32>], k: usize) -> Vec<u32> {
    let n = points.len();
    let dim = points[0].len();
    let mut centroids = kmeans_pp_init(points, k);
    let mut assignments = vec![0u32; n];

    for _iter in 0..300 {
        // Assignment step.
        for (i, p) in points.iter().enumerate() {
            let best = centroids.iter().enumerate()
                .min_by(|(_, a), (_, b)| {
                    sq_dist(p, a).partial_cmp(&sq_dist(p, b)).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(j, _)| j)
                .unwrap_or(0);
            assignments[i] = best as u32;
        }

        // Update step: recompute centroids as mean of assigned points.
        let mut new_centroids = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];
        for (i, p) in points.iter().enumerate() {
            let c = assignments[i] as usize;
            for (d, x) in new_centroids[c].iter_mut().zip(p.iter()) {
                *d += x;
            }
            counts[c] += 1;
        }
        for (c, cnt) in counts.iter().enumerate() {
            if *cnt > 0 {
                for d in new_centroids[c].iter_mut() {
                    *d /= *cnt as f32;
                }
            } else {
                // Empty cluster: keep old centroid.
                new_centroids[c] = centroids[c].clone();
            }
        }

        // Convergence check.
        let movement: f32 = centroids.iter().zip(new_centroids.iter())
            .map(|(old, new)| sq_dist(old, new))
            .sum();
        centroids = new_centroids;
        if movement < 1e-8 {
            break;
        }
    }

    // Re-normalise centroids and do a final assignment pass for clean results.
    let centroids: Vec<Vec<f32>> = centroids.into_iter().map(|c| l2_normalise(&c)).collect();
    for (i, p) in points.iter().enumerate() {
        // Use dot product (= cosine similarity on unit vectors) for final assignment.
        let best = centroids.iter().enumerate()
            .max_by(|(_, a), (_, b)| {
                dot(p, a).partial_cmp(&dot(p, b)).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(j, _)| j)
            .unwrap_or(0);
        assignments[i] = best as u32;
    }

    assignments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kmeans_two_clear_clusters() {
        // Two tight clusters far apart.
        let mut points: Vec<Vec<f32>> = (0..20)
            .map(|i| if i < 10 { vec![1.0, 0.0] } else { vec![0.0, 1.0] })
            .collect();
        let assignments = kmeans(&mut points, 2);
        // All first 10 should be in the same cluster.
        let c0 = assignments[0];
        assert!(assignments[..10].iter().all(|&a| a == c0));
        assert!(assignments[10..].iter().all(|&a| a != c0));
    }
}
