//! QuickPal — быстрый в декоде кодек чанков с хорошим сжатием.
//!
//! Формат: один LSB-first битовый поток (varint — LEB128).
//! Заголовок тайла: zigzag-varint(section_min_y), varint(число секций).
//! Далее для каждой секции: блоки (4096), затем биомы (64).
//!
//! Блоки секции — один varint-заголовок H (младшие 3 бита — тег):
//!   tag 0 uniform : payload = id блока, вся секция из него;
//!   tag 1 sparse  : payload = число исключений; затем varint(id фона) и
//!                   пары (varint дельта позиции, varint id) по возрастанию;
//!   tag 2 palette : payload = длина палитры; varint-иды; 4096 индексов по
//!                   ceil(log2(len)) бит;
//!   tag 3 pal+rle : как tag 2, но вместо индексов — раны (varint длина-1,
//!                   индекс на bits бит);
//!   tag 4 backref : payload = индекс предыдущей секции с идентичными
//!                   блоками (декод — clone).
//!
//! Биомы — varint-заголовок (младшие 2 бита): 0 uniform, 1 палитра+битпак
//! на 64 значения, 2 backref.
//!
//! Энтропийного кодирования нет: декод — memset/clone и сдвиги битов,
//! поэтому очень быстрый; сжатие хорошее на типичных чанках Minecraft
//! (воздух/камень — 1–2 байта на секцию, рельеф — палитрой/RLE).

use std::collections::{hash_map::Entry, HashMap};

use anyhow::{bail, Result};

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};

const SEC_BLOCKS: usize = 16 * 16 * 16; // 4096
const SEC_BIOMES: usize = 4 * 4 * 4; // 64

pub struct QuickPalCodec;

// ------------------------- вспомогательные -------------------------

#[inline]
fn bits_for(len: usize) -> u32 {
    if len <= 1 {
        0
    } else {
        32 - ((len - 1) as u32).leading_zeros()
    }
}

#[inline]
fn zigzag(v: i32) -> u64 {
    ((v << 1) ^ (v >> 31)) as u32 as u64
}

#[inline]
fn unzigzag(v: u64) -> i32 {
    let n = v as u32;
    ((n >> 1) as i32) ^ (-((n & 1) as i32))
}

/// Длина varint в битах (все varint пишутся 8-битными кусками).
#[inline]
fn varint_bits(v: u64) -> u64 {
    let mut n = 1u64;
    let mut x = v >> 7;
    while x != 0 {
        n += 1;
        x >>= 7;
    }
    n * 8
}

/// FNV-1a по блокам секции — быстрый префильтр для поиска дубликатов.
fn hash_blocks(blocks: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in blocks {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ------------------------- битовый писатель -------------------------

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    cnt: u32,
    total_bits: u64,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, cnt: 0, total_bits: 0 }
    }

    #[inline]
    fn put(&mut self, v: u32, n: u32) {
        if n == 0 {
            return;
        }
        let vv = (v as u64) & ((1u64 << n) - 1);
        self.acc |= vv << self.cnt;
        self.cnt += n;
        self.total_bits += n as u64;
        while self.cnt >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.cnt -= 8;
        }
    }

    fn put_varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u32;
            v >>= 7;
            if v == 0 {
                self.put(b, 8);
                return;
            }
            self.put(b | 0x80, 8);
        }
    }

    /// Дописать уже закодированный фрагмент, сохранив его битовую длину.
    fn append(&mut self, other: BitWriter) {
        let total = other.total_bits;
        let mut bytes = other.out;
        if other.cnt > 0 {
            bytes.push(other.acc as u8);
        }
        let full = (total / 8) as usize;
        let rem = (total % 8) as u32;
        for b in &bytes[..full] {
            self.put(*b as u32, 8);
        }
        if rem > 0 {
            let mask = ((1u64 << rem) - 1) as u32;
            self.put((bytes[full] as u32) & mask, rem);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.cnt > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

// ------------------------- битовый читатель -------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
    overrun: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, buf: 0, cnt: 0, overrun: false }
    }

    #[inline]
    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        while self.cnt < n && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
        if self.cnt < n {
            self.overrun = true;
            let v = (self.buf & ((1u64 << n) - 1)) as u32;
            self.buf = 0;
            self.cnt = 0;
            return v;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        v
    }

    fn get_varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut sh = 0u32;
        loop {
            let b = self.get(8) as u64;
            v |= (b & 0x7f) << sh;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            sh += 7;
            if sh >= 64 {
                bail!("varint too long");
            }
        }
    }
}

