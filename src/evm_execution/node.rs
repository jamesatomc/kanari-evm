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

use super::chainspec::KanariChainSpec;
use crate::core_consensus::store::{ChainStore, StoreError};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, TxKind as AlloyTxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use revm::{
    context::TxEnv,
    context_interface::result::{ExecutionResult, Output},
    database::InMemoryDB,
    database_interface::{Database, DatabaseCommit},
    handler::{ExecuteEvm, MainnetContext},
    primitives::TxKind as RevmTxKind,
};
use serde::{Deserialize, Serialize};
use smt::SparseMerkleTree;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Base fee charged by every sealed block (1 gwei, Anvil-style).
pub const DEFAULT_BASE_FEE_WEI: u128 = 1_000_000_000;
/// Genesis funding for the faucet account (whole ETH).
pub const FAUCET_GENESIS_ETH: u128 = 1_000_000;
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
    #[error("unsupported transaction type (only legacy, EIP-2930 and EIP-1559 are accepted)")]
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

/// A sealed block: header-ish metadata plus raw transactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedBlock {
    pub number: u64,
    pub timestamp: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub txs: Vec<Bytes>,
    /// SMT state root after this block. `None` for journals written before
    /// state commitments existed (filled in on replay upgrade).
    #[serde(default)]
    pub state_root: Option<B256>,
}

/// Minimal receipt stored per transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReceipt {
    pub tx_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub gas_used: u64,
    pub success: bool,
    pub from: Address,
    pub to: Option<Address>,
    pub contract_address: Option<Address>,
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

/// Single-node dev chain with instant sealing.
pub struct KanariNode {
    spec: KanariChainSpec,
    db: InMemoryDB,
    blocks: Vec<SealedBlock>,
    receipts: HashMap<B256, StoredReceipt>,
    store: ChainStore,
    smt: SparseMerkleTree,
    genesis_root: B256,
    faucet_key: Option<B256>,
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

    /// Latest sealed block number (0 before the first transaction).
    pub fn block_number(&self) -> u64 {
        self.blocks.last().map(|b| b.number).unwrap_or(0)
    }

    /// Balance of an account in the current state.
    pub fn balance_of(&mut self, address: Address) -> Result<U256, NodeError> {
        Ok(self
            .db
            .basic(address)
            .map_err(|e| NodeError::Storage(e.to_string()))?
            .map(|info| info.balance)
            .unwrap_or(U256::ZERO))
    }

    /// Transaction count (nonce) of an account in the current state.
    pub fn nonce_of(&mut self, address: Address) -> Result<u64, NodeError> {
        Ok(self
            .db
            .basic(address)
            .map_err(|e| NodeError::Storage(e.to_string()))?
            .map(|info| info.nonce)
            .unwrap_or(0))
    }

    /// Look up a sealed block by number.
    pub fn block_by_number(&self, number: u64) -> Option<&SealedBlock> {
        self.blocks.iter().find(|b| b.number == number)
    }

    /// Look up a sealed block by hash (linear scan; dev-scale chains only).
    pub fn block_by_hash(&self, hash: &B256) -> Option<&SealedBlock> {
        self.blocks.iter().find(|b| &b.hash == hash)
    }

    /// Block view by hash, including the synthetic empty genesis block.
    pub fn block_view_by_hash(&self, hash: &B256, full_txs: bool) -> Option<serde_json::Value> {
        if *hash == genesis_hash() && self.block_by_number(0).is_none() {
            return Some(empty_block_view(
                0,
                genesis_hash(),
                B256::ZERO,
                0,
                self.genesis_root,
                full_txs,
            ));
        }
        let number = self.block_by_hash(hash)?.number;
        self.block_view(number, full_txs)
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

    /// Deployed bytecode of an account, empty for EOAs and absent accounts.
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
        let max = U256::from(super::chainspec::KANARI_EVM_MAX_SUPPLY_ETH)
            .saturating_mul(U256::from(WEI_IN_ETH));
        (total, max)
    }

    pub fn code_of(&mut self, address: Address) -> Result<Bytes, NodeError> {
        let Some(info) = self
            .db
            .basic(address)
            .map_err(|e| NodeError::Storage(e.to_string()))?
        else {
            return Ok(Bytes::new());
        };
        if info.code_hash == revm::primitives::KECCAK_EMPTY {
            return Ok(Bytes::new());
        }
        if let Some(code) = info.code {
            return Ok(code.original_bytes());
        }
        Ok(self
            .db
            .cache
            .contracts
            .get(&info.code_hash)
            .map(|code| code.original_bytes())
            .unwrap_or_default())
    }

