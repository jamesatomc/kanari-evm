// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Dev faucet: a node-held funded account that drips test ETH on demand.
//!
//! DEV ONLY — no authentication, per-request cap enforced. The faucet secret
//! is generated on fresh state and persisted in the chain store so restarts
//! keep working.

use super::node::{
    DEFAULT_BASE_FEE_WEI, KanariNode, MAX_FAUCET_ETH_PER_REQUEST, NodeError, WEI_IN_ETH,
};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind as AlloyTxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use std::path::Path;

impl KanariNode {
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
}

/// Generate a fresh faucet signer. Returns (address, secret).
/// DEV ONLY: the secret is stored raw in a sidecar file next to the state.
pub fn generate_faucet_key() -> (Address, B256) {
    let signer = PrivateKeySigner::random();
    (signer.address(), signer.to_bytes())
}

fn signer_address(signer: &PrivateKeySigner) -> Address {
    signer.address()
}

/// Parse a legacy faucet sidecar file (64-char hex, `0x` prefix tolerated).
pub(crate) fn parse_faucet_hex_file(path: &Path) -> Result<B256, NodeError> {
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
