# Как добавить свой алгоритм сжатия чанков

Этот документ — спецификация для разработчика (в т.ч. для LLM-агента).
Следуя ей, можно добавить новый алгоритм в бенчмарк, не трогая ничего,
кроме двух файлов. Алгоритм автоматически появится в замерах, JSON-отчёте
и на графиках веб-страницы.

---

## 1. Что такое «алгоритм» в этом проекте

Алгоритм — это реализация трейта `Codec` (см. `src/codec/mod.rs`):

```rust
pub trait Codec: Send + Sync {
    fn id(&self) -> &'static str;          // короткий уникальный id, напр. "my-algo"
    fn name(&self) -> String;               // человекочитаемое имя
    fn description(&self) -> String;        // описание (1-2 предложения)
    fn group_size(&self) -> usize { 1 }     // сколько чанков кодируется одним вызовом
    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8>;
    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>>;
}
```

Ключевые контракты:

- **Вход/выход кодека — «чанк»**: `Chunk { x, z, section_min_y, sections }`,
  где `sections` — список секций 16×16×16 (`blocks: Vec<u32>` — глобальные id
  блоков, `biomes: Vec<u16>` — id биомов, длина 64).
- **Формат вывода — просто последовательность байт** `Vec<u8>`.
  Никаких префиксов длины! Длину знает только вызывающий (фрейминг).
  Если для раскодировки нужна размерность чанка (например количество секций),
  запишите её в поток сами (как это делают `minecraft` и `layered`).
- **Round-trip обязателен**: `decode(encode(chunk))` должен вернуть *идентичный*
  чанк: те же секции, те же `blocks`, те же `biomes`. Это проверяется
  функцией `chunks_equal` в `src/bench.rs`.
- **`ctx` (context)** даёт доступ к реестрам:
  - `ctx.registry.names[id]` — имя блока/состояния по глобальному id;
  - `ctx.registry.get(name)` — id по имени (`Option<u32>`), **без** мутации;
  - `ctx.biomes.names[id]`, `ctx.biomes.get(name)`;
  - `ctx.biome_block_lists[biome_id]` — список до 255 блоков, характерных для
    биома (глобальные id, отсортированы по частоте).
- **В decode используйте только `get()`**, никогда `intern()`: реестр
  мутировать нельзя.
- **`group_size`**: `1` (по умолчанию) — один чанк за вызов. Можно вернуть
  `4`, тогда тайл — квадрат 2×2 чанка (см. `layered-32`), и получите
  `chunks` длиной 4.

---

## 2. Какие файлы создавать/менять

### 2.1. Новый файл кодека: `src/codec/my_algo.rs`

Файл можно положить и во вложенную папку (например `src/codec/many_attempt/my_algo.rs`) — `build.rs` сканирует каталог рекурсивно.

Минимальный каркас (можете скопировать целиком):

```rust
//! Мой алгоритм: <краткое описание в одну строку>

use crate::chunk::Chunk;
use crate::codec::{Category, Codec, Ctx};
use anyhow::Result;

pub struct MyAlgo;

impl Codec for MyAlgo {
    fn id(&self) -> &'static str {
        "my-algo"
    }

    fn name(&self) -> String {
        "My Algo (palette + RLE)".to_string()
    }

    fn description(&self) -> String {
        "Здесь: 1-2 предложения о том, как работает алгоритм и для каких \
         данных он хорош."
            .to_string()
    }

    fn encode_tile(&self, chunks: &[&Chunk], _ctx: &Ctx) -> Vec<u8> {
        let chunk = chunks[0];
        let mut out = Vec::new();
        // 1) заголовок: размерность (например min_y и число секций)
        out.push(chunk.sections.len() as u8);
        out.push((chunk.section_min_y as i8) as u8);
        // 2) ваши данные: палитра, биты, RLE, что угодно
        // 3) вернуть последовательность байт БЕЗ длины
        out
    }

    fn decode_tile(&self, bytes: &[u8], _ctx: &Ctx) -> Result<Vec<Chunk>> {
        // симметрично: прочитать заголовок, восстановить секции,
        // вернуть vec![chunk]
        todo!("реализуйте decode")
    }
}
```

Что обязательно учесть:

- Секция — 4096 блоков; индексация как в `Chunk::block_at`:
  `blocks[(y % 16) * 256 + z * 16 + x]`.
- Если в потоке нужны битовые поля — в `src/codec/layered.rs` уже есть готовые
  `BitWriter`/`BitReader` (LSB-first) и varint-функции. Можно позаимствовать
  или написать свои.
- Пустые секции (всё air, id 0) — нормальный случай, обрабатывайте его.
- Не храните `Vec<u8>`-длину — только если это реально часть формата
  (varint-поля внутри потока допустимы).

### 2.2. Регистрация: `src/codec/<name>.rs` (никаких правок `mod.rs`!)

Регистрация полностью находится внутри файла алгоритма. В конце файла
(или рядом с `impl Codec`) объявите константу `REGISTRY`:

```rust
use crate::codec::RegistryEntry;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "my-algo",
    name: "My Algo (palette + RLE)",
    description: "Здесь: 1-2 предложения о том, как работает алгоритм и для каких данных он хорош.",
    make: || Box::new(MyAlgo) as Box<dyn Codec>,
}];
```

- `id` — уникальный короткий идентификатор (появится в отчёте и на графиках).
- `description` — описание, оно живёт в коде рядом с инициализацией
  и автоматически попадает в JSON-отчёт и на страницу.
- `make` — фабрика: возвращает `Box<dyn Codec>`.
- Если у алгоритма есть параметры, добавьте **несколько записей** в массив
  `REGISTRY` (как `layered`, `layered-h4`, `layered-h16`, `layered-32`).

