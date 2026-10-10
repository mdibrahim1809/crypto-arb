use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{
    env,
    error::Error,
    fmt,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::Path,
    time::{Duration, Instant},
};

const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";
const DECIMALS_SELECTOR: &str = "0x313ce567";
const MULTICALL3: &str = "0xca11bde05977b3631167028862be2a173976ca11";
const MAX_POOLS: usize = 250;
const BATCH_SIZE_DEFAULT: usize = 50;

#[derive(Debug)]
struct AppError(String);
impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for AppError {}

struct Rpc {
    client: Client,
    url: String,
    next_id: u64,
}
impl Rpc {
    fn new(url: String) -> Result<Self, Box<dyn Error>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { client, url, next_id: 1 })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let response: Value = self.client.post(&self.url).json(&body).send()?.error_for_status()?.json()?;
        if let Some(err) = response.get("error") {
            return Err(Box::new(AppError(format!("RPC method {method} failed: {err}"))));
        }
        response.get("result").cloned().ok_or_else(|| Box::new(AppError(format!("RPC response missing result for {method}"))) as Box<dyn Error>)
    }

    fn batch_calls(&mut self, calls: &[(String, String)], block_tag: &str) -> Result<Vec<Result<String, String>>, Box<dyn Error>> {
        // JSON-RPC batch: each item is a normal eth_call. Response order is not guaranteed,
        // so results are correlated by request ID.
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let mut request_body = Vec::with_capacity(calls.len());
        let mut id_to_index = std::collections::HashMap::with_capacity(calls.len());
        for (index, (to, data)) in calls.iter().enumerate() {
            let id = self.next_id;
            self.next_id += 1;
            id_to_index.insert(id, index);
            request_body.push(json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":"eth_call",
                "params":[{"to":to,"data":data},block_tag]
            }));
        }
        let response = self.client.post(&self.url).json(&request_body).send()?.error_for_status()?;
        let values: Value = response.json()?;
        let rows = values.as_array().ok_or_else(|| AppError("JSON-RPC batch response was not an array; provider may not support batches".into()))?;
        let mut output: Vec<Option<Result<String, String>>> = vec![None; calls.len()];
        for row in rows {
            let Some(id) = row.get("id").and_then(Value::as_u64) else { continue };
            let Some(index) = id_to_index.get(&id).copied() else { continue };
            if let Some(err) = row.get("error") {
                output[index] = Some(Err(err.to_string()));
            } else if let Some(result) = row.get("result").and_then(Value::as_str) {
                output[index] = Some(Ok(result.to_string()));
            } else {
                output[index] = Some(Err("response missing result".into()));
            }
        }
        Ok(output.into_iter().map(|x| x.unwrap_or_else(|| Err("missing response item for request ID".into()))).collect())
    }

    fn chain_id(&mut self) -> Result<u64, Box<dyn Error>> {
        let s = self.request("eth_chainId", json!([]))?;
        Ok(u64::from_str_radix(s.as_str().ok_or_else(|| AppError("Invalid chain ID response".into()))?.trim_start_matches("0x"), 16)?)
    }
    fn block_number(&mut self) -> Result<u64, Box<dyn Error>> {
        let s = self.request("eth_blockNumber", json!([]))?;
        Ok(u64::from_str_radix(s.as_str().ok_or_else(|| AppError("Invalid block number response".into()))?.trim_start_matches("0x"), 16)?)
    }
    fn gas_price_wei(&mut self) -> Result<u128, Box<dyn Error>> {
        let s = self.request("eth_gasPrice", json!([]))?;
        Ok(u128::from_str_radix(s.as_str().ok_or_else(|| AppError("Invalid gas price response".into()))?.trim_start_matches("0x"), 16)?)
    }
}

#[derive(Clone, Debug)]
struct PoolConfig {
    dex: String,
    address: String,
    fee_bps: u32,
}
#[derive(Clone, Debug)]
struct Pool {
    dex: String,
    address: String,
    fee_bps: u32,
    usdc_reserve: u128,
    weth_reserve: u128,
}
#[derive(Clone, Debug)]
struct Evaluation {
    route: String,
    start_usdc: u128,
    intermediate_weth: u128,
    final_usdc: u128,
    gross_usdc: i128,
    gas_usdc: f64,
    net_usdc: f64,
}

