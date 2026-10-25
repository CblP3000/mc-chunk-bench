use anyhow::{bail, Context, Result};
use mc_chunk_bench::bench::{run_benchmark, BenchmarkReport};
use mc_chunk_bench::chunk::prune_interior;
use mc_chunk_bench::web;
use mc_chunk_bench::world::{discover_worlds, load_dataset, WorldData};
use std::path::{Path, PathBuf};

const DEFAULT_PORT: u16 = 8095;
const DEFAULT_REPORT: &str = "target/last_report.json";

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
    worlds: Vec<String>,
    excludes: Vec<String>,
    max_chunks: usize,
    seed: u64,
    port: u16,
    host: String,
    save_report: Option<PathBuf>,
    from_report: Option<PathBuf>,
}

fn opt_value(argv: &[String], i: &mut usize, default: &str) -> String {
    if let Some(v) = argv.get(*i) {
        if !v.starts_with('-') {
            *i += 1;
            return v.clone();
        }
    }
    default.to_string()
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
    let mut worlds: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut max_chunks = 512usize;
    let mut seed = 1u64;
    let mut port = DEFAULT_PORT;
    let mut host = String::from("127.0.0.1");
    let mut save_report = None;
    let mut from_report = None;
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
            "--world" => {
                worlds.push(next());
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
            "--save-report" => {
                save_report = Some(expand_home(Path::new(&opt_value(
                    &argv,
                    &mut i,
                    DEFAULT_REPORT,
                ))));
            }
            "--from-report" => {
                from_report = Some(expand_home(Path::new(&opt_value(
                    &argv,
                    &mut i,
                    DEFAULT_REPORT,
                ))));
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
        worlds,
        excludes,
        max_chunks,
        seed,
        port,
        host,
        save_report,
        from_report,
    }
}

fn usage() {
    eprintln!("Usage:");
    eprintln!(
        "  mc-chunk-bench bench [--worlds-dir DIR] [--world NAME]... [--exclude NAME]... [--max-chunks N] [--seed S] [--save-report[=FILE]]"
    );
    eprintln!("  mc-chunk-bench serve [--worlds-dir DIR] [--world NAME]... [--exclude NAME]... [--max-chunks N] [--seed S] [--host H] [--port P] [--from-report[=FILE]]");
    eprintln!();
    eprintln!(
        "  --worlds-dir  path to a save, or a directory of saves (default: ~/.minecraft/saves)"
    );
    eprintln!("  --world        keep only worlds whose name contains NAME (repeatable)");
    eprintln!("  --exclude      exclude worlds whose name contains NAME (repeatable)");
    eprintln!("  --max-chunks  chunks to sample (default 512)");
    eprintln!("  --seed        RNG seed for sampling (default 1)");
    eprintln!("  --host        bind address for 'serve' (default 127.0.0.1; use 0.0.0.0 to expose to LAN)");
    eprintln!("  --port        HTTP port for 'serve' (default {DEFAULT_PORT})");
    eprintln!("  --save-report  save JSON report to FILE (default {DEFAULT_REPORT})");
    eprintln!("  --from-report  serve a previously saved report instead of re-benchmarking");
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

fn load_report(path: &Path) -> Result<BenchmarkReport> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read report: {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("cannot parse report: {}", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();

    if args.cmd == "serve" {
        if let Some(from) = &args.from_report {
            let report = load_report(from)?;
            let app = web::router(report);
            let addr = format!("{}:{}", args.host, args.port);
            eprintln!(
                "serving report from {} on http://{addr} (ctrl-c to stop)",
                from.display()
            );
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
            return Ok(());
        }
    }

    let mut worlds = discover_worlds(&args.worlds_dir, &args.excludes)?;
    if !args.worlds.is_empty() {
        let before = worlds.len();
        worlds.retain(|w| {
            let name = w
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            args.worlds.iter().any(|p| name.contains(&p.to_lowercase()))
        });
        if worlds.is_empty() {
            bail!("no worlds match --world filters");
        }
        eprintln!("  --world filter: {before} -> {}", worlds.len());
    }
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

    if let Some(save) = &args.save_report {
        if let Some(parent) = save.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(save, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("cannot write report: {}", save.display()))?;
        eprintln!("report saved to {}", save.display());
    }

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
