# Crypto Arbitrage Engine — Stage 3.1

A Rust-based crypto arbitrage research project. Stage 3.1 adds **read-only Ethereum Mainnet RPC connectivity** using Alchemy (or any compatible Ethereum JSON-RPC endpoint).

> **Current status:** Stage 2 simulated quote engine and two-pool route scanner; Stage 3.1 live RPC connectivity. This is not yet a live arbitrage bot and does not submit transactions.

## Stage 3.1 — What works

The `stage3_rpc` binary:

- Reads an Ethereum RPC endpoint from the `ETH_RPC_URL` environment variable.
- Calls `eth_chainId` to identify the connected network.
- Calls `eth_blockNumber` to read the latest block number.
- Calls `eth_gasPrice` to read the node's current gas-price quote.
- Handles HTTP failures, RPC errors, missing results, and malformed hexadecimal quantities.
- Keeps wallet signing and transaction execution disabled.

## Requirements

- Rust toolchain and Cargo
- An Ethereum JSON-RPC provider, such as [Alchemy](https://www.alchemy.com/)
- An enabled Ethereum Mainnet HTTPS endpoint

## Configure the RPC endpoint

In PowerShell, from the project directory, set the endpoint for the current terminal session:

```powershell
$env:ETH_RPC_URL = "YOUR_ETHEREUM_HTTPS_RPC_ENDPOINT"
```

Replace the placeholder with your own endpoint. **Never commit your API key, paste it into source code, or publish it in logs/screenshots.** If an endpoint has been exposed, rotate its API key in your provider dashboard.

## Run Stage 3.1

From the repository root:

```powershell
cargo check
cargo run --bin stage3_rpc
```

To build an optimized version:

```powershell
cargo build --release --bin stage3_rpc
```

A successful run should display the chain ID, latest block number, and gas price. Ethereum Mainnet uses chain ID `1`. Values change as the network advances.

## Run the Stage 2 simulator

The original simulated quote engine remains available:

```powershell
cargo run
```

It models USDC/ETH pools on Uniswap V2 and SushiSwap, quotes two-leg swaps, estimates fees and price impact, ranks routes by net profit, and reports the result using simulated reserves and a configured gas estimate.

**Simulator results are not evidence of real-world profitability.** The Stage 2 market is synthetic.

## Current architecture

```text
Stage 2 simulator
  └─ Simulated pools → AMM quotes → two-pool route scanner → report

Stage 3.1
  └─ ETH_RPC_URL → Ethereum JSON-RPC → chain ID / block / gas price

Planned next
  └─ Read on-chain DEX pool state → live quotes → route scanning
     → gas and execution-cost model → alerts → paper execution
```

## Planned work

1. **Stage 3.2:** Read and validate on-chain pool contract data.
2. Read token addresses and reserves from supported DEX pools.
3. Update the quote engine to use live pool state.
4. Expand route scanning across multiple pools and token pairs.
5. Improve profitability estimates with realistic gas, fees, and execution assumptions.
6. Add opportunity monitoring and paper-trading evaluation.

Live data, price differences, and theoretical quotes do not guarantee executable profit. Reserves can change, transactions can fail, and gas, slippage, latency, and MEV can erase a spread.

## Security boundary

At this stage, the project:

- Does **not** sign transactions.
- Does **not** submit swaps.
- Does **not** use private keys.
- Does **not** execute flash loans.
- Does **not** trade real funds.

Keep this boundary in place until the live-data pipeline, quote calculations, risk controls, and paper-trading results have been thoroughly tested.

## Repository layout

```text
crypto-arb/
├── Cargo.toml
├── Cargo.lock
├── README.md
└── src/
    ├── main.rs
    ├── bin/
    │   └── stage3_rpc.rs
    ├── arbitrage/
    ├── pools/
    └── tokens/
```

The exact module contents may evolve as the project progresses.
