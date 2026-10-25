use anyhow::Result;
use mc_chunk_bench::bench::run_benchmark;
use mc_chunk_bench::chunk::prune_interior;
use mc_chunk_bench::web;
use mc_chunk_bench::world::{discover_worlds, load_dataset, WorldData};
use std::path::{Path, PathBuf};

const DEFAULT_PORT: u16 = 8095;

fn default_saves_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".minecraft/saves")
}

fn expand_home(p: &Path) -> PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix("~/")) {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

struct Args {
    cmd: String,
    worlds_dir: PathBuf,
    excludes: Vec<String>,
    max_chunks: usize,
    seed: u64,
    port: u16,
    host: String,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let cmd = match argv.first() {
        Some(a) if !a.starts_with('-') => {
            i += 1;
            a.clone()
        }
        _ => "bench".to_string(),
    };
    let mut worlds_dir = default_saves_dir();
    let mut excludes: Vec<String> = Vec::new();
    let mut max_chunks = 512usize;
    let mut seed = 1u64;
    let mut port = DEFAULT_PORT;
    let mut host = String::from("127.0.0.1");
    while let Some(arg) = argv.get(i) {
        i += 1;
        let mut next = || {
            i += 1;
            argv.get(i - 1).cloned().expect("missing argument value")
        };
        match arg.as_str() {
            "--worlds-dir" => {
                worlds_dir = expand_home(Path::new(&next()));
            }
            "--exclude" => {
                excludes.push(next());
            }
            "--max-chunks" => {
                max_chunks = next().parse().expect("--max-chunks must be a number");
            }
            "--seed" => {
                seed = next().parse().expect("--seed must be a number");
            }
            "--port" => {
                port = next().parse().expect("--port must be a number");
            }
            "--host" => {
                host = next();
            }
            "-h" | "--help" => {
                usage();
                std::process::exit(0);
            }
            other => {
                if !other.starts_with('-') {
                    worlds_dir = expand_home(Path::new(other));
                }
            }
        }
    }
    Args {
        cmd,
        worlds_dir,
        excludes,
        max_chunks,
        seed,
        port,
        host,
    }
}

fn usage() {
    eprintln!("Usage:");
    eprintln!(
        "  mc-chunk-bench bench [--worlds-dir DIR] [--exclude NAME]... [--max-chunks N] [--seed S]"
    );
    eprintln!("  mc-chunk-bench serve [--worlds-dir DIR] [--exclude NAME]... [--max-chunks N] [--seed S] [--host H] [--port P]");
    eprintln!();
    eprintln!(
        "  --worlds-dir  path to a save, or a directory of saves (default: ~/.minecraft/saves)"
    );
    eprintln!("  --exclude      exclude worlds whose name contains NAME (repeatable)");
    eprintln!("  --max-chunks  chunks to sample (default 512)");
    eprintln!("  --seed        RNG seed for sampling (default 1)");
    eprintln!("  --host        bind address for 'serve' (default 127.0.0.1; use 0.0.0.0 to expose to LAN)");
    eprintln!("  --port        HTTP port for 'serve' (default {DEFAULT_PORT})");
}

fn pruned_dataset(data: &WorldData) -> WorldData {
    let mut chunks = data.chunks.clone();
    let mut removed = 0usize;
    for lc in chunks.iter_mut() {
        removed += prune_interior(&mut lc.chunk);
    }
    eprintln!("occlusion culling: removed {removed} interior blocks");
    WorldData {
        chunks,
        registry: data.registry.clone(),
        biomes: data.biomes.clone(),
        biome_block_lists: data.biome_block_lists.clone(),
        global_block_counts: data.global_block_counts.clone(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();

    let worlds = discover_worlds(&args.worlds_dir, &args.excludes)?;
    eprintln!(
        "worlds: {}",
        worlds
            .iter()
            .map(|w| w.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let data = load_dataset(&worlds, args.max_chunks, args.seed)?;
    eprintln!(
        "loaded {} chunks, {} global blocks, {} biomes",
        data.chunks.len(),
        data.registry.names.len(),
        data.biomes.names.len()
    );
    for (b, c) in data.global_block_counts.iter().take(5) {
        eprintln!("  top block: {} (x{})", data.registry.names[*b as usize], c);
    }

    let pruned = pruned_dataset(&data);
    let report = run_benchmark(&data, &pruned, args.seed, args.max_chunks)?;

    if args.cmd == "serve" {
        let app = web::router(report);
        let addr = format!("{}:{}", args.host, args.port);
        eprintln!("listening on http://{addr} (ctrl-c to stop)");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
        Ok(())
    }
}
