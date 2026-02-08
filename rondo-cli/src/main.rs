//! CLI for the rondo time-series storage engine.
//!
//! Provides commands for inspecting, querying, and benchmarking rondo stores.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};

/// rondo — Embedded round-robin time-series storage engine CLI.
#[derive(Parser)]
#[command(name = "rondo", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Available CLI commands.
#[derive(Subcommand)]
enum Commands {
    /// Display store metadata, schemas, series, and tier usage.
    Info {
        /// Path to the store directory.
        store_path: PathBuf,
    },

    /// Query time-series data from a store.
    Query {
        /// Path to the store directory.
        store_path: PathBuf,

        /// Series name to query.
        series: String,

        /// Time range to query (e.g., "1h", "30m", "7d").
        #[arg(long, default_value = "1h")]
        range: String,

        /// Tier to query (0 = highest resolution, "auto" for automatic selection).
        #[arg(long, default_value = "auto")]
        tier: String,

        /// Output format.
        #[arg(long, default_value = "csv")]
        format: OutputFormat,
    },

    /// Run a write-path microbenchmark.
    Bench {
        /// Number of data points to write.
        #[arg(long, default_value = "10000000")]
        points: u64,

        /// Number of series to register.
        #[arg(long, default_value = "30")]
        series: u32,
    },
}