    /// Current SMT state root.
    pub fn state_root(&self) -> Result<B256, NodeError> {
        let root = self
            .smt
            .root_hash()
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        Ok(B256::from_slice(&root))
    }

    /// Prove an account (or one of its storage slots) against the current
    /// state root. Returns `None` only on storage errors, never on absence:
    /// absent keys yield non-membership proofs (`exists: false`).
    pub fn smt_proof(&self, address: Address, slot: Option<U256>) -> Result<SmtProof, NodeError> {
        let key = match slot {
            Some(s) => smt_storage_key(&address, &s),
            None => smt_account_key(&address),
        };
        let value = self
            .smt
            .get(&key)
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        let (exists, leaf, siblings) = self
            .smt
            .proof(&key)
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        Ok(SmtProof {
            root: self.state_root()?,
            key: B256::from_slice(&key),
            value,
            exists,
            leaf: B256::from_slice(&leaf),
            siblings: siblings.into_iter().map(|s| B256::from_slice(&s)).collect(),
        })
    }

    /// Install the faucet key, persisting it in the store immediately so
    /// restarts keep working.
    pub fn set_faucet_key(&mut self, secret: B256) -> Result<(), NodeError> {
        self.store.set_faucet_key(&secret)?;
        self.faucet_key = Some(secret);
        Ok(())
    }

    /// Load a previously persisted faucet key. Returns `false` when none was
    /// stored (faucet stays disabled).
    pub fn load_faucet_key(&mut self) -> Result<bool, NodeError> {
        match self.store.load_faucet_key()? {
            None => Ok(false),
            Some(secret) => {
                self.faucet_key = Some(secret);
                Ok(true)
            }
        }
    }

    /// Address of the configured faucet account, if any.
    pub fn faucet_address(&self) -> Result<Option<Address>, NodeError> {
        match self.faucet_key {
            None => Ok(None),
            Some(secret) => {
                let signer = PrivateKeySigner::from_bytes(&secret)
                    .map_err(|e| NodeError::Storage(format!("bad faucet key: {e}")))?;
                Ok(Some(signer_address(&signer)))
            }
        }
    }

    /// Send `amount_wei` from the faucet account to `to`, sealing immediately.
    /// DEV ONLY — no authentication. Capped per request.
    pub fn faucet(&mut self, to: Address, amount_wei: U256) -> Result<B256, NodeError> {
        if amount_wei > U256::from(MAX_FAUCET_ETH_PER_REQUEST * WEI_IN_ETH) {
            return Err(NodeError::InvalidTransaction(format!(
                "faucet amount exceeds per-request cap of {MAX_FAUCET_ETH_PER_REQUEST} ETH"
            )));
        }
        let secret = self.faucet_key.ok_or(NodeError::NoFaucetConfigured)?;
        let signer = PrivateKeySigner::from_bytes(&secret)
            .map_err(|e| NodeError::Storage(format!("bad faucet key: {e}")))?;
        let from = signer_address(&signer);
        let balance = self.balance_of(from)?;
        let gas_cost = U256::from(21_000u128 * DEFAULT_BASE_FEE_WEI);
        if balance < amount_wei + gas_cost {
            return Err(NodeError::Execution("faucet account is dry".to_string()));
        }
        let nonce = self.nonce_of(from)?;
        let tx = TxEip1559 {
            chain_id: self.spec.chain_id,
            nonce,
            gas_limit: 21_000,
            to: AlloyTxKind::Call(to),
            value: amount_wei,
            input: Bytes::new(),
            access_list: Default::default(),
            max_fee_per_gas: DEFAULT_BASE_FEE_WEI,
            max_priority_fee_per_gas: DEFAULT_BASE_FEE_WEI,
        };
        let sighash = tx.signature_hash();
        let sig = signer
            .sign_hash_sync(&sighash)
            .map_err(|e| NodeError::BadSignature(e.to_string()))?;
        let envelope = TxEnvelope::from(tx.into_signed(sig));
        let mut raw = Vec::new();
        envelope.encode_2718(&mut raw);
        self.send_raw_transaction(raw.into())
    }

