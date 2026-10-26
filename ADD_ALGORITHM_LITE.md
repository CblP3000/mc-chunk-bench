# ADD_ALGORITHM_LITE — спецификация для генерации файла алгоритма

Этот документ самодостаточен. Прочитайте его целиком и создайте **один
файл** по описанному ниже контракту. Вам не нужно читать какие-либо другие
файлы проекта, запускать команды или знать окружение: вы только пишете текст
файла. Сборку и запуск тестов выполнит пользователь.

## 1. Что от вас требуется

Создать файл `src/codec/sandbox/<name>.rs` (имя файла выбирается по
алгоритму, латиницей). Папка `sandbox/` — песочница для новых реализаций:
`build.rs` ищет алгоритмы рекурсивно, так что файл автоматически попадёт
в бенчмарк. Файл должен реализовать трейт `Codec` для нового формата
сжатия чанков и зарегистрировать себя.

## 2. Разрешённые зависимости

Внутри файла можно использовать только:

- стандартную библиотеку Rust (`std`);
- крейт `anyhow` (для `anyhow::Result`, `anyhow::bail!`, `anyhow::anyhow!`);
- крейт `flate2` (только если алгоритму нужно zlib/deflate-сжатие);
- типы, определённые в этом документе (разделы 3–5).

Запрещено добавлять какие-либо другие крейты/зависимости, изменять
`Cargo.toml` или другие файлы проекта.

## 3. Типы данных (даны полностью)

```rust
pub const CHUNK_W: usize = 16;
pub const SECTION_BLOCKS: usize = 4096;  // 16*16*16
pub const SECTION_BIOMES: usize = 64;    // 4*4*4

pub struct Section {
    pub blocks: Vec<u32>,   // ровно 4096 элементов; индексы блоков
    pub biomes: Vec<u16>,   // ровно 64 элемента; индексы биомов
}

pub struct Chunk {
    pub x: i32,
    pub z: i32,
    pub section_min_y: i32, // мировая y первой секции
    pub sections: Vec<Section>, // по порядку: y = section_min_y + k*16
}
```

Доступ к блоку: для секции `si`, локальной высоты `ly` (0..16), `z` (0..16),
`x` (0..16) индекс в `blocks` равен `ly * 256 + z * 16 + x`.
`blocks` индексируются глобальными id блоков; `id == 0` всегда «air».
`biomes` — глобальные id биомов.

## 4. Контекст

```rust
pub struct Ctx<'a> {
    pub registry: &'a BlockRegistry,
    pub biomes: &'a BiomeRegistry,
    pub biome_block_lists: &'a [Vec<u32>], // на биом: до 255 блоков (глобальные id)
}

pub struct BlockRegistry { pub names: Vec<String> }  // get(name) -> Option<u32>
pub struct BiomeRegistry { pub names: Vec<String> }  // get(name) -> Option<u16>
```

Использование:
- `ctx.registry.names[id]` — имя блока по id;
- `ctx.registry.get("имя")` — id по имени (`Option<u32>`);
- `ctx.biomes.names[id]`, `ctx.biomes.get("имя")` — аналогично;
- `ctx.biome_block_lists[biome_id]` — список характерных блоков биома.

Внимание: `get` только читает. Мутировать `Ctx` запрещено.

## 5. Трейт, который нужно реализовать

```rust
pub trait Codec: Send + Sync {
    fn id(&self) -> String;
    fn name(&self) -> String;
    fn group_size(&self) -> usize { 1 }
    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8>;
    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>>;
}
```

(`Result` здесь — `anyhow::Result`.)

## 6. Контракт и правила

1. `decode_tile(encode_tile([chunk]))` должен вернуть вектор из одного чанка,
   идентичного исходному: те же `sections` (длина и содержимое `blocks` и
   `biomes`) и тот же `section_min_y`. `x`/`z` можно не сохранять.
2. Возвращаемая `Vec<u8>` — это просто массив байт. Префикс длины писать
   нельзя. Если декодеру нужна размерность (число секций, `section_min_y`),
   она должна быть закодирована внутри потока.
3. В `encode_tile` вектор `chunks` имеет длину `group_size()` (по умолчанию
   1). Если реализуете `group_size() > 1` — все чанки одного размера и
   одной высоты; `decode_tile` должен вернуть столько же чанков.
4. Один формат — один поток: порядок байт/бит внутри потока должен быть
   детерминированным и полностью восстановимым.
5. `decode_tile` не должен читать ничего вне `bytes` и не должен мутировать
   `ctx`.
6. Не используйте глобальное состояние, `unsafe`, многопоточность.

## 7. Регистрация

В конце файла обязательно объявите:

```rust
use crate::codec::RegistryEntry;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    group: "mygroup",
    description: "Краткое описание, что делает алгоритм и для каких данных он хорош.",
    make: || Box::new(MyAlgoCodec) as Box<dyn Codec>,
}];
```

Поля:
- `group` — имя группы метода, отражающее механизм сжатия. Существующие
  группы: `"raw"` (без сжатия), `"rle"` (run-length), `"layered"` (слои),
  `"minecraft"` (стандарт MC), `"palette"` (палитра+битпак),
  `"palette-rle"` (палитра+RLE), `"adaptive"` (выбор наименьшего
  представления на секцию), `"context-modeling"` (контекстное моделирование
  + арифметический кодер), `"predictive"` (пространственный предиктор),
  `"deflate"` (zlib/deflate). Если вашего метода нет среди существующих —
  укажите свою строку; группа влияет на цвет на графиках;
- `description` — описание (единственное место, где оно живёт; в трейте
  `Codec` метода `description()` нет);
- `make` — функция, возвращающая `Box<dyn Codec>` с вашим алгоритмом.

`id` и `name` в реестре НЕ указываются: они берутся из методов
`Codec::id()`/`Codec::name()` (раздел 5). Поле `id` метода `Codec::id()`
должно быть уникальным коротким идентификатором (латиница, дефисы/цифры);
`name` — человекочитаемое имя, которое появится в отчёте и на графиках.

Ничего в других файлах менять не нужно: регистрация происходит автоматически.

## 8. Каркас файла

```rust
use crate::chunk::Chunk;
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::Result;

pub struct MyAlgoCodec;

pub const REGISTRY: &[RegistryEntry] = &[/* как в разделе 7 */];

impl Codec for MyAlgoCodec {
    fn id(&self) -> String { "my-algo".to_string() }
    fn name(&self) -> String { "My Algo".to_string() }

    fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8> {
        // реализация
    }

    fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>> {
        // реализация
    }
}
```

## 9. Проверка валидности

Алгоритм валиден, если проект собирается (`cargo build --release`) и в выводе
`cargo run --release --bin mc-chunk-bench -- bench --max-chunks 512` его `id`
присутствует с `ok=true`. Остальное (сборка, запуск, анализ) делает пользователь.

## 10. Распространённые причины провала

- `decode` возвращает не те блоки/биомы (нарушение п. 1);
- поток читается не до конца или содержит лишние байты (п. 2);
- `encode`/`decode` несимметричны (п. 4);
- попытка использовать запрещённые зависимости или `intern` вместо `get`
  (п. 2 раздела 4).
