use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::env;
use std::error::Error;
use std::time::Duration;

// Uniswap V2 WETH/USDC pool on Ethereum Mainnet.
const POOL_ADDRESS: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

const USDC_ADDRESS: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

const WETH_ADDRESS: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

// Uniswap V2 contract function selectors.
const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";

// ERC-20 decimals() selector.
const DECIMALS_SELECTOR: &str = "0x313ce567";

// Standard Uniswap V2 fee parameters:
// 0.30% fee => 997 / 1000 of the input is used in the formula.
const FEE_NUMERATOR: u128 = 997;
const FEE_DENOMINATOR: u128 = 1000;

struct RpcClient {
    client: Client,
    endpoint: String,
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
                {
                    "to": address,
                    "data": data
                },
                block_tag
            ]),
        )?;

        result
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "eth_call returned a non-string result".into())
    }
}

struct PoolState {
    token0: String,
    token1: String,

    decimals0: u32,
    decimals1: u32,

    reserve0: u128,
    reserve1: u128,

    block_number: u64,
}

struct SwapQuote {
    amount_in: u128,
    amount_out: u128,

    // Output at the reserve ratio, before fees or curve impact.
    spot_output: u128,

    // Output using the constant-product curve without a fee.
    no_fee_output: u128,

    // Difference between the no-fee curve output and spot output.
    // Price impact in hundredths of a basis point (1 bp = 100 units).
    price_impact_bps_x100: u128,

    // Difference between no-fee curve output and actual output.
    fee_cost: u128,
}

fn parse_hex_quantity(value: &Value) -> Result<u64, Box<dyn Error>> {
    let hex = value.as_str().ok_or("Expected hexadecimal RPC quantity")?;

    let digits = hex.strip_prefix("0x").ok_or("Missing 0x prefix")?;

    if digits.is_empty() {
        return Ok(0);
    }

    Ok(u64::from_str_radix(digits, 16)?)
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
    Ok(format!("0x{}", &word[24..]))
}

fn decode_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> {
    let word = decode_word(data, index)?;
    Ok(u128::from_str_radix(word, 16)?)
}

/// Convert a human-readable decimal amount into raw token units.
///
/// Examples:
/// parse_units("1", 18)       => 1 WETH in wei
/// parse_units("1000", 6)     => 1000 USDC in raw units
/// parse_units("1.25", 6)     => 1.25 USDC in raw units
///
/// Excess decimal places are rejected instead of silently rounded.
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

/// Display raw token units as a decimal string.
///
/// This function avoids floating-point arithmetic when formatting amounts.
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

/// Calculate Uniswap V2 output using integer arithmetic.
///
/// amount_out =
///     (amount_in * 997 * reserve_out)
///     / (reserve_in * 1000 + amount_in * 997)
///
/// The calculation uses checked arithmetic so overflow is rejected.
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
        .checked_mul(FEE_NUMERATOR)
        .ok_or("Input fee calculation overflow")?;

    let numerator = amount_with_fee
        .checked_mul(reserve_out)
        .ok_or("Swap numerator overflow")?;

    let reserve_term = reserve_in
        .checked_mul(FEE_DENOMINATOR)
        .ok_or("Swap denominator overflow")?;

    let denominator = reserve_term
        .checked_add(amount_with_fee)
        .ok_or("Swap denominator overflow")?;

    let amount_out = numerator / denominator;

    if amount_out == 0 {
        return Err("Output rounds to zero raw token units.".into());
    }

    if amount_out >= reserve_out {
        return Err("Output would exhaust the pool reserve.".into());
    }

    Ok(amount_out)
}

/// Calculate the output at the current reserve ratio.
///
/// This is a reference value, not an executable swap quote.
fn get_spot_output(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, Box<dyn Error>> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err("Pool reserves must be non-zero.".into());
    }

    let numerator = amount_in
        .checked_mul(reserve_out)
        .ok_or("Spot-price calculation overflow")?;

    Ok(numerator / reserve_in)
}

/// Calculate constant-product output without charging a fee.
///
/// This isolates curve price impact from the pool fee.
fn get_no_fee_output(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, Box<dyn Error>> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return Err("Input and reserves must be non-zero.".into());
    }

    let numerator = amount_in
        .checked_mul(reserve_out)
        .ok_or("No-fee quote numerator overflow")?;

    let denominator = reserve_in
        .checked_add(amount_in)
        .ok_or("No-fee quote denominator overflow")?;

    Ok(numerator / denominator)
}

