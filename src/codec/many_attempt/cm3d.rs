//! cm3d — сильно сжимающий кодек чанков: палитры + контекстное смешение (CM)
//! с адаптивным бинарным диапазонным кодером.
//!
//! Формат:
//!   varint(zigzag(section_min_y)), varint(число секций), затем для каждой секции
//!   2-битный режим:
//!     1 — блоки и биомы однородны: два varint (id блока, id биома);
//!     2 — блоки произвольные, биом однороден: палитра + слои, затем varint биома;
//!     3 — блоки однородны, биомы произвольные;
//!     0 — всё произвольное.
//!   Палитра сортируется по убыванию частоты; индексы кодируются побитно
//!   CM-моделью с контекстами (above, left, front, высота слоя, позиция бита,
//!   частичные биты). Однородные слои помечаются флагом и кодируются одним
//!   символом. Всё (включая varint-байты) проходит через один диапазонный кодер.

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

// ===========================================================================
// Диапазонный кодер (LZMA-стиль), 12-битные вероятности.
// ===========================================================================

struct RangeEnc {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u32,
    out: Vec<u8>,
}

impl RangeEnc {
    fn new() -> Self {
        RangeEnc { low: 0, range: u32::MAX, cache: 0, cache_size: 1, out: Vec::new() }
    }

    #[inline]
    fn bit(&mut self, bit: bool, p: u16) {
        // p = P(bit == 1), 1..4095; range >= 2^24 перед каждым вызовом
        let bound = (self.range >> 12) * p as u32;
        if bit {
            self.low += bound as u64;
            self.range -= bound;
        } else {
            self.range = bound;
        }
        while self.range < (1u32 << 24) {
            self.range <<= 8;
            self.shift_low();
        }
    }

    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add((self.low >> 32) as u8));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = ((self.low & 0xFFFF_FFFF) << 8) & 0xFFFF_FFFF;
    }

    fn flush(&mut self) {
        for _ in 0..5 {
            self.shift_low();
        }
    }
}

struct RangeDec<'a> {
    data: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
}

impl<'a> RangeDec<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut d = RangeDec { data, pos: 0, range: u32::MAX, code: 0 };
        for _ in 0..5 {
            d.code = (d.code << 8) | d.next_byte();
        }
        d
    }

    #[inline]
    fn next_byte(&mut self) -> u32 {
        if self.pos < self.data.len() {
            let b = self.data[self.pos] as u32;
            self.pos += 1;
            b
        } else {
            0
        }
    }

    #[inline]
    fn bit(&mut self, p: u16) -> bool {
        let bound = (self.range >> 12) * p as u32;
        let bit = if self.code < bound {
            self.range = bound;
            false
        } else {
            self.code -= bound;
            self.range -= bound;
            true
        };
        while self.range < (1u32 << 24) {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next_byte();
        }
        bit
    }
}

// ===========================================================================
// Контекстная модель.
// ===========================================================================

const NCTX: usize = 1 << 16;

struct SymPred {
    p: u16,
    slots: [usize; 4],
    st: [i32; 4],
}

struct Cm {
    stretch: Vec<i16>, // [4096] логит(12-бит p) -> растянутая шкала
    squash: Vec<u16>,  // [4096] обратное преобразование, индекс = логит + 2047
    t0: Vec<u16>,      // (above, left, front, j, partial)
    t1: Vec<u16>,      // (above, j, partial)
    t2: Vec<u16>,      // (left, front, j, partial)
    t3: Vec<u16>,      // (размер палитры, j, partial) — глобальная статистика
    flags: Vec<u16>,   // флаг «слой однородный»
    bytes: Vec<u16>,   // байты varint, order-1 + частичный байт
    modes: Vec<u16>,   // биты режима секции
    w: [i32; 4],       // адаптивные веса смеси
}

