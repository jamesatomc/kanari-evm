// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Networked DAG validator: one process in a multi-node Kanari EVM network,
//! mirroring the `kanari-node` validator topology.
//!
//! Each validator runs a Mysticeti DAG core on a TCP full mesh
//! ([`dag::sync::network::Network::load`]), drives it with
//! [`NetworkSyncer`](dag::sync::NetworkSyncer), and executes committed
//! payloads (opaque signed EVM transactions) into its local [`KanariNode`]
//! in consensus order.
//!
//! Convergence across validators rests on two facts:
//!
//! 1. Consensus safety: every validator commits identical sub-DAGs in
//!    identical order.
//! 2. Deterministic sealing: each commit seals with its **anchor block
//!    timestamp** (never wall-clock), so block hashes and state roots match
//!    on every validator. Timestamps are clamped monotonic per commit
//!    stream, which is itself deterministic given identical commit order.
//!
//! While attached, the node's `send_raw_transaction` submits to the DAG
//! mempool instead of sealing instantly; receipts appear once a commit
//! covers the transaction.

use super::committee::{CommitteeError, load_validator};
use super::ordering::DagOrdering;
use crate::server_rpc::rpc::SharedNode;
use alloy_primitives::Bytes;
use consensus::{committer::Committer, protocol::ConsensusProtocol};
use dag::{
    block::transaction::Transaction,
    consensus::CommittedSubDag,
    context::TokioCtx,
    core::{
        Core,
        block_handler::{CommitHandler, RealBlockHandler},
    },
    crypto::CryptoEngine,
    metrics::Metrics,
    storage::Storage,
    sync::{net_sync::NetworkSyncer, network::Network},
};
use std::{path::PathBuf, time::Duration};

/// Default DAG round timeout for validators.
pub const DEFAULT_ROUND_TIMEOUT: Duration = Duration::from_secs(1);

/// Cap for the commit-execution pending queue (see spawn); oldest payloads
/// are dropped first once exceeded. Identical on all validators, so drops
/// never diverge state.
const MAX_PENDING_PAYLOADS: usize = 4096;

/// Options for spawning one networked validator.
pub struct ValidatorOpts {
    /// Shared `dag-committee.json` (every validator uses the same file).
    pub committee_path: PathBuf,
    /// This validator's `validator-{i}.key` secret file.
    pub key_path: PathBuf,
    /// DAG WAL directory for crash recovery. `None` keeps WAL in a
    /// tempfile (tests); production validators pass
    /// `<data-dir>/dag-wal` so restarts resume proposing cleanly.
    pub dag_wal_dir: Option<PathBuf>,
    /// Leader round timeout (shorter = faster commits on quiet networks).
    pub round_timeout: Duration,
}

/// A running networked validator: DAG mesh + commit-driven EVM execution.
///
/// Keep this value alive for as long as the validator should run — dropping
/// it stops the DAG tasks. The EVM side stays usable through
/// [`ValidatorNode::node`] (same `SharedNode` the RPC router serves).
pub struct ValidatorNode {
    node: SharedNode,
    _syncer: NetworkSyncer<TokioCtx, Committer>,
}

