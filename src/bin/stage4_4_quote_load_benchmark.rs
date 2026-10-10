//! Stage 4.4 — read-only multi-token scanner with heavier quote-load benchmarks.
//!
//! Registry CSV format: dex,pair_address,fee_bps
//! Only include verified V2-compatible pairs. This program reads token0/token1,
//! token decimals, and reserves from Ethereum at one pinned block. It never signs
//! transactions or submits trades.

use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{collections::{HashMap, HashSet}, env, fs, time::Instant};

const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const SEL_TOKEN0: &str = "0x0dfe1681";
const SEL_TOKEN1: &str = "0xd21220a7";
const SEL_RESERVES: &str = "0x0902f1ac";
const SEL_DECIMALS: &str = "0x313ce567";
const DEFAULT_REGISTRY: &str = "pools_multitoken.csv";

#[derive(Clone, Debug)]
struct PoolConfig { dex: String, address: String, fee_bps: u32 }
#[derive(Clone, Debug)]
struct Pool {
    dex: String, address: String, fee_bps: u32,
    token0: String, token1: String, decimals0: u32, decimals1: u32,
    reserve0: u128, reserve1: u128,
}
#[derive(Clone, Debug)]
struct Edge { pool_idx: usize, from: String, to: String }
#[derive(Clone, Debug)]
struct Route { edges: Vec<Edge> }
#[derive(Clone, Debug)]
struct ResultRow { route: String, start_usdc: f64, final_usdc: f64, gross_usdc: f64, gas_usdc: f64, net_usdc: f64 }

