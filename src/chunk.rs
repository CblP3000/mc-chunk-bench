//! Abstract chunk model shared by all codecs.

use crate::nbt::NbtValue;
use anyhow::{bail, Result};

pub const CHUNK_W: usize = 16;
pub const SECTION_BLOCKS: usize = CHUNK_W * CHUNK_W * CHUNK_W; // 4096
pub const SECTION_BIOMES: usize = 64; // 4^3

/// One 16x16x16 section. `blocks` holds global block-state ids, `biomes`
/// holds biome ids (indices into the world biome registry).
#[derive(Debug, Clone)]
pub struct Section {
    pub blocks: Vec<u32>,
    pub biomes: Vec<u16>,
}

impl Section {
    pub fn air() -> Self {
        Self {
            blocks: vec![0; SECTION_BLOCKS],
            biomes: vec![0; SECTION_BIOMES],
        }
    }
}

/// A column of sections (1.18+ worlds have 24 sections, y = -64..320).
#[derive(Debug, Clone)]
pub struct Chunk {
    pub x: i32,
    pub z: i32,
    pub section_min_y: i32,
    pub sections: Vec<Section>,
}

impl Chunk {
    /// Section index by world y coordinate.
    pub fn section_index(&self, y: i32) -> usize {
        ((y - self.section_min_y) / 16) as usize
    }

    pub fn block_at(&self, x: u32, y: i32, z: u32) -> u32 {
        let s = self.section_index(y);
        let local_y = (y - self.section_min_y) % 16;
        self.sections[s].blocks[(local_y as usize * 256) + (z as usize * 16) + x as usize]
    }

    pub fn set_block(&mut self, x: u32, y: i32, z: u32, id: u32) {
        let s = self.section_index(y);
        let local_y = (y - self.section_min_y) % 16;
        self.sections[s].blocks[(local_y as usize * 256) + (z as usize * 16) + x as usize] = id;
    }

    pub fn height(&self) -> usize {
        self.sections.len() * 16
    }

    pub fn world_y(&self, sec: usize) -> i32 {
        self.section_min_y + (sec as i32) * 16
    }
}

fn unpack_long_array(data: &[i64], count: usize, bits: u32) -> Vec<u32> {
    if bits == 0 {
        return vec![0; count];
    }
    let mask = if bits >= 32 { u64::MAX } else { (1u64 << bits) - 1 };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bit_index = i as u64 * bits as u64;
        let long_index = (bit_index >> 6) as usize;
        let offset = (bit_index & 63) as u32;
        let mut v = (data[long_index] as u64) >> offset;
        if offset + bits > 64 && long_index + 1 < data.len() {
            v |= (data[long_index + 1] as u64) << (64 - offset);
        }
        out.push((v & mask) as u32);
    }
    out
}

fn canonical_state(name: &str, properties: &[(String, NbtValue)]) -> String {
    if properties.is_empty() {
        return name.to_string();
    }
    let mut props: Vec<(String, String)> = properties
        .iter()
        .map(|(k, v)| {
            let s = match v {
                NbtValue::String(s) => s.clone(),
                NbtValue::Byte(b) => b.to_string(),
                NbtValue::Int(i) => i.to_string(),
                NbtValue::Short(s) => s.to_string(),
                NbtValue::Long(l) => l.to_string(),
                other => format!("{other:?}"),
            };
            (k.clone(), s)
        })
        .collect();
    props.sort();
    let joined = props
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name}[{joined}]")
}

/// Parse a chunk from its NBT root compound (as stored in region files).
/// Supports modern 1.18+ ("sections" with palettes) and legacy pre-1.18
/// ("Level" with numeric Blocks/Data/Add arrays) chunk formats.
/// `intern` returns a global block-state id for a canonical state name.
pub fn parse_chunk(
    root: &NbtValue,
    intern: &mut impl FnMut(&str) -> u32,
    intern_biome: &mut impl FnMut(&str) -> u16,
) -> Result<Chunk> {
    if root.get("Level").is_some() {
        return parse_legacy_chunk(root, intern, intern_biome);
    }
    parse_modern_chunk(root, intern, intern_biome)
}