    /// Standard `eth_getTransactionByHash` view (v/r/s omitted).
    pub fn tx_view(&self, hash: &B256) -> Option<serde_json::Value> {
        let receipt = self.receipts.get(hash)?;
        let (sender, raw) = self.tx_record(hash)?;
        let view = decode_tx_view(raw)?;
        Some(serde_json::json!({
            "hash": hash.to_string(),
            "nonce": quantity(view.nonce),
            "blockHash": receipt.block_hash.to_string(),
            "blockNumber": quantity(receipt.block_number),
            "transactionIndex": "0x0",
            "from": sender.to_string(),
            "to": view.to.map(|a| a.to_string()),
            "value": quantity_u256(view.value),
            "gasPrice": quantity_u256(view.effective_gas_price),
            "gas": quantity(view.gas_limit),
            "input": bytes_hex(&view.input),
            "type": quantity(view.tx_type as u64),
            "chainId": quantity(view.chain_id),
        }))
    }

    /// Standard `eth_getTransactionReceipt` view.
    pub fn receipt_view(&self, hash: &B256) -> Option<serde_json::Value> {
        let r = self.receipts.get(hash)?;
        Some(serde_json::json!({
            "transactionHash": r.tx_hash.to_string(),
            "transactionIndex": "0x0",
            "blockNumber": quantity(r.block_number),
            "blockHash": r.block_hash.to_string(),
            "from": r.from.to_string(),
            "to": r.to.map(|a| a.to_string()),
            "contractAddress": r.contract_address.map(|a| a.to_string()),
            "cumulativeGasUsed": quantity(r.gas_used),
            "gasUsed": quantity(r.gas_used),
            "effectiveGasPrice": quantity_u256(U256::from(DEFAULT_BASE_FEE_WEI)),
            "status": if r.success { "0x1" } else { "0x0" },
            "type": "0x2",
            "logs": [],
            "logsBloom": format!("0x{}", "00".repeat(256)),
        }))
    }

    /// Minimal `eth_getBlockByNumber` view. Block 0 is a synthetic empty    /// genesis view (no transactions were sealed yet).
    pub fn block_view(&self, number: u64, full_txs: bool) -> Option<serde_json::Value> {
        if number == 0 && self.block_by_number(0).is_none() {
            return Some(empty_block_view(
                0,
                genesis_hash(),
                B256::ZERO,
                0,
                self.genesis_root,
                full_txs,
            ));
        }
        let block = self.block_by_number(number)?;
        let txs: Vec<serde_json::Value> = if full_txs {
            block
                .txs
                .iter()
                .map(|raw| {
                    let hash = keccak256(raw);
                    self.tx_view(&hash).unwrap_or(serde_json::Value::Null)
                })
                .collect()
        } else {
            block
                .txs
                .iter()
                .map(|raw| serde_json::Value::String(keccak256(raw).to_string()))
                .collect()
        };
        let gas_used: u64 = block
            .txs
            .iter()
            .map(|raw| {
                let hash = keccak256(raw);
                self.receipts.get(&hash).map(|r| r.gas_used).unwrap_or(0)
            })
            .sum();
        Some(serde_json::json!({
            "number": quantity(block.number),
            "hash": block.hash.to_string(),
            "parentHash": block.parent_hash.to_string(),
            "nonce": "0x0000000000000000",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a741f0a22ef665e9a5c",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "stateRoot": block.state_root.unwrap_or(B256::ZERO).to_string(),
            "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "miner": BLOCK_BENEFICIARY.to_string(),
            "difficulty": "0x0",
            "totalDifficulty": "0x0",
            "gasLimit": quantity(BLOCK_GAS_LIMIT),
            "gasUsed": quantity(gas_used),
            "timestamp": quantity(block.timestamp),
            "extraData": "0x",
            "mixHash": B256::ZERO.to_string(),
            "baseFeePerGas": quantity_u256(U256::from(DEFAULT_BASE_FEE_WEI)),
            "transactions": txs,
            "uncles": [],
        }))
    }

