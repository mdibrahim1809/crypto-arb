// Load the arbitrage module containing the engine, DEX type, and opportunity model.
mod arbitrage;
// Load the pool module containing the AMM reserve and quote logic.
mod pools;
// Load the token module containing token metadata and amount formatting.
mod tokens;

// Import the arbitrage engine that evaluates routes between pools.
use arbitrage::engine::ArbitrageEngine;
// Import the DEX enum used to identify each simulated venue.
use arbitrage::dex::Dex;
// Import the opportunity type so we can print a detailed result.
use arbitrage::opportunity::Opportunity;
// Import the pool type used to construct our test market.
use pools::pool::Pool;
// Import the token type used to construct USDC and ETH.
use tokens::token::Token;

// Add comma separators to the integer portion of a decimal string.
fn add_commas(number: &str) -> String {
    // Split a possible negative sign away from the numeric digits.
    let (sign, digits) = if let Some(rest) = number.strip_prefix('-') {
        // Preserve the negative sign separately.
        ("-", rest)
    } else {
        // Use an empty sign for non-negative values.
        ("", number)
    };

    // Build the grouped integer from the right side in three-digit chunks.
    let mut grouped = String::new();
    // Read the digits in reverse order so grouping is easy.
    for (index, character) in digits.chars().rev().enumerate() {
        // Insert a comma before every new three-digit group.
        if index > 0 && index % 3 == 0 {
            // Add the thousands separator.
            grouped.push(',');
        }
        // Add the current digit.
        grouped.push(character);
    }

    // Reverse the grouped string back into normal reading order.
    let grouped: String = grouped.chars().rev().collect();
    // Put the sign back in front of the grouped number.
    format!("{sign}{grouped}")
}

// Format a floating-point number with two decimal places and comma separators.
fn format_decimal(value: f64) -> String {
    // Convert the value to exactly two decimal places.
    let raw = format!("{value:.2}");
    // Split the number around its decimal point.
    let mut pieces = raw.split('.');
    // Read the integer component.
    let integer = pieces.next().unwrap_or("0");
    // Read the fractional component.
    let fraction = pieces.next().unwrap_or("00");
    // Add grouping to the integer component.
    let grouped_integer = add_commas(integer);
    // Reassemble the formatted decimal number.
    format!("{grouped_integer}.{fraction}")
}

// Format a raw token amount as a human-readable dollar value for USDC.
fn format_usdc(raw_amount: u128) -> String {
    // Convert six-decimal USDC base units to a floating-point number for display only.
    let value = raw_amount as f64 / 1_000_000.0;
    // Return the value with a dollar sign and comma separators.
    format!("${}", format_decimal(value))
}

// Format a raw ETH amount as a human-readable ETH value.
fn format_eth(raw_amount: u128) -> String {
    // Convert eighteen-decimal ETH base units to ETH for display only.
    let value = raw_amount as f64 / 1_000_000_000_000_000_000.0;
    // Return the ETH amount with six decimal places.
    format!("{value:.6} ETH")
}

// Convert basis points into a percentage string.
fn format_bps(bps: u128) -> String {
    // One basis point equals 0.01 percent, so divide by 100.
    let percentage = bps as f64 / 100.0;
    // Return the percentage with two decimal places.
    format!("{percentage:.2}%")
}

