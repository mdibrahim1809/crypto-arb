use reqwest::blocking::Client;
use reqwest::header::RETRY_AFTER;
use serde_json::{json, Value};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const RESERVES_SELECTOR: &str = "0x0902f1ac";
const DECIMALS_SELECTOR: &str = "0x313ce567";
const DEFAULT_REGISTRY: &str = "pools_multitoken_stage4_5.csv";
const DEFAULT_OUTPUT: &str = "stage4_10_verified_market_data.csv";

#[derive(Debug, Clone)]
struct Pool {
    dex: String,
    address: String,
    fee_bps: u32,
}

#[derive(Debug, Clone)]
struct TokenInfo {
    address: String,
    decimals: u8,
}

#[derive(Debug, Clone)]
struct VerifiedPool {
    dex: String,
    pair_address: String,
    fee_bps: u32,
    token0: TokenInfo,
    token1: TokenInfo,
    reserve0_raw: u128,
    reserve1_raw: u128,
    reserve0_human: String,
    reserve1_human: String,
    block: u64,
    attempts: usize,
    elapsed_ms: u128,
    status: String,
    error: String,
}

#[derive(Debug)]
struct RpcError {
    message: String,
    retry_after: Option<Duration>,
    retryable: bool,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn normalize_address(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    let raw = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X"))
        .ok_or_else(|| format!("address lacks 0x prefix: {trimmed}"))?;
    if raw.len() != 40 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid 20-byte Ethereum address: {trimmed}"));
    }
    Ok(format!("0x{}", raw.to_ascii_lowercase()))
}

fn parse_hex_u128(value: &str) -> Result<u128, String> {
    let raw = value.strip_prefix("0x").ok_or_else(|| format!("missing 0x in hex value: {value}"))?;
    if raw.is_empty() {
        return Err("empty hexadecimal value".to_string());
    }
    u128::from_str_radix(raw, 16).map_err(|e| format!("invalid/oversized uint128 hex value {value}: {e}"))
}

fn parse_hex_u64(value: &str) -> Result<u64, String> {
    let raw = value.strip_prefix("0x").ok_or_else(|| format!("missing 0x in hex value: {value}"))?;
    u64::from_str_radix(raw, 16).map_err(|e| format!("invalid uint64 hex value {value}: {e}"))
}

fn word_address(data: &str) -> Result<String, String> {
    let raw = data.strip_prefix("0x").ok_or_else(|| "eth_call result lacks 0x prefix".to_string())?;
    if raw.len() < 64 {
        return Err(format!("ABI address response too short: {} hex chars", raw.len()));
    }
    let word = &raw[raw.len() - 64..];
    normalize_address(&format!("0x{}", &word[24..]))
}

fn human_amount(raw: u128, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let scale = 10u128.checked_pow(decimals as u32).unwrap_or(u128::MAX);
    let whole = raw / scale;
    let fractional = raw % scale;
    let mut fraction = format!("{:0width$}", fractional, width = decimals as usize);
    while fraction.ends_with('0') {
        fraction.pop();
    }
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds.min(60)));
    }
    // HTTP-date Retry-After is intentionally not parsed without an additional date dependency.
    None
}

fn backoff_ms(base_ms: u64, attempt: usize) -> u64 {
    let shift = attempt.saturating_sub(1).min(10) as u32;
    base_ms.saturating_mul(1u64 << shift).min(30_000)
}

struct RpcClient {
    client: Client,
    url: String,
    retries: usize,
    retry_base_ms: u64,
    next_id: u64,
}