    /// Read-only contract/message call against the current state.
    pub fn call(&mut self, call: CallRequest) -> Result<CallResult, NodeError> {
        let caller = call.from.unwrap_or(Address::ZERO);
        let nonce = match call.nonce {
            Some(n) => n,
            None => self.nonce_of(caller).unwrap_or(0),
        };
        let tx = TxEnv {
            caller,
            gas_limit: call.gas.unwrap_or(DEFAULT_CALL_GAS),
            gas_price: call.gas_price.unwrap_or(DEFAULT_BASE_FEE_WEI),
            kind: match call.to {
                Some(to) => RevmTxKind::Call(to),
                None => RevmTxKind::Create,
            },
            value: call.value.unwrap_or(U256::ZERO),
            data: call.data.unwrap_or_default(),
            nonce,
            chain_id: Some(self.spec.chain_id),
            gas_priority_fee: None,
            access_list: Default::default(),
            ..Default::default()
        };
        let mut evm = self.evm_for_block(self.block_number(), now_secs());
        let out = evm
            .transact_one(tx)
            .map_err(|e| NodeError::Execution(e.to_string()))?;
        Ok(CallResult::from_output(out))
    }

    /// Validate, execute and instantly seal a signed raw transaction.
    /// Reverted transactions are sealed with `success: false` (standard EVM
    /// semantics: gas is still charged and the nonce advances).
    pub fn send_raw_transaction(&mut self, raw: Bytes) -> Result<B256, NodeError> {
        let number = self.block_number() + 1;
        let timestamp = now_secs();
        self.seal_raw(raw, number, timestamp)
    }

    fn evm_for_block(
        &mut self,
        number: u64,
        timestamp: u64,
    ) -> super::precompiles::KanariEvm<MainnetContext<&mut InMemoryDB>> {
        super::precompiles::build_kanari_evm(
            &mut self.db,
            super::precompiles::KanariEvmParams {
                chain_id: self.spec.chain_id,
                spec: self.spec.spec_id,
                number,
                timestamp,
                basefee: DEFAULT_BASE_FEE_WEI as u64,
                gas_limit: BLOCK_GAS_LIMIT,
                beneficiary: BLOCK_BENEFICIARY,
            },
        )
    }

