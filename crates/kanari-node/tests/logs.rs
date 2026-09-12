// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Event logs end to end: deploy a tiny contract that emits LOG1, invoke
//! it, then prove the receipt carries the log and `eth_getLogs` filters it
//! (address / topics / block range) while `GET /metrics` reports the seals.

mod common;

use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, open_for_signer, rpc, serve, sign_1559};
use kanari_evm_move_execution::contracts;
use serde_json::json;

// Runtime: LOG1 with topic 0xaa over zero bytes, then STOP.
//   PUSH1 0xaa, PUSH1 0x00, PUSH1 0x00, LOG1, STOP
const LOG_RUNTIME: &[u8] = &[0x60, 0xaa, 0x60, 0x00, 0x60, 0x00, 0xa1, 0x00];
const LOG_TOPIC: &str = "0x00000000000000000000000000000000000000000000000000000000000000aa";

#[tokio::test]
async fn logs_flow_from_receipt_to_get_logs() {
    let funder = PrivateKeySigner::random();
    let (mut node, dir, _) = open_for_signer("logs", &funder);

    // Block 1: deploy the LOG contract.
    let init = contracts::deploy_init(LOG_RUNTIME);
    let deploy_raw = sign_1559(&funder, TxKind::Create, init.into(), 0, 1_000_000, GWEI_WEI, U256::ZERO).await;
    let deploy_hash = node.send_raw_transaction(deploy_raw).expect("deploy seals");
    let deploy_receipt = node.receipt(&deploy_hash).expect("receipt");
    assert!(deploy_receipt.success);
    assert!(deploy_receipt.logs.is_empty(), "constructor emits nothing");
    let contract: Address = deploy_receipt.contract_address.expect("created");

    // Block 2: invoke it — one LOG1 lands in the receipt.
    let call_raw = sign_1559(
        &funder,
        TxKind::Call(contract),
        Bytes::new(),
        1,
        100_000,
        GWEI_WEI,
        U256::ZERO,
    )
    .await;
    let call_hash = node.send_raw_transaction(call_raw).expect("call seals");
    let receipt = node.receipt(&call_hash).expect("receipt");
    assert!(receipt.success);
    assert_eq!(receipt.logs.len(), 1);
    assert_eq!(receipt.logs[0].address, contract);
    assert_eq!(receipt.logs[0].topics.len(), 1);
    assert_eq!(
        receipt.logs[0].topics[0].to_string(),
        LOG_TOPIC,
        "topic must be 0xaa"
    );

    // Serve RPC and prove eth_getLogs filtering + metrics.
    let (url, server) = serve(node).await;
    let client = reqwest::Client::new();

    let all = rpc(&client, &url, "eth_getLogs", json!([{}])).await;
    assert_eq!(all.as_array().expect("array").len(), 1);
    assert_eq!(all[0]["address"], json!(contract.to_string()));
    assert_eq!(all[0]["topics"], json!([LOG_TOPIC]));
    assert_eq!(all[0]["blockNumber"], json!("0x2"));

    // Address filter.
    let by_addr = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"address": contract.to_string()}]),
    )
    .await;
    assert_eq!(by_addr.as_array().expect("array").len(), 1);
    let no_addr = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"address": funder.address().to_string()}]),
    )
    .await;
    assert_eq!(no_addr, json!([]));

    // Topic filters: match, mismatch, wildcard, OR-list.
    let by_topic = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"topics": [LOG_TOPIC]}]),
    )
    .await;
    assert_eq!(by_topic.as_array().expect("array").len(), 1);
    let bad_topic = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"topics": ["0x00000000000000000000000000000000000000000000000000000000000000bb"]}]),
    )
    .await;
    assert_eq!(bad_topic, json!([]));
    let wildcard = rpc(&client, &url, "eth_getLogs", json!([{"topics": [null]}])).await;
    assert_eq!(wildcard.as_array().expect("array").len(), 1);
    let or_topics = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"topics": [["0x00000000000000000000000000000000000000000000000000000000000000bb", LOG_TOPIC]]}]),
    )
    .await;
    assert_eq!(or_topics.as_array().expect("array").len(), 1);

    // Block range: only block 2 holds the log.
    let ranged = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"fromBlock": "0x2", "toBlock": "0x2"}]),
    )
    .await;
    assert_eq!(ranged.as_array().expect("array").len(), 1);
    let empty_range = rpc(
        &client,
        &url,
        "eth_getLogs",
        json!([{"fromBlock": "0x0", "toBlock": "0x1"}]),
    )
    .await;
    assert_eq!(empty_range, json!([]));

    // Metrics report both seals.
    let metrics = client
        .get(format!("{url}metrics"))
        .send()
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("body");
    assert!(
        metrics.contains("kanari_evm_blocks_sealed_total 2"),
        "metrics must report 2 seals, got:\n{metrics}"
    );
    assert!(
        metrics.contains("kanari_evm_block_number 2"),
        "metrics must report head block 2, got:\n{metrics}"
    );

    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}

