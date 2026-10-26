use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{bail, Result};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;

use crate::chunk::{Chunk, Section, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};

pub struct Apex3dCodec;

#[inline(always)]
fn block_idx(x: usize, y: usize, z: usize) -> usize {
    y * 256 + z * 16 + x
}

fn write_varu32(buf: &mut Vec<u8>, mut val: u32) {
    while val >= 0x80 {
        buf.push((val as u8 & 0x7F) | 0x80);
        val >>= 7;
    }
    buf.push(val as u8);
}

fn read_varu32(slice: &[u8], offset: &mut usize) -> Result<u32> {
    let mut val = 0u32;
    let mut shift = 0;
    loop {
        if *offset >= slice.len() {
            bail!("unexpected EOF in varu32");
        }
        let b = slice[*offset];
        *offset += 1;
        val |= ((b & 0x7F) as u32) << shift;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
        if shift > 35 {
            bail!("varu32 overflow");
        }
    }
    Ok(val)
}

fn write_vari32(buf: &mut Vec<u8>, val: i32) {
    let zigzag = ((val << 1) ^ (val >> 31)) as u32;
    write_varu32(buf, zigzag);
}

fn read_vari32(slice: &[u8], offset: &mut usize) -> Result<i32> {
    let zigzag = read_varu32(slice, offset)?;
    let val = ((zigzag >> 1) as i32) ^ (-((zigzag & 1) as i32));
    Ok(val)
}

fn encode_predictive_blocks(
    buf: &mut Vec<u8>,
    blocks: &[u32],
    prev_blocks: Option<&[u32]>,
    palette: &[u32],
    block_to_pal: &HashMap<u32, u32>,
) {
    let mut packed_cmds = Vec::with_capacity(1024);
    let mut literals = Vec::new();
    let mut cur_byte = 0u8;
    let mut bit_pos = 0;

    for z in 0..16 {
        for x in 0..16 {
            for y in 0..16 {
                let target = blocks[block_idx(x, y, z)];

                let mut cands = [0u32; 3];
                let mut cand_count = 0;

                // 1. Сосед снизу (Down)
                let d = if y > 0 {
                    Some(blocks[block_idx(x, y - 1, z)])
                } else { prev_blocks.map(|prev| prev[block_idx(x, 15, z)]) };
                if let Some(val) = d {
                    cands[cand_count] = val;
                    cand_count += 1;
                }

                // 2. Сосед с запада (West: x - 1)
                if x > 0 {
                    let val = blocks[block_idx(x - 1, y, z)];
                    if !cands[..cand_count].contains(&val) && cand_count < 3 {
                        cands[cand_count] = val;
                        cand_count += 1;
                    }
                }

                // 3. Сосед с севера (North: z - 1)
                if z > 0 {
                    let val = blocks[block_idx(x, y, z - 1)];
                    if !cands[..cand_count].contains(&val) && cand_count < 3 {
                        cands[cand_count] = val;
                        cand_count += 1;
                    }
                }

                // 4. Сосед с северо-запада (Northwest: x - 1, z - 1)
                if cand_count < 3 && x > 0 && z > 0 {
                    let val = blocks[block_idx(x - 1, y, z - 1)];
                    if !cands[..cand_count].contains(&val) {
                        cands[cand_count] = val;
                        cand_count += 1;
                    }
                }

                // Фоллбэки из наиболее популярных блоков чанка
                if cand_count < 3 && !palette.is_empty() && !cands[..cand_count].contains(&palette[0]) {
                    cands[cand_count] = palette[0];
                    cand_count += 1;
                }
                if cand_count < 3 && palette.len() > 1 && !cands[..cand_count].contains(&palette[1]) {
                    cands[cand_count] = palette[1];
                    cand_count += 1;
                }

                let code = if cand_count > 0 && target == cands[0] {
                    0u8
                } else if cand_count > 1 && target == cands[1] {
                    1u8
                } else if cand_count > 2 && target == cands[2] {
                    2u8
                } else {
                    let pal_idx = block_to_pal[&target];
                    literals.push(pal_idx);
                    3u8
                };

                cur_byte |= code << bit_pos;
                bit_pos += 2;
                if bit_pos == 8 {
                    packed_cmds.push(cur_byte);
                    cur_byte = 0;
                    bit_pos = 0;
                }
            }
        }
    }

    buf.extend_from_slice(&packed_cmds);

    write_varu32(buf, literals.len() as u32);
    let pal_fits_u8 = palette.len() <= 256;
    for lit in literals {
        if pal_fits_u8 {
            buf.push(lit as u8);
        } else {
            write_varu32(buf, lit);
        }
    }
}

