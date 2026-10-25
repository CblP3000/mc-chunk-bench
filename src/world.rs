//! World loading: scan real save directories, extract chunks, build the
//! global block registry and per-biome block lists.

use crate::chunk::{parse_chunk, Chunk};
use crate::mca::RegionFile;
use crate::nbt::{parse_root, NbtValue};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BlockRegistry {
    pub names: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Default for BlockRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockRegistry {
    pub fn new() -> Self {
        let mut r = Self {
            names: Vec::new(),
            ids: HashMap::new(),
        };
        r.intern("minecraft:air");
        r
    }

    pub fn get(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }
}

#[derive(Debug, Clone)]
pub struct BiomeRegistry {
    pub names: Vec<String>,
    ids: HashMap<String, u16>,
}

impl BiomeRegistry {
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            ids: HashMap::new(),
        }
    }

    pub fn intern(&mut self, name: &str) -> u16 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len() as u16;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    pub fn get(&self, name: &str) -> Option<u16> {
        self.ids.get(name).copied()
    }
}

impl Default for BiomeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct LoadedChunk {
    pub chunk: Chunk,
    pub world: String,
    pub region: String,
    pub dominant_biome: u16,
}

pub struct WorldData {
    pub chunks: Vec<LoadedChunk>,
    pub registry: BlockRegistry,
    pub biomes: BiomeRegistry,
    /// per biome-id: sorted list of global block ids (most frequent first), max 255
    pub biome_block_lists: Vec<Vec<u32>>,
    pub global_block_counts: Vec<(u32, u64)>,
}

/// Tiny deterministic RNG (xorshift64*) for reproducible sampling.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

fn is_world_dir(dir: &Path) -> bool {
    dir.join("region").is_dir() || dir.join("level.dat").is_file()
}

fn is_excluded_world(dir: &Path, extra_excludes: &[String]) -> bool {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    extra_excludes
        .iter()
        .any(|p| name.contains(&p.to_lowercase()))
}

/// List world directories under a saves root.
/// `extra_excludes` — дополнительные подстроки имён миров для исключения.
pub fn discover_worlds(saves_root: &Path, extra_excludes: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if is_world_dir(saves_root) {
        out.push(saves_root.to_path_buf());
        return Ok(out);
    }
    let rd = std::fs::read_dir(saves_root)
        .with_context(|| format!("cannot read saves root: {}", saves_root.display()))?;
    for e in rd {
        let e = e?;
        let p = e.path();
        if p.is_dir() && is_world_dir(&p) && !is_excluded_world(&p, extra_excludes) {
            eprintln!("  include world: {}", p.display());
            out.push(p);
        } else if p.is_dir() && is_world_dir(&p) {
            eprintln!("  EXCLUDED world: {}", p.display());
        }
    }
    if out.is_empty() {
        bail!("no worlds found under {}", saves_root.display());
    }
    Ok(out)
}

fn chunk_dominant_biome(chunk: &Chunk) -> u16 {
    let mut counts: HashMap<u16, u32> = HashMap::new();
    for sec in &chunk.sections {
        for &b in &sec.biomes {
            *counts.entry(b).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(b, _)| b)
        .unwrap_or(0)
}

type RegionReport = (Vec<LoadedChunk>, Vec<(u16, u32, u64)>);

fn load_region_chunks(
    region_path: &Path,
    world_label: &str,
    max_chunks: usize,
    rng: &mut Rng,
    registry: &mut BlockRegistry,
    biomes: &mut BiomeRegistry,
) -> Result<RegionReport> {
    let region = RegionFile::open(region_path)?;
    let mut slots: Vec<usize> = (0..1024).filter(|&s| region.is_present(s)).collect();
    if slots.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let total = slots.len().min(max_chunks);
    while slots.len() > total {
        let k = rng.below(slots.len());
        slots.swap_remove(k);
    }

    let mut chunks = Vec::new();
    let mut pair_counts: HashMap<(u16, u32), u64> = HashMap::new();
    for slot in slots {
        let c = match region.read_chunk(slot) {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("skip chunk {slot} in {}: {e}", region_path.display());
                continue;
            }
        };
        // Chunk data may contain multiple root compounds (usually one). Parse first.
        let root = match parse_root(&c.nbt) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip chunk {slot} (nbt) in {}: {e}", region_path.display());
                continue;
            }
        };
        let chunk = match parse_chunk(&root, &mut |name| registry.intern(name), &mut |name| {
            biomes.intern(name)
        }) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "skip chunk {slot} (parse) in {}: {e}",
                    region_path.display()
                );
                continue;
            }
        };
        let biome = chunk_dominant_biome(&chunk);
        for sec in &chunk.sections {
            for &b in &sec.blocks {
                *pair_counts.entry((biome, b)).or_insert(0) += 1;
            }
        }
        chunks.push(LoadedChunk {
            chunk,
            world: world_label.to_string(),
            region: region_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            dominant_biome: biome,
        });
        if chunks.len() >= max_chunks {
            break;
        }
    }
    Ok((
        chunks,
        pair_counts
            .into_iter()
            .map(|((b, bl), c)| (b, bl, c))
            .collect(),
    ))
}

