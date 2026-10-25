use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

// ---------- varint ----------

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            break;
        }
    }
}

fn read_varint(data: &[u8], pos: &mut usize) -> Result<u32> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            bail!("varint: unexpected eof");
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 35 {
            bail!("varint too long");
        }
    }
    Ok(result)
}

// ---------- bit packing ----------

struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), cur: 0, nbits: 0 }
    }

    fn write_bits(&mut self, value: u32, bits: u8) {
        for i in 0..bits {
            let bit = (value >> i) & 1;
            self.cur |= (bit as u8) << self.nbits;
            self.nbits += 1;
            if self.nbits == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitpos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bitpos: 0 }
    }

    fn read_bits(&mut self, bits: u8) -> Result<u32> {
        let mut result: u32 = 0;
        for i in 0..bits {
            if self.pos >= self.data.len() {
                bail!("bitreader: unexpected eof");
            }
            let bit = (self.data[self.pos] >> self.bitpos) & 1;
            result |= (bit as u32) << i;
            self.bitpos += 1;
            if self.bitpos == 8 {
                self.bitpos = 0;
                self.pos += 1;
            }
        }
        Ok(result)
    }
}

fn bits_needed(n: usize) -> u8 {
    if n <= 1 {
        return 1;
    }
    let mut bits = 0u8;
    while (1usize << bits) < n {
        bits += 1;
    }
    bits.max(1)
}

// ---------- palette encoding (generic, values as u32) ----------

fn encode_palette(values: &[u32], out: &mut Vec<u8>) {
    let mut palette: Vec<u32> = Vec::new();
    let mut index_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for &v in values {
        if !index_map.contains_key(&v) {
            index_map.insert(v, palette.len() as u32);
            palette.push(v);
        }
    }

    if palette.len() == 1 {
        out.push(0u8);
        write_varint(out, palette[0]);
    } else {
        out.push(1u8);
        write_varint(out, palette.len() as u32);
        for &p in &palette {
            write_varint(out, p);
        }
        let bits = bits_needed(palette.len());
        out.push(bits);
        let mut bw = BitWriter::new();
        for &v in values {
            let idx = *index_map.get(&v).unwrap();
            bw.write_bits(idx, bits);
        }
        let packed = bw.finish();
        write_varint(out, packed.len() as u32);
        out.extend_from_slice(&packed);
    }
}

fn decode_palette(data: &[u8], pos: &mut usize, n: usize) -> Result<Vec<u32>> {
    if *pos >= data.len() {
        bail!("decode_palette: unexpected eof");
    }
    let marker = data[*pos];
    *pos += 1;

    if marker == 0 {
        let v = read_varint(data, pos)?;
        Ok(vec![v; n])
    } else {
        let palette_len = read_varint(data, pos)? as usize;
        let mut palette = Vec::with_capacity(palette_len);
        for _ in 0..palette_len {
            palette.push(read_varint(data, pos)?);
        }
        if *pos >= data.len() {
            bail!("decode_palette: unexpected eof (bits byte)");
        }
        let bits = data[*pos];
        *pos += 1;
        let packed_len = read_varint(data, pos)? as usize;
        if *pos + packed_len > data.len() {
            bail!("decode_palette: packed data out of bounds");
        }
        let packed = &data[*pos..*pos + packed_len];
        *pos += packed_len;

        let mut br = BitReader::new(packed);
        let mut result = Vec::with_capacity(n);
        for _ in 0..n {
            let idx = br.read_bits(bits)? as usize;
            if idx >= palette.len() {
                bail!("decode_palette: index out of range");
            }
            result.push(palette[idx]);
        }
        Ok(result)
    }
}

// ---------- PackBits-style generic RLE ----------

fn packbits_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let n = data.len();
    let mut i = 0usize;
    while i < n {
        let mut run_len = 1usize;
        while i + run_len < n && data[i + run_len] == data[i] && run_len < 128 {
            run_len += 1;
        }
        if run_len >= 2 {
            let control = (257 - run_len) as u8; // run_len 2..=128 -> control 129..=255
            out.push(control);
            out.push(data[i]);
            i += run_len;
        } else {
            let start = i;
            let mut lit_len = 1usize;
            i += 1;
            while i < n && lit_len < 128 {
                if i + 1 < n && data[i] == data[i + 1] {
                    break;
                }
                lit_len += 1;
                i += 1;
            }
            let control = (lit_len - 1) as u8; // 0..=127
            out.push(control);
            out.extend_from_slice(&data[start..start + lit_len]);
        }
    }
    out
}

fn packbits_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let control = data[i];
        i += 1;
        if control < 128 {
            let len = control as usize + 1;
            if i + len > data.len() {
                bail!("packbits: literal run out of bounds");
            }
            out.extend_from_slice(&data[i..i + len]);
            i += len;
        } else if control > 128 {
            let run_len = 257 - control as usize;
            if i >= data.len() {
                bail!("packbits: repeat run out of bounds");
            }
            let byte = data[i];
            i += 1;
            out.extend(std::iter::repeat(byte).take(run_len));
        }
        // control == 128: no-op, never emitted by our encoder
    }
    Ok(out)
}

// ---------- Codec ----------

pub struct PaletteRleCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "palette-rle",
    description: "Палитра уникальных блоков/биомов на секцию с битовой упаковкой индексов минимальной ширины (как в формате Anvil), поверх которой применяется лёгкий универсальный RLE (PackBits) для дополнительного сжатия однородных участков (воздух, сплошная порода, плоские биомы). Хорошее соотношение простоты реализации и степени сжатия.",
    make: || Box::new(PaletteRleCodec) as Box<dyn Codec>,
}];

impl Codec for PaletteRleCodec {
    fn id(&self) -> String {
        "palette-rle-packbits".to_string()
    }

    fn name(&self) -> String {
        "Palette + PackBits RLE".to_string()
    }

    fn description(&self) -> String {
        "Палитра блоков/биомов с битовой упаковкой индексов + PackBits RLE поверх потока.".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut raw = Vec::new();
        raw.extend_from_slice(&chunk.section_min_y.to_le_bytes());
        write_varint(&mut raw, chunk.sections.len() as u32);

        for section in &chunk.sections {
            encode_palette(&section.blocks, &mut raw);
            let biomes_u32: Vec<u32> = section.biomes.iter().map(|&b| b as u32).collect();
            encode_palette(&biomes_u32, &mut raw);
        }

        packbits_encode(&raw)
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let raw = packbits_decode(bytes)?;
        if raw.len() < 4 {
            bail!("decode_tile: header too short");
        }
        let section_min_y = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let mut pos = 4usize;
        let num_sections = read_varint(&raw, &mut pos)? as usize;

        let mut sections = Vec::with_capacity(num_sections);
        for _ in 0..num_sections {
            let blocks = decode_palette(&raw, &mut pos, 4096)?;
            let biomes_u32 = decode_palette(&raw, &mut pos, 64)?;
            let biomes: Vec<u16> = biomes_u32.iter().map(|&v| v as u16).collect();
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
        for i in 0..8 {
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
        let mut rng = 12345u64;
        let mut noise = Section::air();
        for v in noise.blocks.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = 260 + (rng % 40) as u32;
        }
        sections.push(noise);
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