    /// Replay sealed blocks (from RocksDB or a legacy journal) into memory,
    /// verifying hashes and state roots. Any failure is a hard error.
    /// Callers reset to genesis first; this only executes and verifies.
    fn replay_blocks(&mut self, blocks: Vec<SealedBlock>) -> Result<(), NodeError> {
        for block in blocks {
            for raw in &block.txs {
                let hash = self.seal_raw(raw.clone(), block.number, block.timestamp)?;
                let sealed = self.blocks.last().expect("just sealed");
                if sealed.hash != block.hash {
                    return Err(NodeError::Storage(format!(
                        "replay diverged at block {}",
                        block.number
                    )));
                }
                if let Some(expected) = block.state_root
                    && sealed.state_root != Some(expected)
                {
                    return Err(NodeError::Storage(format!(
                        "state root mismatch at block {}",
                        block.number
                    )));
                }
                let _ = hash;
            }
        }
        Ok(())
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

    /// Decode, execute, commit to revm state, update the SMT commitment and
    /// seal a block, persisting it atomically (block + receipts + tx index
    /// + height) to RocksDB.
    fn seal_raw(&mut self, raw: Bytes, number: u64, timestamp: u64) -> Result<B256, NodeError> {
        let prepared = PreparedTx::decode(&raw, self.spec.chain_id)?;
        let parent_hash = next_parent_hash(&self.blocks);
        let mut evm = self.evm_for_block(number, timestamp);
        let output = evm
            .transact(prepared.tx_env())
            .map_err(|e| NodeError::Execution(e.to_string()))?;
        self.db.commit(output.state);
        let state_root = self.rebuild_smt()?;
        let (success, gas_used, created) = execution_summary(&output.result);
        let hash = keccak256(&raw);
        let block_hash = block_hash(parent_hash, number, timestamp);
        let block = SealedBlock {
            number,
            timestamp,
            hash: block_hash,
            parent_hash,
            txs: vec![raw],
            state_root: Some(state_root),
        };
        let receipt = StoredReceipt {
            tx_hash: hash,
            block_number: number,
            block_hash,
            gas_used,
            success,
            from: prepared.sender,
            to: prepared.to,
            contract_address: created,
        };
        self.store.save_block(&block, &[(&hash, &receipt)])?;
        self.blocks.push(block);
        self.receipts.insert(hash, receipt);
        Ok(hash)
    }

    /// Rebuild the SMT state commitment from the full live revm state.
    /// Dev-scale full rebuilds stay correct under every state transition
    /// (including selfdestructs) without dirty-set tracking.
    fn rebuild_smt(&self) -> Result<B256, NodeError> {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (address, account) in self.db.cache.accounts.iter() {
            let info = &account.info;
            if info.balance.is_zero()
                && info.nonce == 0
                && info.code_hash == revm::primitives::KECCAK_EMPTY
            {
                continue;
            }
            entries.push((smt_account_key(address).to_vec(), encode_account(info)));
            for (slot, value) in account.storage.iter() {
                entries.push((
                    smt_storage_key(address, slot).to_vec(),
                    value.to_be_bytes::<32>().to_vec(),
                ));
            }
        }
        self.smt
            .rebuild(&entries)
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        let root = self
            .smt
            .root_hash()
            .map_err(|e| NodeError::Storage(e.to_string()))?;
        Ok(B256::from_slice(&root))
    }
}

/// Parameters for `eth_call` / `eth_estimateGas`.
#[derive(Debug, Default, Clone)]
pub struct CallRequest {
    pub from: Option<Address>,
    pub to: Option<Address>,
    pub data: Option<Bytes>,
    pub value: Option<U256>,
    pub gas: Option<u64>,
    pub gas_price: Option<u128>,
    pub nonce: Option<u64>,
}

/// Result of a read-only call.
#[derive(Debug, Clone)]
pub struct CallResult {
    pub success: bool,
    pub gas_used: u64,
    pub output: Bytes,
}

impl CallResult {
    fn from_output(out: ExecutionResult) -> Self {
        let (success, gas_used, output) = match out {
            ExecutionResult::Success { gas, output, .. } => {
                (true, gas.tx_gas_used(), output.into_data())
            }
            ExecutionResult::Revert { gas, output, .. } => (false, gas.tx_gas_used(), output),
            ExecutionResult::Halt { gas, .. } => (false, gas.tx_gas_used(), Bytes::new()),
        };
        Self {
            success,
            gas_used,
            output,
        }
    }
}

/// A validated transaction ready for revm.
struct PreparedTx {
    sender: Address,
    to: Option<Address>,
    inner: TxEnv,
}

impl PreparedTx {
    fn decode(raw: &[u8], chain_id: u64) -> Result<Self, NodeError> {
        let envelope = TxEnvelope::decode_2718_exact(raw)
            .map_err(|e| NodeError::InvalidTransaction(e.to_string()))?;
        let sender = envelope
            .recover_signer()
            .map_err(|e| NodeError::BadSignature(e.to_string()))?;
        let (tx_chain_id, inner) = match &envelope {
            TxEnvelope::Legacy(signed) => {
                let tx = signed.tx();
                let tx_chain_id = tx.chain_id.ok_or(NodeError::InvalidTransaction(
                    "unprotected legacy transactions are rejected".to_string(),
                ))?;
                let inner = TxEnv {
                    caller: sender,
                    gas_limit: tx.gas_limit,
                    gas_price: tx.gas_price,
                    kind: alloy_kind(&tx.to),
                    value: tx.value,
                    data: tx.input.clone(),
                    nonce: tx.nonce,
                    chain_id: Some(tx_chain_id),
                    gas_priority_fee: None,
                    access_list: Default::default(),
                    ..Default::default()
                };
                (tx_chain_id, inner)
            }
            TxEnvelope::Eip2930(signed) => {
                let tx = signed.tx();
                let inner = TxEnv {
                    caller: sender,
                    gas_limit: tx.gas_limit,
                    gas_price: tx.gas_price,
                    kind: alloy_kind(&tx.to),
                    value: tx.value,
                    data: tx.input.clone(),
                    nonce: tx.nonce,
                    chain_id: Some(tx.chain_id),
                    gas_priority_fee: None,
                    access_list: tx.access_list.clone(),
                    ..Default::default()
                };
                (tx.chain_id, inner)
            }
            TxEnvelope::Eip1559(signed) => {
                let tx = signed.tx();
                let inner = TxEnv {
                    caller: sender,
                    gas_limit: tx.gas_limit,
                    gas_price: tx.max_fee_per_gas,
                    kind: alloy_kind(&tx.to),
                    value: tx.value,
                    data: tx.input.clone(),
                    nonce: tx.nonce,
                    chain_id: Some(tx.chain_id),
                    gas_priority_fee: Some(tx.max_priority_fee_per_gas),
                    access_list: tx.access_list.clone(),
                    ..Default::default()
                };
                (tx.chain_id, inner)
            }
            TxEnvelope::Eip4844(_) | TxEnvelope::Eip7702(_) => {
                return Err(NodeError::UnsupportedTxType);
            }
        };
        if tx_chain_id != chain_id {
            return Err(NodeError::WrongChainId(tx_chain_id, chain_id));
        }
        let to = match inner.kind {
            RevmTxKind::Call(addr) => Some(addr),
            RevmTxKind::Create => None,
        };
        Ok(Self { sender, to, inner })
    }