/// Validator spawn errors.
#[derive(Debug, thiserror::Error)]
pub enum ValidatorError {
    #[error(transparent)]
    Committee(#[from] CommitteeError),
    #[error("dag storage error: {0}")]
    Storage(String),
}

impl ValidatorNode {
    /// Spawn a validator around an already-opened node: attaches the DAG
    /// mempool sender, joins the DAG mesh, and starts executing commits.
    /// Must be called inside a Tokio runtime.
    pub async fn spawn(node: SharedNode, opts: ValidatorOpts) -> Result<Self, ValidatorError> {
        let loaded = load_validator(&opts.committee_path, &opts.key_path)?;
        let committee = loaded.committee.clone();
        let authority = loaded.authority;
        let metrics = Metrics::new_for_test(committee.len());

        let storage = match &opts.dag_wal_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|e| ValidatorError::Storage(e.to_string()))?;
                // NOTE: Storage::open takes a FILE path (it opens the WAL
                // itself); passing the directory fails with "Access denied"
                // on Windows. Mirrors mysticeti's storage-path.join("wal").
                let (storage, recovered) = Storage::open(
                    authority,
                    dir.join("wal"),
                    metrics.clone(),
                    &committee,
                )
                .map_err(|e| ValidatorError::Storage(e.to_string()))?;
                (storage, recovered)
            }
            None => Storage::ephemeral(authority, metrics.clone(), &committee),
        };
        let (storage, recovered) = storage;

        let protocol = ConsensusProtocol::default()
            .to_protocol(&committee)
            .expect("default protocol is infallible");
        let committer = Committer::new(committee.clone(), storage.block_reader().clone(), protocol);
        let (block_handler, mempool) = RealBlockHandler::new(metrics.clone());
        let transaction_time = block_handler.transaction_time.clone();
        let core = Core::open(
            block_handler,
            authority,
            committee.clone(),
            metrics.clone(),
            storage,
            recovered,
            false,
            committer,
            CryptoEngine::enabled(loaded.signer),
        );
        let commit_handler = CommitHandler::new(transaction_time, metrics.clone());

        let network =
            Network::load(&loaded.dag_addresses, authority, loaded.own_address, metrics.clone())
                .await;
        let (commit_tx, mut commit_rx) =
            tokio::sync::mpsc::channel::<CommittedSubDag>(1024);
        let syncer = NetworkSyncer::start(
            network,
            core,
            opts.round_timeout,
            true,
            commit_handler,
            metrics,
            Some(commit_tx),
        );

        // Node -> DAG direction: RPC submissions land here, get wrapped as
        // DAG transactions, and enter the local core's mempool.
        let (dag_tx, mut dag_rx) = tokio::sync::mpsc::channel::<Vec<Vec<u8>>>(1024);
        node.lock().await.set_dag_sender(dag_tx);
        tokio::spawn(async move {
            while let Some(batch) = dag_rx.recv().await {
                let txs: Vec<Transaction> =
                    batch.into_iter().map(|b| Transaction::new(b.into())).collect();
                if mempool.send(txs).await.is_err() {
                    break;
                }
            }
        });

        // DAG -> node direction: execute every commit in consensus order
        // with the commit's anchor timestamp (deterministic across
        // validators).
        //
        // Payloads that fail today may succeed tomorrow (e.g. a nonce gap
        // when a same-sender transaction commits first — standard mempool
        // behavior, the DAG itself orders nothing by nonce). Failed payloads
        // therefore wait in a pending queue that is swept to fixation on
        // every commit; the queue evolves identically on all validators
        // because the commit sequence is identical. Permanently invalid
        // payloads re-fail identically everywhere and are dropped once the
        // queue exceeds its cap (oldest first).
        let exec_node = node.clone();
        let validator_id = loaded.id.clone();
        tokio::spawn(async move {
            let mut last_ts: u64 = 0;
            let mut pending: Vec<Bytes> = Vec::new();
            while let Some(commit) = commit_rx.recv().await {
                let ts = commit_timestamp(&commit).max(last_ts);
                last_ts = ts;
                pending.extend(
                    DagOrdering::ordered_payloads(std::slice::from_ref(&commit))
                        .into_iter()
                        .map(Bytes::from),
                );
                if pending.len() > MAX_PENDING_PAYLOADS {
                    let drop_n = pending.len() - MAX_PENDING_PAYLOADS;
                    pending.drain(..drop_n);
                    eprintln!(
                        "validator {validator_id}: dropping {drop_n} overfull pending payloads"
                    );
                }
                loop {
                    let mut progress = false;
                    let mut still_pending = Vec::new();
                    for raw in pending.drain(..) {
                        match exec_node.lock().await.seal_committed(raw.clone(), ts) {
                            Ok(_) => progress = true,
                            Err(e) => {
                                eprintln!(
                                    "validator {validator_id}: payload deferred, retrying on next commit: {e}"
                                );
                                still_pending.push(raw);
                            }
                        }
                    }
                    pending = still_pending;
                    if !progress || pending.is_empty() {
                        break;
                    }
                }
            }
        });

        eprintln!(
            "validator {} (authority {}) joined DAG mesh on {} ({} validators)",
            loaded.id,
            loaded.index,
            loaded.own_address,
            committee.len()
        );
        Ok(Self {
            node,
            _syncer: syncer,
        })
    }

    /// The underlying node (same handle the RPC router serves).
    pub fn node(&self) -> &SharedNode {
        &self.node
    }
}

/// EVM block timestamp for a commit: its anchor block's timestamp.
///
/// The anchor is part of the committed sub-DAG, hence identical on every
/// validator. Falls back to the newest block timestamp in the commit (also
/// identical everywhere); `0` only for an empty commit, which never seals
/// anything.
fn commit_timestamp(commit: &CommittedSubDag) -> u64 {
    if let Some(anchor) = commit
        .blocks
        .iter()
        .find(|b| b.reference() == &commit.anchor)
    {
        return anchor.timestamp().as_secs();
    }
    commit
        .blocks
        .iter()
        .map(|b| b.timestamp().as_secs())
        .max()
        .unwrap_or(0)
}
