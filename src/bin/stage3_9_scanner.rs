use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{env, error::Error, fmt, fs::File, io::{BufRead, BufReader, Write}, path::Path};

const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";
const DECIMALS_SELECTOR: &str = "0x313ce567";
const MAX_POOLS: usize = 50;

#[derive(Debug)] struct AppError(String);
impl fmt::Display for AppError { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) } }
impl Error for AppError {}

struct Rpc { client: Client, url: String, next_id: u64 }
impl Rpc {
    fn new(url: String) -> Self { Self { client: Client::new(), url, next_id: 1 } }
    fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id; self.next_id += 1;
        let response: Value = self.client.post(&self.url).json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).send()?.error_for_status()?.json()?;
        if let Some(err) = response.get("error") { return Err(Box::new(AppError(format!("RPC method {method} failed: {err}")))); }
        response.get("result").cloned().ok_or_else(|| Box::new(AppError(format!("RPC response missing result for {method}"))) as Box<dyn Error>)
    }
    fn string_result(&mut self, method: &str, params: Value) -> Result<String, Box<dyn Error>> {
        self.request(method, params)?.as_str().map(str::to_owned).ok_or_else(|| Box::new(AppError(format!("{method} returned non-string result"))) as Box<dyn Error>)
    }
    fn chain_id(&mut self) -> Result<u64, Box<dyn Error>> { Ok(u64::from_str_radix(self.string_result("eth_chainId", json!([]))?.trim_start_matches("0x"), 16)?) }
    fn block_number(&mut self) -> Result<u64, Box<dyn Error>> { Ok(u64::from_str_radix(self.string_result("eth_blockNumber", json!([]))?.trim_start_matches("0x"), 16)?) }
    fn gas_price_wei(&mut self) -> Result<u128, Box<dyn Error>> { Ok(u128::from_str_radix(self.string_result("eth_gasPrice", json!([]))?.trim_start_matches("0x"), 16)?) }
    fn call(&mut self, to: &str, data: &str, block_tag: &str) -> Result<String, Box<dyn Error>> { self.string_result("eth_call", json!([{"to":to,"data":data},block_tag])) }
}

#[derive(Clone, Debug)] struct PoolConfig { dex: String, address: String, fee_bps: u32 }
#[derive(Clone, Debug)] struct Pool { dex: String, address: String, fee_bps: u32, usdc_reserve: u128, weth_reserve: u128 }
#[derive(Clone, Debug)] struct Evaluation { route: String, start_usdc: u128, intermediate_weth: u128, final_usdc: u128, gross_usdc: i128, gas_usdc: f64, net_usdc: f64 }

