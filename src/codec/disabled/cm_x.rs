//! CM-Mix: контекстно-моделирующий кодек чанков.
//!
//! Схема:
//!  1. Палитры блоков и биомов (отсортированные id, дельта-varint).
//!  2. Индексы палитры кодируются побитово (бинарное дерево, MSB->LSB)
//!     адаптивным бинарным арифметическим кодером.
//!  3. Вероятность каждого бита — смесь предсказаний нескольких
//!     хэшированных контекстов (соседи снизу, слева, спереди, диагонали,
//!     высота) через логистический миксер + APM/SSE стадия.

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub struct CmXCodec;

pub const REGISTRY: &[RegistryEntry] = &[
    // ВРЕМЕННО ОТКЛЮЧЁН: слишком медленный encode/decode (~11 мс/чанк).
    // Для включения уберите комментарий со строк ниже.
    // RegistryEntry {
    //     group: "context-modeling",
    //     description: "Контекстное моделирование (lpaq-подобное): палитра + побитовый \
    //                   бинарный арифметический кодер с миксером хэшированных 3D-контекстов \
    //                   и APM. Максимальная степень сжатия для воксельных данных.",
    //     make: || Box::new(CmXCodec) as Box<dyn Codec>,
    // },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

    #[test]
    fn roundtrip_noise() {
        let mut reg = BlockRegistry::new();
        let mut bio = BiomeRegistry::new();
        let mut ids = Vec::new();
        for i in 0..60 {
            ids.push(reg.intern(&format!("minecraft:b{i}")));
        }
        for i in 0..8 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };
        let mut rng = 12345u64;
        let mut sections = Vec::new();
        for _ in 0..3 {
            let mut sec = Section::air();
            for v in sec.blocks.iter_mut() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *v = ids[(rng % 60) as usize];
            }
            for (i, b) in sec.biomes.iter_mut().enumerate() {
                *b = (i % 8) as u16;
            }
            sections.push(sec);
        }
        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = CmXCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}

// ---------------------------------------------------------------------------
// Утилиты: varint / zigzag
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        } else {
            out.push(b | 0x80);
        }
    }
}

fn get_varint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= bytes.len() {
            bail!("cm-x: unexpected end of stream in varint");
        }
        if shift >= 64 {
            bail!("cm-x: varint too long");
        }
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

// ---------------------------------------------------------------------------
// squash / stretch (логистические функции, 12-битная точность)
// ---------------------------------------------------------------------------

fn squash(d: i32) -> i32 {
    if d > 2047 {
        return 4095;
    }
    if d < -2047 {
        return 0;
    }
    const T: [i32; 33] = [
        1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994,
        3348, 3607, 3785, 3901, 3975, 4024, 4050, 4068, 4079, 4085, 4089, 4092, 4093, 4094,
    ];
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    (T[i] * (128 - w) + T[i + 1] * w + 64) >> 7
}

fn build_stretch() -> Vec<i16> {
    let mut t = vec![0i16; 4096];
    let mut pi = 0usize;
    for x in -2047..=2047i32 {
        let v = squash(x) as usize;
        for i in pi..=v {
            t[i] = x as i16;
        }
        pi = v + 1;
    }
    for i in pi..4096 {
        t[i] = 2047;
    }
    t
}

// ---------------------------------------------------------------------------
// Арифметический кодер (бинарный, 32-битный)
// ---------------------------------------------------------------------------

struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Encoder { x1: 0, x2: 0xffff_ffff, out: Vec::new() }
    }

    /// p — вероятность бита 1 в 12-битной шкале (1..4095).
    fn encode(&mut self, bit: u32, p: u32) {
        let range = (self.x2 - self.x1) as u64;
        let xmid = self.x1 + ((range * p as u64) >> 12) as u32;
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        let x = self.x1;
        self.out.push((x >> 24) as u8);
        self.out.push((x >> 16) as u8);
        self.out.push((x >> 8) as u8);
        self.out.push(x as u8);
        self.out
    }
}

struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut d = Decoder { x1: 0, x2: 0xffff_ffff, x: 0, data, pos: 0 };
        for _ in 0..4 {
            d.x = (d.x << 8) | d.next_byte() as u32;
        }
        d
    }

    fn next_byte(&mut self) -> u8 {
        if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            b
        } else {
            0
        }
    }

    fn decode(&mut self, p: u32) -> u32 {
        let range = (self.x2 - self.x1) as u64;
        let xmid = self.x1 + ((range * p as u64) >> 12) as u32;
        let bit = if self.x <= xmid {
            self.x2 = xmid;
            1
        } else {
            self.x1 = xmid + 1;
            0
        };
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
            self.x = (self.x << 8) | self.next_byte() as u32;
        }
        bit
    }
}

// ---------------------------------------------------------------------------
// StateMap: адаптивная вероятность со счётчиком
// ---------------------------------------------------------------------------

struct StateMap {
    t: Vec<u32>, // старшие 22 бита — вероятность, младшие 10 — счётчик
    limit: u32,
}

impl StateMap {
    fn new(n: usize, limit: u32) -> Self {
        StateMap { t: vec![1u32 << 31; n], limit }
    }

    #[inline]
    fn p(&self, i: usize) -> u32 {
        self.t[i] >> 20
    }

    #[inline]
    fn update(&mut self, i: usize, y: u32) {
        let v = self.t[i];
        let n = v & 1023;
        let p = (v >> 10) as i64;
        let target = (y as i64) << 22;
        let np = p + (target - p) * 2 / (2 * n as i64 + 3);
        let np = np.clamp(0, (1 << 22) - 1) as u32;
        let nn = if n < self.limit { n + 1 } else { n };
        self.t[i] = (np << 10) | nn;
    }
}

// ---------------------------------------------------------------------------
// APM / SSE
// ---------------------------------------------------------------------------

struct Apm {
    t: Vec<u16>,
    idx: usize,
    rate: u32,
}

impl Apm {
    fn new(n: usize, rate: u32) -> Self {
        let mut t = vec![0u16; n * 33];
        for i in 0..n {
            for j in 0..33 {
                t[i * 33 + j] = (squash((j as i32 - 16) * 128) * 16) as u16;
            }
        }
        Apm { t, idx: 0, rate }
    }

    fn p(&mut self, pr: i32, cx: usize, stretch: &[i16]) -> i32 {
        let s = stretch[pr as usize] as i32 + 2048;
        let lo = (s >> 7) as usize;
        let w = s & 127;
        let base = cx * 33 + lo;
        self.idx = if w > 63 { base + 1 } else { base };
        ((self.t[base] as i32) * (128 - w) + (self.t[base + 1] as i32) * w) >> 11
    }

    fn update(&mut self, y: u32) {
        let g = ((y as i32) << 16) + ((y as i32) << self.rate) - (y as i32) - (y as i32);
        let v = self.t[self.idx] as i32;
        self.t[self.idx] = (v + ((g - v) >> self.rate)) as u16;
    }
}

// ---------------------------------------------------------------------------
// Predictor: набор контекстных моделей + миксер + APM
// ---------------------------------------------------------------------------

struct Predictor {
    nm: usize,
    mask: u32,
    maps: Vec<StateMap>,
    weights: Vec<i32>,
    nsets: usize,
    wbase: usize,
    st: Vec<i32>,
    idx: Vec<usize>,
    pr_mix: i32,
    apm: Apm,
    apm_cx_n: usize,
    stretch: Vec<i16>,
}

impl Predictor {
    fn new(nm: usize, tbl_bits: u32, nsets: usize, apm_cx_n: usize) -> Self {
        let size = 1usize << tbl_bits;
        let mut maps = Vec::with_capacity(nm);
        for _ in 0..nm {
            maps.push(StateMap::new(size, 127));
        }
        Predictor {
            nm,
            mask: (size as u32) - 1,
            maps,
            weights: vec![(0.3 * 65536.0) as i32; nsets * nm],
            nsets,
            wbase: 0,
            st: vec![0; nm],
            idx: vec![0; nm],
            pr_mix: 2048,
            apm: Apm::new(apm_cx_n, 7),
            apm_cx_n,
            stretch: build_stretch(),
        }
    }

