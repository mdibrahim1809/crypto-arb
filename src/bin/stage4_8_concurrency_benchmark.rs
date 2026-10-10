//! Stage 4.8 — bounded, read-only RPC concurrency benchmark.
//!
//! Reads pair addresses from the Stage 4.x registry (dex,pair_address,fee_bps).
//! Benchmarks JSON-RPC batch sizes and bounded worker counts against one pinned block.
//! It only performs eth_call(pair.getReserves); no wallet, signing, or transactions.
//!
//! Environment:
//! ETH_RPC_URL (required)
//! ARB_POOL_REGISTRY (default pools_multitoken_stage4_5.csv)
//! ARB_BENCH_TRIALS (default 5, range 3..10)
//! ARB_BENCH_REPEATS_PER_POOL (default 10, range 1..20)
//! ARB_BENCH_BATCH_SIZES (default "10,20,50")
//! ARB_BENCH_WORKERS (default "1,2,4")
//! ARB_BENCH_PAUSE_MS (default 150, range 0..2000)

use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{
    env, fs, fs::OpenOptions, io::Write, sync::Arc, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_REGISTRY: &str = "pools_multitoken_stage4_5.csv";
const MAX_POOLS: usize = 50;
const RESERVES_SELECTOR: &str = "0x0902f1ac";

#[derive(Clone, Debug)]
struct PoolConfig { dex: String, address: String, fee_bps: u32 }

#[derive(Clone, Debug)]
struct BenchRow {
    timestamp: u64,
    block: u64,
    pools: usize,
    calls: usize,
    batch_size: usize,
    workers: usize,
    trial: usize,
    elapsed_ms: f64,
    requests: usize,
    successes: usize,
    failures: usize,
    error: String,
}

fn env_usize(key: &str, default: usize, min: usize, max: usize) -> usize {
    env::var(key).ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(default).clamp(min, max)
}

fn parse_csv_usizes(key: &str, default: &str, allowed: &[usize]) -> Result<Vec<usize>, String> {
    let raw = env::var(key).unwrap_or_else(|_| default.to_string());
    let mut values = Vec::new();
    for part in raw.split(',') {
        let n: usize = part.trim().parse().map_err(|_| format!("{key} contains invalid number: {}", part.trim()))?;
        if !allowed.contains(&n) { return Err(format!("{key} values must be one of {allowed:?}")); }
        if !values.contains(&n) { values.push(n); }
    }
    if values.is_empty() { return Err(format!("{key} cannot be empty")); }
    Ok(values)
}

fn normalize_address(s: &str) -> Result<String, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.len() != 42 || !s.starts_with("0x") || !s[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid Ethereum address: {s}"));
    }
    Ok(s)
}

fn load_registry(path: &str) -> Result<Vec<PoolConfig>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut pools = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.to_ascii_lowercase().starts_with("dex,") { continue; }
        let cols: Vec<_> = line.split(',').map(str::trim).collect();
        if cols.len() != 3 { return Err(format!("{path}:{} expected dex,pair_address,fee_bps", line_no + 1)); }
        let address = normalize_address(cols[1])?;
        let fee_bps: u32 = cols[2].parse().map_err(|_| format!("{path}:{} invalid fee_bps", line_no + 1))?;
        if fee_bps >= 10_000 { return Err(format!("{path}:{} fee_bps must be below 10000", line_no + 1)); }
        if !seen.insert(address.clone()) { return Err(format!("duplicate pair address: {address}")); }
        pools.push(PoolConfig { dex: cols[0].to_string(), address, fee_bps });
        if pools.len() > MAX_POOLS { return Err(format!("registry exceeds safe benchmark cap of {MAX_POOLS} pools")); }
    }
    if pools.is_empty() { return Err(format!("{path} contains no pools")); }
    Ok(pools)
}

fn rpc_block_number(client: &Client, url: &str) -> Result<u64, String> {
    let response = client.post(url).json(&json!({
        "jsonrpc":"2.0", "id":1, "method":"eth_blockNumber", "params":[]
    })).send().map_err(|e| format!("block-number request failed: {e}"))?;
    if !response.status().is_success() { return Err(format!("block-number HTTP status {}", response.status())); }
    let v: Value = response.json().map_err(|e| format!("invalid block-number JSON: {e}"))?;
    if let Some(err) = v.get("error") { return Err(format!("block-number RPC error: {err}")); }
    let h = v.get("result").and_then(Value::as_str).ok_or("block number missing result")?;
    u64::from_str_radix(h.trim_start_matches("0x"), 16).map_err(|e| format!("invalid block number: {e}"))
}

