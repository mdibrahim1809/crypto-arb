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
    name: &'static str,
    address: String,
    usdc_reserve: u128,
    weth_reserve: u128,
}

#[derive(Clone, Debug)]
struct RouteResult {
    name: &'static str,
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
        Self {
            client: Client::new(),
            url,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let id = self.next_id;
        self.next_id += 1;

        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }))
            .send()?
            .error_for_status()?
            .json()?;

        if let Some(err) = response.get("error") {
            return Err(Box::new(AppError(format!(
                "RPC method {method} failed: {err}"
            ))));
        }

        response.get("result").cloned().ok_or_else(|| {
            Box::new(AppError(format!(
                "RPC response missing result for {method}"
            ))) as Box<dyn Error>
        })
    }

    fn chain_id(&mut self) -> Result<u64, Box<dyn Error>> {
        let result = self.request("eth_chainId", json!([]))?;
        let text = result
            .as_str()
            .ok_or_else(|| AppError("eth_chainId returned a non-string result".into()))?;
        Ok(u64::from_str_radix(text.trim_start_matches("0x"), 16)?)
    }

    fn block_number(&mut self) -> Result<u64, Box<dyn Error>> {
        let result = self.request("eth_blockNumber", json!([]))?;
        let text = result
            .as_str()
            .ok_or_else(|| AppError("eth_blockNumber returned a non-string result".into()))?;
        Ok(u64::from_str_radix(text.trim_start_matches("0x"), 16)?)
    }

    fn gas_price_wei(&mut self) -> Result<u128, Box<dyn Error>> {
        let result = self.request("eth_gasPrice", json!([]))?;
        let text = result
            .as_str()
            .ok_or_else(|| AppError("eth_gasPrice returned a non-string result".into()))?;
        Ok(u128::from_str_radix(text.trim_start_matches("0x"), 16)?)
    }

    fn call(&mut self, to: &str, data: &str, block_tag: &str) -> Result<String, Box<dyn Error>> {
        let result = self.request("eth_call", json!([{"to": to, "data": data}, block_tag]))?;
        result.as_str().map(str::to_owned).ok_or_else(|| {
            Box::new(AppError("eth_call returned a non-string result".into())) as Box<dyn Error>
        })
    }
}

fn decode_word(data: &str, word_index: usize) -> Result<String, Box<dyn Error>> {
    let raw = data.strip_prefix("0x").unwrap_or(data);
    let start = word_index
        .checked_mul(64)
        .ok_or_else(|| AppError("ABI word index overflow".into()))?;
    let end = start + 64;
    if raw.len() < end {
        return Err(Box::new(AppError(format!(
            "ABI response too short for word {word_index}: {} hex characters",
            raw.len()
        ))));
    }
    Ok(raw[start..end].to_lowercase())
}

fn word_to_u128(data: &str, word_index: usize) -> Result<u128, Box<dyn Error>> {
    Ok(u128::from_str_radix(&decode_word(data, word_index)?, 16)?)
}

fn word_to_address(data: &str, word_index: usize) -> Result<String, Box<dyn Error>> {
    let word = decode_word(data, word_index)?;
    Ok(format!("0x{}", &word[24..]))
}

fn normalize_address(address: &str) -> String {
    address.to_ascii_lowercase()
}

fn address_argument(address: &str) -> Result<String, Box<dyn Error>> {
    let raw = address
        .strip_prefix("0x")
        .ok_or_else(|| AppError(format!("Address lacks 0x prefix: {address}")))?;
    if raw.len() != 40 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Box::new(AppError(format!(
            "Invalid Ethereum address: {address}"
        ))));
    }
    Ok(format!("{:0>64}", raw.to_ascii_lowercase()))
}

fn get_pair_address(
    rpc: &mut Rpc,
    factory: &str,
    token_a: &str,
    token_b: &str,
    block_tag: &str,
) -> Result<String, Box<dyn Error>> {
    let data = format!(
        "0x{}{}{}",
        GET_PAIR_SELECTOR,
        address_argument(token_a)?,
        address_argument(token_b)?
    );
    let result = rpc.call(factory, &data, block_tag)?;
    let pair = word_to_address(&result, 0)?;
    if pair == "0x0000000000000000000000000000000000000000" {
        return Err(Box::new(AppError(
            "Factory returned the zero address for the requested pair".into(),
        )));
    }
    Ok(pair)
}

