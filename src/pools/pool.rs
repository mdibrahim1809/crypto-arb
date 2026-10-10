// Import the DEX identifier so every pool can identify its venue.
use crate::arbitrage::dex::Dex;
// Import the token metadata used by this pool.
use crate::tokens::token::Token;

// Store ten-thousandths as the basis-point fee denominator.
const FEE_DENOMINATOR: u128 = 10_000;

// Store the result of one AMM quote.
#[derive(Clone, Debug)]
pub struct Quote {
    // Store the input amount supplied to the swap.
    pub amount_in: u128,
    // Store the amount received from the swap.
    pub amount_out: u128,
    // Store the fee charged in the input token's base units.
    pub fee_amount: u128,
    // Store the estimated price impact in basis points.
    pub price_impact_bps: u128,
}

// Define a constant-product AMM liquidity pool.
#[derive(Clone, Debug)]
pub struct Pool {
    // Store the first token in the pair.
    pub token_a: Token,
    // Store the second token in the pair.
    pub token_b: Token,
    // Store the swap fee in basis points.
    pub fee_bps: u32,
    // Store a simple human-readable liquidity score for this simulation.
    pub liquidity: u128,
    // Store reserve A in the token's base units.
    pub reserve_a: u128,
    // Store reserve B in the token's base units.
    pub reserve_b: u128,
    // Store the simulated DEX where this pool exists.
    pub dex: Dex,
}

// Implement pool construction and AMM pricing.
impl Pool {
    // Construct a new pool from token metadata, reserves, fees, and venue information.
    pub fn new(
        token_a: Token,
        token_b: Token,
        fee_bps: u32,
        liquidity: u128,
        reserve_a: u128,
        reserve_b: u128,
        dex: Dex,
    ) -> Self {
        // Reject impossible fee values before creating a pool.
        assert!(
            fee_bps < FEE_DENOMINATOR as u32,
            "Pool fee must be below 100%"
        );

        // Return the fully initialized pool.
        Self {
            // Store token A.
            token_a,
            // Store token B.
            token_b,
            // Store the fee.
            fee_bps,
            // Store the liquidity score.
            liquidity,
            // Store reserve A.
            reserve_a,
            // Store reserve B.
            reserve_b,
            // Store the exchange identifier.
            dex,
        }
    }

    // Check whether this pool can perform the requested token swap.
    pub fn supports_pair(&self, token_in: &Token, token_out: &Token) -> bool {
        // Require positive reserves because an empty pool cannot execute a quote.
        let has_liquidity = self.reserve_a > 0 && self.reserve_b > 0 && self.liquidity > 0;
        // Check the normal A -> B orientation.
        let forward = self.token_a == *token_in && self.token_b == *token_out;
        // Check the reverse B -> A orientation.
        let reverse = self.token_b == *token_in && self.token_a == *token_out;
        // Return true only when the pair and liquidity are both valid.
        has_liquidity && (forward || reverse)
    }

    // Calculate a swap quote without mutating the pool.
    pub fn quote(&self, token_in: &Token, amount_in: u128) -> Option<Quote> {
        // Reject zero-sized swaps.
        if amount_in == 0 {
            // A zero-input quote has no useful economic meaning.
            return None;
        }

        // Determine whether token A is entering the pool.
        let input_is_a = self.token_a == *token_in;
        // Select the correct input reserve for the swap direction.
        let reserve_in = if input_is_a {
            self.reserve_a
        } else {
            self.reserve_b
        };
        // Select the correct output reserve for the swap direction.
        let reserve_out = if input_is_a {
            self.reserve_b
        } else {
            self.reserve_a
        };

        // Reject empty reserves because the AMM formula would be invalid.
        if reserve_in == 0 || reserve_out == 0 {
            // Return no quote when the pool cannot provide liquidity.
            return None;
        }

        // Calculate the portion of the input that remains after the pool fee.
        let fee_adjusted_input =
            amount_in.saturating_mul(FEE_DENOMINATOR - self.fee_bps as u128) / FEE_DENOMINATOR;

        // Calculate the explicit fee in the input token's base units.
        let fee_amount = amount_in.saturating_sub(fee_adjusted_input);

        // Calculate the numerator of the constant-product output formula.
        let numerator = fee_adjusted_input.saturating_mul(reserve_out);
        // Calculate the denominator of the constant-product output formula.
        let denominator = reserve_in.saturating_add(fee_adjusted_input);

        // Reject an invalid denominator even though normal pool state makes this impossible.
        if denominator == 0 {
            // Return no quote when arithmetic would otherwise be invalid.
            return None;
        }

        // Calculate the actual output after price impact and fees.
        let amount_out = numerator / denominator;

        // Calculate the theoretical output at the current spot price after fees but before price impact.
        let spot_output = fee_adjusted_input.saturating_mul(reserve_out) / reserve_in;

        // Estimate price impact in basis points.
        let price_impact_bps = if spot_output == 0 {
            // Report zero when the theoretical output is too small to calculate impact.
            0
        } else {
            // Calculate the percentage of theoretical output lost to price impact.
            let impact_numerator = spot_output.saturating_sub(amount_out);
            // Convert that ratio to basis points using integer arithmetic.
            impact_numerator.saturating_mul(10_000) / spot_output
        };

        // Return the complete quote object.
        Some(Quote {
            // Store the original input amount.
            amount_in,
            // Store the actual output.
            amount_out,
            // Store the input-token fee.
            fee_amount,
            // Store estimated price impact.
            price_impact_bps,
        })
    }
}