// Print one complete opportunity using the fields instead of relying on Debug output.
fn print_opportunity(opportunity: &Opportunity, min_profit: u128) {
    // Print a visual separator before the opportunity report.
    println!();
    // Print the report heading.
    println!("============================================================");
    // Print the route heading.
    println!("ARBITRAGE OPPORTUNITY");
    // Print the second separator.
    println!("============================================================");

    // Print the route tokens.
    println!(
        "Route:            {} -> {} -> {}",
        opportunity.token_in, opportunity.token_mid, opportunity.token_in
    );

    // Print the DEX route.
    println!(
        "DEX route:        {} -> {}",
        opportunity.buy_dex, opportunity.sell_dex
    );

    // Print the first-leg section heading.
    println!();
    println!("LEG 1 — BUY / ACQUIRE {}", opportunity.token_mid);
    println!("------------------------------------------------------------");

    // Print the exact first-leg input amount stored inside the first quote.
    println!(
        "Input:             {}",
        format_usdc(opportunity.quote_first.amount_in)
    );
    // Print the first-leg token amount received.
    println!(
        "Output:            {}",
        format_eth(opportunity.quote_first.amount_out)
    );
    // Print the first-leg swap fee in its native input unit.
    println!(
        "Swap fee:          {}",
        format_usdc(opportunity.quote_first.fee_amount)
    );
    // Print the first-leg price impact.
    println!(
        "Price impact:      {}",
        format_bps(opportunity.quote_first.price_impact_bps)
    );

    // Print the second-leg section heading.
    println!();
    println!("LEG 2 — SELL {}", opportunity.token_mid);
    println!("------------------------------------------------------------");

    // Print the exact second-leg input amount stored inside the second quote.
    println!(
        "Input:             {}",
        format_eth(opportunity.quote_second.amount_in)
    );
    // Print the second-leg output amount.
    println!("Output:            {}", format_usdc(opportunity.amount_out));
    // Print the second-leg swap fee in ETH because ETH is the input to leg two.
    println!(
        "Swap fee:          {}",
        format_eth(opportunity.quote_second.fee_amount)
    );
    // Print the second-leg price impact.
    println!(
        "Price impact:      {}",
        format_bps(opportunity.quote_second.price_impact_bps)
    );

    // Print the final P&L section.
    println!();
    println!("PROFIT ANALYSIS");
    println!("------------------------------------------------------------");

    // Print the original input.
    println!("Starting capital:  {}", format_usdc(opportunity.amount_in));
    // Print the final output after both swaps.
    println!("Final capital:     {}", format_usdc(opportunity.amount_out));
    // Print gross profit before gas.
    println!(
        "Gross profit:      {}",
        format_profit(opportunity.gross_profit)
    );
    // Print the estimated gas cost.
    println!("Estimated gas:     {}", format_usdc(opportunity.gas_cost));

    // Print net profit after gas.
    println!(
        "Net profit:        {}",
        format_profit(opportunity.net_profit)
    );

    // Calculate the net ROI using the starting capital.
    let roi = opportunity.net_profit as f64 / opportunity.amount_in as f64 * 100.0;
    // Print the net ROI.
    println!("Net ROI:           {roi:.4}%");

    // Print the configured minimum profit.
    println!("Minimum required:  {}", format_usdc(min_profit));

    // Print whether the route passes our profitability threshold.
    if opportunity.is_profitable(min_profit) {
        // Show the successful state.
        println!("STATUS:            PROFITABLE");
    } else {
        // Show the rejected state.
        println!("STATUS:            NOT PROFITABLE");
    }

    // Print the closing separator.
    println!("============================================================");
}

// Format a signed profit amount using six-decimal USDC base units.
fn format_profit(raw_profit: i128) -> String {
    // Convert the signed base-unit value into a dollar value.
    let value = raw_profit as f64 / 1_000_000.0;
    // Select an explicit plus sign for non-negative profit and retain the minus sign for losses.
    let sign = if value >= 0.0 { "+" } else { "" };
    // Combine the explicit sign with our comma-formatted dollar value.
    format!("{sign}${}", format_decimal(value.abs()))
}

// Print the configured pool information.
fn print_pool(pool: &Pool) {
    // Print the exchange and pair name.
    println!(
        "{:<10} | {} / {}",
        pool.dex, pool.token_a.symbol, pool.token_b.symbol
    );
    // Print the fee.
    println!("  Fee:         {} bps", pool.fee_bps);
    // Print the simplified liquidity score so the field is actively used.
    println!("  Liquidity:   {}", pool.liquidity);
    // Print the ETH reserve using ETH's 18-decimal base unit.
    println!("  Reserve A:   {}", pool.token_a.format_raw(pool.reserve_a));
    // Print the USDC reserve using USDC's 6-decimal base unit.
    println!("  Reserve B:   {}", pool.token_b.format_raw(pool.reserve_b));
}

