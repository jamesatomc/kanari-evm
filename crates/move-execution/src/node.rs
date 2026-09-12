// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Single-node Kanari EVM dev chain: instant-seal execution over revm with a
//! replay journal for persistence.
//!
//! The journal (`state file`) stores the chain id plus every sealed block's
//! raw transactions. On startup the node replays the journal, so a restart
//! reproduces identical state or fails loudly on corruption. No real state
//! trie is maintained: block hashes are deterministic
//! `keccak256(parent || number || timestamp)` placeholders, which wallets
//! accept but which must NOT be mistaken for full L1 validity proofs.
//!
//! The engine is split across focused modules:
//!
//! - [`crate::execution`] — transaction execution and instant sealing
//! - [`crate::views`] — JSON block / transaction / receipt views
//! - [`crate::faucet`] — dev faucet
//! - [`crate::state`] — state reads and the SMT state commitment

use crate::chainspec::KanariChainSpec;
use crate::faucet::parse_faucet_hex_file;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use kanari_evm_storage::{ChainStore, SealedBlock, StoreError, StoredReceipt};
use revm::database::InMemoryDB;
use serde::{Deserialize, Serialize};
use smt::SparseMerkleTree;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc};

/// Base fee charged by every sealed block (1 gwei, Anvil-style).
pub const DEFAULT_BASE_FEE_WEI: u128 = 1_000_000_000;
/// Max drip per faucet call (whole ETH).
pub const MAX_FAUCET_ETH_PER_REQUEST: u128 = 10_000;
/// Wei per whole ETH.
pub const WEI_IN_ETH: u128 = 1_000_000_000_000_000_000;
/// Block gas limit for sealed blocks.
pub const BLOCK_GAS_LIMIT: u64 = 30_000_000;
/// Default gas for `eth_call`/`eth_estimateGas` when the caller omits it.
/// Must stay below revm's per-transaction cap of 2^24 (16_777_216).
pub const DEFAULT_CALL_GAS: u64 = 15_000_000;
/// Block beneficiary: priority fees accrue to the treasury account on this
/// dev chain (base fee is still burned per EIP-1559).
pub const BLOCK_BENEFICIARY: Address =
    alloy_primitives::address!("0x7985132aa87aD878a1fCb69Fa62b301f4CDe4ccA");

/// Errors from node operations (surfaced as JSON-RPC `-32000` errors).
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("invalid transaction bytes: {0}")]
    InvalidTransaction(String),
    #[error(
        "unsupported transaction type (only legacy, EIP-2930, EIP-1559 and EIP-7702 are accepted)"
    )]
    UnsupportedTxType,
    #[error("signature recovery failed: {0}")]
    BadSignature(String),
    #[error("wrong chain id: tx for {0}, this chain is {1}")]
    WrongChainId(u64, u64),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("no faucet key configured (fresh state creates one automatically)")]
    NoFaucetConfigured,
}

impl From<StoreError> for NodeError {
    fn from(error: StoreError) -> Self {
        NodeError::Storage(error.to_string())
    }
}

/// Legacy JSON journal format (pre-RocksDB). Kept only to migrate old
/// state directories forward exactly once.
#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    chain_id: u64,
    #[serde(default)]
    genesis_alloc: Vec<(Address, U256)>,
    blocks: Vec<SealedBlock>,
}

/// Directory holding the chain RocksDB, derived from the `--state-file`
/// base path: `<base>.chain.db` (extension stripped, e.g.
/// `kanari-evm-state.json` -> `kanari-evm-state.chain.db`).
fn chain_db_dir(state_base: &Path) -> PathBuf {
    let name = state_base
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("kanari-evm-state");
    let stem = name.split('.').next().unwrap_or(name);
    state_base
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.chain.db"))
}

/// Shared node handle: what the RPC router and the DAG validator both hold.
pub type SharedNode = Arc<Mutex<KanariNode>>;

/// Lock-free node counters, served at `GET /metrics` in Prometheus text
/// format. Uniform across modes: instant seals and commit seals both flow
/// through `seal_raw`; validators additionally bump `commits_seen`.
#[derive(Debug, Default)]
pub struct NodeMetrics {
    /// Sealed blocks (one transaction each on this chain).
    pub blocks_sealed: AtomicU64,
    /// Sealed transactions (equals blocks + replayed ones).
    pub txs_sealed: AtomicU64,
    /// DAG commits executed (validator mode only, 0 on single nodes).
    pub commits_seen: AtomicU64,
}