**Ничего больше менять не нужно!** `build.rs` при сборке сканирует каталог
`src/codec/`, находит ваш файл, автоматически объявляет модуль и собирает
реестр. Регистрация в `src/codec/mod.rs` не требуется вообще.

---

## 3. Что за «тест» и как проверять

В проекте три уровня проверки:

1. **Unit-тесты** (`cargo test`) — битовые распаковщики и пр.
   Свои тесты пишите в `#[cfg(test)]` внутри файла кодека.

2. **Синтетический round-trip тест** (`cargo run --release --bin laytest`):
   гоняет все зарегистрированные варианты `layered` по паттернам
   (равномерный, шумный, колонки, длинные серии и т.д.).
   **Для своего алгоритма добавьте аналогичную проверку** — например,
   вынесите общий хелпер `synthetic`-чанка и прогоните через `MyAlgo`.

3. **Реальный тест на чанках из миров** (`cargo run --release --bin dbg`):
   читает настоящие сейвы (путь по умолчанию — `/mnt/D/lol/.minecraft/home/Fabric 1.21.11/saves`)
   и проверяет round-trip для `minecraft` и `layered`.
   Свой алгоритм подключите так же: добавьте в `src/bin/dbg.rs`
   инициализацию и проверку `chunks_equal`.

4. **Полный бенчмарк** (самый важный тест):
   ```bash
   cargo run --release --bin chunk_saver -- bench --max-chunks 96
   ```
   Он замерит encode/decode время, размер и **проверит round-trip для всех
   алгоритмов из реестра**. Если `ok=false` — в отчёте будет причина
   (decode-ошибка или несовпадение данных). После исправления запустите
   `serve`:
   ```bash
   cargo run --release --bin chunk_saver -- serve --port 8085
   ```
   Новый алгоритм появится на графиках и в таблице автоматически.

---

## 4. Чек-лист для агента-разработчика

- [ ] Создан `src/codec/<name>.rs` с `impl Codec`.
- [ ] В этом же файле объявлен `pub const REGISTRY: &[RegistryEntry]`
      (id, имя, категория, описание, фабрика).
- [ ] `src/codec/mod.rs` **не изменялся** (это проверено тестом ниже).
- [ ] `cargo build --release` собирается без ошибок (build.rs сам подхватил файл).
- [ ] `cargo test` проходит.
- [ ] `cargo run --release --bin chunk_saver -- bench --max-chunks 96`
      показывает `ok=true` для нового алгоритма (round-trip OK) и id в списке.
- [ ] `cargo run --release --bin chunk_saver -- serve --port 8085` — алгоритм
      виден на странице.

---

## 5. Частые грабли

| Проблема | Решение |
|---|---|
| `decode` даёт не те блоки | Проверьте порядок байт/бит и что `bytes` читаются ровно до конца (лишние/недочитанные биты). Сравните свой поток с `layered`/`minecraft`. |
| Всё декодируется в 0 | В `minecraft`-подобных форматах упаковывайте **индексы палитры**, а не глобальные id (см. `encode_section`). |
| Биомы не совпадают | `biomes` — часть round-trip. Либо храните их в потоке, либо копируйте из исходника (не подменяйте нулями). |
| Медленный encode | Избегайте аллокаций на каждый блок; пишите в `Vec<u8>` с `with_capacity`; не используйте `format!` в горячем цикле. |
| Алгоритм медленный, но сжимает отлично | Ставьте `Category::Good` — попадёт в «медленные/плотные». |
| Хочется кодировать несколько чанков сразу | `group_size() -> 4` и работайте с `chunks` длиной 4 (аналог `layered-32`). |

---

## 6. Пример готового простого алгоритма

Самый простой рабочий пример — «палитра на чанк + прямой индекс»:

```rust
//! Palette-per-chunk: одна палитра на весь чанк, индексы по 1 байту.
use crate::chunk::{Chunk, Section, SECTION_BLOCKS};
use crate::codec::{Category, Codec, Ctx};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub struct PaletteChunkCodec;

impl Codec for PaletteChunkCodec {
    fn id(&self) -> &'static str { "palette-chunk" }
    fn name(&self) -> String { "Palette per chunk".to_string() }

    fn description(&self) -> String {
        "Одна палитра на весь чанк + 1 байт на блок. Проще Minecraft-стандарта, \
         хуже на больших палитрах."
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
        let n_sec = bytes[p] as usize; p += 1;
        let min_y = bytes[p] as i8 as i32; p += 1;
        let n_pal = bytes[p] as usize; p += 1;
        let mut palette = Vec::with_capacity(n_pal);
        for _ in 0..n_pal {
            let l = bytes[p] as usize; p += 1;
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
        Ok(vec![Chunk { x: 0, z: 0, section_min_y: min_y, sections }])
    }
}
```

Добавьте в конец файла `REGISTRY` (см. раздел 2.2) — и он готов к тестам,
без правок `mod.rs`.

---

## 7. Подсказка: где смотреть эталонные реализации

- `src/codec/minecraft.rs` — стандарт MC: палитра + бит-упакованные longs,
  varint'ы, строки с префиксом длины.
- `src/codec/layered.rs` — слойный формат: `BitWriter`/`BitReader`,
  составные заголовки, выбор наилучшего представления.
- `src/codec/rle.rs` — простой RLE по колонкам.
- `src/codec/raw.rs` — самый примитивный формат (эталон скорости).
- `src/codec/deflate.rs` — обёртка, добавляющая zlib к любому кодеку.