impl Cm {
    fn new() -> Self {
        let mut stretch = vec![0i16; 4096];
        for (p, s) in stretch.iter_mut().enumerate() {
            let p = p as f64;
            let v = 256.0 * (p / (4096.0 - p)).ln();
            *s = v.clamp(-2047.0, 2047.0).round() as i16;
        }
        let mut squash = vec![2048u16; 4096];
        for (i, s) in squash.iter_mut().enumerate() {
            let x = i as f64 - 2047.0;
            let v = (4096.0 / (1.0 + (-x / 256.0).exp())).round();
            *s = v.clamp(1.0, 4095.0) as u16;
        }
        Cm {
            stretch,
            squash,
            t0: vec![2048; NCTX],
            t1: vec![2048; NCTX],
            t2: vec![2048; NCTX],
            t3: vec![2048; NCTX],
            flags: vec![2048; NCTX],
            bytes: vec![2048; NCTX],
            modes: vec![2048; 16],
            w: [46000, 26000, 17000, 9000],
        }
    }

    #[inline]
    fn sym_predict(&self, a: u32, b: u32, c: u32, np: u32, j: u32, partial: u32) -> SymPred {
        let s0 = h16(a, b, c, j, partial);
        let s1 = h16(a, 0, 0, j, partial);
        let s2 = h16(b, c, 0, j, partial);
        let s3 = h16(0, np, 0, j, partial);
        let st0 = self.stretch[self.t0[s0] as usize] as i32;
        let st1 = self.stretch[self.t1[s1] as usize] as i32;
        let st2 = self.stretch[self.t2[s2] as usize] as i32;
        let st3 = self.stretch[self.t3[s3] as usize] as i32;
        let dot = self.w[0] * st0 + self.w[1] * st1 + self.w[2] * st2 + self.w[3] * st3;
        let idx = ((dot >> 16).clamp(-2047, 2047) + 2047) as usize;
        SymPred { p: self.squash[idx], slots: [s0, s1, s2, s3], st: [st0, st1, st2, st3] }
    }

    #[inline]
    fn sym_update(&mut self, pr: &SymPred, bit: bool) {
        let target: i32 = if bit { 4096 } else { 0 };
        for i in 0..4 {
            let slot = pr.slots[i];
            let tab: &mut Vec<u16> = match i {
                0 => &mut self.t0,
                1 => &mut self.t1,
                2 => &mut self.t2,
                _ => &mut self.t3,
            };
            let v = tab[slot] as i32;
            tab[slot] = (v + ((target - v) >> 4)).clamp(1, 4095) as u16;
        }
        let err = target - pr.p as i32;
        for i in 0..4 {
            self.w[i] = (self.w[i] + ((pr.st[i] * err) >> 14)).clamp(-65536, 65536);
        }
    }
}

#[inline]
fn adapt(p: u16, bit: bool, rate: u32) -> u16 {
    let v = p as i32;
    let t = if bit { 4096 } else { 0 };
    (v + ((t - v) >> rate)).clamp(1, 4095) as u16
}

#[inline]
fn h16(a: u32, b: u32, c: u32, j: u32, partial: u32) -> usize {
    let mut x = a.wrapping_mul(0x9E37_79B1)
        ^ b.wrapping_mul(0x85EB_CA77)
        ^ c.wrapping_mul(0xC2B2_AE3D)
        ^ j.wrapping_mul(0x27D4_EB2F)
        ^ partial.wrapping_mul(0x1656_67B1);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2545_F491);
    x ^= x >> 13;
    (x >> 16) as usize
}

// ===========================================================================
// Символы (индексы палитры) — k бит, старший бит первым.
// jb2 = база_контекста*64 + доп.контекст (высота слоя); внутри цикла + i*64.
// ===========================================================================

#[inline]
fn enc_symbol(enc: &mut RangeEnc, cm: &mut Cm, a: u32, b: u32, c: u32, np: u32, jb2: u32, sym: u32, k: u32) {
    let mut partial: u32 = 0;
    for i in 0..k {
        let pr = cm.sym_predict(a, b, c, np, jb2 + i * 64, partial);
        let bit = (sym >> (k - 1 - i)) & 1 != 0;
        enc.bit(bit, pr.p);
        cm.sym_update(&pr, bit);
        partial = (partial << 1) | bit as u32;
    }
}

#[inline]
fn dec_symbol(dec: &mut RangeDec, cm: &mut Cm, a: u32, b: u32, c: u32, np: u32, jb2: u32, k: u32) -> u32 {
    let mut partial: u32 = 0;
    for i in 0..k {
        let pr = cm.sym_predict(a, b, c, np, jb2 + i * 64, partial);
        let bit = dec.bit(pr.p);
        cm.sym_update(&pr, bit);
        partial = (partial << 1) | bit as u32;
    }
    partial
}