impl RpcClient {
    fn new(url: String, retries: usize, retry_base_ms: u64) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| format!("failed to create HTTP client: {e}"))?;
        Ok(Self { client, url, retries, retry_base_ms, next_id: 1 })
    }

    fn call(&mut self, method: &str, params: Value) -> Result<(Value, usize), RpcError> {
        let mut last_error = RpcError {
            message: "RPC request did not run".to_string(),
            retry_after: None,
            retryable: true,
        };
        for attempt in 1..=self.retries.saturating_add(1) {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            let response = self.client.post(&self.url).json(&body).send();
            match response {
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = parse_retry_after(resp.headers());
                    let text = match resp.text() {
                        Ok(text) => text,
                        Err(e) => {
                            last_error = RpcError { message: format!("failed reading HTTP body: {e}"), retry_after, retryable: true };
                            if attempt <= self.retries {
                                sleep_before_retry(self.retry_base_ms, attempt, last_error.retry_after);
                                continue;
                            }
                            return Err(last_error);
                        }
                    };
                    if !status.is_success() {
                        let retryable = status.as_u16() == 429 || status.is_server_error();
                        last_error = RpcError {
                            message: format!("HTTP {}: {}", status.as_u16(), text.chars().take(300).collect::<String>()),
                            retry_after,
                            retryable,
                        };
                    } else {
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(e) => {
                                return Err(RpcError { message: format!("invalid JSON-RPC JSON: {e}"), retry_after: None, retryable: false });
                            }
                        };
                        if let Some(err) = parsed.get("error") {
                            let code = err.get("code").and_then(Value::as_i64).unwrap_or_default();
                            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown JSON-RPC error");
                            let retryable = code == -32005 || msg.to_ascii_lowercase().contains("rate") ||
                                msg.to_ascii_lowercase().contains("limit") || msg.to_ascii_lowercase().contains("temporar");
                            last_error = RpcError { message: format!("JSON-RPC error {code}: {msg}"), retry_after, retryable };
                        } else if let Some(result) = parsed.get("result") {
                            return Ok((result.clone(), attempt));
                        } else {
                            return Err(RpcError { message: "JSON-RPC response has neither result nor error".to_string(), retry_after: None, retryable: false });
                        }
                    }
                }
                Err(e) => {
                    last_error = RpcError { message: format!("HTTP transport error: {e}"), retry_after: None, retryable: true };
                }
            }
            if attempt <= self.retries && last_error.retryable {
                sleep_before_retry(self.retry_base_ms, attempt, last_error.retry_after);
            } else {
                break;
            }
        }
        Err(last_error)
    }

    fn eth_call(&mut self, to: &str, data: &str, block_hex: &str) -> Result<(String, usize), RpcError> {
        let (result, attempts) = self.call("eth_call", json!([{"to":to,"data":data}, block_hex]))?;
        let value = result.as_str().ok_or_else(|| RpcError {
            message: format!("eth_call returned non-string result: {result}"),
            retry_after: None,
            retryable: false,
        })?;
        Ok((value.to_string(), attempts))
    }
}

fn sleep_before_retry(base_ms: u64, attempt: usize, retry_after: Option<Duration>) {
    let delay = retry_after.unwrap_or_else(|| Duration::from_millis(backoff_ms(base_ms, attempt)));
    thread::sleep(delay.min(Duration::from_secs(60)));
}

fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => { current.push('"'); chars.next(); }
            '"' => quoted = !quoted,
            ',' if !quoted => { fields.push(current.trim().to_string()); current = String::new(); }
            _ => current.push(ch),
        }
    }
    fields.push(current.trim().to_string());
    fields
}

