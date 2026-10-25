//! Шаблон нового кодека для песочницы (`src/codec/sandbox/`).
//!
//! Как пользоваться:
//! 1. Скопируйте этот файл в `src/codec/sandbox/my_algo.rs`.
//! 2. Переименуйте `TemplateCodec` и id/name/description под свой алгоритм,
//!    реализуйте encode/decode (сейчас это рабочий минимальный пример:
//!    палитра на чанк + 1 байт индекса на блок).
//! 3. Заполните `REGISTRY` внизу (сейчас пуст — в бенчмарк не попадает).
//! 4. Проверьте: `cargo test` и
//!    `cargo run --release --bin chunk_saver -- bench --max-chunks 512`
//!    (для нового алгоритма должно быть `ok=true`).
//! 5. Удался — `git mv` в `src/codec/custom/`; нет — удалите файл.

use crate::chunk::{Chunk, Section, SECTION_BLOCKS};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub struct TemplateCodec;

impl Codec for TemplateCodec {
    fn id(&self) -> String {
        "template-palette".to_string()
    }

    fn name(&self) -> String {
        "Template Palette (per-chunk)".to_string()
    }

    fn description(&self) -> String {
        "Пример-заготовка: одна палитра на весь чанк + 1 байт индекса на блок. \
         Показывает минимальный каркас кодека (round-trip работает)."
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
                if let std::collections::hash_map::Entry::Vacant(e) = idx.entry(b) {
                    e.insert(palette.len() as u8);
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

// Регистрация. Пока пустая — алгоритм не попадает в бенчмарк.
// Когда реализация готова, замените на:
//
// pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
//     group: "palette", // rle | layered | minecraft | palette | palette-rle | adaptive | context-modeling | predictive | deflate | raw
//     description: "Краткое описание.",
//     make: || Box::new(TemplateCodec) as Box<dyn Codec>,
// }];
pub const REGISTRY: &[RegistryEntry] = &[];
