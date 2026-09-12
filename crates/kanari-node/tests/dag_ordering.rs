// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! DAG-ordered EVM execution: signed transfers ride mysticeti DAG blocks as
//! opaque payloads, consensus commits them in one deterministic order, and
//! the EVM executes that order. Final balances must match sequential
//! execution of the committed sequence.

mod common;

use alloy_primitives::{Bytes, TxKind, U256, address};
use alloy_signer_local::PrivateKeySigner;
use common::{GWEI_WEI, ONE_ETH_WEI, open_funded, sign_1559};
use kanari_evm_consensus::ordering::DagOrdering;
use kanari_evm_move_execution::DEV_FUNDED_BALANCE;
use std::collections::HashSet;

#[tokio::test]
async fn dag_orders_evm_transfers_for_execution() {
    // Two funded signers, two transfers each (sequential nonces).
    let signer_a = PrivateKeySigner::random();
    let signer_b = PrivateKeySigner::random();
    let recipient = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    let (mut node, dir) = open_funded(
        "dag",
        &[
            (signer_a.address(), U256::from(DEV_FUNDED_BALANCE)),
            (signer_b.address(), U256::from(DEV_FUNDED_BALANCE)),
        ],
    );
    let value = U256::from(ONE_ETH_WEI);
    let tx_a0 = sign_1559(
        &signer_a,
        TxKind::Call(recipient),
        Bytes::new(),
        0,
        21_000,
        GWEI_WEI,
        value,
    )
    .await
    .to_vec();
    let tx_a1 = sign_1559(
        &signer_a,
        TxKind::Call(recipient),
        Bytes::new(),
        1,
        21_000,
        GWEI_WEI,
        value,
    )
    .await
    .to_vec();
    let tx_b0 = sign_1559(
        &signer_b,
        TxKind::Call(recipient),
        Bytes::new(),
        0,
        21_000,
        GWEI_WEI,
        value,
    )
    .await
    .to_vec();
    let tx_b1 = sign_1559(
        &signer_b,
        TxKind::Call(recipient),
        Bytes::new(),
        1,
        21_000,
        GWEI_WEI,
        value,
    )
    .await
    .to_vec();
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
    // (node opened above with both signers funded).
    for raw in &ordered {
        node.send_raw_transaction(raw.clone().into())
            .expect("ordered tx executes");
    }
    let fee_each = 21_000u128 * GWEI_WEI;
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
