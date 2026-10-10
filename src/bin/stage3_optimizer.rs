use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{env, error::Error, fmt};

const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const UNISWAP_PAIR: &str = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
const SUSHI_FACTORY: &str = "0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac";

const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";
const GET_PAIR_SELECTOR: &str = "e6a43905";
const DECIMALS_SELECTOR: &str = "0x313ce567";

#[derive(Debug)]
struct AppError(String);

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for AppError {}

#[derive(Clone, Debug)]
struct Pool {
    address: String,
    usdc_reserve: u128,
    weth_reserve: u128,
}

#[derive(Clone, Debug)]
struct Evaluation {
    route: &'static str,
    start_usdc: u128,
    intermediate_weth: u128,
    final_usdc: u128,
    gross_profit_usdc: i128,
    gas_cost_usdc: f64,
    net_profit_usdc: f64,
}

struct Rpc {
    client: Client,
    url: String,
    next_id: u64,
}

impl Rpc {
    fn new(url: String) -> Self {
        Self { client: Client::new(), url, next_id: 1 }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;
        let response: Value = self.client.post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()?.error_for_status()?.json()?;
        if let Some(err) = response.get("error") {
            return Err(Box::new(AppError(format!("RPC method {method} failed: {err}"))));
        }
        response.get("result").cloned()
            .ok_or_else(|| Box::new(AppError(format!("RPC response missing result for {method}"))) as Box<dyn Error>)
    }

    fn string_result(&mut self, method: &str, params: Value) -> Result<String, Box<dyn Error>> {
        self.request(method, params)?.as_str().map(str::to_owned)
            .ok_or_else(|| Box::new(AppError(format!("{method} returned a non-string result"))) as Box<dyn Error>)
    }

    fn chain_id(&mut self) -> Result<u64, Box<dyn Error>> {
        Ok(u64::from_str_radix(self.string_result("eth_chainId", json!([]))?.trim_start_matches("0x"), 16)?)
    }
    fn block_number(&mut self) -> Result<u64, Box<dyn Error>> {
        Ok(u64::from_str_radix(self.string_result("eth_blockNumber", json!([]))?.trim_start_matches("0x"), 16)?)
    }
    fn gas_price_wei(&mut self) -> Result<u128, Box<dyn Error>> {
        Ok(u128::from_str_radix(self.string_result("eth_gasPrice", json!([]))?.trim_start_matches("0x"), 16)?)
    }
    fn call(&mut self, to: &str, data: &str, block_tag: &str) -> Result<String, Box<dyn Error>> {
        self.string_result("eth_call", json!([{"to":to,"data":data},block_tag]))
    }
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
fn word_to_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> {
    Ok(u128::from_str_radix(&decode_word(data, index)?, 16)?)
}
fn word_to_address(data: &str, index: usize) -> Result<String, Box<dyn Error>> {
    Ok(format!("0x{}", &decode_word(data, index)?[24..]))
}
fn address_argument(address: &str) -> Result<String, Box<dyn Error>> {
    let raw = address.strip_prefix("0x").ok_or_else(|| AppError("Address missing 0x prefix".into()))?;
    if raw.len() != 40 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Box::new(AppError(format!("Invalid Ethereum address: {address}"))));
    }
    Ok(format!("{:0>64}", raw.to_ascii_lowercase()))
}
fn normalized(address: &str) -> String { address.to_ascii_lowercase() }

fn get_pair(rpc: &mut Rpc, block_tag: &str) -> Result<String, Box<dyn Error>> {
    let data = format!("0x{}{}{}", GET_PAIR_SELECTOR, address_argument(USDC)?, address_argument(WETH)?);
    let pair = word_to_address(&rpc.call(SUSHI_FACTORY, &data, block_tag)?, 0)?;
    if pair == "0x0000000000000000000000000000000000000000" {
        return Err(Box::new(AppError("SushiSwap factory returned zero pair address".into())));
    }
    Ok(pair)
}