// ------------------------- энкодер: блоки секции -------------------------

fn encode_blocks(sec: &Section, hash: u64, seen: &[(u64, &Section)], w: &mut BitWriter) {
    let blocks: &[u32] = &sec.blocks;
    if blocks.is_empty() {
        w.put_varint(0); // вне контракта (ожидается 4096); uniform «air»
        return;
    }

    // предыдущая секция с идентичным содержимым? (hash — быстрый префильтр)
    let mut ref_j: Option<usize> = None;
    for (j, (h, s)) in seen.iter().enumerate() {
        if *h == hash && s.blocks[..] == blocks[..] {
            ref_j = Some(j);
            break;
        }
    }

    let first = blocks[0];

    // --- uniform: вся секция из одного блока
    if blocks.iter().all(|&b| b == first) {
        let uni_bits = varint_bits((first as u64) << 3);
        if let Some(j) = ref_j {
            let hdr = ((j as u64) << 3) | 4;
            if varint_bits(hdr) < uni_bits {
                w.put_varint(hdr);
                return;
            }
        }
        w.put_varint((first as u64) << 3);
        return;
    }

    // --- идентичная секция уже была: ссылка всегда короче любого кандидата
    //     (минимальный кандидат — 4 байта, ссылка — до 3)
    if let Some(j) = ref_j {
        w.put_varint(((j as u64) << 3) | 4);
        return;
    }

    // --- палитра в порядке первого появления
    let mut pal: Vec<u32> = Vec::new();
    let mut pidx: HashMap<u32, u32> = HashMap::with_capacity(32);
    let mut vals: Vec<u32> = Vec::with_capacity(blocks.len());
    for &b in blocks {
        let v = match pidx.entry(b) {
            Entry::Occupied(o) => *o.get(),
            Entry::Vacant(vac) => {
                let n = pal.len() as u32;
                pal.push(b);
                vac.insert(n);
                n
            }
        };
        vals.push(v);
    }
    let bits = bits_for(pal.len());

    // кандидат A: палитра + жёсткий битпак (tag 2)
    let mut raw = BitWriter::new();
    raw.put_varint(((pal.len() as u64) << 3) | 2);
    for &p in &pal {
        raw.put_varint(p as u64);
    }
    for &v in &vals {
        raw.put(v, bits);
    }

    // кандидат B: палитра + RLE по ранам (tag 3)
    let mut rle = BitWriter::new();
    rle.put_varint(((pal.len() as u64) << 3) | 3);
    for &p in &pal {
        rle.put_varint(p as u64);
    }
    let mut i = 0usize;
    while i < vals.len() {
        let v = vals[i];
        let mut j = i + 1;
        while j < vals.len() && vals[j] == v {
            j += 1;
        }
        rle.put_varint((j - i - 1) as u64);
        rle.put(v, bits);
        i = j;
    }

    // кандидат C: фон + исключения (tag 1)
    let mut sparse = BitWriter::new();
    {
        let mut freq: HashMap<u32, usize> = HashMap::with_capacity(pal.len());
        for &b in blocks {
            *freq.entry(b).or_insert(0) += 1;
        }
        // самый частый блок; при равенстве — меньший id (детерминизм)
        let mut bg = first;
        let mut bc = 0usize;
        for (&v, &c) in freq.iter() {
            if c > bc || (c == bc && v < bg) {
                bg = v;
                bc = c;
            }
        }
        let mut items: Vec<(u32, u32)> = Vec::new();
        for (p, &b) in blocks.iter().enumerate() {
            if b != bg {
                items.push((p as u32, b));
            }
        }
        sparse.put_varint(((items.len() as u64) << 3) | 1);
        sparse.put_varint(bg as u64);
        let mut prev = 0usize;
        for (t, &(idx, id)) in items.iter().enumerate() {
            let d = (idx as usize - prev) as u64;
            sparse.put_varint(if t == 0 { d } else { d - 1 });
            sparse.put_varint(id as u64);
            prev = idx as usize;
        }
    }

    // выбираем самый короткий (при равенстве — более простой к декоду)
    let mut best = raw;
    if sparse.total_bits < best.total_bits {
        best = sparse;
    }
    if rle.total_bits < best.total_bits {
        best = rle;
    }
    w.append(best);
}