fn load_registry(path: &str, max_pools: usize) -> Result<Vec<Pool>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open registry '{path}': {e}"))?;
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot read registry '{path}': {e}"))?;

    // Stage 4.5's starter registry begins with comment lines and may omit a real CSV header.
    // Skip blank/comment lines, then detect a header if one is present. If not, use the
    // documented starter format: dex,pair_address,fee_bps.
    let data_lines: Vec<(usize, String)> = lines
        .into_iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                None
            } else {
                Some((idx + 1, line))
            }
        })
        .collect();

    if data_lines.is_empty() {
        return Err("registry contains no pool rows after comments/blank lines".to_string());
    }

    let first_fields = csv_fields(&data_lines[0].1);
    let first_lower: Vec<String> = first_fields.iter().map(|s| s.to_ascii_lowercase()).collect();
    let has_header = first_lower.iter().any(|h| ["dex", "exchange", "venue"].contains(&h.as_str()))
        && first_lower.iter().any(|h| ["pair_address", "address", "pool_address", "pair"].contains(&h.as_str()))
        && first_lower.iter().any(|h| ["fee_bps", "fee"].contains(&h.as_str()));

    let (dex_col, address_col, fee_col, start_index) = if has_header {
        let find_col = |names: &[&str]| first_lower.iter().position(|h| names.iter().any(|n| h == n));
        (
            find_col(&["dex", "exchange", "venue"]).ok_or_else(|| "registry header needs a dex/exchange column".to_string())?,
            find_col(&["pair_address", "address", "pool_address", "pair"]).ok_or_else(|| "registry header needs a pair_address/address column".to_string())?,
            find_col(&["fee_bps", "fee"]).ok_or_else(|| "registry header needs a fee_bps column".to_string())?,
            1usize,
        )
    } else {
        (0usize, 1usize, 2usize, 1usize)
    };

    let mut pools = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (source_idx, line) in data_lines.iter().skip(start_index) {
        if line.trim().is_empty() {
            continue;
        }
        let fields = csv_fields(line);
        let get = |col: usize| fields.get(col).map(String::as_str).unwrap_or("");
        let address = normalize_address(get(address_col))
            .map_err(|e| format!("registry line {source_idx}: {e}"))?;
        let dex = get(dex_col).to_string();
        if dex.is_empty() {
            return Err(format!("registry line {source_idx} has empty dex"));
        }
        let fee_bps: u32 = get(fee_col).parse()
            .map_err(|_| format!("registry line {source_idx} has invalid fee_bps '{}'", get(fee_col)))?;
        if fee_bps >= 10_000 {
            return Err(format!("registry line {source_idx} fee_bps must be < 10000"));
        }
        if !seen.insert(address.clone()) {
            return Err(format!("duplicate pool address in registry: {address}"));
        }
        pools.push(Pool { dex, address, fee_bps });
        if pools.len() >= max_pools {
            break;
        }
    }

    // If the file has no header, the first non-comment line is a pool row.
    // It was skipped above only when a header was detected.
    if !has_header {
        let (source_idx, line) = &data_lines[0];
        let fields = csv_fields(line);
        let get = |col: usize| fields.get(col).map(String::as_str).unwrap_or("");
        let address = normalize_address(get(1))
            .map_err(|e| format!("registry line {source_idx}: {e}"))?;
        let dex = get(0).to_string();
        if dex.is_empty() {
            return Err(format!("registry line {source_idx} has empty dex"));
        }
        let fee_bps: u32 = get(2).parse()
            .map_err(|_| format!("registry line {source_idx} has invalid fee_bps '{}'", get(2)))?;
        if fee_bps >= 10_000 {
            return Err(format!("registry line {source_idx} fee_bps must be < 10000"));
        }
        if !seen.insert(address.clone()) {
            return Err(format!("duplicate pool address in registry: {address}"));
        }
        pools.insert(0, Pool { dex, address, fee_bps });
        if pools.len() > max_pools {
            pools.truncate(max_pools);
        }
    }

    if pools.is_empty() {
        return Err("registry contains no pools".to_string());
    }
    Ok(pools)
}

fn get_token_info(rpc: &mut RpcClient, address: &str, block_hex: &str) -> Result<(TokenInfo, usize), RpcError> {
    let (decimals_hex, attempts) = rpc.eth_call(address, DECIMALS_SELECTOR, block_hex)?;
    let decimals_value = parse_hex_u128(&decimals_hex).map_err(|e| RpcError { message: e, retry_after: None, retryable: false })?;
    let decimals = u8::try_from(decimals_value).map_err(|_| RpcError {
        message: format!("token decimals out of uint8 range: {decimals_value}"),
        retry_after: None,
        retryable: false,
    })?;
    if decimals > 36 {
        return Err(RpcError { message: format!("token decimals {decimals} exceeds safety limit 36"), retry_after: None, retryable: false });
    }
    Ok((TokenInfo { address: address.to_string(), decimals }, attempts))
}

