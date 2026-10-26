// ============================================================================
// ctx-palette — контекстно-моделируемое палитровое сжатие чанков
// ============================================================================
// Схема (один детерминированный поток, всё через один интервальный кодер):
//   1) Заголовок: varint(число секций), zigzag-varint(section_min_y).
//   2) Каждая секция классифицируется: воздух / однородная / палитра.
//      Тип кодируется 2-битным деревом с контекстом типа предыдущей секции,
//      поэтому серии воздуха и однородных секций стоят доли бита.
//   3) Однородная секция: флаг «тот же id, что и в прошлый раз» + varint.
//   4) Палитровая секция: флаг «та же палитра, что и в прошлый раз» (палитра
//      и её вероятностные модели переиспользуются), иначе — размер палитры и
//      отсортированные дельты id (varint). Индексы палитры кодируются
//      адаптивным бинарным интервальным кодером: сначала флаг «совпадает с
//      предыдущим блоком» с контекстом (длина серии, совпадение нижнего
//      соседа по Y с предыдущим индексом), затем битовое дерево с контекстом
//      нижнего соседа по Y (для палитр до 32 элементов).
//   5) Биомы: «массив как в прошлой секции» / один биом (с флагом повтора
//      id) / палитра + индексы.
//   6) Все varint-байты (заголовки, палитры, id) идут через тот же кодер:
//      8-битное дерево с контекстом — старшие 4 бита предыдущего байта.
//
// Декодер зеркален и использует тот же код, что и кодер (одна структура
// RangeCoder обслуживает оба направления) — расхождение потока исключено
// по построению. Декодер читает только байты входного среза (за пределами —
// условные нули), не мутирует Ctx, без unsafe, без глобального состояния.
//
// Лучше всего сжимает типичный террейн Minecraft: воздух, слоистый
// камень/землю, руды, пещеры, деревья; на случайном шуме остаётся вблизи
// энтропии порядка 0.
// ============================================================================

use crate::chunk::{Chunk, Section};
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::{bail, Result};

const SEC_BLOCKS: usize = 4096;
const SEC_BIOMES: usize = 64;
const PB_INIT: u16 = 1024; // 11-битная вероятность P(bit=0), старт 0.5
const RC_TOP: u32 = 1 << 24;
const MAX_SECTIONS: usize = 65536;

#[inline]
fn zigzag_encode(v: i32) -> u64 {
    ((v.wrapping_shl(1) ^ (v >> 31)) as u32) as u64
}

#[inline]
fn zigzag_decode(z: u64) -> i32 {
    let z = z as u32;
    ((z >> 1) as i32) ^ (-((z & 1) as i32))
}

#[inline]
fn bit_len_u32(x: u32) -> u32 {
    32 - x.leading_zeros()
}

// ---------------------------------------------------------------------------
// Интервальный кодер (LZMA-подобный, 11-битные вероятности).
// Один и тот же метод `bit()` работает в обоих режимах: для кодера бит
// подаётся на вход, для декодера — возвращается. Обновление моделей
// идентично в обе стороны.
// ---------------------------------------------------------------------------
struct RangeCoder<'a> {
    enc: bool,
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
    code: u32,
    src: &'a [u8],
    spos: usize,
}

impl<'a> RangeCoder<'a> {
    fn new_enc() -> Self {
        RangeCoder {
            enc: true,
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
            code: 0,
            src: &[],
            spos: 0,
        }
    }

    fn new_dec(src: &'a [u8]) -> Self {
        let mut r = RangeCoder {
            enc: false,
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
            code: 0,
            src,
            spos: 0,
        };
        for _ in 0..5 {
            r.code = (r.code << 8) | r.next_byte() as u32;
        }
        r
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        let b = if self.spos < self.src.len() {
            self.src[self.spos]
        } else {
            0
        };
        self.spos += 1;
        b
    }