fn decode_predictive_blocks(
    slice: &[u8],
    offset: &mut usize,
    prev_blocks: Option<&[u32]>,
    palette: &[u32],
) -> Result<Vec<u32>> {
    if *offset + 1024 > slice.len() {
        bail!("unexpected EOF reading commands");
    }
    let cmd_bytes = &slice[*offset..*offset + 1024];
    *offset += 1024;

    let num_literals = read_varu32(slice, offset)? as usize;
    let pal_fits_u8 = palette.len() <= 256;
    let mut literals = Vec::with_capacity(num_literals);

    if pal_fits_u8 {
        if *offset + num_literals > slice.len() {
            bail!("unexpected EOF reading literals");
        }
        for _ in 0..num_literals {
            literals.push(slice[*offset] as u32);
            *offset += 1;
        }
    } else {
        for _ in 0..num_literals {
            let lit = read_varu32(slice, offset)?;
            literals.push(lit);
        }
    }

    let mut blocks = vec![0u32; SECTION_BLOCKS];
    let mut cmd_byte_idx = 0;
    let mut bit_pos = 0;
    let mut cur_byte = cmd_bytes[0];
    let mut lit_idx = 0;

    for z in 0..16 {
        for x in 0..16 {
            for y in 0..16 {
                let code = (cur_byte >> bit_pos) & 3;
                bit_pos += 2;
                if bit_pos == 8 {
                    cmd_byte_idx += 1;
                    if cmd_byte_idx < 1024 {
                        cur_byte = cmd_bytes[cmd_byte_idx];
                    }
                    bit_pos = 0;
                }

                let mut cands = [0u32; 3];
                let mut cand_count = 0;

                // 1. Сосед снизу (Down)
                let d = if y > 0 {
                    Some(blocks[block_idx(x, y - 1, z)])
                } else { prev_blocks.map(|prev| prev[block_idx(x, 15, z)]) };
                if let Some(val) = d {
                    cands[cand_count] = val;
                    cand_count += 1;
                }

                // 2. Сосед с запада (West: x - 1)
                if x > 0 {
                    let val = blocks[block_idx(x - 1, y, z)];
                    if !cands[..cand_count].contains(&val) && cand_count < 3 {
                        cands[cand_count] = val;
                        cand_count += 1;
                    }
                }

                // 3. Сосед с севера (North: z - 1)
                if z > 0 {
                    let val = blocks[block_idx(x, y, z - 1)];
                    if !cands[..cand_count].contains(&val) && cand_count < 3 {
                        cands[cand_count] = val;
                        cand_count += 1;
                    }
                }

                // 4. Сосед с северо-запада (Northwest: x - 1, z - 1)
                if cand_count < 3 && x > 0 && z > 0 {
                    let val = blocks[block_idx(x - 1, y, z - 1)];
                    if !cands[..cand_count].contains(&val) {
                        cands[cand_count] = val;
                        cand_count += 1;
                    }
                }

                // Фоллбэки
                if cand_count < 3 && !palette.is_empty() && !cands[..cand_count].contains(&palette[0]) {
                    cands[cand_count] = palette[0];
                    cand_count += 1;
                }
                if cand_count < 3 && palette.len() > 1 && !cands[..cand_count].contains(&palette[1]) {
                    cands[cand_count] = palette[1];
                }

                let target = match code {
                    0 => cands[0],
                    1 => cands[1],
                    2 => cands[2],
                    3 => {
                        if lit_idx >= literals.len() {
                            bail!("literal index overflow");
                        }
                        let pal_idx = literals[lit_idx] as usize;
                        lit_idx += 1;
                        if pal_idx >= palette.len() {
                            bail!("palette index out of bounds: {}", pal_idx);
                        }
                        palette[pal_idx]
                    }
                    _ => unreachable!(),
                };

                blocks[block_idx(x, y, z)] = target;
            }
        }
    }

    Ok(blocks)
}

impl Codec for Apex3dCodec {
    fn id(&self) -> String {
        "apex-3d".to_string()
    }

    fn name(&self) -> String {
        "Apex 3D".to_string()
    }

