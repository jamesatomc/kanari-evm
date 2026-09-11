// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Core consensus and durability: Mysticeti DAG transaction ordering plus the
//! RocksDB-backed chain store (blocks, receipts, metadata) shared with the
//! SMT state-commitment layer.

pub mod ordering;
pub mod store;

pub use store::ChainStore;