fn decode_word(data: &str, index: usize) -> Result<String, Box<dyn Error>> {
    let raw = data.strip_prefix("0x").unwrap_or(data);
    let start = index.checked_mul(64).ok_or_else(|| AppError("ABI index overflow".into()))?;
    let end = start + 64;
    if raw.len() < end {
        return Err(Box::new(AppError(format!("ABI response too short for word {index}"))));
    }
    Ok(raw[start..end].to_ascii_lowercase())
}
fn word_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> {
    Ok(u128::from_str_radix(&decode_word(data, index)?, 16)?)
}
fn word_address(data: &str, index: usize) -> Result<String, Box<dyn Error>> {
    Ok(format!("0x{}", &decode_word(data, index)?[24..]))
}
fn valid_address(address: &str) -> bool {
    let a = address.strip_prefix("0x").unwrap_or("");
    a.len() == 40 && a.chars().all(|c| c.is_ascii_hexdigit())
}
fn normalized(a: &str) -> String { a.to_ascii_lowercase() }

fn read_registry(path: &str) -> Result<Vec<PoolConfig>, Box<dyn Error>> {
    let file = File::open(path).map_err(|e| AppError(format!("Cannot open '{path}': {e}. Copy pools.example.csv to pools.csv or set ARB_POOL_REGISTRY.")))?;
    let mut rows = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.to_ascii_lowercase().starts_with("dex,") { continue; }
        let cols: Vec<&str> = trimmed.split(',').map(str::trim).collect();
        if cols.len() != 3 { return Err(Box::new(AppError(format!("Registry line {} must be dex,pair_address,fee_bps", idx + 1)))); }
        if cols[0].is_empty() || !valid_address(cols[1]) { return Err(Box::new(AppError(format!("Invalid DEX name or address on line {}", idx + 1)))); }
        let fee: u32 = cols[2].parse().map_err(|_| AppError(format!("Invalid fee_bps on line {}", idx + 1)))?;
        if fee >= 1000 { return Err(Box::new(AppError(format!("Fee must be below 1000 bps on line {}", idx + 1)))); }
        rows.push(PoolConfig { dex: cols[0].to_string(), address: normalized(cols[1]), fee_bps: fee });
        if rows.len() > MAX_POOLS { return Err(Box::new(AppError(format!("Registry exceeds safe limit of {MAX_POOLS} pools")))); }
    }
    if rows.len() < 2 { return Err(Box::new(AppError("At least two pools are required".into()))); }
    let mut seen = std::collections::HashSet::new();
    for p in &rows {
        if !seen.insert(p.address.clone()) { return Err(Box::new(AppError(format!("Duplicate pool address: {}", p.address)))); }
    }
    Ok(rows)
}

fn make_call(to: &str, data: &str) -> (String, String) { (to.to_string(), data.to_string()) }