fn env_usize(key: &str, default: usize, min: usize, max: usize) -> usize {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default).clamp(min, max)
}
fn env_f64(key: &str, default: f64, min: f64, max: f64) -> f64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).filter(|v: &f64| v.is_finite()).unwrap_or(default).clamp(min, max)
}
fn normalize_address(s: &str) -> Result<String, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.len() != 42 || !s.starts_with("0x") || !s[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid Ethereum address: {s}"));
    }
    Ok(s)
}
fn parse_registry(path: &str) -> Result<Vec<PoolConfig>, String> {
    let contents = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let mut pools = Vec::new();
    let mut seen = HashSet::new();
    for (line_no, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.to_ascii_lowercase().starts_with("dex,") { continue; }
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() != 3 { return Err(format!("{path}:{} expected dex,pair_address,fee_bps", line_no + 1)); }
        let address = normalize_address(cols[1])?;
        let fee_bps: u32 = cols[2].parse().map_err(|_| format!("{path}:{} invalid fee_bps", line_no + 1))?;
        if fee_bps >= 10_000 { return Err(format!("{path}:{} fee_bps must be below 10000", line_no + 1)); }
        if !seen.insert(address.clone()) { return Err(format!("duplicate pair address {address}")); }
        pools.push(PoolConfig { dex: cols[0].to_string(), address, fee_bps });
        if pools.len() > 250 { return Err("pool registry exceeds hard limit of 250 pools".into()); }
    }
    if pools.is_empty() { return Err(format!("{path} contains no pools")); }
    Ok(pools)
}
fn word_address(data: &str) -> Result<String, String> {
    let h = data.strip_prefix("0x").ok_or("RPC result is not hex")?;
    if h.len() < 64 { return Err("address call returned less than one ABI word".into()); }
    normalize_address(&format!("0x{}", &h[h.len()-40..]))
}
fn word_u128(data: &str, word_index: usize) -> Result<u128, String> {
    let h = data.strip_prefix("0x").ok_or("RPC result is not hex")?;
    let start = word_index.checked_mul(64).ok_or("ABI offset overflow")?;
    if h.len() < start + 64 { return Err("RPC result is shorter than expected ABI data".into()); }
    u128::from_str_radix(&h[start+32..start+64], 16).map_err(|e| format!("invalid ABI integer: {e}"))
}
fn rpc_batch(client: &Client, url: &str, calls: &[(String, String)], block: &str) -> Result<Vec<String>, String> {
    let body: Vec<Value> = calls.iter().enumerate().map(|(i, (to, data))| json!({
        "jsonrpc":"2.0", "id": i as u64 + 1, "method":"eth_call",
        "params":[{"to":to,"data":data}, block]
    })).collect();
    let response = client.post(url).json(&body).send().map_err(|e| format!("RPC request failed: {e}"))?;
    if !response.status().is_success() { return Err(format!("RPC HTTP status {}", response.status())); }
    let value: Value = response.json().map_err(|e| format!("invalid RPC JSON: {e}"))?;
    let arr = value.as_array().ok_or("provider did not accept JSON-RPC batch; expected an array response")?;
    let mut by_id: HashMap<u64, String> = HashMap::new();
    for item in arr {
        let id = item.get("id").and_then(Value::as_u64).ok_or("RPC batch response missing numeric id")?;
        if let Some(err) = item.get("error") { return Err(format!("RPC call id {id} returned error: {err}")); }
        let result = item.get("result").and_then(Value::as_str).ok_or("RPC batch response missing result")?;
        by_id.insert(id, result.to_string());
    }
    (1..=calls.len()).map(|id| by_id.remove(&(id as u64)).ok_or_else(|| format!("RPC batch omitted response id {id}"))).collect()
}
fn batches(client: &Client, url: &str, calls: &[(String, String)], block: &str, batch_size: usize) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(batch_size) { out.extend(rpc_batch(client, url, chunk, block)?); }
    Ok(out)
}
fn block_number(client: &Client, url: &str) -> Result<u64, String> {
    let v: Value = client.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}))
        .send().map_err(|e| format!("block-number request failed: {e}"))?.json().map_err(|e| format!("invalid block-number JSON: {e}"))?;
    if let Some(err) = v.get("error") { return Err(format!("block-number RPC error: {err}")); }
    let h = v.get("result").and_then(Value::as_str).ok_or("block-number response missing result")?;
    u64::from_str_radix(h.trim_start_matches("0x"), 16).map_err(|e| format!("invalid block number: {e}"))
}
fn short(a: &str) -> String { format!("{}…{}", &a[..6], &a[a.len()-4..]) }
fn pow10(decimals: u32) -> Result<u128, String> {
    if decimals > 36 { return Err(format!("unsupported token decimals {decimals} (max 36)")); }
    10u128.checked_pow(decimals).ok_or("decimal scale overflow".into())
}
fn quote(pool: &Pool, from: &str, amount_in: u128) -> Option<u128> {
    let (rin, rout) = if from == pool.token0 { (pool.reserve0, pool.reserve1) }
        else if from == pool.token1 { (pool.reserve1, pool.reserve0) } else { return None; };
    if rin == 0 || rout == 0 { return None; }
    let fee_factor = 10_000u128.checked_sub(pool.fee_bps as u128)?;
    let amount_fee = amount_in.checked_mul(fee_factor)?;
    let numerator = amount_fee.checked_mul(rout)?;
    let denominator = rin.checked_mul(10_000)?.checked_add(amount_fee)?;
    if denominator == 0 { return None; }
    Some(numerator / denominator)
}
fn generate_routes(pools: &[Pool], start: &str, max_hops: usize, max_routes: usize) -> Vec<Route> {
    let mut adjacency: HashMap<String, Vec<Edge>> = HashMap::new();
    for (i, p) in pools.iter().enumerate() {
        adjacency.entry(p.token0.clone()).or_default().push(Edge { pool_idx: i, from: p.token0.clone(), to: p.token1.clone() });
        adjacency.entry(p.token1.clone()).or_default().push(Edge { pool_idx: i, from: p.token1.clone(), to: p.token0.clone() });
    }
    fn walk(current: &str, start: &str, adj: &HashMap<String, Vec<Edge>>, max_hops: usize,
             used_pools: &mut HashSet<usize>, used_tokens: &mut HashSet<String>, path: &mut Vec<Edge>, out: &mut Vec<Route>, cap: usize) {
        if out.len() >= cap { return; }
        if !path.is_empty() && current == start { out.push(Route { edges: path.clone() }); return; }
        if path.len() >= max_hops { return; }
        if let Some(edges) = adj.get(current) {
            for edge in edges {
                if out.len() >= cap { break; }
                if used_pools.contains(&edge.pool_idx) { continue; }
                let closes = edge.to == start;
                if !closes && used_tokens.contains(&edge.to) { continue; }
                if closes && path.len() + 1 < 2 { continue; }
                used_pools.insert(edge.pool_idx);
                if !closes { used_tokens.insert(edge.to.clone()); }
                path.push(edge.clone());
                walk(&edge.to, start, adj, max_hops, used_pools, used_tokens, path, out, cap);
                path.pop();
                used_pools.remove(&edge.pool_idx);
                if !closes { used_tokens.remove(&edge.to); }
            }
        }
    }
    let mut out = Vec::new();
    let mut used_tokens = HashSet::new(); used_tokens.insert(start.to_string());
    walk(start, start, &adjacency, max_hops, &mut HashSet::new(), &mut used_tokens, &mut Vec::new(), &mut out, max_routes);
    out
}
fn evaluate_route(route: &Route, pools: &[Pool], start_raw: u128) -> Option<u128> {
    let mut amount = start_raw;
    for edge in &route.edges { amount = quote(&pools[edge.pool_idx], &edge.from, amount)?; if amount == 0 { return Some(0); } }
    Some(amount)
}
fn route_label(route: &Route, pools: &[Pool]) -> String {
    route.edges.iter().map(|e| format!("{}[{}]:{}→{}", pools[e.pool_idx].dex, short(&pools[e.pool_idx].address), short(&e.from), short(&e.to))).collect::<Vec<_>>().join(" | ")
}
fn main() {
    if let Err(e) = run() { eprintln!("ERROR: {e}"); std::process::exit(1); }
}
fn run() -> Result<(), String> {
    let url = env::var("ETH_RPC_URL").map_err(|_| "ETH_RPC_URL is not set in this terminal's environment".to_string())?;
    let registry_path = env::var("ARB_POOL_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.to_string());
    let batch_size = env_usize("ARB_BATCH_SIZE", 50, 1, 100);
    let max_hops = env_usize("ARB_MAX_HOPS", 3, 2, 3);
    let max_routes = env_usize("ARB_MAX_ROUTES", 20_000, 1, 100_000);
    let steps = env_usize("ARB_OPTIMIZER_STEPS", 100, 10, 500);
    // Re-run quote workloads against the same pinned, real pool states. This increases CPU work
    // without pretending that duplicate/synthetic pools are additional market coverage.
    let bench_repeats = env_usize("ARB_BENCH_REPEATS", 1000, 1, 100_000);
    let bench_eval_cap = env_usize("ARB_BENCH_MAX_EVALS", 5_000_000, 10_000, 20_000_000);
    let min_size = env_f64("ARB_MIN_SIZE_USDC", 50.0, 1.0, 1_000_000.0);
    let max_size = env_f64("ARB_MAX_SIZE_USDC", 20_000.0, min_size, 10_000_000.0);
    let threshold = env_f64("ARB_MIN_PROFIT_USDC", 1.0, 0.0, 1_000_000.0);
    let gas_units = env_f64("ARB_GAS_UNITS", 300_000.0, 21_000.0, 2_000_000.0);
    let gas_gwei = env_f64("ARB_GAS_GWEI", 2.0, 0.0, 1_000.0);
    let client = Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| format!("HTTP client: {e}"))?;
    let configs = parse_registry(&registry_path)?;
    let t0 = Instant::now();
    let block = block_number(&client, &url)?;
    let block_tag = format!("0x{block:x}");
    println!("Stage 4.4 | multi-token scanner + quote-load stress benchmark | Ethereum block {block}");
    println!("Registry: {} | pools configured: {} | max hops: {} | batch size: {}", registry_path, configs.len(), max_hops, batch_size);
    println!("Read-only mode: no wallet, no signing, no transaction submission.");

    // Read token0/token1 from every configured pair at the same pinned block.
    let meta_calls: Vec<(String,String)> = configs.iter().flat_map(|p| [(p.address.clone(), SEL_TOKEN0.to_string()), (p.address.clone(), SEL_TOKEN1.to_string())]).collect();
    let meta_results = batches(&client, &url, &meta_calls, &block_tag, batch_size)?;
    let mut token_set = HashSet::new();
    let mut raw_pools = Vec::new();
    for (i, cfg) in configs.iter().enumerate() {
        let token0 = word_address(&meta_results[i*2])?;
        let token1 = word_address(&meta_results[i*2+1])?;
        if token0 == token1 { return Err(format!("pair {} returned identical token addresses", cfg.address)); }
        token_set.insert(token0.clone()); token_set.insert(token1.clone());
        raw_pools.push((cfg.clone(), token0, token1));
    }
    // Read decimals once per unique token.
    let tokens: Vec<String> = token_set.into_iter().collect();
    let decimal_calls: Vec<(String,String)> = tokens.iter().map(|t| (t.clone(), SEL_DECIMALS.to_string())).collect();
    let decimal_results = batches(&client, &url, &decimal_calls, &block_tag, batch_size)?;
    let mut decimals = HashMap::new();
    for (token, result) in tokens.iter().zip(decimal_results.iter()) {
        let d = word_u128(result, 0)?;
        let d: u32 = d.try_into().map_err(|_| format!("invalid decimals for {token}"))?;
        pow10(d)?;
        decimals.insert(token.clone(), d);
    }
    // Read reserves at the same block. Any malformed/unavailable pair aborts rather than silently mispricing.
    let reserve_calls: Vec<(String,String)> = configs.iter().map(|p| (p.address.clone(), SEL_RESERVES.to_string())).collect();
    let reserve_results = batches(&client, &url, &reserve_calls, &block_tag, batch_size)?;
    let mut pools = Vec::new();
    for (i, (cfg, token0, token1)) in raw_pools.into_iter().enumerate() {
        let reserve0 = word_u128(&reserve_results[i], 0)?;
        let reserve1 = word_u128(&reserve_results[i], 1)?;
        if reserve0 == 0 || reserve1 == 0 { println!("SKIP {} {} — zero reserves", cfg.dex, cfg.address); continue; }
        pools.push(Pool { dex: cfg.dex, address: cfg.address, fee_bps: cfg.fee_bps, decimals0: *decimals.get(&token0).ok_or("missing token0 decimals")?, decimals1: *decimals.get(&token1).ok_or("missing token1 decimals")?, token0, token1, reserve0, reserve1 });
    }
    if pools.is_empty() { return Err("no pools with non-zero reserves".into()); }
    let usdc_scale = pow10(6)?;
    let usdc = normalize_address(USDC)?;
    let weth = normalize_address(WETH)?;
    if !pools.iter().any(|p| (p.token0 == usdc && p.token1 == weth) || (p.token0 == weth && p.token1 == usdc)) {
        return Err("registry must include at least one USDC/WETH pool to estimate gas cost in USDC".into());
    }
    let reference = pools.iter().find(|p| (p.token0 == usdc && p.token1 == weth) || (p.token0 == weth && p.token1 == usdc)).unwrap();
    // Approximate WETH price in USDC from human-unit reserves. This is a conservative model input only, not a gas oracle.
    let (usdc_res, weth_res, usdc_dec, weth_dec) = if reference.token0 == usdc {
        (reference.reserve0 as f64, reference.reserve1 as f64, reference.decimals0, reference.decimals1)
    } else { (reference.reserve1 as f64, reference.reserve0 as f64, reference.decimals1, reference.decimals0) };
    let weth_price_usdc = (usdc_res / pow10(usdc_dec)? as f64) / (weth_res / pow10(weth_dec)? as f64);
    let gas_usdc = gas_units * gas_gwei * 1e-9 * weth_price_usdc;

    let routes = generate_routes(&pools, &usdc, max_hops, max_routes);
    if routes.is_empty() { return Err("no closed USDC routes found. Add compatible verified pools to the registry".into()); }
    println!("Loaded pools: {} | unique tokens: {} | closed USDC routes: {}", pools.len(), decimals.len(), routes.len());

    // Bounded CPU benchmark on the exact pool states fetched above. This does not
    // fabricate pools: each target reports the number actually available in registry.
    println!("\nQUOTE-LOAD STRESS BENCHMARK (same pinned-block pool states; no synthetic pools)");
    println!("target_pools,actual_pools,tokens,routes,requested_repeats,actual_repeats,quote_evaluations,elapsed_ms");
    println!("Requested repeats per benchmark: {} | per-row evaluation cap: {}", bench_repeats, bench_eval_cap);
    for target in [50usize, 100, 250] {
        let actual = pools.len().min(target);
        let subset = &pools[..actual];
        let token_count = subset.iter().flat_map(|p| [p.token0.as_str(), p.token1.as_str()]).collect::<HashSet<_>>().len();
        let bench_routes = generate_routes(subset, &usdc, max_hops, max_routes);
        let base_evals = bench_routes.len().saturating_mul(steps).max(1);
        let actual_repeats = bench_repeats.min((bench_eval_cap / base_evals).max(1));
        let bench_start = Instant::now();
        let mut evaluations = 0usize;
        let mut checksum = 0u128;
        for _repeat in 0..actual_repeats {
            for route in &bench_routes {
                for i in 0..steps {
                    let fraction = if steps <= 1 { 0.0 } else { i as f64 / (steps - 1) as f64 };
                    let amount = min_size + (max_size - min_size) * fraction;
                    let raw_f = amount * usdc_scale as f64;
                    if raw_f.is_finite() && raw_f >= 1.0 && raw_f <= u128::MAX as f64 {
                        if let Some(out) = evaluate_route(route, subset, raw_f as u128) {
                            checksum = checksum.wrapping_add(out);
                        }
                        evaluations += 1;
                    }
                }
            }
        }
        // Keep the computation observable to the optimizer without affecting results.
        std::hint::black_box(checksum);
        println!("{},{},{},{},{},{},{},{:.3}", target, actual, token_count, bench_routes.len(), bench_repeats, actual_repeats, evaluations, bench_start.elapsed().as_secs_f64() * 1000.0);
    }
    println!("Approx gas model: {:.0} gas × {:.2} gwei × WETH ${:.2} = ${:.6} per route", gas_units, gas_gwei, weth_price_usdc, gas_usdc);
    println!("Trade-size range: ${:.2}–${:.2} USDC | optimizer samples per route: {} | minimum net threshold: ${:.2}", min_size, max_size, steps, threshold);

    let mut rows: Vec<ResultRow> = Vec::new();
    let mut passing = 0usize;
    for route in &routes {
        let mut best: Option<ResultRow> = None;
        for i in 0..steps {
            let fraction = if steps == 1 { 0.0 } else { i as f64 / (steps - 1) as f64 };
            let start_usdc = min_size + (max_size - min_size) * fraction;
            let start_raw_f = start_usdc * usdc_scale as f64;
            if !start_raw_f.is_finite() || start_raw_f < 1.0 || start_raw_f > u128::MAX as f64 { continue; }
            let start_raw = start_raw_f as u128;
            let final_raw = match evaluate_route(route, &pools, start_raw) { Some(v) => v, None => continue };
            let final_usdc = final_raw as f64 / usdc_scale as f64;
            let gross = final_usdc - start_usdc;
            let net = gross - gas_usdc;
            let row = ResultRow { route: route_label(route, &pools), start_usdc, final_usdc, gross_usdc: gross, gas_usdc, net_usdc: net };
            if best.as_ref().map(|b| row.net_usdc > b.net_usdc).unwrap_or(true) { best = Some(row); }
        }
        if let Some(row) = best { if row.net_usdc >= threshold { passing += 1; } rows.push(row); }
    }
    rows.sort_by(|a,b| b.net_usdc.total_cmp(&a.net_usdc));
    println!("Routes evaluated: {} | threshold passes: {} | total time: {:.2?}", routes.len() * steps, passing, t0.elapsed());
    println!("\nTOP ROUTES (theoretical; no execution or MEV/slippage guarantees)");
    for (i, r) in rows.iter().take(10).enumerate() {
        println!("{:>2}. net {:+.6} USDC | gross {:+.6} | gas {:.6} | start {:.2} → final {:.6} | {}", i+1, r.net_usdc, r.gross_usdc, r.gas_usdc, r.start_usdc, r.final_usdc, r.route);
    }
    let csv_path = env::var("ARB_RESULTS_CSV").unwrap_or_else(|_| "stage4_4_quote_load_results.csv".to_string());
    let mut csv = String::from("route,start_usdc,final_usdc,gross_usdc,gas_usdc,net_usdc\n");
    for r in &rows { csv.push_str(&format!("\"{}\",{:.8},{:.8},{:.8},{:.8},{:.8}\n", r.route.replace('"', "\"\""), r.start_usdc, r.final_usdc, r.gross_usdc, r.gas_usdc, r.net_usdc)); }
    fs::write(&csv_path, csv).map_err(|e| format!("cannot write CSV {csv_path}: {e}"))?;
    println!("CSV saved: {csv_path}");
    println!("Important: pool fee values and gas settings are configuration inputs. Results exclude private order flow, MEV, failed transactions, stale-state risk, and execution price impact beyond the constant-product quote model.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pool(a: &str, b: &str, r0: u128, r1: u128, fee: u32, dex: &str, address: &str) -> Pool {
        Pool { dex:dex.into(), address:address.into(), fee_bps:fee, token0:a.into(), token1:b.into(), decimals0:6, decimals1:6, reserve0:r0, reserve1:r1 }
    }
    #[test] fn v2_quote_applies_fee_and_reserves() {
        let p = pool("a", "b", 1_000_000, 2_000_000, 30, "test", "pair");
        let out = quote(&p, "a", 10_000).unwrap();
        assert!(out > 0 && out < 20_000);
    }
    #[test] fn quote_respects_direction() {
        let p = pool("a", "b", 1_000_000, 2_000_000, 30, "test", "pair");
        assert!(quote(&p, "b", 10_000).unwrap() < 5_000);
        assert!(quote(&p, "x", 10_000).is_none());
    }
    #[test] fn generates_two_hop_cycle() {
        let p1 = pool("usdc", "weth", 1_000_000, 500_000, 30, "A", "p1");
        let p2 = pool("usdc", "weth", 1_000_000, 510_000, 30, "B", "p2");
        let routes = generate_routes(&[p1,p2], "usdc", 3, 100);
        assert!(routes.iter().any(|r| r.edges.len() == 2));
    }
    #[test] fn generates_three_hop_cycle() {
        let ps = vec![pool("usdc","weth",1000,1000,30,"A","p1"), pool("weth","usdt",1000,1000,30,"B","p2"), pool("usdt","usdc",1000,1000,30,"C","p3")];
        let routes = generate_routes(&ps, "usdc", 3, 100);
        assert!(routes.iter().any(|r| r.edges.len() == 3));
    }
    #[test] fn address_validation_rejects_bad_input() { assert!(normalize_address("0x1234").is_err()); }
    #[test] fn decimal_scale_is_bounded() { assert_eq!(pow10(6).unwrap(), 1_000_000); assert!(pow10(37).is_err()); }
}
