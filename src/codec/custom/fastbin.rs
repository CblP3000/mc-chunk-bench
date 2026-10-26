//! fastbin — byte-aligned codec tuned for very fast decode.
//!
//! Design principles:
//! * byte-aligned stream (no interleaved bit-level varints), so decode is
//!   mostly memcpy / slice-fill / unrolled bit-unpacking;
//! * per-section modes with the cheapest decode path first:
//!   0 = all-air section  (memset)
//!   1 = uniform single block id (fill)
//!   2 = exact copy of an earlier section (clone)
//!   3 = palette + bit-packed indices (unrolled LUT decode)
//!   4 = palette + byte-aligned RLE (slice fill)
//! * same cheap mode set for biomes (64 cells).
//!
//! The encoder picks the smallest representation, but strongly prefers modes
//! with zero/cheap decode work (air, uniform, copy) whenever they are not
//! bigger than palette modes.

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

const SEC_BLOCKS: usize = 4096;
const SEC_BIOMES: usize = 64;

pub struct FastBinCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "fastbin",
    name: "FastBin",
    group: "adaptive",
    description: "Байт-выровненный формат с дешёвыми режимами декода: air/uniform/копия секции/палитра+битпак/RLE. Decode — почти чистый memcpy и slice-fill.",
    make: || Box::new(FastBinCodec) as Box<dyn Codec>,
}];

// ---------------------------------------------------------------------------
// varint helpers
// ---------------------------------------------------------------------------

#[inline(always)]
fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline(always)]
fn get_varint(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        if *pos >= bytes.len() {
            bail!("fastbin: unexpected EOF in varint");
        }
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift >= 35 {
            bail!("fastbin: varint too long");
        }
    }
}

#[inline(always)]
fn bits_for(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

// ---------------------------------------------------------------------------
// palette + bit-packed indices (width in {1,2,4,8,16,32})
// ---------------------------------------------------------------------------

fn put_bitpacked(out: &mut Vec<u8>, vals: &[u32], bits: u32) {
    if bits == 0 {
        return;
    }
    let n = vals.len();
    let total_bits = n as u64 * bits as u64;
    let total_bytes = total_bits.div_ceil(8) as usize;
    let start = out.len();
    out.resize(start + total_bytes, 0);
    let mut bit_off = 0u64;
    for &v in vals {
        let byte_idx = start + (bit_off / 8) as usize;
        let bit_idx = (bit_off % 8) as u32;
        let mut val = v;
        let mut remaining = bits;
        let mut bo = bit_idx;
        let mut bi = byte_idx;
        while remaining > 0 {
            let space = 8 - bo;
            let take = remaining.min(space);
            out[bi] |= ((val & ((1u32 << take) - 1)) as u8) << bo;
            val >>= take;
            remaining -= take;
            bo = 0;
            bi += 1;
        }
        bit_off += bits as u64;
    }
}

/// Decode `count` raw indices of `bits` (no palette mapping).
fn get_indices(bytes: &[u8], pos: &mut usize, count: usize, bits: u32, out: &mut [u32]) -> Result<()> {
    let total_bits = count as u64 * bits as u64;
    let total_bytes = total_bits.div_ceil(8) as usize;
    if *pos + total_bytes > bytes.len() {
        bail!("fastbin: truncated bitpacked data");
    }
    let src = &bytes[*pos..*pos + total_bytes];
    match bits {
        1 => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = ((src[i >> 3] >> (i & 7)) & 1) as u32;
            }
        }
        2 => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = ((src[i >> 2] >> ((i & 3) * 2)) & 3) as u32;
            }
        }
        4 => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = ((src[i >> 1] >> ((i & 1) * 4)) & 15) as u32;
            }
        }
        8 => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = src[i] as u32;
            }
        }
        _ => {
            let mask = (1u32 << bits) - 1;
            let mut bit_off = 0u64;
            for o in out.iter_mut() {
                let mut v = 0u32;
                let mut remaining = bits;
                let mut bo = (bit_off % 8) as u32;
                let mut bi = (bit_off / 8) as usize;
                let mut shift = 0u32;
                while remaining > 0 {
                    let space = 8 - bo;
                    let take = remaining.min(space);
                    v |= (((src[bi] >> bo) as u32) & ((1u32 << take) - 1)) << shift;
                    remaining -= take;
                    shift += take;
                    bo = 0;
                    bi += 1;
                }
                *o = v & mask;
                bit_off += bits as u64;
            }
        }
    }
    *pos += total_bytes;
    Ok(())
}