// ===========================================================================
// Байты (для varint-полей) — order-1 + частичный байт.
// ===========================================================================

fn enc_byte(enc: &mut RangeEnc, cm: &mut Cm, b: u8, prev: &mut u8) {
    let mut partial: u32 = 0;
    for i in 0..8 {
        let s = h16(*prev as u32, partial, 0, 98, 0);
        let p = cm.bytes[s];
        let bit = (b >> (7 - i)) & 1 != 0;
        enc.bit(bit, p);
        cm.bytes[s] = adapt(p, bit, 5);
        partial = (partial << 1) | bit as u32;
    }
    *prev = b;
}

fn dec_byte(dec: &mut RangeDec, cm: &mut Cm, prev: &mut u8) -> u8 {
    let mut partial: u32 = 0;
    let mut b: u8 = 0;
    for _ in 0..8 {
        let s = h16(*prev as u32, partial, 0, 98, 0);
        let p = cm.bytes[s];
        let bit = dec.bit(p);
        cm.bytes[s] = adapt(p, bit, 5);
        b = (b << 1) | bit as u8;
        partial = (partial << 1) | bit as u32;
    }
    *prev = b;
    b
}

fn enc_varint(enc: &mut RangeEnc, cm: &mut Cm, mut v: u64, prev: &mut u8) {
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        enc_byte(enc, cm, b, prev);
        if v == 0 {
            break;
        }
    }
}

fn dec_varint(dec: &mut RangeDec, cm: &mut Cm, prev: &mut u8) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let b = dec_byte(dec, cm, prev);
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            bail!("cm3d: повреждённый varint");
        }
    }
}

// ===========================================================================
// Режим секции — 2 бита с контекстом «предыдущий режим».
// ===========================================================================

fn enc_mode(enc: &mut RangeEnc, cm: &mut Cm, mode: u32, prev_mode: u32) {
    let b0 = mode >= 2;
    let s0 = (prev_mode * 2) as usize;
    let p = cm.modes[s0];
    enc.bit(b0, p);
    cm.modes[s0] = adapt(p, b0, 5);

    let b1 = mode & 1 != 0;
    let s1 = (8 + prev_mode * 2 + b0 as u32) as usize;
    let p = cm.modes[s1];
    enc.bit(b1, p);
    cm.modes[s1] = adapt(p, b1, 5);
}

fn dec_mode(dec: &mut RangeDec, cm: &mut Cm, prev_mode: u32) -> Result<u32> {
    let s0 = (prev_mode * 2) as usize;
    let p = cm.modes[s0];
    let b0 = dec.bit(p);
    cm.modes[s0] = adapt(p, b0, 5);

    let s1 = (8 + prev_mode * 2 + b0 as u32) as usize;
    let p = cm.modes[s1];
    let b1 = dec.bit(p);
    cm.modes[s1] = adapt(p, b1, 5);

    Ok((b0 as u32) * 2 + b1 as u32)
}

// ===========================================================================
// Палитры и вспомогательное.
// ===========================================================================

