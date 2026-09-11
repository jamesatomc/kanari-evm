// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::print_stdout)]

//! Kanari EVM node entry point, shaped after `kanari-node`:
//! clap subcommands (`start` / `local` / `reset`), a boot banner, and a
//! standard data-directory layout.
//!
//! ```text
//! kanari-evm-node start [--network devnet] [--rpc-port 8545]
//!                       [--rpc-host 127.0.0.1] [--data-dir ...]
//! kanari-evm-node local   # one-shot localhost dev node (faucet on)
//! kanari-evm-node reset [--data-dir ...] [--force]
//! ```
//!
//! `--rpc-host 0.0.0.0` exposes the node on the LAN (e.g. for phone wallets
//! on the same Wi-Fi — a phone's `127.0.0.1` is itself, not this PC).
//!
//! The data directory holds the replay journal (`state.json`), the RocksDB
//! chain database (`*.chain.db/`), and the dev faucet key (`*.faucet.key`).
//! Deleting it (or `reset`) restarts the chain from genesis.

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use clap::{Parser, Subcommand, ValueEnum};
use kanari_evm::{
    DEV_FUNDED_ACCOUNT, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KanariChainSpec, KanariNode, rpc,
};
use std::{net::SocketAddr, str::FromStr, sync::Arc};
use tokio::sync::Mutex;

const WEI_IN_ETH: u128 = 1_000_000_000_000_000_000;
const DEFAULT_FAUCET_ETH: u128 = 100;

/// Network mode for chain parameters and production safety defaults.
/// Only `devnet` has a chain spec today; the others fail fast with a clear
/// message instead of silently running dev parameters.
#[derive(Clone, Debug, ValueEnum)]
enum NetworkMode {
    Devnet,
    Testnet,
    Mainnet,
}

impl NetworkMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Devnet => "devnet",
            Self::Testnet => "testnet",
            Self::Mainnet => "mainnet",
        }
    }
}

/// Kanari EVM node command-line interface.
#[derive(Parser)]
#[command(name = "kanari-evm-node", about = "Kanari EVM run server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the node (persistent data dir, LAN-capable).
    Start {
        /// Network mode for chain parameters.
        #[arg(long, value_enum, default_value = "devnet")]
        network: NetworkMode,
        /// JSON-RPC listen port.
        #[arg(long, default_value = "8545")]
        rpc_port: u16,
        /// JSON-RPC listen host/IP (use 0.0.0.0 to bind all interfaces).
        #[arg(long, default_value = "127.0.0.1")]
        rpc_host: String,
        /// Data directory for chain state (defaults to ~/.kanari/evm-devnet).
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
        /// Fund an extra address at genesis: --faucet 0xAddr [whole-eth].
        /// Only applies to fresh state. Repeatable.
        #[arg(long, action = clap::ArgAction::Append, num_args = 1..=2, value_names = ["ADDRESS", "ETH"], value_parser = clap::value_parser!(String))]
        faucet: Vec<Vec<String>>,
        /// Pin the dev faucet account to your own 32-byte hex secret.
        /// DEV ONLY — never use a real key.
        #[arg(long)]
        faucet_key: Option<String>,
        /// Legacy state-file path (overrides --data-dir layout).
        #[arg(long, hide = true)]
        state_file: Option<std::path::PathBuf>,
    },
    /// Run a local-only dev node: RPC on 127.0.0.1:8545, data in
    /// ./.kanari-evm-local, faucet auto-created.
    Local {
        /// JSON-RPC listen port.
        #[arg(long, default_value = "8545")]
        rpc_port: u16,
    },
    /// Wipe chain state in the data directory (fresh genesis on next start).
    Reset {
        /// Data directory to wipe (defaults to ~/.kanari/evm-devnet).
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
        /// Skip the confirmation prompt.
        #[arg(long, default_value = "false")]
        force: bool,
    },
}

