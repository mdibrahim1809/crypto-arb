// Derive Clone so token metadata can be reused by multiple pools.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
// Store the metadata needed to interpret token amounts.
pub struct Token {
    // Store the human-readable ticker symbol.
    pub symbol: String,
    // Store the token's number of decimal places.
    pub decimals: u8,
}

// Implement constructors and formatting helpers for Token.
impl Token {
    // Create a token from a symbol and decimal count.
    pub fn new(symbol: &str, decimals: u8) -> Self {
        // Return a fully initialized token.
        Self {
            // Convert the borrowed symbol into owned string data.
            symbol: symbol.to_string(),
            // Store the decimal precision.
            decimals,
        }
    }

    // Convert a raw integer amount into a human-readable decimal string.
    pub fn format_raw(&self, raw_amount: u128) -> String {
        // Convert the raw amount into a string so we can place the decimal point exactly.
        let raw = raw_amount.to_string();

        // Handle tokens that use zero decimal places.
        if self.decimals == 0 {
            // Return the raw integer directly.
            return raw;
        }

        // Convert the decimal count to usize for string indexing.
        let decimals = self.decimals as usize;

        // Add leading zeroes when the raw value has fewer digits than the decimal count.
        let padded = if raw.len() <= decimals {
            // Create enough leading zeroes for a value smaller than one whole token.
            format!("{}{}", "0".repeat(decimals + 1 - raw.len()), raw)
        } else {
            // Reuse the raw string when it already has enough integer digits.
            raw
        };

        // Split the padded value into the integer portion and fractional portion.
        let split_index = padded.len() - decimals;
        // Extract the integer portion.
        let integer_part = &padded[..split_index];
        // Extract the fractional portion.
        let fractional_part = &padded[split_index..];

        // Remove insignificant trailing zeroes from the fractional part.
        let fractional_trimmed = fractional_part.trim_end_matches('0');

        // Return just the integer part when the fractional part is all zeroes.
        if fractional_trimmed.is_empty() {
            // Return the clean whole-token value.
            return integer_part.to_string();
        }

        // Combine the integer and fractional portions.
        format!("{integer_part}.{fractional_trimmed}")
    }
}
