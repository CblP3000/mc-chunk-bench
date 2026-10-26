//! Whole-chunk linear RLE: the chunk is treated as one continuous line.
//!
//! Two scan orders (both registered):
//! - `rle-linear`      — x, y, z (y ascending, bottom-up);
//! - `rle-linear-top`  — x, y, z with y descending (top-down): the line
//!   starts with the air above the terrain, then ground layers.
//!
//! Runs of equal blocks are stored as (length, block-id) pairs. Biome data is
//! stored after the runs (64 x u16 per section).

use crate::chunk::{Chunk, Section, SECTION_BIOMES};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOrder {
    /// x outer, y middle (ascending), z inner
    XyzUp,
    /// same but y descending: air (top) comes first
    XyzDown,
}

pub struct RleLinearCodec {
    pub order: ScanOrder,
}

pub const REGISTRY: &[RegistryEntry] = &[
    RegistryEntry {
        group: "rle",
        description: "Весь чанк как одна линия в порядке x, y, z (снизу вверх): длинные серии камня под землёй и воздуха над поверхностью сжимаются в пары «длина, блок».",
        make: || Box::new(RleLinearCodec { order: ScanOrder::XyzUp }) as Box<dyn Codec>,
    },
    // ВРЕМЕННО ОТКЛЮЧЁН: то же, что rle-linear (порядок обхода влияет лишь
    // на положение «длинных серий», но не на размер на реальных чанках).
    // Для включения уберите комментарий со строк ниже.
    // RegistryEntry {
    //     group: "rle",
    //     description: "Весь чанк как одна линия, но y идёт сверху вниз: линия начинается с воздуха (небо), затем слои земли. Хорош, когда верхняя часть чанка почти пустая.",
    //     make: || Box::new(RleLinearCodec { order: ScanOrder::XyzDown }) as Box<dyn Codec>,
    // },
];

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

impl RleLinearCodec {
    fn block_at(&self, chunk: &Chunk, height: usize, idx: usize) -> u32 {
        // inverse of cell_index
        let x = idx / (height * 16);
        let rest = idx % (height * 16);
        let (local_y, z) = match self.order {
            ScanOrder::XyzUp => (rest / 16, rest % 16),
            ScanOrder::XyzDown => (height - 1 - rest / 16, rest % 16),
        };
        chunk.sections[local_y / 16].blocks[(local_y % 16) * 256 + z * 16 + x]
    }

    fn set_block_at(&self, sections: &mut [Section], height: usize, idx: usize, id: u32) {
        let x = idx / (height * 16);
        let rest = idx % (height * 16);
        let (local_y, z) = match self.order {
            ScanOrder::XyzUp => (rest / 16, rest % 16),
            ScanOrder::XyzDown => (height - 1 - rest / 16, rest % 16),
        };
        sections[local_y / 16].blocks[(local_y % 16) * 256 + z * 16 + x] = id;
    }
}

impl Codec for RleLinearCodec {
    fn id(&self) -> String {
        match self.order {
            ScanOrder::XyzUp => "rle-linear".to_string(),
            ScanOrder::XyzDown => "rle-linear-top".to_string(),
        }
    }

    fn name(&self) -> String {
        match self.order {
            ScanOrder::XyzUp => "Linear RLE x,y,z (whole chunk)".to_string(),
            ScanOrder::XyzDown => "Linear RLE top-down (air first)".to_string(),
        }
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let n_sec = chunk.sections.len();
        let height = n_sec * 16;
        let n_cells = height * 16 * 16;

        let mut runs: Vec<(u32, u32)> = Vec::new();
        for i in 0..n_cells {
            let id = self.block_at(chunk, height, i);
            if let Some(last) = runs.last_mut() {
                if last.1 == id {
                    last.0 += 1;
                    continue;
                }
            }
            runs.push((1, id));
        }

        let mut out = Vec::new();
        out.push(n_sec as u8);
        out.push((chunk.section_min_y as i8) as u8);
        write_varint(&mut out, runs.len() as u32);
        for (len, id) in runs {
            write_varint(&mut out, len);
            write_varint(&mut out, id);
        }
        for sec in &chunk.sections {
            for &b in &sec.biomes {
                out.extend_from_slice(&b.to_le_bytes());
            }
        }
        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        if bytes.len() < 3 {
            bail!("linear rle stream too short");
        }
        let n_sec = bytes[0] as usize;
        let min_y = bytes[1] as i8 as i32;
        let height = n_sec * 16;
        let n_cells = height * 16 * 16;

        let mut pos = 2usize;
        let n_runs = read_varint(bytes, &mut pos)? as usize;
        let mut sections = vec![Section::air(); n_sec];
        let mut i = 0usize;
        for _ in 0..n_runs {
            let len = read_varint(bytes, &mut pos)? as usize;
            let id = read_varint(bytes, &mut pos)?;
            let end = (i + len).min(n_cells);
            while i < end {
                self.set_block_at(&mut sections, height, i, id);
                i += 1;
            }
            if i >= n_cells {
                break;
            }
        }
        for sec in sections.iter_mut() {
            for b in sec.biomes.iter_mut() {
                if pos + 2 > bytes.len() {
                    bail!("linear rle stream truncated (biomes)");
                }
                *b = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
                pos += 2;
            }
        }
        let _ = SECTION_BIOMES;
        Ok(vec![Chunk {
            x: 0,
            z: 0,
            section_min_y: min_y,
            sections,
        }])
    }
}
