use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{env, error::Error, time::Duration};

const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const UNISWAP_V2_PAIR: &str = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
const SUSHI_V2_FACTORY: &str = "0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac";

const GET_PAIR: &str = "e6a43905";
const TOKEN0: &str = "0x0dfe1681";
const TOKEN1: &str = "0xd21220a7";
const GET_RESERVES: &str = "0x0902f1ac";
const DECIMALS: &str = "0x313ce567";

#[derive(Clone)]
struct RpcClient {
    client: Client,
    endpoint: String,
}

#[derive(Clone, Debug)]
struct Pool {
    venue: &'static str,
    address: String,
    token0: String,
    token1: String,
    decimals0: u32,
    decimals1: u32,
    reserve0: u128,
    reserve1: u128,
}

struct OrientedPool {
    venue: &'static str,
    address: String,
    usdc_reserve: u128,
    weth_reserve: u128,
    usdc_decimals: u32,
    weth_decimals: u32,
}

impl RpcClient {
    fn new(endpoint: String) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(15)).build()?,
            endpoint,
        })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params
            }))
            .send()?
            .error_for_status()?;

        let body: Value = response.json()?;
        if let Some(error) = body.get("error") {
            return Err(format!("RPC error for {method}: {error}").into());
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| format!("RPC response has no result for {method}").into())
    }

    fn eth_call(
        &self,
        address: &str,
        data: &str,
        block_tag: &str,
    ) -> Result<String, Box<dyn Error>> {
        self.call(
            "eth_call",
            json!([
                {"to": address, "data": data},
                block_tag
            ]),
        )?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "eth_call returned a non-string result".into())
    }
}

fn parse_hex_u64(value: &Value) -> Result<u64, Box<dyn Error>> {
    let text = value
        .as_str()
        .ok_or("Expected hexadecimal quantity")?
        .strip_prefix("0x")
        .ok_or("Missing 0x prefix")?;
    if text.is_empty() {
        return Ok(0);
    }
    Ok(u64::from_str_radix(text, 16)?)
}

fn word(data: &str, index: usize) -> Result<&str, Box<dyn Error>> {
    let hex = data
        .strip_prefix("0x")
        .ok_or("ABI data missing 0x prefix")?;
    let start = index.checked_mul(64).ok_or("ABI offset overflow")?;
    let end = start.checked_add(64).ok_or("ABI offset overflow")?;
    if hex.len() < end {
        return Err(format!("ABI response too short for word {index}").into());
    }
    Ok(&hex[start..end])
}

fn decode_address(data: &str) -> Result<String, Box<dyn Error>> {
    Ok(format!("0x{}", &word(data, 0)?[24..]).to_ascii_lowercase())
}

fn decode_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> {
    Ok(u128::from_str_radix(word(data, index)?, 16)?)
}

fn encode_address(address: &str) -> Result<String, Box<dyn Error>> {
    let raw = address
        .strip_prefix("0x")
        .ok_or("Address missing 0x prefix")?;
    if raw.len() != 40 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("Invalid address: {address}").into());
    }
    Ok(format!("{raw:0>64}").to_ascii_lowercase())
}

fn format_units(raw: u128, decimals: u32) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let scale = 10_u128.pow(decimals);
    let whole = raw / scale;
    let fraction = raw % scale;
    let fraction = format!("{:0width$}", fraction, width = decimals as usize);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

fn parse_units(input: &str, decimals: u32) -> Result<u128, Box<dyn Error>> {
    if input.is_empty() || input.starts_with('-') {
        return Err("Amount must be a positive decimal number.".into());
    }
    let mut parts = input.split('.');
    let whole = parts.next().ok_or("Missing whole amount")?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err("Multiple decimal points.".into());
    }
    if whole.is_empty() {
        return Err("Use a leading zero, e.g. 0.5.".into());
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Amount must contain decimal digits only.".into());
    }
    if fraction.len() > decimals as usize {
        return Err(format!("Too many decimal places; token supports {decimals}.").into());
    }
    let scale = 10_u128
        .checked_pow(decimals)
        .ok_or("Decimal scale overflow")?;
    let whole_raw = whole
        .parse::<u128>()?
        .checked_mul(scale)
        .ok_or("Amount overflow")?;
    let fraction_padded = format!("{:0<width$}", fraction, width = decimals as usize);
    let fraction_raw = if fraction_padded.is_empty() {
        0
    } else {
        fraction_padded.parse::<u128>()?
    };
    let amount = whole_raw
        .checked_add(fraction_raw)
        .ok_or("Amount overflow")?;
    if amount == 0 {
        return Err("Amount must be greater than zero.".into());
    }
    Ok(amount)
}