fn default_data_dir() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        std::path::PathBuf::from(home)
            .join(".kanari")
            .join("evm-devnet")
    } else {
        std::path::PathBuf::from("./.kanari-evm-devnet")
    }
}

fn print_boot_banner(
    network: &NetworkMode,
    rpc_host: &str,
    rpc_port: u16,
    data_dir: &std::path::Path,
    chain_id: u64,
) {
    println!("========================================");
    println!("Starting Kanari EVM Node ({})", network.as_str());
    println!("========================================");
    println!("RPC URL:   http://{rpc_host}:{rpc_port}");
    println!("Chain ID:  {chain_id}");
    println!("Data Dir:  {}", data_dir.display());
    println!("========================================");
    println!();
}

fn fatal(message: &str) -> ! {
    eprintln!("kanari-evm-node: {message}");
    std::process::exit(1);
}

fn parse_faucet_args(raw: &[Vec<String>]) -> Vec<(Address, u128)> {
    let mut out = Vec::new();
    for group in raw {
        let addr = group
            .first()
            .unwrap_or_else(|| fatal("--faucet needs an address"));
        let addr = Address::from_str(addr.trim())
            .unwrap_or_else(|_| fatal(&format!("invalid faucet address: {addr}")));
        let amount = match group.get(1) {
            Some(next) => next
                .parse::<u128>()
                .unwrap_or_else(|_| fatal(&format!("invalid faucet amount: {next}"))),
            None => DEFAULT_FAUCET_ETH,
        };
        out.push((addr, amount));
    }
    out
}

struct StartOptions {
    network: NetworkMode,
    rpc_port: u16,
    rpc_host: String,
    data_dir: std::path::PathBuf,
    state_file: std::path::PathBuf,
    faucets: Vec<(Address, u128)>,
    faucet_key: Option<B256>,
}

fn parse_secret(hex: &str) -> B256 {
    hex.trim()
        .parse()
        .unwrap_or_else(|_| fatal("invalid --faucet-key: want 32-byte 0x hex"))
}

async fn run(opts: StartOptions) {
    if !matches!(opts.network, NetworkMode::Devnet) {
        fatal(&format!(
            "--network {} has no EVM chain spec yet; use devnet",
            opts.network.as_str()
        ));
    }
    let data_dir = opts.data_dir;
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        fatal(&format!(
            "cannot create data dir {}: {e}",
            data_dir.display()
        ));
    }
    let state_file = opts.state_file;
    let fresh = !state_file.exists();

    let faucet_secret: Option<(Address, B256)> = match opts.faucet_key {
        Some(secret) => {
            let signer = PrivateKeySigner::from_bytes(&secret)
                .unwrap_or_else(|_| fatal("invalid --faucet-key secret"));
            eprintln!("kanari-evm-node: WARNING: faucet key from command line (dev only!)");
            Some((signer.address(), secret))
        }
        None if fresh => {
            let (addr, secret) = kanari_evm::generate_faucet_key();
            println!("faucet account (dev only): {addr}");
            Some((addr, secret))
        }
        None => None,
    };
    let spec = if opts.faucets.is_empty() && faucet_secret.is_none() {
        KanariChainSpec::devnet()
    } else {
        if !fresh {
            eprintln!("kanari-evm-node: --faucet only applies to fresh state; ignoring");
        }
        let mut alloc = vec![(DEV_FUNDED_ACCOUNT, U256::from(DEV_FUNDED_BALANCE))];
        if fresh {
            for (addr, eth) in &opts.faucets {
                alloc.push((*addr, U256::from(eth.saturating_mul(WEI_IN_ETH))));
            }
            if let Some((addr, _)) = &faucet_secret {
                alloc.push((
                    *addr,
                    U256::from(kanari_evm::FAUCET_GENESIS_ETH.saturating_mul(WEI_IN_ETH)),
                ));
            }
        }
        KanariChainSpec::with_alloc(KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, alloc)
    };

    let mut node = KanariNode::open(spec, &state_file)
        .unwrap_or_else(|e| fatal(&format!("failed to open node: {e}")));
    if let Some((addr, secret)) = faucet_secret {
        node.set_faucet_key(secret)
            .unwrap_or_else(|e| fatal(&format!("failed to store faucet key: {e}")));
        println!(
            "faucet funded with {} ETH for {addr}",
            kanari_evm::FAUCET_GENESIS_ETH
        );
    } else {
        match node.load_faucet_key() {
            Ok(true) => println!(
                "faucet enabled for {}",
                node.faucet_address()
                    .expect("key just loaded")
                    .expect("address derivable")
            ),
            Ok(false) => println!("faucet disabled (no key file; fresh state creates one)"),
            Err(e) => fatal(&format!("failed to load faucet key: {e}")),
        }
    }

    print_boot_banner(
        &opts.network,
        &opts.rpc_host,
        opts.rpc_port,
        &data_dir,
        node.chain_id(),
    );
    println!(
        "kanari-evm {}: block={} state={}",
        opts.network.as_str(),
        node.block_number(),
        state_file.display()
    );

    let app = rpc::router(Arc::new(Mutex::new(node)));
    let bind: std::net::IpAddr = opts
        .rpc_host
        .parse()
        .unwrap_or_else(|_| fatal("--rpc-host needs an IP address"));
    let addr = SocketAddr::new(bind, opts.rpc_port);
    println!("JSON-RPC listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| fatal(&format!("failed to bind {addr}: {e}")));
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| fatal(&format!("server error: {e}")));
}