fn read_pool(rpc: &mut Rpc, address: &str, block_tag: &str) -> Result<Pool, Box<dyn Error>> {
    let token0 = normalized(&word_to_address(&rpc.call(address, TOKEN0_SELECTOR, block_tag)?, 0)?);
    let token1 = normalized(&word_to_address(&rpc.call(address, TOKEN1_SELECTOR, block_tag)?, 0)?);
    let usdc = normalized(USDC);
    let weth = normalized(WETH);
    if !((token0 == usdc && token1 == weth) || (token0 == weth && token1 == usdc)) {
        return Err(Box::new(AppError(format!("Pair token mismatch: {token0}, {token1}"))));
    }

    let usdc_decimals = word_to_u128(&rpc.call(USDC, DECIMALS_SELECTOR, block_tag)?, 0)?;
    let weth_decimals = word_to_u128(&rpc.call(WETH, DECIMALS_SELECTOR, block_tag)?, 0)?;
    if usdc_decimals != 6 || weth_decimals != 18 {
        return Err(Box::new(AppError(format!("Unexpected token decimals: USDC={usdc_decimals}, WETH={weth_decimals}"))));
    }

    let reserves = rpc.call(address, RESERVES_SELECTOR, block_tag)?;
    let r0 = word_to_u128(&reserves, 0)?;
    let r1 = word_to_u128(&reserves, 1)?;
    let (usdc_reserve, weth_reserve) = if token0 == usdc { (r0, r1) } else { (r1, r0) };
    if usdc_reserve == 0 || weth_reserve == 0 {
        return Err(Box::new(AppError("Pool has a zero reserve".into())));
    }
    Ok(Pool { address: address.to_ascii_lowercase(), usdc_reserve, weth_reserve })
}

fn amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 { return None; }
    let input_fee = amount_in.checked_mul(997)?;
    let numerator = input_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in.checked_mul(1000)?.checked_add(input_fee)?;
    Some(numerator / denominator)
}

fn format_usdc(raw: u128) -> String {
    format!("{}.{:06}", raw / 1_000_000, raw % 1_000_000)
}
fn format_weth(raw: u128) -> String {
    format!("{}.{:018}", raw / 1_000_000_000_000_000_000, raw % 1_000_000_000_000_000_000)
}

// Round to the nearest 0.000001 USDC (one base unit). Floating-point candidate
// values are used only to generate a search grid; on-chain token amounts remain integers.
fn raw_usdc(amount: f64) -> Result<u128, Box<dyn Error>> {
    if !amount.is_finite() || amount <= 0.0 {
        return Err(Box::new(AppError(format!("Invalid amount: {amount}"))));
    }
    let raw = amount * 1_000_000.0;
    if !raw.is_finite() || raw < 1.0 || raw >= u128::MAX as f64 {
        return Err(Box::new(AppError(format!("USDC amount is out of range: {amount}"))));
    }
    Ok(raw.round() as u128)
}