// ------------------------- энкодер: биомы секции -------------------------

fn encode_biomes(sec: &Section, seen: &[&[u16]], w: &mut BitWriter) {
    let bs: &[u16] = &sec.biomes;
    if bs.is_empty() {
        w.put_varint(0);
        return;
    }
    let first = bs[0];

    let mut ref_j: Option<usize> = None;
    for (j, p) in seen.iter().enumerate() {
        if *p == bs {
            ref_j = Some(j);
            break;
        }
    }

    // uniform: весь биом один
    if bs.iter().all(|&b| b == first) {
        let uni = (first as u64) << 2;
        if let Some(j) = ref_j {
            let hdr = ((j as u64) << 2) | 2;
            if varint_bits(hdr) < varint_bits(uni) {
                w.put_varint(hdr);
                return;
            }
        }
        w.put_varint(uni);
        return;
    }

    if let Some(j) = ref_j {
        w.put_varint(((j as u64) << 2) | 2);
        return;
    }

    // палитра + битпак на 64 значения
    let mut pal: Vec<u16> = Vec::new();
    let mut pidx: HashMap<u16, u32> = HashMap::with_capacity(8);
    let mut vals: Vec<u32> = Vec::with_capacity(bs.len());
    for &b in bs {
        let v = match pidx.entry(b) {
            Entry::Occupied(o) => *o.get(),
            Entry::Vacant(vac) => {
                let n = pal.len() as u32;
                pal.push(b);
                vac.insert(n);
                n
            }
        };
        vals.push(v);
    }
    let bits = bits_for(pal.len());

    let mut cand = BitWriter::new();
    cand.put_varint(((pal.len() as u64) << 2) | 1);
    for &p in &pal {
        cand.put_varint(p as u64);
    }
    for &v in &vals {
        cand.put(v, bits);
    }
    w.append(cand);
}

// ------------------------- кодек -------------------------

impl Codec for QuickPalCodec {
    fn id(&self) -> String {
        "quickpal".to_string()
    }

    fn name(&self) -> String {
        "QuickPal".to_string()
    }

