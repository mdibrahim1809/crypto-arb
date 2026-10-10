use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::env;
use std::error::Error;
use std::time::Duration;

struct RpcClient {
    client: Client,
    endpoint: String,
}

impl RpcClient {
    fn new(endpoint: String) -> Result<Self, Box<dyn Error>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self { client, endpoint })
    }

    fn call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn Error>> {
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
            return Err(
                format!("RPC error for {method}: {error}").into()
            );
        }

        body.get("result")
            .cloned()
            .ok_or_else(|| {
                format!("RPC response has no result for {method}").into()
            })
    }
}

fn parse_hex_quantity(value: &Value) -> Result<u128, Box<dyn Error>> {
    let hex = value
        .as_str()
        .ok_or("Expected a hexadecimal RPC quantity")?;

    let digits = hex
        .strip_prefix("0x")
        .ok_or("RPC quantity does not start with 0x")?;

    if digits.is_empty() {
        return Ok(0);
    }

    Ok(u128::from_str_radix(digits, 16)?)
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("==================================================");
    println!("       CRYPTO ARBITRAGE ENGINE — STAGE 3");
    println!("       Live Blockchain RPC Connectivity");
    println!("==================================================");

    let endpoint = env::var("ETH_RPC_URL")
        .map_err(|_| {
            "ETH_RPC_URL is missing. Set it to your Ethereum RPC endpoint."
        })?;

    let rpc = RpcClient::new(endpoint)?;

    println!("\nConnecting to Ethereum RPC...");

    // Identify the connected blockchain.
    let chain_id = rpc.call("eth_chainId", json!([]))?;

    let chain_id_number = parse_hex_quantity(&chain_id)?;

    println!("Chain ID: {chain_id_number}");

    if chain_id_number != 1 {
        println!(
            "WARNING: This is not Ethereum Mainnet. \
             Continue only if this network is intentional."
        );
    }

    // Read the latest block number.
    let block = rpc.call("eth_blockNumber", json!([]))?;

    let block_number = parse_hex_quantity(&block)?;

    println!("Latest block: {block_number}");

    // Read the current suggested gas price.
    let gas_price = rpc.call("eth_gasPrice", json!([]))?;

    let gas_price_wei = parse_hex_quantity(&gas_price)?;

    let gas_price_gwei =
        gas_price_wei as f64 / 1_000_000_000.0;

    println!("Gas price: {:.4} Gwei", gas_price_gwei);

    println!("\n==================================================");
    println!("RPC CONNECTION SUCCESSFUL");
    println!("==================================================");

    println!("Live blockchain data is available.");
    println!("Transaction execution: DISABLED");
    println!("Wallet signing: DISABLED");

    Ok(())
}