fn evaluate(
    route: &'static str,
    first: &Pool,
    second: &Pool,
    start_usdc: u128,
    gas_cost_usdc: f64,
) -> Option<Evaluation> {
    let weth = amount_out(start_usdc, first.usdc_reserve, first.weth_reserve)?;
    let final_usdc = amount_out(weth, second.weth_reserve, second.usdc_reserve)?;
    let gross_profit_usdc = final_usdc as i128 - start_usdc as i128;
    Some(Evaluation {
        route,
        start_usdc,
        intermediate_weth: weth,
        final_usdc,
        gross_profit_usdc,
        gas_cost_usdc,
        net_profit_usdc: gross_profit_usdc as f64 / 1e6 - gas_cost_usdc,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!(" CRYPTO ARBITRAGE ENGINE — STAGE 3.7");
    println!(" TRADE-SIZE OPTIMIZER (READ-ONLY)");
    println!("==================================================");

    let rpc_url = env::var("ETH_RPC_URL")
        .map_err(|_| AppError("ETH_RPC_URL environment variable is missing".into()))?;
    let min_profit: f64 = env::var("ARB_MIN_PROFIT_USDC").unwrap_or_else(|_| "1.00".into())
        .parse().map_err(|_| AppError("ARB_MIN_PROFIT_USDC must be numeric".into()))?;
    if !min_profit.is_finite() || min_profit < 0.0 {
        return Err(Box::new(AppError("ARB_MIN_PROFIT_USDC must be finite and >= 0".into())));
    }
    let gas_units: u128 = env::var("ARB_GAS_UNITS").unwrap_or_else(|_| "300000".into())
        .parse().map_err(|_| AppError("ARB_GAS_UNITS must be an integer".into()))?;
    if gas_units == 0 {
        return Err(Box::new(AppError("ARB_GAS_UNITS must be > 0".into())));
    }
    let min_size: f64 = env::var("ARB_MIN_SIZE_USDC").unwrap_or_else(|_| "50".into())
        .parse().map_err(|_| AppError("ARB_MIN_SIZE_USDC must be numeric".into()))?;
    let max_size: f64 = env::var("ARB_MAX_SIZE_USDC").unwrap_or_else(|_| "20000".into())
        .parse().map_err(|_| AppError("ARB_MAX_SIZE_USDC must be numeric".into()))?;
    let steps: usize = env::var("ARB_OPTIMIZER_STEPS").unwrap_or_else(|_| "250".into())
        .parse().map_err(|_| AppError("ARB_OPTIMIZER_STEPS must be numeric".into()))?;
    if !min_size.is_finite() || !max_size.is_finite() || min_size <= 0.0 || max_size < min_size {
        return Err(Box::new(AppError("Require 0 < ARB_MIN_SIZE_USDC <= ARB_MAX_SIZE_USDC".into())));
    }
    if steps < 2 || steps > 100_000 {
        return Err(Box::new(AppError("ARB_OPTIMIZER_STEPS must be between 2 and 100000".into())));
    }

    let mut rpc = Rpc::new(rpc_url);
    let chain = rpc.chain_id()?;
    if chain != 1 {
        return Err(Box::new(AppError(format!("Expected Ethereum Mainnet chain ID 1, got {chain}"))));
    }
    let block = rpc.block_number()?;
    let block_tag = format!("0x{block:x}");
    let gas_price = rpc.gas_price_wei()?;
    let sushi_address = get_pair(&mut rpc, &block_tag)?;
    let uni = read_pool(&mut rpc, UNISWAP_PAIR, &block_tag)?;
    let sushi = read_pool(&mut rpc, &sushi_address, &block_tag)?;

    let uni_price = (uni.usdc_reserve as f64 / 1e6) / (uni.weth_reserve as f64 / 1e18);
    let gas_eth = gas_price as f64 * gas_units as f64 / 1e18;
    let gas_usdc = gas_eth * uni_price;

    println!("Network: Ethereum Mainnet | block: {block}");
    println!("Uniswap pair: {}", uni.address);
    println!("SushiSwap pair: {}", sushi.address);
    println!("Gas price: {:.3} gwei | gas units assumption: {gas_units}", gas_price as f64 / 1e9);
    println!("Estimated gas: {:.8} ETH (~${:.4} USDC)", gas_eth, gas_usdc);
    println!("Search range: ${:.2} to ${:.2} | samples: {steps}", min_size, max_size);
    println!("Minimum net-profit threshold: ${:.2} USDC", min_profit);
    println!();
    println!("Starting USDC | Route                  | final USDC              | gross P/L | gas USDC | net P/L");
    println!("------------------------------------------------------------------------------------------------");

    let mut best: Option<Evaluation> = None;
    let mut threshold_count = 0usize;
    let mut seen_sizes = 0usize;

    for i in 0..steps {
        let fraction = i as f64 / (steps - 1) as f64;
        let size = min_size + (max_size - min_size) * fraction;
        let start = raw_usdc(size)?;
        seen_sizes += 1;

        for candidate in [
            evaluate("SushiSwap -> Uniswap", &sushi, &uni, start, gas_usdc),
            evaluate("Uniswap -> SushiSwap", &uni, &sushi, start, gas_usdc),
        ].into_iter().flatten() {
            println!(
                "{:>10} | {:<22} | {:>22} | {:+9.4} | {:>8.4} | {:+9.4}",
                format_usdc(candidate.start_usdc),
                candidate.route,
                format_usdc(candidate.final_usdc),
                candidate.gross_profit_usdc as f64 / 1e6,
                candidate.gas_cost_usdc,
                candidate.net_profit_usdc
            );
            if candidate.net_profit_usdc >= min_profit {
                threshold_count += 1;
            }
            if best.as_ref().map(|current| candidate.net_profit_usdc > current.net_profit_usdc).unwrap_or(true) {
                best = Some(candidate);
            }
        }
    }

    println!();
    println!("================ OPTIMIZER SUMMARY ================");
    println!("Unique starting sizes evaluated: {seen_sizes}");
    println!("Route evaluations: {}", seen_sizes * 2);
    println!("Candidates meeting threshold: {threshold_count}");
    if let Some(best) = best {
        println!("Best sampled route: {}", best.route);
        println!("Starting capital: {} USDC", format_usdc(best.start_usdc));
        println!("Intermediate WETH: {}", format_weth(best.intermediate_weth));
        println!("Final output: {} USDC", format_usdc(best.final_usdc));
        println!("Gross P/L: {:+.6} USDC", best.gross_profit_usdc as f64 / 1e6);
        println!("Estimated gas: {:.6} USDC", best.gas_cost_usdc);
        println!("Estimated net P/L: {:+.6} USDC", best.net_profit_usdc);
        if best.net_profit_usdc >= min_profit {
            println!("Decision: passes the configured estimated-profit threshold (theoretical only).");
        } else {
            println!("Decision: no sampled route passed the configured estimated-profit threshold.");
        }
    }

    println!();
    println!("LIMITATIONS");
    println!("- Uniform grid search; it does not prove a global optimum.");
    println!("- Pool snapshots are pinned to one block; execution would happen later.");
    println!("- Gas units are an assumption, not eth_estimateGas output.");
    println!("- Gas conversion uses Uniswap's reserve ratio as an approximation.");
    println!("- No MEV, priority fees, failed-transaction cost, or execution simulation.");
    println!("- Read-only: no private key, wallet signing, or transaction execution.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(usdc: u128, weth: u128) -> Pool {
        Pool {
            address: "0x0000000000000000000000000000000000000001".into(),
            usdc_reserve: usdc,
            weth_reserve: weth,
        }
    }

    #[test]
    fn v2_quote_reference_example() {
        assert_eq!(amount_out(1_000, 10_000, 20_000), Some(1_813));
    }

    #[test]
    fn rejects_zero_inputs_and_reserves() {
        assert_eq!(amount_out(0, 10, 10), None);
        assert_eq!(amount_out(1, 0, 10), None);
        assert_eq!(amount_out(1, 10, 0), None);
    }

    #[test]
    fn address_is_abi_padded() {
        let encoded = address_argument("0x0000000000000000000000000000000000000001").unwrap();
        assert_eq!(encoded.len(), 64);
        assert!(encoded.ends_with('1'));
    }

    #[test]
    fn fractional_usdc_candidate_rounds_to_base_unit() {
        let raw = raw_usdc(130.12048192771084).unwrap();
        assert_eq!(raw, 130_120_482);
        assert_eq!(format_usdc(raw), "130.120482");
    }

    #[test]
    fn route_accounts_for_gas() {
        let uni = pool(10_000_000_000, 4_000_000_000_000_000_000_000);
        let sushi = pool(10_000_000_000, 4_000_000_000_000_000_000_000);
        let start = 100_000_000u128;
        let no_gas = evaluate("A -> B", &uni, &sushi, start, 0.0).unwrap();
        let with_gas = evaluate("A -> B", &uni, &sushi, start, 1.0).unwrap();
        assert!((no_gas.net_profit_usdc - with_gas.net_profit_usdc - 1.0).abs() < 1e-9);
    }
}
