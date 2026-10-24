//! Minecraft region file (.mca / .mcr) reader.
//! Header: 1024 x 4-byte entries (sector offset << 8 | sector count),
//! then 1024 x 4-byte timestamps, then sector data (4096-byte sectors).

use anyhow::{bail, Result};
use flate2::read::{GzDecoder, ZlibDecoder};
use std::io::Read;
use std::path::Path;

pub const COMPRESSION_GZIP: u8 = 1;
pub const COMPRESSION_ZLIB: u8 = 2;
pub const COMPRESSION_NONE: u8 = 3;

pub struct RegionFile {
    data: Vec<u8>,
}

pub struct ChunkSlot {
    pub slot: usize,
    pub local_x: u32,
    pub local_z: u32,
    pub compression: u8,
    pub nbt: Vec<u8>,
}

impl RegionFile {
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)?;
        if data.len() < 8192 {
            bail!("region file too small: {}", data.len());
        }
        Ok(Self { data })
    }

    fn entry(&self, slot: usize) -> (u64, u32) {
        let e = u32::from_be_bytes([
            self.data[slot * 4],
            self.data[slot * 4 + 1],
            self.data[slot * 4 + 2],
            self.data[slot * 4 + 3],
        ]);
        ((e >> 8) as u64 * 4096, e & 0xff)
    }

    /// Slot is present (has data) in the file.
    pub fn is_present(&self, slot: usize) -> bool {
        self.entry(slot).1 != 0
    }

    /// Read chunk data of the given slot. Returns None if empty.
    pub fn read_chunk(&self, slot: usize) -> Result<Option<ChunkSlot>> {
        let (offset, sectors) = self.entry(slot);
        if sectors == 0 {
            return Ok(None);
        }
        let end = (offset + sectors as u64 * 4096) as usize;
        if end > self.data.len() {
            bail!("chunk {slot} out of range");
        }
        let len = u32::from_be_bytes([
            self.data[offset as usize],
            self.data[offset as usize + 1],
            self.data[offset as usize + 2],
            self.data[offset as usize + 3],
        ]) as usize;
        if len == 0 || offset as usize + 4 + len > end {
            bail!("bad chunk length {len} at slot {slot}");
        }
        let compression = self.data[offset as usize + 4];
        let raw = &self.data[offset as usize + 5..offset as usize + 4 + len];
        let nbt = match compression {
            COMPRESSION_ZLIB => {
                let mut d = ZlibDecoder::new(raw);
                let mut buf = Vec::new();
                d.read_to_end(&mut buf)?;
                buf
            }
            COMPRESSION_GZIP => {
                let mut d = GzDecoder::new(raw);
                let mut buf = Vec::new();
                d.read_to_end(&mut buf)?;
                buf
            }
            COMPRESSION_NONE => raw.to_vec(),
            other => bail!("unknown chunk compression {other}"),
        };
        Ok(Some(ChunkSlot {
            slot,
            local_x: (slot % 32) as u32,
            local_z: (slot / 32) as u32,
            compression,
            nbt,
        }))
    }
}
