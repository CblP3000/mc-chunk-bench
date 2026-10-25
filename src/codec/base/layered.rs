//! Layered codec (user-designed format).
//!
//! A chunk (or a 2x2 chunk tile for footprint = 32) is split into horizontal
//! layers. Each layer has an 8-bit header:
//!
//! ```text
//! bit 7..6  storage type
//!    0 = solid: all cells identical; bit 5..4 = id width
//!               (0 -> 4 bits, 1 -> 8, 2 -> 16, 3 -> 32); bits 3..0 unused
//!    1 = adaptive map: bit 5..3 = index width (0 -> 1 bit ... 5 -> 32 bits)
//!               bit 2 = has local palette; bit 1..0 = source list
//!               (0 global, 1 biome, 2 layer palette, 3 reserved)
//!    2 = replicator: bit 5..4 = run-count width
//!               (0 -> 4 bits, 1 -> 8, 2 -> 12, 3 -> 20);
//!               bit 3..0 = high nibble of the first run count
//!    3 = repeat: bit 5..4 = mode (0 exact copy of previous layer,
//!               1 periodic copy of layer n-2); bits 3..0 unused
//! ```
//!
//! Everything after the fixed header is one LSB-first bitstream (varints are
//! written bit-wise, 7 bits per group). The encoder picks the cheapest valid
//! representation per layer.

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub const LAYER_MAGIC: [u8; 2] = [0x43, 0x4c]; // "CL"
pub const LAYER_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy)]
pub struct LayeredConfig {
    /// layer footprint width (16 = one chunk, 32 = 2x2 chunk tile)
    pub footprint: usize,
    /// layer thickness in blocks
    pub slice_height: usize,
}

impl Default for LayeredConfig {
    fn default() -> Self {
        Self {
            footprint: 16,
            slice_height: 1,
        }
    }
}

pub struct LayeredCodec {
    pub cfg: LayeredConfig,
}

impl LayeredCodec {
    pub fn new(cfg: LayeredConfig) -> Self {
        Self { cfg }
    }

    fn group(&self) -> usize {
        let w = self.cfg.footprint;
        (w * w) / 256
    }
}

pub const REGISTRY: &[RegistryEntry] = &[
    RegistryEntry {
        id: "layered",
        name: "Layered 16x16 h1 (solid/adaptive/replicator/repeat)",
        group: "layered",
        description: "Слойный формат (слой = 1 блок, 16×16): заголовок байт на слой, 4 типа — solid / adaptive map / replicator / repeat. Выбирается минимальный по битам.",
        make: || Box::new(LayeredCodec::new(LayeredConfig::default())) as Box<dyn Codec>,
    },
];

// ---------------- bit helpers (LSB-first) ----------------

struct BitWriter {
    buf: Vec<u8>,
    bitpos: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            bitpos: 0,
        }
    }

    fn write_bits(&mut self, mut val: u64, n: u32) {
        debug_assert!(n <= 64);
        for _ in 0..n {
            if self.bitpos % 8 == 0 {
                self.buf.push(0);
            }
            let byte = self.buf.len() - 1;
            if val & 1 == 1 {
                self.buf[byte] |= 1 << (self.bitpos % 8);
            }
            val >>= 1;
            self.bitpos += 1;
        }
    }

    fn write_varint(&mut self, mut v: u32) {
        loop {
            let b = (v & 0x7f) as u64;
            v >>= 7;
            if v == 0 {
                self.write_bits(b, 7);
                self.write_bits(0, 1);
                break;
            }
            self.write_bits(b, 7);
            self.write_bits(1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bitpos % 8 != 0 {
            let byte = self.buf.len() - 1;
            let rest = self.bitpos % 8;
            self.buf[byte] &= (1 << rest) - 1;
        }
        self.buf
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    bitpos: u64,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, bitpos: 0 }
    }

    fn read_bits(&mut self, n: u32) -> Result<u64> {
        let mut v = 0u64;
        for i in 0..n {
            let byte = (self.bitpos / 8) as usize;
            if byte >= self.buf.len() {
                bail!("bit stream out of bounds");
            }
            let bit = (self.bitpos % 8) as u32;
            if self.buf[byte] & (1 << bit) != 0 {
                v |= 1 << i;
            }
            self.bitpos += 1;
        }
        Ok(v)
    }

    fn read_varint(&mut self) -> Result<u32> {
        let mut v = 0u32;
        let mut shift = 0u32;
        loop {
            let group = self.read_bits(7)? as u32;
            let cont = self.read_bits(1)? == 1;
            v |= group << shift;
            if !cont {
                return Ok(v);
            }
            shift += 7;
            if shift > 28 {
                bail!("varint too long");
            }
        }
    }
}