fn calculate_quote(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<SwapQuote, Box<dyn Error>> {
    let amount_out = get_amount_out(amount_in, reserve_in, reserve_out)?;

    let spot_output = get_spot_output(amount_in, reserve_in, reserve_out)?;

    let no_fee_output = get_no_fee_output(amount_in, reserve_in, reserve_out)?;

    // Keep two decimal places of basis-point precision. The previous integer
    // calculation truncated sub-1-bp impacts to zero. This is display/reporting
    // precision only; swap amounts remain integer token units.
    let price_impact_bps_x100 = if spot_output == 0 {
        0
    } else {
        let difference = spot_output.saturating_sub(no_fee_output);

        difference
            .checked_mul(1_000_000)
            .ok_or("Price-impact calculation overflow")?
            / spot_output
    };

    let fee_cost = no_fee_output.saturating_sub(amount_out);

    Ok(SwapQuote {
        amount_in,
        amount_out,
        spot_output,
        no_fee_output,
        price_impact_bps_x100,
        fee_cost,
    })
}

fn load_pool(rpc: &RpcClient) -> Result<PoolState, Box<dyn Error>> {
    // Verify Ethereum Mainnet.
    let chain_id = parse_hex_quantity(&rpc.call("eth_chainId", json!([]))?)?;

    if chain_id != 1 {
        return Err(format!("Expected Ethereum Mainnet chain ID 1, got {chain_id}.").into());
    }

    // Pin every contract read to the same block.
    let block_number = parse_hex_quantity(&rpc.call("eth_blockNumber", json!([]))?)?;

    let block_tag = format!("0x{block_number:x}");

    // Read the pair's token addresses.
    let token0 = decode_address(&rpc.eth_call(POOL_ADDRESS, TOKEN0_SELECTOR, &block_tag)?)?;

    let token1 = decode_address(&rpc.eth_call(POOL_ADDRESS, TOKEN1_SELECTOR, &block_tag)?)?;

    // Confirm this is the expected USDC/WETH pair.
    let contains_usdc =
        token0.eq_ignore_ascii_case(USDC_ADDRESS) || token1.eq_ignore_ascii_case(USDC_ADDRESS);

    let contains_weth =
        token0.eq_ignore_ascii_case(WETH_ADDRESS) || token1.eq_ignore_ascii_case(WETH_ADDRESS);

    if !contains_usdc || !contains_weth {
        return Err("Pool does not contain both expected USDC and WETH tokens.".into());
    }

    if token0.eq_ignore_ascii_case(&token1) {
        return Err("Pool returned identical token addresses.".into());
    }

    // Read reserves at the same block.
    let reserves_data = rpc.eth_call(POOL_ADDRESS, RESERVES_SELECTOR, &block_tag)?;

    let reserve0 = decode_u128(&reserves_data, 0)?;
    let reserve1 = decode_u128(&reserves_data, 1)?;

    if reserve0 == 0 || reserve1 == 0 {
        return Err("Pool contains a zero reserve.".into());
    }

    // Read decimals at the same block.
    let decimals0_raw = decode_u128(&rpc.eth_call(&token0, DECIMALS_SELECTOR, &block_tag)?, 0)?;

    let decimals1_raw = decode_u128(&rpc.eth_call(&token1, DECIMALS_SELECTOR, &block_tag)?, 0)?;

    if decimals0_raw > 38 || decimals1_raw > 38 {
        return Err("Token decimals exceed the supported range.".into());
    }

    let decimals0 = decimals0_raw as u32;
    let decimals1 = decimals1_raw as u32;

    Ok(PoolState {
        token0,
        token1,
        decimals0,
        decimals1,
        reserve0,
        reserve1,
        block_number,
    })
}

fn print_quote(
    title: &str,
    input_symbol: &str,
    output_symbol: &str,
    input_decimals: u32,
    output_decimals: u32,
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<(), Box<dyn Error>> {
    let quote = calculate_quote(amount_in, reserve_in, reserve_out)?;

    println!("\n==================================================");
    println!("{title}");
    println!("==================================================");

    println!(
        "Input: {} {input_symbol}",
        format_units(quote.amount_in, input_decimals)
    );

    println!(
        "Expected output: {} {output_symbol}",
        format_units(quote.amount_out, output_decimals)
    );

    println!(
        "Spot-ratio reference: {} {output_symbol}",
        format_units(quote.spot_output, output_decimals)
    );

    println!(
        "No-fee curve output: {} {output_symbol}",
        format_units(quote.no_fee_output, output_decimals)
    );

    println!(
        "Estimated pool-fee effect: {} {output_symbol}",
        format_units(quote.fee_cost, output_decimals)
    );

    println!(
        "Curve price impact: {} basis points ({:.4}%)",
        format!(
            "{}.{:02}",
            quote.price_impact_bps_x100 / 100,
            quote.price_impact_bps_x100 % 100
        ),
        quote.price_impact_bps_x100 as f64 / 10_000.0
    );

    println!("Pool fee assumption: 0.30%");

    println!("Note: output is a theoretical quote from one block's reserves.");

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!("       CRYPTO ARBITRAGE ENGINE — STAGE 3.3");
    println!("       LIVE AMM QUOTE ENGINE");
    println!("==================================================");

    // Load the RPC endpoint from the environment.
    let endpoint = env::var("ETH_RPC_URL")
        .map_err(|_| "ETH_RPC_URL is missing. Set your Ethereum RPC endpoint.")?;

    let rpc = RpcClient::new(endpoint)?;

    println!("\nConnecting to Ethereum Mainnet...");

    // Load and validate live pool state.
    let pool = load_pool(&rpc)?;

    println!("Network: Ethereum Mainnet");
    println!("Pool: {POOL_ADDRESS}");
    println!("Block: {}", pool.block_number);

    println!("\nToken 0: {}", pool.token0);
    println!("Token 1: {}", pool.token1);

    // Orient the reserves by token address rather than assuming
    // which token is token0.
    let (usdc_reserve, weth_reserve) = if pool.token0.eq_ignore_ascii_case(USDC_ADDRESS) {
        (pool.reserve0, pool.reserve1)
    } else {
        (pool.reserve1, pool.reserve0)
    };

    let (usdc_decimals, weth_decimals) = if pool.token0.eq_ignore_ascii_case(USDC_ADDRESS) {
        (pool.decimals0, pool.decimals1)
    } else {
        (pool.decimals1, pool.decimals0)
    };

    println!("\n========== LIVE POOL STATE ==========");

    println!(
        "USDC reserve: {}",
        format_units(usdc_reserve, usdc_decimals)
    );

    println!(
        "WETH reserve: {}",
        format_units(weth_reserve, weth_decimals)
    );

    // Example A: quote 1 WETH -> USDC.
    let one_weth = parse_units("1", weth_decimals)?;

    print_quote(
        "QUOTE A: WETH -> USDC",
        "WETH",
        "USDC",
        weth_decimals,
        usdc_decimals,
        one_weth,
        weth_reserve,
        usdc_reserve,
    )?;

    // Example B: quote 1,000 USDC -> WETH.
    let one_thousand_usdc = parse_units("1000", usdc_decimals)?;

    print_quote(
        "QUOTE B: USDC -> WETH",
        "USDC",
        "WETH",
        usdc_decimals,
        weth_decimals,
        one_thousand_usdc,
        usdc_reserve,
        weth_reserve,
    )?;

    println!("\n==================================================");
    println!("QUOTE ENGINE COMPLETED");
    println!("==================================================");

    println!("Live pool data: READ");
    println!("Swap quotes: CALCULATED");
    println!("Wallet signing: DISABLED");
    println!("Transaction execution: DISABLED");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniswap_v2_quote_matches_reference_example() {
        // Reference example:
        // amountIn = 1000
        // reserveIn = 10000
        // reserveOut = 20000
        //
        // Expected:
        // floor((1000 * 997 * 20000) /
        //       (10000 * 1000 + 1000 * 997))
        // = 1813

        let result = get_amount_out(1000, 10000, 20000).unwrap();

        assert_eq!(result, 1813);
    }

    #[test]
    fn zero_input_is_rejected() {
        assert!(get_amount_out(0, 10000, 20000).is_err());
    }

    #[test]
    fn zero_reserve_is_rejected() {
        assert!(get_amount_out(1000, 0, 20000).is_err());

        assert!(get_amount_out(1000, 10000, 0).is_err());
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
    fn price_impact_retains_sub_basis_point_precision() {
        // A small trade can have a curve impact below one basis point.
        // The report should preserve hundredths of a basis point instead of
        // truncating the displayed value to zero.
        let quote = calculate_quote(1_000_000, 1_000_000_000_000, 2_000_000_000_000).unwrap();
        assert!(quote.price_impact_bps_x100 > 0);
    }

    #[test]
    fn quote_output_is_less_than_spot_output() {
        let quote = calculate_quote(1_000_000, 10_000_000, 20_000_000).unwrap();

        assert!(quote.amount_out < quote.spot_output);
    }
}