    fn tx_env(&self) -> TxEnv {
        self.inner.clone()
    }
}

fn alloy_kind(to: &AlloyTxKind) -> RevmTxKind {
    match to {
        AlloyTxKind::Call(addr) => RevmTxKind::Call(*addr),
        AlloyTxKind::Create => RevmTxKind::Create,
    }
}

fn execution_summary(out: &ExecutionResult) -> (bool, u64, Option<Address>) {
    match out {
        ExecutionResult::Success { gas, output, .. } => {
            let created = match output {
                Output::Create(_, addr) => *addr,
                Output::Call(_) => None,
            };
            (true, gas.tx_gas_used(), created)
        }
        ExecutionResult::Revert { gas, .. } => (false, gas.tx_gas_used(), None),
        ExecutionResult::Halt { gas, .. } => (false, gas.tx_gas_used(), None),
    }
}

fn block_hash(parent: B256, number: u64, timestamp: u64) -> B256 {
    let mut preimage = Vec::with_capacity(96);
    preimage.extend_from_slice(parent.as_slice());
    preimage.extend_from_slice(&number.to_be_bytes());
    preimage.extend_from_slice(&timestamp.to_be_bytes());
    keccak256(preimage)
}

/// Hash reported for the synthetic empty genesis block (block 0). Also used
/// as the parent hash of block 1 so the chain links correctly.
fn genesis_hash() -> B256 {
    keccak256(b"kanari-evm-genesis")
}

/// Parent hash for the next block to seal.
fn next_parent_hash(blocks: &[SealedBlock]) -> B256 {
    blocks.last().map(|b| b.hash).unwrap_or_else(genesis_hash)
}

/// Parse a legacy faucet sidecar file (64-char hex, `0x` prefix tolerated).
fn parse_faucet_hex_file(path: &Path) -> Result<B256, NodeError> {
    let raw = std::fs::read_to_string(path).map_err(|e| NodeError::Storage(e.to_string()))?;
    let hex = raw.trim().trim_start_matches("0x");
    if hex.len() != 64 {
        return Err(NodeError::Storage("malformed faucet key file".to_string()));
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk)
            .map_err(|_| NodeError::Storage("malformed faucet key file".to_string()))?;
        bytes[i] = u8::from_str_radix(s, 16)
            .map_err(|_| NodeError::Storage("malformed faucet key file".to_string()))?;
    }
    Ok(B256::from_slice(&bytes))
}

/// Domain tag for the Kanari EVM state commitment key derivation.
const SMT_DOMAIN: &[u8] = b"kanari-evm-smt-v1";

/// Generate a fresh faucet signer. Returns (address, secret).
/// DEV ONLY: the secret is stored raw in a sidecar file next to the state.
pub fn generate_faucet_key() -> (Address, B256) {
    let signer = PrivateKeySigner::random();
    (signer.address(), signer.to_bytes())
}

fn signer_address(signer: &PrivateKeySigner) -> Address {
    signer.address()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SMT key for an account: `BLAKE3(domain || 0x00 || address)`.
fn smt_account_key(address: &Address) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(SMT_DOMAIN.len() + 1 + 20);
    preimage.extend_from_slice(SMT_DOMAIN);
    preimage.push(0x00);
    preimage.extend_from_slice(address.as_slice());
    kanari_crypto::hash_data_blake3_array(&preimage)
}

/// SMT key for a storage slot: `BLAKE3(domain || 0x01 || address || slot)`.
fn smt_storage_key(address: &Address, slot: &U256) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(SMT_DOMAIN.len() + 1 + 20 + 32);
    preimage.extend_from_slice(SMT_DOMAIN);
    preimage.push(0x01);
    preimage.extend_from_slice(address.as_slice());
    preimage.extend_from_slice(&slot.to_be_bytes::<32>());
    kanari_crypto::hash_data_blake3_array(&preimage)
}

/// SMT value for an account: `balance_be32 || nonce_be8 || code_hash`.
fn encode_account(info: &revm::state::AccountInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(72);
    out.extend_from_slice(&info.balance.to_be_bytes::<32>());
    out.extend_from_slice(&info.nonce.to_be_bytes());
    out.extend_from_slice(info.code_hash.as_slice());
    out
}

