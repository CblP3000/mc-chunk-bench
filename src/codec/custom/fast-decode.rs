use crate::chunk::{Chunk, Section, SECTION_BIOMES, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub struct FastDecodeCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "palette-rle",
    description: "Palette + bit-packed RLE optimized for very fast decoding with good compression. \
                  Uses per-section palettes with minimal bit-width indices and run-length encoding \
                  on palette indices for both blocks and biomes.",
    make: || Box::new(FastDecodeCodec) as Box<dyn crate::codec::Codec>,
}];

impl Codec for FastDecodeCodec {
    fn id(&self) -> String {
        "fast-decode".to_string()
    }
    fn name(&self) -> String {
        "Fast Decode".to_string()
    }
    fn description(&self) -> String {
        "Palette + bit-packed RLE, optimized for very fast decoding with good compression.".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8> {
        let _ = ctx;
        let chunk = chunks[0];
        let mut out = Vec::with_capacity(8192);

        // Header: section_min_y (i32), num_sections (u16)
        write_i32(&mut out, chunk.section_min_y);
        write_u16(&mut out, chunk.sections.len() as u16);

        for section in &chunk.sections {
            // Encode blocks
            encode_palette_rle(&section.blocks, &mut out);
            // Encode biomes (as u32 for uniform handling)
            let biomes_u32: Vec<u32> = section.biomes.iter().map(|&b| b as u32).collect();
            encode_palette_rle(&biomes_u32, &mut out);
        }

        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut pos = 0;

        let section_min_y = read_i32(bytes, &mut pos)?;
        let num_sections = read_u16(bytes, &mut pos)? as usize;

        let mut sections = Vec::with_capacity(num_sections);

        for _ in 0..num_sections {
            let blocks = decode_palette_rle(bytes, &mut pos, SECTION_BLOCKS)?;
            let biomes_u32 = decode_palette_rle(bytes, &mut pos, SECTION_BIOMES)?;
            let biomes: Vec<u16> = biomes_u32.into_iter().map(|v| v as u16).collect();
            sections.push(Section { blocks, biomes });
        }

        if pos != bytes.len() {
            bail!(
                "Trailing bytes: consumed {} of {}",
                pos,
                bytes.len()
            );
        }

        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y,
            sections,
        }])
    }
}

// ---------- Encoding helpers ----------

/// Encode a data array using palette + RLE on palette indices.
/// Format:
///   palette_size: u32 (varint)
///   palette[0..palette_size]: u32 each (varint)
///   if palette_size == 1: nothing more (entire array is palette[0])
///   else:
///     num_runs: u32 (varint)
///     for each run:
///       run_length: u16 (varint-u16)
///       palette_index: packed in minimal bits (bit-packed stream)
///     bit-packed stream is byte-aligned at end
fn encode_palette_rle(data: &[u32], out: &mut Vec<u8>) {
    // Build palette
    let mut palette: Vec<u32> = Vec::new();
    let mut map = std::collections::HashMap::new();
    let mut indices = Vec::with_capacity(data.len());

    for &val in data {
        let idx = if let Some(&i) = map.get(&val) {
            i
        } else {
            let i = palette.len() as u32;
            palette.push(val);
            map.insert(val, i);
            i
        };
        indices.push(idx);
    }

    // Write palette
    write_varint(out, palette.len() as u32);
    for &val in &palette {
        write_varint(out, val);
    }

    if palette.len() == 1 {
        // Uniform section — nothing more needed
        return;
    }

    // RLE encode the indices
    let mut runs: Vec<(u16, u32)> = Vec::new(); // (length, palette_index)
    let mut i = 0;
    while i < indices.len() {
        let val = indices[i];
        let mut run_len = 1u32;
        while (i + run_len as usize) < indices.len()
            && indices[i + run_len as usize] == val
            && run_len < 65535
        {
            run_len += 1;
        }
        runs.push((run_len as u16, val));
        i += run_len as usize;
    }

    // Decide: raw indices vs RLE. Pick whichever is smaller.
    let bits_per_index = bits_needed(palette.len() as u32 - 1);

    let raw_bytes = (data.len() * bits_per_index as usize).div_ceil(8);
    // RLE cost: varint(num_runs) + for each run: varint(run_length) + bits_per_index bits
    // Approximate RLE bytes
    let rle_varint_bytes: usize = runs
        .iter()
        .map(|(len, _)| varint_size(*len as u32))
        .sum::<usize>();
    let rle_index_bits = runs.len() * bits_per_index as usize;
    let rle_bytes = varint_size(runs.len() as u32) + rle_varint_bytes + rle_index_bits.div_ceil(8);

    if runs.len() >= data.len() / 2 || rle_bytes >= raw_bytes {
        // Raw mode: flag 0
        out.push(0u8); // mode = raw
        write_bitpacked(out, &indices, bits_per_index);
    } else {
        // RLE mode: flag 1
        out.push(1u8); // mode = rle
        write_varint(out, runs.len() as u32);
        // Write run lengths as varints
        for &(len, _) in &runs {
            write_varint(out, len as u32);
        }
        // Write palette indices bit-packed
        let rle_indices: Vec<u32> = runs.iter().map(|&(_, idx)| idx).collect();
        write_bitpacked(out, &rle_indices, bits_per_index);
    }
}

