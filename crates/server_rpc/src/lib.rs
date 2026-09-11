// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Kanari EVM JSON-RPC server: minimal Ethereum JSON-RPC over HTTP (axum)
//! plus the bundled single-file block explorer UI.

pub mod rpc;

// Re-exported so existing `server_rpc::SharedNode` paths keep working;
// the alias is owned by the execution layer.
pub use kanari_evm_move_execution::node::SharedNode;