    fn group_size(&self) -> usize {
        1
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut raw = Vec::new();

        write_vari32(&mut raw, chunk.section_min_y);
        write_varu32(&mut raw, chunk.sections.len() as u32);

        // Палитра блоков чанка, упорядоченная по частоте убывания
        let mut block_counts: HashMap<u32, usize> = HashMap::new();
        for sec in &chunk.sections {
            for &b in &sec.blocks {
                *block_counts.entry(b).or_insert(0) += 1;
            }
        }
        let mut pal_vec: Vec<(u32, usize)> = block_counts.into_iter().collect();
        pal_vec.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        let block_palette: Vec<u32> = pal_vec.into_iter().map(|(b, _)| b).collect();

        write_varu32(&mut raw, block_palette.len() as u32);
        for &b in &block_palette {
            write_varu32(&mut raw, b);
        }

        let block_to_pal: HashMap<u32, u32> = block_palette
            .iter()
            .enumerate()
            .map(|(i, &b)| (b, i as u32))
            .collect();

        // Палитра биомов чанка
        let mut biome_counts: HashMap<u16, usize> = HashMap::new();
        for sec in &chunk.sections {
            for &bm in &sec.biomes {
                *biome_counts.entry(bm).or_insert(0) += 1;
            }
        }
        let mut biome_pal_vec: Vec<(u16, usize)> = biome_counts.into_iter().collect();
        biome_pal_vec.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        let biome_palette: Vec<u16> = biome_pal_vec.into_iter().map(|(bm, _)| bm).collect();

        write_varu32(&mut raw, biome_palette.len() as u32);
        for &bm in &biome_palette {
            write_varu32(&mut raw, bm as u32);
        }

        let biome_to_pal: HashMap<u16, u16> = biome_palette
            .iter()
            .enumerate()
            .map(|(i, &bm)| (bm, i as u16))
            .collect();

        let mut resolved_blocks: Vec<Vec<u32>> = Vec::with_capacity(chunk.sections.len());
        let mut resolved_biomes: Vec<Vec<u16>> = Vec::with_capacity(chunk.sections.len());

        for s in 0..chunk.sections.len() {
            let sec = &chunk.sections[s];

            // 1. Кодирование биомов секции
            let first_bm = sec.biomes[0];
            let bm_uniform = sec.biomes.iter().all(|&bm| bm == first_bm);
            let bm_same_as_prev = s > 0 && sec.biomes == resolved_biomes[s - 1];

            if bm_same_as_prev {
                raw.push(0); // Повтор биома секции снизу
            } else if bm_uniform {
                raw.push(1); // Однородный биом
                write_varu32(&mut raw, biome_to_pal[&first_bm] as u32);
            } else {
                raw.push(2); // Разнородные биомы
                for &bm in &sec.biomes {
                    write_varu32(&mut raw, biome_to_pal[&bm] as u32);
                }
            }
            resolved_biomes.push(sec.biomes.clone());

            // 2. Кодирование блоков секции
            let all_air = sec.blocks.iter().all(|&b| b == 0);
            let first_b = sec.blocks[0];
            let all_same = !all_air && sec.blocks.iter().all(|&b| b == first_b);

            let mut copy_from = None;
            if !all_air && !all_same {
                for (prev_i, rb) in resolved_blocks.iter().enumerate().take(s) {
                    if sec.blocks == *rb {
                        copy_from = Some(prev_i);
                        break;
                    }
                }
            }

            if all_air {
                raw.push(0); // Секция полностью из воздуха
                resolved_blocks.push(vec![0u32; SECTION_BLOCKS]);
            } else if all_same {
                raw.push(1); // Монолитная секция (камень, сланец и т.д.)
                let pal_idx = block_to_pal[&first_b];
                write_varu32(&mut raw, pal_idx);
                resolved_blocks.push(vec![first_b; SECTION_BLOCKS]);
            } else if let Some(prev_i) = copy_from {
                raw.push(2); // Полная копия предыдущей секции
                write_varu32(&mut raw, prev_i as u32);
                let copied = resolved_blocks[prev_i].clone();
                resolved_blocks.push(copied);
            } else {
                raw.push(3); // 3D-пространственное адаптивное предсказание
                let prev_b = if s > 0 { Some(&resolved_blocks[s - 1][..]) } else { None };
                encode_predictive_blocks(&mut raw, &sec.blocks, prev_b, &block_palette, &block_to_pal);
                resolved_blocks.push(sec.blocks.clone());
            }
        }

        // Финальное сжатие DEFLATE на максимальном уровне
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&raw).expect("deflate write failed");
        encoder.finish().expect("deflate finish failed")
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut decoder = DeflateDecoder::new(bytes);
        let mut raw = Vec::new();
        decoder.read_to_end(&mut raw)?;

