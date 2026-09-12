// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Smart-contract lifecycle: deploy `SimpleStorage`, mutate state with a
//! signed `set()` transaction, and read it back with `get()`.
//!
//! The `live_deploy_simple_storage` test additionally runs the same flow
//! against a real node over JSON-RPC when `KANARI_EVM_LIVE_RPC` is set
//! (e.g. a local dev node with the faucet enabled).

mod common;

use alloy_consensus::{SignableTransaction, TxEip1559, TxEip7702, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_eips::eip7702::{Authorization, SignedAuthorization};
use alloy_primitives::{Address, TxKind, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, sign_tx};
use kanari_evm_move_execution::{
    CallRequest, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KanariChainSpec, KanariNode, contracts,
};

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

/// Normalize an ECDSA signature to canonical low-s form for EIP-7702
/// authorizations (revm drops high-s auths at recovery).
fn canonical_auth(auth: Authorization, sig: alloy_primitives::Signature) -> SignedAuthorization {
    /// secp256k1 group order.
    const N: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";
    let order = U256::from_str_radix(N, 16).expect("order");
    let (s, parity) = if sig.s() > order >> 1 {
        (order - sig.s(), !sig.v())
    } else {
        (sig.s(), sig.v())
    };
    SignedAuthorization::new_unchecked(auth, parity as u8, sig.r(), s)
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
        .send_raw_transaction(sign_tx(&deployer, create_tx(0, init)).await)
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
            sign_tx(
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

/// EIP-7702 set-code transactions: a real signed authorization delegates
/// the sender's account to SimpleStorage, then calling the sender runs
/// `set()` inside the delegation. The tx view reports type 4.
#[tokio::test]
async fn eip7702_delegates_and_executes() {
    let sender = PrivateKeySigner::random();
    let sender_addr = sender.address();
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(sender_addr, U256::from(DEV_FUNDED_BALANCE))],
    );
    let dir = std::env::temp_dir().join(format!("kanari-evm-7702-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");

    // Deploy the delegation target with a plain legacy-style tx (nonce 0).
    let init = contracts::deploy_init(contracts::SIMPLE_STORAGE_RUNTIME);
    let deploy_hash = node
        .send_raw_transaction(sign_tx(&sender, create_tx(0, init)).await)
        .expect("deploy seals");
    let target = node
        .receipt(&deploy_hash)
        .expect("receipt")
        .contract_address
        .expect("created");

    // Nonce choreography for SELF-sponsored 7702 in revm's phase order:
    // validate+deduce bumps the caller (1->2) BEFORE the auth list is
    // applied, so the tx runs at the pre-state nonce (1) while the auth
    // must carry the post-bump nonce (2). (Third-party authorities use
    // their plain state nonce — only the caller bump shifts this.)
    let auth = Authorization {
        chain_id: U256::from(KANARI_EVM_DEV_CHAIN_ID),
        address: target,
        nonce: 2,
    };
    let auth_sig = sender
        .sign_hash(&auth.signature_hash())
        .await
        .expect("sign auth");
    // revm drops authorizations whose `s` exceeds the curve half-order,
    // and k256 signing is not canonical — normalize (wallets always send
    // low-s). Production path needs no handling.
    let signed_auth = canonical_auth(auth, auth_sig);
    let tx = TxEip7702 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce: 1,
        gas_limit: 500_000,
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
        to: sender_addr,
        value: U256::ZERO,
        input: contracts::encode_set(777).into(),
        access_list: Default::default(),
        authorization_list: vec![signed_auth],
    };
    let sig = sender
        .sign_hash(&tx.signature_hash())
        .await
        .expect("sign tx");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    let hash = node.send_raw_transaction(raw.into()).expect("7702 seals");
    let receipt = node.receipt(&hash).expect("receipt");
    assert!(receipt.success, "7702 delegated call must succeed");
    let view = node.tx_view(&hash).expect("tx view");
    assert_eq!(view["type"], serde_json::json!("0x4"));

    // The sender account now carries the delegation designation...
    let code = node.code_of(sender_addr).expect("code");
    assert_eq!(
        &code[..3],
        &[0xef, 0x01, 0x00],
        "0xef0100 delegation marker"
    );
    assert_eq!(&code[3..], target.as_slice(), "delegates to target");
    // ...and set(777) ran in the sender's own storage.
    let out = node
        .call(CallRequest {
            from: Some(sender_addr),
            to: Some(sender_addr),
            data: Some(contracts::encode_get().into()),
            gas: Some(100_000),
            ..Default::default()
        })
        .expect("call");
    assert!(out.success);
    assert_eq!(word_to_u64(&out.output), 777);

    std::fs::remove_dir_all(&dir).ok();
}

/// Relay-sponsored EIP-7702 (the viem `signAuthorization` pattern): the
/// USER signs only the authorization (never pays gas); a separate RELAYER
/// submits and pays for the type-4 tx. Authority and sender nonces stay
/// independent, so no nonce choreography is needed.
#[tokio::test]
async fn eip7702_relay_sponsored() {
    let deployer = PrivateKeySigner::random();
    let user = PrivateKeySigner::random();
    let user_addr = user.address();
    let relayer = PrivateKeySigner::random();
    let relayer_addr = relayer.address();
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![
            (deployer.address(), U256::from(DEV_FUNDED_BALANCE)),
            (user_addr, U256::from(DEV_FUNDED_BALANCE)),
            (relayer_addr, U256::from(DEV_FUNDED_BALANCE)),
        ],
    );
    let dir = std::env::temp_dir().join(format!("kanari-evm-7702r-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");

    // Delegation target.
    let init = contracts::deploy_init(contracts::SIMPLE_STORAGE_RUNTIME);
    let deploy_hash = node
        .send_raw_transaction(sign_tx(&deployer, create_tx(0, init)).await)
        .expect("deploy seals");
    let target = node
        .receipt(&deploy_hash)
        .expect("receipt")
        .contract_address
        .expect("created");

    // USER (state nonce 0) authorizes the target; never sends a thing.
    let auth = Authorization {
        chain_id: U256::from(KANARI_EVM_DEV_CHAIN_ID),
        address: target,
        nonce: 0,
    };
    let auth_sig = user
        .sign_hash(&auth.signature_hash())
        .await
        .expect("user signs auth");
    let signed_auth = canonical_auth(auth, auth_sig);

    // RELAYER (state nonce 0) pays for a type-4 tx calling the USER
    // (now delegated): set(555) lands in the USER's storage.
    let tx = TxEip7702 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce: 0,
        gas_limit: 500_000,
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
        to: user_addr,
        value: U256::ZERO,
        input: contracts::encode_set(555).into(),
        access_list: Default::default(),
        authorization_list: vec![signed_auth],
    };
    let sig = relayer
        .sign_hash(&tx.signature_hash())
        .await
        .expect("relayer signs tx");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    let hash = node
        .send_raw_transaction(raw.into())
        .expect("relay 7702 seals");
    let receipt = node.receipt(&hash).expect("receipt");
    assert!(receipt.success, "relayed delegated call must succeed");
    assert_eq!(receipt.from, relayer_addr, "relayer paid");

    // Delegation sits on the USER, value in the USER's slot.
    let code = node.code_of(user_addr).expect("code");
    assert_eq!(&code[..3], &[0xef, 0x01, 0x00]);
    assert_eq!(&code[3..], target.as_slice());
    let out = node
        .call(CallRequest {
            from: Some(relayer_addr),
            to: Some(user_addr),
            data: Some(contracts::encode_get().into()),
            gas: Some(100_000),
            ..Default::default()
        })
        .expect("call");
    assert!(out.success);
    assert_eq!(word_to_u64(&out.output), 555);

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
    let deploy_raw = sign_tx(&signer, create_tx(nonce, init)).await;
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
    let set_raw = sign_tx(
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
