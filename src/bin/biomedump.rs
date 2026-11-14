use anyhow::Result;
use mc_chunk_bench::mca::RegionFile;
use mc_chunk_bench::nbt::parse_root;

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: biomedump <world-dir>");
        std::process::exit(2);
    });
    let region_dir = std::path::Path::new(&dir).join("region");

    let mut region_paths: Vec<_> = std::fs::read_dir(&region_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "mca").unwrap_or(false))
        .collect();
    region_paths.sort();

    let mut total_chunks = 0usize;
    let mut total_sections = 0usize;
    let mut sections_with_biomes = 0usize;
    let mut sections_biome_pal = 0usize;
    let mut sections_biome_data = 0usize;
    let mut seen_palette: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut seen_biome_bits: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    let mut seen_entry_kinds: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();

    for rp in &region_paths {
        let region = RegionFile::open(rp)?;
        for slot in 0..1024 {
            if !region.is_present(slot) {
                continue;
            }
            let c = match region.read_chunk(slot) {
                Ok(Some(c)) => c,
                _ => continue,
            };
            let root = match parse_root(&c.nbt) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if root.get("Level").is_some() {
                continue; // legacy, skip for now
            }
            let Some(sections) = root.get("sections").and_then(|s| s.as_list()) else {
                continue;
            };
            total_chunks += 1;
            for sec in sections {
                total_sections += 1;
                let y = sec.get_i32("Y").unwrap_or(-999);
                let Some(bi) = sec.get("biomes") else {
                    continue;
                };
                sections_with_biomes += 1;
                let pal = bi.get("palette").and_then(|p| p.as_list());
                if let Some(pal) = pal {
                    sections_biome_pal += 1;
                    for p in pal {
                        match p {
                            mc_chunk_bench::nbt::NbtValue::String(s) => {
                                *seen_entry_kinds.entry("String".into()).or_insert(0) += 1;
                                *seen_palette.entry(s.clone()).or_insert(0) += 1;
                            }
                            _ => {
                                *seen_entry_kinds.entry("not-String".into()).or_insert(0) += 1;
                                if let Some(name) = p.get_str("Name") {
                                    *seen_palette.entry(name.to_string()).or_insert(0) += 1;
                                }
                            }
                        }
                    }
                }
                if let Some(data) = bi.get("data").and_then(|d| d.as_long_array()) {
                    sections_biome_data += 1;
                    let bits = (data.len() * 64) / 64;
                    *seen_biome_bits.entry(bits).or_insert(0) += 1;
                }
                if total_chunks <= 2 {
                    let pal = bi.get("palette").and_then(|p| p.as_list());
                    println!("chunk slot={slot} region={} secY={y}", rp.file_name().unwrap_or_default().to_string_lossy());
                    println!("  biomes tag: {:?}", bi);
                    if let Some(pal) = pal {
                        println!("  palette list len={} first entry: {:?}", pal.len(), pal.first());
                    }
                }
            }
        }
    }
    println!();
    println!("total_chunks = {total_chunks}");
    println!("total_sections = {total_sections}");
    println!("sections_with_biomes = {sections_with_biomes}");
    println!("sections_biome_pal = {sections_biome_pal}");
    println!("sections_biome_data = {sections_biome_data}");
    println!("biome palette entry kinds: {:?}", seen_entry_kinds);
    println!("biome palette names seen (name -> count of palette entries):");
    for (k, v) in &seen_palette {
        println!("  {k}: {v}");
    }
    println!("biome data long-array bits distribution: {:?}", seen_biome_bits);
    Ok(())
}