    fn description(&self) -> String {
        "Палитра на секцию + жёсткий битпак; на каждую секцию выбирается минимальный формат \
         (uniform / фон+исключения / палитра+RLE / ссылка на предыдущую секцию). \
         Без энтропийного кодирования: decode — это memset/clone и сдвиги битов, \
         поэтому очень быстрый; хорошо жмёт типичные чанки Minecraft."
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        if chunks.is_empty() {
            return Vec::new();
        }
        let ch = chunks[0];
        let mut w = BitWriter::new();
        w.put_varint(zigzag(ch.section_min_y));
        w.put_varint(ch.sections.len() as u64);
        let mut seen: Vec<(u64, &Section)> = Vec::with_capacity(ch.sections.len());
        let mut seen_b: Vec<&[u16]> = Vec::with_capacity(ch.sections.len());
        for sec in &ch.sections {
            let h = hash_blocks(&sec.blocks);
            encode_blocks(sec, h, &seen, &mut w);
            encode_biomes(sec, &seen_b, &mut w);
            seen.push((h, sec));
            seen_b.push(&sec.biomes);
        }
        w.finish()
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let mut r = BitReader::new(bytes);
        let min_y = unzigzag(r.get_varint()?);
        let n_sections = r.get_varint()? as usize;
        if r.overrun {
            bail!("truncated stream");
        }
        if n_sections > (1 << 16) {
            bail!("too many sections");
        }
        let mut sections: Vec<Section> = Vec::with_capacity(n_sections);

        for _ in 0..n_sections {
            // ---------------- блоки ----------------
            let head = r.get_varint()?;
            let tag = head & 7;
            let payload = head >> 3;
            let blocks: Vec<u32> = match tag {
                0 => {
                    if payload > u32::MAX as u64 {
                        bail!("bad block id");
                    }
                    vec![payload as u32; SEC_BLOCKS]
                }
                1 => {
                    let count = payload as usize;
                    if count > SEC_BLOCKS {
                        bail!("bad sparse count");
                    }
                    let bg = r.get_varint()?;
                    if bg > u32::MAX as u64 {
                        bail!("bad block id");
                    }
                    let mut blocks = vec![bg as u32; SEC_BLOCKS];
                    let mut prev = 0usize;
                    for t in 0..count {
                        let d = r.get_varint()?;
                        if d >= SEC_BLOCKS as u64 {
                            bail!("bad sparse index");
                        }
                        let idx = if t == 0 { d as usize } else { prev + 1 + d as usize };
                        if idx >= SEC_BLOCKS {
                            bail!("bad sparse index");
                        }
                        let id = r.get_varint()?;
                        if id > u32::MAX as u64 {
                            bail!("bad block id");
                        }
                        blocks[idx] = id as u32;
                        prev = idx;
                    }
                    blocks
                }
                2 | 3 => {
                    let plen = payload as usize;
                    if plen == 0 || plen > SEC_BLOCKS {
                        bail!("bad palette len");
                    }
                    let mut pal = Vec::with_capacity(plen);
                    for _ in 0..plen {
                        let id = r.get_varint()?;
                        if id > u32::MAX as u64 {
                            bail!("bad block id");
                        }
                        pal.push(id as u32);
                    }
                    let bits = bits_for(plen);
                    if tag == 2 {
                        let mut blocks = Vec::with_capacity(SEC_BLOCKS);
                        for _ in 0..SEC_BLOCKS {
                            let v = r.get(bits) as usize;
                            match pal.get(v) {
                                Some(&id) => blocks.push(id),
                                None => bail!("bad palette index"),
                            }
                        }
                        blocks
                    } else {
                        let mut blocks = vec![0u32; SEC_BLOCKS];
                        let mut pos = 0usize;
                        while pos < SEC_BLOCKS {
                            let l = r.get_varint()?;
                            if l >= SEC_BLOCKS as u64 {
                                bail!("bad run len");
                            }
                            let len = l as usize + 1;
                            if len > SEC_BLOCKS - pos {
                                bail!("bad run len");
                            }
                            let v = r.get(bits) as usize;
                            let id = match pal.get(v) {
                                Some(&id) => id,
                                None => bail!("bad palette index"),
                            };
                            blocks[pos..pos + len].fill(id);
                            pos += len;
                        }
                        blocks
                    }
                }
                4 => {
                    let j = payload as usize;
                    if j >= sections.len() {
                        bail!("bad backref");
                    }
                    sections[j].blocks.clone()
                }
                _ => bail!("bad section tag"),
            };

            // ---------------- биомы ----------------
            let head = r.get_varint()?;
            let tag = head & 3;
            let payload = head >> 2;
            let biomes: Vec<u16> = match tag {
                0 => {
                    if payload > u16::MAX as u64 {
                        bail!("bad biome id");
                    }
                    vec![payload as u16; SEC_BIOMES]
                }
                1 => {
                    let plen = payload as usize;
                    if plen == 0 || plen > SEC_BIOMES {
                        bail!("bad biome palette len");
                    }
                    let mut pal = Vec::with_capacity(plen);
                    for _ in 0..plen {
                        let id = r.get_varint()?;
                        if id > u16::MAX as u64 {
                            bail!("bad biome id");
                        }
                        pal.push(id as u16);
                    }
                    let bits = bits_for(plen);
                    let mut biomes = Vec::with_capacity(SEC_BIOMES);
                    for _ in 0..SEC_BIOMES {
                        let v = r.get(bits) as usize;
                        match pal.get(v) {
                            Some(&id) => biomes.push(id),
                            None => bail!("bad biome palette index"),
                        }
                    }
                    biomes
                }
                2 => {
                    let j = payload as usize;
                    if j >= sections.len() {
                        bail!("bad biome backref");
                    }
                    sections[j].biomes.clone()
                }
                _ => bail!("bad biome tag"),
            };

            sections.push(Section { blocks, biomes });
            if r.overrun {
                bail!("truncated stream");
            }
        }

        if r.overrun {
            bail!("truncated stream");
        }
        // остаток должен состоять только из паддинга (< 8 бит) —
        // гарантирует, что поток прочитан ровно до конца
        let unread = (r.data.len() - r.pos) as u64 * 8 + r.cnt as u64;
        if unread >= 8 {
            bail!("trailing data");
        }

        Ok(vec![Chunk { x: 0, z: 0, section_min_y: min_y, sections }])
    }
}

