<!-- Copyright (c) KanariNetwork, Inc. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kanari EVM node — run guide

Mirrors the `kanari-node` run system: clap subcommands, boot banner,
standard data dir, and a `start-*.ps1` launcher.

## Commands

```powershell
# Build once
cargo build -p kanari-evm --bin kanari-evm-node   # from crates/kanari-evm

# Local-only dev node (127.0.0.1:8545, data in ./.kanari-evm-local)
.\start-evm-node.ps1 -Command local

# Persistent node (default data dir ~/.kanari/evm-devnet)
.\start-evm-node.ps1 -Command start -RpcHost 0.0.0.0 -RpcPort 8546

# Same via the binary directly
.\target\debug\kanari-evm-node.exe start --network devnet `
  --rpc-port 8546 --rpc-host 0.0.0.0 --data-dir D:\evm-data

# Fresh genesis (deletes state.json, *.chain.db, *.faucet.key)
.\target\debug\kanari-evm-node.exe reset --force
```

Direct RPC: `http://127.0.0.1:8545/` (chain id `19088` / `0x4a90`).
LAN/phone wallets: bind `0.0.0.0` and use the PC's LAN IP (a phone's
`127.0.0.1` is itself, not this PC).

## Data layout (`--data-dir`)

| File             | Purpose                                    |
| ---------------- | ------------------------------------------ |
| `state.json`     | Replay journal (delete = reset to genesis) |
| `*.chain.db/`    | RocksDB: blocks, receipts, SMT, faucet     |
| `*.faucet.key`   | Dev faucet secret — **never commit**       |

## Notes

- `testnet`/`mainnet` have no EVM chain spec yet — `start` fails fast
  outside `devnet` instead of running dev parameters silently.
- `--faucet 0xAddr [eth]` funds extra genesis accounts (fresh state only).
- `--faucet-key <32-byte-hex>` pins the faucet to your own dev key.
- Dev faucet RPC: `kanari_faucet(["0xAddr","5"])` — dev only, no auth.
- Live deploy test: `KANARI_EVM_LIVE_RPC=http://127.0.0.1:8545 cargo test
  -p kanari-evm --test contracts live_`.
