// Import the DEX identifier stored by each opportunity.
use super::dex::Dex;
// Import the quote structure returned by the pool pricing engine.
use crate::pools::pool::Quote;

// Store every economic input and result for one complete arbitrage cycle.
#[derive(Clone, Debug)]
pub struct Opportunity {
    // Store the starting token symbol.
    pub token_in: String,
    // Store the intermediate token symbol.
    pub token_mid: String,
    // Store the starting capital in base units.
    pub amount_in: u128,
    // Store the complete quote for leg one.
    pub quote_first: Quote,
    // Store the complete quote for leg two.
    pub quote_second: Quote,
    // Store the final amount after both legs.
    pub amount_out: u128,
    // Store the gross profit before gas.
    pub gross_profit: i128,
    // Store the estimated gas cost in starting-token base units.
    pub gas_cost: u128,
    // Store the net profit after gas.
    pub net_profit: i128,
    // Store the venue used for the first leg.
    pub buy_dex: Dex,
    // Store the venue used for the second leg.
    pub sell_dex: Dex,
}

// Implement profitability helpers for opportunities.
impl Opportunity {
    // Return true when this route clears the configured minimum profit.
    pub fn is_profitable(&self, minimum_profit: u128) -> bool {
        // Compare signed net profit against the unsigned threshold safely.
        self.net_profit > minimum_profit as i128
    }
}
