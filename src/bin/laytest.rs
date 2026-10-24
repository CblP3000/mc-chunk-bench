use anyhow::Result;
use chunk_saver::bench::chunks_equal;
use chunk_saver::chunk::{Chunk, Section, SECTION_BLOCKS};
use chunk_saver::codec::layered::{LayeredCodec, LayeredConfig};
use chunk_saver::codec::{Codec, Ctx};
use chunk_saver::world::{BiomeRegistry, BlockRegistry};

fn synthetic(blocks: &[u32], n_sec: usize) -> Chunk {
    let mut sections = Vec::with_capacity(n_sec);
    for s in 0..n_sec {
        let mut sec = Section::air();
        for i in 0..SECTION_BLOCKS {
            let v = blocks[(s * SECTION_BLOCKS + i) % blocks.len()];
            sec.blocks[i] = v;
        }
        for b in sec.biomes.iter_mut() {
            *b = 0;
        }
        sections.push(sec);
    }
    Chunk {
        x: 0,
        z: 0,
        section_min_y: -64,
        sections,
    }
}

fn main() -> Result<()> {
    let mut reg = BlockRegistry::new();
    let mut biome = BiomeRegistry::new();
    biome.intern("minecraft:plains");
    let mut ids = Vec::new();
    for i in 0..260 {
        ids.push(reg.intern(&format!("minecraft:test_block_{i}")));
    }
    let ctx = Ctx {
        registry: &reg,
        biomes: &biome,
        biome_block_lists: &[vec![]],
    };
    let bios = LayeredCodec::new(LayeredConfig::default());
    let h4 = LayeredCodec::new(LayeredConfig { slice_height: 4, ..Default::default() });
    let h16 = LayeredCodec::new(LayeredConfig { slice_height: 16, ..Default::default() });
    let h32 = LayeredCodec::new(LayeredConfig { footprint: 32, slice_height: 16 });

    let mut patterns: Vec<(&str, Vec<u32>)> = vec![];
    patterns.push(("air", vec![0]));
    patterns.push(("uniform-stone", vec![1]));
    patterns.push(("small-palette", vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]));
    patterns.push(("larger-palette", ids.clone()));
    // periodic layers
    let mut p = vec![0u32; SECTION_BLOCKS];
    for i in 0..SECTION_BLOCKS {
        p[i] = if i % 4 == 0 { 1 } else { 0 };
    }
    patterns.push(("checker", p));
    // vertical stripes (column runs)
    let mut c = vec![0u32; SECTION_BLOCKS];
    for z in 0..16 {
        for x in 0..16 {
            let id = ((z * 3 + x * 5) % 7) as u32;
            for y in 0..16 {
                c[y * 256 + z * 16 + x] = id;
            }
        }
    }
    patterns.push(("columns", c));
    // mixed noise-like
    let mut n = vec![0u32; SECTION_BLOCKS];
    let mut rng = 12345u64;
    for v in n.iter_mut() {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        *v = (rng % 40) as u32;
    }
    patterns.push(("noise", n));

    // long runs (replicator count widths 8/12/20)
    let mut lr = vec![0u32; SECTION_BLOCKS];
    for i in 0..SECTION_BLOCKS {
        lr[i] = if i < 200 { 1 } else if i < 300 { 2 } else if i < 500 { 3 } else { 0 };
    }
    patterns.push(("longrun200-300-500", lr));
    let mut lr2 = vec![0u32; SECTION_BLOCKS];
    for i in 0..SECTION_BLOCKS {
        lr2[i] = if i < 4090 { 1 } else { 2 };
    }
    patterns.push(("longrun4090", lr2));

    // all cells same except tiny
    let mut mostly = vec![5u32; SECTION_BLOCKS];
    mostly[7] = 6;
    mostly[1000] = 7;
    patterns.push(("mostly-uniform", mostly));

    for (name, blocks) in &patterns {
        for n_sec in [1usize, 2, 4] {
            let ch = synthetic(blocks, n_sec);
            for (codec_name, codec) in [("h1", &bios as &dyn Codec), ("h4", &h4), ("h16", &h16)] {
                let enc = codec.encode(&ch, &ctx);
                match codec.decode(&enc, &ctx) {
                    Ok(d) => {
                        let ok = chunks_equal(&ch, &d);
                        println!("{name} n_sec={n_sec} {codec_name}: len={} ok={ok}", enc.len());
                        if !ok {
                            let mut shown = 0;
                            for (si, (sa, sb)) in ch.sections.iter().zip(d.sections.iter()).enumerate() {
                                for bi in 0..sa.blocks.len() {
                                    if sa.blocks[bi] != sb.blocks[bi] {
                                        println!("   sec{si} blk{bi} orig={} dec={}", sa.blocks[bi], sb.blocks[bi]);
                                        shown += 1;
                                        if shown > 8 {
                                            break;
                                        }
                                    }
                                }
                                if shown > 8 {
                                    break;
                                }
                            }
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        println!("{name} n_sec={n_sec} {codec_name}: DECODE ERR {e} len={}", enc.len());
                        return Ok(());
                    }
                }
            }
        }
    }
    // 32x32 tile (4 chunks)
    {
        let blocks = &patterns[3].1;
        let mut tile = Vec::new();
        for _ in 0..4 {
            tile.push(synthetic(blocks, 1));
        }
        let refs: Vec<&Chunk> = tile.iter().collect();
        let enc = h32.encode_tile(&refs, &ctx);
        match h32.decode_tile(&enc, &ctx) {
            Ok(d) => {
                let mut ok = true;
                for (o, x) in tile.iter().zip(d.iter()) {
                    if !chunks_equal(o, x) {
                        ok = false;
                    }
                }
                println!("tile32 check: len={} ok={ok}", enc.len());
            }
            Err(e) => println!("tile32 DECODE ERR {e} len={}", enc.len()),
        }
    }
    Ok(())
}
