# evm

EVM blockchain interaction layer for ethpayserver.

## Features

- **Network configs** - Pre-defined settings for 12 EVM mainnets and 9 testnets
- **HD wallet** - BIP-32/44 address derivation for payment invoices
- **RPC provider** - Alloy-based provider for blockchain interaction
- **Token support** - ERC20, ERC721, ERC1155 standards
- **Payment monitor** - Real-time block monitoring via WebSocket
- **Event bridge** - Redis pub/sub for distributed deployments

## Supported Networks

| Network | Chain ID | Native Token |
|---------|----------|--------------|
| Ethereum | 1 | ETH |
| Polygon | 137 | POL |
| Arbitrum | 42161 | ETH |
| Optimism | 10 | ETH |
| Base | 8453 | ETH |
| Avalanche | 43114 | AVAX |
| BNB Smart Chain | 56 | BNB |
| zkSync Era | 324 | ETH |
| Linea | 59144 | ETH |
| Scroll | 534352 | ETH |
| Fantom | 250 | FTM |
| Gnosis | 100 | xDAI |

Plus 9 testnets (Sepolia, Holesky, Hoodi, Polygon Amoy, Arbitrum Sepolia,
Optimism Sepolia, Base Sepolia, Avalanche Fuji, BSC Testnet) — see
`evm::testnet::ALL_TESTNETS`.

## Usage

```rust
use evm::{EvmNetwork, HdWallet, EvmProvider};

// Create wallet from mnemonic
let wallet = HdWallet::from_mnemonic("your mnemonic here", "")?;

// Derive payment address
let address = wallet.derive_address(0)?;

// Connect to network
let provider = EvmProvider::new(EvmNetwork::Ethereum, "https://eth.llamarpc.com").await?;

// Check balance
let balance = provider.get_native_balance(address).await?;
```

## Modules

| Module | Description |
|--------|-------------|
| `network` | `EvmNetwork` enum and mainnet `ChainConfig`s |
| `testnet` | Testnet `ChainConfig`s (`ALL_TESTNETS`) |
| `family` | EVM/Tron chain-family logic shared across chain namespaces |
| `wallet` | BIP-32/44 HD wallet derivation |
| `provider` | Alloy RPC provider wrapper |
| `tokens` | `EvmTokenStandard`, ERC20 operations |
| `monitor` | Payment monitoring system |
| `metrics` | RPC call metrics |
| `telemetry` | Sentry PII/secret scrubber shared by `server` and `evmmonitor` (feature: `sentry-scrub`) |
| `transaction` | Signing/broadcasting infrastructure, reserved for a possible future hot-wallet mode (feature: `hot-wallet`, default off) |
| `error` | `EvmError` and `EvmResult` |
| `api` | REST API endpoints (feature: `api`) |

Other Cargo features: `redis` (event bridge), `test-utils`, `monitor-bin`
(builds the `evmmonitor` binary).

## Payment Monitor (`evmmonitor` binary)

Standalone binary that monitors EVM chains for payments and publishes events to Redis.

### Architecture

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│  evmmonitor     │     │  evmmonitor     │     │  evmmonitor     │
│  (Ethereum)     │     │  (Polygon)      │     │  (Arbitrum)     │
└────────┬────────┘     └────────┬────────┘     └────────┬────────┘
         │ WebSocket             │ WebSocket             │ WebSocket
         │ subscription          │ subscription          │ subscription
         └───────────────────────┼───────────────────────┘
                                 │ Redis pub/sub
                                 ▼
                       ┌─────────────────┐
                       │  ethpayserver   │
                       │  (API server)   │
                       └─────────────────┘
```

### Build

```bash
cargo build --release --bin evmmonitor --features monitor-bin
```

### Run

```bash
# Environment variables
EVMMONITOR_REDIS_URL=redis://localhost:6379 \
EVMMONITOR_CHAINS=1,137,42161 \
EVMMONITOR_CHAIN_1_RPC_HTTP=https://eth-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_1_RPC_WS=wss://eth-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_137_RPC_HTTP=https://polygon-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_137_RPC_WS=wss://polygon-mainnet.g.alchemy.com/v2/KEY \
./target/release/evmmonitor
```

Or with config file (`evmmonitor.toml`):

```toml
[bridge]
redis_url = "redis://localhost:6379"