/// An SMT inclusion/non-inclusion proof over the current state.
#[derive(Debug, Clone)]
pub struct SmtProof {
    /// Current state root the proof is valid against.
    pub root: B256,
    /// SMT key that was proven.
    pub key: B256,
    /// Committed value, or `None` for non-membership proofs.
    pub value: Option<Vec<u8>>,
    /// True for membership proofs.
    pub exists: bool,
    /// Leaf hash covered by the proof (feed to `smt::verify_proof`).
    pub leaf: B256,
    /// Sibling hashes, bottom-up.
    pub siblings: Vec<B256>,
}

fn quantity(v: u64) -> String {
    format!("0x{v:x}")
}

fn quantity_u256(v: U256) -> String {
    format!("0x{v:x}")
}

fn bytes_hex(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + b.len() * 2);
    out.push_str("0x");
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    for byte in b.iter() {
        out.push(CHARS[(byte >> 4) as usize] as char);
        out.push(CHARS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decoded transaction fields for JSON views (used by tx/receipt views).
struct TxView {
    nonce: u64,
    gas_limit: u64,
    effective_gas_price: U256,
    to: Option<Address>,
    value: U256,
    input: Bytes,
    tx_type: u8,
    chain_id: u64,
}

fn decode_tx_view(raw: &[u8]) -> Option<TxView> {
    let envelope = TxEnvelope::decode_2718_exact(raw).ok()?;
    match &envelope {
        TxEnvelope::Legacy(signed) => {
            let tx = signed.tx();
            Some(TxView {
                nonce: tx.nonce,
                gas_limit: tx.gas_limit,
                effective_gas_price: U256::from(tx.gas_price),
                to: match tx.to {
                    AlloyTxKind::Call(addr) => Some(addr),
                    AlloyTxKind::Create => None,
                },
                value: tx.value,
                input: tx.input.clone(),
                tx_type: 0,
                chain_id: tx.chain_id?,
            })
        }
        TxEnvelope::Eip2930(signed) => {
            let tx = signed.tx();
            Some(TxView {
                nonce: tx.nonce,
                gas_limit: tx.gas_limit,
                effective_gas_price: U256::from(tx.gas_price),
                to: match tx.to {
                    AlloyTxKind::Call(addr) => Some(addr),
                    AlloyTxKind::Create => None,
                },
                value: tx.value,
                input: tx.input.clone(),
                tx_type: 1,
                chain_id: tx.chain_id,
            })
        }
        TxEnvelope::Eip1559(signed) => {
            let tx = signed.tx();
            let effective = tx
                .max_fee_per_gas
                .min(DEFAULT_BASE_FEE_WEI + tx.max_priority_fee_per_gas);
            Some(TxView {
                nonce: tx.nonce,
                gas_limit: tx.gas_limit,
                effective_gas_price: U256::from(effective),
                to: match tx.to {
                    AlloyTxKind::Call(addr) => Some(addr),
                    AlloyTxKind::Create => None,
                },
                value: tx.value,
                input: tx.input.clone(),
                tx_type: 2,
                chain_id: tx.chain_id,
            })
        }
        TxEnvelope::Eip4844(_) | TxEnvelope::Eip7702(_) => None,
    }
}

/// Shared renderer for the synthetic empty genesis block.
fn empty_block_view(
    number: u64,
    hash: B256,
    parent_hash: B256,
    timestamp: u64,
    state_root: B256,
    _full_txs: bool,
) -> serde_json::Value {
    serde_json::json!({
        "number": quantity(number),
        "hash": hash.to_string(),
        "parentHash": parent_hash.to_string(),
        "nonce": "0x0000000000000000",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a741f0a22ef665e9a5c",
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
        "stateRoot": state_root.to_string(),
        "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
        "miner": BLOCK_BENEFICIARY.to_string(),
        "difficulty": "0x0",
        "totalDifficulty": "0x0",
        "gasLimit": quantity(BLOCK_GAS_LIMIT),
        "gasUsed": "0x0",
        "timestamp": quantity(timestamp),
        "extraData": "0x",
        "mixHash": B256::ZERO.to_string(),
        "baseFeePerGas": quantity_u256(U256::from(DEFAULT_BASE_FEE_WEI)),
        "transactions": [],
        "uncles": [],
    })
}
