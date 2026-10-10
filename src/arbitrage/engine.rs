// Import the complete opportunity record returned from route evaluation.
use super::opportunity::Opportunity;
// Import the pool model used to produce swap quotes.
use crate::pools::pool::Pool;
// Import the token model used to define the route.
use crate::tokens::token::Token;

// Define the arbitrage scanner and its configuration.
pub struct ArbitrageEngine {
    // Store every liquidity pool currently known to the scanner.
    pools: Vec<Pool>,
    // Store the minimum net profit that the caller considers actionable.
    minimum_profit: u128,
    // Store the estimated cost of executing both legs.
    gas_cost: u128,
}

// Implement market management and route evaluation.
impl ArbitrageEngine {
    // Create a new empty arbitrage engine.
    pub fn new(minimum_profit: u128, gas_cost: u128) -> Self {
        // Return a scanner with no pools loaded yet.
        Self {
            // Start with an empty market database.
            pools: Vec::new(),
            // Store the configured profit threshold.
            minimum_profit,
            // Store the configured gas estimate.
            gas_cost,
        }
    }

    // Add a pool to the scanner's market database.
    pub fn add_pool(&mut self, pool: Pool) {
        // Append the supplied pool to the internal list.
        self.pools.push(pool);
    }

    // Expose the minimum-profit threshold for reporting.
    pub fn minimum_profit(&self) -> u128 {
        // Return the configured threshold.
        self.minimum_profit
    }

    // Scan all two-pool token_in -> token_mid -> token_in routes.
    pub fn scan(&self, token_in: &Token, token_mid: &Token, amount_in: u128) -> Vec<Opportunity> {
        // Create a vector that will contain every valid route.
        let mut candidates = Vec::new();

        // Try every pool as the first leg.
        for first_pool in &self.pools {
            // Skip pools that cannot perform token_in -> token_mid.
            if !first_pool.supports_pair(token_in, token_mid) {
                // Continue to the next first-leg pool.
                continue;
            }

            // Ask the first pool for a quote.
            let Some(quote_first) = first_pool.quote(token_in, amount_in) else {
                // Skip this route if the first quote is invalid.
                continue;
            };

            // Try every different pool as the second leg.
            for second_pool in &self.pools {
                // Do not execute both legs against the same pool.
                if std::ptr::eq(first_pool, second_pool) {
                    // Continue to the next second-leg pool.
                    continue;
                }

                // Skip pools that cannot perform token_mid -> token_in.
                if !second_pool.supports_pair(token_mid, token_in) {
                    // Continue to the next second-leg pool.
                    continue;
                }

                // Quote the second leg using the first leg's output.
                let Some(quote_second) = second_pool.quote(token_mid, quote_first.amount_out)
                else {
                    // Skip the route when the second quote is invalid.
                    continue;
                };

                // Save the final output returned to the starting token.
                let amount_out = quote_second.amount_out;
                // Calculate gross profit before gas.
                let gross_profit = amount_out as i128 - amount_in as i128;
                // Calculate net profit after the estimated execution cost.
                let net_profit = gross_profit - self.gas_cost as i128;

                // Create the complete opportunity record.
                candidates.push(Opportunity {
                    // Save the starting token symbol.
                    token_in: token_in.symbol.clone(),
                    // Save the intermediate token symbol.
                    token_mid: token_mid.symbol.clone(),
                    // Save starting capital.
                    amount_in,
                    // Save the first-leg quote.
                    quote_first: quote_first.clone(),
                    // Save the second-leg quote.
                    quote_second,
                    // Save final output.
                    amount_out,
                    // Save gross profit.
                    gross_profit,
                    // Save gas cost.
                    gas_cost: self.gas_cost,
                    // Save net profit.
                    net_profit,
                    // Store the first venue.
                    buy_dex: first_pool.dex,
                    // Store the second venue.
                    sell_dex: second_pool.dex,
                });
            }
        }

        // Put the highest-net-profit route first for easy reporting.
        candidates.sort_by(|left, right| right.net_profit.cmp(&left.net_profit));

        // Return every route so the caller can inspect both profitable and rejected candidates.
        candidates
    }
}