fn validate_configs_batched(rpc: &mut Rpc, configs: &[PoolConfig], block_tag: &str, batch_size: usize) -> Result<Vec<PoolConfig>, Box<dyn Error>> {
    let mut valid = Vec::new();
    for chunk in configs.chunks(batch_size) {
        let mut calls = Vec::with_capacity(chunk.len() * 4);
        for c in chunk {
            calls.push(make_call(&c.address, TOKEN0_SELECTOR));
            calls.push(make_call(&c.address, TOKEN1_SELECTOR));
            calls.push(make_call(USDC, DECIMALS_SELECTOR));
            calls.push(make_call(WETH, DECIMALS_SELECTOR));
        }
        let replies = rpc.batch_calls(&calls, block_tag)?;
        for (i, c) in chunk.iter().enumerate() {
            let base = i * 4;
            let parsed = (|| -> Result<(), Box<dyn Error>> {
                let t0 = normalized(&word_address(replies[base].as_ref().map_err(|e| AppError(e.clone()))?, 0)?);
                let t1 = normalized(&word_address(replies[base + 1].as_ref().map_err(|e| AppError(e.clone()))?, 0)?);
                let usdc_dec = word_u128(replies[base + 2].as_ref().map_err(|e| AppError(e.clone()))?, 0)?;
                let weth_dec = word_u128(replies[base + 3].as_ref().map_err(|e| AppError(e.clone()))?, 0)?;
                let usdc = normalized(USDC); let weth = normalized(WETH);
                if !((t0 == usdc && t1 == weth) || (t0 == weth && t1 == usdc)) {
                    return Err(Box::new(AppError(format!("not a USDC/WETH pair: {t0}, {t1}"))));
                }
                if usdc_dec != 6 || weth_dec != 18 {
                    return Err(Box::new(AppError("unexpected token decimals".into())));
                }
                Ok(())
            })();
            match parsed {
                Ok(()) => { println!("  OK {:<14} {}", c.dex, c.address); valid.push(c.clone()); }
                Err(e) => eprintln!("  SKIP {} {}: {}", c.dex, c.address, e),
            }
        }
    }
    Ok(valid)
}

fn read_states_batched(rpc: &mut Rpc, configs: &[PoolConfig], block_tag: &str, batch_size: usize) -> Result<Vec<Pool>, Box<dyn Error>> {
    let mut pools = Vec::with_capacity(configs.len());
    for chunk in configs.chunks(batch_size) {
        // Batch reserves and token0 together so each pool needs no follow-up RPC request.
        let mut calls = Vec::with_capacity(chunk.len() * 2);
        for c in chunk {
            calls.push(make_call(&c.address, RESERVES_SELECTOR));
            calls.push(make_call(&c.address, TOKEN0_SELECTOR));
        }
        let replies = rpc.batch_calls(&calls, block_tag)?;
        for (i, c) in chunk.iter().enumerate() {
            let parsed = (|| -> Result<Pool, Box<dyn Error>> {
                let reserves = replies[i * 2].as_ref().map_err(|e| AppError(e.clone()))?;
                let token0_data = replies[i * 2 + 1].as_ref().map_err(|e| AppError(e.clone()))?;
                let r0 = word_u128(reserves, 0)?;
                let r1 = word_u128(reserves, 1)?;
                let token0 = normalized(&word_address(token0_data, 0)?);
                let (usdc_reserve, weth_reserve) = if token0 == normalized(USDC) { (r0, r1) } else { (r1, r0) };
                if usdc_reserve == 0 || weth_reserve == 0 { return Err(Box::new(AppError("zero reserves".into()))); }
                Ok(Pool { dex: c.dex.clone(), address: c.address.clone(), fee_bps: c.fee_bps, usdc_reserve, weth_reserve })
            })();
            match parsed { Ok(p) => pools.push(p), Err(e) => eprintln!("  Pool read failed {} {}: {}", c.dex, c.address, e) }
        }
    }
    Ok(pools)
}

