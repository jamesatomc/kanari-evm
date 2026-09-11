// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! JSON views over sealed chain data: blocks, transactions and receipts in
//! the shapes `eth_getBlockByNumber`, `eth_getTransactionByHash` and
//! `eth_getTransactionReceipt` return.
//!
//! Block hashes are deterministic `keccak256(parent || number || timestamp)`
//! placeholders (see `node.rs`), which wallets accept but which must NOT be
//! mistaken for full L1 validity proofs.

use crate::node::{BLOCK_BENEFICIARY, BLOCK_GAS_LIMIT, DEFAULT_BASE_FEE_WEI, KanariNode};
use kanari_evm_storage::SealedBlock;
use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind as AlloyTxKind, U256, keccak256};

impl KanariNode {
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

    /// Minimal `eth_getBlockByNumber` view. Block 0 is a synthetic empty
    /// genesis view (no transactions were sealed yet).
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

pub(crate) fn block_hash(parent: B256, number: u64, timestamp: u64) -> B256 {
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
pub(crate) fn next_parent_hash(blocks: &[SealedBlock]) -> B256 {
    blocks.last().map(|b| b.hash).unwrap_or_else(genesis_hash)
}
