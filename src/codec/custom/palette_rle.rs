use crate::chunk::Chunk;
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub struct PaletteRleCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "palette-rle",
    description: "Палитра + bit-packing индексов + RLE. Отлично сжимает типичные секции Minecraft (мало уникальных блоков + длинные серии). Хорошее соотношение размер/скорость.",
    make: || Box::new(PaletteRleCodec) as Box<dyn Codec>,
}];

impl Codec for PaletteRleCodec {
    fn id(&self) -> String {
        "palette-rle".to_string()
    }
    fn name(&self) -> String {
        "Palette + RLE".to_string()
    }
    fn description(&self) -> String {
        "Палитра + bit-packing + RLE по индексам. Оптимален для воксельных данных с локальной однородностью.".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        assert_eq!(chunks.len(), 1);
        let chunk = chunks[0];
        let mut out = Vec::with_capacity(4096);

        // header: section_min_y (i32 LE) + num_sections (u16 LE)
        out.extend_from_slice(&chunk.section_min_y.to_le_bytes());
        out.extend_from_slice(&(chunk.sections.len() as u16).to_le_bytes());

        for section in &chunk.sections {
            encode_section(&mut out, section);
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut r = Reader::new(bytes);

        let section_min_y = r.read_i32()?;
        let num_sections = r.read_u16()? as usize;

        let mut sections = Vec::with_capacity(num_sections);
        for _ in 0..num_sections {
            sections.push(decode_section(&mut r)?);
        }

        if !r.is_empty() {
            bail!("extra bytes left after decode");
        }

        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y,
            sections,
        }])
    }
}

// ---------------------------------------------------------------------------
// Section encoding
// ---------------------------------------------------------------------------

fn encode_section(out: &mut Vec<u8>, section: &crate::chunk::Section) {
    // --- biomes (always 64) ---
    encode_palette_rle(out, &section.biomes.iter().map(|&b| b as u32).collect::<Vec<_>>());

    // --- blocks (4096) ---
    // special case: all air
    if section.blocks.iter().all(|&b| b == 0) {
        out.push(0); // flag: empty
        return;
    }
    out.push(1); // flag: has data
    encode_palette_rle(out, &section.blocks);
}

fn decode_section(r: &mut Reader<'_>) -> Result<crate::chunk::Section> {
    // biomes
    let biomes_u32 = decode_palette_rle(r, 64)?;
    let biomes: Vec<u16> = biomes_u32.into_iter().map(|v| v as u16).collect();

    // blocks
    let flag = r.read_u8()?;
    let blocks = if flag == 0 {
        vec![0u32; 4096]
    } else if flag == 1 {
        decode_palette_rle(r, 4096)?
    } else {
        bail!("invalid section flag {}", flag);
    };

    Ok(crate::chunk::Section { blocks, biomes })
}

// ---------------------------------------------------------------------------
// Core: palette + RLE + bit-packing
// ---------------------------------------------------------------------------

fn encode_palette_rle(out: &mut Vec<u8>, data: &[u32]) {
    // 1. build palette (order of first appearance)
    let mut palette = Vec::new();
    let mut map = std::collections::HashMap::new();
    let mut indices = Vec::with_capacity(data.len());

    for &v in data {
        let idx = if let Some(&i) = map.get(&v) {
            i
        } else {
            let i = palette.len() as u32;
            map.insert(v, i);
            palette.push(v);
            i
        };
        indices.push(idx);
    }

    let pal_len = palette.len();
    write_var_u32(out, pal_len as u32);

    // write palette values
    for &v in &palette {
        write_var_u32(out, v);
    }

    if pal_len <= 1 {
        // all identical — nothing more to write
        return;
    }

    // 2. RLE on indices
    let mut runs: Vec<(u32, u32)> = Vec::new(); // (value, length)
    let mut i = 0;
    while i < indices.len() {
        let v = indices[i];
        let mut len = 1u32;
        while (i + len as usize) < indices.len() && indices[i + len as usize] == v {
            len += 1;
        }
        runs.push((v, len));
        i += len as usize;
    }

    write_var_u32(out, runs.len() as u32);

    // 3. bit-pack the runs: value (bits for palette) + length (varint)
    let bits = bits_needed(pal_len);
    let mut bw = BitWriter::new();
    for &(val, _len) in &runs {
        bw.write_bits(val as u64, bits);
    }
    let bit_bytes = bw.finish();
    write_var_u32(out, bit_bytes.len() as u32);
    out.extend_from_slice(&bit_bytes);

    // lengths as varints
    for &(_, len) in &runs {
        write_var_u32(out, len);
    }
}