fn amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128, fee_bps: u32) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 || fee_bps >= 1000 { return None; }
    let scale = 10_000u128;
    let adjusted = amount_in.checked_mul(scale.checked_sub(fee_bps as u128)?)?;
    let numerator = adjusted.checked_mul(reserve_out)?;
    let denominator = reserve_in.checked_mul(scale)?.checked_add(adjusted)?;
    Some(numerator / denominator)
}
fn raw_usdc(amount: f64) -> Result<u128, Box<dyn Error>> {
    if !amount.is_finite() || amount <= 0.0 || amount * 1e6 >= u128::MAX as f64 {
        return Err(Box::new(AppError(format!("Invalid USDC amount: {amount}"))));
    }
    Ok((amount * 1e6).round() as u128)
}
fn fmt_usdc(v: u128) -> String { format!("{}.{:06}", v / 1_000_000, v % 1_000_000) }
fn fmt_weth(v: u128) -> String {
    format!("{}.{:018}", v / 1_000_000_000_000_000_000, v % 1_000_000_000_000_000_000)
}
fn evaluate(a: &Pool, b: &Pool, start: u128, gas_usdc: f64) -> Option<Evaluation> {
    if a.address == b.address { return None; }
    let weth = amount_out(start, a.usdc_reserve, a.weth_reserve, a.fee_bps)?;
    let final_usdc = amount_out(weth, b.weth_reserve, b.usdc_reserve, b.fee_bps)?;
    let gross = final_usdc as i128 - start as i128;
    Some(Evaluation { route: format!("{} -> {}", a.dex, b.dex), start_usdc: start, intermediate_weth: weth, final_usdc, gross_usdc: gross, gas_usdc, net_usdc: gross as f64 / 1e6 - gas_usdc })
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================\n CRYPTO ARBITRAGE ENGINE — STAGE 4.1\n BATCHED MARKET-DATA READER (READ-ONLY)\n==================================================");
    let registry_path = env::var("ARB_POOL_REGISTRY").unwrap_or_else(|_| "pools.csv".into());
    if !Path::new(&registry_path).exists() && Path::new("pools.example.csv").exists() {
        return Err(Box::new(AppError(format!("Registry '{registry_path}' missing; copy pools.example.csv to pools.csv."))));
    }
    let configs = read_registry(&registry_path)?;
    let rpc_url = env::var("ETH_RPC_URL").map_err(|_| AppError("ETH_RPC_URL environment variable is missing".into()))?;
    let batch_size: usize = env::var("ARB_BATCH_SIZE").unwrap_or_else(|_| BATCH_SIZE_DEFAULT.to_string()).parse().map_err(|_| AppError("ARB_BATCH_SIZE must be an integer".into()))?;
    let min_profit: f64 = env::var("ARB_MIN_PROFIT_USDC").unwrap_or_else(|_| "1.00".into()).parse().map_err(|_| AppError("ARB_MIN_PROFIT_USDC must be numeric".into()))?;
    let gas_units: u128 = env::var("ARB_GAS_UNITS").unwrap_or_else(|_| "300000".into()).parse().map_err(|_| AppError("ARB_GAS_UNITS must be an integer".into()))?;
    let min_size: f64 = env::var("ARB_MIN_SIZE_USDC").unwrap_or_else(|_| "50".into()).parse().map_err(|_| AppError("ARB_MIN_SIZE_USDC must be numeric".into()))?;
    let max_size: f64 = env::var("ARB_MAX_SIZE_USDC").unwrap_or_else(|_| "20000".into()).parse().map_err(|_| AppError("ARB_MAX_SIZE_USDC must be numeric".into()))?;
    let steps: usize = env::var("ARB_OPTIMIZER_STEPS").unwrap_or_else(|_| "250".into()).parse().map_err(|_| AppError("ARB_OPTIMIZER_STEPS must be an integer".into()))?;
    let poll_secs: u64 = env::var("ARB_POLL_SECS").unwrap_or_else(|_| "5".into()).parse().map_err(|_| AppError("ARB_POLL_SECS must be an integer".into()))?;
    let csv_path = env::var("ARB_BATCH_CSV").unwrap_or_else(|_| "stage4_1_batch_results.csv".into());
    if !(1..=100).contains(&batch_size) || !(2..=1000).contains(&steps) || !(2..=300).contains(&poll_secs)
        || !min_profit.is_finite() || min_profit < 0.0 || gas_units == 0
        || !min_size.is_finite() || !max_size.is_finite() || min_size <= 0.0 || max_size < min_size {
        return Err(Box::new(AppError("Invalid configuration. Require batch size 1..100, steps 2..1000, poll 2..300 seconds, nonnegative threshold, positive gas units, and 0 < min size <= max size.".into())));
    }

    let mut rpc = Rpc::new(rpc_url)?;
    let chain = rpc.chain_id()?;
    if chain != 1 { return Err(Box::new(AppError(format!("Expected Ethereum Mainnet chain ID 1, got {chain}")))); }
    let block = rpc.block_number()?;
    let tag = format!("0x{block:x}");
    println!("Ethereum Mainnet | block {block} | configured pools {} | JSON-RPC batch size {batch_size}", configs.len());
    println!("Validating configured pool metadata in batches...");
    let valid_configs = validate_configs_batched(&mut rpc, &configs, &tag, batch_size)?;
    if valid_configs.len() < 2 { return Err(Box::new(AppError("Fewer than two valid pools after validation.".into()))); }

    let mut csv = File::create(&csv_path)?;
    writeln!(csv, "block,source_dex,destination_dex,start_usdc,intermediate_weth,final_usdc,gross_profit_usdc,estimated_gas_usdc,estimated_net_profit_usdc,passes_threshold")?;
    let start_time = Instant::now();
    let block_tag = format!("0x{block:x}");
    println!("Fetching reserve states with batched eth_call requests...");
    let pools = read_states_batched(&mut rpc, &valid_configs, &block_tag, batch_size)?;
    if pools.len() < 2 { return Err(Box::new(AppError("Fewer than two pool states loaded.".into()))); }
    let gas_price = rpc.gas_price_wei()?;
    let ref_pool = &pools[0];
    let price = (ref_pool.usdc_reserve as f64 / 1e6) / (ref_pool.weth_reserve as f64 / 1e18);
    let gas_usdc = gas_price as f64 * gas_units as f64 / 1e18 * price;
    let mut best: Option<Evaluation> = None;
    let mut evals = 0usize;
    let mut passing = 0usize;
    for i in 0..steps {
        let size = min_size + (max_size - min_size) * i as f64 / (steps - 1) as f64;
        let start = raw_usdc(size)?;
        for a in &pools { for b in &pools {
            if a.address == b.address { continue; }
            if let Some(ev) = evaluate(a, b, start, gas_usdc) {
                evals += 1;
                let pass = ev.net_usdc >= min_profit;
                if pass { passing += 1; }
                writeln!(csv, "{},{},{},{},{},{},{:.6},{:.6},{:.6},{}", block, a.dex, b.dex, fmt_usdc(start), fmt_weth(ev.intermediate_weth), fmt_usdc(ev.final_usdc), ev.gross_usdc as f64 / 1e6, gas_usdc, ev.net_usdc, pass)?;
                if best.as_ref().map(|x| ev.net_usdc > x.net_usdc).unwrap_or(true) { best = Some(ev); }
            }
        }}
    }
    csv.flush()?;
    println!("\n================ STAGE 4.1 SUMMARY ================");
    println!("Valid pools: {} / {} | batch size: {}", pools.len(), configs.len(), batch_size);
    println!("Routes evaluated: {evals} | sample sizes: {steps} | threshold passes: {passing}");
    println!("Pool-state + route processing time: {:.2?}", start_time.elapsed());
    if let Some(b) = best {
        println!("Best theoretical route: {} | start {} USDC | final {} USDC | estimated net P/L {:+.6} USDC", b.route, fmt_usdc(b.start_usdc), fmt_usdc(b.final_usdc), b.net_usdc);
    }
    println!("CSV saved: {csv_path}");
    println!("READ-ONLY. Batch support depends on RPC provider behavior; no signing or transaction submission.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn amount_out_reference() { assert_eq!(amount_out(1_000, 10_000, 20_000, 30), Some(1_813)); }
    #[test] fn lower_fee_gives_more_output() { assert!(amount_out(1_000,10_000,20_000,5).unwrap() > amount_out(1_000,10_000,20_000,30).unwrap()); }
    #[test] fn rejects_bad_amounts() { assert_eq!(amount_out(0,10,10,30),None); assert_eq!(amount_out(1,0,10,30),None); }
    #[test] fn rejects_bad_fees() { assert_eq!(amount_out(1,10,10,1000),None); }
    #[test] fn address_validation() { assert!(valid_address("0x397ff1542f962076d0bfe58ea045ffa2d347aca0")); assert!(!valid_address("0x1234")); }
    #[test] fn token_formatting() { assert_eq!(fmt_usdc(1_234_567),"1.234567"); assert_eq!(fmt_weth(1_000_000_000_000_000_000),"1.000000000000000000"); }
}
