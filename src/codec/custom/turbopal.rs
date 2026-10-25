use crate::chunk::{Chunk, Section, SECTION_BIOMES, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub struct TurboPalCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "adaptive",
    description: "Ultra-fast decoding codec with adaptive RLE and power-of-two bit-packed palettes.",
    make: || Box::new(TurboPalCodec) as Box<dyn Codec>,
}];

// Режимы кодирования блоков в секции
const MODE_SINGLE: u8 = 0;
const MODE_RLE: u8 = 1;
const MODE_1BIT: u8 = 2;
const MODE_2BIT: u8 = 3;
const MODE_4BIT: u8 = 4;
const MODE_8BIT: u8 = 5;
const MODE_RAW: u8 = 6;

// Режимы кодирования биомов в секции
const BIOME_SINGLE: u8 = 0;
const BIOME_2BIT: u8 = 1;
const BIOME_4BIT: u8 = 2;
const BIOME_RAW: u8 = 3;

#[inline(always)]
fn write_varint_u32(buf: &mut Vec<u8>, mut val: u32) {
    while val >= 0x80 {
        buf.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
    buf.push(val as u8);
}

#[inline(always)]
fn read_varint_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut result = 0u32;
    let mut shift = 0;
    loop {
        if *pos >= bytes.len() {
            bail!("Unexpected EOF reading varint");
        }
        let b = bytes[*pos];
        *pos += 1;
        result |= ((b & 0x7F) as u32) << shift;
        if (b & 0x80) == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 35 {
            bail!("Varint overflow");
        }
    }
}

#[inline(always)]
fn encode_rle_u8(indices: &[u8]) -> Vec<u8> {
    let mut rle = Vec::with_capacity(indices.len() / 2);
    let mut i = 0;
    while i < indices.len() {
        let val = indices[i];
        let mut run = 1;
        while i + run < indices.len() && indices[i + run] == val && run < 256 {
            run += 1;
        }
        rle.push(val);
        rle.push((run - 1) as u8);
        i += run;
    }
    rle
}

impl Codec for TurboPalCodec {
    fn id(&self) -> String {
        "turbopal".to_string()
    }

    fn name(&self) -> String {
        "TurboPal".to_string()
    }

    fn description(&self) -> String {
        "Ultra-fast decoding codec with adaptive RLE and power-of-two bit-packed palettes.".to_string()
    }