fn decode_word(data: &str, index: usize) -> Result<String, Box<dyn Error>> {
    let raw = data.strip_prefix("0x").unwrap_or(data); let start = index.checked_mul(64).ok_or_else(|| AppError("ABI index overflow".into()))?; let end = start + 64;
    if raw.len() < end { return Err(Box::new(AppError(format!("ABI response too short for word {index}")))); }
    Ok(raw[start..end].to_ascii_lowercase())
}
fn word_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> { Ok(u128::from_str_radix(&decode_word(data, index)?, 16)?) }
fn word_address(data: &str, index: usize) -> Result<String, Box<dyn Error>> { Ok(format!("0x{}", &decode_word(data, index)?[24..])) }
fn valid_address(address: &str) -> bool { let a = address.strip_prefix("0x").unwrap_or(""); a.len() == 40 && a.chars().all(|c| c.is_ascii_hexdigit()) }
fn normalized(a: &str) -> String { a.to_ascii_lowercase() }
fn read_registry(path: &str) -> Result<Vec<PoolConfig>, Box<dyn Error>> {
    let file = File::open(path).map_err(|e| AppError(format!("Cannot open pool registry '{path}': {e}. Copy pools.example.csv to pools.csv or set ARB_POOL_REGISTRY.")))?;
    let mut rows = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?; let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.to_ascii_lowercase().starts_with("dex,") { continue; }
        let cols: Vec<&str> = trimmed.split(',').map(str::trim).collect();
        if cols.len() != 3 { return Err(Box::new(AppError(format!("Registry line {} must have 3 columns: dex,pair_address,fee_bps", idx + 1)))); }
        if !valid_address(cols[1]) { return Err(Box::new(AppError(format!("Invalid pair address on registry line {}", idx + 1)))); }
        let fee: u32 = cols[2].parse().map_err(|_| AppError(format!("Invalid fee_bps on registry line {}", idx + 1)))?;
        if fee >= 1000 { return Err(Box::new(AppError(format!("fee_bps must be below 1000 on line {}", idx + 1)))); }
        rows.push(PoolConfig { dex: cols[0].to_string(), address: normalized(cols[1]), fee_bps: fee });
        if rows.len() > MAX_POOLS { return Err(Box::new(AppError(format!("Registry exceeds safe limit of {MAX_POOLS} pools")))); }
    }
    if rows.len() < 2 { return Err(Box::new(AppError("Pool registry must contain at least two pools for cross-pool comparisons".into()))); }
    let mut seen = std::collections::HashSet::new();
    for p in &rows { if !seen.insert(p.address.clone()) { return Err(Box::new(AppError(format!("Duplicate pair address in registry: {}", p.address)))); } }
    Ok(rows)
}
fn read_pool(rpc: &mut Rpc, config: &PoolConfig, block_tag: &str) -> Result<Pool, Box<dyn Error>> {
    let token0 = normalized(&word_address(&rpc.call(&config.address, TOKEN0_SELECTOR, block_tag)?, 0)?);
    let token1 = normalized(&word_address(&rpc.call(&config.address, TOKEN1_SELECTOR, block_tag)?, 0)?);
    let usdc = normalized(USDC); let weth = normalized(WETH);
    if !((token0 == usdc && token1 == weth) || (token0 == weth && token1 == usdc)) {
        return Err(Box::new(AppError(format!("Pool {} ({}) is not the configured USDC/WETH pair; tokens are {token0}, {token1}", config.dex, config.address))));
    }
    let usdc_dec = word_u128(&rpc.call(USDC, DECIMALS_SELECTOR, block_tag)?, 0)?;
    let weth_dec = word_u128(&rpc.call(WETH, DECIMALS_SELECTOR, block_tag)?, 0)?;
    if usdc_dec != 6 || weth_dec != 18 { return Err(Box::new(AppError("Unexpected USDC/WETH decimals; expected 6 and 18".into()))); }
    let reserves = rpc.call(&config.address, RESERVES_SELECTOR, block_tag)?;
    let r0 = word_u128(&reserves, 0)?; let r1 = word_u128(&reserves, 1)?;
    let (ur, wr) = if token0 == usdc { (r0, r1) } else { (r1, r0) };
    if ur == 0 || wr == 0 { return Err(Box::new(AppError(format!("Pool {} has zero reserves", config.address)))); }
    Ok(Pool { dex: config.dex.clone(), address: config.address.clone(), fee_bps: config.fee_bps, usdc_reserve: ur, weth_reserve: wr })
}
fn amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128, fee_bps: u32) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 || fee_bps >= 1000 { return None; }
    let scale = 10_000u128; let fee_multiplier = scale.checked_sub(fee_bps as u128)?;
    let adjusted = amount_in.checked_mul(fee_multiplier)?;
    let numerator = adjusted.checked_mul(reserve_out)?;
    let denominator = reserve_in.checked_mul(scale)?.checked_add(adjusted)?;
    Some(numerator / denominator)
}
fn raw_usdc(amount: f64) -> Result<u128, Box<dyn Error>> {
    if !amount.is_finite() || amount <= 0.0 || amount * 1e6 >= u128::MAX as f64 { return Err(Box::new(AppError(format!("Invalid USDC amount: {amount}")))); }
    Ok((amount * 1e6).round() as u128)
}
fn fmt_usdc(v: u128) -> String { format!("{}.{:06}", v / 1_000_000, v % 1_000_000) }
fn fmt_weth(v: u128) -> String { format!("{}.{:018}", v / 1_000_000_000_000_000_000, v % 1_000_000_000_000_000_000) }
fn evaluate(a: &Pool, b: &Pool, start: u128, gas_usdc: f64) -> Option<Evaluation> {
    if a.address == b.address { return None; }
    let weth = amount_out(start, a.usdc_reserve, a.weth_reserve, a.fee_bps)?;
    let final_usdc = amount_out(weth, b.weth_reserve, b.usdc_reserve, b.fee_bps)?;
    let gross = final_usdc as i128 - start as i128;
    Some(Evaluation { route: format!("{} -> {}", a.dex, b.dex), start_usdc: start, intermediate_weth: weth, final_usdc, gross_usdc: gross, gas_usdc, net_usdc: gross as f64 / 1e6 - gas_usdc })
}
fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================\n CRYPTO ARBITRAGE ENGINE — STAGE 3.9\n CONFIGURABLE MULTI-POOL SCANNER (READ-ONLY)\n==================================================");
    let registry_path = env::var("ARB_POOL_REGISTRY").unwrap_or_else(|_| "pools.csv".into());
    if !Path::new(&registry_path).exists() && Path::new("pools.example.csv").exists() { return Err(Box::new(AppError(format!("Registry '{registry_path}' not found. Copy pools.example.csv to pools.csv, then edit only with verified USDC/WETH pool addresses.")))); }
    let configs = read_registry(&registry_path)?;
    let rpc_url = env::var("ETH_RPC_URL").map_err(|_| AppError("ETH_RPC_URL environment variable is missing".into()))?;
    let min_profit: f64 = env::var("ARB_MIN_PROFIT_USDC").unwrap_or_else(|_| "1.00".into()).parse().map_err(|_| AppError("ARB_MIN_PROFIT_USDC must be numeric".into()))?;
    let gas_units: u128 = env::var("ARB_GAS_UNITS").unwrap_or_else(|_| "300000".into()).parse().map_err(|_| AppError("ARB_GAS_UNITS must be an integer".into()))?;
    let min_size: f64 = env::var("ARB_MIN_SIZE_USDC").unwrap_or_else(|_| "50".into()).parse().map_err(|_| AppError("ARB_MIN_SIZE_USDC must be numeric".into()))?;
    let max_size: f64 = env::var("ARB_MAX_SIZE_USDC").unwrap_or_else(|_| "20000".into()).parse().map_err(|_| AppError("ARB_MAX_SIZE_USDC must be numeric".into()))?;
    let steps: usize = env::var("ARB_OPTIMIZER_STEPS").unwrap_or_else(|_| "250".into()).parse().map_err(|_| AppError("ARB_OPTIMIZER_STEPS must be an integer".into()))?;
    if !min_profit.is_finite() || min_profit < 0.0 || gas_units == 0 || !min_size.is_finite() || !max_size.is_finite() || min_size <= 0.0 || max_size < min_size || !(2..=10000).contains(&steps) { return Err(Box::new(AppError("Invalid configuration: threshold >= 0, gas units > 0, 0 < min size <= max size, and 2 <= steps <= 10000 required".into()))); }
    let mut rpc = Rpc::new(rpc_url);
    let chain = rpc.chain_id()?; if chain != 1 { return Err(Box::new(AppError(format!("Expected Ethereum Mainnet chain ID 1, got {chain}")))); }
    let block = rpc.block_number()?; let tag = format!("0x{block:x}"); let gas_price = rpc.gas_price_wei()?;
    println!("Loading {} configured pools at block {block}...", configs.len());
    let mut pools = Vec::with_capacity(configs.len());
    for config in &configs { match read_pool(&mut rpc, config, &tag) { Ok(p) => { println!("  OK {:<14} {} fee={} bps", p.dex, p.address, p.fee_bps); pools.push(p); }, Err(e) => eprintln!("  SKIP {} {}: {}", config.dex, config.address, e) } }
    if pools.len() < 2 { return Err(Box::new(AppError("Fewer than two valid configured pools remain. Check pools.csv addresses and fee assumptions.".into()))); }
    let uni_price = (pools[0].usdc_reserve as f64 / 1e6) / (pools[0].weth_reserve as f64 / 1e18);
    let gas_eth = gas_price as f64 * gas_units as f64 / 1e18; let gas_usdc = gas_eth * uni_price;
    let csv_path = env::var("ARB_SCANNER_CSV").unwrap_or_else(|_| "stage3_9_opportunities.csv".into());
    let mut csv = File::create(&csv_path)?;
    writeln!(csv, "block,source_dex,source_pool,source_fee_bps,destination_dex,destination_pool,destination_fee_bps,start_usdc,intermediate_weth,final_usdc,gross_profit_usdc,estimated_gas_usdc,estimated_net_profit_usdc,passes_threshold")?;
    let mut best: Option<Evaluation> = None; let mut evaluations = 0usize; let mut passing = 0usize;
    for i in 0..steps {
        let size = min_size + (max_size - min_size) * i as f64 / (steps - 1) as f64; let start = raw_usdc(size)?;
        for a in &pools { for b in &pools { if a.address == b.address { continue; }
            if let Some(ev) = evaluate(a, b, start, gas_usdc) {
                evaluations += 1; let pass = ev.net_usdc >= min_profit; if pass { passing += 1; }
                writeln!(csv, "{},{},{},{},{},{},{},{},{},{},{:.6},{:.6},{:.6},{}", block, a.dex, a.address, a.fee_bps, b.dex, b.address, b.fee_bps, fmt_usdc(ev.start_usdc), fmt_weth(ev.intermediate_weth), fmt_usdc(ev.final_usdc), ev.gross_usdc as f64 / 1e6, ev.gas_usdc, ev.net_usdc, pass)?;
                if best.as_ref().map(|x| ev.net_usdc > x.net_usdc).unwrap_or(true) { best = Some(ev); }
            }
        }}
    }
    csv.flush()?;
    println!("\n================ STAGE 3.9 SUMMARY ================");
    println!("Block: {block} | valid pools: {} / {}", pools.len(), configs.len());
    println!("Ordered pool routes: {} | size samples: {steps} | evaluations: {evaluations}", pools.len() * (pools.len() - 1));
    println!("Gas price: {:.3} gwei | assumed gas: {gas_units} | estimated gas: {:.6} USDC/route", gas_price as f64 / 1e9, gas_usdc);
    println!("Search range: ${:.2}–${:.2} | minimum net-profit threshold: ${:.2}", min_size, max_size, min_profit);
    println!("Candidates meeting threshold: {passing}");
    if let Some(b) = best { println!("\nBEST THEORETICAL CANDIDATE\n  Route: {}\n  Start: {} USDC\n  Intermediate WETH: {}\n  Final: {} USDC\n  Gross P/L: {:+.6} USDC\n  Estimated gas: {:.6} USDC\n  Estimated net P/L: {:+.6} USDC\n  Passes threshold: {}", b.route, fmt_usdc(b.start_usdc), fmt_weth(b.intermediate_weth), fmt_usdc(b.final_usdc), b.gross_usdc as f64 / 1e6, b.gas_usdc, b.net_usdc, b.net_usdc >= min_profit); }
    println!("\nCSV saved: {csv_path}\nREAD-ONLY: no wallet, private key, signing, or transaction submission.");
    println!("Limitations: USDC/WETH pools only; registry addresses and fee tiers must be verified; gas units are assumed; gas conversion uses the first valid pool's reserve ratio; no MEV, priority-fee, failure-cost, or execution simulation.");
    Ok(())
}