// Uniswap V2 and SushiSwap V2 standard fee assumption: 0.30% (997/1000).
fn amount_out(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, Box<dyn Error>> {
    if amount_in == 0 {
        return Err("Input must be greater than zero.".into());
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err("Pool reserves must be non-zero.".into());
    }
    let input_with_fee = amount_in
        .checked_mul(997)
        .ok_or("Fee calculation overflow")?;
    let numerator = input_with_fee
        .checked_mul(reserve_out)
        .ok_or("Quote numerator overflow")?;
    let denominator = reserve_in
        .checked_mul(1000)
        .and_then(|x| x.checked_add(input_with_fee))
        .ok_or("Quote denominator overflow")?;
    let output = numerator / denominator;
    if output == 0 || output >= reserve_out {
        return Err("Quote output is zero or would exhaust reserve.".into());
    }
    Ok(output)
}

fn read_pool(
    rpc: &RpcClient,
    venue: &'static str,
    pair_address: &str,
    block_tag: &str,
) -> Result<Pool, Box<dyn Error>> {
    let token0 = decode_address(&rpc.eth_call(pair_address, TOKEN0, block_tag)?)?;
    let token1 = decode_address(&rpc.eth_call(pair_address, TOKEN1, block_tag)?)?;
    let valid_pair = (token0.eq_ignore_ascii_case(USDC) && token1.eq_ignore_ascii_case(WETH))
        || (token0.eq_ignore_ascii_case(WETH) && token1.eq_ignore_ascii_case(USDC));
    if !valid_pair {
        return Err(format!("{venue} pair token validation failed.").into());
    }

    let reserves = rpc.eth_call(pair_address, GET_RESERVES, block_tag)?;
    let reserve0 = decode_u128(&reserves, 0)?;
    let reserve1 = decode_u128(&reserves, 1)?;
    if reserve0 == 0 || reserve1 == 0 {
        return Err(format!("{venue} pool has zero reserves.").into());
    }

    let decimals0 = decode_u128(&rpc.eth_call(&token0, DECIMALS, block_tag)?, 0)?;
    let decimals1 = decode_u128(&rpc.eth_call(&token1, DECIMALS, block_tag)?, 0)?;
    if decimals0 > 38 || decimals1 > 38 {
        return Err("Unsupported token decimals.".into());
    }

    Ok(Pool {
        venue,
        address: pair_address.to_ascii_lowercase(),
        token0,
        token1,
        decimals0: decimals0 as u32,
        decimals1: decimals1 as u32,
        reserve0,
        reserve1,
    })
}

fn orient(pool: Pool) -> OrientedPool {
    if pool.token0.eq_ignore_ascii_case(USDC) {
        OrientedPool {
            venue: pool.venue,
            address: pool.address,
            usdc_reserve: pool.reserve0,
            weth_reserve: pool.reserve1,
            usdc_decimals: pool.decimals0,
            weth_decimals: pool.decimals1,
        }
    } else {
        OrientedPool {
            venue: pool.venue,
            address: pool.address,
            usdc_reserve: pool.reserve1,
            weth_reserve: pool.reserve0,
            usdc_decimals: pool.decimals1,
            weth_decimals: pool.decimals0,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!("       CRYPTO ARBITRAGE ENGINE — STAGE 3.5");
    println!("       CROSS-DEX QUOTE COMPARATOR");
    println!("==================================================");

    let endpoint = env::var("ETH_RPC_URL")
        .map_err(|_| "ETH_RPC_URL is missing. Set your Ethereum RPC endpoint.")?;
    let rpc = RpcClient::new(endpoint)?;

    let chain_id = parse_hex_u64(&rpc.call("eth_chainId", json!([]))?)?;
    if chain_id != 1 {
        return Err(format!("Expected Ethereum Mainnet chain ID 1, got {chain_id}.").into());
    }

    // Both venues are read at the same explicit block tag for a fairer comparison.
    let block = parse_hex_u64(&rpc.call("eth_blockNumber", json!([]))?)?;
    let block_tag = format!("0x{block:x}");

    let usdc_arg = encode_address(USDC)?;
    let weth_arg = encode_address(WETH)?;
    let get_pair_data = format!("0x{GET_PAIR}{usdc_arg}{weth_arg}");
    let sushi_pair =
        decode_address(&rpc.eth_call(SUSHI_V2_FACTORY, &get_pair_data, &block_tag)?)?;
    if sushi_pair == "0x0000000000000000000000000000000000000000" {
        return Err("SushiSwap factory returned no WETH/USDC pair.".into());
    }

    let uniswap = orient(read_pool(&rpc, "Uniswap V2", UNISWAP_V2_PAIR, &block_tag)?);
    let sushi = orient(read_pool(&rpc, "SushiSwap V2", &sushi_pair, &block_tag)?);

    if uniswap.usdc_decimals != sushi.usdc_decimals || uniswap.weth_decimals != sushi.weth_decimals
    {
        return Err("Token decimal mismatch between pools.".into());
    }

    let usdc_decimals = uniswap.usdc_decimals;
    let weth_decimals = uniswap.weth_decimals;

    println!("Network: Ethereum Mainnet");
    println!("Shared snapshot block: {block}");
    println!("Fee assumption: 0.30% per pool");
    println!("\n========== POOL SNAPSHOTS ==========");
    for pool in [&uniswap, &sushi] {
        println!("\nVenue: {}", pool.venue);
        println!("Pair: {}", pool.address);
        println!(
            "USDC reserve: {}",
            format_units(pool.usdc_reserve, usdc_decimals)
        );
        println!(
            "WETH reserve: {}",
            format_units(pool.weth_reserve, weth_decimals)
        );
    }

    println!("\n==================================================");
    println!("ROUTE COMPARISON A — INPUT: 1 WETH");
    println!("==================================================");
    let one_weth = parse_units("1", weth_decimals)?;
    let uni_usdc = amount_out(one_weth, uniswap.weth_reserve, uniswap.usdc_reserve)?;
    let sushi_usdc = amount_out(one_weth, sushi.weth_reserve, sushi.usdc_reserve)?;
    println!(
        "Uniswap V2 output: {} USDC",
        format_units(uni_usdc, usdc_decimals)
    );
    println!(
        "SushiSwap V2 output: {} USDC",
        format_units(sushi_usdc, usdc_decimals)
    );
    if uni_usdc > sushi_usdc {
        println!("Higher quoted output: Uniswap V2");
        println!(
            "Gross output difference: {} USDC",
            format_units(uni_usdc - sushi_usdc, usdc_decimals)
        );
    } else if sushi_usdc > uni_usdc {
        println!("Higher quoted output: SushiSwap V2");
        println!(
            "Gross output difference: {} USDC",
            format_units(sushi_usdc - uni_usdc, usdc_decimals)
        );
    } else {
        println!("Quoted outputs are equal at raw token precision.");
        println!("Gross output difference: 0 USDC");
    }

    println!("\n==================================================");
    println!("ROUTE COMPARISON B — INPUT: 1000 USDC");
    println!("==================================================");
    let one_thousand_usdc = parse_units("1000", usdc_decimals)?;
    let uni_weth = amount_out(
        one_thousand_usdc,
        uniswap.usdc_reserve,
        uniswap.weth_reserve,
    )?;
    let sushi_weth = amount_out(one_thousand_usdc, sushi.usdc_reserve, sushi.weth_reserve)?;
    println!(
        "Uniswap V2 output: {} WETH",
        format_units(uni_weth, weth_decimals)
    );
    println!(
        "SushiSwap V2 output: {} WETH",
        format_units(sushi_weth, weth_decimals)
    );
    if uni_weth > sushi_weth {
        println!("Higher quoted output: Uniswap V2");
        println!(
            "Gross output difference: {} WETH",
            format_units(uni_weth - sushi_weth, weth_decimals)
        );
    } else if sushi_weth > uni_weth {
        println!("Higher quoted output: SushiSwap V2");
        println!(
            "Gross output difference: {} WETH",
            format_units(sushi_weth - uni_weth, weth_decimals)
        );
    } else {
        println!("Quoted outputs are equal at raw token precision.");
        println!("Gross output difference: 0 WETH");
    }

    println!("\n==================================================");
    println!("COMPARISON COMPLETED");
    println!("Pool data: READ-ONLY");
    println!("Quotes: THEORETICAL");
    println!("Gas costs: NOT YET DEDUCTED");
    println!("Net arbitrage profitability: NOT YET DETERMINED");
    println!("Wallet signing: DISABLED");
    println!("Transaction execution: DISABLED");
    println!("Warning: output differences alone are not proof of arbitrage profit.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_arguments_are_abi_padded() {
        let encoded = encode_address(USDC).unwrap();
        assert_eq!(encoded.len(), 64);
        assert!(encoded.ends_with("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"));
    }

    #[test]
    fn decimal_units_are_parsed_exactly() {
        assert_eq!(parse_units("1", 18).unwrap(), 1_000_000_000_000_000_000);
        assert_eq!(parse_units("1000", 6).unwrap(), 1_000_000_000);
        assert_eq!(parse_units("1.25", 6).unwrap(), 1_250_000);
    }

    #[test]
    fn too_many_decimal_places_are_rejected() {
        assert!(parse_units("1.1234567", 6).is_err());
    }

    #[test]
    fn v2_quote_matches_reference_example() {
        assert_eq!(amount_out(1000, 10000, 20000).unwrap(), 1813);
    }

    #[test]
    fn zero_input_or_reserves_are_rejected() {
        assert!(amount_out(0, 10000, 20000).is_err());
        assert!(amount_out(1000, 0, 20000).is_err());
        assert!(amount_out(1000, 10000, 0).is_err());
    }
}