    /// Возвращает вероятность бита 1 (12 бит, 1..4095).
    fn predict(&mut self, node: u32, hashes: &[u32], apm_cx: u32) -> u32 {
        let nh = node.wrapping_mul(0x9E37_79B1);
        for i in 0..self.nm {
            let mut h = (hashes[i] ^ nh).wrapping_mul(0x85EB_CA6B);
            h ^= h >> 15;
            h = h.wrapping_mul(0xC2B2_AE35);
            h ^= h >> 13;
            let ix = (h & self.mask) as usize;
            self.idx[i] = ix;
            self.st[i] = self.stretch[self.maps[i].p(ix) as usize] as i32;
        }
        self.wbase = (node as usize).min(self.nsets - 1) * self.nm;
        let mut dot: i64 = 0;
        for i in 0..self.nm {
            dot += self.st[i] as i64 * self.weights[self.wbase + i] as i64;
        }
        let d = (dot >> 16).clamp(-2047, 2047) as i32;
        let pr = squash(d);
        self.pr_mix = pr;
        let cx = (apm_cx as usize) % self.apm_cx_n;
        let pa = self.apm.p(pr, cx, &self.stretch);
        let p = (pr + 3 * pa) >> 2;
        p.clamp(1, 4095) as u32
    }

    fn update(&mut self, y: u32) {
        let err = (((y as i32) << 12) - self.pr_mix) * 6;
        for i in 0..self.nm {
            let w = &mut self.weights[self.wbase + i];
            *w += (self.st[i] * err) >> 14;
            *w = (*w).clamp(-(1 << 22), 1 << 22);
        }
        for i in 0..self.nm {
            let ix = self.idx[i];
            self.maps[i].update(ix, y);
        }
        self.apm.update(y);
    }
}

fn hs(vals: &[u32]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &v in vals {
        h = (h ^ v).wrapping_mul(0x0100_0193);
        h = h.wrapping_add(h >> 16).wrapping_mul(0x9E37_79B1);
    }
    h ^ (h >> 16)
}

fn bits_for(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - ((n - 1) as u32).leading_zeros()
    }
}

// ---------------------------------------------------------------------------
// Контексты для блоков и биомов
// ---------------------------------------------------------------------------

const BLOCK_MODELS: usize = 9;
const BIOME_MODELS: usize = 5;

struct BlockGrid<'a> {
    v: &'a [u16],
    h: usize,
    sentinel: u32,
}

impl<'a> BlockGrid<'a> {
    #[inline]
    fn g(&self, x: i32, y: i32, z: i32) -> u32 {
        if x < 0 || x >= 16 || z < 0 || z >= 16 || y < 0 || y >= self.h as i32 {
            self.sentinel
        } else {
            self.v[(y as usize) * 256 + (z as usize) * 16 + x as usize] as u32
        }
    }
}

fn block_contexts(gr: &BlockGrid, x: i32, y: i32, z: i32, _run: u32, out: &mut [u32; BLOCK_MODELS]) -> u32 {
    let a = gr.g(x, y - 1, z);
    let aa = gr.g(x, y - 2, z);
    let w = gr.g(x - 1, y, z);
    let n = gr.g(x, y, z - 1);
    let d = gr.g(x - 1, y, z - 1);
    let ne = gr.g(x + 1, y, z - 1);
    let ww = gr.g(x - 2, y, z);
    let nn = gr.g(x, y, z - 2);
    let wa = gr.g(x - 1, y - 1, z);
    let na = gr.g(x, y - 1, z - 1);
    let ea = gr.g(x + 1, y - 1, z);
    let sa = gr.g(x, y - 1, z + 1);
    out[0] = hs(&[0]);
    out[1] = hs(&[1, a]);
    out[2] = hs(&[2, w, n]);
    out[3] = hs(&[3, a, w, n]);
    out[4] = hs(&[4, a, aa]);
    out[5] = hs(&[5, a, w, n, d, ne]);
    out[6] = hs(&[6, w, ww, n, nn]);
    out[7] = hs(&[7, a, wa, na, ea, sa]);
    out[8] = hs(&[8, a, (y >> 2) as u32]);
    a
}

struct BiomeGrid<'a> {
    v: &'a [u16],
    h: usize,
    sentinel: u32,
}

impl<'a> BiomeGrid<'a> {
    #[inline]
    fn g(&self, x: i32, y: i32, z: i32) -> u32 {
        if x < 0 || x >= 4 || z < 0 || z >= 4 || y < 0 || y >= self.h as i32 {
            self.sentinel
        } else {
            self.v[(y as usize) * 16 + (z as usize) * 4 + x as usize] as u32
        }
    }
}

