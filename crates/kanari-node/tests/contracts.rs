// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Smart-contract lifecycle: deploy `SimpleStorage`, mutate state with a
//! signed `set()` transaction, and read it back with `get()`.
//!
//! The `live_deploy_simple_storage` test additionally runs the same flow
//! against a real node over JSON-RPC when `KANARI_EVM_LIVE_RPC` is set
//! (e.g. a local dev node with the faucet enabled).

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use kanari_evm_move_execution::{
    CallRequest, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KanariChainSpec, KanariNode, contracts,
};

const GWEI_WEI: u128 = 1_000_000_000;

async fn sign_1559(signer: &PrivateKeySigner, tx: TxEip1559) -> Bytes {
    let hash = tx.signature_hash();
    let sig = signer.sign_hash(&hash).await.expect("sign");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    raw.into()
}

fn create_tx(nonce: u64, init: Vec<u8>) -> TxEip1559 {
    TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce,
        gas_limit: 1_000_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init.into(),
        access_list: Default::default(),
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    }
}

fn call_tx(nonce: u64, to: Address, data: Vec<u8>) -> TxEip1559 {
    TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce,
        gas_limit: 500_000,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: data.into(),
        access_list: Default::default(),
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    }
}

fn word_to_u64(word: &[u8]) -> u64 {
    assert_eq!(word.len(), 32);
    let mut b = [0u8; 8];
    b.copy_from_slice(&word[24..]);
    u64::from_be_bytes(b)
}

#[tokio::test]
async fn simple_storage_lifecycle() {
    let deployer = PrivateKeySigner::random();
    let deployer_addr = deployer.address();
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(deployer_addr, U256::from(DEV_FUNDED_BALANCE))],
    );
    let dir = std::env::temp_dir().join(format!("kanari-evm-contracts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");

    // Deploy.
    let init = contracts::deploy_init(contracts::SIMPLE_STORAGE_RUNTIME);
    let deploy_hash = node
        .send_raw_transaction(sign_1559(&deployer, create_tx(0, init)).await)
        .expect("deploy seals");
    let receipt = node.receipt(&deploy_hash).expect("receipt");
    assert!(receipt.success, "deploy must succeed");
    let contract = receipt.contract_address.expect("create yields address");
    assert_eq!(
        node.code_of(contract).expect("code").as_ref(),
        contracts::SIMPLE_STORAGE_RUNTIME
    );

    // Read before write: get() == 0.
    let read = |node: &mut KanariNode| {
        node.call(CallRequest {
            from: Some(deployer_addr),
            to: Some(contract),
            data: Some(contracts::encode_get().into()),
            gas: Some(100_000),
            ..Default::default()
        })
        .expect("call")
    };
    let r0 = read(&mut node);
    assert!(r0.success);
    assert_eq!(word_to_u64(&r0.output), 0);

    // State-changing set(12345) as a signed transaction.
    let set_hash = node
        .send_raw_transaction(
            sign_1559(
                &deployer,
                call_tx(1, contract, contracts::encode_set(12345)),
            )
            .await,
        )
        .expect("set seals");
    let set_receipt = node.receipt(&set_hash).expect("set receipt");
    assert!(set_receipt.success, "set must succeed");

    // Read after write: get() == 12345.
    let r1 = read(&mut node);
    assert!(r1.success);
    assert_eq!(word_to_u64(&r1.output), 12345);

    std::fs::remove_dir_all(&dir).ok();
}