// ------------------------- регистрация -------------------------

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "quickpal",
    name: "QuickPal",
    group: "adaptive",
    description: "Адаптивная палитра+битпак (uniform/sparse/RLE/backref на секцию): \
                  декод без энтропийного декодера — очень быстрый, сжатие хорошее.",
    make: || Box::new(QuickPalCodec) as Box<dyn Codec>,
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

    fn sec_from(blocks: &[u32], biomes: &[u16]) -> Section {
        let mut s = Section::air();
        for (i, &b) in blocks.iter().enumerate() {
            s.blocks[i] = b;
        }
        for (i, &b) in biomes.iter().enumerate() {
            s.biomes[i] = b;
        }
        s
    }

    #[test]
    fn roundtrip_patterns() {
        let mut reg = BlockRegistry::new();
        let mut bio = BiomeRegistry::new();
        let mut ids = Vec::new();
        for i in 0..300 {
            ids.push(reg.intern(&format!("minecraft:b{i}")));
        }
        for i in 0..20 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };

        let air_biomes = vec![0u16; 64];
        let mut sections = Vec::new();

        // air
        sections.push(sec_from(&vec![0u32; 4096], &air_biomes));
        // uniform non-air
        sections.push(sec_from(&vec![7u32; 4096], &vec![1u16; 64]));
        // duplicate (backref)
        sections.push(sec_from(&vec![7u32; 4096], &vec![1u16; 64]));
        // 2-color checker (1bit / RLE)
        let mut two = vec![0u32; 4096];
        for i in 0..4096 {
            two[i] = (i % 2) as u32;
        }
        sections.push(sec_from(&two, &vec![0u16; 64]));
        // 16-color blocky
        let mut sixteen = vec![0u32; 4096];
        for i in 0..4096 {
            sixteen[i] = ((i / 64) % 16) as u32;
        }
        let mut bio16 = vec![0u16; 64];
        for i in 0..64 {
            bio16[i] = (i % 5) as u16;
        }
        sections.push(sec_from(&sixteen, &bio16));
        // noise with >16 colors (8bit)
        let mut rng = 999u64;
        let mut noise = vec![0u32; 4096];
        for v in noise.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = 20 + (rng % 60) as u32;
        }
        sections.push(sec_from(&noise, &vec![2u16; 64]));
        // sparse-ish background + exceptions
        let mut sparse = vec![5u32; 4096];
        for i in (0..4096).step_by(11) {
            sparse[i] = 9;
        }
        sections.push(sec_from(&sparse, &vec![0u16; 64]));
        // >256 unique (RAW)
        let mut huge = vec![0u32; 4096];
        for i in 0..4096 {
            huge[i] = 300 + (i % 400) as u32;
        }
        sections.push(sec_from(&huge, &vec![0u16; 64]));

        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = QuickPalCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}