[[chains]]
chain_id = 1
rpc_http = "https://eth-mainnet.g.alchemy.com/v2/KEY"
rpc_ws = "wss://eth-mainnet.g.alchemy.com/v2/KEY"

[[chains]]
chain_id = 137
rpc_http = "https://polygon-mainnet.g.alchemy.com/v2/KEY"
rpc_ws = "wss://polygon-mainnet.g.alchemy.com/v2/KEY"
```

```bash
./target/release/evmmonitor --config evmmonitor.toml
```

### Connection Modes

| Mode | Config | Latency | Use Case |
|------|--------|---------|----------|
| **WebSocket** | `rpc_ws` + `rpc_http` | Real-time (~1s) | Production |
| **HTTP Polling** | `rpc_http` only | Block time interval | Fallback |

WebSocket uses `eth_subscribe("newHeads")` for instant block notifications. HTTP polling is automatic fallback if WebSocket fails.

### Bidirectional Communication

The monitor supports bidirectional communication via Redis:

- **Events channel** (`evmmonitor:events`): Monitor -> API server
- **Commands channel** (`evmmonitor:commands`): API server -> Monitor

### Commands (API Server -> Monitor)

- `WatchAddress` - Start watching an address for payments
- `UnwatchAddress` - Stop watching an address
- `GetStatus` - Request status of watched addresses

### Events (Monitor -> API Server)

- `PaymentDetected` - Payment received (unconfirmed)
- `PaymentConfirmed` - Payment reached required confirmations
- `ReorgDetected` - Chain reorganization detected
- `AddressWatched` - Confirmation that address is being watched
- `AddressUnwatched` - Address removed from watch list
- `StatusReport` - Response to GetStatus command
- `MonitorStarted` / `MonitorStopped` - Lifecycle events
- `MonitorError` - Chain-level error (`chain_id`, `error`)

### API Server Integration

```rust
use evm::monitor::{EventBridge, RedisBridge, COMMANDS_CHANNEL, EVENTS_CHANNEL, MonitorEvent, MonitorCommand};
use evm::monitor::events::WatchAddressCommand;
use tokio_stream::StreamExt;

let bridge = RedisBridge::new("redis://localhost:6379", EVENTS_CHANNEL, COMMANDS_CHANNEL).await?;

// Send command to watch an address
let cmd = MonitorCommand::WatchAddress(WatchAddressCommand {
    chain_id: 1,
    address: payment_address,
    invoice_id,
    expected_amount: Some(amount),
    token_contract: None,
});
bridge.publish_command(&cmd).await?;

// Subscribe to events
let mut events = bridge.subscribe().await?;

while let Some(event) = events.next().await {
    match event {
        MonitorEvent::PaymentDetected(p) => { /* update invoice */ }
        MonitorEvent::PaymentConfirmed(p) => { /* complete payment */ }
        MonitorEvent::ReorgDetected(r) => { /* re-evaluate payments */ }
        _ => {}
    }
}
```

## Token Standards

```rust
use evm::EvmTokenStandard;

let standard: EvmTokenStandard = "erc20".parse()?;
standard.validate(Some(18), None)?; // ERC20 needs decimals, no token_id
```

## REST API (feature: `api`)

Enable with `--features api`. Provides axum routes for token and network management.

```rust
use evm::api::{EvmState, router};
use data_service::PgDataService;
use auth::{AuthService, AuthRepository};
use std::sync::Arc;

let data_service = Arc::new(PgDataService::connect("postgres://...").await?);
let auth_service = Arc::new(AuthService::new(auth_repo));
let state = EvmState::new(data_service, auth_service);
let app = Router::new().nest("/evm", router(state));
```

### Token Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/evm/tokens` | List tokens (filter by network, type, symbol) |
| POST | `/evm/tokens` | Create token |
| GET | `/evm/tokens/{id}` | Get token |
| PUT | `/evm/tokens/{id}` | Update token |
| DELETE | `/evm/tokens/{id}` | Delete token |
| PUT | `/evm/tokens/{id}/enabled` | Enable/disable token |

### Network Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/evm/networks` | List all networks |
| GET | `/evm/networks/{id}` | Get network info |