// ---------------- helpers ----------------

fn bits_for_max(value: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    32 - value.leading_zeros()
}

fn id_width_kind(id: u32) -> u32 {
    if id < 16 {
        0
    } else if id < 256 {
        1
    } else if id < 65536 {
        2
    } else {
        3
    }
}

fn id_bits_for_kind(kind: u32) -> u32 {
    match kind {
        0 => 4,
        1 => 8,
        2 => 16,
        _ => 32,
    }
}

fn count_width_kind(max_run: u32) -> u32 {
    if max_run <= 15 {
        0
    } else if max_run <= 255 {
        1
    } else if max_run <= 4095 {
        2
    } else {
        3
    }
}

fn count_bits_for_kind(kind: u32) -> u32 {
    match kind {
        0 => 4,
        1 => 8,
        2 => 12,
        _ => 20,
    }
}

fn build_layers(chunks: &[&Chunk], cfg: &LayeredConfig) -> Result<(Vec<Vec<u32>>, i32, usize)> {
    let w = cfg.footprint;
    if chunks.is_empty() || chunks.len() != (w * w) / 256 {
        bail!("layered tile needs {} chunks for footprint {w}", (w * w) / 256);
    }
    let min_y = chunks[0].section_min_y;
    let n_sec = chunks[0].sections.len();
    for c in chunks {
        if c.section_min_y != min_y || c.sections.len() != n_sec {
            bail!("chunks in tile have different heights");
        }
    }
    let height = n_sec * 16;
    if height % cfg.slice_height != 0 {
        bail!("slice_height must divide world height");
    }
    let n_layers = height / cfg.slice_height;
    let cells_per_layer = w * w * cfg.slice_height;

    let mut layers: Vec<Vec<u32>> = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        let base_y = min_y + (l as i32) * cfg.slice_height as i32;
        let mut cells = Vec::with_capacity(cells_per_layer);
        for i in 0..cells_per_layer {
            let ly = i / (w * w);
            let rem = i % (w * w);
            let lz = rem / w;
            let lx = rem % w;
            let abs_y = base_y + ly as i32;
            let sec_idx = ((abs_y - min_y) / 16) as usize;
            let gy = (abs_y - min_y) % 16;
            let chunk_cx = lx / 16;
            let chunk_cz = lz / 16;
            let x = lx % 16;
            let z = lz % 16;
            let chunk = chunks[chunk_cz * (w / 16) + chunk_cx];
            cells.push(chunk.sections[sec_idx].blocks[(gy as usize) * 256 + z * 16 + x]);
        }
        layers.push(cells);
    }
    Ok((layers, min_y, n_sec))
}

/// Candidate costs (in bits) per layer; then re-encoded deterministically.
fn layer_solid_cost_bits(_cells: &[u32], id: u32) -> u64 {
    8 + id_bits_for_kind(id_width_kind(id)) as u64
}

fn layer_replicator_cost_bits(cells: &[u32]) -> u64 {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &c in cells {
        if let Some(last) = runs.last_mut() {
            if last.1 == c {
                last.0 += 1;
                continue;
            }
        }
        runs.push((1, c));
    }
    let max_run = runs.iter().map(|(n, _)| *n).max().unwrap_or(1);
    let ck = count_width_kind(max_run);
    let cw = count_bits_for_kind(ck);
    let max_len = (1u64 << cw) - 1;
    let mut n_runs = 0u64;
    for (n, _) in runs {
        n_runs += (n as u64).div_ceil(max_len);
    }
    let max_id = cells.iter().copied().max().unwrap_or(0);
    let iw = id_bits_for_kind(id_width_kind(max_id));
    8 + 8 + n_runs * (cw + iw) as u64
}

