//! Wrapper that applies deflate (zlib) to another codec's stream.
//! Represents the "slow but high-compression" family for disk storage.
//! Also used by the "сжатие" toggle to wrap every base method with the same
//! compressor.

use crate::chunk::Chunk;
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::Result;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;

/// В базовом реестре «+deflate»-вариантов нет: они создаются на лету
/// флагом «Сжатие» в интерфейсе (см. `all_registrations(with_gzip_extra)`).
pub const REGISTRY: &[RegistryEntry] = &[];

pub struct DeflateCodec {
    inner: Box<dyn Codec>,
    level: Compression,
    tag: String,
}

impl DeflateCodec {
    pub fn new(inner: Box<dyn Codec>, level: Compression, tag: &str) -> Self {
        Self {
            inner,
            level,
            tag: tag.to_string(),
        }
    }
}

impl Codec for DeflateCodec {
    fn id(&self) -> String {
        format!("{}+deflate{}", self.tag, self.level.level())
    }

    fn name(&self) -> String {
        format!("{}+deflate{} (zlib)", self.tag, self.level.level())
    }

    fn group_size(&self) -> usize {
        self.inner.group_size()
    }

    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8> {
        let raw = self.inner.encode_tile(chunks, ctx);
        let mut enc = ZlibEncoder::new(Vec::new(), self.level);
        let _ = enc.write_all(&raw);
        enc.finish().unwrap_or(raw)
    }

    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut dec = ZlibDecoder::new(bytes);
        let mut buf = Vec::new();
        use std::io::Read;
        dec.read_to_end(&mut buf)
            .map_err(|e| anyhow::anyhow!("deflate decode: {e}"))?;
        self.inner.decode_tile(&buf, ctx)
    }
}