fn biome_contexts(gr: &BiomeGrid, x: i32, y: i32, z: i32, out: &mut [u32; BIOME_MODELS]) -> u32 {
    let a = gr.g(x, y - 1, z);
    let w = gr.g(x - 1, y, z);
    let n = gr.g(x, y, z - 1);
    let d = gr.g(x - 1, y, z - 1);
    out[0] = hs(&[100]);
    out[1] = hs(&[101, a]);
    out[2] = hs(&[102, w, n]);
    out[3] = hs(&[103, a, w, n]);
    out[4] = hs(&[104, a, w, n, d]);
    a
}

// ---------------------------------------------------------------------------
// Кодек
// ---------------------------------------------------------------------------

fn encode_symbol(pred: &mut Predictor, enc: &mut Encoder, sym: u32, nbits: u32, hashes: &[u32], apm_cx: u32) {
    let mut node: u32 = 1;
    for i in (0..nbits).rev() {
        let bit = (sym >> i) & 1;
        let p = pred.predict(node, hashes, apm_cx);
        enc.encode(bit, p);
        pred.update(bit);
        node = (node << 1) | bit;
    }
}

fn decode_symbol(pred: &mut Predictor, dec: &mut Decoder, nbits: u32, hashes: &[u32], apm_cx: u32) -> u32 {
    let mut node: u32 = 1;
    for _ in 0..nbits {
        let p = pred.predict(node, hashes, apm_cx);
        let bit = dec.decode(p);
        pred.update(bit);
        node = (node << 1) | bit;
    }
    node - (1u32 << nbits)
}

