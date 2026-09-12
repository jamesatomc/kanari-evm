<!-- Copyright (c) KanariNetwork, Inc. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kanari EVM node — run guide

Mirrors the `kanari-node` run system: clap subcommands, boot banner,
standard data dir, and a `start-*.ps1` launcher.

## Commands

```powershell
# Build once
cargo build -p kanari-evm-node

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

## Multi-node (4 validators on one machine)

```powershell
# One terminal window per validator (DAG 3500+i*10, RPC = DAG+1)
.\start-validators.ps1 -NodeCount 4

# Same steps manually:
.\target\debug\kanari-evm-node.exe keygen --node-count 4 --output-dir ./dag-keys
.\target\debug\kanari-evm-node.exe validator --committee ./dag-keys/dag-committee.json `
  --key ./dag-keys/validator-1.key --data-dir ./.kanari-evm-validator-1 --rpc-port 3501
# validators 2..4: same, with their own key / data dir / RPC port

# Fresh multi-node genesis (wipe committee + all validator state)
Remove-Item -Recurse -Force ./dag-keys, ./.kanari-evm-validators
```

Rules: all validators share `dag-committee.json` and byte-identical
genesis (validator mode never auto-creates a faucet — pass the same
`--faucet-key` everywhere or none). Submit a raw tx to any validator's
RPC; every validator seals it once a DAG commit covers it, with identical
block hashes and state roots. See `ARCHITECTURE.md` for the convergence
design.

## Multi-machine (validators on separate hosts)

Same binaries, real IPs instead of localhost. On any one machine:

```powershell
# Use the LAN IPs of the 4 hosts (order fixes validator numbers 1..4)
# NOTE: keygen writes one committee for the whole network — do this once.
.\target\debug\kanari-evm-node.exe keygen --node-count 4 --output-dir ./dag-keys `
  --host 192.168.1.11 --base-dag-port 3500
```

`--host` sets every entry's IP; for mixed IPs, edit `dag-committee.json`
`dag_address` fields by hand (one per validator), keeping the ports.

Then copy to each host: `dag-committee.json` (same file everywhere) plus
that host's `validator-{i}.key`. On host `i`:

```powershell
.\kanari-evm-node.exe validator --committee ./dag-keys/dag-committee.json `
  --key ./dag-keys/validator-2.key --data-dir ./evm-data `
  --rpc-host 0.0.0.0 --rpc-port 3511
```

Checklist per host:

- Firewall: allow inbound TCP on the validator's DAG port (default
  `3500 + (i-1)*10`) from the other validators, and the RPC port from
  wallets.
- Clocks: keep NTP on. Block timestamps come from commit anchors and are
  clamped monotonic, so small skew is harmless — but hours of skew makes
  block times nonsense.
- Same `--faucet-key` on all hosts (or none), otherwise genesis — and
  state roots — diverge from block 0.
- Start order does not matter; late joiners sync missing DAG blocks from
  peers automatically.

## Data layout (`--data-dir`)

| File             | Purpose                                    |
| ---------------- | ------------------------------------------ |
| `state.json`     | Replay journal (delete = reset to genesis) |
| `*.chain.db/`    | RocksDB: blocks, receipts, SMT, faucet     |
| `*.faucet.key`   | Dev faucet secret — **never commit**       |

## Notes

- `testnet`/`mainnet` have no EVM chain spec yet — `start` fails fast
  outside `devnet` instead of running dev parameters silently.
- Genesis holds the FULL supply at the dev account (`0xC88C…`); the faucet
  gets nothing — fund it with a transfer from the dev account first.
- `--faucet 0xAddr [eth]` funds extra genesis accounts (fresh state only).
- `--faucet-key <32-byte-hex>` pins the faucet to your own dev key.
- Dev faucet RPC: `kanari_faucet(["0xAddr","5"])` — dev only, no auth.
- Fees: no burn — beneficiary (`0x7985…`) takes base + priority per tx.
- `--log-level debug` for RPC traffic; Ctrl+C drains cleanly.
- Validator TOML: `kanari-evm-node validator --config node1.toml`
  (see `ValidatorFileConfig` docs in `crates/kanari-node/src/main.rs`).
- Live deploy test: `KANARI_EVM_LIVE_RPC=http://127.0.0.1:8545 cargo test
  -p kanari-evm-node --test contracts live_`.