        let mut offset = 0;
        let section_min_y = read_vari32(&raw, &mut offset)?;
        let num_sections = read_varu32(&raw, &mut offset)? as usize;

        // Восстановление палитры блоков
        let block_pal_len = read_varu32(&raw, &mut offset)? as usize;
        let mut block_palette = Vec::with_capacity(block_pal_len);
        for _ in 0..block_pal_len {
            block_palette.push(read_varu32(&raw, &mut offset)?);
        }

        // Восстановление палитры биомов
        let biome_pal_len = read_varu32(&raw, &mut offset)? as usize;
        let mut biome_palette = Vec::with_capacity(biome_pal_len);
        for _ in 0..biome_pal_len {
            biome_palette.push(read_varu32(&raw, &mut offset)? as u16);
        }

        let mut sections: Vec<Section> = Vec::with_capacity(num_sections);

        for s in 0..num_sections {
            if offset >= raw.len() {
                bail!("unexpected EOF in section header");
            }

            // 1. Декодирование биомов
            let bm_mode = raw[offset];
            offset += 1;

            let biomes = match bm_mode {
                0 => {
                    if s == 0 {
                        bail!("cannot copy biomes for first section");
                    }
                    sections[s - 1].biomes.clone()
                }
                1 => {
                    let pal_idx = read_varu32(&raw, &mut offset)? as usize;
                    if pal_idx >= biome_palette.len() {
                        bail!("biome palette index out of bounds");
                    }
                    vec![biome_palette[pal_idx]; 64]
                }
                2 => {
                    let mut bms = Vec::with_capacity(64);
                    for _ in 0..64 {
                        let pal_idx = read_varu32(&raw, &mut offset)? as usize;
                        if pal_idx >= biome_palette.len() {
                            bail!("biome palette index out of bounds");
                        }
                        bms.push(biome_palette[pal_idx]);
                    }
                    bms
                }
                _ => bail!("unknown biome mode: {}", bm_mode),
            };

            // 2. Декодирование блоков
            if offset >= raw.len() {
                bail!("unexpected EOF in block mode");
            }
            let block_mode = raw[offset];
            offset += 1;

            let blocks = match block_mode {
                0 => vec![0u32; SECTION_BLOCKS],
                1 => {
                    let pal_idx = read_varu32(&raw, &mut offset)? as usize;
                    if pal_idx >= block_palette.len() {
                        bail!("block palette index out of bounds");
                    }
                    vec![block_palette[pal_idx]; SECTION_BLOCKS]
                }
                2 => {
                    let prev_i = read_varu32(&raw, &mut offset)? as usize;
                    if prev_i >= s {
                        bail!("invalid copy section index");
                    }
                    sections[prev_i].blocks.clone()
                }
                3 => {
                    let prev_b = if s > 0 { Some(&sections[s - 1].blocks[..]) } else { None };
                    decode_predictive_blocks(&raw, &mut offset, prev_b, &block_palette)?
                }
                _ => bail!("unknown block mode: {}", block_mode),
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

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "predictive",
    description: "Экстремальное сжатие чанков: 3D-пространственный причинный предиктор вокселей, дедупликация секций, единая частотная палитра и DEFLATE Level 9.",
    make: || Box::new(Apex3dCodec) as Box<dyn Codec>,
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

    #[test]
    fn roundtrip_large_palettes() {
        let mut reg = BlockRegistry::new();
        let mut bio = BiomeRegistry::new();
        let mut ids = Vec::new();
        for i in 0..400 {
            ids.push(reg.intern(&format!("minecraft:b{i}")));
        }
        for i in 0..512 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };
        let mut sections = Vec::new();
        for s in 0..6 {
            let mut sec = Section::air();
            for i in 0..SECTION_BLOCKS {
                sec.blocks[i] = ids[(i * 7 + s * 11) % ids.len()];
            }
            for (i, b) in sec.biomes.iter_mut().enumerate() {
                *b = (s * 64 + i) as u16;
            }
            sections.push(sec);
        }
        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = Apex3dCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}