    /// Кодирует/декодирует один бит моделью `p` (11 бит, P(bit=0)=p/2048).
    #[inline]
    fn bit(&mut self, p: &mut u16, bit_in: u32) -> u32 {
        let bound = (self.range >> 11) * (*p as u32);
        let b = if self.enc {
            bit_in & 1
        } else {
            let b = (self.code >= bound) as u32;
            if b == 1 {
                self.code -= bound;
            }
            b
        };
        if b == 0 {
            *p += (2048 - *p) >> 5;
            self.range = bound;
        } else {
            if self.enc {
                self.low += bound as u64;
            }
            *p -= *p >> 5;
            self.range -= bound;
        }
        while self.range < RC_TOP {
            self.range <<= 8;
            if self.enc {
                self.shift_low();
            } else {
                let nb = self.next_byte() as u32;
                self.code = (self.code << 8) | nb;
            }
        }
        b
    }

    #[inline]
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF000000 || (self.low >> 32) != 0 {
            let mut temp = self.cache;
            loop {
                let carry = (self.low >> 32) as u8;
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = ((self.low as u32) << 8) as u64;
    }

    fn flush(&mut self) {
        for _ in 0..5 {
            self.shift_low();
        }
    }

    /// Битовое дерево из `nb` бит; вероятности в `probs`, корень — индекс 1.
    /// Кодирование: `v` — значение. Декодирование: `v` должен быть 0,
    /// заполняется битами.
    #[inline]
    fn tree(&mut self, probs: &mut [u16], nb: u32, v: &mut u32) {
        let mut node = 1usize;
        let mut i = nb;
        while i > 0 {
            i -= 1;
            let want = if self.enc { (*v >> i) & 1 } else { 0 };
            let b = self.bit(&mut probs[node], want);
            if !self.enc {
                *v |= b << i;
            }
            node = (node << 1) | b as usize;
        }
    }
}

// ---------------------------------------------------------------------------
// Модель байта: 16 контекстов (старшие 4 бита предыдущего байта) x
// 256-узловое битовое дерево. Используется для varint-байтов.
// ---------------------------------------------------------------------------
struct ByteModel {
    probs: Vec<u16>,
    prev: u8,
}

impl ByteModel {
    fn new() -> Self {
        ByteModel {
            probs: vec![PB_INIT; 16 * 256],
            prev: 0,
        }
    }
}

fn code_varint(rc: &mut RangeCoder, bm: &mut ByteModel, v: &mut u64) -> Result<()> {
    if rc.enc {
        let mut x = *v;
        loop {
            let mut b = (x & 0x7F) as u8;
            x >>= 7;
            if x != 0 {
                b |= 0x80;
            }
            let mut bits = b as u32;
            let base = ((bm.prev >> 4) as usize) * 256;
            rc.tree(&mut bm.probs[base..base + 256], 8, &mut bits);
            bm.prev = b;
            if x == 0 {
                break;
            }
        }
        Ok(())
    } else {
        let mut x: u64 = 0;
        let mut shift = 0u32;
        loop {
            let mut bits = 0u32;
            let base = ((bm.prev >> 4) as usize) * 256;
            rc.tree(&mut bm.probs[base..base + 256], 8, &mut bits);
            let b = bits as u8;
            bm.prev = b;
            x |= ((b & 0x7F) as u64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
            if shift > 63 {
                bail!("ctx-palette: varint too long");
            }
        }
        *v = x;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Индексы палитры секции блоков (4096 значений, порядок ly,z,x):
// флаг «совпадает с предыдущим» (контекст: серия, below==prev) + битовое
// дерево с контекстом нижнего соседа по Y (для ps <= 32).
// `probs` переиспользуется между секциями с одинаковой палитрой (вызывающая
// сторона очищает его при смене палитры).
// ---------------------------------------------------------------------------
fn code_block_indices(
    rc: &mut RangeCoder,
    ps: usize,
    idx: &mut [u16],
    flags: &mut [u16],
    probs: &mut Vec<u16>,
) -> Result<()> {
    if ps <= 1 {
        if !rc.enc {
            for v in idx.iter_mut() {
                *v = 0;
            }
        }
        return Ok(());
    }
    let nb = bit_len_u32(ps as u32 - 1) as usize;
    let tw = 1usize << nb;
    let use_ctx = ps <= 32;
    let need = if use_ctx { (ps + 1) * tw } else { tw };
    if probs.len() != need {
        probs.clear();
        probs.resize(need, PB_INIT);
    }
    let mut prev: usize = 0;
    let mut run: usize = 0;
    for ly in 0..16 {
        for z in 0..16 {
            for x in 0..16 {
                let i = ly * 256 + z * 16 + x;
                let below = if ly == 0 {
                    ps
                } else {
                    idx[(ly - 1) * 256 + z * 16 + x] as usize
                };
                let fctx = run.min(2) * 2 + if below == prev { 1 } else { 0 };
                let want = if rc.enc {
                    (idx[i] as usize == prev) as u32
                } else {
                    0
                };
                let b = rc.bit(&mut flags[fctx], want);
                if b == 1 {
                    if !rc.enc {
                        idx[i] = prev as u16;
                    }
                    run = (run + 1).min(2);
                } else {
                    run = 0;
                    let mut v: u32 = if rc.enc { idx[i] as u32 } else { 0 };
                    if use_ctx {
                        let base = below * tw;
                        rc.tree(&mut probs[base..base + tw], nb as u32, &mut v);
                    } else {
                        rc.tree(&mut probs[..tw], nb as u32, &mut v);
                    }
                    if !rc.enc {
                        if v as usize >= ps {
                            bail!("ctx-palette: block index out of range");
                        }
                        idx[i] = v as u16;
                    }
                    prev = v as usize;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Индексы палитры биомов (64 значения): флаг «совпадает с предыдущим»
// (контекст: серия) + битовое дерево.
// ---------------------------------------------------------------------------
fn code_biome_indices(
    rc: &mut RangeCoder,
    ps: usize,
    idx: &mut [u16],
    flags: &mut [u16],
) -> Result<()> {
    if ps <= 1 {
        if !rc.enc {
            for v in idx.iter_mut() {
                *v = 0;
            }
        }
        return Ok(());
    }
    let nb = bit_len_u32(ps as u32 - 1);
    let tw = 1usize << nb;
    let mut probs = vec![PB_INIT; tw];
    let mut prev: usize = 0;
    let mut run: usize = 0;
    for x in idx.iter_mut() {
        let fctx = run.min(2);
        let want = if rc.enc {
            (*x as usize == prev) as u32
        } else {
            0
        };
        let b = rc.bit(&mut flags[fctx], want);
        if b == 1 {
            if !rc.enc {
                *x = prev as u16;
            }
            run = (run + 1).min(2);
        } else {
            run = 0;
            let mut v: u32 = if rc.enc { *x as u32 } else { 0 };
            rc.tree(&mut probs[..tw], nb, &mut v);
            if !rc.enc {
                if v as usize >= ps {
                    bail!("ctx-palette: biome index out of range");
                }
                *x = v as u16;
            }
            prev = v as usize;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Кодек
// ---------------------------------------------------------------------------
pub struct CtxPaletteCodec;

impl Codec for CtxPaletteCodec {
    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let ch = chunks[0];
        let mut rc = RangeCoder::new_enc();
        let mut bm = ByteModel::new();
        let mut sec_type = [PB_INIT; 12]; // 3 контекста x 2-битное дерево
        let mut unif_same = PB_INIT;
        let mut pal_same = PB_INIT;
        let mut idx_flags = [PB_INIT; 6];
        let mut bio_type = [PB_INIT; 12]; // 3 контекста x 2-битное дерево
        let mut bio_same = PB_INIT;
        let mut bio_flags = [PB_INIT; 3];

        let mut v = ch.sections.len() as u64;
        let _ = code_varint(&mut rc, &mut bm, &mut v);
        let mut v = zigzag_encode(ch.section_min_y);
        let _ = code_varint(&mut rc, &mut bm, &mut v);

        let mut prev_type = 0usize;
        let mut prev_bio_type = 0usize;
        let mut last_unif: Option<u32> = None;
        let mut last_pal: Vec<u32> = Vec::new();
        let mut have_pal = false;
        let mut tree_cache: Vec<u16> = Vec::new();
        let mut last_bio: Vec<u16> = Vec::new();
        let mut have_bio = false;
        let mut last_bio_id = 0u16;
        let mut have_bio_id = false;

        for sec in &ch.sections {
            let blocks = &sec.blocks[..];
            let first = blocks[0];
            let is_air = blocks.iter().all(|&x| x == 0);
            let is_unif = !is_air && blocks.iter().all(|&x| x == first);
            let t = if is_air {
                0usize
            } else if is_unif {
                1
            } else {
                2
            };
            let mut tv = t as u32;
            rc.tree(&mut sec_type[prev_type * 4..prev_type * 4 + 4], 2, &mut tv);
            prev_type = t;

            if t == 1 {
                let same = last_unif == Some(first);
                rc.bit(&mut unif_same, same as u32);
                if !same {
                    let mut v = first as u64;
                    let _ = code_varint(&mut rc, &mut bm, &mut v);
                    last_unif = Some(first);
                }
            } else if t == 2 {
                let mut pal: Vec<u32> = blocks.to_vec();
                pal.sort_unstable();
                pal.dedup();
                let same = have_pal && last_pal == pal;
                rc.bit(&mut pal_same, same as u32);
                if !same {
                    let mut v = pal.len() as u64;
                    let _ = code_varint(&mut rc, &mut bm, &mut v);
                    let mut v = pal[0] as u64;
                    let _ = code_varint(&mut rc, &mut bm, &mut v);
                    for k in 1..pal.len() {
                        let mut v = (pal[k] - pal[k - 1] - 1) as u64;
                        let _ = code_varint(&mut rc, &mut bm, &mut v);
                    }
                    last_pal = pal.clone();
                    have_pal = true;
                    tree_cache.clear(); // палитра сменилась — модели заново
                }
                let ps = pal.len();
                let mut idx = vec![0u16; SEC_BLOCKS];
                for (i, &x) in blocks.iter().enumerate() {
                    idx[i] = pal.binary_search(&x).unwrap() as u16;
                }
                let _ = code_block_indices(&mut rc, ps, &mut idx, &mut idx_flags, &mut tree_cache);
            }

            let biomes = &sec.biomes[..];
            let bio_unif = biomes.iter().all(|&x| x == biomes[0]);
            let bt: u32 = if have_bio && biomes == last_bio.as_slice() {
                0
            } else if bio_unif {
                1
            } else {
                2
            };
            let mut btv = bt;
            rc.tree(
                &mut bio_type[prev_bio_type * 4..prev_bio_type * 4 + 4],
                2,
                &mut btv,
            );
            prev_bio_type = bt as usize;
            match bt {
                0 => {}
                1 => {
                    let id = biomes[0];
                    let same = have_bio_id && last_bio_id == id;
                    rc.bit(&mut bio_same, same as u32);
                    if !same {
                        let mut v = id as u64;
                        let _ = code_varint(&mut rc, &mut bm, &mut v);
                        last_bio_id = id;
                        have_bio_id = true;
                    }
                }
                _ => {
                    let mut pal: Vec<u16> = biomes.to_vec();
                    pal.sort_unstable();
                    pal.dedup();
                    let mut v = pal.len() as u64;
                    let _ = code_varint(&mut rc, &mut bm, &mut v);
                    let mut v = pal[0] as u64;
                    let _ = code_varint(&mut rc, &mut bm, &mut v);
                    for k in 1..pal.len() {
                        let mut v = (pal[k] - pal[k - 1] - 1) as u64;
                        let _ = code_varint(&mut rc, &mut bm, &mut v);
                    }
                    let psb = pal.len();
                    let mut idx = vec![0u16; SEC_BIOMES];
                    for (i, &x) in biomes.iter().enumerate() {
                        idx[i] = pal.binary_search(&x).unwrap() as u16;
                    }
                    let _ = code_biome_indices(&mut rc, psb, &mut idx, &mut bio_flags);
                }
            }
            last_bio = biomes.to_vec();
            have_bio = true;
        }

        rc.flush();
        rc.out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        let mut rc = RangeCoder::new_dec(bytes);
        let mut bm = ByteModel::new();
        let mut sec_type = [PB_INIT; 12];
        let mut unif_same = PB_INIT;
        let mut pal_same = PB_INIT;
        let mut idx_flags = [PB_INIT; 6];
        let mut bio_type = [PB_INIT; 12];
        let mut bio_same = PB_INIT;
        let mut bio_flags = [PB_INIT; 3];

        let mut v = 0u64;
        code_varint(&mut rc, &mut bm, &mut v)?;
        if v > MAX_SECTIONS as u64 {
            bail!("ctx-palette: too many sections");
        }
        let ns = v as usize;

        let mut v = 0u64;
        code_varint(&mut rc, &mut bm, &mut v)?;
        let min_y = zigzag_decode(v);

        let mut sections: Vec<Section> = Vec::with_capacity(ns.min(512));
        let mut prev_type = 0usize;
        let mut prev_bio_type = 0usize;
        let mut last_unif: Option<u32> = None;
        let mut last_pal: Vec<u32> = Vec::new();
        let mut have_pal = false;
        let mut tree_cache: Vec<u16> = Vec::new();
        let mut last_bio: Vec<u16> = Vec::new();
        let mut have_bio = false;
        let mut last_bio_id = 0u16;
        let mut have_bio_id = false;

        for _ in 0..ns {
            let mut tv = 0u32;
            rc.tree(&mut sec_type[prev_type * 4..prev_type * 4 + 4], 2, &mut tv);
            if tv > 2 {
                bail!("ctx-palette: bad section type");
            }
            let t = tv as usize;
            prev_type = t;

            let mut blocks = vec![0u32; SEC_BLOCKS];
            if t == 1 {
                let b = rc.bit(&mut unif_same, 0);
                let id = if b == 1 {
                    match last_unif {
                        Some(x) => x,
                        None => bail!("ctx-palette: uniform id without reference"),
                    }
                } else {
                    let mut v = 0u64;
                    code_varint(&mut rc, &mut bm, &mut v)?;
                    if v > u32::MAX as u64 {
                        bail!("ctx-palette: block id overflow");
                    }
                    last_unif = Some(v as u32);
                    v as u32
                };
                blocks.fill(id);
            } else if t == 2 {
                let b = rc.bit(&mut pal_same, 0);
                let pal: Vec<u32> = if b == 1 {
                    if !have_pal {
                        bail!("ctx-palette: palette without reference");
                    }
                    last_pal.clone()
                } else {
                    let mut v = 0u64;
                    code_varint(&mut rc, &mut bm, &mut v)?;
                    if v < 2 || v > SEC_BLOCKS as u64 {
                        bail!("ctx-palette: bad block palette size");
                    }
                    let ps = v as usize;
                    let mut p: Vec<u32> = Vec::with_capacity(ps);
                    let mut v0 = 0u64;
                    code_varint(&mut rc, &mut bm, &mut v0)?;
                    if v0 > u32::MAX as u64 {
                        bail!("ctx-palette: block id overflow");
                    }
                    p.push(v0 as u32);
                    for _ in 1..ps {
                        let mut dv = 0u64;
                        code_varint(&mut rc, &mut bm, &mut dv)?;
                        if dv > u32::MAX as u64 {
                            bail!("ctx-palette: palette delta overflow");
                        }
                        let last = *p.last().unwrap();
                        let next = match last.checked_add(dv as u32).and_then(|s| s.checked_add(1))
                        {
                            Some(n) => n,
                            None => bail!("ctx-palette: palette overflow"),
                        };
                        p.push(next);
                    }
                    tree_cache.clear(); // палитра сменилась — модели заново
                    last_pal = p.clone();
                    have_pal = true;
                    p
                };
                let ps = pal.len();
                let mut idx = vec![0u16; SEC_BLOCKS];
                code_block_indices(&mut rc, ps, &mut idx, &mut idx_flags, &mut tree_cache)?;
                for (i, &r) in idx.iter().enumerate() {
                    blocks[i] = pal[r as usize];
                }
            }

            let mut btv = 0u32;
            rc.tree(
                &mut bio_type[prev_bio_type * 4..prev_bio_type * 4 + 4],
                2,
                &mut btv,
            );
            if btv > 2 {
                bail!("ctx-palette: bad biome type");
            }
            prev_bio_type = btv as usize;
            let mut biomes: Vec<u16> = Vec::new();
            match btv {
                0 => {
                    if !have_bio {
                        bail!("ctx-palette: biomes without reference");
                    }
                    biomes = last_bio.clone();
                }
                1 => {
                    let b = rc.bit(&mut bio_same, 0);
                    let id = if b == 1 {
                        if !have_bio_id {
                            bail!("ctx-palette: biome id without reference");
                        }
                        last_bio_id
                    } else {
                        let mut v = 0u64;
                        code_varint(&mut rc, &mut bm, &mut v)?;
                        if v > u16::MAX as u64 {
                            bail!("ctx-palette: biome id overflow");
                        }
                        last_bio_id = v as u16;
                        have_bio_id = true;
                        last_bio_id
                    };
                    biomes = vec![id; SEC_BIOMES];
                }
                _ => {
                    let mut v = 0u64;
                    code_varint(&mut rc, &mut bm, &mut v)?;
                    if v < 1 || v > SEC_BIOMES as u64 {
                        bail!("ctx-palette: bad biome palette size");
                    }
                    let ps = v as usize;
                    let mut pal: Vec<u16> = Vec::with_capacity(ps);
                    let mut v0 = 0u64;
                    code_varint(&mut rc, &mut bm, &mut v0)?;
                    if v0 > u16::MAX as u64 {
                        bail!("ctx-palette: biome id overflow");
                    }
                    pal.push(v0 as u16);
                    for _ in 1..ps {
                        let mut dv = 0u64;
                        code_varint(&mut rc, &mut bm, &mut dv)?;
                        if dv > u16::MAX as u64 {
                            bail!("ctx-palette: biome palette delta overflow");
                        }
                        let last = *pal.last().unwrap();
                        let next = match last.checked_add(dv as u16).and_then(|s| s.checked_add(1))
                        {
                            Some(n) => n,
                            None => bail!("ctx-palette: biome palette overflow"),
                        };
                        pal.push(next);
                    }
                    let mut idx = vec![0u16; SEC_BIOMES];
                    code_biome_indices(&mut rc, ps, &mut idx, &mut bio_flags)?;
                    for r in idx.iter() {
                        biomes.push(pal[*r as usize]);
                    }
                }
            }
            last_bio = biomes.clone();
            have_bio = true;
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

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "ctx-palette",
    name: "CTX-Palette (range coder + context modeling)",
    group: "context-modeling",
    description: "Палитры по секциям + LZMA-подобный интервальный кодер с контекстными моделями: тип секции (воздух/однородная/палитра), повтор палитры/id, флаг совпадения с предыдущим блоком, контекст нижнего соседа по Y. Хорош для типичного террейна (воздух, слои, руды, пещеры, деревья).",
    make: || Box::new(CtxPaletteCodec) as Box<dyn Codec>,
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::chunks_equal;
    use crate::world::{BiomeRegistry, BlockRegistry};

    #[test]
    fn roundtrip_patterns() {
        let mut reg = BlockRegistry::new();
        let mut bio = BiomeRegistry::new();
        for i in 0..500 {
            reg.intern(&format!("minecraft:b{i}"));
        }
        for i in 0..512 {
            bio.intern(&format!("minecraft:bm{i}"));
        }
        let ctx = Ctx {
            registry: &reg,
            biomes: &bio,
            biome_block_lists: &[vec![]],
        };

        let mut sections = Vec::new();
        sections.push(Section::air());
        let mut mono = Section::air();
        for b in mono.blocks.iter_mut() {
            *b = 7;
        }
        sections.push(mono.clone());
        // повтор однородной секции (unif_same)
        sections.push(mono);
        let mut two = Section::air();
        for i in 0..4096 {
            two.blocks[i] = (i % 2) as u32;
        }
        sections.push(two);
        let mut sixteen = Section::air();
        for i in 0..4096 {
            sixteen.blocks[i] = ((i / 64) % 16) as u32;
        }
        for (i, b) in sixteen.biomes.iter_mut().enumerate() {
            *b = (i % 7) as u16;
        }
        sections.push(sixteen.clone());
        // повтор палитровой секции (pal_same)
        sections.push(sixteen);
        let mut rng = 777u64;
        let mut noise = Section::air();
        for v in noise.blocks.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *v = (rng % 300) as u32;
        }
        for (i, b) in noise.biomes.iter_mut().enumerate() {
            *b = (i * 3 % 512) as u16;
        }
        sections.push(noise);
        let mut bio_hi = Section::air();
        for i in 0..64 {
            bio_hi.biomes[i] = (i * 11 % 512) as u16;
        }
        sections.push(bio_hi);

        let ch = Chunk {
            x: 0,
            z: 0,
            section_min_y: -64,
            sections,
        };
        let codec = CtxPaletteCodec;
        let enc = codec.encode(&ch, &ctx);
        let dec = codec.decode(&enc, &ctx).unwrap();
        assert!(chunks_equal(&ch, &dec));
    }
}