fn layer_adaptive_cost_bits(cells: &[u32], list_ids: Option<&[u32]>, pal: Option<&[u32]>) -> u64 {
    let (idx_bits, palette_cost) = if let Some(pal) = pal {
        (bits_for_max((pal.len() - 1) as u32).max(1), {
            let mut bits = 0u64;
            bits += 8; // count varint (1 byte min)
            for id in pal {
                bits += 7 * (bits_for_max(*id).div_ceil(7)) as u64 + 1;
            }
            bits
        })
    } else {
        let list_max = list_ids.map(|l| l.iter().copied().max().unwrap_or(0)).unwrap_or(0);
        (bits_for_max(list_max).clamp(1, 32), 0)
    };
    let width = match idx_bits {
        1 => 1,
        2 => 2,
        3..=4 => 4,
        5..=8 => 8,
        9..=16 => 16,
        _ => 32,
    };
    let n = cells.len() as u64;
    8 + palette_cost + n * width as u64
}

fn encode_layer_best(
    cells: &[u32],
    prev: Option<&[u32]>,
    prev2: Option<&[u32]>,
    ctx: &Ctx,
    biome_list_candidates: &[(u16, Vec<u32>)],
    w: &mut BitWriter,
) {
    let mut best_bits = u64::MAX;
    let mut best = 0u8;
    let mut best_id = 0u32;
    let mut best_list_sel = 0u32;
    let mut best_biome: Option<u16> = None;
    let mut best_pal: Option<Vec<u32>> = None;
    let mut best_mode = 0u32;

    if let Some(p) = prev {
        if p == cells {
            best_bits = 8;
            best = 3;
            best_mode = 0;
        }
    }
    if let Some(p) = prev2 {
        if p == cells && best != 3 {
            best_bits = 8;
            best = 3;
            best_mode = 1;
        }
    }

    if let Some(id) = cells.iter().copied().reduce(|a, b| if a == b { a } else { u32::MAX }) {
        if id != u32::MAX {
            let c = layer_solid_cost_bits(cells, id);
            if c < best_bits {
                best_bits = c;
                best = 0;
                best_id = id;
            }
        }
    }

    let rc = layer_replicator_cost_bits(cells);
    if rc < best_bits {
        best_bits = rc;
        best = 2;
    }

    // adaptive
    let max_id = cells.iter().copied().max().unwrap_or(0);
    let global_ids: Vec<u32> = (0..=max_id).collect();
    let gc = layer_adaptive_cost_bits(cells, Some(&global_ids), None);
    if gc < best_bits {
        best_bits = gc;
        best = 1;
        best_list_sel = 0;
    }
    for (biome_id, list) in biome_list_candidates {
        if list.is_empty() {
            continue;
        }
        let ok = cells.iter().all(|c| list.contains(c));
        if !ok {
            continue;
        }
        let bc = layer_adaptive_cost_bits(cells, Some(list), None);
        if bc < best_bits {
            best_bits = bc;
            best = 1;
            best_list_sel = 1;
            best_biome = Some(*biome_id);
        }
    }
    {
        let mut pal: Vec<u32> = cells.to_vec();
        pal.sort_unstable();
        pal.dedup();
        let pc = layer_adaptive_cost_bits(cells, None, Some(&pal));
        if pc < best_bits {
            best_bits = pc;
            best = 1;
            best_list_sel = 2;
            best_pal = Some(pal);
        }
    }

    match best {
        0 => {
            let kind = id_width_kind(best_id);
            w.write_bits(0, 2);
            w.write_bits(kind as u64, 2);
            w.write_bits(0, 4);
            w.write_bits(best_id as u64, id_bits_for_kind(kind));
        }
        3 => {
            w.write_bits(3, 2);
            w.write_bits(best_mode as u64, 2);
            w.write_bits(0, 4);
        }
        2 => {
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for &c in cells {
                if let Some(last) = runs.last_mut() {
                    if last.1 == c {
                        last.0 += 1;
                        continue;
                    }
                }
                runs.push((1, c));
            }
            let max_run = runs.iter().map(|(n, _)| *n).max().unwrap_or(1);
            let ck = count_width_kind(max_run);
            let cw = count_bits_for_kind(ck);
            let max_len = (1u64 << cw) - 1;
            let mut runs2: Vec<(u32, u32)> = Vec::new();
            for (n, id) in runs {
                let mut left = n;
                while left > max_len as u32 {
                    runs2.push((max_len as u32, id));
                    left -= max_len as u32;
                }
                runs2.push((left, id));
            }
            let max_id = cells.iter().copied().max().unwrap_or(0);
            let ik = id_width_kind(max_id);
            let iw = id_bits_for_kind(ik);
            let first_count = runs2[0].0;
            w.write_bits(2, 2);
            w.write_bits(ck as u64, 2);
            w.write_bits((first_count >> (cw - 4)) as u64, 4);
            w.write_bits(ik as u64, 8);
            let first_low = cw - 4;
            if first_low > 0 {
                w.write_bits((first_count as u64) & ((1u64 << first_low) - 1), first_low);
            }
            w.write_bits(runs2[0].1 as u64, iw);
            for (n, id) in runs2.iter().skip(1) {
                w.write_bits(*n as u64, cw);
                w.write_bits(*id as u64, iw);
            }
        }
        1 => {
            let idx_bits = match best_list_sel {
                1 => {
                    let list = biome_list_candidates
                        .iter()
                        .find(|(b, _)| Some(*b) == best_biome)
                        .map(|(_, l)| l.clone())
                        .unwrap_or_default();
                    bits_for_max(list.iter().copied().max().unwrap_or(0)).clamp(1, 32)
                }
                2 => {
                    let pal = best_pal.as_ref().unwrap();
                    bits_for_max((pal.len() - 1) as u32).max(1)
                }
                _ => {
                    let max_id = cells.iter().copied().max().unwrap_or(0);
                    bits_for_max(max_id).clamp(1, 32)
                }
            };
            let kind = match idx_bits {
                1 => 0,
                2 => 1,
                3..=4 => 2,
                5..=8 => 3,
                9..=16 => 4,
                _ => 5,
            };
            let width = match kind {
                0 => 1,
                1 => 2,
                2 => 4,
                3 => 8,
                4 => 16,
                _ => 32,
            };
            let has_palette = (best_list_sel == 2) as u32;
            w.write_bits(1, 2);
            w.write_bits(kind as u64, 3);
            w.write_bits(has_palette as u64, 1);
            w.write_bits(best_list_sel as u64, 2);
            if best_list_sel == 2 {
                let pal = best_pal.as_ref().unwrap();
                w.write_varint(pal.len() as u32);
                for id in pal {
                    w.write_varint(*id);
                }
                for &c in cells {
                    let idx = pal.iter().position(|&p| p == c).unwrap() as u64;
                    w.write_bits(idx, width);
                }
            } else if best_list_sel == 1 {
                let biome_id = best_biome.unwrap();
                w.write_bits(biome_id as u64, 16);
                let list = biome_list_candidates
                    .iter()
                    .find(|(b, _)| Some(*b) == best_biome)
                    .map(|(_, l)| l)
                    .unwrap();
                for &c in cells {
                    let idx = list.iter().position(|&p| p == c).unwrap() as u64;
                    w.write_bits(idx, width);
                }
            } else {
                for &c in cells {
                    w.write_bits(c as u64, width);
                }
            }
        }
        _ => unreachable!(),
    }
    let _ = ctx;
}