/// Decode `count` palette indices of `bits` directly into final block values.
/// One pass: index decode + palette lookup combined (no separate remap).
/// Only valid indices can appear (our encoder writes them), so no per-element
/// bounds checks are needed; the stream length is checked up-front.
fn get_palette_final(bytes: &[u8], pos: &mut usize, count: usize, bits: u32, pal: &[u32]) -> Result<Vec<u32>> {
    let total_bits = count as u64 * bits as u64;
    let total_bytes = total_bits.div_ceil(8) as usize;
    if *pos + total_bytes > bytes.len() {
        bail!("fastbin: truncated bitpacked data");
    }
    let src = &bytes[*pos..*pos + total_bytes];
    let mut out = Vec::with_capacity(count);
    match bits {
        1 => {
            for i in 0..count {
                out.push(pal[((src[i >> 3] >> (i & 7)) & 1) as usize]);
            }
        }
        2 => {
            for i in 0..count {
                out.push(pal[((src[i >> 2] >> ((i & 3) * 2)) & 3) as usize]);
            }
        }
        4 => {
            for i in 0..count {
                out.push(pal[((src[i >> 1] >> ((i & 1) * 4)) & 15) as usize]);
            }
        }
        8 => {
            for i in 0..count {
                out.push(pal[src[i] as usize]);
            }
        }
        _ => {
            let mask = (1u32 << bits) - 1;
            let mut bit_off = 0u64;
            for _ in 0..count {
                let mut v = 0u32;
                let mut remaining = bits;
                let mut bo = (bit_off % 8) as u32;
                let mut bi = (bit_off / 8) as usize;
                let mut shift = 0u32;
                while remaining > 0 {
                    let space = 8 - bo;
                    let take = remaining.min(space);
                    v |= (((src[bi] >> bo) as u32) & ((1u32 << take) - 1)) << shift;
                    remaining -= take;
                    shift += take;
                    bo = 0;
                    bi += 1;
                }
                out.push(pal[(v & mask) as usize]);
                bit_off += bits as u64;
            }
        }
    }
    *pos += total_bytes;
    Ok(out)
}

// ---------------------------------------------------------------------------
// encode section blocks
// ---------------------------------------------------------------------------

fn encode_blocks(out: &mut Vec<u8>, blocks: &[u32], seen: &[&[u32]]) {
    let first = blocks[0];
    if blocks.iter().all(|&b| b == 0) {
        out.push(0);
        return;
    }
    if blocks.iter().all(|&b| b == first) {
        out.push(1);
        put_varint(out, first);
        return;
    }
    for (j, s) in seen.iter().enumerate() {
        if **s == *blocks {
            out.push(2);
            put_varint(out, j as u32);
            return;
        }
    }
    // palette (order of first appearance)
    let mut pal = Vec::with_capacity(16);
    let mut map = std::collections::HashMap::with_capacity(32);
    let mut idx = Vec::with_capacity(SEC_BLOCKS);
    for &b in blocks {
        let i = match map.get(&b) {
            Some(&i) => i,
            None => {
                let i = pal.len() as u32;
                pal.push(b);
                map.insert(b, i);
                i
            }
        };
        idx.push(i);
    }
    let bits = bits_for(pal.len() as u32);
    let packed_bytes = (SEC_BLOCKS as u64 * bits as u64).div_ceil(8);

    // RLE candidate cost
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut i = 0usize;
    while i < SEC_BLOCKS {
        let v = idx[i];
        let mut len = 1u32;
        while (i + len as usize) < SEC_BLOCKS && idx[i + len as usize] == v {
            len += 1;
        }
        runs.push((len, v));
        i += len as usize;
    }
    let idx_bytes = if bits <= 8 { 1u64 } else { 4u64 };
    let mut rle_bytes = 1u64; // mode
    for (len, _) in &runs {
        rle_bytes += (*len as u64).div_ceil(256) * (1 + idx_bytes);
    }

    let mut pal_bytes = 1u64; // mode
    pal_bytes += varint_len(pal.len() as u32) as u64;
    for &p in &pal {
        pal_bytes += varint_len(p) as u64;
    }
    pal_bytes += packed_bytes;

    let use_rle = rle_bytes <= pal_bytes;
    if use_rle {
        out.push(4);
        put_varint(out, pal.len() as u32);
        for &p in &pal {
            put_varint(out, p);
        }
        for (len, v) in runs {
            let mut left = len;
            while left > 0 {
                let c = left.min(256);
                out.push((c - 1) as u8);
                if bits <= 8 {
                    out.push(v as u8);
                } else {
                    put_varint(out, v);
                }
                left -= c;
            }
        }
    } else {
        out.push(3);
        put_varint(out, pal.len() as u32);
        for &p in &pal {
            put_varint(out, p);
        }
        put_bitpacked(out, &idx, bits);
    }
}

