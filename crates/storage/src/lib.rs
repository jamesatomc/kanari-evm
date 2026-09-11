// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM durable chain storage on `kanari-db-common` (RocksDB):
//! sealed blocks, receipts, transaction index and chain metadata, plus the
//! stored shapes (`SealedBlock`, `StoredReceipt`) shared with the execution
//! layer. See `store.rs` for the key layout.

pub mod store;

pub use store::{ChainStore, SealedBlock, StoreError, StoredReceipt};
