use anyhow::Result;
use mc_chunk_bench::codec::Ctx;
use mc_chunk_bench::world::{discover_worlds, load_dataset};

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(|h| format!("{}/.minecraft/saves", h.to_string_lossy()))
            .unwrap_or_default()
    });
    let worlds = discover_worlds(std::path::Path::new(&dir), &[])?;
    let data = load_dataset(&worlds, 512, 1)?;
    let regs = mc_chunk_bench::codec::all_registrations(true);
    let reg = regs
        .iter()
        .find(|r| r.codec.id() == "rle-column+deflate12")
        .unwrap();
    let ctx = Ctx {
        registry: &data.registry,
        biomes: &data.biomes,
        biome_block_lists: &data.biome_block_lists,
    };

    let mut rows: Vec<(usize, f64)> = Vec::new();
    for lc in &data.chunks {
        let c = &lc.chunk;
        let total = c.sections.len() * 4096;
        let air = c
            .sections
            .iter()
            .map(|s| s.blocks.iter().filter(|&&b| b == 0).count())
            .sum::<usize>();
        let enc = reg.codec.encode_tile(&[c], &ctx);
        rows.push((enc.len(), air as f64 / total as f64));
    }
    rows.sort_by_key(|(n, _)| *n);
    let q = |p: f64| rows[((rows.len() - 1) as f64 * p) as usize].0;
    let avg: usize = rows.iter().map(|(n, _)| n).sum::<usize>() / rows.len();
    println!("chunks: {}  avg: {} B", rows.len(), avg);
    println!(
        "quantiles: min {}  q25 {}  median {}  q75 {}  max {}",
        q(0.0),
        q(0.25),
        q(0.5),
        q(0.75),
        q(1.0)
    );
    println!("air share by chunk:");
    let mut airs: Vec<f64> = rows.iter().map(|(_, a)| *a).collect();
    airs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  air fraction: min {:.3} median {:.3} max {:.3}",
        airs[0],
        airs[airs.len() / 2],
        airs[airs.len() - 1]
    );
    // 10 самых плотных чанков
    println!("top-10 largest chunks:");
    for (n, a) in rows.iter().rev().take(10) {
        println!("  {} B  (air {:.1}%)", n, (1.0 - a) * 100.0);
    }
    // корсиляция воздух-размер
    let corr = {
        let n = rows.len() as f64;
        let mx = rows.iter().map(|(x, _)| *x as f64).sum::<f64>() / n;
        let my = rows.iter().map(|(_, y)| *y).sum::<f64>() / n;
        let cov = rows
            .iter()
            .map(|(x, y)| (*x as f64 - mx) * (y - my))
            .sum::<f64>()
            / n;
        let sx = (rows
            .iter()
            .map(|(x, _)| (*x as f64 - mx).powi(2))
            .sum::<f64>()
            / n)
            .sqrt();
        let sy = (rows.iter().map(|(_, y)| (y - my).powi(2)).sum::<f64>() / n).sqrt();
        cov / (sx * sy)
    };
    println!("correlation(size, air)= {:.3}", corr);
    Ok(())
}
