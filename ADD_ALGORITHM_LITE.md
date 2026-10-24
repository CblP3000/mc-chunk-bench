# ADD_ALGORITHM_LITE

Что нужно написать в файле `src/codec/<name>.rs`, чтобы получился валидный
алгоритм. Без примеров, ссылок и подсказок.

## 1. Содержимое файла

```rust
use crate::chunk::Chunk;
use crate::codec::{Codec, Ctx, RegistryEntry};
use anyhow::Result;

pub struct <Name>Codec;

pub const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
    id: "<id>",
    name: "<name>",
    group: "<group>",
    description: "<description>",
    make: || Box::new(<Name>Codec) as Box<dyn Codec>,
}];

impl Codec for <Name>Codec {
    // обязательные методы trait
}
```

## 2. Обязательные методы `Codec`

- `fn id(&self) -> String` — уникальный короткий идентификатор.
- `fn name(&self) -> String` — человекочитаемое имя.
- `fn description(&self) -> String` — описание.
- `fn encode_tile(&self, chunks: &[&Chunk], ctx: &Ctx) -> Vec<u8>` —
  преобразовать чанки в последовательность байт.
- `fn decode_tile(&self, bytes: &[u8], ctx: &Ctx) -> Result<Vec<Chunk>>` —
  восстановить чанки из байт.

Необязательный метод:
- `fn group_size(&self) -> usize` — сколько чанков кодируется одним вызовом
  (по умолчанию 1).

## 3. Требования к реализации

- `decode(encode(chunk))` должен вернуть чанк, идентичный исходному:
  те же секции, те же `blocks`, те же `biomes`, тот же `section_min_y`.
- Возвращаемые байты — просто массив; префикс длины не пишется.
- Если для декодирования нужна размерность чанка, она должна храниться
  внутри потока.
- В `decode` разрешено только чтение `ctx` (`get`), мутация запрещена.
- `chunks` в `encode_tile` имеет длину `group_size()`.

## 4. Поля `Ctx`, доступные в коде

- `ctx.registry.names[id]` — имя блока по глобальному id.
- `ctx.registry.get(name)` — глобальный id по имени, `Option<u32>`.
- `ctx.biomes.names[id]`, `ctx.biomes.get(name)` — аналогично для биомов.
- `ctx.biome_block_lists[biome_id]` — список до 255 блоков биома
  (глобальные id).

## 5. Регистрация

Файл автоматически подхватывается `build.rs`. Ничего в `src/codec/mod.rs`
менять не нужно.

## 6. Валидность

Алгоритм считается валидным, если `cargo build --release` собирается без
ошибок, а `cargo run --release --bin chunk_saver -- bench --max-chunks 96`
показывает `ok=true` для его `id`.