impl Codec for CmXCodec {
    fn id(&self) -> String {
        "cm-x".to_string()
    }
    fn name(&self) -> String {
        "CM-X".to_string()
    }
    fn description(&self) -> String {
        "Контекстное моделирование с миксером 3D-контекстов и арифметическим кодером".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let ns = chunk.sections.len();
        let h = ns * 16;
        let bh = ns * 4;

        // --- shortcut: полностью воздушный чанк с одним биомом ---
        let all_air = chunk.sections.iter().all(|s| s.blocks.iter().all(|&b| b == 0))
            && chunk.sections.iter().all(|s| s.biomes.iter().all(|&b| b == s.biomes[0]))
            && chunk.sections.iter().all(|s| s.biomes[0] == chunk.sections[0].biomes[0]);
        if all_air {
            let mut out = vec![0xFEu8];
            put_varint(&mut out, ns as u64);
            put_varint(&mut out, zigzag(chunk.section_min_y as i64));
            put_varint(&mut out, chunk.sections[0].biomes[0] as u64);
            return out;
        }

        // --- палитры ---
        let mut bl_ids: Vec<u32> = Vec::new();
        let mut bi_ids: Vec<u16> = Vec::new();
        {
            let mut seen_b: HashMap<u32, ()> = HashMap::new();
            let mut seen_i: HashMap<u16, ()> = HashMap::new();
            for s in &chunk.sections {
                for &b in &s.blocks {
                    if seen_b.insert(b, ()).is_none() {
                        bl_ids.push(b);
                    }
                }
                for &b in &s.biomes {
                    if seen_i.insert(b, ()).is_none() {
                        bi_ids.push(b);
                    }
                }
            }
        }
        bl_ids.sort_unstable();
        bi_ids.sort_unstable();
        let bl_map: HashMap<u32, u16> = bl_ids.iter().enumerate().map(|(i, &v)| (v, i as u16)).collect();
        let bi_map: HashMap<u16, u16> = bi_ids.iter().enumerate().map(|(i, &v)| (v, i as u16)).collect();

        // --- заголовок ---
        let mut out = Vec::new();
        put_varint(&mut out, ns as u64);
        put_varint(&mut out, zigzag(chunk.section_min_y as i64));
        put_varint(&mut out, bl_ids.len() as u64);
        let mut prev: i64 = -1;
        for &id in &bl_ids {
            put_varint(&mut out, (id as i64 - prev - 1) as u64);
            prev = id as i64;
        }
        put_varint(&mut out, bi_ids.len() as u64);
        prev = -1;
        for &id in &bi_ids {
            put_varint(&mut out, (id as i64 - prev - 1) as u64);
            prev = id as i64;
        }
        // shortcut: все биомы во всех секциях одинаковые
        let bio_uniform_all = chunk.sections.iter().all(|s| {
            s.biomes.iter().all(|&b| b == s.biomes[0])
        }) && chunk.sections.iter().all(|s| s.biomes[0] == chunk.sections[0].biomes[0]);
        if bio_uniform_all {
            out.push(1);
            put_varint(&mut out, chunk.sections[0].biomes[0] as u64);
        } else {
            out.push(0);
        }
        if ns == 0 {
            return out;
        }

        // --- индексы ---
        let mut bl_idx = vec![0u16; h * 256];
        let mut bi_idx = vec![0u16; bh * 16];
        for (k, s) in chunk.sections.iter().enumerate() {
            for i in 0..4096 {
                bl_idx[k * 4096 + i] = bl_map[&s.blocks[i]];
            }
            for i in 0..64 {
                bi_idx[k * 64 + i] = bi_map[&s.biomes[i]];
            }
        }

        let mut enc = Encoder::new();

        // --- блоки ---
        let nb = bits_for(bl_ids.len());
        if nb > 0 {
            let nsets = (1usize << nb).min(4096);
            let mut pred = Predictor::new(BLOCK_MODELS, 20, nsets, 4096);
            let gr = BlockGrid { v: &bl_idx, h, sentinel: bl_ids.len() as u32 };
            let mut hashes = [0u32; BLOCK_MODELS];
            let mut run = 0u32;
            for y in 0..h as i32 {
                for z in 0..16i32 {
                    for x in 0..16i32 {
                        let a = block_contexts(&gr, x, y, z, run, &mut hashes);
                        let sym = bl_idx[(y as usize) * 256 + (z as usize) * 16 + x as usize] as u32;
                        encode_symbol(&mut pred, &mut enc, sym, nb, &hashes, a);
                        if sym == a {
                            run = (run + 1).min(7);
                        } else {
                            run = 0;
                        }
                    }
                }
            }
        }

        // --- биомы ---
        let nbi = bits_for(bi_ids.len());
        if nbi > 0 && !bio_uniform_all {
            let nsets = (1usize << nbi).min(4096);
            let mut pred = Predictor::new(BIOME_MODELS, 14, nsets, 256);
            let gr = BiomeGrid { v: &bi_idx, h: bh, sentinel: bi_ids.len() as u32 };
            let mut hashes = [0u32; BIOME_MODELS];
            for y in 0..bh as i32 {
                for z in 0..4i32 {
                    for x in 0..4i32 {
                        let a = biome_contexts(&gr, x, y, z, &mut hashes);
                        let sym = bi_idx[(y as usize) * 16 + (z as usize) * 4 + x as usize] as u32;
                        encode_symbol(&mut pred, &mut enc, sym, nbi, &hashes, a);
                    }
                }
            }
        }

        out.extend_from_slice(&enc.finish());
        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut pos = 0usize;
        if pos < bytes.len() && bytes[pos] == 0xFE {
            pos += 1;
            let ns = get_varint(bytes, &mut pos)? as usize;
            if ns > 1 << 16 {
                bail!("cm-x: unreasonable section count");
            }
            let min_y = unzigzag(get_varint(bytes, &mut pos)?) as i32;
            let bio = get_varint(bytes, &mut pos)? as u16;
            let mut sections = Vec::with_capacity(ns);
            for _ in 0..ns {
                sections.push(Section { blocks: vec![0u32; 4096], biomes: vec![bio; 64] });
            }
            return Ok(vec![Chunk { x: 0, z: 0, section_min_y: min_y, sections }]);
        }
        let ns = get_varint(bytes, &mut pos)? as usize;
        if ns > 1 << 16 {
            bail!("cm-x: unreasonable section count");
        }
        let section_min_y = unzigzag(get_varint(bytes, &mut pos)?) as i32;

        let nbl = get_varint(bytes, &mut pos)? as usize;
        if nbl > 1 << 20 {
            bail!("cm-x: unreasonable block palette size");
        }
        let mut bl_ids: Vec<u32> = Vec::with_capacity(nbl);
        let mut prev: i64 = -1;
        for _ in 0..nbl {
            let d = get_varint(bytes, &mut pos)? as i64;
            let id = prev + 1 + d;
            if id < 0 || id > u32::MAX as i64 {
                bail!("cm-x: bad block id in palette");
            }
            bl_ids.push(id as u32);
            prev = id;
        }

        let nbi = get_varint(bytes, &mut pos)? as usize;
        if nbi > 1 << 17 {
            bail!("cm-x: unreasonable biome palette size");
        }
        let mut bi_ids: Vec<u16> = Vec::with_capacity(nbi);
        prev = -1;
        for _ in 0..nbi {
            let d = get_varint(bytes, &mut pos)? as i64;
            let id = prev + 1 + d;
            if id < 0 || id > u16::MAX as i64 {
                bail!("cm-x: bad biome id in palette");
            }
            bi_ids.push(id as u16);
            prev = id;
        }

        let bio_uniform_all = if pos < bytes.len() && bytes[pos] == 1 {
            pos += 1;
            let id = get_varint(bytes, &mut pos)? as u16;
            Some(id)
        } else {
            if pos < bytes.len() && bytes[pos] == 0 {
                pos += 1;
            }
            None
        };
        if ns == 0 {
            return Ok(vec![Chunk { x: 0, z: 0, section_min_y, sections: Vec::new() }]);
        }
        if bl_ids.is_empty() || bi_ids.is_empty() {
            bail!("cm-x: empty palette with non-empty sections");
        }

        let h = ns * 16;
        let bh = ns * 4;
        let mut bl_idx = vec![0u16; h * 256];
        let mut bi_idx = vec![0u16; bh * 16];

        let mut dec = Decoder::new(&bytes[pos..]);

        let nb = bits_for(bl_ids.len());
        if nb > 0 {
            let nsets = (1usize << nb).min(4096);
            let mut pred = Predictor::new(BLOCK_MODELS, 20, nsets, 4096);
            let mut hashes = [0u32; BLOCK_MODELS];
            let mut run = 0u32;
            let sentinel = bl_ids.len() as u32;
            for y in 0..h as i32 {
                for z in 0..16i32 {
                    for x in 0..16i32 {
                        let a = {
                            let gr = BlockGrid { v: &bl_idx, h, sentinel };
                            block_contexts(&gr, x, y, z, run, &mut hashes)
                        };
                        let sym = decode_symbol(&mut pred, &mut dec, nb, &hashes, a);
                        if sym as usize >= bl_ids.len() {
                            bail!("cm-x: decoded block index out of palette");
                        }
                        bl_idx[(y as usize) * 256 + (z as usize) * 16 + x as usize] = sym as u16;
                        if sym == a {
                            run = (run + 1).min(7);
                        } else {
                            run = 0;
                        }
                    }
                }
            }
        }

        let nbib = bits_for(bi_ids.len());
        if nbib > 0 && bio_uniform_all.is_none() {
            let nsets = (1usize << nbib).min(4096);
            let mut pred = Predictor::new(BIOME_MODELS, 14, nsets, 256);
            let mut hashes = [0u32; BIOME_MODELS];
            let sentinel = bi_ids.len() as u32;
            for y in 0..bh as i32 {
                for z in 0..4i32 {
                    for x in 0..4i32 {
                        let a = {
                            let gr = BiomeGrid { v: &bi_idx, h: bh, sentinel };
                            biome_contexts(&gr, x, y, z, &mut hashes)
                        };
                        let sym = decode_symbol(&mut pred, &mut dec, nbib, &hashes, a);
                        if sym as usize >= bi_ids.len() {
                            bail!("cm-x: decoded biome index out of palette");
                        }
                        bi_idx[(y as usize) * 16 + (z as usize) * 4 + x as usize] = sym as u16;
                    }
                }
            }
        }

        let mut sections = Vec::with_capacity(ns);
        for k in 0..ns {
            let mut blocks = Vec::with_capacity(4096);
            for i in 0..4096 {
                blocks.push(bl_ids[bl_idx[k * 4096 + i] as usize]);
            }
            let mut biomes = Vec::with_capacity(64);
            if let Some(id) = bio_uniform_all {
                biomes.resize(64, id);
            } else {
                for i in 0..64 {
                    biomes.push(bi_ids[bi_idx[k * 64 + i] as usize]);
                }
            }
            sections.push(Section { blocks, biomes });
        }

        Ok(vec![Chunk { x: 0, z: 0, section_min_y, sections }])
    }
}