fn call_batch(client: &Client, url: &str, calls: &[(String, String)], block_hex: &str) -> Result<usize, String> {
    if calls.is_empty() { return Ok(0); }
    let body: Vec<Value> = calls.iter().enumerate().map(|(i, (to, data))| json!({
        "jsonrpc":"2.0", "id": i as u64 + 1, "method":"eth_call",
        "params":[{"to":to,"data":data}, block_hex]
    })).collect();
    let response = client.post(url).json(&body).send().map_err(|e| format!("HTTP/RPC transport error: {e}"))?;
    if !response.status().is_success() { return Err(format!("HTTP status {}", response.status())); }
    let v: Value = response.json().map_err(|e| format!("invalid JSON response: {e}"))?;
    let arr = v.as_array().ok_or("provider did not return a JSON-RPC batch array")?;
    let mut successes = 0usize;
    let mut seen = std::collections::HashSet::new();
    for item in arr {
        let id = item.get("id").and_then(Value::as_u64).ok_or("response item missing numeric id")?;
        if id == 0 || id > calls.len() as u64 || !seen.insert(id) {
            return Err("response contained invalid or duplicate JSON-RPC id".into());
        }
        if item.get("error").is_none() && item.get("result").and_then(Value::as_str).is_some() {
            successes += 1;
        }
    }
    if seen.len() != calls.len() { return Err(format!("batch returned {} of {} responses", seen.len(), calls.len())); }
    Ok(successes)
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn run_trial(
    client: &Client, url: &str, calls: &[(String, String)], block: u64,
    batch_size: usize, workers: usize, trial: usize, pool_count: usize,
) -> BenchRow {
    let block_hex = format!("0x{block:x}");
    let chunks: Vec<Vec<(String, String)>> = calls.chunks(batch_size).map(|c| c.to_vec()).collect();
    let chunk_count = chunks.len();
    let worker_count = workers.min(chunk_count.max(1));
    let mut assignments: Vec<Vec<Vec<(String, String)>>> = (0..worker_count).map(|_| Vec::new()).collect();
    for (i, chunk) in chunks.into_iter().enumerate() {
        assignments[i % worker_count].push(chunk);
    }

    let client = client.clone();
    let url = url.to_string();
    let block_hex = Arc::new(block_hex);
    let started = Instant::now();
    let handles: Vec<_> = assignments.into_iter().map(|assigned| {
        let c = client.clone();
        let u = url.clone();
        let b = Arc::clone(&block_hex);
        thread::spawn(move || {
            let mut successes = 0usize;
            let mut requests = 0usize;
            let mut failures = 0usize;
            let mut errors = Vec::new();
            for chunk in assigned {
                requests += 1;
                match call_batch(&c, &u, &chunk, &b) {
                    Ok(n) => {
                        successes += n;
                        if n != chunk.len() {
                            failures += chunk.len() - n;
                            errors.push(format!("{} of {} calls returned RPC errors", chunk.len() - n, chunk.len()));
                        }
                    }
                    Err(e) => {
                        failures += chunk.len();
                        errors.push(e);
                    }
                }
            }
            (requests, successes, failures, errors)
        })
    }).collect();

    let mut requests = 0usize;
    let mut successes = 0usize;
    let mut failures = 0usize;
    let mut errors = Vec::new();
    for handle in handles {
        match handle.join() {
            Ok((r, s, f, e)) => { requests += r; successes += s; failures += f; errors.extend(e); }
            Err(_) => { failures += calls.len(); errors.push("benchmark worker panicked".into()); }
        }
    }
    BenchRow {
        timestamp: unix_seconds(), block, pools: pool_count, calls: calls.len(),
        batch_size, workers, trial, elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        requests, successes, failures, error: errors.into_iter().take(3).collect::<Vec<_>>().join(" | "),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((sorted.len() - 1) as f64 * p).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn append_csv(path: &str, rows: &[BenchRow]) -> Result<(), String> {
    let exists = std::path::Path::new(path).exists();
    let mut file = OpenOptions::new().create(true).append(true).open(path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    if !exists {
        writeln!(file, "timestamp,block,pools,calls,batch_size,workers,trial,elapsed_ms,requests,successes,failures,error")
            .map_err(|e| format!("cannot write CSV header: {e}"))?;
    }
    for r in rows {
        let error = r.error.replace('"', "\"\"");
        writeln!(file, "{},{},{},{},{},{},{},{:.3},{},{},{},\"{}\"",
            r.timestamp, r.block, r.pools, r.calls, r.batch_size, r.workers, r.trial,
            r.elapsed_ms, r.requests, r.successes, r.failures, error)
            .map_err(|e| format!("cannot append CSV row: {e}"))?;
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let url = env::var("ETH_RPC_URL").map_err(|_| "ETH_RPC_URL is not set in this terminal session".to_string())?;
    let registry_path = env::var("ARB_POOL_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.to_string());
    let trials = env_usize("ARB_BENCH_TRIALS", 5, 3, 10);
    let repeats = env_usize("ARB_BENCH_REPEATS_PER_POOL", 10, 1, 20);
    let pause_ms = env_usize("ARB_BENCH_PAUSE_MS", 150, 0, 2000);
    let batch_sizes = parse_csv_usizes("ARB_BENCH_BATCH_SIZES", "10,20,50", &[1, 5, 10, 20, 50])?;
    let worker_counts = parse_csv_usizes("ARB_BENCH_WORKERS", "1,2,4", &[1, 2, 4])?;
    let pools = load_registry(&registry_path)?;
    let client = Client::builder().timeout(Duration::from_secs(15)).build()
        .map_err(|e| format!("cannot create HTTP client: {e}"))?;
    let block = rpc_block_number(&client, &url)?;
    let block_hex = format!("0x{block:x}");

    let mut calls = Vec::with_capacity(pools.len() * repeats);
    for _ in 0..repeats {
        for pool in &pools {
            // The same read-only reserves call is repeated to create a controlled request load.
            calls.push((pool.address.clone(), RESERVES_SELECTOR.to_string()));
        }
    }

    println!("Stage 4.8 | bounded RPC concurrency benchmark");
    println!("Registry: {registry_path} | pools: {} / {MAX_POOLS} max | calls per trial: {}", pools.len(), calls.len());
    println!("Pinned block: {block} | trials per configuration: {trials} | pause: {pause_ms} ms");
    println!("Batch sizes: {batch_sizes:?} | worker counts: {worker_counts:?}");
    println!("Read-only safety: eth_call only; no wallet, signing, or transaction submission.");
    println!("Note: repeated reserve calls are a controlled load test, not extra unique market coverage.");

    let mut all_rows = Vec::new();
    for batch_size in batch_sizes {
        for &workers in &worker_counts {
            let mut times = Vec::new();
            println!("\nCONFIG | batch_size={batch_size} | workers={workers}");
            for trial in 1..=trials {
                let row = run_trial(&client, &url, &calls, block, batch_size, workers, trial, pools.len());
                println!(
                    "trial {trial}/{trials} | {:.2} ms | HTTP batches {} | calls ok {}/{} | failures {}{}",
                    row.elapsed_ms, row.requests, row.successes, row.calls, row.failures,
                    if row.error.is_empty() { String::new() } else { format!(" | {}", row.error) }
                );
                times.push(row.elapsed_ms);
                all_rows.push(row);
                if pause_ms > 0 { thread::sleep(Duration::from_millis(pause_ms as u64)); }
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            println!("SUMMARY | mean {:.2} ms | median {:.2} ms | p95 {:.2} ms | min {:.2} ms | max {:.2} ms",
                mean, percentile(&times, 0.50), percentile(&times, 0.95), times[0], times[times.len()-1]);
        }
    }
    append_csv("stage4_8_trials.csv", &all_rows)?;
    println!("\nSaved {} trial rows to stage4_8_trials.csv", all_rows.len());
    println!("Compare configurations using median and p95, and reject any configuration with failures.");
    Ok(())
}

fn main() {
    if let Err(e) = run() { eprintln!("ERROR: {e}"); std::process::exit(1); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_validation_accepts_pair_shape() {
        assert!(normalize_address("0xB4e16D0168e52d35CaCD2c6185b44281Ec28C9Dc").is_ok());
    }

    #[test]
    fn address_validation_rejects_bad_input() {
        assert!(normalize_address("0x1234").is_err());
        assert!(normalize_address("not-an-address").is_err());
    }

    #[test]
    fn percentile_uses_nearest_rank_style_index() {
        let values = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile(&values, 0.50), 30.0);
        assert_eq!(percentile(&values, 0.95), 50.0);
    }

    #[test]
    fn csv_config_rejects_unsupported_values() {
        assert!(parse_csv_usizes("SHOULD_NOT_EXIST_4_8", "3", &[1, 2, 4]).is_err());
    }
}