#[cfg(test)] mod tests {
    use super::*;
    fn pool(name: &str, address: &str, usdc: u128, weth: u128, fee: u32) -> Pool { Pool { dex:name.into(), address:address.into(), fee_bps:fee, usdc_reserve:usdc, weth_reserve:weth } }
    #[test] fn v2_fee_30_bps_reference() { assert_eq!(amount_out(1_000, 10_000, 20_000, 30), Some(1_813)); }
    #[test] fn supports_different_fee_tiers() { assert!(amount_out(1_000,10_000,20_000,5).unwrap() > amount_out(1_000,10_000,20_000,30).unwrap()); }
    #[test] fn rejects_invalid_amounts_and_fees() { assert_eq!(amount_out(0,10,10,30),None); assert_eq!(amount_out(1,0,10,30),None); assert_eq!(amount_out(1,10,10,1000),None); }
    #[test] fn route_round_trip_is_computed() { let a=pool("A","0x1",10_000_000_000,4_000_000_000_000_000_000_000,30); let b=pool("B","0x2",10_100_000_000,4_000_000_000_000_000_000_000,30); assert!(evaluate(&a,&b,100_000_000,0.1).is_some()); }
    #[test] fn duplicate_pool_route_rejected() { let a=pool("A","same",10_000,20_000,30); assert!(evaluate(&a,&a,100,0.0).is_none()); }
    #[test] fn registry_requires_valid_fee_and_address() { assert!(!valid_address("0x1234")); assert!(valid_address("0x397ff1542f962076d0bfe58ea045ffa2d347aca0")); }
}
