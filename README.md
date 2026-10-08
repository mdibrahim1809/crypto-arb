# Crypto Arbitrage Engine — Stage 2

This is the Stage 2 replacement for the `crypto-arb` Rust project.

## What changed

Stage 2 fixes the warnings from the earlier build and adds a proper quote/reporting pipeline.

The program now:

1. Represents USDC and ETH with correct base-unit decimal conventions.
2. Uses realistic simulated pool reserve scales.
3. Calculates fee-adjusted AMM output.
4. Calculates the swap fee for every leg.
5. Estimates price impact in basis points.
6. Scans every valid two-pool route.
7. Sorts routes by net profit.
8. Calculates gross profit, gas, net profit, and ROI.
9. Prints the opportunity fields directly, so the compiler no longer reports those fields as unused.
10. Reads the liquidity field during pair validation, removing that warning.
11. Uses `is_profitable()` during reporting, removing that warning.
12. Keeps the build dependency-free.

## Expected test behavior

The supplied test market intentionally contains a price spread:

- Uniswap V2 has 105 ETH against 40,000 USDC.
- SushiSwap has 95 ETH against 40,000 USDC.
- The test starts with 1,000 USDC.
- Each pool charges 30 bps.
- The simulated gas cost is 5 USDC.

The best route should therefore be:

```text
USDC
  ↓
Uniswap V2
  ↓
ETH
  ↓
SushiSwap
  ↓
USDC
```

The exact integer output depends only on the formulas in the source. With the supplied values, the cycle is profitable after the simulated gas estimate.

## Run

From the project directory:

```powershell
cargo check
cargo run
```

Then build an optimized binary:

```powershell
cargo build --release
```

## Files

```text
crypto-arb-stage2/
├── Cargo.toml
├── README.md
├── LINE_BY_LINE_MAP.txt
└── src/
    ├── main.rs
    ├── tokens/
    │   ├── mod.rs
    │   └── token.rs
    ├── pools/
    │   ├── mod.rs
    │   └── pool.rs
    └── arbitrage/
        ├── mod.rs
        ├── dex.rs
        ├── engine.rs
        └── opportunity.rs
```

## Safety boundary

This remains a local simulator.

It does not contain:

- private keys
- wallet signing
- live RPC execution
- real swap transactions
- flash-loan execution
- real funds

That separation is deliberate.

## Next stage

The correct next engineering layer is real market data:

```text
Real RPC / WebSocket
        ↓
Pool discovery
        ↓
Live reserves / quotes
        ↓
Route engine
        ↓
Profitability + gas
        ↓
Paper execution
        ↓
Risk controls
        ↓
Only then live execution
```

Do not insert private keys into this simulator.