fn verify_pool(rpc: &mut RpcClient, pool: &Pool, block: u64, block_hex: &str) -> Result<VerifiedPool, RpcError> {
    let started = Instant::now();
    let mut attempts_total = 0usize;

    let (token0_data, a) = rpc.eth_call(&pool.address, TOKEN0_SELECTOR, block_hex)?;
    attempts_total += a;
    let token0_address = word_address(&token0_data).map_err(|e| RpcError { message: format!("token0 ABI decode: {e}"), retry_after: None, retryable: false })?;

    let (token1_data, a) = rpc.eth_call(&pool.address, TOKEN1_SELECTOR, block_hex)?;
    attempts_total += a;
    let token1_address = word_address(&token1_data).map_err(|e| RpcError { message: format!("token1 ABI decode: {e}"), retry_after: None, retryable: false })?;

    if token0_address == token1_address {
        return Err(RpcError { message: "pool token0 and token1 are identical".to_string(), retry_after: None, retryable: false });
    }

    let (token0, a) = get_token_info(rpc, &token0_address, block_hex)?;
    attempts_total += a;
    let (token1, a) = get_token_info(rpc, &token1_address, block_hex)?;
    attempts_total += a;

    let (reserves_data, a) = rpc.eth_call(&pool.address, RESERVES_SELECTOR, block_hex)?;
    attempts_total += a;
    let raw = reserves_data.strip_prefix("0x").ok_or_else(|| RpcError {
        message: "reserves ABI response lacks 0x prefix".to_string(), retry_after: None, retryable: false
    })?;
    if raw.len() < 192 {
        return Err(RpcError { message: format!("reserves ABI response too short: {} hex chars", raw.len()), retry_after: None, retryable: false });
    }
    // getReserves returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast).
    let reserve0_raw = u128::from_str_radix(&raw[0..64], 16).map_err(|e| RpcError { message: format!("reserve0 decode: {e}"), retry_after: None, retryable: false })?;
    let reserve1_raw = u128::from_str_radix(&raw[64..128], 16).map_err(|e| RpcError { message: format!("reserve1 decode: {e}"), retry_after: None, retryable: false })?;
    if reserve0_raw >= (1u128 << 112) || reserve1_raw >= (1u128 << 112) {
        return Err(RpcError { message: "reserve exceeds uint112 bound".to_string(), retry_after: None, retryable: false });
    }
    if reserve0_raw == 0 || reserve1_raw == 0 {
        return Err(RpcError { message: "one or both reserves are zero".to_string(), retry_after: None, retryable: false });
    }

    Ok(VerifiedPool {
        dex: pool.dex.clone(),
        pair_address: pool.address.clone(),
        fee_bps: pool.fee_bps,
        reserve0_human: human_amount(reserve0_raw, token0.decimals),
        reserve1_human: human_amount(reserve1_raw, token1.decimals),
        token0, token1, reserve0_raw, reserve1_raw, block,
        attempts: attempts_total,
        elapsed_ms: started.elapsed().as_millis(),
        status: "OK".to_string(),
        error: String::new(),
    })
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else { s.to_string() }
}

fn write_row(file: &mut File, row: &[String]) -> std::io::Result<()> {
    let line = row.iter().map(|s| csv_escape(s)).collect::<Vec<_>>().join(",");
    writeln!(file, "{line}")
}