#[inline(always)]
fn varint_len(mut v: u32) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        n += 1;
        v >>= 7;
    }
    n
}

fn encode_biomes(out: &mut Vec<u8>, biomes: &[u16], seen: &[&[u16]]) {
    let first = biomes[0];
    if biomes.iter().all(|&b| b == first) {
        out.push(0);
        put_varint(out, first as u32);
        return;
    }
    for (j, s) in seen.iter().enumerate() {
        if **s == *biomes {
            out.push(1);
            put_varint(out, j as u32);
            return;
        }
    }
    let mut pal = Vec::with_capacity(4);
    let mut map = std::collections::HashMap::with_capacity(8);
    let mut idx = Vec::with_capacity(SEC_BIOMES);
    for &b in biomes {
        let i = match map.get(&b) {
            Some(&i) => i,
            None => {
                let i = pal.len() as u32;
                pal.push(b);
                map.insert(b, i);
                i
            }
        };
        idx.push(i);
    }
    out.push(2);
    put_varint(out, pal.len() as u32);
    for &p in &pal {
        put_varint(out, p as u32);
    }
    put_bitpacked(out, &idx, bits_for(pal.len() as u32));
}

fn decode_blocks(bytes: &[u8], pos: &mut usize, sections: &[Section]) -> Result<Vec<u32>> {
    if *pos >= bytes.len() {
        bail!("fastbin: EOF in block mode");
    }
    let mode = bytes[*pos];
    *pos += 1;
    match mode {
        0 => Ok(vec![0u32; SEC_BLOCKS]),
        1 => {
            let v = get_varint(bytes, pos)?;
            Ok(vec![v; SEC_BLOCKS])
        }
        2 => {
            let j = get_varint(bytes, pos)? as usize;
            if j >= sections.len() {
                bail!("fastbin: bad backref {j}");
            }
            Ok(sections[j].blocks.clone())
        }
        3 | 4 => {
            let plen = get_varint(bytes, pos)? as usize;
            if !(2..=SEC_BLOCKS).contains(&plen) {
                bail!("fastbin: bad palette length {plen}");
            }
            let mut pal = Vec::with_capacity(plen);
            for _ in 0..plen {
                pal.push(get_varint(bytes, pos)?);
            }
            let bits = bits_for(plen as u32);
            if mode == 3 {
                let blocks = get_palette_final(bytes, pos, SEC_BLOCKS, bits, &pal)?;
                Ok(blocks)
            } else {
                let mut blocks = vec![0u32; SEC_BLOCKS];
                let mut off = 0usize;
                while off < SEC_BLOCKS {
                    if *pos + 1 >= bytes.len() {
                        bail!("fastbin: EOF in RLE stream");
                    }
                    let c = bytes[*pos] as usize + 1;
                    *pos += 1;
                    let v = if bits <= 8 {
                        let v = bytes[*pos] as u32;
                        *pos += 1;
                        v
                    } else {
                        get_varint(bytes, pos)?
                    };
                    if v as usize >= plen {
                        bail!("fastbin: RLE palette index {v} >= {plen}");
                    }
                    let end = off + c;
                    if end > SEC_BLOCKS {
                        bail!("fastbin: RLE overrun");
                    }
                    blocks[off..end].fill(pal[v as usize]);
                    off = end;
                }
                Ok(blocks)
            }
        }
        _ => bail!("fastbin: unknown block mode {mode}"),
    }
}

