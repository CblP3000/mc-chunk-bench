use anyhow::Result;
use chunk_saver::bench::chunks_equal;
use chunk_saver::codec::{Codec, Ctx};
use chunk_saver::world::{discover_worlds, load_dataset};

fn main() -> Result<()> {
    let worlds = discover_worlds(std::path::Path::new(
        "/mnt/D/lol/.minecraft/home/Fabric 1.21.11/saves",
    ))?;
    let data = load_dataset(&worlds, 32, 7)?;
    let ctx = Ctx {
        registry: &data.registry,
        biomes: &data.biomes,
        biome_block_lists: &data.biome_block_lists,
    };
    let mc = chunk_saver::codec::base::minecraft::MinecraftCodec;
    let layered = chunk_saver::codec::base::layered::LayeredCodec::new(
        chunk_saver::codec::base::layered::LayeredConfig::default(),
    );

    for (i, lc) in data.chunks.iter().enumerate() {
        let c = &lc.chunk;
        println!("chunk {i}: x={} z={} secs={} min_y={}", c.x, c.z, c.sections.len(), c.section_min_y);
        let enc = mc.encode(c, &ctx);
        if enc.len() > 100 {
            println!("  enc head: {:02x?}", &enc[..32.min(enc.len())]);
        }
        // distinct ids in first section
        let mut d: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for b in c.sections[0].blocks.iter().take(4096) {
            d.insert(*b);
        }
        println!(
            "  distinct(sec0)={} first10: {:?}",
            d.len(),
            d.iter().take(10).collect::<Vec<_>>()
        );
        let dec = mc.decode(&enc, &ctx)?;
        if !chunks_equal(c, &dec) {
            println!("  MC MISMATCH (len {})", enc.len());
            for (si, (sa, sb)) in c.sections.iter().zip(dec.sections.iter()).enumerate() {
                for bi in 0..sa.blocks.len() {
                    if sa.blocks[bi] != sb.blocks[bi] {
                        println!(
                            "    section {si} block {bi}: orig={} decoded={}",
                            sa.blocks[bi], sb.blocks[bi]
                        );
                        if bi > 20 {
                            break;
                        }
                    }
                }
                if sa.biomes != sb.biomes {
                    println!("    section {si} biomes differ");
                }
            }
            break;
        } else {
            println!("  MC ok (len {})", enc.len());
        }
        let enc2 = layered.encode(c, &ctx);
        match layered.decode(&enc2, &ctx) {
            Ok(dec2) => {
                if chunks_equal(c, &dec2) {
                    println!("  LAYERED ok (len {})", enc2.len());
                } else {
                    println!("  LAYERED MISMATCH (len {})", enc2.len());
                    for (si, (sa, sb)) in c.sections.iter().zip(dec2.sections.iter()).enumerate() {
                        let mut shown = 0;
                        for bi in 0..sa.blocks.len() {
                            if sa.blocks[bi] != sb.blocks[bi] {
                                println!(
                                    "    section {si} block {bi}: orig={} decoded={}",
                                    sa.blocks[bi], sb.blocks[bi]
                                );
                                shown += 1;
                                if shown > 10 {
                                    break;
                                }
                            }
                        }
                        if shown == 0 && sa.biomes != sb.biomes {
                            println!("    section {si} biomes differ");
                        }
                    }
                    break;
                }
            }
            Err(e) => {
                println!("  LAYERED DECODE ERR: {e} (len {})", enc2.len());
                break;
            }
        }
    }
    Ok(())
}
