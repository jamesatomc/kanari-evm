<!-- Copyright (c) KanariNetwork, Inc. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kanari EVM — Architecture

Single-node Kanari EVM dev chain: instant-seal execution over `revm`,
Mysticeti DAG transaction ordering, and a minimal Ethereum JSON-RPC server.
Fresh chains activate the newest hardfork (`AMSTERDAM`) at genesis — there
is no fork history to replay.

## Module layout

```text
src/
├── lib.rs                  — crate facade + backward-compat re-exports
├── bin/kanari-evm-node.rs  — node entry point (clap CLI)
├── evm_execution/          — EVM execution layer
│   ├── node.rs             — lifecycle, persistence glue, chain accessors
│   ├── execution.rs        — tx validation, revm execution, instant sealing
│   ├── views.rs            — JSON block / transaction / receipt views
│   ├── faucet.rs           — dev faucet (no auth, capped)
│   ├── state.rs            — state reads + SMT state commitment
│   ├── chainspec.rs        — chain spec + genesis allocations
│   ├── precompiles.rs      — Falcon-512 / Dilithium3 verify precompiles
│   └── contracts.rs        — hand-assembled demo contracts (no solc)
├── core_consensus/         — consensus & durability
│   ├── ordering.rs         — Mysticeti DAG ordering driver (in-process)
│   ├── committee.rs        — validator committee files (`keygen`)
│   ├── validator.rs        — networked DAG validator + commit execution
│   └── store.rs            — RocksDB chain store (blocks, receipts, meta)
└── server_rpc/             — RPC server
    ├── rpc.rs              — Ethereum JSON-RPC over HTTP (axum)
    └── explorer.html       — bundled single-file block explorer
```

> Rust module names cannot contain `-`, so the folders use underscores
> (`evm_execution`, `core_consensus`, `server_rpc`). The old flat paths
> (`kanari_evm::node`, `kanari_evm::rpc`, `kanari_evm::ordering`, …) are
> kept as re-export aliases in `lib.rs`; new code should use the grouped
> paths (`kanari_evm::evm_execution::node`, …).

## Execution (`evm_execution`)

`KanariNode` (`node.rs`) owns the in-memory `revm` state, the sealed-block
log, and the RocksDB-backed `ChainStore` / SMT commitment. Its behavior is
split by concern: `execution.rs` decodes and executes signed raw
transactions (`send_raw_transaction`), seals one block per transaction, and
replays sealed blocks on startup; `state.rs` serves point reads and rebuilds
the sparse Merkle tree commitment after every transition; `views.rs`
renders RPC shapes; `faucet.rs` holds the dev drip account.

Chain parameters: chain id `19088`, 1 gwei base fee (Anvil-style),
30M block gas limit, priority fees to the treasury beneficiary
(`0x7985…ccA`), base fee burned per EIP-1559.

Post-quantum precompiles (`precompiles.rs`, à la EIP-8052/8053):

| Address | Scheme                  | Gas   |
| ------- | ----------------------- | ----- |
| `0x100` | Falcon-512 (FN-DSA)     | 3,000 |
| `0x101` | Dilithium3 (ML-DSA-65)  | 5,000 |

Input layout is `pubkey_len:u16BE \|\| pubkey \|\| msg_len:u16BE \|\|
msg \|\| sig`. Output is a 32-byte big-endian boolean (`0`/`1`); any
failure — malformed input, wrong lengths, bad signature — returns `0`
(fail-closed, ecrecover-style). Gas is charged in full regardless.

## Consensus & durability (`core_consensus`)

`ordering.rs` runs N in-process Mysticeti validator cores over a full mesh,
commits through the standard committer, and yields linearized payload
batches (opaque signed EVM transactions) for execution. Networked
multi-process validators reuse the same `Core`/`Committer` types; that
transport is a follow-up phase.

`store.rs` persists blocks, receipts, tx index, and chain metadata in a
single RocksDB (default column family, prefixed keys so the SMT layer never
collides). A block seal commits atomically in one `WriteBatch`. Legacy
pre-RocksDB JSON journals are migrated forward exactly once on open, then
renamed to `*.migrated.json`. Every open replays the store deterministically
— any replay divergence is a hard error, never silent.

State commitment: account and storage-slot keys are
`BLAKE3("kanari-evm-smt-v1" || tag || …)` leaves in a sparse Merkle tree
sharing the chain RocksDB; only the current root is queryable
(`kanari_getSmtProof`, `eth_getStorageAt`).