fn cmd_reset(data_dir: Option<std::path::PathBuf>, force: bool) {
    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    if !data_dir.exists() {
        println!("nothing to reset: {} does not exist", data_dir.display());
        return;
    }
    if !force {
        println!(
            "This deletes ALL chain state in {}. Type YES to continue:",
            data_dir.display()
        );
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() || line.trim() != "YES" {
            println!("aborted");
            return;
        }
    }
    let mut removed = 0u32;
    for entry in std::fs::read_dir(&data_dir)
        .unwrap_or_else(|e| fatal(&format!("cannot read {}: {e}", data_dir.display())))
        .flatten()
    {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let state_entry = name == "state.json"
            || name.ends_with(".chain.db")
            || name.ends_with(".faucet.key")
            || name.ends_with(".smt");
        if !state_entry {
            continue;
        }
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .unwrap_or_else(|e| fatal(&format!("cannot remove {}: {e}", path.display())));
        removed += 1;
    }
    println!(
        "reset complete: removed {removed} state entr(y/ies) from {}",
        data_dir.display()
    );
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start {
            network,
            rpc_port,
            rpc_host,
            data_dir,
            faucet,
            faucet_key,
            state_file,
        } => {
            let data_dir = data_dir.unwrap_or_else(default_data_dir);
            let state_path = state_file.unwrap_or_else(|| data_dir.join("state.json"));
            run(StartOptions {
                network,
                rpc_port,
                rpc_host,
                data_dir,
                state_file: state_path,
                faucets: parse_faucet_args(&faucet),
                faucet_key: faucet_key.as_deref().map(parse_secret),
            })
            .await;
        }
        Commands::Local { rpc_port } => {
            let data_dir = std::path::PathBuf::from("./.kanari-evm-local");
            let state_path = data_dir.join("state.json");
            println!("Starting local node: RPC on 127.0.0.1:{rpc_port} (LAN access disabled)");
            run(StartOptions {
                network: NetworkMode::Devnet,
                rpc_port,
                rpc_host: "127.0.0.1".to_string(),
                data_dir,
                state_file: state_path,
                faucets: Vec::new(),
                faucet_key: None,
            })
            .await;
        }
        Commands::Reset { data_dir, force } => cmd_reset(data_dir, force),
    }
}