impl NodeMetrics {
    /// Snapshot as `(blocks, txs, commits)`.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.blocks_sealed.load(Ordering::Relaxed),
            self.txs_sealed.load(Ordering::Relaxed),
            self.commits_seen.load(Ordering::Relaxed),
        )
    }
}

/// Single-node dev chain with instant sealing.
///
/// In validator mode (`kanari-evm-consensus`) `dag_sender` is
/// set and [`KanariNode::send_raw_transaction`] (see `execution.rs`)
/// submits to the DAG mempool instead of sealing instantly; sealing then
/// happens deterministically on each DAG commit via `seal_committed`.
pub struct KanariNode {
    pub(crate) spec: KanariChainSpec,
    pub(crate) db: InMemoryDB,
    pub(crate) blocks: Vec<SealedBlock>,
    pub(crate) receipts: HashMap<B256, StoredReceipt>,
    pub(crate) store: ChainStore,
    pub(crate) smt: SparseMerkleTree,
    pub(crate) genesis_root: B256,
    pub(crate) faucet_key: Option<B256>,
    pub(crate) dag_sender: Option<mpsc::Sender<Vec<Vec<u8>>>>,
    pub(crate) metrics: Arc<NodeMetrics>,
}

impl KanariNode {
    /// Open (or create) a node. Durable state lives in RocksDB
    /// (`<state-base>.chain.db`, shared with the SMT layer); the in-memory
    /// revm state is rebuilt by deterministic replay on every open, so any
    /// replay failure is a hard error (corrupt store).
    ///
    /// A legacy JSON journal at `state_base` (pre-RocksDB format) is migrated
    /// forward once: replayed, verified, persisted into RocksDB, then renamed
    /// to `<state-base>.migrated.json`.
    pub fn open(spec: KanariChainSpec, state_base: impl Into<PathBuf>) -> Result<Self, NodeError> {
        let state_base = state_base.into();
        let store = ChainStore::open(chain_db_dir(&state_base))?;
        let smt = SparseMerkleTree::new(store.shared_db());
        let mut node = Self {
            db: InMemoryDB::default(),
            blocks: Vec::new(),
            receipts: HashMap::new(),
            spec,
            store,
            smt,
            genesis_root: B256::ZERO,
            faucet_key: None,
            dag_sender: None,
            metrics: Arc::new(NodeMetrics::default()),
        };
        if node.store.stored_chain_id()?.is_none() {
            if state_base.is_file() {
                let pending = node.migrate_legacy_journal(&state_base)?;
                node.spec.apply_genesis(&mut node.db);
                node.genesis_root = node.rebuild_smt()?;
                node.replay_blocks(pending)?;
            } else {
                let chain_id = node.spec.chain_id;
                let alloc = node.spec.genesis_alloc.clone();
                node.store.init_meta(chain_id, &alloc)?;
                node.spec.apply_genesis(&mut node.db);
                node.genesis_root = node.rebuild_smt()?;
            }
        } else {
            let stored_chain = node
                .store
                .stored_chain_id()?
                .ok_or_else(|| NodeError::Storage("chain database missing chain id".to_string()))?;
            if stored_chain != node.spec.chain_id {
                return Err(NodeError::Storage(format!(
                    "stored chain id {stored_chain} does not match node chain id {}",
                    node.spec.chain_id
                )));
            }
            let alloc = node.store.stored_genesis_alloc()?;
            if !alloc.is_empty() {
                node.spec.genesis_alloc = alloc;
            }
            node.spec.apply_genesis(&mut node.db);
            node.genesis_root = node.rebuild_smt()?;
            let blocks = node.store.load_blocks()?;
            node.replay_blocks(blocks)?;
        }
        // Faucet key lives in the store now; pick it up when present.
        if let Some(secret) = node.store.load_faucet_key()? {
            node.faucet_key = Some(secret);
        }
        Ok(node)
    }

    /// Chain id of this node.
    pub fn chain_id(&self) -> u64 {
        self.spec.chain_id
    }

