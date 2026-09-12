// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shared integration-test helpers: signing, RPC driving, temp state dirs.
//!
//! Included via `mod common;` (i.e. `tests/common/mod.rs`) from each
//! integration test target — one copy instead of eight. Helpers unused by
//! a given target are fine (dead code allowed here by design).

#![allow(dead_code)]

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use kanari_evm_move_execution::{
    DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec,
    KanariNode,
};
use kanari_evm_rpc::rpc;
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

/// 1 ETH in wei.
pub const ONE_ETH_WEI: u128 = 1_000_000_000_000_000_000;
/// 1 gwei in wei (the genesis base fee).
pub const GWEI_WEI: u128 = 1_000_000_000;

/// Whole-ETH amount in wei (faucet-sized conveniences).
pub fn eth(n: u128) -> U256 {
    U256::from(n.saturating_mul(ONE_ETH_WEI))
}

/// Fresh temp dir for one test's chain state.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kanari-evm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Open a node whose genesis funds each `(address, balance)` pair.
pub fn open_funded(tag: &str, funds: &[(Address, U256)]) -> (KanariNode, PathBuf) {
    let dir = temp_dir(tag);
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        funds.to_vec(),
    );
    let node = KanariNode::open(spec, dir.join("state.json")).expect("open node");
    (node, dir)
}

/// Open a node funding one signer with the full dev balance.
pub fn open_for_signer(tag: &str, signer: &PrivateKeySigner) -> (KanariNode, PathBuf, Address) {
    let addr = signer.address();
    let (node, dir) = open_funded(tag, &[(addr, U256::from(DEV_FUNDED_BALANCE))]);
    (node, dir, addr)
}

/// Sign any EIP-1559 transaction and RLP-encode the envelope.
pub async fn sign_tx(signer: &PrivateKeySigner, tx: TxEip1559) -> Bytes {
    let sig = signer.sign_hash(&tx.signature_hash()).await.expect("sign");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    Bytes::from(raw)
}

/// Sign a standard EIP-1559 transaction (1 gwei priority) and encode it.
pub async fn sign_1559(
    signer: &PrivateKeySigner,
    to: TxKind,
    input: Bytes,
    nonce: u64,
    gas_limit: u64,
    max_fee: u128,
    value: U256,
) -> Bytes {
    sign_tx(
        signer,
        TxEip1559 {
            chain_id: KANARI_EVM_DEV_CHAIN_ID,
            nonce,
            gas_limit,
            to,
            value,
            input,
            access_list: Default::default(),
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: GWEI_WEI,
        },
    )
    .await
}

/// POST one JSON-RPC call, asserting a clean (error-free) response and
/// returning `result`.
pub async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp: Value = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("rpc reachable")
        .json()
        .await
        .expect("valid json");
    assert!(
        resp.get("error").is_none(),
        "rpc {method} errored: {}",
        resp["error"]
    );
    resp["result"].clone()
}

/// Parse a `0x` quantity response into u128.
pub fn quantity_u128(v: &Value) -> u128 {
    let s = v.as_str().expect("0x quantity");
    u128::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex quantity")
}

/// Serve a node on an ephemeral localhost port. Returns the base URL and a
/// handle to abort the server.
pub async fn serve(node: KanariNode) -> (String, tokio::task::JoinHandle<()>) {
    let app = rpc::router(Arc::new(Mutex::new(node)));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (url, server)
}
