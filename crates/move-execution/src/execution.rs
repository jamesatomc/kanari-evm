// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Transaction execution: validation, revm execution and instant sealing.
//!
//! Signed raw transactions are decoded, checked against the chain id,
//! executed in a [`KanariEvm`] for the target block, committed to revm state,
//! covered by a fresh SMT commitment, then sealed and persisted atomically.
//! Reverted transactions are sealed with `success: false` (standard EVM
//! semantics: gas is still charged and the nonce advances).

use crate::node::{
    BLOCK_BENEFICIARY, BLOCK_GAS_LIMIT, DEFAULT_BASE_FEE_WEI, DEFAULT_CALL_GAS, KanariNode,
    NodeError,
};
use crate::precompiles::{KanariEvmParams, build_kanari_evm};
use crate::views::{block_hash, next_parent_hash};
use alloy_consensus::{TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind as AlloyTxKind, U256, keccak256};
use kanari_evm_storage::{SealedBlock, StoredLog, StoredReceipt};
use revm::{
    context::TxEnv,
    context_interface::result::{ExecutionResult, Output},
    database::InMemoryDB,
    database_interface::DatabaseCommit,
    handler::{ExecuteEvm, MainnetContext},
    primitives::TxKind as RevmTxKind,
};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

impl KanariNode {
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

    /// Validate and queue a signed raw transaction.
    ///
    /// Single-node mode executes and instantly seals (reverted transactions
    /// seal with `success: false` — standard EVM semantics: gas is still
    /// charged and the nonce advances). Validator mode submits to the DAG
    /// mempool and returns the transaction hash immediately; the transaction
    /// seals once a DAG commit covers it.
    pub fn send_raw_transaction(&mut self, raw: Bytes) -> Result<B256, NodeError> {
        if let Some(sender) = &self.dag_sender {
            sender
                .try_send(vec![raw.to_vec()])
                .map_err(|e| NodeError::Execution(format!("dag mempool full: {e}")))?;
            return Ok(keccak256(&raw));
        }
        let number = self.block_number() + 1;
        let timestamp = now_secs();
        self.seal_raw(raw, number, timestamp)
    }

    /// Seal one DAG-committed payload with a deterministic timestamp.
    ///
    /// Every validator commits identical sub-DAGs in identical order, so
    /// feeding the commit's anchor timestamp here keeps block hashes and
    /// state roots convergent across validators. An invalid payload is
    /// rejected (all validators reject it identically) without stopping
    /// the commit loop — the caller logs and continues.
    ///
    /// Public because the commit-execution loop lives in
    /// `kanari-evm-consensus`; single-node code paths never call this.
    pub fn seal_committed(&mut self, raw: Bytes, timestamp: u64) -> Result<B256, NodeError> {
        let number = self.block_number() + 1;
        self.seal_raw(raw, number, timestamp)
    }

    pub(crate) fn evm_for_block(
        &mut self,
        number: u64,
        timestamp: u64,
    ) -> crate::precompiles::KanariEvm<MainnetContext<&mut InMemoryDB>> {
        build_kanari_evm(
            &mut self.db,
            KanariEvmParams {
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
    pub(crate) fn replay_blocks(&mut self, blocks: Vec<SealedBlock>) -> Result<(), NodeError> {
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

    /// Decode, execute, commit to revm state, update the SMT commitment and
    /// seal a block, persisting it atomically (block + receipts + tx index
    /// + height) to RocksDB.
    pub(crate) fn seal_raw(
        &mut self,
        raw: Bytes,
        number: u64,
        timestamp: u64,
    ) -> Result<B256, NodeError> {
        let prepared = PreparedTx::decode(&raw, self.spec.chain_id)?;
        let parent_hash = next_parent_hash(&self.blocks);
        let mut evm = self.evm_for_block(number, timestamp);
        let output = evm
            .transact(prepared.tx_env())
            .map_err(|e| NodeError::Execution(e.to_string()))?;
        self.db.commit(output.state);
        let state_root = self.rebuild_smt()?;
        let (success, gas_used, created, logs) = execution_summary(&output.result);
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
            logs,
        };
        self.store.save_block(&block, &[(&hash, &receipt)])?;
        self.blocks.push(block);
        self.receipts.insert(hash, receipt);
        self.metrics.blocks_sealed.fetch_add(1, Ordering::Relaxed);
        self.metrics.txs_sealed.fetch_add(1, Ordering::Relaxed);
        Ok(hash)
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
            TxEnvelope::Eip7702(signed) => {
                let tx = signed.tx();
                let mut inner = TxEnv {
                    caller: sender,
                    gas_limit: tx.gas_limit,
                    gas_price: tx.max_fee_per_gas,
                    kind: RevmTxKind::Call(tx.to),
                    value: tx.value,
                    data: tx.input.clone(),
                    nonce: tx.nonce,
                    chain_id: Some(tx.chain_id),
                    gas_priority_fee: Some(tx.max_priority_fee_per_gas),
                    access_list: tx.access_list.clone(),
                    tx_type: 4, // EIP-7702: revm keys auth handling off this field
                    ..Default::default()
                };
                inner.set_signed_authorization(tx.authorization_list.clone());
                (tx.chain_id, inner)
            }
            TxEnvelope::Eip4844(_) => {
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

fn execution_summary(out: &ExecutionResult) -> (bool, u64, Option<Address>, Vec<StoredLog>) {
    match out {
        ExecutionResult::Success {
            gas, output, logs, ..
        } => {
            let created = match output {
                Output::Create(_, addr) => *addr,
                Output::Call(_) => None,
            };
            let logs = logs
                .iter()
                .map(|log| StoredLog {
                    address: log.address,
                    topics: log.topics().to_vec(),
                    data: log.data.data.clone(),
                })
                .collect();
            (true, gas.tx_gas_used(), created, logs)
        }
        // Reverted logs are discarded by consensus: receipts stay log-free.
        ExecutionResult::Revert { gas, .. } => (false, gas.tx_gas_used(), None, Vec::new()),
        ExecutionResult::Halt { gas, .. } => (false, gas.tx_gas_used(), None, Vec::new()),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
