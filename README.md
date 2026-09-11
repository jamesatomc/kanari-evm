<!-- Copyright (c) KanariNetwork, Inc. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kanari EVM

Single-node Kanari EVM dev chain: instant-seal execution over `revm`,
Mysticeti DAG transaction ordering, and a wallet-compatible Ethereum
JSON-RPC server — with Falcon-512 / Dilithium3 post-quantum verification
precompiles on top.

- Chain id `19088`, 1 gwei base fee, newest hardfork active at genesis.
- One signed transaction = one sealed block, persisted to RocksDB and
  covered by a sparse-Merkle state commitment.
- MetaMask-compatible RPC at `http://127.0.0.1:8545/` plus a bundled block
  explorer at `GET /`.

Docs: [`RUN_GUIDE.md`](RUN_GUIDE.md) (runbook) ·
[`ARCHITECTURE.md`](ARCHITECTURE.md) (module layout, RPC coverage,
invariants).

## Quickstart

Prerequisites: stable Rust (`rustup`), Windows or Linux.

```powershell
# Local-only dev node: RPC on 127.0.0.1:8545, data in ./.kanari-evm-local
.\start-evm-node.ps1 -Command local

# Persistent node (data in ~/.kanari/evm-devnet), reachable on the LAN
.\start-evm-node.ps1 -Command start -RpcHost 0.0.0.0 -RpcPort 8546

# Fresh genesis (wipes state.json, *.chain.db, *.faucet.key)
.\target\debug\kanari-evm-node.exe reset --force
```

Direct binary equivalents:

```powershell
cargo run -p kanari-evm --bin kanari-evm-node -- local
cargo run -p kanari-evm --bin kanari-evm-node -- start --network devnet `
  --rpc-port 8546 --rpc-host 0.0.0.0 --data-dir D:\evm-data
```

Add the network to MetaMask with RPC URL `http://<PC-LAN-IP>:8545` and
chain id `19088` (a phone's `127.0.0.1` is itself, not this PC — bind
`0.0.0.0` for phone wallets on the same Wi-Fi).

## Faucet (dev only, no auth)

A fresh node prints its faucet account on first start. Fund any address:

```powershell
# via RPC params [address, whole-ETH]
curl http://127.0.0.1:8545/ -H "Content-Type: application/json" `
  -d '{"jsonrpc":"2.0","id":1,"method":"kanari_faucet","params":["0xYourAddress","100"]}'
```

Capped per request; the key persists in the data dir so restarts keep
working. **Never use a real key with `--faucet-key`.**

## Repository layout

```text
src/
├── bin/kanari-evm-node.rs  — CLI: start / local / reset
├── evm_execution/          — chain spec, PQC precompiles, KanariNode engine
├── core_consensus/         — DAG ordering + RocksDB chain store
└── server_rpc/             — JSON-RPC server + explorer UI
tests/                      — e2e: wallet flow, contracts, PQC, DAG, SMT proofs
```

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full picture.

## Verify

```powershell
cargo clippy --all-targets -- -D warnings
cargo test

# Optional: same contract flow against a live node
$env:KANARI_EVM_LIVE_RPC = "http://127.0.0.1:8545"
cargo test -p kanari-evm --test contracts live_
```

CI runs clippy + tests on Windows and Linux
(`.github/workflows/ci.yml`).

## Notes

- `testnet` / `mainnet` have no EVM chain spec yet — `start` fails fast
  outside `devnet` instead of silently running dev parameters.
- Block hashes are deterministic placeholders, receipts carry no logs
  (`eth_getLogs` returns `[]`), and only the current state is queryable —
  this is a dev chain, not a full L1.
- The full `reth` node (sync, MDBX, networking) stays a Linux/docker
  target; execution here builds on the published `revm` interpreter, which
  is Windows-clean (see `Cargo.toml`).