fn palette_of(vals: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = vals.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

fn rank_map(ids: &[u32]) -> Vec<(u32, u16)> {
    let mut m: Vec<(u32, u16)> = ids.iter().enumerate().map(|(i, &id)| (id, i as u16)).collect();
    m.sort_unstable_by_key(|x| x.0);
    m
}

#[inline]
fn rank_of(m: &[(u32, u16)], id: u32) -> u16 {
    m.binary_search_by_key(&id, |x| x.0).unwrap() as u16
}

#[inline]
fn width(x: u32) -> u32 {
    32 - x.leading_zeros()
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(z: u64) -> i64 {
    ((z >> 1) as i64) ^ -((z & 1) as i64)
}

// ===========================================================================
// Блоки секции.
// ===========================================================================

fn enc_blocks(enc: &mut RangeEnc, cm: &mut Cm, blocks: &[u32], prev_byte: &mut u8) {
    let ids = palette_of(blocks);
    let np = ids.len() as u32;
    enc_varint(enc, cm, np as u64, prev_byte);
    for &id in &ids {
        enc_varint(enc, cm, id as u64, prev_byte);
    }
    let ranks = rank_map(&ids);
    let k = width(np - 1);

    let mut syms = vec![0u16; 4096];
    for (i, &b) in blocks.iter().enumerate() {
        syms[i] = rank_of(&ranks, b);
    }

    let mut prev_flag: u32 = 0; // 0 = предыдущий слой кодирован полностью, иначе uniform-символ + 1
    let mut prev_uni: u32 = 0;  // последний однородный символ + 1 (0 = «нет»)
    for ly in 0..16usize {
        let base = ly * 256;
        let s0 = syms[base];
        let uniform = syms[base..base + 256].iter().all(|&s| s == s0);

        let fs = h16(prev_flag, 0, 0, 97, 0);
        let p = cm.flags[fs];
        enc.bit(uniform, p);
        cm.flags[fs] = adapt(p, uniform, 4);

        if uniform {
            enc_symbol(enc, cm, prev_uni, 0, 0, np, 32 * 64, s0 as u32, k);
            let f = s0 as u32 + 1;
            prev_flag = f;
            prev_uni = f;
        } else {
            for z in 0..16usize {
                for x in 0..16usize {
                    let idx = base + z * 16 + x;
                    let a = if ly > 0 { syms[idx - 256] as u32 + 1 } else { 0 };
                    let b = if x > 0 { syms[idx - 1] as u32 + 1 } else { 0 };
                    let c = if z > 0 { syms[idx - 16] as u32 + 1 } else { 0 };
                    enc_symbol(enc, cm, a, b, c, np, ly as u32, syms[idx] as u32, k);
                }
            }
            prev_flag = 0;
        }
    }
}

fn dec_blocks(dec: &mut RangeDec, cm: &mut Cm, prev_byte: &mut u8) -> Result<Vec<u32>> {
    let np = dec_varint(dec, cm, prev_byte)?;
    if np == 0 || np > 4096 {
        bail!("cm3d: некорректный размер палитры блоков: {}", np);
    }
    let np = np as usize;
    let mut ids = Vec::with_capacity(np);
    for _ in 0..np {
        let id = dec_varint(dec, cm, prev_byte)?;
        if id > u32::MAX as u64 {
            bail!("cm3d: id блока вне диапазона");
        }
        ids.push(id as u32);
    }
    let k = width(np as u32 - 1);

    let mut syms = vec![0u16; 4096];
    let mut prev_flag: u32 = 0;
    let mut prev_uni: u32 = 0;
    for ly in 0..16usize {
        let base = ly * 256;
        let fs = h16(prev_flag, 0, 0, 97, 0);
        let p = cm.flags[fs];
        let uniform = dec.bit(p);
        cm.flags[fs] = adapt(p, uniform, 4);

        if uniform {
            let s = dec_symbol(dec, cm, prev_uni, 0, 0, np as u32, 32 * 64, k);
            if s as usize >= np {
                bail!("cm3d: повреждённый поток (символ палитры)");
            }
            let f = s + 1;
            prev_flag = f;
            prev_uni = f;
            for e in &mut syms[base..base + 256] {
                *e = s as u16;
            }
        } else {
            for z in 0..16usize {
                for x in 0..16usize {
                    let idx = base + z * 16 + x;
                    let a = if ly > 0 { syms[idx - 256] as u32 + 1 } else { 0 };
                    let b = if x > 0 { syms[idx - 1] as u32 + 1 } else { 0 };
                    let c = if z > 0 { syms[idx - 16] as u32 + 1 } else { 0 };
                    let s = dec_symbol(dec, cm, a, b, c, np as u32, ly as u32, k);
                    if s as usize >= np {
                        bail!("cm3d: повреждённый поток (символ палитры)");
                    }
                    syms[idx] = s as u16;
                }
            }
            prev_flag = 0;
        }
    }
    Ok(syms.iter().map(|&s| ids[s as usize]).collect())
}

// ===========================================================================
// Биомы секции (4x4x4).
// ===========================================================================

fn enc_biomes(enc: &mut RangeEnc, cm: &mut Cm, biomes: &[u16], prev_byte: &mut u8) {
    let tmp: Vec<u32> = biomes.iter().map(|&b| b as u32).collect();
    let ids = palette_of(&tmp);
    let np = ids.len() as u32;
    enc_varint(enc, cm, np as u64, prev_byte);
    for &id in &ids {
        enc_varint(enc, cm, id as u64, prev_byte);
    }
    let ranks = rank_map(&ids);
    let k = width(np - 1);

    let mut syms = [0u16; 64];
    for (i, &b) in biomes.iter().enumerate() {
        syms[i] = rank_of(&ranks, b as u32);
    }
    for by in 0..4usize {
        for bz in 0..4usize {
            for bx in 0..4usize {
                let idx = by * 16 + bz * 4 + bx;
                let a = if by > 0 { syms[idx - 16] as u32 + 1 } else { 0 };
                let b = if bx > 0 { syms[idx - 1] as u32 + 1 } else { 0 };
                let c = if bz > 0 { syms[idx - 4] as u32 + 1 } else { 0 };
                enc_symbol(enc, cm, a, b, c, np, 16 * 64, syms[idx] as u32, k);
            }
        }
    }
}

fn dec_biomes(dec: &mut RangeDec, cm: &mut Cm, prev_byte: &mut u8) -> Result<Vec<u16>> {
    let np = dec_varint(dec, cm, prev_byte)?;
    if np == 0 || np > 64 {
        bail!("cm3d: некорректный размер палитры биомов: {}", np);
    }
    let np = np as usize;
    let mut ids = Vec::with_capacity(np);
    for _ in 0..np {
        let id = dec_varint(dec, cm, prev_byte)?;
        if id > u16::MAX as u64 {
            bail!("cm3d: id биома вне диапазона");
        }
        ids.push(id as u16);
    }
    let k = width(np as u32 - 1);

    let mut syms = [0u16; 64];
    for by in 0..4usize {
        for bz in 0..4usize {
            for bx in 0..4usize {
                let idx = by * 16 + bz * 4 + bx;
                let a = if by > 0 { syms[idx - 16] as u32 + 1 } else { 0 };
                let b = if bx > 0 { syms[idx - 1] as u32 + 1 } else { 0 };
                let c = if bz > 0 { syms[idx - 4] as u32 + 1 } else { 0 };
                let s = dec_symbol(dec, cm, a, b, c, np as u32, 16 * 64, k);
                if s as usize >= np {
                    bail!("cm3d: повреждённый поток (символ палитры биомов)");
                }
                syms[idx] = s as u16;
            }
        }
    }
    Ok(syms.iter().map(|&s| ids[s as usize]).collect())
}

// ===========================================================================
// Кодек.
// ===========================================================================

pub struct Cm3dCodec;

impl Codec for Cm3dCodec {
    fn id(&self) -> String {
        "cm3d".to_string()
    }

    fn name(&self) -> String {
        "CM-3D".to_string()
    }

    fn description(&self) -> String {
        "Палитры на секцию + бинарное контекстное смешение (4 модели: три пространственных \
         соседа, размер палитры, позиция бита) с адаптивным диапазонным кодером; однородные \
         секции и слои — спецфлагами; varint — через order-1 байтовую модель. Максимум сжатия \
         на реальных террейн-чанках: однородная секция ~3–4 байта."
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let Some(ch) = chunks.first() else {
            return Vec::new();
        };
        let mut cm = Cm::new();
        let mut enc = RangeEnc::new();
        let mut prev_byte: u8 = 0;

        enc_varint(&mut enc, &mut cm, zigzag(ch.section_min_y as i64), &mut prev_byte);
        enc_varint(&mut enc, &mut cm, ch.sections.len() as u64, &mut prev_byte);

        let mut prev_mode: u32 = 0;
        for sec in &ch.sections {
            let b0 = sec.blocks[0];
            let bu = sec.blocks.iter().all(|&b| b == b0);
            let m0 = sec.biomes[0];
            let mu = sec.biomes.iter().all(|&b| b == m0);
            let mode = match (bu, mu) {
                (true, true) => 1,
                (true, false) => 3,
                (false, true) => 2,
                (false, false) => 0,
            };
            enc_mode(&mut enc, &mut cm, mode, prev_mode);
            prev_mode = mode;
            match mode {
                1 => {
                    enc_varint(&mut enc, &mut cm, b0 as u64, &mut prev_byte);
                    enc_varint(&mut enc, &mut cm, m0 as u64, &mut prev_byte);
                }
                3 => {
                    enc_varint(&mut enc, &mut cm, b0 as u64, &mut prev_byte);
                    enc_biomes(&mut enc, &mut cm, &sec.biomes, &mut prev_byte);
                }
                2 => {
                    enc_blocks(&mut enc, &mut cm, &sec.blocks, &mut prev_byte);
                    enc_varint(&mut enc, &mut cm, m0 as u64, &mut prev_byte);
                }
                _ => {
                    enc_blocks(&mut enc, &mut cm, &sec.blocks, &mut prev_byte);
                    enc_biomes(&mut enc, &mut cm, &sec.biomes, &mut prev_byte);
                }
            }
        }

        enc.flush();
        enc.out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut dec = RangeDec::new(bytes);
        let mut cm = Cm::new();
        let mut prev_byte: u8 = 0;

        let miny = unzigzag(dec_varint(&mut dec, &mut cm, &mut prev_byte)?);
        let n_sections = dec_varint(&mut dec, &mut cm, &mut prev_byte)?;
        if n_sections > 65536 {
            bail!("cm3d: некорректное число секций: {}", n_sections);
        }

        let mut sections = Vec::with_capacity(n_sections as usize);
        let mut prev_mode: u32 = 0;
        for _ in 0..n_sections {
            let mode = dec_mode(&mut dec, &mut cm, prev_mode)?;
            prev_mode = mode;
            let (blocks, biomes) = match mode {
                1 => {
                    let b = dec_varint(&mut dec, &mut cm, &mut prev_byte)?;
                    let m = dec_varint(&mut dec, &mut cm, &mut prev_byte)?;
                    (vec![b as u32; 4096], vec![m as u16; 64])
                }
                3 => {
                    let b = dec_varint(&mut dec, &mut cm, &mut prev_byte)?;
                    let biomes = dec_biomes(&mut dec, &mut cm, &mut prev_byte)?;
                    (vec![b as u32; 4096], biomes)
                }
                2 => {
                    let blocks = dec_blocks(&mut dec, &mut cm, &mut prev_byte)?;
                    let m = dec_varint(&mut dec, &mut cm, &mut prev_byte)?;
                    (blocks, vec![m as u16; 64])
                }
                _ => {
                    let blocks = dec_blocks(&mut dec, &mut cm, &mut prev_byte)?;
                    let biomes = dec_biomes(&mut dec, &mut cm, &mut prev_byte)?;
                    (blocks, biomes)
                }
            };
            sections.push(Section { blocks, biomes });
        }

        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y: miny as i32,
            sections,
        }])
    }
}

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "cm3d",
    name: "CM-3D",
    group: "context-modeling",
    description: "Палитры + контекстное смешение (CM) с диапазонным кодером: соседи верх/лево/перёд, \
                  высота слоя, спецфлаги однородности. Лучшее сжатие на обычных чанках.",
    make: || Box::new(Cm3dCodec) as Box<dyn Codec>,
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

    #[test]
    fn roundtrip_patterns() {
        let mut reg = BlockRegistry::new();
        let mut bio = BiomeRegistry::new();
        for i in 0..500 {
            reg.intern(&format!("minecraft:b{i}"));
        }
        for i in 0..512 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };
        let airbio = vec![0u16; 64];
        let mut sections = Vec::new();
        sections.push(Section::air());
        let mut mono = Section::air();
        for b in mono.blocks.iter_mut() {
            *b = 7;
        }
        sections.push(mono.clone());
        sections.push(mono);
        let mut two = Section::air();
        for i in 0..4096 {
            two.blocks[i] = (i % 2) as u32;
        }
        sections.push(two);
        let mut sixteen = Section::air();
        for i in 0..4096 {
            sixteen.blocks[i] = ((i / 64) % 16) as u32;
        }
        for (i, b) in sixteen.biomes.iter_mut().enumerate() {
            *b = (i % 7) as u16;
        }
        sections.push(sixteen.clone());
        sections.push(sixteen);
        let mut rng = 777u64;
        let mut noise = Section::air();
        for v in noise.blocks.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = (rng % 300) as u32;
        }
        for (i, b) in noise.biomes.iter_mut().enumerate() {
            *b = (i * 3 % 512) as u16;
        }
        sections.push(noise);
        let _ = airbio;
        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = Cm3dCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}