fn parse_modern_chunk(
    root: &NbtValue,
    intern: &mut impl FnMut(&str) -> u32,
    intern_biome: &mut impl FnMut(&str) -> u16,
) -> Result<Chunk> {
    let x = root.get_i32("xPos").unwrap_or(0);
    let z = root.get_i32("zPos").unwrap_or(0);
    let sections = root
        .get("sections")
        .and_then(|s| s.as_list())
        .ok_or_else(|| anyhow::anyhow!("chunk has no sections list"))?;

    let mut entries: Vec<(i32, &NbtValue)> = Vec::new();
    for sec in sections {
        let y = sec.get_i32("Y").unwrap_or(0);
        entries.push((y, sec));
    }
    if entries.is_empty() {
        bail!("chunk has zero sections");
    }
    let min_y = entries.iter().map(|(y, _)| *y).min().unwrap();
    let max_y = entries.iter().map(|(y, _)| *y).max().unwrap();
    let n_sections = (((max_y - min_y) / 16) + 1) as usize;

    let mut out = Vec::with_capacity(n_sections);
    for _ in 0..n_sections {
        out.push(Section::air());
    }
    let mut any = false;
    for (y, sec) in entries {
        let idx = ((y - min_y) / 16) as usize;
        let section = &mut out[idx];
        if let Some(bs) = sec.get("block_states") {
            let palette = bs
                .get("palette")
                .and_then(|p| p.as_list())
                .filter(|p| !p.is_empty());
            if let Some(palette) = palette {
                let ids: Vec<u32> = palette
                    .iter()
                    .map(|p| {
                        let name = p.get_str("Name").unwrap_or("minecraft:air");
                        let props = p
                            .get("Properties")
                            .and_then(|pr| pr.as_compound())
                            .map(|pr| pr.clone())
                            .unwrap_or_default();
                        intern(&canonical_state(name, &props))
                    })
                    .collect();
                if let Some(data) = bs.get("data").and_then(|d| d.as_long_array()) {
                    let bits = (data.len() * 64) / SECTION_BLOCKS;
                    if bits == 0 {
                        let p = ids.first().copied().unwrap_or(0);
                        for b in section.blocks.iter_mut() {
                            *b = p;
                        }
                    } else {
                        let unpacked = unpack_long_array(data, SECTION_BLOCKS, bits as u32);
                        for (i, b) in section.blocks.iter_mut().enumerate() {
                            *b = ids.get(unpacked[i] as usize).copied().unwrap_or(0);
                        }
                    }
                } else {
                    let p = ids.first().copied().unwrap_or(0);
                    for b in section.blocks.iter_mut() {
                        *b = p;
                    }
                }
            } else {
                for b in section.blocks.iter_mut() {
                    *b = 0;
                }
            }
        }
        if let Some(bi) = sec.get("biomes") {
            let palette = bi
                .get("palette")
                .and_then(|p| p.as_list())
                .filter(|p| !p.is_empty());
            if let Some(palette) = palette {
                let ids: Vec<u16> = palette
                    .iter()
                    .map(|p| {
                        let name = p.get_str("Name").unwrap_or("minecraft:plains");
                        intern_biome(name)
                    })
                    .collect();
                if let Some(data) = bi.get("data").and_then(|d| d.as_long_array()) {
                    let bits = (data.len() * 64) / SECTION_BIOMES;
                    if bits == 0 {
                        let p = ids.first().copied().unwrap_or(0);
                        for b in section.biomes.iter_mut() {
                            *b = p;
                        }
                    } else {
                        let unpacked = unpack_long_array(data, SECTION_BIOMES, bits as u32);
                        for (i, b) in section.biomes.iter_mut().enumerate() {
                            *b = ids.get(unpacked[i] as usize).copied().unwrap_or(0);
                        }
                    }
                } else {
                    let p = ids.first().copied().unwrap_or(0);
                    for b in section.biomes.iter_mut() {
                        *b = p;
                    }
                }
            }
        }
        any = true;
    }
    if !any {
        bail!("empty chunk");
    }

    Ok(Chunk {
        x,
        z,
        section_min_y: min_y,
        sections: out,
    })
}

