<!-- Copyright (c) KanariNetwork, Inc. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kanari EVM — Architecture

Single-node Kanari EVM dev chain: instant-seal execution over `revm`,
Mysticeti DAG transaction ordering, and a minimal Ethereum JSON-RPC server.
Fresh chains activate the newest hardfork (`AMSTERDAM`) at genesis — there
is no fork history to replay.

## Crate layout (Cargo workspace)

```text
crates/
├── move-execution/  — EVM execution layer (lib: kanari-evm-move-execution)│   ├── chainspec.rs  — chain spec + genesis allocations
│   ├── precompiles.rs— Falcon-512 / Dilithium3 verify precompiles
│   ├── contracts.rs  — hand-assembled demo contracts (no solc)
│   ├── node.rs       — lifecycle, persistence glue, chain accessors
│   ├── execution.rs  — tx validation, revm execution, instant sealing
│   ├── views.rs      — JSON block / transaction / receipt views
│   ├── faucet.rs     — dev faucet (no auth, capped)
│   └── state.rs      — state reads + SMT state commitment
├── consensus/       — consensus (lib: kanari-evm-consensus)
│   ├── ordering.rs   — Mysticeti DAG ordering driver (in-process)
│   ├── committee.rs  — validator committee files (`keygen`)
│   └── validator.rs  — networked DAG validator + commit execution
├── storage/         — durability (lib: kanari-evm-storage)
│   └── store.rs      — RocksDB store + SealedBlock/StoredReceipt shapes
├── server_rpc/      — RPC server (lib: kanari-evm-rpc)
│   ├── rpc.rs        — Ethereum JSON-RPC over HTTP (axum)
│   └── explorer.html — bundled single-file block explorer
└── kanari-node/     — node binary (bin: kanari-evm-node) + e2e tests
    ├── src/main.rs   — entry point: start / local / reset / keygen / validator
    └── tests/        — wallet flow, contracts, PQC, DAG, SMT proofs, multinode
```

Plus `crates/evm-types` (lib: `kanari-evm-types`) — leaf shared
primitives (hex + QUANTITY formatting, gas economics) that every crate
imports; the only allowed dependency direction into it keeps the graph
acyclic. Integration tests share signing/RPC/temp-dir helpers through
`crates/kanari-node/tests/common/` instead of copying them per file.

Dependencies flow one way (no cycles):
`evm-types` ← `storage` ← `move-execution` ← {`consensus`, `server_rpc`} ← `kanari-node`.
`SealedBlock`/`StoredReceipt` live in `storage`; `SharedNode` lives in
`move-execution::node` and is re-exported by `server_rpc`.

## Execution (`move-execution`)

`KanariNode` (`node.rs`) owns the in-memory `revm` state, the sealed-block
log, and the RocksDB-backed `ChainStore` / SMT commitment. Its behavior is
split by concern: `execution.rs` decodes and executes signed raw
transactions (`send_raw_transaction`), seals one block per transaction, and
replays sealed blocks on startup; `state.rs` serves point reads and rebuilds
the sparse Merkle tree commitment after every transition; `views.rs`
renders RPC shapes; `faucet.rs` holds the dev drip account.
Chain parameters: chain id `19088`, 30M block gas limit, genesis base fee
1 gwei (overridable per chain via `KanariChainSpec::with_base_fee`).
**Dynamic base fee (EIP-1559)**: every block's fee derives from the
parent's fullness (±12.5% around the 15M gas target, 1-wei floor) —
`pending_base_fee()` is a pure function of sealed history, so all
validators quote identically. Stored per block (`SealedBlock.base_fee`);
replay re-seals under the STORED fee so pre-dynamic chains re-verify
byte-for-byte.
**Kanari fee policy — no burn**: the block beneficiary
(`0x7985…ccA`) receives the FULL fee (base + priority) of every
transaction. revm credits the priority share during execution; the base
share (`gas_used × base_fee`, destroyed by vanilla EIP-1559) is credited
to the beneficiary at seal time instead. The sender already paid exactly
that amount, so supply is conserved and every validator computes the
identical credit. Genesis holds the full 11M supply at the dev account
(`0xC88C…`); the faucet gets no genesis allocation and must be funded by
transfer.

Post-quantum precompiles (`precompiles.rs`, à la EIP-8052/8053):

| Address | Scheme                  | Gas   |
| ------- | ----------------------- | ----- |
| `0x100` | Falcon-512 (FN-DSA)     | 3,000 |
| `0x101` | Dilithium3 (ML-DSA-65)  | 5,000 |

Input layout is `pubkey_len:u16BE \|\| pubkey \|\| msg_len:u16BE \|\|
msg \|\| sig`. Output is a 32-byte big-endian boolean (`0`/`1`); any
failure — malformed input, wrong lengths, bad signature — returns `0`
(fail-closed, ecrecover-style). Gas is charged in full regardless.

## Consensus & durability (`consensus` + `storage`)

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
| `eth_getLogs` | real address/topic/block-range filtering over sealed receipts |
| `kanari_faucet` | dev drip, no auth |
| `kanari_supply` | genesis-sum circulating + 11M protocol cap |
| `kanari_getSmtProof` | account / storage-slot inclusion proofs |
| `GET /metrics` | Prometheus counters: head block, seals, txs, DAG commits |

Supported transaction types: legacy (protected), EIP-2930, EIP-1559 and
EIP-7702 (self-sponsored nonces follow revm's validate-then-apply order:
tx.nonce is pre-state, auth.nonce post-caller-bump; relay-sponsored flows
work with independent nonces). EIP-4844 is rejected with a guided error
(no blob mempool — resubmit as type 2/4).

Execution traces: `debug_traceTransaction` replays sealed history into
scratch state (genesis + prior blocks, stored block envs) and runs revm's
EIP-3155 tracer over the full precompile set — historically accurate,
opcode-for-opcode with sealed execution. `debug_traceCall` traces against
current state. Only `disableStack`/`disableMemory` options; no custom
tracers (use the struct logs).

Checkpoint fork: `kanari-evm-node fork --rpc-url URL --account 0x..
--slot 0xAddr:0xSlot` snapshots listed balances, code and slots at one
remote block into genesis, then runs fully local. NOT a live fork:
unlisted storage starts empty, and the faucet is test-minted (no dev
account exists on a fork).

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
   validator (or none) for a shared faucet account — then fund that
   account with a transfer from the dev account before dripping.

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
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI (`.github/workflows/ci.yml`) runs both on `windows-latest` and
`ubuntu-latest`. Integration tests bind ephemeral ports (`127.0.0.1:0`),
so they are parallel-safe.

## Operability

- `--log-level trace|debug|info|warn|error` (all serve modes) drives
  `tracing` output; RPC traffic logs at `debug`, validator commit activity
  at `info`/`debug`, banners always print.
- Live activity: every sealed block logs `number/tx/gas_used/success`
  (both modes), every DAG commit logs its head height + pending depth, and
  a `heartbeat` line (height, seals, commits) prints every 10s while the
  mesh is quiet — validators are never silent anymore. `GET /metrics`
  exposes the same counters for scrapers.
- Ctrl+C (or SIGTERM) drains in-flight RPC calls, stops the DAG syncer,
  then exits. Sealed EVM state replays from the chain store; uncommitted
  DAG rounds resume from the WAL.
- Validators take `--config node.toml` (TOML, CLI flags override file
  values); `start-validators.ps1` writes one per validator automatically.