## RPC (`server_rpc`)

JSON-RPC at `POST /`, explorer UI at `GET /`. Single requests and batch
arrays are accepted (wallets batch on load). Method coverage:

| Method | Notes |
| ------ | ----- |
| `web3_clientVersion`, `net_version`, `net_listening` | static / chain id |
| `eth_chainId`, `eth_blockNumber`, `eth_syncing` | head state |
| `eth_getBalance`, `eth_getTransactionCount` | live state |
| `eth_getCode`, `eth_getStorageAt` | live state (storage: 32-byte padded hex) |
| `eth_gasPrice`, `eth_maxPriorityFeePerGas`, `eth_feeHistory` | static 1 gwei schedule |
| `eth_sendRawTransaction` | validate → execute → instant-seal |
| `eth_getTransactionByHash`, `eth_getTransactionReceipt` | sealed data, `null` when unknown |
| `eth_getBlockByNumber`, `eth_getBlockByHash` | incl. synthetic empty genesis (block 0) |
| `eth_call`, `eth_estimateGas` | read-only, never seals |
| `eth_getLogs` | always `[]` — receipts carry no logs; keeps dApps loading |
| `kanari_faucet` | dev drip, no auth |
| `kanari_supply` | genesis-sum circulating + 11M protocol cap |
| `kanari_getSmtProof` | account / storage-slot inclusion proofs |

Anything else returns `-32601 Method not found`. Block hashes are
deterministic `keccak256(parent || number || timestamp)` placeholders —
accepted by wallets, but NOT full L1 validity proofs.

## Multi-node validators

`kanari-evm-node validator` runs one process in a multi-validator network,
mirroring the `kanari-node` topology from kanari-sdk:

```powershell
kanari-evm-node keygen --node-count 4 --output-dir ./dag-keys --base-dag-port 3500
kanari-evm-node validator --committee ./dag-keys/dag-committee.json `
  --key ./dag-keys/validator-1.key --data-dir ./.kanari-evm-validator-1
# …or all at once, one terminal per validator:
.\start-validators.ps1 -NodeCount 4
```

- **Committee files**: `dag-committee.json` (validator ids, DAG socket
  addresses, Ed25519 pubkeys) is shared; each validator holds its own
  `validator-{i}.key` secret. Authority ids are 1-based (`0x1`, …) like
  kanari-sdk. Keep `(base + (count-1) * 10) * 10 <= 65535` — the Mysticeti
  TCP mesh dials out from source port `listen * 10`.
- **Mempool**: while attached, `send_raw_transaction` submits to the local
  DAG mempool and returns the tx hash immediately; receipts appear once a
  commit covers the transaction.
- **Execution**: every commit seals with its **anchor block timestamp**
  (never wall-clock), so block hashes and state roots converge on all
  validators. Payloads that fail today but may succeed tomorrow (e.g. nonce
  gaps) wait in a deterministic pending queue swept on every commit —
  standard queued-tx mempool behavior, identical everywhere because the
  commit sequence is identical.
- **Genesis discipline**: all validators must start from byte-identical
  genesis or state roots diverge from block 0. Validator mode therefore
  never auto-creates a faucet; pass the SAME `--faucet-key` to every
  validator (or none) for a shared faucet account.

## Invariants (do not break)

1. **Replay determinism** — reopening the same data dir must reproduce
   identical state, or fail loudly (`replay diverged`, `state root
   mismatch`, chain-id mismatch).
2. **Fail-closed crypto** — precompile and faucet failures return `0` /
   errors, never success on bad input.
3. **No post-genesis minting** — circulating supply always equals the
   genesis-allocation sum; fees only move value to the beneficiary.
4. **Key disjointness** — chain-store keys (`m:`/`b:`/`r:`/`t:`) must never
   collide with SMT keys (`n:`/`d:` + roots).
5. **Validator convergence** — given identical genesis and identical
   commits, every validator must seal identical blocks: seal commits with
   the anchor timestamp, never wall-clock, and keep the pending queue
   (order, cap, drop policy) a pure function of the commit sequence.

## Verification

```powershell
cargo clippy --all-targets -- -D warnings
cargo test
```

CI (`.github/workflows/ci.yml`) runs both on `windows-latest` and
`ubuntu-latest`. Integration tests bind ephemeral ports (`127.0.0.1:0`),
so they are parallel-safe.