/// Legacy (< 1.18): root has "Level" compound; sections use numeric block ids
/// (Blocks byte array + optional Add high bits + Data nibbles). Biome ids come
/// from Level.Biomes (byte[256], one per column).
fn parse_legacy_chunk(
    root: &NbtValue,
    intern: &mut impl FnMut(&str) -> u32,
    intern_biome: &mut impl FnMut(&str) -> u16,
) -> Result<Chunk> {
    let level = root.get("Level").unwrap();
    let x = level.get_i32("xPos").unwrap_or(0);
    let z = level.get_i32("zPos").unwrap_or(0);

    let column_biomes: Vec<u16> = if let Some(NbtValue::ByteArray(arr)) = level.get("Biomes") {
        (0..256)
            .map(|i| intern_biome(&format!("legacy_biome:{}", arr[i])))
            .collect()
    } else {
        (0..256).map(|_| intern_biome("legacy_biome:0")).collect()
    };

    let sections = level
        .get("Sections")
        .and_then(|s| s.as_list())
        .ok_or_else(|| anyhow::anyhow!("legacy chunk has no Sections"))?;

    let mut entries: Vec<(i32, &NbtValue)> = Vec::new();
    for sec in sections {
        let y = sec.get_i32("Y").unwrap_or(0);
        entries.push((y, sec));
    }
    if entries.is_empty() {
        bail!("legacy chunk has zero sections");
    }
    let min_y = entries.iter().map(|(y, _)| *y).min().unwrap();
    let max_y = entries.iter().map(|(y, _)| *y).max().unwrap();
    let n_sections = (((max_y - min_y) / 16) + 1) as usize;

    let mut out = Vec::with_capacity(n_sections);
    for _ in 0..n_sections {
        out.push(Section::air());
    }

    for (y, sec) in entries {
        let idx = ((y - min_y) / 16) as usize;
        let section = &mut out[idx];
        let blocks = sec
            .get("Blocks")
            .and_then(|b| match b {
                NbtValue::ByteArray(v) => Some(v.as_slice()),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("legacy section missing Blocks"))?;
        let add = sec.get("Add").and_then(|b| match b {
            NbtValue::ByteArray(v) => Some(v.as_slice()),
            _ => None,
        });
        let data = sec.get("Data").and_then(|b| match b {
            NbtValue::ByteArray(v) => Some(v.as_slice()),
            _ => None,
        });

        for i in 0..SECTION_BLOCKS {
            let base = blocks[i] as u32;
            let hi = match add {
                Some(a) => ((a[i >> 1] >> ((i & 1) * 4)) & 0x0f) as u32,
                None => 0,
            };
            let meta = match data {
                Some(d) => ((d[i >> 1] >> ((i & 1) * 4)) & 0x0f) as u32,
                None => 0,
            };
            let id = (base | (hi << 8)) << 4 | meta;
            section.blocks[i] = if id == 0 {
                0
            } else {
                intern(&format!("legacy:{id}"))
            };
        }
        // biome: use the column biome at (x=0,z=0) for the whole section
        for b in section.biomes.iter_mut() {
            *b = column_biomes[0];
        }
    }

    Ok(Chunk {
        x,
        z,
        section_min_y: min_y,
        sections: out,
    })
}

/// Remove blocks that cannot be seen from the outside ("Bobby-style" culling).
///
/// Flood-fill from the 6 boundary faces through air cells; any non-air block
/// adjacent to a reached air cell is considered visible. Everything else is
/// replaced with air (id 0).
pub fn prune_interior(chunk: &mut Chunk) -> usize {
    let w = CHUNK_W;
    let h = chunk.height();
    if chunk.sections.is_empty() {
        return 0;
    }
    let air = 0u32;
    let mut visible_air = vec![false; w * w * h];
    let mut queue = std::collections::VecDeque::new();

    let idx = |x: usize, y: usize, z: usize| y * w * w + z * w + x;
    let bi = |x: i32, y: i32, z: i32| -> Option<(usize, usize, usize)> {
        if x >= 0 && (x as usize) < w && z >= 0 && (z as usize) < w {
            let y = y as i32 - chunk.section_min_y;
            if y >= 0 && (y as usize) < h {
                return Some((x as usize, y as usize, z as usize));
            }
        }
        None
    };
    let block_at = |x: i32, y: i32, z: i32| -> Option<u32> {
        bi(x, y, z).map(|(x, y, z)| chunk.sections[y / 16].blocks[(y % 16) * 256 + z * 16 + x])
    };

    // Seed: air cells on the boundary (x=0/15, z=0/15, or y min/max).
    for y in 0..h {
        for z in 0..w {
            for (x, wall) in [(0usize, true), (w - 1, true)] {
                let _ = wall;
                if chunk.sections[y / 16].blocks[(y % 16) * 256 + z * 16 + x] == air {
                    let i = idx(x, y, z);
                    visible_air[i] = true;
                    queue.push_back((x, y, z));
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            for z in [0usize, w - 1] {
                if chunk.sections[y / 16].blocks[(y % 16) * 256 + z * 16 + x] == air {
                    let i = idx(x, y, z);
                    visible_air[i] = true;
                    queue.push_back((x, y, z));
                }
            }
        }
    }
    for x in 0..w {
        for z in 0..w {
            for y in [0usize, h - 1] {
                if chunk.sections[y / 16].blocks[(y % 16) * 256 + z * 16 + x] == air {
                    let i = idx(x, y, z);
                    visible_air[i] = true;
                    queue.push_back((x, y, z));
                }
            }
        }
    }

    // BFS through air.
    let dirs: [(i32, i32, i32); 6] = [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ];
    while let Some((x, y, z)) = queue.pop_front() {
        for (dx, dy, dz) in dirs {
            let (nx, ny, nz) = (x as i32 + dx, y as i32 + dy, z as i32 + dz);
            if let Some((nx, ny, nz)) = bi(nx, ny, nz) {
                if visible_air[idx(nx, ny, nz)] {
                    continue;
                }
                if block_at(nx as i32, ny as i32 + chunk.section_min_y, nz as i32) == Some(air) {
                    visible_air[idx(nx, ny, nz)] = true;
                    queue.push_back((nx, ny, nz));
                }
            }
        }
    }

    // Remove non-visible non-air blocks: a solid block is visible iff at
    // least one of its 6 neighbours is reached air (the "shell" of the escape).
    let mut removed = 0;
    for y in 0..h {
        let sec = &mut chunk.sections[y / 16];
        for z in 0..w {
            for x in 0..w {
                let r = (y % 16) * 256 + z * 16 + x;
                if sec.blocks[r] == air {
                    continue;
                }
                let visible = dirs.iter().any(|&(dx, dy, dz)| {
                    let (nx, ny, nz) = (x as i32 + dx, (y as i32) + dy, z as i32 + dz);
                    if nx < 0 || nx >= w as i32 || nz < 0 || nz >= w as i32 {
                        return false;
                    }
                    if ny < 0 || ny >= h as i32 {
                        return false;
                    }
                    visible_air[idx(nx as usize, ny as usize, nz as usize)]
                });
                if !visible {
                    sec.blocks[r] = air;
                    removed += 1;
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_small_bits() {
        // 3 bit values packed into longs: values 0..7
        let mut data = vec![0i64; 2];
        for (i, v) in [3u32, 5, 0, 7, 2, 1, 6, 4, 3, 3].iter().enumerate() {
            let bit = i * 3;
            let li = bit / 64;
            let off = bit % 64;
            data[li] |= ((*v as i64) & 0x7) << off;
            if off + 3 > 64 {
                data[li + 1] |= ((*v as i64) & 0x7) >> (64 - off);
            }
        }
        let out = unpack_long_array(&data, 10, 3);
        assert_eq!(out, vec![3, 5, 0, 7, 2, 1, 6, 4, 3, 3]);
    }

    #[test]
    fn unpack_spanning_long() {
        // 12-bit values crossing long boundary
        let mut data = vec![0i64; 3];
        let vals = vec![0x123u32, 0xabc, 0xfff, 0x0, 0x7ff];
        for (i, v) in vals.iter().enumerate() {
            let bit = i * 12;
            let li = bit / 64;
            let off = bit % 64;
            data[li] |= ((*v as i64) & 0xfff) << off;
            if off + 12 > 64 {
                data[li + 1] |= ((*v as i64) & 0xfff) >> (64 - off);
            }
        }
        let out = unpack_long_array(&data, vals.len(), 12);
        assert_eq!(out, vals);
    }

    fn solid_section(id: u32) -> Section {
        Section { blocks: vec![id; SECTION_BLOCKS], biomes: vec![0u16; SECTION_BIOMES] }
    }

    #[test]
    fn prune_keeps_shell_next_to_air() {
        // 16^3 chunk of solid stone with an air tunnel along X at y=8, z=8.
        // BFS must reach the tunnel from the x=0 boundary face and keep only
        // solid blocks directly touching it.
        let mut ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: 0,
            sections: vec![solid_section(5)],
        };
        {
            let air = 0u32;
            let b = &mut ch.sections[0].blocks;
            for x in 0..CHUNK_W {
                b[8 * 256 + 8 * 16 + x] = air;
            }
        }
        let removed = prune_interior(&mut ch);
        let blocks = &ch.sections[0].blocks;
        let at = |x: usize, y: usize, z: usize| blocks[y * 256 + z * 16 + x];

        // tunnel stays air
        for x in 0..CHUNK_W {
            assert_eq!(at(x, 8, 8), 0);
        }
        // direct neighbours of the tunnel stay
        let kept = (0..CHUNK_W)
            .map(|x| {
                [(x, 7, 8), (x, 9, 8), (x, 8, 7), (x, 8, 9)]
                    .iter()
                    .filter(|&&(x, y, z)| at(x, y, z) == 5)
                    .count()
            })
            .sum::<usize>();
        assert_eq!(kept, 4 * CHUNK_W);
        // corners one step away from the tunnel are culled
        assert_eq!(at(5, 7, 7), 0);
        assert_eq!(at(5, 6, 8), 0);
        assert_eq!(removed, SECTION_BLOCKS - CHUNK_W - kept);
    }

    #[test]
    fn prune_air_chunk_removes_nothing() {
        let mut ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: 0,
            sections: vec![Section::air()],
        };
        assert_eq!(prune_interior(&mut ch), 0);
    }
}
