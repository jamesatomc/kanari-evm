// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! DAG ordering for EVM payloads, backed by mysticeti consensus.
//!
//! Phase 1 (in-process): validator cores exchange blocks over a full mesh
//! (a perfect network), commit through the standard Mysticeti committer, and
//! yield linearized payload batches for EVM execution. Payloads are opaque
//! signed EVM transactions (`Transaction::new` treats bytes as a black box).
//!
//! Networked multi-process validators reuse the same `Core`/`Committer` types
//! through the replica syncer loop; that transport is a follow-up phase and
//! deliberately NOT part of this driver.

use consensus::{committer::Committer, protocol::ConsensusProtocol};
use dag::{
    authority::Authority,
    block::Block,
    block::transaction::Transaction,
    committee::{AuthorityInfo, Committee},
    consensus::CommittedSubDag,
    context::TokioCtx,
    core::{
        Core,
        block_handler::{CommitHandler, RealBlockHandler},
    },
    crypto::{AsBytes, CryptoEngine},
    data::Data,
    metrics::Metrics,
    storage::Storage,
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Opaque EVM payload carried by the DAG (a signed raw transaction).
pub type DagPayload = Vec<u8>;

/// In-process DAG ordering driver: N validator cores, full-mesh exchange.
pub struct DagOrdering {
    committee: Arc<Committee>,
    authorities: Vec<Authority>,
    cores: Vec<Core<TokioCtx, Committer>>,
    handlers: Vec<CommitHandler<TokioCtx>>,
    senders: Vec<mpsc::Sender<Vec<Transaction>>>,
}

impl DagOrdering {
    /// Spin up `n_validators` cores with ephemeral storage and default
    /// consensus parameters, fully connected to each other.
    pub fn new(n_validators: usize) -> Self {
        assert!(n_validators >= 4, "need at least 4 validators for quorum");
        let committee: Arc<Committee> = Committee::new(
            (0..n_validators)
                .map(|_| AuthorityInfo::test_from_stake(1))
                .collect(),
        );
        let authorities: Vec<Authority> = committee.authorities().collect();

        let mut cores = Vec::with_capacity(n_validators);
        let mut handlers = Vec::with_capacity(n_validators);
        let mut senders = Vec::with_capacity(n_validators);
        for authority in &authorities {
            let metrics = Metrics::new_for_test(committee.len());
            let (storage, recovered) = Storage::ephemeral(*authority, metrics.clone(), &committee);
            let protocol = ConsensusProtocol::default()
                .to_protocol(&committee)
                .expect("default protocol is infallible");
            let committer =
                Committer::new(committee.clone(), storage.block_reader().clone(), protocol);
            let (block_handler, sender) = RealBlockHandler::new(metrics.clone());
            let transaction_time = block_handler.transaction_time.clone();
            let core = Core::open(
                block_handler,
                *authority,
                committee.clone(),
                metrics.clone(),
                storage,
                recovered,
                false,
                committer,
                CryptoEngine::disabled(),
            );
            handlers.push(CommitHandler::new(transaction_time, metrics));
            cores.push(core);
            senders.push(sender);
        }

        Self {
            committee,
            authorities,
            cores,
            handlers,
            senders,
        }
    }

    /// Number of validators.
    pub fn validator_count(&self) -> usize {
        self.authorities.len()
    }

    /// Submit opaque payloads to one validator's mempool. They are proposed
    /// in a later block via [`Core::drain_submitted_transactions`].
    pub fn submit(&self, authority_index: usize, payloads: Vec<DagPayload>) {
        let txs = payloads
            .into_iter()
            .map(|bytes| Transaction::new(bytes.into()))
            .collect();
        self.senders[authority_index]
            .try_send(txs)
            .expect("validator mempool has capacity");
    }

    /// Drive `rounds` full-mesh exchange rounds. Each round every core drains
    /// submitted transactions, proposes at most one block, receives every
    /// other core's proposal, then attempts to commit.
    ///
    /// Returns per-core committed sub-DAGs (one entry per core, in validator
    /// order). Consensus safety means all cores agree on a common prefix.
    pub fn run_rounds(&mut self, rounds: usize) -> Vec<Vec<CommittedSubDag>> {
        let mut committed: Vec<Vec<CommittedSubDag>> =
            (0..self.cores.len()).map(|_| Vec::new()).collect();
        for _ in 0..rounds {
            let mut new_blocks: Vec<Data<Block>> = Vec::new();
            for core in self.cores.iter_mut() {
                core.drain_submitted_transactions();
                if let Some(block) = core.try_new_block() {
                    new_blocks.push(block);
                }
            }
            for ((core, handler), out) in self
                .cores
                .iter_mut()
                .zip(self.handlers.iter_mut())
                .zip(committed.iter_mut())
            {
                core.add_blocks(new_blocks.clone());
                let leaders = core.try_commit();
                out.extend(handler.handle_commit(core.block_reader(), leaders));
            }
        }
        committed
    }

    /// Flatten committed sub-DAGs into payload bytes in consensus order.
    pub fn ordered_payloads(commits: &[CommittedSubDag]) -> Vec<DagPayload> {
        commits
            .iter()
            .flat_map(|commit| commit.blocks.iter())
            .flat_map(|block| block.transactions().iter())
            .map(|tx| tx.as_bytes().to_vec())
            .collect()
    }

    /// Committee (for cross-checking commits across cores in tests).
    pub fn committee(&self) -> &Arc<Committee> {
        &self.committee
    }
}
