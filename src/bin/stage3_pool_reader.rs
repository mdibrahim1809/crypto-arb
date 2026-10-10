use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::env;
use std::error::Error;
use std::time::Duration;

const POOL_ADDRESS: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

// Uniswap V2 function selectors.
const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";

// ERC-20 decimals() selector.
const DECIMALS_SELECTOR: &str = "0x313ce567";

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

    let start = index.checked_mul(64).ok_or("ABI word offset overflow")?;

    let end = start.checked_add(64).ok_or("ABI word offset overflow")?;

    if hex.len() < end {
        return Err(format!(
            "ABI response too short for word {index}: \
                 expected at least {end} hex characters, \
                 received {}",
            hex.len()
        )
        .into());
    }

    Ok(&hex[start..end])
}

fn decode_address(data: &str) -> Result<String, Box<dyn Error>> {
    let word = decode_word(data, 0)?;

    // An ABI-encoded address occupies the final 20 bytes.
    Ok(format!("0x{}", &word[24..]))
}

fn decode_u128(data: &str, index: usize) -> Result<u128, Box<dyn Error>> {
    let word = decode_word(data, index)?;

    Ok(u128::from_str_radix(word, 16)?)
}

fn format_units(raw: u128, decimals: u32) -> f64 {
    raw as f64 / 10_f64.powi(decimals as i32)
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!("       CRYPTO ARBITRAGE ENGINE — STAGE 3.2");
    println!("       UNISWAP V2 LIVE POOL READER");
    println!("==================================================");

    // --------------------------------------------------
    // 1. Connect to Ethereum RPC.
    // --------------------------------------------------

    let endpoint = env::var("ETH_RPC_URL")
        .map_err(|_| "ETH_RPC_URL is missing. Set your Ethereum RPC endpoint.")?;

    let rpc = RpcClient::new(endpoint)?;

    println!("\nConnecting to Ethereum RPC...");

    // --------------------------------------------------
    // 2. Verify Ethereum Mainnet.
    // --------------------------------------------------

    let chain_id = parse_hex_quantity(&rpc.call("eth_chainId", json!([]))?)?;

    if chain_id != 1 {
        return Err(format!(
            "Expected Ethereum Mainnet chain ID 1, \
                 received {chain_id}. Aborting."
        )
        .into());
    }

    // --------------------------------------------------
    // 3. Pin all contract reads to the same block.
    // --------------------------------------------------

    let block_number = parse_hex_quantity(&rpc.call("eth_blockNumber", json!([]))?)?;

    let block_tag = format!("0x{block_number:x}");

    println!("Network: Ethereum Mainnet");
    println!("Pinned block: {block_number}");
    println!("Reading Uniswap V2 pool...");

    // --------------------------------------------------
    // 4. Read token addresses from the pair contract.
    // --------------------------------------------------

    let token0_data = rpc.eth_call(POOL_ADDRESS, TOKEN0_SELECTOR, &block_tag)?;

    let token1_data = rpc.eth_call(POOL_ADDRESS, TOKEN1_SELECTOR, &block_tag)?;

    let token0 = decode_address(&token0_data)?;
    let token1 = decode_address(&token1_data)?;

    // --------------------------------------------------
    // 5. Read reserves and the last-update timestamp.
    // --------------------------------------------------

    let reserves_data = rpc.eth_call(POOL_ADDRESS, RESERVES_SELECTOR, &block_tag)?;

    let reserve0 = decode_u128(&reserves_data, 0)?;
    let reserve1 = decode_u128(&reserves_data, 1)?;
    let timestamp = decode_u128(&reserves_data, 2)?;

    // --------------------------------------------------
    // 6. Read decimals from each ERC-20 token contract.
    // --------------------------------------------------

    let decimals0_data = rpc.eth_call(&token0, DECIMALS_SELECTOR, &block_tag)?;

    let decimals1_data = rpc.eth_call(&token1, DECIMALS_SELECTOR, &block_tag)?;

    let decimals0_raw = decode_u128(&decimals0_data, 0)?;
    let decimals1_raw = decode_u128(&decimals1_data, 0)?;

    // ERC-20 decimals() returns uint8.
    if decimals0_raw > u8::MAX as u128 || decimals1_raw > u8::MAX as u128 {
        return Err("Invalid token decimals response: value exceeds uint8.".into());
    }

    let decimals0 = decimals0_raw as u32;
    let decimals1 = decimals1_raw as u32;

    // Prevent unreasonable exponents and invalid display calculations.
    if decimals0 > 38 || decimals1 > 38 {
        return Err("Token decimals exceed the supported display range.".into());
    }

    // --------------------------------------------------
    // 7. Validate the pool responses.
    // --------------------------------------------------

    if token0.eq_ignore_ascii_case(&token1) {
        return Err("Invalid pool: token0 and token1 are identical.".into());
    }

    if reserve0 == 0 || reserve1 == 0 {
        return Err("Pool has a zero reserve. Rejecting unusable pool data.".into());
    }

    // --------------------------------------------------
    // 8. Convert raw reserves into human-readable units.
    // --------------------------------------------------

    let human_reserve0 = format_units(reserve0, decimals0);

    let human_reserve1 = format_units(reserve1, decimals1);

    // Reserve ratio: units of token0 per token1.
    let price_token1_in_token0 = human_reserve0 / human_reserve1;

    // Inverse reserve ratio: units of token1 per token0.
    let price_token0_in_token1 = human_reserve1 / human_reserve0;

    // --------------------------------------------------
    // 9. Display raw on-chain data.
    // --------------------------------------------------

    println!("\n========== LIVE POOL DATA ==========");

    println!("Pool address: {POOL_ADDRESS}");
    println!("Token 0 address: {token0}");
    println!("Token 1 address: {token1}");

    println!("\nRaw reserve 0: {reserve0}");
    println!("Raw reserve 1: {reserve1}");

    println!("Last reserve update timestamp: {timestamp}");

    // --------------------------------------------------
    // 10. Display human-readable reserves.
    // --------------------------------------------------

    println!("\n========== HUMAN-READABLE RESERVES ==========");

    println!("Token 0 decimals: {decimals0}");
    println!("Token 1 decimals: {decimals1}");

    println!("Token 0 reserve: {:.6}", human_reserve0);

    println!("Token 1 reserve: {:.8}", human_reserve1);

    // --------------------------------------------------
    // 11. Display preliminary reserve-ratio prices.
    // --------------------------------------------------

    println!("\n========== RESERVE-RATIO PRICES ==========");

    println!("Token 0 per token 1: {:.6}", price_token1_in_token0);

    println!("Token 1 per token 0: {:.12}", price_token0_in_token1);

    println!(
        "\nPrice interpretation: units of token 0 \
         required for one token 1."
    );

    println!(
        "These prices are reserve ratios, not guaranteed \
         swap execution prices."
    );

    // --------------------------------------------------
    // 12. Final validation report.
    // --------------------------------------------------

    println!("\n========== VALIDATION ==========");

    println!("Network check: PASSED");
    println!("ABI response decoding: PASSED");
    println!("Token address check: PASSED");
    println!("Non-zero reserve check: PASSED");
    println!("Token decimals decoding: PASSED");
    println!("Block consistency: PASSED");

    println!("\n========== SECURITY ==========");

    println!("Transaction execution: DISABLED");
    println!("Wallet signing: DISABLED");
    println!("Private key access: NOT USED");

    println!("\nSTAGE 3.2 POOL READER COMPLETE.");

    Ok(())
}
