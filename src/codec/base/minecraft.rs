//! Standard Minecraft 1.18+ chunk storage format (palette + bit-packed longs).
//! Mirrors the real on-disk layout: each section carries a block-state palette
//! (names + properties) and a packed long array; biome sections use the same
//! container pattern.

use crate::chunk::{Chunk, Section, SECTION_BIOMES, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub struct MinecraftCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "minecraft",
    description: "Стандартный формат Minecraft 1.18+: палитра состояний блоков на секцию 16×16×16 + бит-упакованные long-массивы. Совместим с ванильным чанком.",
    make: || Box::new(MinecraftCodec) as Box<dyn Codec>,
}];

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut v: u32 = 0;
    let mut shift = 0;
    loop {
        if *pos >= bytes.len() {
            bail!("varint overflow");
        }
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 28 {
            bail!("varint too long");
        }
    }
}

/// Minimal bits needed to store `n` distinct values (with `min` floor).
fn bits_for(n: usize, min: u32) -> u32 {
    if n <= 1 {
        return min;
    }
    let mut b = 0u32;
    let mut m = n as u64 - 1;
    while m > 0 {
        b += 1;
        m >>= 1;
    }
    b.max(min)
}

fn pack_long_array(values: &[u32], bits: u32) -> Vec<i64> {
    if bits == 0 {
        return Vec::new();
    }
    let n_longs = (values.len() as u64 * bits as u64).div_ceil(64) as usize;
    let mut out = vec![0i64; n_longs];
    for (i, &v) in values.iter().enumerate() {
        let bit = i as u64 * bits as u64;
        let li = (bit >> 6) as usize;
        let off = (bit & 63) as u32;
        out[li] |= ((v as i64) & ((1i64 << bits.min(63)) - 1)) << off;
        if off + bits > 64 && li + 1 < n_longs {
            out[li + 1] |= (v as i64) >> (64 - off);
        }
    }
    out
}

fn unpack_long_array(data: &[i64], count: usize, bits: u32) -> Vec<u32> {
    if bits == 0 {
        return vec![0; count];
    }
    let mask = if bits >= 63 { u64::MAX } else { (1u64 << bits) - 1 };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bit = i as u64 * bits as u64;
        let li = (bit >> 6) as usize;
        let off = (bit & 63) as u32;
        let mut v = (data[li] as u64) >> off;
        if off + bits > 64 && li + 1 < data.len() {
            v |= (data[li + 1] as u64) << (64 - off);
        }
        out.push((v & mask) as u32);
    }
    out
}

/// Serialize a string as varint length + UTF-8 bytes.
fn write_string(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn read_string(bytes: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_varint(bytes, pos)? as usize;
    if *pos + len > bytes.len() {
        bail!("string out of bounds");
    }
    let s = std::str::from_utf8(&bytes[*pos..*pos + len])
        .map_err(|e| anyhow::anyhow!("utf8: {e}"))?
        .to_string();
    *pos += len;
    Ok(s)
}

/// Canonical state string <-> (name, properties) split. Properties are stored
/// as a sorted list of (key, value) pairs.
fn split_state(state: &str) -> (String, Vec<(String, String)>) {
    if let Some(brace) = state.find('[') {
        let name = state[..brace].to_string();
        let inner = &state[brace + 1..state.len() - 1];
        let props = inner
            .split(',')
            .filter_map(|p| {
                let mut it = p.splitn(2, '=');
                match (it.next(), it.next()) {
                    (Some(k), Some(v)) => Some((k.to_string(), v.to_string())),
                    _ => None,
                }
            })
            .collect();
        (name, props)
    } else {
        (state.to_string(), Vec::new())
    }
}

fn write_state(out: &mut Vec<u8>, state: &str) {
    let (name, props) = split_state(state);
    write_string(out, &name);
    write_varint(out, props.len() as u32);
    for (k, v) in props {
        write_string(out, &k);
        write_string(out, &v);
    }
}

fn read_state(bytes: &[u8], pos: &mut usize) -> Result<String> {
    let name = read_string(bytes, pos)?;
    let n = read_varint(bytes, pos)? as usize;
    let mut props = Vec::with_capacity(n);
    for _ in 0..n {
        let k = read_string(bytes, pos)?;
        let v = read_string(bytes, pos)?;
        props.push((k, v));
    }
    if props.is_empty() {
        return Ok(name);
    }
    props.sort();
    let inner = props
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!("{name}[{inner}]"))
}

