//! quack!!!!

use crate::chunk::{Chunk, Section, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub struct QuackCodec;

impl Codec for QuackCodec {
    fn id(&self) -> String {
        "quack".to_string()
    }

    fn name(&self) -> String {
        "Quack".to_string()
    }

    fn description(&self) -> String {
        "quack! quack!"
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::new();
        out.push(chunk.sections.len() as u8);
        out.push((chunk.section_min_y as i8) as u8);

        // палитра
        let mut palette: Vec<u32> = Vec::new();
        let mut idx: HashMap<u32, u8> = HashMap::new();
        for sec in &chunk.sections {
            for &b in &sec.blocks {
                if !idx.contains_key(&b) {
                    idx.insert(b, palette.len() as u8);
                    palette.push(b);
                }
            }
        }
        out.push(palette.len() as u8);
        for id in &palette {
            let name = &ctx.registry.names[*id as usize];
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        // индексы
        for sec in &chunk.sections {
            for &b in &sec.blocks {
                out.push(idx[&b]);
            }
        }
        // биомы (просто, по 2 байта на секцию)
        for sec in &chunk.sections {
            for &bi in &sec.biomes {
                out.extend_from_slice(&bi.to_le_bytes());
            }
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut p = 0usize;
        let n_sec = bytes[p] as usize;
        p += 1;
        let min_y = bytes[p] as i8 as i32;
        p += 1;
        let n_pal = bytes[p] as usize;
        p += 1;
        let mut palette = Vec::with_capacity(n_pal);
        for _ in 0..n_pal {
            let l = bytes[p] as usize;
            p += 1;
            let name = std::str::from_utf8(&bytes[p..p + l])?;
            p += l;
            palette.push(ctx.registry.get(name).unwrap_or(0));
        }
        let mut sections = Vec::with_capacity(n_sec);
        for _ in 0..n_sec {
            let mut blocks = Vec::with_capacity(SECTION_BLOCKS);
            for _ in 0..SECTION_BLOCKS {
                blocks.push(palette[bytes[p] as usize]);
                p += 1;
            }
            let mut biomes = Vec::with_capacity(64);
            for _ in 0..64 {
                biomes.push(u16::from_le_bytes([bytes[p], bytes[p + 1]]));
                p += 2;
            }
            sections.push(Section { blocks, biomes });
        }
        if p != bytes.len() {
            bail!("trailing bytes");
        }
        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y: min_y,
            sections,
        }])
    }
}


pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "adaptive",
    description: "quak! quak!",
    make: || Box::new(QuackCodec) as Box<dyn Codec>,
}];