impl Codec for LayeredCodec {
    fn id(&self) -> String {
        match (self.cfg.footprint, self.cfg.slice_height) {
            (16, 1) => "layered".to_string(),
            (16, 4) => "layered-h4".to_string(),
            (16, 16) => "layered-h16".to_string(),
            (32, 16) => "layered-32".to_string(),
            _ => "layered-custom".to_string(),
        }
    }

    fn name(&self) -> String {
        format!(
            "Layered {}x{} h{} (solid/adaptive/replicator/repeat)",
            self.cfg.footprint, self.cfg.footprint, self.cfg.slice_height
        )
    }


    fn description(&self) -> String {
        "Per-layer header byte with 4 storage types: solid (uniform layer), \
         adaptive map (index width 1..32 bits, optional local palette, source \
         list: global / biome / layer), replicator (run-length with 4/8/12/20 \
         bit counts, first count nibble in the header), repeat (identical to \
         previous layer or period-2 pattern)."
            .to_string()
    }

    fn group_size(&self) -> usize {
        self.group()
    }

    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8> {
        let (layers, min_y, n_sec) = match build_layers(chunks, &self.cfg) {
            Ok(v) => v,
            Err(_) => {
                let mut out = Vec::new();
                out.extend_from_slice(&LAYER_MAGIC);
                out.push(0xff);
                return out;
            }
        };
        let w = self.cfg.footprint;
        let sh = self.cfg.slice_height;

        let mut out = Vec::new();
        out.extend_from_slice(&LAYER_MAGIC);
        out.push(LAYER_VERSION);
        out.push(w as u8);
        out.push(sh as u8);
        out.push((min_y as i8) as u8);
        out.push(n_sec as u8);
        out.push(chunks.len() as u8);
        // biome payload: per chunk, per section, 64 x u16 LE
        for c in chunks {
            for sec in &c.sections {
                for &b in &sec.biomes {
                    out.extend_from_slice(&b.to_le_bytes());
                }
            }
        }

        // Candidate biome lists: dominant biome (biome id) of each chunk, dedup.
        let mut biome_list_candidates: Vec<(u16, Vec<u32>)> = Vec::new();
        for c in chunks {
            let b = c.sections[0].biomes[0] as usize;
            if b < ctx.biome_block_lists.len() {
                let list = &ctx.biome_block_lists[b];
                if !biome_list_candidates
                    .iter()
                    .any(|(_, l)| l == list)
                {
                    biome_list_candidates.push((b as u16, list.clone()));
                }
            }
        }

        let mut w_bits = BitWriter::new();
        for i in 0..layers.len() {
            let prev = if i > 0 { Some(layers[i - 1].as_slice()) } else { None };
            let prev2 = if i > 1 { Some(layers[i - 2].as_slice()) } else { None };
            encode_layer_best(
                &layers[i],
                prev,
                prev2,
                ctx,
                &biome_list_candidates,
                &mut w_bits,
            );
        }
        out.extend(w_bits.finish());
        out
    }

    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.len() < 8 || bytes[0] != LAYER_MAGIC[0] || bytes[1] != LAYER_MAGIC[1] {
            bail!("not a layered stream");
        }
        if bytes[2] != LAYER_VERSION {
            bail!("unsupported layered version");
        }
        let w = bytes[3] as usize;
        let sh = bytes[4] as usize;
        let min_y = bytes[5] as i8 as i32;
        let n_sec = bytes[6] as usize;
        let n_chunks = bytes[7] as usize;
        if w != self.cfg.footprint {
            bail!("footprint mismatch");
        }
        let height = n_sec * 16;
        let n_layers = height / sh;
        let cells_per_layer = w * w * sh;
        let bio_bytes = n_chunks * n_sec * 64 * 2;
        if bytes.len() < 8 + bio_bytes {
            bail!("layered stream too short for biome payload");
        }
        // read biome payload
        let mut biome_ids: Vec<Vec<u16>> = Vec::new(); // per chunk: n_sec*64
        {
            let mut pos = 8usize;
            for _ in 0..n_chunks {
                let mut sec_bio = Vec::with_capacity(n_sec * 64);
                for _ in 0..n_sec * 64 {
                    sec_bio.push(u16::from_le_bytes([bytes[pos], bytes[pos + 1]]));
                    pos += 2;
                }
                biome_ids.push(sec_bio);
            }
        }
        let mut r = BitReader::new(&bytes[8 + bio_bytes..]);

