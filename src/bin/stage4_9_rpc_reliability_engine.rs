//! Stage 4.9 — read-only RPC reliability engine.
//!
//! Loads the Stage 4.x pool registry, pins reads to one block, and retrieves
//! getReserves() using bounded sequential batches. Transient failures (429,
//! 5xx, transport errors, and per-item JSON-RPC errors) are retried with
//! exponential backoff and jitter. Successful calls are not repeated when
//! only individual items in a JSON-RPC batch fail.
//!
//! Environment:
//! ETH_RPC_URL (required)
//! ARB_POOL_REGISTRY (default pools_multitoken_stage4_5.csv)
//! ARB_BATCH_SIZE (default 10; allowed 1,5,10,20; keep low for rate-limited RPC)
//! ARB_RETRIES (default 3; retries after first attempt, 0..5)
//! ARB_RETRY_BASE_MS (default 500; 100..10000)
//! ARB_BATCH_PAUSE_MS (default 250; 0..5000)
//! ARB_MAX_POOLS (default 50; 1..50)
//!
//! This is a market-data reliability tool only: eth_call, no wallet/signing/trades.

use reqwest::blocking::{Client, Response};
use reqwest::header::RETRY_AFTER;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    env, fs, fs::OpenOptions, io::Write, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_REGISTRY: &str = "pools_multitoken_stage4_5.csv";
const RESERVES_SELECTOR: &str = "0x0902f1ac";

#[derive(Clone, Debug)]
struct PoolConfig {
    dex: String,
    address: String,
    fee_bps: u32,
}

#[derive(Clone, Debug)]
struct PoolResult {
    dex: String,
    address: String,
    fee_bps: u32,
    reserve0: Option<String>,
    reserve1: Option<String>,
    attempts: usize,
    elapsed_ms: f64,
    status: String,
    error: String,
}

fn env_usize(key: &str, default: usize, min: usize, max: usize) -> usize {
    env::var(key).ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(default).clamp(min, max)
}

fn normalize_address(s: &str) -> Result<String, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.len() != 42 || !s.starts_with("0x") || !s[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid Ethereum address: {s}"));
    }
    Ok(s)
}

fn load_registry(path: &str, max_pools: usize) -> Result<Vec<PoolConfig>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut pools = Vec::new();
    let mut seen = HashSet::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.to_ascii_lowercase().starts_with("dex,") {
            continue;
        }
        let cols: Vec<_> = line.split(',').map(str::trim).collect();
        if cols.len() != 3 {
            return Err(format!("{path}:{} expected dex,pair_address,fee_bps", line_no + 1));
        }
        let address = normalize_address(cols[1])?;
        let fee_bps: u32 = cols[2].parse().map_err(|_| format!("{path}:{} invalid fee_bps", line_no + 1))?;
        if fee_bps >= 10_000 {
            return Err(format!("{path}:{} fee_bps must be below 10000", line_no + 1));
        }
        if !seen.insert(address.clone()) {
            return Err(format!("duplicate pair address: {address}"));
        }
        pools.push(PoolConfig { dex: cols[0].to_string(), address, fee_bps });
        if pools.len() > max_pools {
            return Err(format!("{path} exceeds configured pool cap of {max_pools}"));
        }
    }
    if pools.is_empty() {
        return Err(format!("{path} contains no pools"));
    }
    Ok(pools)
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn parse_block(v: &Value) -> Result<u64, String> {
    if let Some(err) = v.get("error") {
        return Err(format!("block-number RPC error: {err}"));
    }
    let h = v.get("result").and_then(Value::as_str).ok_or("block number missing result")?;
    u64::from_str_radix(h.trim_start_matches("0x"), 16).map_err(|e| format!("invalid block number: {e}"))
}

fn response_error(resp: &Response) -> String {
    format!("HTTP status {}", resp.status())
}

fn retry_after_ms(resp: &Response) -> Option<u64> {
    resp.headers().get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
}

fn backoff_ms(base_ms: u64, attempt: usize, retry_after: Option<u64>) -> u64 {
    if let Some(ms) = retry_after {
        return ms.clamp(100, 30_000);
    }
    let exponent = attempt.saturating_sub(1).min(6) as u32;
    let raw = base_ms.saturating_mul(1u64 << exponent).min(30_000);
    // Small deterministic jitter to avoid synchronized retries across runs.
    let jitter = ((unix_seconds().wrapping_mul(1103515245).wrapping_add(attempt as u64 * 12345)) % 251) as u64;
    raw.saturating_add(jitter).min(30_000)
}