/// Load up to `max_chunks` chunks from the given world directories.
/// Returns the sampled dataset with registries and per-biome lists built.
pub fn load_dataset(worlds: &[PathBuf], max_chunks: usize, seed: u64) -> Result<WorldData> {
    let mut rng = Rng::new(seed);
    let mut registry = BlockRegistry::new();
    let mut biome_reg = BiomeRegistry::new();
    let mut all_chunks: Vec<LoadedChunk> = Vec::new();
    let mut pair_counts: HashMap<(u16, u32), u64> = HashMap::new();

    // Collect all (world, region) candidates, then shuffle for variety.
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    for w in worlds {
        let label = w
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let region_dir = w.join("region");
        if !region_dir.is_dir() {
            continue;
        }
        let mut regions: Vec<PathBuf> = std::fs::read_dir(&region_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "mca").unwrap_or(false))
            .collect();
        regions.sort();
        // cap regions per world to keep the scan reasonable
        for rp in regions.into_iter().take(64) {
            candidates.push((rp, label.clone()));
        }
    }
    if candidates.is_empty() {
        bail!("no region files found");
    }
    // deterministic shuffle
    for i in (1..candidates.len()).rev() {
        let j = rng.below(i + 1);
        candidates.swap(i, j);
    }

    let target_regions = candidates.len().clamp(1, 24);
    let per_region = ((max_chunks as f64) / (target_regions as f64)).ceil() as usize;
    let mut remaining = max_chunks;
    for (rp, label) in candidates {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(per_region);
        let (chunks, pairs) =
            match load_region_chunks(&rp, &label, take, &mut rng, &mut registry, &mut biome_reg) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("skip region {}: {e}", rp.display());
                    continue;
                }
            };
        for c in chunks {
            remaining = remaining.saturating_sub(1);
            all_chunks.push(c);
            if remaining == 0 {
                break;
            }
        }
        for (b, bl, c) in pairs {
            *pair_counts.entry((b, bl)).or_insert(0) += c;
        }
    }

    if all_chunks.is_empty() {
        bail!("no chunks could be loaded (check world paths)");
    }

    // Per-biome block lists (max 255, sorted by frequency).
    let mut biome_map: HashMap<u16, Vec<(u32, u64)>> = HashMap::new();
    for ((b, bl), c) in &pair_counts {
        biome_map.entry(*b).or_default().push((*bl, *c));
    }
    let mut biome_block_lists = vec![Vec::new(); biome_reg.names.len()];
    for (b, mut v) in biome_map {
        v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        v.truncate(255);
        biome_block_lists[b as usize] = v.into_iter().map(|(bl, _)| bl).collect();
    }

    // Global counts.
    let mut g: HashMap<u32, u64> = HashMap::new();
    for ((_, bl), c) in &pair_counts {
        *g.entry(*bl).or_insert(0) += c;
    }
    let mut global_block_counts: Vec<(u32, u64)> = g.into_iter().collect();
    global_block_counts.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    global_block_counts.truncate(5000);

    Ok(WorldData {
        chunks: all_chunks,
        registry,
        biomes: biome_reg,
        biome_block_lists,
        global_block_counts,
    })
}

/// Convenience: parse NBT root into a compound value (for tests / debug).
pub fn _parse_debug(data: &[u8]) -> Result<NbtValue> {
    parse_root(data)
}
