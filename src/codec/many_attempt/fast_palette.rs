use crate::chunk::Chunk;
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub struct FastPaletteCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "fast-palette",
    name: "Fast Palette",
    group: "palette",
    description: "Палитра + bit-packing с ультра-быстрым decode. Специальные пути для air и single-value секций. Максимальная скорость распаковки при хорошем сжатии.",
    make: || Box::new(FastPaletteCodec) as Box<dyn Codec>,
}];

impl Codec for FastPaletteCodec {
    fn id(&self) -> String {
        "fast-palette".to_string()
    }
    fn name(&self) -> String {
        "Fast Palette".to_string()
    }
    fn description(&self) -> String {
        "Оптимизированная палитра + bit-packing. Decode максимально быстрый (fill + простой bit-unpack).".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        assert_eq!(chunks.len(), 1);
        let chunk = chunks[0];
        let mut out = Vec::with_capacity(2048);

        // header
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
            bail!("extra bytes after decode");
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
// Section
// ---------------------------------------------------------------------------

fn encode_section(out: &mut Vec<u8>, section: &crate::chunk::Section) {
    // biomes always through the fast path
    encode_palette(out, &section.biomes.iter().map(|&b| b as u32).collect::<Vec<_>>());

    // blocks
    if section.blocks.iter().all(|&b| b == 0) {
        out.push(0); // empty (all air)
        return;
    }

    // check single value
    let first = section.blocks[0];
    if section.blocks.iter().all(|&b| b == first) {
        out.push(1); // single value
        write_var_u32(out, first);
        return;
    }

    out.push(2); // full palette
    encode_palette(out, &section.blocks);
}

fn decode_section(r: &mut Reader<'_>) -> Result<crate::chunk::Section> {
    // biomes
    let biomes_u32 = decode_palette(r, 64)?;
    let biomes: Vec<u16> = biomes_u32.into_iter().map(|v| v as u16).collect();

    // blocks
    let flag = r.read_u8()?;
    let blocks = match flag {
        0 => vec![0u32; 4096],
        1 => {
            let v = r.read_var_u32()?;
            vec![v; 4096]
        }
        2 => decode_palette(r, 4096)?,
        _ => bail!("invalid section flag: {}", flag),
    };

    Ok(crate::chunk::Section { blocks, biomes })
}

// ---------------------------------------------------------------------------
// Palette + bit-packing (core)
// ---------------------------------------------------------------------------

fn encode_palette(out: &mut Vec<u8>, data: &[u32]) {
    // build palette in order of appearance
    let mut palette = Vec::with_capacity(16);
    let mut map = std::collections::HashMap::with_capacity(32);
    let mut indices = Vec::with_capacity(data.len());

    for &v in data {
        let idx = match map.get(&v) {
            Some(&i) => i,
            None => {
                let i = palette.len() as u32;
                map.insert(v, i);
                palette.push(v);
                i
            }
        };
        indices.push(idx);
    }

    let pal_len = palette.len();
    write_var_u32(out, pal_len as u32);

    for &v in &palette {
        write_var_u32(out, v);
    }

    if pal_len <= 1 {
        return; // nothing to pack
    }

    let bits = bits_needed(pal_len);
    let mut bw = BitWriter::new();
    for &idx in &indices {
        bw.write_bits(idx as u64, bits);
    }
    let packed = bw.finish();
    write_var_u32(out, packed.len() as u32);
    out.extend_from_slice(&packed);
}

fn decode_palette(r: &mut Reader<'_>, expected: usize) -> Result<Vec<u32>> {
    let pal_len = r.read_var_u32()? as usize;
    if pal_len == 0 {
        bail!("empty palette");
    }

    let mut palette = Vec::with_capacity(pal_len);
    for _ in 0..pal_len {
        palette.push(r.read_var_u32()?);
    }

    if pal_len == 1 {
        return Ok(vec![palette[0]; expected]);
    }

    let packed_len = r.read_var_u32()? as usize;
    let packed = r.read_bytes(packed_len)?;

    let bits = bits_needed(pal_len);
    let mut br = BitReader::new(packed);
    let mut out = Vec::with_capacity(expected);

    for _ in 0..expected {
        let idx = br.read_bits(bits)? as usize;
        if idx >= pal_len {
            bail!("palette index out of range");
        }
        out.push(palette[idx]);
    }

    Ok(out)
}

fn bits_needed(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        (usize::BITS - (n - 1).leading_zeros()) as u32
    }
}

// ---------------------------------------------------------------------------
// Helpers (optimized for speed)
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
            buf: Vec::with_capacity(512),
            cur: 0,
            bits: 0,
        }
    }

    #[inline(always)]
    fn write_bits(&mut self, mut val: u64, mut n: u32) {
        while n > 0 {
            let space = 8 - self.bits;
            let take = space.min(n as u8);
            self.cur |= ((val & ((1u64 << take) - 1)) as u8) << self.bits;
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
    cur: u64,
    bits: u32,
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

    #[inline(always)]
    fn read_bits(&mut self, n: u32) -> Result<u64> {
        while self.bits < n {
            if self.pos >= self.data.len() {
                bail!("bit reader underflow");
            }
            self.cur |= (self.data[self.pos] as u64) << self.bits;
            self.pos += 1;
            self.bits += 8;
        }
        let mask = (1u64 << n) - 1;
        let result = self.cur & mask;
        self.cur >>= n;
        self.bits -= n;
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

    #[inline(always)]
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
        for b in &mut buf {
            *b = self.read_u8()?;
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
                return Ok(result);
            }
            shift += 7;
            if shift >= 35 {
                bail!("varint overflow");
            }
        }
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
        let codec = FastPaletteCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}