/// Output format for query results.
#[derive(Clone, ValueEnum)]
enum OutputFormat {
    /// Comma-separated values.
    Csv,
    /// JSON array of objects.
    Json,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Info { store_path } => cmd_info(&store_path),
        Commands::Query {
            store_path,
            series,
            range,
            tier,
            format,
        } => cmd_query(&store_path, &series, &range, &tier, &format),
        Commands::Bench { points, series } => cmd_bench(points, series),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

/// Implements `rondo info <store_path>`.
fn cmd_info(store_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Open the store with empty schemas to read metadata.
    // We need to read meta.json manually to get the schemas.
    let meta_path = store_path.join("meta.json");
    if !meta_path.exists() {
        return Err(format!("No store found at '{}'", store_path.display()).into());
    }

    let meta_data = std::fs::read_to_string(&meta_path)?;
    let meta: serde_json::Value = serde_json::from_str(&meta_data)?;

    println!("Store: {}", store_path.display());
    println!();

    // Parse schemas from metadata
    if let Some(schemas) = meta.get("schemas").and_then(|s| s.as_array()) {
        println!("Schemas: {}", schemas.len());
        println!();

        for (i, schema) in schemas.iter().enumerate() {
            let name = schema
                .get("config")
                .and_then(|c| c.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            let max_series = schema
                .get("config")
                .and_then(|c| c.get("max_series"))
                .and_then(|m| m.as_u64())
                .unwrap_or(0);
            let hash = schema
                .get("hash")
                .and_then(|h| h.as_u64())
                .unwrap_or(0);

            println!("  Schema {i}: \"{name}\"");
            println!("    Max series: {max_series}");
            println!("    Hash: {hash:016x}");

            // List tiers from metadata
            if let Some(tiers) = schema
                .get("config")
                .and_then(|c| c.get("tiers"))
                .and_then(|t| t.as_array())
            {
                println!("    Tiers: {}", tiers.len());
                for (j, tier) in tiers.iter().enumerate() {
                    let interval = tier.get("interval").and_then(|i| i.as_object());
                    let retention = tier.get("retention").and_then(|r| r.as_object());
                    let consolidation_fn = tier
                        .get("consolidation_fn")
                        .and_then(|c| {
                            if c.is_null() {
                                None
                            } else {
                                Some(format!("{c}"))
                            }
                        });

                    let interval_str = interval
                        .and_then(|i| i.get("secs").and_then(|s| s.as_u64()))
                        .map(format_duration_secs)
                        .unwrap_or_else(|| "?".to_string());
                    let retention_str = retention
                        .and_then(|r| r.get("secs").and_then(|s| s.as_u64()))
                        .map(format_duration_secs)
                        .unwrap_or_else(|| "?".to_string());

                    let fn_str = consolidation_fn
                        .as_deref()
                        .unwrap_or("none (raw)");

                    println!("      Tier {j}: interval={interval_str}, retention={retention_str}, fn={fn_str}");

                    // Check for slab file and print size
                    let slab_path = store_path.join(format!("schema_{i}")).join(format!("tier_{j}.slab"));
                    if slab_path.exists()
                        && let Ok(metadata) = std::fs::metadata(&slab_path)
                    {
                        println!("        Slab: {} ({} bytes)", slab_path.display(), metadata.len());
                    }
                }
            }
            println!();
        }
    }

    // Calculate total disk size
    let total_size = dir_size(store_path)?;
    println!("Total disk usage: {} ({total_size} bytes)", format_bytes(total_size));

    // Show consolidation cursor state
    let cursor_path = store_path.join("consolidation_cursors.json");
    if cursor_path.exists() {
        println!();
        println!("Consolidation cursors: present");
    }

    // Try to open the store to show series info
    let schemas = reconstruct_schemas(&meta);
    if let Ok(store) = rondo::Store::open(store_path, schemas) {
        let handles = store.handles();
        if !handles.is_empty() {
            println!();
            println!("Registered series: {}", handles.len());
            for handle in &handles {
                if let Some((name, labels)) = store.series_info(handle) {
                    let labels_str = if labels.is_empty() {
                        String::new()
                    } else {
                        let pairs: Vec<_> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        format!(" {{{}}}", pairs.join(", "))
                    };
                    println!("  - {name}{labels_str} (schema={}, column={})", handle.schema_index, handle.column);
                }
            }
        }
    }

    Ok(())
}

/// Implements `rondo query <store_path> <series>`.
fn cmd_query(
    store_path: &PathBuf,
    series_name: &str,
    range: &str,
    tier_str: &str,
    format: &OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta_path = store_path.join("meta.json");
    if !meta_path.exists() {
        return Err(format!("No store found at '{}'", store_path.display()).into());
    }

    let meta_data = std::fs::read_to_string(&meta_path)?;
    let meta: serde_json::Value = serde_json::from_str(&meta_data)?;
    let schemas = reconstruct_schemas(&meta);

    let store = rondo::Store::open(store_path, schemas)?;

    // Find the series handle by name
    let handles = store.handles();
    let handle = handles
        .iter()
        .find(|h| {
            store
                .series_info(h)
                .is_some_and(|(name, _)| name == series_name)
        })
        .ok_or_else(|| format!("Series '{series_name}' not found"))?;

    // Parse time range
    let range_ns = parse_duration(range)?;
    #[allow(clippy::cast_possible_truncation)] // Current epoch nanos fit in u64 until year 2554
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let start_ns = now_ns.saturating_sub(range_ns);
    let end_ns = now_ns;

    // Query data
    let result = if tier_str == "auto" {
        store.query_auto(*handle, start_ns, end_ns)?
    } else {
        let tier: usize = tier_str.parse()?;
        store.query(*handle, tier, start_ns, end_ns)?
    };

    let tier_used = result.tier_used();
    let data: Vec<_> = result.collect();

    match format {
        OutputFormat::Csv => {
            println!("# series={series_name}, tier={tier_used}, points={}", data.len());
            println!("timestamp_ns,value");
            for (ts, val) in &data {
                println!("{ts},{val}");
            }
        }
        OutputFormat::Json => {
            let json_data: Vec<serde_json::Value> = data
                .iter()
                .map(|(ts, val)| {
                    serde_json::json!({
                        "timestamp_ns": ts,
                        "value": val,
                    })
                })
                .collect();

            let output = serde_json::json!({
                "series": series_name,
                "tier": tier_used,
                "count": data.len(),
                "data": json_data,
            });

            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }

    Ok(())
}

/// Implements `rondo bench`.
#[allow(clippy::cast_precision_loss)] // Benchmark stats are fine with f64 precision
fn cmd_bench(points: u64, series_count: u32) -> Result<(), Box<dyn std::error::Error>> {
    println!("rondo write-path benchmark");
    println!("  Points: {points}");
    println!("  Series: {series_count}");
    println!();

    let temp_dir = std::env::temp_dir().join("rondo_bench");
    let _ = std::fs::remove_dir_all(&temp_dir);

    let schemas = vec![rondo::SchemaConfig {
        name: "bench".to_string(),
        label_matcher: rondo::LabelMatcher::any(),
        tiers: vec![rondo::TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(3600),
            consolidation_fn: None,
        }],
        max_series: series_count + 10,
    }];

    let mut store = rondo::Store::open(&temp_dir, schemas)?;

    let mut handles = Vec::with_capacity(series_count as usize);
    for i in 0..series_count {
        let handle = store.register(
            &format!("metric_{i}"),
            &[("id".to_string(), format!("{i}"))],
        )?;
        handles.push(handle);
    }

    println!("Writing {points} data points across {series_count} series...");

    let base_time = 1_700_000_000_000_000_000u64;
    let mut ts = base_time;
    let points_per_series = points / u64::from(series_count);

    let start = Instant::now();

    for _ in 0..points_per_series {
        ts += 1_000_000_000;
        for (i, handle) in handles.iter().enumerate() {
            store.record(*handle, i as f64, ts).unwrap();
        }
    }

    let elapsed = start.elapsed();
    let total_writes = points_per_series * u64::from(series_count);
    let ns_per_write = elapsed.as_nanos() as f64 / total_writes as f64;
    let writes_per_sec = total_writes as f64 / elapsed.as_secs_f64();

    println!();
    println!("Results:");
    println!("  Total writes: {total_writes}");
    println!("  Elapsed: {elapsed:.3?}");
    println!("  Avg latency: {ns_per_write:.1} ns/write");
    println!("  Throughput: {writes_per_sec:.0} writes/sec");
    println!();

    // Clean up
    let _ = std::fs::remove_dir_all(&temp_dir);

    Ok(())
}

/// Parses a human-readable duration string (e.g., "1h", "30m", "7d") to nanoseconds.
fn parse_duration(s: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Empty duration string".into());
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str.parse()?;

    let secs = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        _ => return Err(format!("Unknown duration unit: '{unit}'. Use s, m, h, or d.").into()),
    };

    Ok(secs * 1_000_000_000)
}

/// Formats seconds as a human-readable duration.
fn format_duration_secs(secs: u64) -> String {
    if secs >= 86400 && secs.is_multiple_of(86400) {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Formats a byte count as a human-readable string.
#[allow(clippy::cast_precision_loss)] // Byte counts are display-only
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Recursively calculates directory size.
fn dir_size(path: &PathBuf) -> Result<u64, Box<dyn std::error::Error>> {
    let mut total = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path)?;
            } else {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

/// Reconstructs `SchemaConfig` values from stored metadata JSON.
fn reconstruct_schemas(meta: &serde_json::Value) -> Vec<rondo::SchemaConfig> {
    let Some(schemas) = meta.get("schemas").and_then(|s| s.as_array()) else {
        return Vec::new();
    };

    schemas
        .iter()
        .filter_map(|schema| {
            let config = schema.get("config")?;
            serde_json::from_value::<rondo::SchemaConfig>(config.clone()).ok()
        })
        .collect()
}
