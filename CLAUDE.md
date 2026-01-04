# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

MMDNCA is a Rust-based automated market-making bot for Polymarket binary prediction markets. It implements a hedging strategy that combines passive maker orders with active hedging to maintain neutral positions while capturing spread.

**Language**: Rust (Edition 2024)
**Runtime**: Tokio async
**Target Platform**: Polymarket CLOB (Central Limit Order Book)

## Common Commands

```bash
# Build and run
cargo build --release    # Production build
cargo run                # Run in debug mode (development)
cargo check              # Fast compilation check without binary

# The bot runs interactively - select option 1 to scan and trade
```

## Environment Setup

Required environment variables in `.env`:

```
POLYMARKET_PRIVATE_KEY=<wallet_private_key>
FUNDER_ADDRESS=<0x_wallet_address>
POLYMARKET_API_KEY=<uuid_api_key>
POLYMARKET_API_SECRET=<secret>
POLYMARKET_API_PASSPHRASE=<passphrase>
CLOB_WS_MARKET=<market_ws_url>
CLOB_WS_USER=<user_ws_url>
```

## Architecture

### Module Structure

The codebase is organized into 7 modules with clear separation of concerns:

**`main.rs`** (src/main.rs:1)
- Entry point with Tokio async runtime
- Authenticates with Polymarket CLOB REST API and WebSocket API
- Interactive CLI menu for market scanning and trading
- Runs two concurrent streams using `tokio::select!`:
  - Market data stream (order book updates)
  - User event stream (trade fill confirmations)

**`engine.rs`** (src/engine.rs:1)
- Core trading logic in `RealEngine` struct
- Manages portfolio state via `Mutex<Portfolio>`
- Implements three trading behaviors:
  1. **Hedging**: Executes taker orders when position skew exceeds `HEDGE_SIZE` (3.0 shares)
  2. **Emergency cover**: Closes positions if cost basis + opposite ask > 1.05
  3. **Maker orders**: Places paired limit orders on both sides with 5-second TTL
- Constants: `MAX_BALANCE` (3000 USDC), `SIZE` (3.0 shares per order)

**`scanner.rs`** (src/scanner.rs:1)
- `AutoScanner` polls Polymarket API for trading opportunities
- Searches for events matching slug prefix and time window
- Uses pagination (500 events per request) to traverse all active markets
- Polls every 5 seconds until suitable market found

**`websocket.rs`** (src/websocket.rs:1)
- `DataStream` subscribes to order book updates via WebSocket
- WebSocket URL loaded from `CLOB_WS_MARKET` environment variable
- Monitors two token IDs (up/down) for bid/ask price changes
- Triggers `engine.process_tick()` on each market update
- Runs until market end date

**`websocket_user.rs`** (src/websocket_user.rs:1)
- `UserStream` receives authenticated trade fill events
- Calls `engine.handle_ws_trade()` to update portfolio on fills
- Critical for tracking actual executed trades vs placed orders

**`models.rs`** (src/models.rs:1)
- Data structures:
  - `Portfolio`: Position tracking (shares, spent, trade counts)
  - `MarketPrices`: Current bid/ask for both sides
  - `TargetMarket`: Selected market metadata
  - `Side` enum: Up/Down position direction
  - `TradeRecord`, `GlobalSummary`: Reporting structures

**`report.rs`** (src/report.rs:1)
- `Reporter` logs trades to `results/{slug}.txt`
- Maintains global statistics in `results/global_summary.json`
- Tracks PnL, win rate, min/max profits per event

### Trading Strategy Flow

```
AutoScanner finds market matching "btc-updown-15m" in 0-15 min window
    ↓
RealEngine initialized with up_token & down_token
    ↓
DataStream (WebSocket) → price updates → engine.process_tick()
    ↓
Engine logic:
  - If |skew| >= HEDGE_SIZE: execute taker order (FOK market order)
  - If cost basis risky: emergency cover
  - Otherwise: spawn maker order pairs (5 sec TTL)
    ↓
UserStream (WebSocket) → trade fills → engine.handle_ws_trade()
    ↓
Portfolio updated (shares & spent tracked separately for up/down)
    ↓
On market end: engine.finalize() calculates PnL & updates reports
```

### Concurrency Model

- **Shared state**: `Arc<Mutex<Portfolio>>` and `Arc<RealEngine>` allow safe concurrent access
- **Background tasks**: `tokio::spawn` for:
  - Maker order placement and cancellation (src/engine.rs:98)
  - Taker order execution (src/engine.rs:152)
- **Stream handling**: `tokio::select!` runs market and user streams concurrently (src/main.rs:79)

### Order Types

**Maker Orders** (src/engine.rs:99-143):
- Limit orders placed just above best bid
- Paired orders on both sides to capture spread
- 5-second lifecycle before cancellation
- Skipped if potential pair cost >= 0.99 (no profit margin)

**Taker Orders** (src/engine.rs:146-174):
- Market orders with FOK (Fill or Kill) execution
- Used for hedging when skew exceeds threshold
- Emergency exits when position becomes unprofitable

### Authentication Flow

1. Load credentials from `.env` (src/main.rs:23-29)
2. Create `PrivateKeySigner` with Polygon chain ID (src/main.rs:31)
3. Authenticate CLOB client with Proxy signature type (src/main.rs:36-41)
4. Authenticate WebSocket client with credentials (src/main.rs:45)

### Critical Implementation Details

**Position Skew Calculation** (src/engine.rs:51):
```rust
let skew = port.up_shares - port.down_shares;
```
Positive skew = long UP, negative skew = long DOWN

**Emergency Cover Logic** (src/engine.rs:74-78):
```rust
if port.up_avg() + prices.down_ask > 1.05 {
    // Close UP position by buying DOWN
}
```
Triggers when combined cost would exceed profit threshold

**Maker Order Pricing** (src/engine.rs:90-91):
```rust
let up_price = if prices.up_bid_size > 100.0 {
    prices.up_bid + 0.01
} else {
    prices.up_bid
};
```
Adds 1 cent spread only when sufficient liquidity exists

## Dependencies

**Core SDK**: `polymarket-client-sdk` v0.3.1 with WebSocket support
**Signing**: `alloy` v1.2.1 for Ethereum transaction signing
**Async**: `tokio` v1.48.0 with full feature set
**WebSocket**: `tokio-tungstenite` v0.28.0
**Precision**: `rust_decimal` v1.39.0 for financial calculations
**HTTP**: `reqwest` v0.12.28 for REST API calls

## Output Files

- `results/{event_slug}.txt` - Per-event trade log with formatted table
- `results/global_summary.json` - Aggregate statistics across all events
- Both directories in `.gitignore` to avoid committing trading data

## Notes for Development

- All comments in code are in Russian (Cyrillic)
- The bot uses nightly Rust toolchain (1.94.0-nightly specified in project)
- Market WebSocket URL loaded from `CLOB_WS_MARKET` environment variable (src/main.rs:30)
- User WebSocket URL loaded from `CLOB_WS_USER` environment variable
- Target market search uses specific slug prefix: "btc-updown-15m"
- Market scanning window: 0-15 minutes before event end
- No automated restarts - single session per execution
