//! Benchmark harness: encode/decode every tile with every codec, verify
//! round-trips, collect size + time metrics.

use crate::chunk::Chunk;
use crate::codec::Ctx;
use crate::world::WorldData;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecResult {
    pub id: String,
    pub name: String,
    /// method group ("raw" | "rle" | "minecraft" | "layered" | ...)
    pub group: String,
    /// true for entries produced by the "compress everything with deflate" toggle
    pub gzip_extra: bool,
    pub description: String,
    pub group_size: usize,
    pub bytes_total: u64,
    pub bytes_avg: f64,
    pub bytes_min: u64,
    pub bytes_max: u64,
    pub encode_us_avg: f64,
    pub decode_us_avg: f64,
    pub ok: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetReport {
    pub label: String,
    pub n_chunks: usize,
    pub worlds: Vec<String>,
    pub raw_bytes: u64,
    pub results: Vec<CodecResult>,
}

impl DatasetReport {
    pub fn best_by_size(&self) -> Option<&CodecResult> {
        self.results
            .iter()
            .filter(|r| r.ok)
            .min_by(|a, b| a.bytes_avg.total_cmp(&b.bytes_avg))
    }

    pub fn fastest_encode(&self) -> Option<&CodecResult> {
        self.results
            .iter()
            .filter(|r| r.ok)
            .min_by(|a, b| a.encode_us_avg.total_cmp(&b.encode_us_avg))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub generated_at: String,
    pub world_dirs: Vec<String>,
    pub max_chunks: usize,
    pub seed: u64,
    pub total_blocks: u64,
    pub global_blocks: usize,
    pub biomes: usize,
    pub datasets: Vec<DatasetReport>,
}

pub fn chunks_equal(a: &Chunk, b: &Chunk) -> bool {
    if a.section_min_y != b.section_min_y || a.sections.len() != b.sections.len() {
        return false;
    }
    for (sa, sb) in a.sections.iter().zip(b.sections.iter()) {
        if sa.blocks != sb.blocks || sa.biomes != sb.biomes {
            return false;
        }
    }
    true
}

fn run_dataset(label: &str, data: &WorldData, pruned: Option<&WorldData>) -> Result<DatasetReport> {
    let data_ref = pruned.unwrap_or(data);
    let chunks: Vec<&Chunk> = data_ref.chunks.iter().map(|c| &c.chunk).collect();
    let ctx = Ctx {
        registry: &data.registry,
        biomes: &data.biomes,
        biome_block_lists: &data.biome_block_lists,
    };

    let raw_avg = chunks
        .iter()
        .map(|c| (c.sections.len() * 4096 * 4 + c.sections.len() * 64 * 2) as u64)
        .sum::<u64>();

    let n_chunks = chunks.len();
    // tiles: for group_size > 1, only tile chunks with identical height signature
    let g_codecs: Vec<(crate::codec::CodecRegistration, Vec<Vec<&Chunk>>)> =
        crate::codec::all_registrations(true)
            .into_iter()
            .map(|reg| {
                let g = reg.codec.group_size();
                let mut tiles: Vec<Vec<&Chunk>> = Vec::new();
                if g == 1 {
                    tiles = chunks.iter().map(|c| vec![*c]).collect();
                } else {
                    let mut run_start = 0usize;
                    while run_start < chunks.len() {
                        let sig = (
                            chunks[run_start].section_min_y,
                            chunks[run_start].sections.len(),
                        );
                        let mut end = run_start;
                        while end < chunks.len()
                            && (chunks[end].section_min_y, chunks[end].sections.len()) == sig
                        {
                            end += 1;
                        }
                        let mut i = run_start;
                        while i + g <= end {
                            tiles.push(chunks[i..i + g].to_vec());
                            i += g;
                        }
                        run_start = end;
                    }
                }
                (reg, tiles)
            })
            .collect();

    let mut results = Vec::new();
    for (reg, tiles) in g_codecs {
        let codec = &reg.codec;
        let g = codec.group_size();
        let n_units = tiles.len().max(1);
        let group = reg.group.to_string();
        let gzip_extra = reg.gzip_extra;

        let mut bytes_total = 0u64;
        let mut bytes_min = u64::MAX;
        let mut bytes_max = 0u64;
        let mut per_chunk_bytes: Vec<u64> = Vec::new();
        let mut encode_us: u128 = 0;
        let mut decode_us: u128 = 0;
        let mut ok = true;
        let mut note = String::new();

        let mut tile_idx = 0usize;
        let mut any_measured = false;
        for tile in &tiles {
            if tile.len() != g {
                continue;
            }
            let tile_refs: Vec<&Chunk> = tile.to_vec();
            let t0 = Instant::now();
            let encoded = codec.encode_tile(&tile_refs, &ctx);
            encode_us += t0.elapsed().as_micros();

            let t1 = Instant::now();
            let decoded = match codec.decode_tile(&encoded, &ctx) {
                Ok(d) => d,
                Err(e) => {
                    ok = false;
                    note = format!("decode error: {e}");
                    Vec::new()
                }
            };
            decode_us += t1.elapsed().as_micros();

            bytes_total += encoded.len() as u64;
            let per = (encoded.len() as u64).div_ceil(g as u64);
            bytes_min = bytes_min.min(per);
            bytes_max = bytes_max.max(per);
            per_chunk_bytes.push(per);

            if ok {
                if decoded.len() != g {
                    ok = false;
                    note = format!("decode returned {} chunks, expected {g}", decoded.len());
                } else {
                    for (k, (orig, dec)) in tile.iter().zip(decoded.iter()).enumerate() {
                        if !chunks_equal(orig, dec) {
                            ok = false;
                            note = format!("round-trip mismatch at tile {tile_idx} chunk {k}");
                            break;
                        }
                    }
                }
            }
            any_measured = true;
            tile_idx += 1;
        }
        results.push(CodecResult {
            id: codec.id(),
            name: codec.name(),
            group,
            gzip_extra,
            description: codec.description(),
            group_size: g,
            bytes_total,
            bytes_avg: if any_measured {
                per_chunk_bytes.iter().sum::<u64>() as f64 / per_chunk_bytes.len() as f64
            } else {
                0.0
            },
            bytes_min: if any_measured { bytes_min } else { 0 },
            bytes_max: if any_measured { bytes_max } else { 0 },
            encode_us_avg: if any_measured {
                encode_us as f64 / n_units as f64 / g as f64
            } else {
                0.0
            },
            decode_us_avg: if any_measured {
                decode_us as f64 / n_units as f64 / g as f64
            } else {
                0.0
            },
            ok,
            note,
        });
    }

    let mut worlds: Vec<String> = data_ref.chunks.iter().map(|c| c.world.clone()).collect();
    worlds.sort();
    worlds.dedup();

    Ok(DatasetReport {
        label: label.to_string(),
        n_chunks,
        worlds,
        raw_bytes: raw_avg,
        results,
    })
}

pub fn run_benchmark(
    data: &WorldData,
    pruned: &WorldData,
    seed: u64,
    max_chunks: usize,
) -> Result<BenchmarkReport> {
    let original = run_dataset("original", data, None)?;
    let pruned_report = run_dataset("occlusion-pruned", data, Some(pruned))?;

    let total_blocks: u64 = data
        .chunks
        .iter()
        .map(|c| c.chunk.sections.len() as u64 * 4096)
        .sum();

    Ok(BenchmarkReport {
        generated_at: chrono_like_now(),
        world_dirs: original.worlds.clone(),
        max_chunks,
        seed,
        total_blocks,
        global_blocks: data.registry.names.len(),
        biomes: data.biomes.names.len(),
        datasets: vec![original, pruned_report],
    })
}

fn chrono_like_now() -> String {
    // avoid extra dependency; produce ISO-ish local time
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}
