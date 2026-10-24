use anyhow::Result;
use chunk_saver::bench::run_benchmark;
use chunk_saver::chunk::prune_interior;
use chunk_saver::web;
use chunk_saver::world::{discover_worlds, load_dataset, WorldData};
use std::path::PathBuf;

const DEFAULT_SAVES: &str = "/mnt/D/lol/.minecraft/home/Fabric 1.21.11/saves";
const DEFAULT_PORT: u16 = 8095;

struct Args {
    cmd: String,
    world: PathBuf,
    max_chunks: usize,
    seed: u64,
    port: u16,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "bench".to_string());
    let mut world = PathBuf::from(DEFAULT_SAVES);
    let mut max_chunks = 512usize;
    let mut seed = 1u64;
    let mut port = DEFAULT_PORT;
    let mut a = args.next();
    while let Some(arg) = a {
        match arg.as_str() {
            "--world" => {
                world = PathBuf::from(args.next().expect("--world needs a value"));
            }
            "--max-chunks" => {
                max_chunks = args
                    .next()
                    .expect("--max-chunks needs a value")
                    .parse()
                    .expect("--max-chunks must be a number");
            }
            "--seed" => {
                seed = args.next().expect("--seed needs a value").parse().expect("--seed must be a number");
            }
            "--port" => {
                port = args
                    .next()
                    .expect("--port needs a value")
                    .parse()
                    .expect("--port must be a number");
                if port == 8080 {
                    eprintln!("warning: port 8080 is not allowed, using {DEFAULT_PORT}");
                    port = DEFAULT_PORT;
                }
            }
            "-h" | "--help" => {
                usage();
                std::process::exit(0);
            }
            other => {
                if !other.starts_with('-') {
                    world = PathBuf::from(other);
                }
            }
        }
        a = args.next();
    }
    Args {
        cmd,
        world,
        max_chunks,
        seed,
        port,
    }
}

fn usage() {
    eprintln!("Usage:");
    eprintln!("  chunk_saver bench [--world DIR] [--max-chunks N] [--seed S]");
    eprintln!("  chunk_saver serve [--world DIR] [--max-chunks N] [--seed S] [--port P]");
    eprintln!();
    eprintln!("  --world       path to a save, or a directory of saves (default: {DEFAULT_SAVES})");
    eprintln!("  --max-chunks  chunks to sample (default 512)");
    eprintln!("  --seed        RNG seed for sampling (default 1)");
    eprintln!("  --port        HTTP port for 'serve' (default {DEFAULT_PORT}; never 8080)");
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

    let worlds = discover_worlds(&args.world)?;
    eprintln!(
        "worlds: {}",
        worlds.iter().map(|w| w.display().to_string()).collect::<Vec<_>>().join(", ")
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
        let addr = format!("0.0.0.0:{}", args.port);
        eprintln!("listening on http://{addr} (ctrl-c to stop)");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
        Ok(())
    }
}
