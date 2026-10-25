//! Column-wise Run-Length Encoding: for each (x, z) column, runs of equal
//! blocks along Y. Fast, exploits the columnar structure of Minecraft terrain.

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

pub struct ColumnRleCodec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "rle",
    description: "RLE по вертикальным колонкам (x, z): длинные серии камня/земли/воздуха сжимаются в пары «длина, блок». Быстро и просто.",
    make: || Box::new(ColumnRleCodec) as Box<dyn Codec>,
}];

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut v = 0u32;
    let mut shift = 0u32;
    loop {
        if *pos >= bytes.len() {
            bail!("varint overflow");
        }
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 28 {
            bail!("varint too long");
        }
    }
}

impl Codec for ColumnRleCodec {
    fn id(&self) -> String {
        "rle-column".to_string()
    }

    fn name(&self) -> String {
        "Column RLE (runs along Y)".to_string()
    }


    fn description(&self) -> String {
        "Run-length encoding per vertical column (x, z). Fast and simple; \
         compresses the tall runs of stone/dirt/air found in real terrain."
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::new();
        out.push(chunk.sections.len() as u8);
        out.push((chunk.section_min_y as i8) as u8);

        let n_sec = chunk.sections.len();
        let height = n_sec * 16;
        for z in 0..16 {
            for x in 0..16 {
                let mut runs: Vec<(u32, u32)> = Vec::new();
                for y in 0..height {
                    let id = chunk.sections[y / 16].blocks[(y % 16) * 256 + z * 16 + x];
                    if let Some(last) = runs.last_mut() {
                        if last.1 == id {
                            last.0 += 1;
                            continue;
                        }
                    }
                    runs.push((1, id));
                }
                write_varint(&mut out, runs.len() as u32);
                for (len, id) in runs {
                    write_varint(&mut out, len);
                    write_varint(&mut out, id);
                }
            }
        }
        // biomes per section (small, 64 x u16 LE)
        for sec in &chunk.sections {
            for &b in &sec.biomes {
                out.extend_from_slice(&b.to_le_bytes());
            }
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.len() < 2 {
            bail!("rle stream too short");
        }
        let n_sec = bytes[0] as usize;
        let min_y = bytes[1] as i8 as i32;
        let height = n_sec * 16;
        let mut pos = 2usize;
        let mut sections = vec![Section::air(); n_sec];
        for z in 0..16 {
            for x in 0..16 {
                let n_runs = read_varint(bytes, &mut pos)? as usize;
                let mut y = 0usize;
                for _ in 0..n_runs {
                    let len = read_varint(bytes, &mut pos)? as usize;
                    let id = read_varint(bytes, &mut pos)?;
                    let end = (y + len).min(height);
                    while y < end {
                        let sec = y / 16;
                        sections[sec].blocks[(y % 16) * 256 + z * 16 + x] = id;
                        y += 1;
                    }
                    if y >= height {
                        break;
                    }
                }
            }
        }
        // After parsing all columns, pos points at the biome payload.
        for sec in sections.iter_mut() {
            for b in sec.biomes.iter_mut() {
                if pos + 2 > bytes.len() {
                    bail!("rle stream truncated (biomes)");
                }
                *b = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
                pos += 2;
            }
        }
        let _ = ctx;
        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y: min_y,
            sections,
        }])
    }
}