fn encode_section(section: &Section, ctx: &Ctx, out: &mut Vec<u8>) {
    // Block palette
    let mut palette: Vec<u32> = Vec::new();
    let mut index: HashMap<u32, u32> = HashMap::new();
    for &b in &section.blocks {
        let e = index.entry(b).or_insert_with(|| {
            palette.push(b);
            (palette.len() - 1) as u32
        });
        let _ = e;
    }
    write_varint(out, palette.len() as u32);
    for &id in &palette {
        write_state(out, &ctx.registry.names[id as usize]);
    }
    let bits = bits_for(palette.len(), 4);
    // Only write data when more than one palette entry.
    if palette.len() > 1 {
        let indexed: Vec<u32> = section
            .blocks
            .iter()
            .map(|b| index[b])
            .collect();
        let packed = pack_long_array(&indexed, bits);
        out.extend(packed.iter().flat_map(|v| v.to_be_bytes()));
    }

    // Biome palette
    let mut bpal: Vec<u16> = Vec::new();
    let mut bindex: HashMap<u16, u32> = HashMap::new();
    for &b in &section.biomes {
        let e = bindex.entry(b).or_insert_with(|| {
            bpal.push(b);
            (bpal.len() - 1) as u32
        });
        let _ = e;
    }
    write_varint(out, bpal.len() as u32);
    for &id in &bpal {
        write_string(out, &ctx.biomes.names[id as usize]);
    }
    let bbits = bits_for(bpal.len(), 0);
    if bpal.len() > 1 {
        let indexed: Vec<u32> = section.biomes.iter().map(|b| bindex[b]).collect();
        let packed = pack_long_array(&indexed, bbits);
        out.extend(packed.iter().flat_map(|v| v.to_be_bytes()));
    }
}

fn decode_section(bytes: &[u8], pos: &mut usize, ctx: &Ctx) -> Result<Section> {
    let pal_len = read_varint(bytes, pos)? as usize;
    let mut ids = Vec::with_capacity(pal_len);
    for _ in 0..pal_len {
        let state = read_state(bytes, pos)?;
        ids.push(ctx.registry.get(&state).unwrap_or(0));
    }
    let bits = bits_for(pal_len, 4);
    let mut blocks = vec![0u32; SECTION_BLOCKS];
    if pal_len > 1 {
        let n_longs = (SECTION_BLOCKS as u64 * bits as u64).div_ceil(64) as usize;
        let mut longs = Vec::with_capacity(n_longs);
        for _ in 0..n_longs {
            if *pos + 8 > bytes.len() {
                bail!("long array out of bounds");
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[*pos..*pos + 8]);
            *pos += 8;
            longs.push(i64::from_be_bytes(b));
        }
        let unpacked = unpack_long_array(&longs, SECTION_BLOCKS, bits);
        for (i, b) in blocks.iter_mut().enumerate() {
            *b = ids.get(unpacked[i] as usize).copied().unwrap_or(0);
        }
    } else {
        let p = ids.first().copied().unwrap_or(0);
        for b in blocks.iter_mut() {
            *b = p;
        }
    }

    // Biomes
    let bpal_len = read_varint(bytes, pos)? as usize;
    let mut bids = Vec::with_capacity(bpal_len);
    for _ in 0..bpal_len {
        let name = read_string(bytes, pos)?;
        bids.push(ctx.biomes.get(&name).unwrap_or(0));
    }
    let bbits = bits_for(bpal_len, 0);
    let mut biomes = vec![0u16; SECTION_BIOMES];
    if bpal_len > 1 {
        let n_longs = (SECTION_BIOMES as u64 * bbits as u64).div_ceil(64) as usize;
        let mut longs = Vec::with_capacity(n_longs);
        for _ in 0..n_longs {
            if *pos + 8 > bytes.len() {
                bail!("biome long array out of bounds");
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[*pos..*pos + 8]);
            *pos += 8;
            longs.push(i64::from_be_bytes(b));
        }
        let unpacked = unpack_long_array(&longs, SECTION_BIOMES, bbits);
        for (i, b) in biomes.iter_mut().enumerate() {
            *b = bids.get(unpacked[i] as usize).copied().unwrap_or(0);
        }
    } else {
        let p = bids.first().copied().unwrap_or(0);
        for b in biomes.iter_mut() {
            *b = p;
        }
    }
    Ok(Section { blocks, biomes })
}

impl Codec for MinecraftCodec {
    fn id(&self) -> String {
        "minecraft".to_string()
    }

    fn name(&self) -> String {
        "Minecraft standard (palette + packed longs)".to_string()
    }


    fn description(&self) -> String {
        "Reference format mirroring vanilla 1.18+ section storage: per-section \
         palette with varint-prefixed names/properties, bit-packed long arrays \
         (block bits >= 4, biome bits from palette size)."
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::new();
        out.push(chunk.sections.len() as u8);
        for (i, sec) in chunk.sections.iter().enumerate() {
            out.push((chunk.world_y(i) as i8) as u8);
            encode_section(sec, ctx, &mut out);
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.is_empty() {
            bail!("empty stream");
        }
        let n = bytes[0] as usize;
        let mut pos = 1usize;
        let mut sections = Vec::with_capacity(n);
        let mut first_y: Option<i32> = None;
        for _ in 0..n {
            if pos >= bytes.len() {
                bail!("not enough data for sections");
            }
            let y = bytes[pos] as i8 as i32;
            pos += 1;
            if first_y.is_none() {
                first_y = Some(y);
            }
            sections.push(decode_section(bytes, &mut pos, ctx)?);
        }
        let min_y = first_y.unwrap_or(-64);
        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y: min_y,
            sections,
        }])
    }
}
