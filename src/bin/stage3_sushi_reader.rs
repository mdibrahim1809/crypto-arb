use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::env;
use std::error::Error;
use std::time::Duration;

// Ethereum Mainnet token contracts.
const USDC_ADDRESS: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH_ADDRESS: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

// SushiSwap V2 factory on Ethereum Mainnet.
// The pair address is discovered from the factory at runtime, not hard-coded.
const SUSHI_FACTORY_ADDRESS: &str = "0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac";

// Function selectors.
const GET_PAIR_SELECTOR: &str = "e6a43905"; // getPair(address,address)
const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";
const DECIMALS_SELECTOR: &str = "0x313ce567";

struct RpcClient {
    client: Client,
    endpoint: String,
}

struct PoolState {
    pair_address: String,
    token0: String,
    token1: String,
    decimals0: u32,
    decimals1: u32,
    reserve0: u128,
    reserve1: u128,
    block_number: u64,
}

impl RpcClient {
    fn new(endpoint: String) -> Result<Self, Box<dyn Error>> {
        let client = Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Self { client, endpoint })
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
        let result = self.call(
            "eth_call",
            json!([
                { "to": address, "data": data },
                block_tag
            ]),
        )?;

        result
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "eth_call returned a non-string result".into())
    }
}

fn parse_hex_quantity(value: &Value) -> Result<u64, Box<dyn Error>> {
    let hex = value
        .as_str()
        .ok_or("Expected hexadecimal RPC quantity")?
        .strip_prefix("0x")
        .ok_or("Missing 0x prefix")?;

    if hex.is_empty() {
        return Ok(0);
    }
    Ok(u64::from_str_radix(hex, 16)?)
}

fn decode_word(data: &str, index: usize) -> Result<&str, Box<dyn Error>> {
    let hex = data
        .strip_prefix("0x")
        .ok_or("ABI response missing 0x prefix")?;
    let start = index.checked_mul(64).ok_or("ABI offset overflow")?;
    let end = start.checked_add(64).ok_or("ABI offset overflow")?;
    if hex.len() < end {
        return Err(format!("ABI response too short for word {index}").into());
    }
    Ok(&hex[start..end])
}

fn decode_address(data: &str) -> Result<String, Box<dyn Error>> {
    let word = decode_word(data, 0)?;
    Ok(format!("0x{}", &word[24..]).to_ascii_lowercase())
}

fn decode_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> {
    Ok(u128::from_str_radix(decode_word(data, index)?, 16)?)
}

fn encode_address_argument(address: &str) -> Result<String, Box<dyn Error>> {
    let hex = address
        .strip_prefix("0x")
        .ok_or("Address is missing 0x prefix")?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("Invalid Ethereum address: {address}").into());
    }
    Ok(format!("{hex:0>64}").to_ascii_lowercase())
}

fn format_units(raw: u128, decimals: u32) -> String {
    if decimals == 0 {
        return raw.to_string();
    }

    let scale = 10_u128.pow(decimals);
    let whole = raw / scale;
    let fraction = raw % scale;
    let fraction_string = format!("{:0width$}", fraction, width = decimals as usize);
    let trimmed = fraction_string.trim_end_matches('0');

    if trimmed.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{trimmed}")
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
        return Err("Amount contains multiple decimal points.".into());
    }
    if whole.is_empty() {
        return Err("Enter a leading zero, e.g. 0.5.".into());
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Amount must contain decimal digits only.".into());
    }
    if fraction.len() > decimals as usize {
        return Err(format!("Amount has more than {decimals} decimal places.").into());
    }

    let scale = 10_u128
        .checked_pow(decimals)
        .ok_or("Token decimal scale overflow")?;
    let whole_value: u128 = whole.parse()?;
    let whole_raw = whole_value.checked_mul(scale).ok_or("Amount overflow")?;
    let padded_fraction = format!("{:0<width$}", fraction, width = decimals as usize);
    let fraction_raw: u128 = if padded_fraction.is_empty() {
        0
    } else {
        padded_fraction.parse()?
    };
    let amount = whole_raw
        .checked_add(fraction_raw)
        .ok_or("Amount overflow")?;
    if amount == 0 {
        return Err("Amount must be greater than zero.".into());
    }
    Ok(amount)
}

