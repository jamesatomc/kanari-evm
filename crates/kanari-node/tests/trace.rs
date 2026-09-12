// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Execution traces: `debug_traceTransaction` replays history exactly,
//! `debug_traceCall` runs against current state. Both serve Geth-style
//! struct logs through revm's EIP-3155 tracer.

mod common;

use alloy_primitives::{TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, open_for_signer, quantity_u128, rpc, serve, sign_1559};
use kanari_evm_move_execution::{CallRequest, TraceOptions, contracts};
use serde_json::json;

#[tokio::test]
async fn trace_transaction_matches_receipt() {
    let deployer = PrivateKeySigner::random();
    let (mut node, dir, _) = open_for_signer("trace", &deployer);

    // Deploy + set(12345): two sealed blocks.
    let init = contracts::deploy_init(contracts::SIMPLE_STORAGE_RUNTIME);
    let deploy_hash = node
        .send_raw_transaction(
            sign_1559(
                &deployer,
                TxKind::Create,
                init.into(),
                0,
                1_000_000,
                GWEI_WEI,
                U256::ZERO,
            )
            .await,
        )
        .expect("deploy seals");
    let contract = node
        .receipt(&deploy_hash)
        .expect("receipt")
        .contract_address
        .expect("created");
    let set_hash = node
        .send_raw_transaction(
            sign_1559(
                &deployer,
                TxKind::Call(contract),
                contracts::encode_set(12345).into(),
                1,
                500_000,
                GWEI_WEI,
                U256::ZERO,
            )
            .await,
        )
        .expect("set seals");
    let receipt_gas = {
        let receipt = node.receipt(&set_hash).expect("receipt");
        assert!(receipt.success);
        receipt.gas_used
    };

    // Historical trace: same gas as the receipt, real struct logs.
    let trace = node
        .trace_transaction(&set_hash, TraceOptions::default())
        .expect("trace works");
    assert!(!trace.failed);
    assert_eq!(trace.gas_used, receipt_gas, "trace gas matches receipt");
    assert!(
        !trace.struct_logs.is_empty(),
        "SSTORE call must step through opcodes"
    );
    let first = &trace.struct_logs[0];
    assert!(first.get("pc").is_some());
    assert!(first.get("op").is_some());
    assert!(first.get("gas").is_some());

    // Unknown hash fails closed.
    let bogus = alloy_primitives::B256::from_slice(&[9u8; 32]);
    assert!(
        node.trace_transaction(&bogus, TraceOptions::default())
            .is_err()
    );

    // TraceCall on current state agrees with eth_call.
    let traced = node
        .trace_call(
            CallRequest {
                from: Some(deployer.address()),
                to: Some(contract),
                data: Some(contracts::encode_get().into()),
                gas: Some(100_000),
                ..Default::default()
            },
            TraceOptions {
                disable_stack: true,
                disable_memory: true,
            },
        )
        .expect("traceCall works");
    assert!(!traced.failed);
    assert!(
        traced.struct_logs.iter().all(|l| l.get("stack").is_none()),
        "disableStack must strip stacks"
    );
    let live = node
        .call(CallRequest {
            from: Some(deployer.address()),
            to: Some(contract),
            data: Some(contracts::encode_get().into()),
            gas: Some(100_000),
            ..Default::default()
        })
        .expect("call");
    assert_eq!(traced.gas_used, live.gas_used);

    // Same assertions over RPC.
    let (url, server) = serve(node).await;
    let client = reqwest::Client::new();
    let rpc_trace = rpc(
        &client,
        &url,
        "debug_traceTransaction",
        json!([format!("{set_hash}"), {"disableMemory": true}]),
    )
    .await;
    assert_eq!(rpc_trace["failed"], json!(false));
    assert!(
        !rpc_trace["structLogs"]
            .as_array()
            .expect("array")
            .is_empty()
    );
    assert!(
        rpc_trace["structLogs"][0].get("memory").is_none(),
        "disableMemory must strip memory"
    );
    let gas_rpc = quantity_u128(&rpc_trace["gas"]);
    assert_eq!(gas_rpc, receipt_gas as u128);

    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}