/// Live end-to-end deploy against a running node:
/// `KANARI_EVM_LIVE_RPC=http://127.0.0.1:8546 cargo test -p kanari-evm
/// --test contracts live_ -- --nocapture`
#[tokio::test]
async fn live_deploy_simple_storage() {
    let Some(rpc) = std::env::var("KANARI_EVM_LIVE_RPC")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("skipping live deploy (KANARI_EVM_LIVE_RPC unset)");
        return;
    };
    let http = reqwest::Client::new();
    async fn rpc_call(
        http: &reqwest::Client,
        rpc: &str,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        });
        let resp: serde_json::Value = http
            .post(rpc)
            .json(&body)
            .send()
            .await
            .expect("rpc reachable")
            .json()
            .await
            .expect("rpc json");
        if let Some(err) = resp.get("error") {
            panic!("rpc {method} failed: {err}");
        }
        resp["result"].clone()
    }

    let signer = PrivateKeySigner::random();
    let addr = signer.address();
    println!("deployer: {addr}");

    // Fund via dev faucet (5 ETH is plenty: deploy + one call).
    let drip = rpc_call(
        &http,
        &rpc,
        "kanari_faucet",
        serde_json::json!([addr.to_string(), "5"]),
    )
    .await;
    println!("faucet tx: {drip}");

    // Nonce may lag the instant-seal faucet block; retry briefly.
    let mut nonce: u64 = 0;
    for _ in 0..30 {
        let n = rpc_call(
            &http,
            &rpc,
            "eth_getTransactionCount",
            serde_json::json!([addr.to_string(), "latest"]),
        )
        .await;
        let n = u64::from_str_radix(n.as_str().expect("hex").trim_start_matches("0x"), 16)
            .expect("nonce");
        // Faucet send does not touch our nonce; deploy starts at 0 — but
        // wait until the balance is visible so the deploy is not rejected.
        let bal = rpc_call(
            &http,
            &rpc,
            "eth_getBalance",
            serde_json::json!([addr.to_string(), "latest"]),
        )
        .await;
        if bal.as_str().expect("hex") != "0x0" {
            nonce = n;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Deploy.
    let init = contracts::deploy_init(contracts::SIMPLE_STORAGE_RUNTIME);
    let deploy_raw = sign_1559(&signer, create_tx(nonce, init)).await;
    let deploy_hash = rpc_call(
        &http,
        &rpc,
        "eth_sendRawTransaction",
        serde_json::json!([format!("0x{}", hex_encode(&deploy_raw))]),
    )
    .await;
    println!("deploy tx: {deploy_hash}");

    // Poll receipt.
    let mut contract = String::new();
    for _ in 0..50 {
        let r = rpc_call(
            &http,
            &rpc,
            "eth_getTransactionReceipt",
            serde_json::json!([deploy_hash]),
        )
        .await;
        if !r.is_null() {
            assert_eq!(r["status"], "0x1", "deploy must succeed");
            contract = r["contractAddress"].as_str().expect("address").to_string();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!contract.is_empty(), "deploy receipt not found");
    println!("contract: {contract}");

    // set(12345) as a signed on-chain transaction.
    let set_raw = sign_1559(
        &signer,
        call_tx(
            nonce + 1,
            contract.parse().expect("addr"),
            contracts::encode_set(12345),
        ),
    )
    .await;
    let set_hash = rpc_call(
        &http,
        &rpc,
        "eth_sendRawTransaction",
        serde_json::json!([format!("0x{}", hex_encode(&set_raw))]),
    )
    .await;
    println!("set tx: {set_hash}");
    for _ in 0..50 {
        let r = rpc_call(
            &http,
            &rpc,
            "eth_getTransactionReceipt",
            serde_json::json!([set_hash]),
        )
        .await;
        if !r.is_null() {
            assert_eq!(r["status"], "0x1", "set must succeed");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // get() must now return 12345.
    let out = rpc_call(
        &http,
        &rpc,
        "eth_call",
        serde_json::json!([{"from": addr.to_string(), "to": contract, "data": format!("0x{}", hex_encode(&contracts::encode_get())), "gas": "0x186a0", "gasPrice": "0x3b9aca00"}, "latest"]),
    )
    .await;
    println!("get() => {out}");
    let raw = hex_decode(out.as_str().expect("hex"));
    assert_eq!(raw.len(), 32, "get returns one word");
    assert_eq!(word_to_u64(&raw), 12345, "stored value must round-trip");
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn hex_decode(hex: &str) -> Vec<u8> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    assert!(hex.len().is_multiple_of(2));
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}
