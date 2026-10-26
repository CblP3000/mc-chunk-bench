use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::Result;

pub struct MyAlgoCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "adaptive",
    description: "Адаптивный алгоритм: для каждой секции выбирает лучший из RLE, битовой упаковки с палитрой или сырых данных.",
    make: || Box::new(MyAlgoCodec) as Box<dyn Codec>,
}];

// ============================================================================
// Вспомогательные функции записи/чтения
// ============================================================================

fn write_val(out: &mut Vec<u8>, v: u32, is_u16: bool) {
    if is_u16 {
        out.extend_from_slice(&(v as u16).to_le_bytes());
    } else {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn read_val(data: &[u8], pos: &mut usize, is_u16: bool) -> u32 {
    if is_u16 {
        let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
        *pos += 2;
        v as u32
    } else {
        let v = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
        *pos += 4;
        v
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn read_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    result
}

// ============================================================================
// Кодирование и декодирование одной секции (блоков или биомов)
// ============================================================================

fn encode_section(values: &[u32], _len: usize, is_u16: bool) -> Vec<u8> {
    // Считаем уникальные значения и последовательности (runs)
    let mut runs = Vec::new();
    let mut unique = Vec::new();
    let mut current = values[0];
    let mut count = 1;
    unique.push(current);
    for &v in &values[1..] {
        if v == current {
            count += 1;
        } else {
            runs.push((current, count));
            current = v;
            count = 1;
            if !unique.contains(&v) {
                unique.push(v);
            }
        }
    }
    runs.push((current, count));
    let u = unique.len();

    let mut best: (u8, Vec<u8>, usize) = (0, Vec::new(), usize::MAX);

    // Режим 0: Все значения одинаковые
    if u == 1 {
        let mut buf = Vec::new();
        buf.push(0);
        write_val(&mut buf, values[0], is_u16);
        let l = buf.len();
        best = (0, buf, l);
    }

    // Режим 1: Сырые данные
    let mut buf = Vec::new();
    buf.push(1);
    for &v in values {
        write_val(&mut buf, v, is_u16);
    }
    let l = buf.len();
    if l < best.2 {
        best = (1, buf, l);
    }

    // Режим 2: Палитра + битовая упаковка
    if u > 1 {
        let mut buf = Vec::new();
        buf.push(2);
        buf.extend_from_slice(&(u as u16).to_le_bytes());
        for &v in &unique {
            write_val(&mut buf, v, is_u16);
        }
        let u_u32 = u as u32;
        let bits = (31 - u_u32.next_power_of_two().leading_zeros()) as usize;
        let bits = if bits == 0 { 1 } else { bits };
        let mut packed = Vec::new();
        let mut bit_buf: u64 = 0;
        let mut bit_count: u32 = 0;
        for &v in values {
            let idx = unique.iter().position(|&x| x == v).unwrap();
            bit_buf |= (idx as u64) << bit_count;
            bit_count += bits as u32;
            while bit_count >= 8 {
                packed.push((bit_buf & 0xFF) as u8);
                bit_buf >>= 8;
                bit_count -= 8;
            }
        }
        if bit_count > 0 {
            packed.push((bit_buf & 0xFF) as u8);
        }
        buf.extend_from_slice(&packed);
        let l = buf.len();
        if l < best.2 {
            best = (2, buf, l);
        }
    }

    // Режим 3: RLE без палитры
    let mut buf = Vec::new();
    buf.push(3);
    for &(v, c) in &runs {
        write_varint(&mut buf, v as u64);
        write_varint(&mut buf, c as u64);
    }
    let l = buf.len();
    if l < best.2 {
        best = (3, buf, l);
    }

    // Режим 4: Палитра + RLE
    if u > 1 {
        let mut buf = Vec::new();
        buf.push(4);
        buf.extend_from_slice(&(u as u16).to_le_bytes());
        for &v in &unique {
            write_val(&mut buf, v, is_u16);
        }
        for &(v, c) in &runs {
            let idx = unique.iter().position(|&x| x == v).unwrap();
            write_varint(&mut buf, idx as u64);
            write_varint(&mut buf, c as u64);
        }
        let l = buf.len();
        if l < best.2 {
            best = (4, buf, l);
        }
    }

    best.1
}

fn decode_section(pos: &mut usize, data: &[u8], len: usize, is_u16: bool) -> Result<Vec<u32>> {
    let mode = data[*pos];
    *pos += 1;
    let out = match mode {
        0 => {
            let v = read_val(data, pos, is_u16);
            vec![v; len]
        }
        1 => {
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                v.push(read_val(data, pos, is_u16));
            }
            v
        }
        2 => {
            let u = u16::from_le_bytes([data[*pos], data[*pos + 1]]) as usize;
            *pos += 2;
            let mut palette = Vec::with_capacity(u);
            for _ in 0..u {
                palette.push(read_val(data, pos, is_u16));
            }
            let bits = (31 - (u as u32).next_power_of_two().leading_zeros()) as usize;
            let bits = if bits == 0 { 1 } else { bits };
            let mut bit_buf: u64 = 0;
            let mut bit_count: u32 = 0;
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                while bit_count < bits as u32 {
                    let b = data[*pos];
                    *pos += 1;
                    bit_buf |= (b as u64) << bit_count;
                    bit_count += 8;
                }
                let idx = (bit_buf & ((1u64 << bits) - 1)) as usize;
                bit_buf >>= bits;
                bit_count -= bits as u32;
                v.push(palette[idx]);
            }
            v
        }
        3 => {
            let mut v = Vec::with_capacity(len);
            while v.len() < len {
                let val = read_varint(data, pos) as u32;
                let c = read_varint(data, pos) as usize;
                for _ in 0..c {
                    v.push(val);
                }
            }
            v
        }
        4 => {
            let u = u16::from_le_bytes([data[*pos], data[*pos + 1]]) as usize;
            *pos += 2;
            let mut palette = Vec::with_capacity(u);
            for _ in 0..u {
                palette.push(read_val(data, pos, is_u16));
            }
            let mut v = Vec::with_capacity(len);
            while v.len() < len {
                let idx = read_varint(data, pos) as usize;
                let c = read_varint(data, pos) as usize;
                for _ in 0..c {
                    v.push(palette[idx]);
                }
            }
            v
        }
        _ => {
            anyhow::bail!("Invalid section mode: {}", mode);
        }
    };
    Ok(out)
}

// ============================================================================
// Реализация трейта Codec
// ============================================================================

impl Codec for MyAlgoCodec {
    fn id(&self) -> String {
        "adapt-rle-pack".to_string()
    }

    fn name(&self) -> String {
        "Adaptive RLE + Palette".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::new();

        // Заголовок чанка
        out.extend_from_slice(&chunk.section_min_y.to_le_bytes());
        let s_count = chunk.sections.len() as u16;
        out.extend_from_slice(&s_count.to_le_bytes());

        // Секции
        for sec in &chunk.sections {
            let blocks = encode_section(&sec.blocks, 4096, false);
            out.extend_from_slice(&blocks);

            let biome_u32: Vec<u32> = sec.biomes.iter().map(|&b| b as u32).collect();
            let biomes = encode_section(&biome_u32, 64, true);
            out.extend_from_slice(&biomes);
        }

        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut pos = 0;

        let section_min_y =
            i32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        pos += 4;
        let s_count = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;

        let mut sections = Vec::with_capacity(s_count);
        for _ in 0..s_count {
            let blocks = decode_section(&mut pos, bytes, 4096, false)?;
            let biomes_u32 = decode_section(&mut pos, bytes, 64, true)?;
            let biomes: Vec<u16> = biomes_u32.iter().map(|&v| v as u16).collect();

            sections.push(Section { blocks, biomes });
        }

        let chunk = Chunk {
            x: 0,
            z: 0,
            section_min_y,
            sections,
        };

        Ok(vec![chunk])
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
        let codec = MyAlgoCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}