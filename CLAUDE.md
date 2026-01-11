# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

MMDNA is a Rust-based automated market-making bot for Polymarket binary prediction markets. It implements a hedging strategy that combines passive maker orders with active hedging to maintain neutral positions while capturing spread.

**Language**: Rust (Edition 2024)
**Runtime**: Tokio async
**Target Platform**: Polymarket CLOB (Central Limit Order Book)

## Common Commands

```bash
# Build and run
cargo build --release    # Production build
cargo run                # Run in debug mode (development)
cargo check              # Fast compilation check without binary

# The bot runs interactively with simplified menu:
# - Option 1: Start (begins in dryrun mode by default)
# - Option 2: Exit
#
# During execution:
# - Press 'r' to toggle trading ON/OFF (dryrun mode)
# - Press 'q' to exit to menu
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

The codebase is organized into 8 modules with clear separation of concerns:

**`main.rs`** (src/main.rs:1)
- Entry point with Tokio async runtime
- Authenticates with Polymarket CLOB REST API and WebSocket API
- Interactive CLI menu: "1. Start | 2. Exit"
- Trading mode defaults to OFF (dryrun) on startup
- Keyboard controls during execution:
  - 'r' toggles `trading_enabled` state (persists between events)
  - 'q' exits to main menu
- Runs three concurrent streams using `tokio::select!`:
  - Market data stream (order book updates)
  - User event stream (trade fill confirmations)
  - Coinbase stream (BTC/ETH/SOL/XRP price tracking)

**`engine.rs`** (src/trading/engine.rs:1)
- Core trading logic in `RealEngine` struct
- Manages portfolio state via `Mutex<Portfolio>`
- Checks `ui_state.trading_enabled` flag before executing any trades
- Trading behaviors (currently HEDGING DISABLED):
  1. **Hedging** (DISABLED): Would execute taker orders when position skew exceeds `HEDGE_SIZE`
  2. **Emergency cover** (DISABLED): Would close positions if cost basis risky
  3. **Maker orders** (ACTIVE): Places paired limit orders on both sides
- Configuration loaded from `config.toml`: `MAX_BALANCE`, `SIZE`, `HEDGE_SIZE`

**`scanner.rs`** (src/trading/scanner.rs:1)
- `AutoScanner` polls Polymarket API for trading opportunities
- Searches for events matching slug prefix and time window
- Uses pagination (500 events per request) to traverse all active markets
- Polls every 5 seconds until suitable market found

**`websocket/market.rs`** (src/websocket/market.rs:1)
- `DataStream` subscribes to order book updates via WebSocket
- WebSocket URL loaded from `CLOB_WS_MARKET` environment variable
- Monitors two token IDs (up/down) for bid/ask price changes
- Updates UI order book display in real-time
- Triggers `engine.process_tick()` on each market update
- Runs until market end date

**`websocket/user.rs`** (src/websocket/user.rs:1)
- `UserStream` receives authenticated trade fill events
- Calls `engine.handle_ws_order()` for MAKER orders (PLACEMENT/UPDATE/CANCELLATION)
- Calls `engine.handle_ws_trade()` for TAKER order fills
- Critical for tracking actual executed trades vs placed orders

**`websocket/coinbase.rs`** (src/websocket/coinbase.rs:1)
- `CoinbaseStream` subscribes to Coinbase price feed
- Tracks real-time prices for BTC/ETH/SOL/XRP
- Updates `ui_state.current_price` for comparison with `price_to_beat`

**`ui/mod.rs`** (src/ui/mod.rs:1)
- TUI (Terminal User Interface) using `ratatui` and `crossterm`
- **UiStateInner** contains:
  - `trading_enabled`: Controls whether bot executes trades (false = dryrun)
  - `event_info`: Market title, end date, progress
  - `portfolio`: Position tracking (shares, spent, P&L)
  - `up_book`/`down_book`: Order book depth (5 levels)
  - `price_to_beat`: Previous event closing price
  - `current_price`: Live Coinbase price
- **KeyAction enum**: None, Exit, ToggleTrading
- `check_key_action()`: Polls for 'q' (exit) and 'r' (toggle trading)
- `toggle_trading()`: Flips `trading_enabled` flag and logs state change
- Renders 4 panels: MARKET info, PORTFOLIO, Activity Log, ORDER BOOKs

**`models.rs`** (src/models.rs:1)
- Data structures:
  - `Portfolio`: Position tracking (up_shares, down_shares, up_spent, down_spent, maker_trades, taker_trades)
  - `MarketPrices`: Current bid/ask/size for both UP and DOWN sides
  - `TargetMarket`: Selected market metadata (slug, title, tokens, end_date)
  - `Side` enum: Up/Down position direction
  - `Coin` enum: BTC, ETH, SOL, XRP with slug_prefix() mapping

**`config.rs`** (src/config.rs:1)
- Loads `config.toml` with trading parameters:
  - `max_balance`: Maximum total capital to deploy
  - `size`: Order size per trade
  - `hedge_size`: Threshold for hedging (currently unused)

**`price_tracker.rs`** (src/price_tracker.rs:1)
- Persists closing prices from previous events to `price_tracker.json`
- Provides `price_to_beat` for next event of same coin
- Extracts timestamp from event slug to match historical data

### Trading Strategy Flow

```
User selects coin (BTC/ETH/SOL/XRP) from menu
    ↓