fn decode_biomes(bytes: &[u8], pos: &mut usize, sections: &[Section]) -> Result<Vec<u16>> {
    if *pos >= bytes.len() {
        bail!("fastbin: EOF in biome mode");
    }
    let mode = bytes[*pos];
    *pos += 1;
    match mode {
        0 => {
            let v = get_varint(bytes, pos)?;
            if v > u16::MAX as u32 {
                bail!("fastbin: biome id overflow");
            }
            Ok(vec![v as u16; SEC_BIOMES])
        }
        1 => {
            let j = get_varint(bytes, pos)? as usize;
            if j >= sections.len() {
                bail!("fastbin: bad biome backref {j}");
            }
            Ok(sections[j].biomes.clone())
        }
        2 => {
            let plen = get_varint(bytes, pos)? as usize;
            if !(2..=SEC_BIOMES).contains(&plen) {
                bail!("fastbin: bad biome palette length {plen}");
            }
            let mut pal = Vec::with_capacity(plen);
            for _ in 0..plen {
                let v = get_varint(bytes, pos)?;
                if v > u16::MAX as u32 {
                    bail!("fastbin: biome id overflow");
                }
                pal.push(v as u16);
            }
            let bits = bits_for(plen as u32);
            let mut idx = vec![0u32; SEC_BIOMES];
            get_indices(bytes, pos, SEC_BIOMES, bits, &mut idx)?;
            let mut biomes = Vec::with_capacity(SEC_BIOMES);
            for v in idx {
                biomes.push(pal[v as usize] as u16);
            }
            Ok(biomes)
        }
        _ => bail!("fastbin: unknown biome mode {mode}"),
    }
}

impl Codec for FastBinCodec {
    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::with_capacity(2048);
        out.extend_from_slice(&chunk.section_min_y.to_le_bytes());
        out.extend_from_slice(&(chunk.sections.len() as u16).to_le_bytes());

        // Whole-chunk shortcut: every section all-air with uniform biomes.
        let all_air = chunk
            .sections
            .iter()
            .all(|s| s.blocks.iter().all(|&b| b == 0) && s.biomes.iter().all(|&b| b == s.biomes[0]));
        if all_air {
            out.push(0xff);
            for sec in &chunk.sections {
                put_varint(&mut out, sec.biomes[0] as u32);
            }
            return out;
        }

        let mut seen_b: Vec<&[u32]> = Vec::with_capacity(chunk.sections.len());
        let mut seen_m: Vec<&[u16]> = Vec::with_capacity(chunk.sections.len());
        for sec in &chunk.sections {
            encode_blocks(&mut out, &sec.blocks, &seen_b);
            encode_biomes(&mut out, &sec.biomes, &seen_m);
            seen_b.push(&sec.blocks);
            seen_m.push(&sec.biomes);
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.len() < 6 {
            bail!("fastbin: stream too short");
        }
        let section_min_y = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let ns = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        let mut pos = 6usize;
        if pos < bytes.len() && bytes[pos] == 0xff {
            pos += 1;
            let mut sections = Vec::with_capacity(ns);
            for _ in 0..ns {
                let v = get_varint(bytes, &mut pos)?;
                if v > u16::MAX as u32 {
                    bail!("fastbin: biome id overflow");
                }
                sections.push(Section {
                    blocks: vec![0u32; SEC_BLOCKS],
                    biomes: vec![v as u16; SEC_BIOMES],
                });
            }
            if pos != bytes.len() {
                bail!("fastbin: trailing bytes {} of {}", pos, bytes.len());
            }
            return Ok(vec![Chunk { x: 0, z: 0, section_min_y, sections }]);
        }
        let mut sections = Vec::with_capacity(ns);
        for _ in 0..ns {
            let blocks = decode_blocks(bytes, &mut pos, &sections)?;
            let biomes = decode_biomes(bytes, &mut pos, &sections)?;
            sections.push(Section { blocks, biomes });
        }
        if pos != bytes.len() {
            bail!("fastbin: trailing bytes {} of {}", pos, bytes.len());
        }
        Ok(vec![Chunk { x: 0, z: 0, section_min_y, sections }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

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
            *b = (i % 5) as u16;
        }
        sections.push(sixteen);
        let mut rng = 555u64;
        let mut noise = Section::air();
        for v in noise.blocks.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = 20 + (rng % 100) as u32;
        }
        sections.push(noise);
        let mut runs = Section::air();
        let mut i = 0;
        let mut v = 1u32;
        while i < 4096 {
            let len = (i % 700) + 1;
            for _ in 0..len.min(4096 - i) {
                runs.blocks[i] = v;
                i += 1;
            }
            v += 1;
        }
        sections.push(runs);
        let ch = Chunk { x: 0, z: 0, section_min_y: -64, sections };
        let codec = FastBinCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}
