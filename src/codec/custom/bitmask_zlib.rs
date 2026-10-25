use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{Result, bail};
use std::io::{Read, Write};

pub struct BitmaskZlibCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "deflate",
    description: "Air-bitmask + zlib best: 4096-битная маска воздуха + только ненулевые блоки, затем zlib-лучшее сжатие. Отличное цена/качество для воздушных секций.",
    make: || Box::new(BitmaskZlibCodec) as Box<dyn Codec>,
}];

impl Codec for BitmaskZlibCodec {
    fn id(&self) -> String {
        "bitmask-zlib".to_string()
    }

    fn name(&self) -> String {
        "Bitmask Zlib".to_string()
    }

    fn description(&self) -> String {
        "Битмаска воздуха на секцию (4096 бит) + только ненулевые u32 ID блоков; биомы как есть; всё сжато zlib best.".to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];

        // Защита от невозможного размера (по контракту секций мало)
        assert!(
            chunk.sections.len() <= u16::MAX as usize,
            "too many sections for u16 header"
        );

        let mut raw = Vec::with_capacity(4096 * 4 + 1024);

        // Заголовок чанка
        raw.extend_from_slice(&chunk.section_min_y.to_le_bytes());
        raw.extend_from_slice(&(chunk.sections.len() as u16).to_le_bytes());

        for sec in &chunk.sections {
            // --- Биомы: 64 * u16 ---
            for &b in &sec.biomes {
                raw.extend_from_slice(&b.to_le_bytes());
            }

            // --- Блоки: битмаска 4096 бит + значения ---
            let mut bitmask = [0u8; 512];
            let mut values = Vec::with_capacity(sec.blocks.len() / 2);

            for (i, &block_id) in sec.blocks.iter().enumerate() {
                if block_id != 0 {
                    bitmask[i >> 3] |= 1 << (i & 7);
                    values.extend_from_slice(&block_id.to_le_bytes());
                }
            }

            raw.extend_from_slice(&bitmask);
            raw.extend_from_slice(&values);
        }

        // Сжатие zlib (уровень best = 9)
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&raw).expect("zlib write failed");
        enc.finish().expect("zlib finish failed")
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut dec = flate2::read::ZlibDecoder::new(bytes);
        let mut raw = Vec::new();
        dec.read_to_end(&mut raw)?;

        if raw.len() < 6 {
            bail!("stream too short: {} bytes", raw.len());
        }

        let mut p = 0usize;

        let section_min_y = i32::from_le_bytes([
            raw[p], raw[p + 1], raw[p + 2], raw[p + 3],
        ]);
        p += 4;

        let num_sections = u16::from_le_bytes([raw[p], raw[p + 1]]) as usize;
        p += 2;

        let mut sections: Vec<Section> = Vec::with_capacity(num_sections);

        for _ in 0..num_sections {
            // Биомы (128 байт)
            if p + 128 > raw.len() {
                bail!("truncated biomes at offset {}", p);
            }
            let mut biomes = Vec::with_capacity(64);
            for _ in 0..64 {
                let v = u16::from_le_bytes([raw[p], raw[p + 1]]);
                biomes.push(v);
                p += 2;
            }

            // Битмаска (512 байт = 4096 бит)
            if p + 512 > raw.len() {
                bail!("truncated bitmask at offset {}", p);
            }
            let mut bitmask = [0u8; 512];
            bitmask.copy_from_slice(&raw[p..p + 512]);
            p += 512;

            let non_air = bitmask.iter().map(|b| b.count_ones() as usize).sum::<usize>();

            // Значения блоков
            if p + non_air * 4 > raw.len() {
                bail!(
                    "truncated block values: need {} bytes, have {}",
                    non_air * 4,
                    raw.len() - p
                );
            }

            let mut blocks = vec![0u32; 4096];
            let mut val_idx = 0usize;

            for (i, b) in blocks.iter_mut().enumerate() {
                let byte_idx = i >> 3;
                let bit_idx = i & 7;
                if (bitmask[byte_idx] >> bit_idx) & 1 == 1 {
                    let v = u32::from_le_bytes([
                        raw[p + val_idx * 4],
                        raw[p + val_idx * 4 + 1],
                        raw[p + val_idx * 4 + 2],
                        raw[p + val_idx * 4 + 3],
                    ]);
                    *b = v;
                    val_idx += 1;
                }
            }

            p += non_air * 4;

            sections.push(Section { blocks, biomes });
        }

        if p != raw.len() {
            bail!(
                "leftover bytes after decode: expected end at {}, got {}",
                raw.len(),
                p
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

    #[test]
    fn roundtrip_air_and_blocks() {
        let mut reg = BlockRegistry::new();
        let mut bio = BiomeRegistry::new();
        let mut ids = Vec::new();
        for i in 0..30 {
            ids.push(reg.intern(&format!("minecraft:b{i}")));
        }
        for i in 0..4 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };
        let mut sections = Vec::new();
        let mut rng = 42u64;
        for s in 0..4 {
            let mut sec = Section::air();
            if s % 2 == 0 {
                for i in 0..4096 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    if !rng.is_multiple_of(4) {
                        sec.blocks[i] = ids[(rng % 30) as usize];
                    }
                }
            }
            for (i, b) in sec.biomes.iter_mut().enumerate() {
                *b = (i % 4) as u16;
            }
            sections.push(sec);
        }
        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = BitmaskZlibCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}