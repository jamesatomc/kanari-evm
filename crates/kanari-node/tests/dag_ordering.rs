// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! DAG-ordered EVM execution: signed transfers ride mysticeti DAG blocks as
//! opaque payloads, consensus commits them in one deterministic order, and
//! the EVM executes that order. Final balances must match sequential
//! execution of the committed sequence.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, TxKind, U256, address};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use kanari_evm_consensus::ordering::DagOrdering;
use kanari_evm_move_execution::{
    DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, KanariChainSpec,
    KanariNode,
};
use std::collections::HashSet;

const ONE_ETH_WEI: u128 = 1_000_000_000_000_000_000;
const GWEI_WEI: u128 = 1_000_000_000;
const GAS_LIMIT: u64 = 21_000;

fn signed_transfer(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: alloy_primitives::Address,
    rt: &tokio::runtime::Runtime,
) -> Vec<u8> {
    let tx = TxEip1559 {
        chain_id: KANARI_EVM_DEV_CHAIN_ID,
        nonce,
        gas_limit: GAS_LIMIT,
        to: TxKind::Call(to),
        value: U256::from(ONE_ETH_WEI),
        input: Bytes::new(),
        access_list: Default::default(),
        max_fee_per_gas: GWEI_WEI,
        max_priority_fee_per_gas: GWEI_WEI,
    };
    let sig = rt
        .block_on(signer.sign_hash(&tx.signature_hash()))
        .expect("sign");
    let envelope = TxEnvelope::from(tx.into_signed(sig));
    let mut raw = Vec::new();
    envelope.encode_2718(&mut raw);
    raw
}

#[test]
fn dag_orders_evm_transfers_for_execution() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // Two funded signers, two transfers each (sequential nonces).
    let signer_a = PrivateKeySigner::random();
    let signer_b = PrivateKeySigner::random();
    let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let spec = KanariChainSpec::with_alloc(
        KANARI_EVM_DEV_CHAIN_ID,
        KANARI_EVM_GENESIS_SPEC,
        vec![
            (signer_a.address(), U256::from(DEV_FUNDED_BALANCE)),
            (signer_b.address(), U256::from(DEV_FUNDED_BALANCE)),
        ],
    );
    let tx_a0 = signed_transfer(&signer_a, 0, recipient, &rt);
    let tx_a1 = signed_transfer(&signer_a, 1, recipient, &rt);
    let tx_b0 = signed_transfer(&signer_b, 0, recipient, &rt);
    let tx_b1 = signed_transfer(&signer_b, 1, recipient, &rt);
    let submitted: HashSet<Vec<u8>> = [tx_a0.clone(), tx_a1.clone(), tx_b0.clone(), tx_b1.clone()]
        .into_iter()
        .collect();

    // Submit across two different authorities, then run consensus rounds.
    let mut ordering = DagOrdering::new(4);
    ordering.submit(0, vec![tx_a0, tx_a1]);
    ordering.submit(2, vec![tx_b0, tx_b1]);
    let commits = ordering.run_rounds(12);
    assert!(
        commits.iter().any(|c| !c.is_empty()),
        "DAG must commit within 12 rounds"
    );

    // Core 0's committed sequence holds every payload exactly once.
    let ordered = DagOrdering::ordered_payloads(&commits[0]);
    assert_eq!(
        ordered.len(),
        submitted.len(),
        "committed sequence must hold each payload exactly once: {ordered:?}"
    );
    assert_eq!(
        ordered.iter().collect::<HashSet<_>>(),
        submitted.iter().collect::<HashSet<_>>(),
        "committed set must equal submitted set"
    );

    // Every other core agrees on core 0's sequence as a prefix (safety).
    for (i, core_commits) in commits.iter().enumerate().skip(1) {
        let other = DagOrdering::ordered_payloads(core_commits);
        assert!(
            other.starts_with(&ordered) || ordered.starts_with(&other),
            "core {i} diverged from core 0"
        );
    }

    // Execute the committed order on the EVM and check final balances.
    let dir = std::env::temp_dir().join(format!("kanari-evm-dag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut node = KanariNode::open(spec, dir.join("state.json")).expect("open");
    for raw in &ordered {
        node.send_raw_transaction(raw.clone().into())
            .expect("ordered tx executes");
    }
    let fee_each = GAS_LIMIT as u128 * GWEI_WEI;
    assert_eq!(
        node.balance_of(signer_a.address()).expect("balance"),
        U256::from(DEV_FUNDED_BALANCE - 2 * ONE_ETH_WEI - 2 * fee_each)
    );
    assert_eq!(
        node.balance_of(signer_b.address()).expect("balance"),
        U256::from(DEV_FUNDED_BALANCE - 2 * ONE_ETH_WEI - 2 * fee_each)
    );
    assert_eq!(
        node.balance_of(recipient).expect("balance"),
        U256::from(4 * ONE_ETH_WEI)
    );

    std::fs::remove_dir_all(&dir).ok();
}
