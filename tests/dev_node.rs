// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test: boot the dev node, drive it over JSON-RPC like a wallet
//! would (balance checks, signed transfer, receipt), then reopen the same
//! state file and verify persistence via replay.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, TxKind, U256, address};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use kanari_evm::{
    DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec,
    KanariNode, rpc,
};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

const ONE_ETH_WEI: u128 = 1_000_000_000_000_000_000;
const GWEI_WEI: u128 = 1_000_000_000;

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
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

fn quantity_hex(v: &Value) -> u128 {
    let s = v.as_str().expect("0x quantity");
    u128::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex quantity")
}

#[tokio::test]
async fn dev_node_serves_wallet_flow_and_persists() {
    // Fresh random funder: funds come from a custom genesis (no well-known
    // keys anywhere in this test).
    let funder = PrivateKeySigner::random();
    let funder_addr = funder.address();
    let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(funder_addr, U256::from(DEV_FUNDED_BALANCE))],
    );

    let dir = std::env::temp_dir().join(format!("kanari-evm-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let state_file = dir.join("state.json");

    let node = KanariNode::open(spec.clone(), &state_file).expect("open node");
    let app = rpc::router(Arc::new(Mutex::new(node)));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let client = reqwest::Client::new();

    // chainId + prefunded balance.
    let chain_id = rpc(&client, &url, "eth_chainId", json!([])).await;
    assert_eq!(chain_id, json!(format!("0x{KANARI_EVM_DEV_CHAIN_ID:x}")));
    let balance = rpc(
        &client,
        &url,
        "eth_getBalance",
        json!([funder_addr.to_string(), "latest"]),
    )
    .await;
    assert_eq!(quantity_hex(&balance), DEV_FUNDED_BALANCE);

    // Sign an EIP-1559 transfer and submit it as a raw transaction.
    let tx = TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce: 0,
        gas_limit: 21_000,
        to: recipient.into(),
        value: U256::from(ONE_ETH_WEI),
        input: Bytes::new(),
        access_list: Default::default(),
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    };
    let sig = funder.sign_hash(&tx.signature_hash()).await.expect("sign");
    let signed = tx.into_signed(sig);
    let envelope = TxEnvelope::from(signed);
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    let raw_hex = format!("0x{}", alloy_primitives::hex::encode(&raw));

    let tx_hash = rpc(&client, &url, "eth_sendRawTransaction", json!([raw_hex])).await;
    let expected_hash = format!(
        "0x{}",
        alloy_primitives::hex::encode(alloy_primitives::keccak256(&raw))
    );
    assert_eq!(tx_hash.as_str().expect("hash"), expected_hash);

    // Receipt reports success with exact intrinsic gas.
    let receipt = rpc(&client, &url, "eth_getTransactionReceipt", json!([tx_hash])).await;
    assert_eq!(receipt["status"], json!("0x1"));
    assert_eq!(receipt["gasUsed"], json!("0x5208"));
    assert_eq!(
        receipt["from"].as_str().expect("from"),
        funder_addr.to_string().as_str()
    );

    // Balances moved exactly: value + 21000 * 1 gwei fee.
    let fee = 21_000u128 * GWEI_WEI;
    let sender_after = rpc(
        &client,
        &url,
        "eth_getBalance",
        json!([funder_addr.to_string(), "latest"]),
    )
    .await;
    assert_eq!(
        quantity_hex(&sender_after),
        DEV_FUNDED_BALANCE - ONE_ETH_WEI - fee
    );
    let paid_after = rpc(
        &client,
        &url,
        "eth_getBalance",
        json!([recipient.to_string(), "latest"]),
    )
    .await;
    assert_eq!(quantity_hex(&paid_after), ONE_ETH_WEI);

    // Block + tx views line up.
    let block_number = rpc(&client, &url, "eth_blockNumber", json!([])).await;
    assert_eq!(block_number, json!("0x1"));
    let block = rpc(&client, &url, "eth_getBlockByNumber", json!(["0x1", false])).await;
    assert_eq!(block["transactions"][0], tx_hash);
    let tx_view = rpc(&client, &url, "eth_getTransactionByHash", json!([tx_hash])).await;
    assert_eq!(tx_view["nonce"], json!("0x0"));

    server.abort();

    // Reopen from the same state file: replay must reproduce everything.
    let reopened = KanariNode::open(spec, &state_file).expect("reopen");
    assert_eq!(reopened.block_number(), 1);
    let mut reopened = reopened;
    assert_eq!(
        reopened.balance_of(funder_addr).expect("balance"),
        U256::from(DEV_FUNDED_BALANCE - ONE_ETH_WEI - fee)
    );
    assert!(
        reopened
            .receipt(&tx_hash.as_str().expect("h").parse().expect("b256"))
            .is_some()
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn dev_node_serves_explorer_page() {
    let dir = std::env::temp_dir().join(format!("kanari-evm-explorer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let node = KanariNode::open(KanariChainSpec::devnet(), dir.join("state.json")).expect("open");
    let app = rpc::router(Arc::new(Mutex::new(node)));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let client = reqwest::Client::new();
    let html = client
        .get(&url)
        .send()
        .await
        .expect("GET /")
        .text()
        .await
        .expect("body");
    assert!(html.contains("Kanari EVM Explorer"), "explorer page served");
    assert!(
        html.contains("eth_getBalance"),
        "page drives balances via RPC"
    );
    assert!(
        html.contains("kanari_supply"),
        "page shows supply via kanari_supply"
    );
    // Supply endpoint: circulating == dev genesis (no faucet alloc here),
    // max == 11M protocol cap.
    let supply = rpc(&client, &url, "kanari_supply", json!([])).await;
    assert_eq!(
        supply["totalSupply"],
        format!("0x{:x}", DEV_FUNDED_BALANCE),
        "circulating must equal genesis sum"
    );
    assert_eq!(
        supply["maxSupply"],
        format!(
            "0x{:x}",
            kanari_evm::KANARI_EVM_MAX_SUPPLY_ETH * ONE_ETH_WEI
        ),
        "max must be the 11M protocol cap"
    );
    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn dev_node_answers_batch_requests_like_wallets_send() {
    // Wallets batch `eth_chainId` + `eth_blockNumber` + `eth_getBalance` on
    // load; a node that 422s batches shows 0 balances.
    let dir = std::env::temp_dir().join(format!("kanari-evm-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let state_file = dir.join("state.json");

    let node = KanariNode::open(KanariChainSpec::devnet(), &state_file).expect("open");
    let app = rpc::router(Arc::new(Mutex::new(node)));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let client = reqwest::Client::new();

    let batch = json!([
        {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []},
        {"jsonrpc": "2.0", "id": 2, "method": "eth_blockNumber", "params": []},
        {"jsonrpc": "2.0", "id": 3, "method": "eth_getBalance",
         "params": [kanari_evm::DEV_FUNDED_ACCOUNT.to_string(), "latest"]},
        {"jsonrpc": "2.0", "id": 4, "method": "no_such_method", "params": []},
    ]);
    let resp: Value = client
        .post(&url)
        .json(&batch)
        .send()
        .await
        .expect("rpc reachable")
        .json()
        .await
        .expect("valid json");
    let arr = resp.as_array().expect("batch returns array");
    assert_eq!(arr.len(), 4, "one response per request");
    assert_eq!(
        arr[0]["result"],
        json!(format!("0x{KANARI_EVM_DEV_CHAIN_ID:x}"))
    );
    assert_eq!(arr[1]["result"], json!("0x0"));
    assert_eq!(
        quantity_hex(&arr[2]["result"]),
        kanari_evm::DEV_FUNDED_BALANCE
    );
    assert_eq!(arr[3]["error"]["code"], json!(-32601));

    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}

// Priority fees (max_fee above the 1 gwei base) accrue to the block
// beneficiary treasury � this is the "gas kicked back" path.
#[tokio::test]
async fn block_priority_fees_accrue_to_beneficiary() {
    let funder = PrivateKeySigner::random();
    let funder_addr = funder.address();
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![(funder_addr, U256::from(DEV_FUNDED_BALANCE))],
    );
    let dir = std::env::temp_dir().join(format!("kanari-evm-fees-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");

    assert_eq!(
        node.balance_of(kanari_evm::BLOCK_BENEFICIARY)
            .expect("balance"),
        U256::ZERO,
        "treasury starts empty"
    );

    // Plain transfer paying 2 gwei max / 1 gwei priority over the 1 gwei base.
    let tx = TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce: 0,
        gas_limit: 100_000,
        to: TxKind::Call(address!("70997970C51812dc3A010C7d01b50e0d17dc79C8")),
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
        max_fee_per_gas: 2 * GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    };
    let hash = tx.signature_hash();
    let sig = funder.sign_hash(&hash).await.expect("sign");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    let tx_hash = node
        .send_raw_transaction(Bytes::from(raw))
        .expect("transfer seals");
    let receipt = node.receipt(&tx_hash).expect("receipt");
    assert!(receipt.success);
    assert_eq!(receipt.gas_used, 21_000, "plain transfer costs 21k gas");
    let gas_used = receipt.gas_used;
    let _ = receipt;

    // Treasury earned exactly gas_used x priority fee; the base fee burned.
    let got = node
        .balance_of(kanari_evm::BLOCK_BENEFICIARY)
        .expect("treasury balance");
    assert_eq!(
        got,
        U256::from(gas_used) * U256::from(GWEI_WEI),
        "beneficiary must receive the full priority fee"
    );

    std::fs::remove_dir_all(&dir).ok();
}