    /// Attach the DAG mempool sender (validator mode). While set,
    /// `send_raw_transaction` submits to the DAG instead of sealing
    /// instantly; use `seal_committed` (execution.rs) for commit-driven
    /// sealing.
    pub fn set_dag_sender(&mut self, sender: mpsc::Sender<Vec<Vec<u8>>>) {
        self.dag_sender = Some(sender);
    }

    /// True while transactions flow through the DAG mempool.
    pub fn is_validator_mode(&self) -> bool {
        self.dag_sender.is_some()
    }

    /// Lock-free counters for `GET /metrics`.
    pub fn metrics(&self) -> &NodeMetrics {
        &self.metrics
    }
    /// Latest sealed block number (0 before the first transaction).
    pub fn block_number(&self) -> u64 {
        self.blocks.last().map(|b| b.number).unwrap_or(0)
    }

    /// Look up a sealed block by number.
    pub fn block_by_number(&self, number: u64) -> Option<&SealedBlock> {
        self.blocks.iter().find(|b| b.number == number)
    }

    /// Look up a sealed block by hash (linear scan; dev-scale chains only).
    pub fn block_by_hash(&self, hash: &B256) -> Option<&SealedBlock> {
        self.blocks.iter().find(|b| &b.hash == hash)
    }

    /// Look up a transaction's sender and raw bytes.
    pub fn tx_record(&self, hash: &B256) -> Option<(Address, &Bytes)> {
        let receipt = self.receipts.get(hash)?;
        let block = self.block_by_number(receipt.block_number)?;
        let raw = block.txs.iter().find(|raw| keccak256(raw) == *hash)?;
        Some((receipt.from, raw))
    }

    /// Look up a receipt by transaction hash.
    pub fn receipt(&self, hash: &B256) -> Option<&StoredReceipt> {
        self.receipts.get(hash)
    }

    /// Circulating supply and protocol max supply, in wei.
    ///
    /// No minting exists after genesis (fees only move value to the block
    /// beneficiary), so circulating supply is exactly the sum of the
    /// genesis allocations baked into this node's chain spec.
    pub fn supply(&self) -> (U256, U256) {
        let total = self
            .spec
            .genesis_alloc
            .iter()
            .fold(U256::ZERO, |acc, (_, balance)| acc.saturating_add(*balance));
        let max = U256::from(crate::chainspec::KANARI_EVM_MAX_SUPPLY_ETH)
            .saturating_mul(U256::from(WEI_IN_ETH));
        (total, max)
    }

    /// One-time migration of a legacy JSON journal into RocksDB: verify and
    /// persist metadata, migrate the faucet sidecar, rename the legacy file
    /// aside and drop superseded sidecars. Returns the legacy blocks for the
    /// caller to replay uniformly with store-sourced blocks.
    fn migrate_legacy_journal(&mut self, path: &Path) -> Result<Vec<SealedBlock>, NodeError> {
        let raw = std::fs::read(path).map_err(|e| NodeError::Storage(e.to_string()))?;
        let journal: Journal =
            serde_json::from_slice(&raw).map_err(|e| NodeError::Storage(e.to_string()))?;
        if journal.chain_id != self.spec.chain_id {
            return Err(NodeError::Storage(format!(
                "legacy journal chain id {} does not match node chain id {}",
                journal.chain_id, self.spec.chain_id
            )));
        }
        if !journal.genesis_alloc.is_empty() {
            self.spec.genesis_alloc = journal.genesis_alloc.clone();
        }
        let chain_id = self.spec.chain_id;
        let alloc = self.spec.genesis_alloc.clone();
        self.store.init_meta(chain_id, &alloc)?;
        // Migrate a legacy faucet sidecar into the store when present.
        let legacy_sidecar = PathBuf::from(format!("{}.faucet.key", path.display()));
        if legacy_sidecar.exists()
            && let Ok(secret) = parse_faucet_hex_file(&legacy_sidecar)
        {
            self.store.set_faucet_key(&secret)?;
            self.faucet_key = Some(secret);
        }
        let blocks = journal.blocks;
        let migrated = path.with_extension("migrated.json");
        std::fs::rename(path, &migrated).map_err(|e| NodeError::Storage(e.to_string()))?;
        let _ = std::fs::remove_file(&legacy_sidecar);
        let _ = std::fs::remove_dir_all(format!("{}.smt", path.display()));
        Ok(blocks)
    }
}