fn run() -> Result<(), String> {
    let rpc_url = env::var("ETH_RPC_URL").map_err(|_| "ETH_RPC_URL is not set in this terminal session".to_string())?;
    let registry_path = env::var("ARB_POOL_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.to_string());
    let output_path = env::var("ARB_STAGE410_OUTPUT").unwrap_or_else(|_| DEFAULT_OUTPUT.to_string());
    let max_pools = env::var("ARB_MAX_POOLS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(50).clamp(1, 500);
    let retries = env::var("ARB_RETRIES").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(3).min(8);
    let retry_base_ms = env::var("ARB_RETRY_BASE_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(500);
    let pause_ms = env::var("ARB_BATCH_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(250);

    let pools = load_registry(&registry_path, max_pools)?;
    let mut rpc = RpcClient::new(rpc_url, retries, retry_base_ms)?;
    let (block_value, block_attempts) = rpc.call("eth_blockNumber", json!([]))
        .map_err(|e| format!("failed to get block number after retries: {}", e.message))?;
    let block_hex = block_value.as_str().ok_or_else(|| "eth_blockNumber returned non-string".to_string())?;
    let block = parse_hex_u64(block_hex)?;
    let pinned_block = format!("0x{block:x}");

    let mut out = File::create(&output_path).map_err(|e| format!("cannot create output '{output_path}': {e}"))?;
    write_row(&mut out, &[
        "timestamp_ms".into(), "block".into(), "dex".into(), "pair_address".into(), "fee_bps".into(),
        "token0".into(), "token0_decimals".into(), "token1".into(), "token1_decimals".into(),
        "reserve0_raw".into(), "reserve1_raw".into(), "reserve0_human".into(), "reserve1_human".into(),
        "attempts".into(), "elapsed_ms".into(), "status".into(), "error".into()
    ]).map_err(|e| format!("cannot write CSV header: {e}"))?;

    println!("Stage 4.10 — Verified Market Data");
    println!("Registry: {registry_path} | pools: {} | pinned block: {} | block RPC attempts: {}", pools.len(), block, block_attempts);
    println!("Output: {output_path}");
    let started = Instant::now();
    let mut ok_count = 0usize;
    let mut failed_count = 0usize;

    for (index, pool) in pools.iter().enumerate() {
        let pool_started = Instant::now();
        match verify_pool(&mut rpc, pool, block, &pinned_block) {
            Ok(result) => {
                ok_count += 1;
                println!(
                    "[{}/{}] OK {} {} | token0={} ({} dp) reserve={} | token1={} ({} dp) reserve={} | calls={} | {}ms",
                    index + 1, pools.len(), result.dex, result.pair_address,
                    result.token0.address, result.token0.decimals, result.reserve0_human,
                    result.token1.address, result.token1.decimals, result.reserve1_human,
                    result.attempts, result.elapsed_ms
                );
                write_row(&mut out, &[
                    now_ms().to_string(), result.block.to_string(), result.dex, result.pair_address,
                    result.fee_bps.to_string(), result.token0.address, result.token0.decimals.to_string(),
                    result.token1.address, result.token1.decimals.to_string(), format!("0x{:x}", result.reserve0_raw),
                    format!("0x{:x}", result.reserve1_raw), result.reserve0_human, result.reserve1_human,
                    result.attempts.to_string(), result.elapsed_ms.to_string(), result.status, result.error
                ]).map_err(|e| format!("cannot write output row: {e}"))?;
            }
            Err(err) => {
                failed_count += 1;
                println!("[{}/{}] FAILED {} {} | {}", index + 1, pools.len(), pool.dex, pool.address, err.message);
                write_row(&mut out, &[
                    now_ms().to_string(), block.to_string(), pool.dex.clone(), pool.address.clone(),
                    pool.fee_bps.to_string(), String::new(), String::new(), String::new(), String::new(),
                    String::new(), String::new(), String::new(), String::new(), String::new(),
                    pool_started.elapsed().as_millis().to_string(), "FAILED".into(), err.message
                ]).map_err(|e| format!("cannot write failure row: {e}"))?;
            }
        }
        out.flush().map_err(|e| format!("cannot flush CSV: {e}"))?;
        if pause_ms > 0 && index + 1 < pools.len() {
            thread::sleep(Duration::from_millis(pause_ms));
        }
    }

    println!("\nSUMMARY");
    println!("Pools: {} | verified: {} | failed: {} | success rate: {:.1}%",
        pools.len(), ok_count, failed_count, ok_count as f64 * 100.0 / pools.len() as f64);
    println!("Total elapsed: {} ms", started.elapsed().as_millis());
    println!("All eth_call reads were pinned to block {}. No wallet, signing, or transaction methods are used.", block);
    if failed_count > 0 {
        println!("Review FAILED rows and error messages before using this data in any quote engine.");
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn decodes_uint112_reserve_words() {
        assert_eq!(u128::from_str_radix("0000000000000000000000000000000000000000000000000000000000010000", 16).unwrap(), 65536);
    }

    #[test]
    fn normalizes_address_case() {
        assert_eq!(
            normalize_address("0xA0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    #[test]
    fn rejects_malformed_address() {
        assert!(normalize_address("0x1234").is_err());
    }

    #[test]
    fn human_amount_respects_decimals() {
        assert_eq!(human_amount(1_234_567, 6), "1.234567");
        assert_eq!(human_amount(2_000_000, 6), "2");
        assert_eq!(human_amount(123, 0), "123");
    }

    #[test]
    fn parses_retry_after_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(2)));
    }

    #[test]
    fn exponential_backoff_caps() {
        assert_eq!(backoff_ms(500, 1), 500);
        assert_eq!(backoff_ms(500, 2), 1000);
        assert_eq!(backoff_ms(500, 99), 30_000);
    }

    #[test]
    fn decodes_abi_address_word() {
        let data = format!("0x{}{}", "0".repeat(24), "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        assert_eq!(word_address(&data).unwrap(), "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    }

    #[test]
    fn registry_parser_handles_quoted_dex_names() {
        let fields = csv_fields("\"Uniswap, V2\",0x0000000000000000000000000000000000000001,30");
        assert_eq!(fields[0], "Uniswap, V2");
        assert_eq!(fields[2], "30");
    }
}