// SushiSwap V2 uses the Uniswap V2-style 0.30% swap fee on this deployment.
// This is a theoretical quote, not a transaction or guarantee of execution.
fn get_amount_out(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, Box<dyn Error>> {
    if amount_in == 0 {
        return Err("Input amount must be greater than zero.".into());
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err("Pool reserves must both be non-zero.".into());
    }

    let amount_with_fee = amount_in
        .checked_mul(997)
        .ok_or("Fee calculation overflow")?;
    let numerator = amount_with_fee
        .checked_mul(reserve_out)
        .ok_or("Quote numerator overflow")?;
    let denominator = reserve_in
        .checked_mul(1000)
        .and_then(|v| v.checked_add(amount_with_fee))
        .ok_or("Quote denominator overflow")?;
    let output = numerator / denominator;

    if output == 0 || output >= reserve_out {
        return Err("Quote output is zero or would exhaust the reserve.".into());
    }
    Ok(output)
}

fn load_sushi_pool(rpc: &RpcClient) -> Result<PoolState, Box<dyn Error>> {
    let chain_id = parse_hex_quantity(&rpc.call("eth_chainId", json!([]))?)?;
    if chain_id != 1 {
        return Err(format!("Expected Ethereum Mainnet chain ID 1, got {chain_id}.").into());
    }

    // Pin factory lookup and every pool read to one block.
    let block_number = parse_hex_quantity(&rpc.call("eth_blockNumber", json!([]))?)?;
    let block_tag = format!("0x{block_number:x}");

    let token_a = encode_address_argument(USDC_ADDRESS)?;
    let token_b = encode_address_argument(WETH_ADDRESS)?;
    let get_pair_call = format!("0x{GET_PAIR_SELECTOR}{token_a}{token_b}");

    let pair_address =
        decode_address(&rpc.eth_call(SUSHI_FACTORY_ADDRESS, &get_pair_call, &block_tag)?)?;

    if pair_address == "0x0000000000000000000000000000000000000000" {
        return Err("SushiSwap factory returned no WETH/USDC pair.".into());
    }

    let token0 = decode_address(&rpc.eth_call(&pair_address, TOKEN0_SELECTOR, &block_tag)?)?;
    let token1 = decode_address(&rpc.eth_call(&pair_address, TOKEN1_SELECTOR, &block_tag)?)?;

    let contains_usdc =
        token0.eq_ignore_ascii_case(USDC_ADDRESS) || token1.eq_ignore_ascii_case(USDC_ADDRESS);
    let contains_weth =
        token0.eq_ignore_ascii_case(WETH_ADDRESS) || token1.eq_ignore_ascii_case(WETH_ADDRESS);

    if !contains_usdc || !contains_weth || token0.eq_ignore_ascii_case(&token1) {
        return Err("Discovered SushiSwap pair failed token validation.".into());
    }

    let reserves_data = rpc.eth_call(&pair_address, RESERVES_SELECTOR, &block_tag)?;
    let reserve0 = decode_u128(&reserves_data, 0)?;
    let reserve1 = decode_u128(&reserves_data, 1)?;
    if reserve0 == 0 || reserve1 == 0 {
        return Err("SushiSwap pool contains a zero reserve.".into());
    }

    let decimals0_raw = decode_u128(&rpc.eth_call(&token0, DECIMALS_SELECTOR, &block_tag)?, 0)?;
    let decimals1_raw = decode_u128(&rpc.eth_call(&token1, DECIMALS_SELECTOR, &block_tag)?, 0)?;

    // Keep scaling safely within u128 formatting/parsing operations.
    if decimals0_raw > 38 || decimals1_raw > 38 {
        return Err("Token decimals exceed the supported range.".into());
    }

    Ok(PoolState {
        pair_address,
        token0,
        token1,
        decimals0: decimals0_raw as u32,
        decimals1: decimals1_raw as u32,
        reserve0,
        reserve1,
        block_number,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!("       CRYPTO ARBITRAGE ENGINE — STAGE 3.4");
    println!("       SUSHISWAP V2 POOL READER");
    println!("==================================================");

    let endpoint = env::var("ETH_RPC_URL")
        .map_err(|_| "ETH_RPC_URL is missing. Set your Ethereum RPC endpoint.")?;
    let rpc = RpcClient::new(endpoint)?;

    println!("\nConnecting to Ethereum Mainnet...");
    let pool = load_sushi_pool(&rpc)?;

    println!("Network: Ethereum Mainnet");
    println!("Venue: SushiSwap V2");
    println!("Factory: {SUSHI_FACTORY_ADDRESS}");
    println!("Discovered WETH/USDC pair: {}", pool.pair_address);
    println!("Block: {}", pool.block_number);
    println!("Token 0: {}", pool.token0);
    println!("Token 1: {}", pool.token1);

    let (usdc_reserve, weth_reserve, usdc_decimals, weth_decimals) =
        if pool.token0.eq_ignore_ascii_case(USDC_ADDRESS) {
            (pool.reserve0, pool.reserve1, pool.decimals0, pool.decimals1)
        } else {
            (pool.reserve1, pool.reserve0, pool.decimals1, pool.decimals0)
        };

    println!("\n========== SUSHISWAP LIVE POOL STATE ==========");
    println!(
        "USDC reserve: {}",
        format_units(usdc_reserve, usdc_decimals)
    );
    println!(
        "WETH reserve: {}",
        format_units(weth_reserve, weth_decimals)
    );

    let one_weth = parse_units("1", weth_decimals)?;
    let weth_to_usdc = get_amount_out(one_weth, weth_reserve, usdc_reserve)?;
    println!("\n========== THEORETICAL QUOTE A ==========");
    println!("Input: 1 WETH");
    println!(
        "Expected output: {} USDC",
        format_units(weth_to_usdc, usdc_decimals)
    );

    let one_thousand_usdc = parse_units("1000", usdc_decimals)?;
    let usdc_to_weth = get_amount_out(one_thousand_usdc, usdc_reserve, weth_reserve)?;
    println!("\n========== THEORETICAL QUOTE B ==========");
    println!("Input: 1000 USDC");
    println!(
        "Expected output: {} WETH",
        format_units(usdc_to_weth, weth_decimals)
    );

    println!("\n==================================================");
    println!("SUSHISWAP POOL READER COMPLETED");
    println!("Pool discovery: READ-ONLY");
    println!("Pool reserves: READ");
    println!("Swap quotes: CALCULATED");
    println!("Wallet signing: DISABLED");
    println!("Transaction execution: DISABLED");
    println!("Note: quotes are theoretical and exclude gas and execution risk.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_arguments_are_abi_padded() {
        let encoded = encode_address_argument(USDC_ADDRESS).unwrap();
        assert_eq!(encoded.len(), 64);
        assert!(encoded.ends_with("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"));
    }

    #[test]
    fn token_amounts_are_parsed_exactly() {
        assert_eq!(parse_units("1", 18).unwrap(), 1_000_000_000_000_000_000);
        assert_eq!(parse_units("1000", 6).unwrap(), 1_000_000_000);
        assert_eq!(parse_units("1.25", 6).unwrap(), 1_250_000);
    }

    #[test]
    fn excessive_decimal_places_are_rejected() {
        assert!(parse_units("1.1234567", 6).is_err());
    }

    #[test]
    fn sushi_v2_quote_matches_constant_product_reference() {
        assert_eq!(get_amount_out(1000, 10000, 20000).unwrap(), 1813);
    }

    #[test]
    fn zero_input_and_reserves_are_rejected() {
        assert!(get_amount_out(0, 10000, 20000).is_err());
        assert!(get_amount_out(1000, 0, 20000).is_err());
        assert!(get_amount_out(1000, 10000, 0).is_err());
    }
}