fn decode_palette_rle(r: &mut Reader<'_>, expected_len: usize) -> Result<Vec<u32>> {
    let pal_len = r.read_var_u32()? as usize;
    if pal_len == 0 {
        bail!("empty palette");
    }

    let mut palette = Vec::with_capacity(pal_len);
    for _ in 0..pal_len {
        palette.push(r.read_var_u32()?);
    }

    if pal_len == 1 {
        return Ok(vec![palette[0]; expected_len]);
    }

    let num_runs = r.read_var_u32()? as usize;
    let bit_len = r.read_var_u32()? as usize;
    let bit_bytes = r.read_bytes(bit_len)?;

    let bits = bits_needed(pal_len);
    let mut br = BitReader::new(bit_bytes);

    let mut result = Vec::with_capacity(expected_len);
    for _ in 0..num_runs {
        let val = br.read_bits(bits)? as u32;
        if val as usize >= pal_len {
            bail!("palette index out of range");
        }
        let len = r.read_var_u32()? as usize;
        let block = palette[val as usize];
        for _ in 0..len {
            result.push(block);
        }
    }

    if result.len() != expected_len {
        bail!("decoded length {} != expected {}", result.len(), expected_len);
    }
    Ok(result)
}

fn bits_needed(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        (usize::BITS - (n - 1).leading_zeros()) as u32
    }
}

// ---------------------------------------------------------------------------
// Varint + Bit helpers
// ---------------------------------------------------------------------------

fn write_var_u32(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    bits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cur: 0,
            bits: 0,
        }
    }
    fn write_bits(&mut self, mut val: u64, mut n: u32) {
        while n > 0 {
            let space = 8 - self.bits;
            let take = space.min(n as u8);
            let mask = (1u64 << take) - 1;
            self.cur |= ((val & mask) as u8) << self.bits;
            self.bits += take;
            val >>= take;
            n -= take as u32;
            if self.bits == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.bits = 0;
            }
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.buf.push(self.cur);
        }
        self.buf
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    cur: u8,
    bits: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            cur: 0,
            bits: 0,
        }
    }
    fn read_bits(&mut self, mut n: u32) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        while n > 0 {
            if self.bits == 0 {
                if self.pos >= self.data.len() {
                    bail!("bit reader underflow");
                }
                self.cur = self.data[self.pos];
                self.pos += 1;
                self.bits = 8;
            }
            let take = self.bits.min(n as u8);
            let mask = if take == 8 { 0xFFu8 } else { (1u8 << take) - 1 };
            result |= ((self.cur & mask) as u64) << shift;
            self.cur >>= take;
            self.bits -= take;
            shift += take as u32;
            n -= take as u32;
        }
        Ok(result)
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }
    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            bail!("unexpected eof");
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }
    fn read_u16(&mut self) -> Result<u16> {
        let b0 = self.read_u8()? as u16;
        let b1 = self.read_u8()? as u16;
        Ok(b0 | (b1 << 8))
    }
    fn read_i32(&mut self) -> Result<i32> {
        let mut buf = [0u8; 4];
        for i in 0..4 {
            buf[i] = self.read_u8()?;
        }
        Ok(i32::from_le_bytes(buf))
    }
    fn read_var_u32(&mut self) -> Result<u32> {
        let mut result = 0u32;
        let mut shift = 0;
        loop {
            let b = self.read_u8()?;
            result |= ((b & 0x7F) as u32) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                bail!("varint too long");
            }
        }
        Ok(result)
    }
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("unexpected eof");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
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
        for i in 0..8 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };

        let mut sections = Vec::new();

        // все air
        sections.push(Section::air());

        // один блок целиком
        let mut mono = Section::air();
        for b in mono.blocks.iter_mut() {
            *b = 7;
        }
        sections.push(mono);

        // палитра > 256 + шум (проверка 8-битных индексов)
        let mut rng = 12345u64;
        let mut noise = Section::air();
        for v in noise.blocks.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = 260 + (rng % 40) as u32;
        }
        sections.push(noise);

        // длинные серии
        let mut runs = Section::air();
        let mut i = 0;
        let mut v = 1u32;
        while i < 4096 {
            let len = (i % 500) + 1;
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
        let codec = PaletteRleCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}