// The main function is the program entry point.
fn main() {
    // Print a startup banner.
    println!("Crypto arbitrage engine starting...");
    // Print the version of this simulator.
    println!("Stage 2 — quote engine + route scanner + detailed reporting");

    // Create the USDC token with six decimal places.
    let usdc = Token::new("USDC", 6);
    // Create the ETH token with eighteen decimal places.
    let eth = Token::new("ETH", 18);

    // Print the token configuration.
    println!();
    println!("TOKEN CONFIGURATION");
    println!("------------------------------------------------------------");
    println!("Token: {} | decimals: {}", usdc.symbol, usdc.decimals);
    println!("Token: {} | decimals: {}", eth.symbol, eth.decimals);

    // Use realistic reserve unit conventions:
    // 40,000 USDC and roughly 100 ETH per simulated pool.
    let usdc_reserve = 40_000_u128 * 1_000_000_u128;
    // Make Uniswap cheaper because it has more ETH against the same USDC reserve.
    let uniswap_eth_reserve = 105_u128 * 1_000_000_000_000_000_000_u128;
    // Make SushiSwap more expensive because it has less ETH against the same USDC reserve.
    let sushiswap_eth_reserve = 95_u128 * 1_000_000_000_000_000_000_u128;

    // Construct the first simulated pool.
    let uniswap_pool = Pool::new(
        eth.clone(),
        usdc.clone(),
        30,
        105,
        uniswap_eth_reserve,
        usdc_reserve,
        Dex::UniswapV2,
    );

    // Construct the second simulated pool.
    let sushiswap_pool = Pool::new(
        eth.clone(),
        usdc.clone(),
        30,
        95,
        sushiswap_eth_reserve,
        usdc_reserve,
        Dex::SushiSwap,
    );

    // Print the market configuration.
    println!();
    println!("SIMULATED MARKET");
    println!("------------------------------------------------------------");
    print_pool(&uniswap_pool);
    println!();
    print_pool(&sushiswap_pool);

    // Require at least 1 USDC of net profit before a route is considered actionable.
    let minimum_profit = 1_u128 * 1_000_000_u128;
    // Estimate 5 USDC of gas for this local simulation.
    let estimated_gas = 5_u128 * 1_000_000_u128;

    // Create a mutable arbitrage engine because pools will be added to it.
    let mut engine = ArbitrageEngine::new(minimum_profit, estimated_gas);
    // Add the Uniswap pool to the engine's market database.
    engine.add_pool(uniswap_pool);
    // Add the SushiSwap pool to the engine's market database.
    engine.add_pool(sushiswap_pool);

    // Test a 1,000 USDC starting amount.
    let amount_in = 1_000_u128 * 1_000_000_u128;

    // Print the scan configuration.
    println!();
    println!("SCAN CONFIGURATION");
    println!("------------------------------------------------------------");
    println!("Starting amount:  {}", format_usdc(amount_in));
    println!("Gas estimate:     {}", format_usdc(estimated_gas));
    println!("Min net profit:   {}", format_usdc(minimum_profit));

    // Scan every valid two-pool route for USDC -> ETH -> USDC.
    let candidates = engine.scan(&usdc, &eth, amount_in);

    // Print how many routes were evaluated.
    println!();
    println!("Routes evaluated: {}", candidates.len());

    // Find the single best route by net profit.
    let best = candidates.first();

    // Print the best route when at least one valid route exists.
    if let Some(best_opportunity) = best {
        // Print the best route details.
        print_opportunity(best_opportunity, engine.minimum_profit());

        // Count all routes that actually pass the configured threshold.
        let profitable_count = candidates
            .iter()
            .filter(|opportunity| opportunity.is_profitable(engine.minimum_profit()))
            .count();

        // Print the number of actionable routes.
        println!("Profitable opportunities found: {profitable_count}");
    } else {
        // Explain that no valid two-pool route existed at all.
        println!("Profitable opportunities found: 0");
        println!("No valid two-pool route was found for this token pair.");
    }
}