        let mut layers: Vec<Vec<u32>> = Vec::with_capacity(n_layers);
        let mut prev2: Option<Vec<u32>> = None;
        let mut prev: Option<Vec<u32>> = None;
        for _li in 0..n_layers {
            let typ = r.read_bits(2)? as u8;
            let cells = match typ {
                0 => {
                    let kind = r.read_bits(2)? as u32;
                    let _ = r.read_bits(4)?;
                    let id = r.read_bits(id_bits_for_kind(kind))? as u32;
                    vec![id; cells_per_layer]
                }
                3 => {
                    let mode = r.read_bits(2)? as u32;
                    let _ = r.read_bits(4)?;
                    if mode == 0 {
                        prev.clone().unwrap_or_else(|| vec![0; cells_per_layer])
                    } else {
                        prev2.clone().unwrap_or_else(|| vec![0; cells_per_layer])
                    }
                }
                2 => {
                    let ck = r.read_bits(2)? as u32;
                    let first_hi = r.read_bits(4)? as u32;
                    let cw = count_bits_for_kind(ck);
                    let first_low = cw - 4;
                    let ik = r.read_bits(8)? as u32;
                    let iw = id_bits_for_kind(ik);
                    let first_count = (first_hi << first_low) | (r.read_bits(first_low)? as u32);
                    let mut v = Vec::with_capacity(cells_per_layer);
                    let id0 = r.read_bits(iw)? as u32;
                    for _ in 0..first_count.min(cells_per_layer as u32) {
                        v.push(id0);
                    }
                    let mut runs = 0;
                    while v.len() < cells_per_layer && runs < cells_per_layer {
                        let cnt = r.read_bits(cw)? as u32;
                        let id = r.read_bits(iw)? as u32;
                        let take = cnt.min((cells_per_layer - v.len()) as u32);
                        for _ in 0..take {
                            v.push(id);
                        }
                        runs += 1;
                    }
                    v
                }
                1 => {
                    let kind = r.read_bits(3)? as u32;
                    let has_palette = r.read_bits(1)? == 1;
                    let list_sel = r.read_bits(2)? as u32;
                    let idx_bits = match kind {
                        0 => 1,
                        1 => 2,
                        2 => 4,
                        3 => 8,
                        4 => 16,
                        _ => 32,
                    };
                    if has_palette {
                        let pal_len = r.read_varint()? as usize;
                        let mut pal = Vec::with_capacity(pal_len);
                        for _ in 0..pal_len {
                            pal.push(r.read_varint()?);
                        }
                        let mut v = Vec::with_capacity(cells_per_layer);
                        for _ in 0..cells_per_layer {
                            let idx = r.read_bits(idx_bits)? as usize;
                            v.push(pal.get(idx).copied().unwrap_or(0));
                        }
                        v
                    } else if list_sel == 1 {
                        let biome_id = r.read_bits(16)? as u16;
                        let list = ctx
                            .biome_block_lists
                            .get(biome_id as usize)
                            .cloned()
                            .unwrap_or_default();
                        let mut v = Vec::with_capacity(cells_per_layer);
                        for _ in 0..cells_per_layer {
                            let idx = r.read_bits(idx_bits)? as usize;
                            v.push(list.get(idx).copied().unwrap_or(0));
                        }
                        v
                    } else {
                        // list_sel 0: indices are global ids directly
                        let mut v = Vec::with_capacity(cells_per_layer);
                        for _ in 0..cells_per_layer {
                            v.push(r.read_bits(idx_bits)? as u32);
                        }
                        v
                    }
                }
                _ => bail!("bad layer type"),
            };
            prev2 = prev.take();
            prev = Some(cells.clone());
            layers.push(cells);
        }

