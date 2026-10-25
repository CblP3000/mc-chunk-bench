//! Rawest format: every block as a 32-bit little-endian integer.
//! Fastest possible encode/decode, terrible compression (reference point).

use crate::chunk::{Chunk, Section, SECTION_BIOMES, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub struct RawCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "raw",
    description: "Без сжатия: блок = 4 байта. Эталон скорости, верхняя граница размера.",
    make: || Box::new(RawCodec) as Box<dyn Codec>,
}];

impl Codec for RawCodec {
    fn id(&self) -> String {
        "raw".to_string()
    }

    fn name(&self) -> String {
        "RAW u32 blocks".to_string()
    }


    fn description(&self) -> String {
        "No compression: 4 bytes per block + 2 bytes per biome. Upper bound \
         for size, reference for speed."
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::with_capacity(chunk.sections.len() * (SECTION_BLOCKS * 4 + SECTION_BIOMES * 2) + 3);
        out.push(chunk.sections.len() as u8);
        out.push((chunk.section_min_y as i8) as u8);
        for sec in &chunk.sections {
            for &b in &sec.blocks {
                out.extend_from_slice(&b.to_le_bytes());
            }
            for &b in &sec.biomes {
                out.extend_from_slice(&b.to_le_bytes());
            }
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.len() < 2 {
            bail!("raw stream too short");
        }
        let n = bytes[0] as usize;
        let min_y = bytes[1] as i8 as i32;
        let per_sec = SECTION_BLOCKS * 4 + SECTION_BIOMES * 2;
        if bytes.len() < 2 + n * per_sec {
            bail!("raw stream truncated");
        }
        let mut sections = Vec::with_capacity(n);
        let mut pos = 2usize;
        for _ in 0..n {
            let mut blocks = Vec::with_capacity(SECTION_BLOCKS);
            for _ in 0..SECTION_BLOCKS {
                blocks.push(u32::from_le_bytes([
                    bytes[pos],
                    bytes[pos + 1],
                    bytes[pos + 2],
                    bytes[pos + 3],
                ]));
                pos += 4;
            }
            let mut biomes = Vec::with_capacity(SECTION_BIOMES);
            for _ in 0..SECTION_BIOMES {
                biomes.push(u16::from_le_bytes([bytes[pos], bytes[pos + 1]]));
                pos += 2;
            }
            sections.push(Section { blocks, biomes });
        }
        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y: min_y,
            sections,
        }])
    }
}
