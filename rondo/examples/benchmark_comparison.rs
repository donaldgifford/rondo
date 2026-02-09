//! Benchmark A: Write-path overhead comparison.
//!
//! Compares rondo `record()` against alternative write approaches:
//! - Direct `write()` syscall to a file
//! - Atomic counter increment (simulating Prometheus client `counter.inc()`)
//! - UDP send (simulating StatsD push)
//!
//! Run with: `cargo run -p rondo --release --example benchmark_comparison`

#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rondo::schema::{LabelMatcher, SchemaConfig, TierConfig};
use rondo::store::Store;

const ITERATIONS: u64 = 10_000_000;
const WARMUP: u64 = 100_000;

fn main() {
    println!("=== rondo Write-Path Overhead Comparison ===");
    println!("Iterations: {ITERATIONS}");
    println!();

    let rondo_stats = bench_rondo_record();
    let atomic_stats = bench_atomic_counter();
    let syscall_stats = bench_write_syscall();
    let udp_stats = bench_udp_send();

    println!();
    println!("=== Summary ===");
    println!(
        "{:<30} {:>10} {:>10} {:>10} {:>15}",
        "Method", "p50 (ns)", "p99 (ns)", "p999 (ns)", "Throughput"
    );
    println!(
        "{:-<30} {:-<10} {:-<10} {:-<10} {:-<15}",
        "", "", "", "", ""
    );

    for (name, stats) in [
        ("rondo record()", &rondo_stats),
        ("atomic counter.inc()", &atomic_stats),
        ("write() syscall", &syscall_stats),
        ("UDP send (StatsD-like)", &udp_stats),
    ] {
        println!(
            "{:<30} {:>10.1} {:>10.1} {:>10.1} {:>12.0} w/s",
            name, stats.p50, stats.p99, stats.p999, stats.throughput
        );
    }

    println!();
    println!("rondo vs alternatives:");
    println!(
        "  vs atomic:   {:.1}x faster (p99)",
        atomic_stats.p99 / rondo_stats.p99
    );
    println!(
        "  vs write():  {:.1}x faster (p99)",
        syscall_stats.p99 / rondo_stats.p99
    );
    println!(
        "  vs UDP:      {:.1}x faster (p99)",
        udp_stats.p99 / rondo_stats.p99
    );
}

#[derive(Debug)]
struct BenchStats {
    p50: f64,
    p99: f64,
    p999: f64,
    throughput: f64,
}

fn bench_rondo_record() -> BenchStats {
    print!("Benchmarking rondo record()...");
    std::io::stdout().flush().unwrap();

    let temp_dir = std::env::temp_dir().join("rondo_bench_comparison");
    let _ = std::fs::remove_dir_all(&temp_dir);

    let schemas = vec![SchemaConfig {
        name: "bench".to_string(),
        label_matcher: LabelMatcher::any(),
        tiers: vec![TierConfig {
            interval: Duration::from_secs(1),
            retention: Duration::from_secs(3600),
            consolidation_fn: None,
        }],
        max_series: 10,
    }];

    let mut store = Store::open(&temp_dir, schemas).unwrap();
    let handle = store
        .register("metric", &[("id".to_string(), "0".to_string())])
        .unwrap();

    let base_time = 1_700_000_000_000_000_000u64;
    let mut ts = base_time;

    // Warmup
    for _ in 0..WARMUP {
        ts += 1_000_000_000;
        store.record(handle, 42.5, ts).unwrap();
    }

    // Measure
    let mut latencies = Vec::with_capacity(ITERATIONS as usize);
    for _ in 0..ITERATIONS {
        ts += 1_000_000_000;
        let start = Instant::now();
        store.record(handle, 42.5, ts).unwrap();
        latencies.push(start.elapsed().as_nanos() as f64);
    }

    let _ = std::fs::remove_dir_all(&temp_dir);

    let stats = compute_stats(&mut latencies);
    println!(" done (p99={:.1}ns)", stats.p99);
    stats
}

fn bench_atomic_counter() -> BenchStats {
    print!("Benchmarking atomic counter.inc()...");
    std::io::stdout().flush().unwrap();

    let counter = AtomicU64::new(0);

    // Warmup
    for _ in 0..WARMUP {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    // Measure
    let mut latencies = Vec::with_capacity(ITERATIONS as usize);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        counter.fetch_add(1, Ordering::Relaxed);
        latencies.push(start.elapsed().as_nanos() as f64);
    }

    let stats = compute_stats(&mut latencies);
    println!(" done (p99={:.1}ns)", stats.p99);
    stats
}

fn bench_write_syscall() -> BenchStats {
    print!("Benchmarking write() syscall...");
    std::io::stdout().flush().unwrap();

    let temp_path = std::env::temp_dir().join("rondo_bench_write");
    let mut file = std::fs::File::create(&temp_path).unwrap();

    let data = b"metric_0 42.5 1700000000\n";

    // Warmup
    for _ in 0..WARMUP {
        file.write_all(data).unwrap();
    }

    // Measure
    let mut latencies = Vec::with_capacity(ITERATIONS as usize);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        file.write_all(data).unwrap();
        latencies.push(start.elapsed().as_nanos() as f64);
    }

    let _ = std::fs::remove_file(&temp_path);

    let stats = compute_stats(&mut latencies);
    println!(" done (p99={:.1}ns)", stats.p99);
    stats
}

fn bench_udp_send() -> BenchStats {
    print!("Benchmarking UDP send (StatsD-like)...");
    std::io::stdout().flush().unwrap();

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    // Send to a random port — we don't care if anyone receives
    let dest = "127.0.0.1:18125";
    let data = b"metric_0:42.5|g";

    // Warmup
    for _ in 0..WARMUP {
        let _ = socket.send_to(data, dest);
    }

    // Measure
    let mut latencies = Vec::with_capacity(ITERATIONS as usize);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let _ = socket.send_to(data, dest);
        latencies.push(start.elapsed().as_nanos() as f64);
    }

    let stats = compute_stats(&mut latencies);
    println!(" done (p99={:.1}ns)", stats.p99);
    stats
}

fn compute_stats(latencies: &mut [f64]) -> BenchStats {
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = latencies.len();
    let total_ns: f64 = latencies.iter().sum();

    BenchStats {
        p50: latencies[n / 2],
        p99: latencies[n * 99 / 100],
        p999: latencies[n * 999 / 1000],
        throughput: n as f64 / (total_ns / 1_000_000_000.0),
    }
}