        let w_chunks = w / 16;
        let mut chunks: Vec<Chunk> = Vec::with_capacity(n_chunks);
        for ci in 0..n_chunks {
            let mut sections = Vec::with_capacity(n_sec);
            for _ in 0..n_sec {
                sections.push(Section::air());
            }
            chunks.push(Chunk {
                x: 0,
                z: ci as i32,
                section_min_y: min_y,
                sections,
            });
        }
        for (l, cells) in layers.iter().enumerate() {
            let base_y = min_y + (l as i32) * sh as i32;
            for (i, &id) in cells.iter().enumerate() {
                let ly = i / (w * w);
                let rem = i % (w * w);
                let lz = rem / w;
                let lx = rem % w;
                let abs_y = base_y + ly as i32;
                let sec_idx = ((abs_y - min_y) / 16) as usize;
                let gy = (abs_y - min_y) % 16;
                let ccx = lx / 16;
                let ccz = lz / 16;
                let ci = ccz * w_chunks + ccx;
                if ci < chunks.len() {
                    chunks[ci].sections[sec_idx].blocks[(gy as usize) * 256 + (lz % 16) * 16 + (lx % 16)] = id;
                }
            }
        }
        // restore biomes
        for (ci, chunk) in chunks.iter_mut().enumerate() {
            let bio = &biome_ids[ci];
            for (si, sec) in chunk.sections.iter_mut().enumerate() {
                sec.biomes.copy_from_slice(&bio[si * 64..si * 64 + 64]);
            }
        }
        let _ = ctx;
        Ok(chunks)
    }
}