AutoScanner finds market matching "<coin>-updown-15m" in 0-15 min window
    ↓
RealEngine initialized with up_token & down_token
    ↓
UI displays: MARKET info, PORTFOLIO, ORDER BOOKs, Activity Log
Trading status: OFF (dryrun mode) - shown in red
    ↓
User presses 'r' → toggle trading_enabled to TRUE
UI updates: Trading: ON - shown in green
    ↓
DataStream (WebSocket) → price updates → engine.process_tick()
    ↓
Engine checks: if !trading_enabled → skip order placement (dryrun)
            if trading_enabled → execute trading logic
    ↓
Trading logic (HEDGING CURRENTLY DISABLED):
  - Check if total spent < MAX_BALANCE
  - Calculate potential pair cost
  - If profitable: spawn maker order pairs (GTC orders)
  - Anti-spam: limit 3 placements at same price levels
    ↓
UserStream (WebSocket) → MAKER events → engine.handle_ws_order()
                       → TAKER fills → engine.handle_ws_trade()
    ↓
Portfolio updated (shares & spent tracked separately for up/down)
UI shows real-time position, avg cost, P&L projection
    ↓
Event ends → Coinbase price determines winner
Price tracker saves closing price for next event
User can press 'q' to exit or continue to next event with same trading_enabled state
```

### Concurrency Model

- **Shared state**:
  - `Arc<Mutex<UiStateInner>>` for UI updates and trading_enabled flag
  - `Arc<RealEngine>` contains `Mutex<Portfolio>` for position tracking
- **Background tasks**: `tokio::spawn` for:
  - Maker order placement (src/trading/engine.rs:259-292)
  - Asynchronous order submission via Polymarket CLOB API
- **Stream handling**: Two parallel task trees:
  - `trading_task`: market_stream + user_stream + coinbase_stream (tokio::select!)
  - `ui_task`: Keyboard polling + UI rendering loop (100ms refresh rate)
- **Inter-task communication**: Shared `ui_state` for trading_enabled synchronization

### Order Types

**Maker Orders** (src/trading/engine.rs:164-293):
- Limit orders placed at or just above best bid
- Paired orders on both UP and DOWN sides to capture spread
- Order type: GTC (Good Till Canceled) - no expiration
- Pricing strategy:
  - If bid_size > 100: place at bid + 0.01 (create new best bid)
  - If bid_size ≤ 100: place at current bid (join queue)
- Skipped if potential pair cost (up_price + down_price) >= 1.00
- Anti-spam protection: max 3 placements at same price levels
- Only executed when `trading_enabled = true`

**Taker Orders** (CURRENTLY DISABLED):
- Would be FAK (Fill and Kill) market orders
- Would be used for hedging when skew exceeds HEDGE_SIZE threshold
- Code present but commented out in engine.rs

### Authentication Flow

1. Load credentials from `.env` (src/main.rs:23-29)
2. Create `PrivateKeySigner` with Polygon chain ID (src/main.rs:31)
3. Authenticate CLOB client with Proxy signature type (src/main.rs:36-41)
4. Authenticate WebSocket client with credentials (src/main.rs:45)

### Critical Implementation Details

**Trading Enable Check** (src/trading/engine.rs:106-115):
```rust
pub fn process_tick(&self, prices: MarketPrices) {
    let trading_enabled = {
        let state = self.ui_state.lock().unwrap();
        state.trading_enabled
    };
    if !trading_enabled {
        return; // Skip all trading logic in dryrun mode
    }
    self.run_logic(prices);
}
```
Engine reads `trading_enabled` from shared UI state before every tick

**Trading State Toggle** (src/ui/mod.rs:117-121):
```rust
pub fn toggle_trading(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        s.trading_enabled = !s.trading_enabled;
    }
}
```
Called when user presses 'r' key. State persists across events.

**Maker Order Pricing** (src/trading/engine.rs:214-223):
```rust
let up_price = if prices.up_bid_size > 100.0 {
    Self::round_price(prices.up_bid + 0.01)
} else {
    Self::round_price(prices.up_bid)
};
```
Dynamic pricing based on queue depth. Avoid competing when queue > 100 shares.

## Dependencies

**Core SDK**: `polymarket-client-sdk` v0.3.1 with WebSocket support
**Signing**: `alloy` v1.2.1 for Ethereum transaction signing
**Async**: `tokio` v1.48.0 with full feature set
**WebSocket**: `tokio-tungstenite` v0.28.0
**Precision**: `rust_decimal` v1.39.0 for financial calculations
**HTTP**: `reqwest` v0.12.28 for REST API calls
**TUI**: `ratatui` v0.29.0 + `crossterm` v0.28.1 for terminal UI
**Config**: `toml` v0.8.19 for configuration file parsing
**Logging**: `tracing` v0.1.41 + `tracing-subscriber` v0.3.19

## Persistent Files

- `config.toml` - Trading parameters (max_balance, size, hedge_size)
- `price_tracker.json` - Historical closing prices per coin and timestamp
- `logs/app.log` - Continuous append-only log file (no rotation)
- `.env` - API credentials and WebSocket URLs (gitignored)

## Trading Mode Control

**Default Behavior**:
- Bot starts with `trading_enabled = false` (dryrun mode)
- All order placement logic is skipped
- Portfolio tracking and UI updates continue normally

**Toggle During Execution**:
- Press 'r' key to toggle `trading_enabled`
- State change logged: "🟢 ТОРГОВЛЯ ВКЛЮЧЕНА" / "🔴 ТОРГОВЛЯ ВЫКЛЮЧЕНА"
- UI indicator updates: "Trading: ON" (green) / "Trading: OFF" (red)
- Flag persists between events in same session

**State Persistence**:
- When event ends, `trading_enabled` is preserved
- Next event inherits the current trading mode
- Only resets to false when exiting to main menu

## Notes for Development

- All comments in code are in Russian (Cyrillic)
- The bot uses Rust Edition 2024 (nightly toolchain)
- Market WebSocket URL loaded from `CLOB_WS_MARKET` environment variable
- User WebSocket URL loaded from `CLOB_WS_USER` environment variable
- Target market search: dynamic slug prefix based on coin selection
  - BTC: "btc-updown-15m"
  - ETH: "eth-updown-15m"
  - SOL: "sol-updown-15m"
  - XRP: "xrp-updown-15m"
- Market scanning window: 0-15 minutes before event end
- No automated restarts - single session per execution
- Hedging and emergency cover logic currently disabled (commented out)
