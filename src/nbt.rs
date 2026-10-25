//! Minimal NBT (Named Binary Tag) parser, big-endian.
//! Used to read Minecraft level.dat and region chunk data.

use anyhow::{bail, Result};
use std::collections::HashMap;

pub const TAG_END: u8 = 0;
pub const TAG_BYTE: u8 = 1;
pub const TAG_SHORT: u8 = 2;
pub const TAG_INT: u8 = 3;
pub const TAG_LONG: u8 = 4;
pub const TAG_FLOAT: u8 = 5;
pub const TAG_DOUBLE: u8 = 6;
pub const TAG_BYTE_ARRAY: u8 = 7;
pub const TAG_STRING: u8 = 8;
pub const TAG_LIST: u8 = 9;
pub const TAG_COMPOUND: u8 = 10;
pub const TAG_INT_ARRAY: u8 = 11;
pub const TAG_LONG_ARRAY: u8 = 12;

#[derive(Debug, Clone)]
pub enum NbtValue {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<NbtValue>),
    Compound(Vec<(String, NbtValue)>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl NbtValue {
    pub fn as_compound(&self) -> Option<&Vec<(String, NbtValue)>> {
        match self {
            NbtValue::Compound(c) => Some(c),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&NbtValue> {
        self.as_compound()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            NbtValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn get_i32(&self, key: &str) -> Option<i32> {
        match self.get(key)? {
            NbtValue::Int(v) => Some(*v),
            NbtValue::Byte(v) => Some(*v as i32),
            NbtValue::Short(v) => Some(*v as i32),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<NbtValue>> {
        match self {
            NbtValue::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_long_array(&self) -> Option<&Vec<i64>> {
        match self {
            NbtValue::LongArray(l) => Some(l),
            _ => None,
        }
    }
}

pub struct NbtReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> NbtReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() {
            bail!("NBT: unexpected end of input at offset {}", self.pos);
        }
        Ok(())
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        self.need(2)?;
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        self.need(4)?;
        let v = i32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        self.need(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(i64::from_be_bytes(b))
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u16()? as usize;
        self.need(len)?;
        let s = std::str::from_utf8(&self.data[self.pos..self.pos + len])
            .map_err(|e| anyhow::anyhow!("NBT string utf8: {e}"))?
            .to_string();
        self.pos += len;
        Ok(s)
    }

    pub fn read_value(&mut self, tag: u8) -> Result<NbtValue> {
        match tag {
            TAG_BYTE => Ok(NbtValue::Byte(self.read_u8()? as i8)),
            TAG_SHORT => {
                let v = self.read_u16()? as i16;
                Ok(NbtValue::Short(v))
            }
            TAG_INT => Ok(NbtValue::Int(self.read_i32()?)),
            TAG_LONG => Ok(NbtValue::Long(self.read_i64()?)),
            TAG_FLOAT => {
                let v = f32::from_bits(self.read_i32()? as u32);
                Ok(NbtValue::Float(v))
            }
            TAG_DOUBLE => {
                let v = f64::from_bits(self.read_i64()? as u64);
                Ok(NbtValue::Double(v))
            }
            TAG_BYTE_ARRAY => {
                let n = self.read_i32()?;
                if n < 0 || n as usize > self.data.len() - self.pos {
                    bail!("NBT byte array len {n}");
                }
                let v = self.data[self.pos..self.pos + n as usize].to_vec();
                self.pos += n as usize;
                Ok(NbtValue::ByteArray(v))
            }
            TAG_STRING => Ok(NbtValue::String(self.read_string()?)),
            TAG_LIST => {
                let elem_tag = self.read_u8()?;
                let n = self.read_i32()?;
                if n < 0 {
                    bail!("NBT list len {n}");
                }
                let mut out = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    out.push(self.read_value(elem_tag)?);
                }
                Ok(NbtValue::List(out))
            }
            TAG_COMPOUND => {
                let mut fields = Vec::new();
                loop {
                    let t = self.read_u8()?;
                    if t == TAG_END {
                        break;
                    }
                    let name = self.read_string()?;
                    let value = self.read_value(t)?;
                    fields.push((name, value));
                }
                Ok(NbtValue::Compound(fields))
            }
            TAG_INT_ARRAY => {
                let n = self.read_i32()?;
                if n < 0 || n as usize > (self.data.len() - self.pos) / 4 {
                    bail!("NBT int array len {n}");
                }
                let mut out = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    out.push(self.read_i32()?);
                }
                Ok(NbtValue::IntArray(out))
            }
            TAG_LONG_ARRAY => {
                let n = self.read_i32()?;
                if n < 0 || n as usize > (self.data.len() - self.pos) / 8 {
                    bail!("NBT long array len {n}");
                }
                let mut out = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    out.push(self.read_i64()?);
                }
                Ok(NbtValue::LongArray(out))
            }
            other => bail!("NBT unknown tag {other}"),
        }
    }

    /// Read a full NBT root compound (tag + name + payload).
    pub fn read_root(&mut self) -> Result<NbtValue> {
        let tag = self.read_u8()?;
        let _name = self.read_string()?;
        self.read_value(tag)
    }

    /// Read root; on failure, retry assuming little-endian NBT.
    pub fn read_root_any_endian(&mut self) -> Result<NbtValue> {
        match self.read_root() {
            Ok(v) => Ok(v),
            Err(_) => self.read_root_le(),
        }
    }

    fn read_root_le(&mut self) -> Result<NbtValue> {
        self.pos = 0;
        let tag = self.read_u8()?;
        let len = self.read_u16()? as usize;
        self.need(len)?;
        let s = std::str::from_utf8(&self.data[self.pos..self.pos + len])
            .map_err(|e| anyhow::anyhow!("NBT LE string utf8: {e}"))?
            .to_string();
        let _name = s;
        self.pos += len;
        self.read_value_le(tag)
    }

    fn read_value_le(&mut self, tag: u8) -> Result<NbtValue> {
        // Minimal little-endian support (only what we need).
        let le16 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);
        let le32 = |b: &[u8]| i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let le64 = |b: &[u8]| i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        match tag {
            TAG_BYTE => Ok(NbtValue::Byte(self.read_u8()? as i8)),
            TAG_SHORT => {
                self.need(2)?;
                let v = le16(&self.data[self.pos..self.pos + 2]);
                self.pos += 2;
                Ok(NbtValue::Short(v as i16))
            }
            TAG_INT => {
                self.need(4)?;
                let v = le32(&self.data[self.pos..self.pos + 4]);
                self.pos += 4;
                Ok(NbtValue::Int(v))
            }
            TAG_LONG => {
                self.need(8)?;
                let v = le64(&self.data[self.pos..self.pos + 8]);
                self.pos += 8;
                Ok(NbtValue::Long(v))
            }
            TAG_BYTE_ARRAY => {
                self.need(4)?;
                let n = le32(&self.data[self.pos..self.pos + 4]);
                self.pos += 4;
                self.need(n as usize)?;
                let v = self.data[self.pos..self.pos + n as usize].to_vec();
                self.pos += n as usize;
                Ok(NbtValue::ByteArray(v))
            }
            TAG_STRING => {
                self.need(2)?;
                let n = le16(&self.data[self.pos..self.pos + 2]) as usize;
                self.pos += 2;
                self.need(n)?;
                let s = std::str::from_utf8(&self.data[self.pos..self.pos + n])
                    .map_err(|e| anyhow::anyhow!("NBT LE string utf8: {e}"))?
                    .to_string();
                self.pos += n;
                Ok(NbtValue::String(s))
            }
            TAG_LIST => {
                let elem_tag = self.read_u8()?;
                self.need(4)?;
                let n = le32(&self.data[self.pos..self.pos + 4]);
                self.pos += 4;
                let mut out = Vec::with_capacity(n.max(0) as usize);
                for _ in 0..n.max(0) {
                    out.push(self.read_value_le(elem_tag)?);
                }
                Ok(NbtValue::List(out))
            }
            TAG_COMPOUND => {
                let mut fields = Vec::new();
                loop {
                    let t = self.read_u8()?;
                    if t == TAG_END {
                        break;
                    }
                    self.need(2)?;
                    let n = le16(&self.data[self.pos..self.pos + 2]) as usize;
                    self.pos += 2;
                    self.need(n)?;
                    let name = std::str::from_utf8(&self.data[self.pos..self.pos + n])
                        .map_err(|e| anyhow::anyhow!("NBT LE compound name utf8: {e}"))?
                        .to_string();
                    self.pos += n;
                    let value = self.read_value_le(t)?;
                    fields.push((name, value));
                }
                Ok(NbtValue::Compound(fields))
            }
            TAG_INT_ARRAY => {
                self.need(4)?;
                let n = le32(&self.data[self.pos..self.pos + 4]);
                self.pos += 4;
                let mut out = Vec::with_capacity(n.max(0) as usize);
                for _ in 0..n.max(0) {
                    self.need(4)?;
                    out.push(le32(&self.data[self.pos..self.pos + 4]));
                    self.pos += 4;
                }
                Ok(NbtValue::IntArray(out))
            }
            TAG_LONG_ARRAY => {
                self.need(4)?;
                let n = le32(&self.data[self.pos..self.pos + 4]);
                self.pos += 4;
                let mut out = Vec::with_capacity(n.max(0) as usize);
                for _ in 0..n.max(0) {
                    self.need(8)?;
                    out.push(le64(&self.data[self.pos..self.pos + 8]));
                    self.pos += 8;
                }
                Ok(NbtValue::LongArray(out))
            }
            other => bail!("NBT LE unknown tag {other}"),
        }
    }
}

/// Convenience: parse a root compound, returning it as a map view.
pub fn parse_root(data: &[u8]) -> Result<NbtValue> {
    let mut r = NbtReader::new(data);
    r.read_root_any_endian()
}

/// Turn a Compound into an easy-lookup HashMap (only for read-only use).
pub fn compound_map(c: &[(String, NbtValue)]) -> HashMap<&str, &NbtValue> {
    c.iter().map(|(k, v)| (k.as_str(), v)).collect()
}