    fn group_size(&self) -> usize {
        1
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        if chunks.is_empty() {
            return Vec::new();
        }

        let chunk = chunks[0];
        let mut out = Vec::with_capacity(chunk.sections.len() * 512);

        // Заголовок чанка
        out.extend_from_slice(&chunk.section_min_y.to_le_bytes());
        out.extend_from_slice(&(chunk.sections.len() as u16).to_le_bytes());

        for sec in &chunk.sections {
            // ================= 1. БЛОКИ =================
            let blocks = &sec.blocks;

            // Построение палитры блоков
            let mut palette = Vec::with_capacity(16);
            let mut last_b = u32::MAX;
            for &b in blocks {
                if b != last_b {
                    last_b = b;
                    if !palette.contains(&b) {
                        palette.push(b);
                    }
                }
            }

            if palette.len() == 1 {
                // Монолитная секция (100% один блок)
                out.push(MODE_SINGLE);
                write_varint_u32(&mut out, palette[0]);
            } else if palette.len() <= 256 {
                // Преобразование блоков в индексы локальной палитры
                let mut indices = Vec::with_capacity(SECTION_BLOCKS);
                if palette.len() <= 16 {
                    for &b in blocks {
                        let idx = palette.iter().position(|&x| x == b).unwrap() as u8;
                        indices.push(idx);
                    }
                } else {
                    use std::collections::HashMap;
                    let mut map = HashMap::with_capacity(palette.len());
                    for (i, &b) in palette.iter().enumerate() {
                        map.insert(b, i as u8);
                    }
                    for &b in blocks {
                        indices.push(*map.get(&b).unwrap());
                    }
                }

                // Проверяем размер RLE
                let rle_data = encode_rle_u8(&indices);

                let bitpack_size = if palette.len() <= 2 {
                    512
                } else if palette.len() <= 4 {
                    1024
                } else if palette.len() <= 16 {
                    2048
                } else {
                    4096
                };

                if rle_data.len() < bitpack_size {
                    // RLE компактнее bit-packing
                    out.push(MODE_RLE);
                    out.push((palette.len() - 1) as u8);
                    for &p in &palette {
                        write_varint_u32(&mut out, p);
                    }
                    out.extend_from_slice(&(rle_data.len() as u16).to_le_bytes());
                    out.extend_from_slice(&rle_data);
                } else if palette.len() <= 2 {
                    out.push(MODE_1BIT);
                    out.push((palette.len() - 1) as u8);
                    for &p in &palette {
                        write_varint_u32(&mut out, p);
                    }
                    let mut packed = vec![0u8; 512];
                    for (i, &idx) in indices.iter().enumerate() {
                        packed[i / 8] |= (idx & 1) << (i % 8);
                    }
                    out.extend_from_slice(&packed);
                } else if palette.len() <= 4 {
                    out.push(MODE_2BIT);
                    out.push((palette.len() - 1) as u8);
                    for &p in &palette {
                        write_varint_u32(&mut out, p);
                    }
                    let mut packed = vec![0u8; 1024];
                    for (i, &idx) in indices.iter().enumerate() {
                        packed[i / 4] |= (idx & 3) << ((i % 4) * 2);
                    }
                    out.extend_from_slice(&packed);
                } else if palette.len() <= 16 {
                    out.push(MODE_4BIT);
                    out.push((palette.len() - 1) as u8);
                    for &p in &palette {
                        write_varint_u32(&mut out, p);
                    }
                    let mut packed = vec![0u8; 2048];
                    for (i, &idx) in indices.iter().enumerate() {
                        packed[i / 2] |= (idx & 0x0F) << ((i % 2) * 4);
                    }
                    out.extend_from_slice(&packed);
                } else {
                    out.push(MODE_8BIT);
                    out.push((palette.len() - 1) as u8);
                    for &p in &palette {
                        write_varint_u32(&mut out, p);
                    }
                    out.extend_from_slice(&indices);
                }
            } else {
                // Редчайший fallback: >256 уникальных блоков
                out.push(MODE_RAW);
                for &b in blocks {
                    out.extend_from_slice(&b.to_le_bytes());
                }
            }

            // ================= 2. БИОМЫ =================
            let biomes = &sec.biomes;
            let mut b_palette = Vec::with_capacity(4);
            let mut last_bio = u16::MAX;
            for &bio in biomes {
                if bio != last_bio {
                    last_bio = bio;
                    if !b_palette.contains(&bio) {
                        b_palette.push(bio);
                    }
                }
            }

            if b_palette.len() == 1 {
                out.push(BIOME_SINGLE);
                write_varint_u32(&mut out, b_palette[0] as u32);
            } else if b_palette.len() <= 4 {
                out.push(BIOME_2BIT);
                out.push((b_palette.len() - 1) as u8);
                for &bio in &b_palette {
                    write_varint_u32(&mut out, bio as u32);
                }
                let mut packed = [0u8; 16];
                for (i, &bio) in biomes.iter().enumerate() {
                    let idx = b_palette.iter().position(|&x| x == bio).unwrap() as u8;
                    packed[i / 4] |= (idx & 3) << ((i % 4) * 2);
                }
                out.extend_from_slice(&packed);
            } else if b_palette.len() <= 16 {
                out.push(BIOME_4BIT);
                out.push((b_palette.len() - 1) as u8);
                for &bio in &b_palette {
                    write_varint_u32(&mut out, bio as u32);
                }
                let mut packed = [0u8; 32];
                for (i, &bio) in biomes.iter().enumerate() {
                    let idx = b_palette.iter().position(|&x| x == bio).unwrap() as u8;
                    packed[i / 2] |= (idx & 0x0F) << ((i % 2) * 4);
                }
                out.extend_from_slice(&packed);
            } else {
                out.push(BIOME_RAW);
                for &bio in biomes {
                    out.extend_from_slice(&bio.to_le_bytes());
                }
            }
        }

        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        if bytes.len() < 6 {
            bail!("Input bytes too short for chunk header");
        }

        let section_min_y = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let num_sections = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        let mut pos = 6;

        let mut sections = Vec::with_capacity(num_sections);

        for _ in 0..num_sections {
            if pos >= bytes.len() {
                bail!("Unexpected EOF reading section block mode");
            }
            let block_mode = bytes[pos];
            pos += 1;

            let blocks = match block_mode {
                MODE_SINGLE => {
                    let val = read_varint_u32(bytes, &mut pos)?;
                    vec![val; SECTION_BLOCKS]
                }
                MODE_RLE => {
                    if pos >= bytes.len() {
                        bail!("Unexpected EOF reading RLE palette length");
                    }
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;

                    let mut palette = Vec::with_capacity(pal_len);
                    for _ in 0..pal_len {
                        palette.push(read_varint_u32(bytes, &mut pos)?);
                    }

                    if pos + 2 > bytes.len() {
                        bail!("Unexpected EOF reading RLE byte length");
                    }
                    let rle_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                    pos += 2;

                    let rle_end = pos + rle_len;
                    if rle_end > bytes.len() {
                        bail!("RLE stream truncated");
                    }

                    let mut blks = vec![0u32; SECTION_BLOCKS];
                    let mut offset = 0;

                    while pos < rle_end {
                        let p_idx = bytes[pos] as usize;
                        let count = (bytes[pos + 1] as usize) + 1;
                        pos += 2;

                        if p_idx >= palette.len() {
                            bail!("RLE palette index out of bounds");
                        }
                        let block = palette[p_idx];
                        let next = offset + count;
                        if next > SECTION_BLOCKS {
                            bail!("RLE run exceeds section size");
                        }
                        blks[offset..next].fill(block);
                        offset = next;
                    }

                    if offset != SECTION_BLOCKS {
                        bail!("RLE total block count mismatch");
                    }
                    blks
                }
                MODE_1BIT => {
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;
                    let mut pal = [0u32; 2];
                    for i in 0..pal_len {
                        pal[i] = read_varint_u32(bytes, &mut pos)?;
                    }

                    if pos + 512 > bytes.len() {
                        bail!("Unexpected EOF in 1-bit packed data");
                    }
                    let slice = &bytes[pos..pos + 512];
                    pos += 512;

                    let mut blks = vec![0u32; SECTION_BLOCKS];
                    for (i, &b) in slice.iter().enumerate() {
                        let base = i * 8;
                        blks[base] = pal[(b & 1) as usize];
                        blks[base + 1] = pal[((b >> 1) & 1) as usize];
                        blks[base + 2] = pal[((b >> 2) & 1) as usize];
                        blks[base + 3] = pal[((b >> 3) & 1) as usize];
                        blks[base + 4] = pal[((b >> 4) & 1) as usize];
                        blks[base + 5] = pal[((b >> 5) & 1) as usize];
                        blks[base + 6] = pal[((b >> 6) & 1) as usize];
                        blks[base + 7] = pal[(b >> 7) as usize];
                    }
                    blks
                }
                MODE_2BIT => {
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;
                    let mut pal = [0u32; 4];
                    for i in 0..pal_len {
                        pal[i] = read_varint_u32(bytes, &mut pos)?;
                    }

                    if pos + 1024 > bytes.len() {
                        bail!("Unexpected EOF in 2-bit packed data");
                    }
                    let slice = &bytes[pos..pos + 1024];
                    pos += 1024;

                    let mut blks = vec![0u32; SECTION_BLOCKS];
                    for (i, &b) in slice.iter().enumerate() {
                        let base = i * 4;
                        blks[base] = pal[(b & 3) as usize];
                        blks[base + 1] = pal[((b >> 2) & 3) as usize];
                        blks[base + 2] = pal[((b >> 4) & 3) as usize];
                        blks[base + 3] = pal[(b >> 6) as usize];
                    }
                    blks
                }
                MODE_4BIT => {
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;
                    let mut pal = [0u32; 16];
                    for i in 0..pal_len {
                        pal[i] = read_varint_u32(bytes, &mut pos)?;
                    }

                    if pos + 2048 > bytes.len() {
                        bail!("Unexpected EOF in 4-bit packed data");
                    }
                    let slice = &bytes[pos..pos + 2048];
                    pos += 2048;

                    let mut blks = vec![0u32; SECTION_BLOCKS];
                    for (i, &b) in slice.iter().enumerate() {
                        blks[i * 2] = pal[(b & 0x0F) as usize];
                        blks[i * 2 + 1] = pal[(b >> 4) as usize];
                    }
                    blks
                }
                MODE_8BIT => {
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;
                    let mut pal = [0u32; 256];
                    for i in 0..pal_len {
                        pal[i] = read_varint_u32(bytes, &mut pos)?;
                    }

                    if pos + 4096 > bytes.len() {
                        bail!("Unexpected EOF in 8-bit packed data");
                    }
                    let slice = &bytes[pos..pos + 4096];
                    pos += 4096;

                    let mut blks = vec![0u32; SECTION_BLOCKS];
                    for (dest, &b) in blks.iter_mut().zip(slice.iter()) {
                        *dest = pal[b as usize];
                    }
                    blks
                }
                MODE_RAW => {
                    if pos + SECTION_BLOCKS * 4 > bytes.len() {
                        bail!("Unexpected EOF in raw blocks data");
                    }
                    let mut blks = Vec::with_capacity(SECTION_BLOCKS);
                    for _ in 0..SECTION_BLOCKS {
                        let b = u32::from_le_bytes([
                            bytes[pos],
                            bytes[pos + 1],
                            bytes[pos + 2],
                            bytes[pos + 3],
                        ]);
                        pos += 4;
                        blks.push(b);
                    }
                    blks
                }
                _ => bail!("Unknown block mode: {}", block_mode),
            };

            // Декодирование биомов
            if pos >= bytes.len() {
                bail!("Unexpected EOF reading biome mode");
            }
            let biome_mode = bytes[pos];
            pos += 1;

            let biomes = match biome_mode {
                BIOME_SINGLE => {
                    let val = read_varint_u32(bytes, &mut pos)? as u16;
                    vec![val; SECTION_BIOMES]
                }
                BIOME_2BIT => {
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;
                    let mut pal = [0u16; 4];
                    for i in 0..pal_len {
                        pal[i] = read_varint_u32(bytes, &mut pos)? as u16;
                    }
                    if pos + 16 > bytes.len() {
                        bail!("Unexpected EOF reading 2-bit biomes");
                    }
                    let slice = &bytes[pos..pos + 16];
                    pos += 16;

                    let mut bios = vec![0u16; SECTION_BIOMES];
                    for (i, &b) in slice.iter().enumerate() {
                        let base = i * 4;
                        bios[base] = pal[(b & 3) as usize];
                        bios[base + 1] = pal[((b >> 2) & 3) as usize];
                        bios[base + 2] = pal[((b >> 4) & 3) as usize];
                        bios[base + 3] = pal[(b >> 6) as usize];
                    }
                    bios
                }
                BIOME_4BIT => {
                    let pal_len = (bytes[pos] as usize) + 1;
                    pos += 1;
                    let mut pal = [0u16; 16];
                    for i in 0..pal_len {
                        pal[i] = read_varint_u32(bytes, &mut pos)? as u16;
                    }
                    if pos + 32 > bytes.len() {
                        bail!("Unexpected EOF reading 4-bit biomes");
                    }
                    let slice = &bytes[pos..pos + 32];
                    pos += 32;

                    let mut bios = vec![0u16; SECTION_BIOMES];
                    for (i, &b) in slice.iter().enumerate() {
                        bios[i * 2] = pal[(b & 0x0F) as usize];
                        bios[i * 2 + 1] = pal[(b >> 4) as usize];
                    }
                    bios
                }
                BIOME_RAW => {
                    if pos + SECTION_BIOMES * 2 > bytes.len() {
                        bail!("Unexpected EOF reading raw biomes");
                    }
                    let mut bios = Vec::with_capacity(SECTION_BIOMES);
                    for _ in 0..SECTION_BIOMES {
                        let b = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
                        pos += 2;
                        bios.push(b);
                    }
                    bios
                }
                _ => bail!("Unknown biome mode: {}", biome_mode),
            };

            sections.push(Section { blocks, biomes });
        }

        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y,
            sections,
        }])
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

        let air_biomes = vec![0u16; 64];
        let mut sections = Vec::new();

        sections.push(Section::air());
        let mut mono = Section::air();
        for b in mono.blocks.iter_mut() {
            *b = 7;
        }
        sections.push(mono);

        let mut two = Section::air();
        for i in 0..4096 {
            two.blocks[i] = (i % 2) as u32;
        }
        sections.push(two);

        let mut four = Section::air();
        for i in 0..4096 {
            four.blocks[i] = (i % 4) as u32;
        }
        sections.push(four);

        let mut sixteen = Section::air();
        for i in 0..4096 {
            sixteen.blocks[i] = ((i / 64) % 16) as u32;
        }
        for (i, b) in sixteen.biomes.iter_mut().enumerate() {
            *b = (i % 5) as u16;
        }
        sections.push(sixteen);

        let mut rng = 999u64;
        let mut noise = Section::air();
        for v in noise.blocks.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = 20 + (rng % 60) as u32;
        }
        sections.push(noise);

        let mut runs = Section::air();
        let mut i = 0;
        let mut v = 1u32;
        while i < 4096 {
            let len = (i % 300) + 1;
            for _ in 0..len.min(4096 - i) {
                runs.blocks[i] = v;
                i += 1;
            }
            v += 1;
        }
        for b in runs.biomes.iter_mut() {
            *b = 3;
        }
        sections.push(runs);

        let mut huge = Section::air();
        for i in 0..4096 {
            huge.blocks[i] = 300 + (i % 400) as u32;
        }
        sections.push(huge);

        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = TurboPalCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}