fn read_pool(
    rpc: &mut Rpc,
    name: &'static str,
    address: &str,
    block_tag: &str,
) -> Result<Pool, Box<dyn Error>> {
    let token0_data = rpc.call(address, TOKEN0_SELECTOR, block_tag)?;
    let token1_data = rpc.call(address, TOKEN1_SELECTOR, block_tag)?;
    let token0 = word_to_address(&token0_data, 0)?;
    let token1 = word_to_address(&token1_data, 0)?;

    let usdc = normalize_address(USDC);
    let weth = normalize_address(WETH);
    let token0 = normalize_address(&token0);
    let token1 = normalize_address(&token1);

    if !((token0 == usdc && token1 == weth) || (token0 == weth && token1 == usdc)) {
        return Err(Box::new(AppError(format!(
            "{name} pair token mismatch: token0={token0}, token1={token1}"
        ))));
    }

    // Read decimals at the same block to validate the expected token units.
    let usdc_decimals_data = rpc.call(USDC, DECIMALS_SELECTOR, block_tag)?;
    let weth_decimals_data = rpc.call(WETH, DECIMALS_SELECTOR, block_tag)?;
    let usdc_decimals = word_to_u128(&usdc_decimals_data, 0)?;
    let weth_decimals = word_to_u128(&weth_decimals_data, 0)?;
    if usdc_decimals != 6 || weth_decimals != 18 {
        return Err(Box::new(AppError(format!(
            "Unexpected token decimals: USDC={usdc_decimals}, WETH={weth_decimals}"
        ))));
    }

    let reserves_data = rpc.call(address, RESERVES_SELECTOR, block_tag)?;
    let reserve0 = word_to_u128(&reserves_data, 0)?;
    let reserve1 = word_to_u128(&reserves_data, 1)?;

    let (usdc_reserve, weth_reserve) = if token0 == usdc {
        (reserve0, reserve1)
    } else {
        (reserve1, reserve0)
    };

    if usdc_reserve == 0 || weth_reserve == 0 {
        return Err(Box::new(AppError(format!("{name} has a zero reserve"))));
    }

    Ok(Pool {
        name,
        address: address.to_ascii_lowercase(),
        usdc_reserve,
        weth_reserve,
    })
}

// Constant-product AMM quote for a V2 pool with a 0.30% fee.
// Amounts are raw token units: USDC uses 6 decimals, WETH uses 18.
fn amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return None;
    }
    let amount_in_with_fee = amount_in.checked_mul(997)?;
    let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in
        .checked_mul(1000)?
        .checked_add(amount_in_with_fee)?;
    Some(numerator / denominator)
}

fn format_usdc(raw: u128) -> String {
    format!("{}.{:06}", raw / 1_000_000, raw % 1_000_000)
}

fn format_weth(raw: u128) -> String {
    format!(
        "{}.{:018}",
        raw / 1_000_000_000_000_000_000,
        raw % 1_000_000_000_000_000_000
    )
}

fn raw_usdc(amount: f64) -> Result<u128, Box<dyn Error>> {
    if !amount.is_finite() || amount <= 0.0 {
        return Err(Box::new(AppError(format!(
            "Invalid starting amount: {amount}"
        ))));
    }
    let raw = amount * 1_000_000.0;
    if raw.fract().abs() > 0.0001 || raw > u128::MAX as f64 {
        return Err(Box::new(AppError(format!(
            "Starting amount cannot be represented in USDC base units: {amount}"
        ))));
    }
    Ok(raw.round() as u128)
}

