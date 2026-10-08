// Derive traits needed to copy, compare, debug, and hash DEX identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
// Identify the simulated decentralized exchange.
pub enum Dex {
    // Represent a Uniswap V2-style pool.
    UniswapV2,
    // Represent a SushiSwap-style pool.
    SushiSwap,
}

// Implement human-readable output for DEX names.
impl std::fmt::Display for Dex {
    // Format one DEX identifier into the supplied formatter.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Select the correct display label.
        match self {
            // Display the Uniswap label.
            Self::UniswapV2 => write!(formatter, "Uniswap V2"),
            // Display the SushiSwap label.
            Self::SushiSwap => write!(formatter, "SushiSwap"),
        }
    }
}