fn is_retryable_http(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn rpc_block_number(client: &Client, url: &str, retries: usize, base_ms: u64) -> Result<u64, String> {
    for attempt in 0..=retries {
        let response = client.post(url).json(&json!({
            "jsonrpc":"2.0", "id":1, "method":"eth_blockNumber", "params":[]
        })).send();
        match response {
            Ok(resp) if resp.status().is_success() => {
                let v: Value = resp.json().map_err(|e| format!("invalid block-number JSON: {e}"))?;
                return parse_block(&v);
            }
            Ok(resp) => {
                let status = resp.status();
                let retry_after = retry_after_ms(&resp);
                let err = response_error(&resp);
                if attempt == retries || !is_retryable_http(status) {
                    return Err(err);
                }
                let wait = backoff_ms(base_ms, attempt + 1, retry_after);
                eprintln!("RPC block-number {err}; retry {}/{} in {} ms", attempt + 1, retries, wait);
                thread::sleep(Duration::from_millis(wait));
            }
            Err(e) => {
                if attempt == retries {
                    return Err(format!("block-number transport error after retries: {e}"));
                }
                let wait = backoff_ms(base_ms, attempt + 1, None);
                eprintln!("RPC block-number transport error: {e}; retry {}/{} in {} ms", attempt + 1, retries, wait);
                thread::sleep(Duration::from_millis(wait));
            }
        }
    }
    Err("unreachable block-number retry state".into())
}

#[derive(Debug)]
struct CallOutcome {
    result: Option<String>,
    error: Option<String>,
    retry_after_ms: Option<u64>,
}

fn execute_batch_once(
    client: &Client,
    url: &str,
    calls: &[(String, String)],
    block_hex: &str,
) -> Result<Vec<CallOutcome>, String> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let body: Vec<Value> = calls.iter().enumerate().map(|(i, (to, data))| json!({
        "jsonrpc":"2.0",
        "id": i as u64 + 1,
        "method":"eth_call",
        "params":[{"to":to,"data":data}, block_hex]
    })).collect();

    let resp = client.post(url).json(&body).send().map_err(|e| format!("transport error: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let wait = retry_after_ms(&resp);
        let err = response_error(&resp);
        return Err(if let Some(ms) = wait { format!("{err}; retry_after_ms={ms}") } else { err });
    }
    let v: Value = resp.json().map_err(|e| format!("invalid JSON response: {e}"))?;
    let arr = v.as_array().ok_or("provider did not return a JSON-RPC batch array (batch may be unsupported)")?;
    let mut outcomes: Vec<Option<CallOutcome>> = (0..calls.len()).map(|_| None).collect();
    for item in arr {
        let id = item.get("id").and_then(Value::as_u64).ok_or("response item missing numeric id")?;
        if id == 0 || id > calls.len() as u64 {
            return Err(format!("response contained out-of-range JSON-RPC id {id}"));
        }
        let slot = &mut outcomes[(id - 1) as usize];
        if slot.is_some() {
            return Err(format!("response contained duplicate JSON-RPC id {id}"));
        }
        *slot = Some(if let Some(err) = item.get("error") {
            CallOutcome { result: None, error: Some(format!("RPC error: {err}")), retry_after_ms: None }
        } else if let Some(result) = item.get("result").and_then(Value::as_str) {
            CallOutcome { result: Some(result.to_string()), error: None, retry_after_ms: None }
        } else {
            CallOutcome { result: None, error: Some("response item missing result".into()), retry_after_ms: None }
        });
    }
    Ok(outcomes.into_iter().map(|x| x.unwrap_or(CallOutcome {
        result: None, error: Some("missing JSON-RPC batch response item".into()), retry_after_ms: None
    })).collect())
}

fn parse_retry_after_from_error(error: &str) -> Option<u64> {
    error.split("retry_after_ms=").nth(1)?.split_whitespace().next()?.parse().ok()
}

fn fetch_batch_reliably(
    client: &Client,
    url: &str,
    calls: &[(String, String)],
    block_hex: &str,
    max_retries: usize,
    base_ms: u64,
) -> Vec<(Option<String>, usize, Option<String>)> {
    let mut final_outcomes: Vec<(Option<String>, usize, Option<String>)> =
        calls.iter().map(|_| (None, 0, Some("not attempted".into()))).collect();
    let mut pending: Vec<usize> = (0..calls.len()).collect();

    for attempt in 0..=max_retries {
        if pending.is_empty() { break; }
        let pending_calls: Vec<(String, String)> = pending.iter().map(|&i| calls[i].clone()).collect();
        let outcome = execute_batch_once(client, url, &pending_calls, block_hex);
        match outcome {
            Ok(outcomes) => {
                let mut retry_indices = Vec::new();
                for (pending_pos, result) in outcomes.into_iter().enumerate() {
                    let original_idx = pending[pending_pos];
                    final_outcomes[original_idx].1 += 1;
                    if let Some(value) = result.result {
                        let attempts_so_far = final_outcomes[original_idx].1;
                        final_outcomes[original_idx] = (Some(value), attempts_so_far, None);
                    } else {
                        let err = result.error.unwrap_or_else(|| "unknown RPC error".into());
                        final_outcomes[original_idx].2 = Some(err);
                        if attempt < max_retries {
                            retry_indices.push(original_idx);
                        }
                    }
                }
                pending = retry_indices;
                if !pending.is_empty() && attempt < max_retries {
                    let wait = backoff_ms(base_ms, attempt + 1, None);
                    thread::sleep(Duration::from_millis(wait));
                }
            }
            Err(err) => {
                for &idx in &pending {
                    final_outcomes[idx].1 += 1;
                    final_outcomes[idx].2 = Some(err.clone());
                }
                if attempt < max_retries {
                    let wait = backoff_ms(base_ms, attempt + 1, parse_retry_after_from_error(&err));
                    thread::sleep(Duration::from_millis(wait));
                }
            }
        }
    }
    final_outcomes
}

fn append_results(path: &str, block: u64, results: &[PoolResult]) -> Result<(), String> {
    let exists = std::path::Path::new(path).exists();
    let mut f = OpenOptions::new().create(true).append(true).open(path)
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    if !exists {
        writeln!(f, "timestamp,block,dex,pair_address,fee_bps,reserve0_raw,reserve1_raw,attempts,elapsed_ms,status,error")
            .map_err(|e| format!("cannot write CSV header: {e}"))?;
    }
    for r in results {
        let error = r.error.replace('"', "\"\"");
        writeln!(f, "{},{},{},{},{},{},{},{},{:.3},{},\"{}\"",
            unix_seconds(), block, r.dex, r.address, r.fee_bps,
            r.reserve0.as_deref().unwrap_or(""), r.reserve1.as_deref().unwrap_or(""),
            r.attempts, r.elapsed_ms, r.status, error)
            .map_err(|e| format!("cannot append CSV row: {e}"))?;
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let url = env::var("ETH_RPC_URL")
        .map_err(|_| "ETH_RPC_URL is not set in this PowerShell session".to_string())?;
    let registry_path = env::var("ARB_POOL_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.to_string());
    let max_pools = env_usize("ARB_MAX_POOLS", 50, 1, 50);
    let batch_size = env_usize("ARB_BATCH_SIZE", 10, 1, 20);
    let retries = env_usize("ARB_RETRIES", 3, 0, 5);
    let base_ms = env_usize("ARB_RETRY_BASE_MS", 500, 100, 10_000) as u64;
    let pause_ms = env_usize("ARB_BATCH_PAUSE_MS", 250, 0, 5_000) as u64;

    let pools = load_registry(&registry_path, max_pools)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| format!("cannot create HTTP client: {e}"))?;

    println!("Stage 4.9 | RPC reliability engine | read-only");
    println!("Registry: {registry_path} | pools: {} / {max_pools} | batch size: {batch_size}", pools.len());
    println!("Retries: {retries} after initial attempt | base backoff: {base_ms} ms | pause between batches: {pause_ms} ms");
    println!("Policy: sequential batches, retry transient failures, preserve successful individual calls.");
    println!("Safety: eth_blockNumber + eth_call only; no wallet, signing, or transaction submission.");

    let block = rpc_block_number(&client, &url, retries, base_ms)?;
    let block_hex = format!("0x{block:x}");
    println!("Pinned block: {block}");

    let started = Instant::now();
    let mut results: Vec<PoolResult> = pools.iter().map(|p| PoolResult {
        dex: p.dex.clone(), address: p.address.clone(), fee_bps: p.fee_bps,
        reserve0: None, reserve1: None, attempts: 0, elapsed_ms: 0.0,
        status: "PENDING".into(), error: String::new(),
    }).collect();

    let indices: Vec<usize> = (0..pools.len()).collect();
    let batches: Vec<Vec<usize>> = indices.chunks(batch_size).map(|c| c.to_vec()).collect();
    let mut total_attempts = 0usize;
    let mut successes = 0usize;
    let mut failures = 0usize;
    let mut http_batches = 0usize;

    for (batch_no, batch_indices) in batches.iter().enumerate() {
        let calls: Vec<(String, String)> = batch_indices.iter()
            .map(|&i| (pools[i].address.clone(), RESERVES_SELECTOR.to_string())).collect();
        let batch_started = Instant::now();
        http_batches += 1;
        let outcomes = fetch_batch_reliably(&client, &url, &calls, &block_hex, retries, base_ms);
        for (local_idx, (value, attempts, error)) in outcomes.into_iter().enumerate() {
            let pool_idx = batch_indices[local_idx];
            let row = &mut results[pool_idx];
            row.attempts = attempts;
            row.elapsed_ms = batch_started.elapsed().as_secs_f64() * 1000.0;
            total_attempts += attempts;
            match value {
                Some(data) => {
                    // getReserves returns reserve0 and reserve1 as two 32-byte ABI words.
                    let raw = data.trim_start_matches("0x");
                    if raw.len() >= 128 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
                        row.reserve0 = Some(raw[0..64].trim_start_matches('0').to_string());
                        row.reserve1 = Some(raw[64..128].trim_start_matches('0').to_string());
                        if row.reserve0.as_deref() == Some("") { row.reserve0 = Some("0".into()); }
                        if row.reserve1.as_deref() == Some("") { row.reserve1 = Some("0".into()); }
                        row.status = "OK".into();
                        successes += 1;
                    } else {
                        row.status = "INVALID_DATA".into();
                        row.error = "getReserves result was not 128+ hex characters".into();
                        failures += 1;
                    }
                }
                None => {
                    row.status = "FAILED".into();
                    row.error = error.unwrap_or_else(|| "unknown failure".into());
                    failures += 1;
                }
            }
        }
        println!(
            "Batch {}/{} | pools {} | batch elapsed {:.2} ms | cumulative ok {}/{} | failed {}",
            batch_no + 1, batches.len(), batch_indices.len(),
            batch_started.elapsed().as_secs_f64() * 1000.0,
            successes, results.len(), failures
        );
        if pause_ms > 0 && batch_no + 1 < batches.len() {
            thread::sleep(Duration::from_millis(pause_ms));
        }
    }

    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let ok_pct = successes as f64 * 100.0 / results.len() as f64;
    println!("\nSUMMARY | block {block} | pools {} | success {} | failed {} | success rate {:.1}%",
        pools.len(), successes, failures, ok_pct);
    println!("Total elapsed: {:.2} ms | HTTP batch attempts (initial batches): {} | per-call attempts counted: {}",
        elapsed_ms, http_batches, total_attempts);
    println!("Reserve values are raw ABI integers; token order is pair token0/token1 and is not inferred here.");

    append_results("stage4_9_rpc_results.csv", block, &results)?;
    println!("Saved pool results to stage4_9_rpc_results.csv");
    if failures > 0 {
        eprintln!("WARNING: some pools failed after retries. Do not treat this scan as complete.");
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_address_shape() {
        assert!(normalize_address("0xB4e16D0168e52d35CaCD2c6185b44281Ec28C9Dc").is_ok());
        assert!(normalize_address("0x1234").is_err());
    }

    #[test]
    fn backoff_grows_and_caps() {
        let a = backoff_ms(500, 1, None);
        let b = backoff_ms(500, 2, None);
        let c = backoff_ms(500, 3, None);
        assert!(a >= 500 && a < 751);
        assert!(b >= 1000 && b < 1251);
        assert!(c >= 2000 && c < 2251);
        assert_eq!(backoff_ms(500, 1, Some(1200)), 1200);
        assert_eq!(backoff_ms(500, 1, Some(90000)), 30000);
    }

    #[test]
    fn parses_block_number() {
        assert_eq!(parse_block(&json!({"result":"0x10"})).unwrap(), 16);
        assert!(parse_block(&json!({"error":{"code":-1}})).is_err());
    }

    #[test]
    fn rejects_duplicate_registry_entries() {
        let path = std::env::temp_dir().join(format!("stage49_registry_{}.csv", unix_seconds()));
        let content = "dex,pair_address,fee_bps\nA,0xB4e16D0168e52d35CaCD2c6185b44281Ec28C9Dc,30\nB,0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc,30\n";
        fs::write(&path, content).unwrap();
        let result = load_registry(path.to_str().unwrap(), 50);
        let _ = fs::remove_file(path);
        assert!(result.is_err());
    }
}