fn calculate_route(
    name: &'static str,
    first_pool: &Pool,
    second_pool: &Pool,
    start_usdc: u128,
    gas_cost_usdc: f64,
) -> Result<RouteResult, Box<dyn Error>> {
    // Leg 1: USDC -> WETH on the first pool.
    let weth_out = amount_out(start_usdc, first_pool.usdc_reserve, first_pool.weth_reserve)
        .ok_or_else(|| AppError(format!("Could not quote first leg for {name}")))?;

    // Leg 2: WETH -> USDC on the second pool.
    let final_usdc = amount_out(weth_out, second_pool.weth_reserve, second_pool.usdc_reserve)
        .ok_or_else(|| AppError(format!("Could not quote second leg for {name}")))?;

    let gross_profit_usdc = final_usdc as i128 - start_usdc as i128;
    let gross_profit_decimal = gross_profit_usdc as f64 / 1_000_000.0;
    let net_profit_usdc = gross_profit_decimal - gas_cost_usdc;

    Ok(RouteResult {
        name,
        start_usdc,
        intermediate_weth: weth_out,
        final_usdc,
        gross_profit_usdc,
        gas_cost_usdc,
        net_profit_usdc,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!("       CRYPTO ARBITRAGE ENGINE — STAGE 3.6");
    println!("       TWO-POOL PROFITABILITY SIMULATOR");
    println!("==================================================");

    let rpc_url = env::var("ETH_RPC_URL")
        .map_err(|_| AppError("ETH_RPC_URL environment variable is missing.".into()))?;

    let gas_units: u128 = env::var("ARB_GAS_UNITS")
        .unwrap_or_else(|_| "300000".to_string())
        .parse()
        .map_err(|_| AppError("ARB_GAS_UNITS must be a positive integer".into()))?;
    if gas_units == 0 {
        return Err(Box::new(AppError(
            "ARB_GAS_UNITS must be greater than zero".into(),
        )));
    }

    let min_profit_usdc: f64 = env::var("ARB_MIN_PROFIT_USDC")
        .unwrap_or_else(|_| "1.00".to_string())
        .parse()
        .map_err(|_| AppError("ARB_MIN_PROFIT_USDC must be a valid number".into()))?;
    if !min_profit_usdc.is_finite() || min_profit_usdc < 0.0 {
        return Err(Box::new(AppError(
            "ARB_MIN_PROFIT_USDC must be finite and non-negative".into(),
        )));
    }

    let mut rpc = Rpc::new(rpc_url);
    let chain_id = rpc.chain_id()?;
    if chain_id != 1 {
        return Err(Box::new(AppError(format!(
            "Wrong network: expected Ethereum Mainnet chain ID 1, got {chain_id}"
        ))));
    }

    let block = rpc.block_number()?;
    let block_tag = format!("0x{block:x}");
    let gas_price_wei = rpc.gas_price_wei()?;

    let sushi_pair = get_pair_address(&mut rpc, SUSHI_FACTORY, USDC, WETH, &block_tag)?;

    let uniswap = read_pool(&mut rpc, "Uniswap V2", UNISWAP_PAIR, &block_tag)?;
    let sushi = read_pool(&mut rpc, "SushiSwap V2", &sushi_pair, &block_tag)?;

    // Approximate conversion of the ETH gas bill into USDC using Uniswap's
    // reserve ratio. This is a valuation estimate, not an executable gas quote.
    let usdc_per_weth = (uniswap.usdc_reserve as f64 / 1_000_000.0)
        / (uniswap.weth_reserve as f64 / 1_000_000_000_000_000_000.0);
    let gas_cost_eth = (gas_price_wei as f64 * gas_units as f64) / 1e18;
    let gas_cost_usdc = gas_cost_eth * usdc_per_weth;

    println!("Network: Ethereum Mainnet");
    println!("Shared snapshot block: {block}");
    println!("Pool fee assumption: 0.30% per swap");
    println!("Gas units assumption: {gas_units} for the two-swap route");
    println!("Current gas price: {:.3} gwei", gas_price_wei as f64 / 1e9);
    println!(
        "Estimated gas cost: {:.8} ETH (~${:.4} USDC)",
        gas_cost_eth, gas_cost_usdc
    );
    println!("Minimum net-profit threshold: ${min_profit_usdc:.2} USDC");
    println!();
    println!("========== POOL SNAPSHOTS ==========");
    for pool in [&uniswap, &sushi] {
        println!("Venue: {}", pool.name);
        println!("Pair: {}", pool.address);
        println!("USDC reserve: {}", format_usdc(pool.usdc_reserve));
        println!("WETH reserve: {}", format_weth(pool.weth_reserve));
        println!();
    }

    let sizes = [100.0_f64, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0];
    let mut best: Option<RouteResult> = None;

    for size in sizes {
        let start = raw_usdc(size)?;
        println!("==================================================");
        println!("STARTING CAPITAL: ${:.2} USDC", size);
        println!("==================================================");

        // Route A: buy WETH on SushiSwap, sell WETH on Uniswap.
        let route_a = calculate_route(
            "SushiSwap -> Uniswap",
            &sushi,
            &uniswap,
            start,
            gas_cost_usdc,
        )?;
        // Route B: buy WETH on Uniswap, sell WETH on SushiSwap.
        let route_b = calculate_route(
            "Uniswap -> SushiSwap",
            &uniswap,
            &sushi,
            start,
            gas_cost_usdc,
        )?;

        for route in [&route_a, &route_b] {
            println!("Route: {}", route.name);
            println!("  Start:          {} USDC", format_usdc(route.start_usdc));
            println!(
                "  Intermediate:   {} WETH",
                format_weth(route.intermediate_weth)
            );
            println!("  Final output:   {} USDC", format_usdc(route.final_usdc));
            println!(
                "  Gross P/L:      {:+.6} USDC",
                route.gross_profit_usdc as f64 / 1e6
            );
            println!("  Estimated gas:  {:.6} USDC", route.gas_cost_usdc);
            println!("  Estimated net:  {:+.6} USDC", route.net_profit_usdc);
            println!(
                "  Threshold:      {}",
                if route.net_profit_usdc >= min_profit_usdc {
                    "PASSES configured threshold (theoretical)"
                } else {
                    "does not pass configured threshold"
                }
            );
            println!();

            if best
                .as_ref()
                .map(|current| route.net_profit_usdc > current.net_profit_usdc)
                .unwrap_or(true)
            {
                best = Some(route.clone());
            }
        }
    }

    println!("==================================================");
    println!("SUMMARY");
    println!("==================================================");
    if let Some(best_route) = best {
        println!("Best tested route: {}", best_route.name);
        println!(
            "Starting capital: {} USDC",
            format_usdc(best_route.start_usdc)
        );
        println!(
            "Estimated final output: {} USDC",
            format_usdc(best_route.final_usdc)
        );
        println!(
            "Estimated net result: {:+.6} USDC",
            best_route.net_profit_usdc
        );
        if best_route.net_profit_usdc >= min_profit_usdc {
            println!("Status: passes the configured estimated-profit threshold.");
        } else {
            println!("Status: no tested route passed the configured estimated-profit threshold.");
        }
    }

    println!();
    println!("IMPORTANT LIMITATIONS");
    println!("- Read-only RPC calls; no wallet, signing, or transaction execution.");
    println!("- Both pools are read at one block, but quotes remain theoretical.");
    println!("- The 300,000 gas default is an assumption, not an eth_estimateGas result.");
    println!("- Gas is valued using a reserve-ratio spot estimate; actual conversion differs.");
    println!("- Does not model MEV, priority fees, failed transactions, latency, or competition.");
    println!("- A positive estimate is not proof that a real trade would be profitable.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_quote_matches_reference_example() {
        // 1000 input, 10000 reserve in, 20000 reserve out, 0.30% fee.
        assert_eq!(amount_out(1_000, 10_000, 20_000), Some(1_813));
    }

    #[test]
    fn zero_inputs_and_reserves_are_rejected() {
        assert_eq!(amount_out(0, 100, 100), None);
        assert_eq!(amount_out(1, 0, 100), None);
        assert_eq!(amount_out(1, 100, 0), None);
    }

    #[test]
    fn address_argument_is_abi_padded() {
        let padded = address_argument("0x0000000000000000000000000000000000000001").unwrap();
        assert_eq!(padded.len(), 64);
        assert!(padded.ends_with('1'));
    }

    #[test]
    fn decimals_format_correctly() {
        assert_eq!(format_usdc(1_234_567), "1.234567");
        assert_eq!(
            format_weth(1_500_000_000_000_000_000),
            "1.500000000000000000"
        );
    }

    #[test]
    fn negative_profit_is_preserved() {
        let pool_a = Pool {
            name: "A",
            address: "0x1".into(),
            usdc_reserve: 1_000_000_000,
            weth_reserve: 1_000_000_000_000_000_000,
        };
        let pool_b = Pool {
            name: "B",
            address: "0x2".into(),
            usdc_reserve: 1_000_000_000,
            weth_reserve: 1_000_000_000_000_000_000,
        };
        let result = calculate_route("A -> B", &pool_a, &pool_b, 10_000_000, 0.0).unwrap();
        assert!(result.gross_profit_usdc < 0);
    }
}