fn write_bitpacked(out: &mut Vec<u8>, values: &[u32], bits: u8) {
    if bits == 0 {
        return;
    }
    let total_bits = values.len() * bits as usize;
    let total_bytes = total_bits.div_ceil(8);
    let start = out.len();
    out.resize(start + total_bytes, 0);

    let mut bit_offset = 0usize;
    for &val in values {
        let byte_idx = start + bit_offset / 8;
        let bit_idx = bit_offset % 8;

        // Write bits, potentially spanning up to 5 bytes for large bit widths
        let mut remaining = bits as usize;
        let mut v = val;
        let mut bo = bit_idx;
        let mut bi = byte_idx;

        while remaining > 0 {
            let space = 8 - bo;
            let write = remaining.min(space);
            let mask = ((1u32 << write) - 1) as u8;
            out[bi] |= ((v as u8) & mask) << bo;
            v >>= write;
            remaining -= write;
            bo = 0;
            bi += 1;
        }

        bit_offset += bits as usize;
    }
}

fn bits_needed(max_val: u32) -> u8 {
    if max_val == 0 {
        return 0;
    }
    32 - max_val.leading_zeros() as u8
}

fn write_varint(out: &mut Vec<u8>, mut val: u32) {
    loop {
        let mut byte = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if val == 0 {
            break;
        }
    }
}

fn varint_size(mut val: u32) -> usize {
    let mut size = 0;
    loop {
        size += 1;
        val >>= 7;
        if val == 0 {
            break;
        }
    }
    size
}

fn write_i32(out: &mut Vec<u8>, val: i32) {
    out.extend_from_slice(&val.to_le_bytes());
}

fn write_u16(out: &mut Vec<u8>, val: u16) {
    out.extend_from_slice(&val.to_le_bytes());
}

// ---------- Decoding helpers ----------

fn read_i32(bytes: &[u8], pos: &mut usize) -> Result<i32> {
    if *pos + 4 > bytes.len() {
        bail!("unexpected EOF reading i32");
    }
    let val = i32::from_le_bytes([
        bytes[*pos],
        bytes[*pos + 1],
        bytes[*pos + 2],
        bytes[*pos + 3],
    ]);
    *pos += 4;
    Ok(val)
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Result<u16> {
    if *pos + 2 > bytes.len() {
        bail!("unexpected EOF reading u16");
    }
    let val = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
    *pos += 2;
    Ok(val)
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut result = 0u32;
    let mut shift = 0;
    loop {
        if *pos >= bytes.len() {
            bail!("unexpected EOF reading varint");
        }
        let byte = bytes[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 35 {
            bail!("varint too long");
        }
    }
    Ok(result)
}

fn read_bitpacked(bytes: &[u8], pos: &mut usize, count: usize, bits: u8) -> Result<Vec<u32>> {
    if bits == 0 {
        return Ok(vec![0; count]);
    }
    let total_bits = count * bits as usize;
    let total_bytes = total_bits.div_ceil(8);
    if *pos + total_bytes > bytes.len() {
        bail!("unexpected EOF reading bitpacked data");
    }

    let mut result = Vec::with_capacity(count);
    let start = *pos;
    let mut bit_offset = 0usize;
    let mask = (1u32 << bits) - 1;

    for _ in 0..count {
        let mut val = 0u32;
        let mut remaining = bits as usize;
        let mut bo = bit_offset % 8;
        let mut bi = start + bit_offset / 8;
        let mut shift = 0;

        while remaining > 0 {
            let space = 8 - bo;
            let read = remaining.min(space);
            let byte_val = bytes[bi];
            val |= (((byte_val >> bo) as u32) & ((1u32 << read) - 1)) << shift;
            remaining -= read;
            shift += read;
            bo = 0;
            bi += 1;
        }

        result.push(val & mask);
        bit_offset += bits as usize;
    }

    *pos += total_bytes;
    Ok(result)
}

fn decode_palette_rle(bytes: &[u8], pos: &mut usize, expected_len: usize) -> Result<Vec<u32>> {
    // Read palette
    let palette_size = read_varint(bytes, pos)? as usize;
    if palette_size == 0 {
        bail!("empty palette");
    }
    let mut palette = Vec::with_capacity(palette_size);
    for _ in 0..palette_size {
        palette.push(read_varint(bytes, pos)?);
    }

    if palette_size == 1 {
        // Uniform
        return Ok(vec![palette[0]; expected_len]);
    }

    let bits = bits_needed(palette_size as u32 - 1);

    // Read mode
    if *pos >= bytes.len() {
        bail!("unexpected EOF reading mode");
    }
    let mode = bytes[*pos];
    *pos += 1;

    if mode == 0 {
        // Raw mode
        let indices = read_bitpacked(bytes, pos, expected_len, bits)?;
        let mut result = Vec::with_capacity(expected_len);
        for idx in indices {
            if idx as usize >= palette_size {
                bail!("palette index out of range");
            }
            result.push(palette[idx as usize]);
        }
        Ok(result)
    } else if mode == 1 {
        // RLE mode
        let num_runs = read_varint(bytes, pos)? as usize;
        // Read run lengths
        let mut run_lengths = Vec::with_capacity(num_runs);
        for _ in 0..num_runs {
            run_lengths.push(read_varint(bytes, pos)? as usize);
        }
        // Read palette indices
        let run_indices = read_bitpacked(bytes, pos, num_runs, bits)?;

        let mut result = Vec::with_capacity(expected_len);
        for i in 0..num_runs {
            let idx = run_indices[i] as usize;
            if idx >= palette_size {
                bail!("palette index out of range in RLE");
            }
            let val = palette[idx];
            let len = run_lengths[i];
            result.resize(result.len() + len, val);
        }

        if result.len() != expected_len {
            bail!(
                "RLE decoded {} elements, expected {}",
                result.len(),
                expected_len
            );
        }

        Ok(result)
    } else {
        bail!("unknown mode {}", mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::chunk::Section;
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
        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = FastDecodeCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}
