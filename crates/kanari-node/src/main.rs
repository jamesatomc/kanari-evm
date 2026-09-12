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
use kanari_evm_consensus::{ValidatorNode, ValidatorOpts};
use kanari_evm_move_execution::{
    DEV_FUNDED_ACCOUNT, DEV_FUNDED_BALANCE, KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC,
    KanariChainSpec, KanariNode, MAX_FAUCET_ETH_PER_REQUEST, generate_faucet_key, node::SharedNode,
};
use kanari_evm_rpc::rpc;
use std::{net::SocketAddr, str::FromStr, sync::Arc};
use tokio::sync::Mutex;

use kanari_evm_node::fork;

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

/// Verbosity of structured logs (tracing). Banners and key material always
/// print to stdout regardless of level.
#[derive(Clone, Debug, Default, ValueEnum)]
enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Install the global tracing subscriber once. Returns an error string when
/// the level is invalid (clap constrains this, so it only fires for config
/// files).
fn init_logging(level: &str) -> Result<(), String> {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_new(format!(
        "kanari_evm_consensus={level},kanari_evm_rpc={level},kanari_evm_node={level},kanari_evm_move_execution={level}"
    ))
    .map_err(|e| format!("invalid log level '{level}': {e}"))?;
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|e| format!("logging already initialized: {e}"))?;
    Ok(())
}

/// Wait for Ctrl+C (or SIGTERM where supported). Dropping into this future
/// lets `axum::serve(...).with_graceful_shutdown(...)` drain in-flight RPC
/// calls before the process exits.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .unwrap_or_else(|e| fatal(&format!("failed to listen for shutdown signal: {e}")));
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .unwrap_or_else(|e| fatal(&format!("failed to listen for SIGTERM: {e}")))
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining...");
}

/// Validator settings loadable from a TOML file (`--config`). Every field
/// is optional: CLI flags override file values, built-in defaults cover
/// the rest.
///
/// ```toml
/// committee = "./dag-keys/dag-committee.json"
/// key = "./dag-keys/validator-1.key"
/// data_dir = "./.kanari-evm-validator-1"
/// rpc_host = "127.0.0.1"
/// rpc_port = 3501
/// log_level = "info"
/// # faucet_key = "0x..."   # same value on ALL validators, or omit
/// ```
#[derive(Debug, Default, Clone, serde::Deserialize)]
struct ValidatorFileConfig {
    committee: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
    rpc_host: Option<String>,
    rpc_port: Option<u16>,
    log_level: Option<String>,
    faucet_key: Option<String>,
}

fn load_validator_file(path: &std::path::Path) -> ValidatorFileConfig {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| fatal(&format!("cannot read --config {}: {e}", path.display())));
    toml::from_str(&raw)
        .unwrap_or_else(|e| fatal(&format!("invalid --config {}: {e}", path.display())))
}

/// Raw validator settings from CLI flags (all optional — `--config` may
/// supply them).
struct ValidatorCli {
    committee: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
    rpc_port: Option<u16>,
    rpc_host: Option<String>,
    faucet_key: Option<String>,
    log_level: Option<LogLevel>,
}

/// Merged validator settings after applying CLI > file > default precedence.
#[derive(Debug)]
struct ResolvedValidator {
    committee: std::path::PathBuf,
    key: std::path::PathBuf,
    data_dir: Option<std::path::PathBuf>,
    rpc_port: Option<u16>,
    rpc_host: String,
    faucet_key: Option<String>,
    log_level: String,
}

/// Merge CLI flags over an optional TOML file. Pure (no I/O) for testability;
/// the caller turns `Err` into a fatal CLI error.
fn resolve_validator_config(
    cli: ValidatorCli,
    file: ValidatorFileConfig,
) -> Result<ResolvedValidator, String> {
    Ok(ResolvedValidator {
        committee: cli
            .committee
            .or(file.committee)
            .ok_or_else(|| "--committee is required (or set committee in --config)".to_string())?,
        key: cli
            .key
            .or(file.key)
            .ok_or_else(|| "--key is required (or set key in --config)".to_string())?,
        data_dir: cli.data_dir.or(file.data_dir),
        rpc_port: cli.rpc_port.or(file.rpc_port),
        rpc_host: cli
            .rpc_host
            .or(file.rpc_host)
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        faucet_key: cli.faucet_key.or(file.faucet_key),
        log_level: cli
            .log_level
            .map(|l| l.as_str().to_string())
            .or(file.log_level)
            .unwrap_or_else(|| "info".to_string()),
    })
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
        /// Log verbosity (tracing).
        #[arg(long, value_enum, default_value = "info")]
        log_level: LogLevel,
    },
    /// Run a local-only dev node: RPC on 127.0.0.1:8545, data in
    /// ./.kanari-evm-local, faucet auto-created.
    Local {
        /// JSON-RPC listen port.
        #[arg(long, default_value = "8545")]
        rpc_port: u16,
        /// Log verbosity (tracing).
        #[arg(long, value_enum, default_value = "info")]
        log_level: LogLevel,
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
    /// Generate a DAG validator committee: shared dag-committee.json plus
    /// one validator-{i}.key secret per validator (kanari-sdk
    /// `consensus-keygen` equivalent).
    Keygen {
        /// Number of validators (minimum 4 for quorum).
        #[arg(long, default_value = "4")]
        node_count: usize,
        /// Output directory for the committee + key files.
        #[arg(long, default_value = "./dag-keys")]
        output_dir: std::path::PathBuf,
        /// Host IP for the validators' DAG listeners.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// DAG port of validator 1 (10-port stride per validator;
        /// keep `(base + (count-1) * 10) * 10 <= 65535` — the mesh dials
        /// out from source port `listen * 10`).
        #[arg(long, default_value = "3500")]
        base_dag_port: u16,
    },
    /// Run as one networked DAG validator (multi-node mode): transactions
    /// flow through Mysticeti DAG ordering and seal on commit, so every
    /// validator converges to identical blocks and state roots.
    Validator {
        /// Shared committee file (dag-committee.json from `keygen`).
        /// Required unless `--config` provides it.
        #[arg(long)]
        committee: Option<std::path::PathBuf>,
        /// This validator's secret key file (validator-{i}.key).
        /// Required unless `--config` provides it.
        #[arg(long)]
        key: Option<std::path::PathBuf>,
        /// Data directory for chain state (defaults to
        /// ./.kanari-evm-validator-{i}).
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
        /// JSON-RPC listen port (defaults to the validator's DAG port + 1).
        #[arg(long)]
        rpc_port: Option<u16>,
        /// JSON-RPC listen host/IP.
        #[arg(long)]
        rpc_host: Option<String>,
        /// Shared dev faucet secret (32-byte hex). All validators must use
        /// the SAME value (or none) or genesis — and hence state roots —
        /// will diverge. No faucet is auto-created in validator mode.
        #[arg(long)]
        faucet_key: Option<String>,
        /// TOML config file (see ValidatorFileConfig docs). CLI flags
        /// override file values; required fields missing from both fail.
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Log verbosity (tracing).
        #[arg(long, value_enum)]
        log_level: Option<LogLevel>,
    },
    /// Checkpoint-fork a live chain: snapshot listed accounts (balance,
    /// code, chosen storage slots) at one remote block into genesis, then
    /// run fully local and deterministic. NOT a live fork — unlisted
    /// storage starts empty (see fork.rs).
    Fork {
        /// Source JSON-RPC URL (any Ethereum RPC). Required on fresh fork,
        /// optional on reopen (state replays from disk).
        #[arg(long)]
        rpc_url: Option<String>,
        /// Remote block: `latest` or a `0x` block number.
        #[arg(long, default_value = "latest")]
        block: String,
        /// Account to import, repeatable: `--account 0xAddr`.
        #[arg(long)]
        account: Vec<String>,
        /// Storage slot to import, repeatable: `--slot 0xAddr:0xSlot`.
        #[arg(long)]
        slot: Vec<String>,
        /// Data directory for chain state (defaults to ~/.kanari/evm-fork).
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
        /// JSON-RPC listen port.
        #[arg(long, default_value = "8545")]
        rpc_port: u16,
        /// JSON-RPC listen host/IP.
        #[arg(long, default_value = "127.0.0.1")]
        rpc_host: String,
        /// Pin the dev faucet account to your own 32-byte hex secret.
        /// (Fork genesis test-mints it 1M ETH like Anvil defaults, since a
        /// fork has no dev account to fund from.)
        #[arg(long)]
        faucet_key: Option<String>,
        /// Log verbosity (tracing).
        #[arg(long, value_enum, default_value = "info")]
        log_level: LogLevel,
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
    log_level: LogLevel,
    /// Fork imports: chain id override + code/storage genesis extras.
    /// Empty/None on fresh chains.
    chain_id_override: Option<u64>,
    genesis_extra: ForkGenesisExtra,
}

/// Fork-imported genesis pieces beyond the faucet-style alloc list.
#[derive(Default)]
struct ForkGenesisExtra {
    /// Exact-wei allocations (no whole-ETH rounding).
    alloc: Vec<(Address, U256)>,
    code: Vec<(Address, alloy_primitives::Bytes)>,
    storage: Vec<(Address, U256, U256)>,
}

/// Test-minted faucet funding for fork genesis (Anvil-defaults style: a
/// fork has no dev account to fund the faucet from).
const FORK_FAUCET_ETH: u128 = 1_000_000;

fn parse_secret(hex: &str) -> B256 {
    hex.trim()
        .parse()
        .unwrap_or_else(|_| fatal("invalid --faucet-key: want 32-byte 0x hex"))
}

async fn run(opts: StartOptions) {
    init_logging(opts.log_level.as_str()).unwrap_or_else(|e| fatal(&e));
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
            let (addr, secret) = generate_faucet_key();
            println!("faucet account (dev only): {addr}");
            Some((addr, secret))
        }
        None => None,
    };
    // Genesis holds the FULL supply at the dev account only. The faucet gets
    // NO genesis allocation: fund it with a plain transfer from the dev
    // account before dripping (dev key: Anvil default #0).
    let mut spec = if opts.faucets.is_empty() {
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
        }
        KanariChainSpec::with_alloc(KANARI_EVM_DEV_CHAIN_ID, KANARI_EVM_GENESIS_SPEC, alloc)
    };
    // Fork imports ride along: remote chain id + deployed code + storage.
    // Fresh-state only (reopens replay the stored chain as usual).
    if fresh {
        if let Some(chain_id) = opts.chain_id_override {
            spec.chain_id = chain_id;
        }
        spec.genesis_alloc.extend(opts.genesis_extra.alloc);
        spec.genesis_code = opts.genesis_extra.code;
        spec.genesis_storage = opts.genesis_extra.storage;
    }

    let mut node = KanariNode::open(spec, &state_file)
        .unwrap_or_else(|e| fatal(&format!("failed to open node: {e}")));
    if let Some((addr, secret)) = faucet_secret {
        node.set_faucet_key(secret)
            .unwrap_or_else(|e| fatal(&format!("failed to store faucet key: {e}")));
        let funded = node
            .balance_of(addr)
            .map(|b| b >= U256::from(MAX_FAUCET_ETH_PER_REQUEST * WEI_IN_ETH))
            .unwrap_or(false);
        if funded {
            println!("faucet enabled for {addr}");
        } else {
            println!(
                "faucet enabled for {addr} with ZERO balance — fund it first with a transfer from the dev account {DEV_FUNDED_ACCOUNT}"
            );
        }
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

    let shared = Arc::new(Mutex::new(node));
    serve_rpc(shared, &opts.rpc_host, opts.rpc_port).await;
    tracing::info!("node stopped cleanly");
}

/// Bind, serve the JSON-RPC router, and drain gracefully on Ctrl+C/SIGTERM.
/// Shared by single-node and validator modes.
async fn serve_rpc(shared: SharedNode, rpc_host: &str, rpc_port: u16) {
    let app = rpc::router(shared);
    let bind: std::net::IpAddr = rpc_host
        .parse()
        .unwrap_or_else(|_| fatal("--rpc-host needs an IP address"));
    let addr = SocketAddr::new(bind, rpc_port);
    println!("JSON-RPC listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| fatal(&format!("failed to bind {addr}: {e}")));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| fatal(&format!("server error: {e}")));
}

fn default_fork_dir() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        std::path::PathBuf::from(home)
            .join(".kanari")
            .join("evm-fork")
    } else {
        std::path::PathBuf::from("./.kanari-evm-fork")
    }
}

struct ForkOptions {
    rpc_url: Option<String>,
    block: String,
    accounts: Vec<Address>,
    slots: Vec<fork::SlotRef>,
    data_dir: std::path::PathBuf,
    state_file: std::path::PathBuf,
    rpc_port: u16,
    rpc_host: String,
    faucet_key: Option<B256>,
    log_level: LogLevel,
}

async fn run_fork(opts: ForkOptions) {
    init_logging(opts.log_level.as_str()).unwrap_or_else(|e| fatal(&e));
    let fresh = !opts.state_file.exists();

    // Reopen: chain id comes from the store (no fetch needed, flags optional).
    // Fresh: fetch everything from the remote endpoint.
    let (chain_id, mut alloc, code, storage) = if fresh {
        let rpc_url = opts
            .rpc_url
            .clone()
            .unwrap_or_else(|| fatal("fresh fork needs --rpc-url (reopen skips fetching)"));
        println!("fetching fork state from {rpc_url} @ {} ...", opts.block);
        let client = reqwest::Client::new();
        let forked =
            fork::fetch_fork_genesis(&client, &rpc_url, &opts.block, &opts.accounts, &opts.slots)
                .await
                .unwrap_or_else(|e| fatal(&format!("fork fetch failed: {e}")));
        println!(
            "imported {} accounts ({} with code) + {} slots, chain id {}",
            forked.alloc.len(),
            forked.code.len(),
            forked.storage.len(),
            forked.chain_id
        );
        (forked.chain_id, forked.alloc, forked.code, forked.storage)
    } else {
        let store = kanari_evm_storage::ChainStore::open(kanari_evm_storage::chain_db_dir(
            &opts.state_file,
        ))
        .unwrap_or_else(|e| fatal(&format!("failed to open store: {e}")));
        let chain_id = store
            .stored_chain_id()
            .unwrap_or_else(|e| fatal(&format!("failed to read store: {e}")))
            .unwrap_or_else(|| fatal("chain database missing chain id"));
        println!("reopening fork chain {chain_id} (no fetching)");
        (chain_id, Vec::new(), Vec::new(), Vec::new())
    };

    // Faucet: shared key or fresh. On fresh forks it is TEST-MINTED 1M ETH
    // (Anvil-defaults style) since there is no dev account to fund from.
    let faucet_secret: B256 = match opts.faucet_key {
        Some(secret) => {
            PrivateKeySigner::from_bytes(&secret)
                .unwrap_or_else(|_| fatal("invalid --faucet-key secret"));
            eprintln!("kanari-evm-node: WARNING: faucet key from command line (dev only!)");
            secret
        }
        None => generate_faucet_key().1,
    };
    let faucet_addr = PrivateKeySigner::from_bytes(&faucet_secret)
        .unwrap_or_else(|e| fatal(&format!("bad faucet key: {e}")))
        .address();
    if fresh {
        alloc.push((
            faucet_addr,
            U256::from(FORK_FAUCET_ETH.saturating_mul(WEI_IN_ETH)),
        ));
    }
    println!("fork faucet account (dev only): {faucet_addr}");

    run(StartOptions {
        network: NetworkMode::Devnet,
        rpc_port: opts.rpc_port,
        rpc_host: opts.rpc_host,
        data_dir: opts.data_dir,
        state_file: opts.state_file,
        faucets: vec![(faucet_addr, FORK_FAUCET_ETH)],
        faucet_key: Some(faucet_secret),
        log_level: LogLevel::Info,
        chain_id_override: Some(chain_id),
        genesis_extra: ForkGenesisExtra {
            alloc,
            code,
            storage,
        },
    })
    .await;
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

fn cmd_keygen(node_count: usize, output_dir: std::path::PathBuf, host: &str, base_dag_port: u16) {
    use kanari_evm_consensus::generate_committee;
    let host: std::net::IpAddr = host
        .parse()
        .unwrap_or_else(|_| fatal("--host needs an IP address"));
    let committee = generate_committee(node_count, host, base_dag_port, &output_dir)
        .unwrap_or_else(|e| {
            fatal(&format!("keygen failed: {e}"));
        });
    println!(
        "committee for {} validators written to {}",
        committee.len(),
        output_dir.display()
    );
    for entry in &committee.authorities {
        println!(
            "  {}  {}  {}",
            entry.id, entry.dag_address, entry.public_key
        );
    }
    println!(
        "secret keys: validator-{{1..{}}}.key (DO NOT SHARE)",
        committee.len()
    );
}

async fn run_validator(
    committee: std::path::PathBuf,
    key: std::path::PathBuf,
    data_dir: Option<std::path::PathBuf>,
    rpc_port: Option<u16>,
    rpc_host: String,
    faucet_key: Option<B256>,
) {
    use kanari_evm_consensus::DEFAULT_ROUND_TIMEOUT;
    use kanari_evm_consensus::committee::load_validator;

    let identity = load_validator(&committee, &key)
        .unwrap_or_else(|e| fatal(&format!("bad committee/key: {e}")));
    let number = identity.index + 1;
    let data_dir = data_dir
        .unwrap_or_else(|| std::path::PathBuf::from(format!("./.kanari-evm-validator-{number}")));
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        fatal(&format!(
            "cannot create data dir {}: {e}",
            data_dir.display()
        ));
    }
    let rpc_port = rpc_port.unwrap_or_else(|| identity.own_address.port() + 1);

    // Genesis MUST be identical on all validators or state roots diverge:
    // the FULL supply sits at the dev account. A shared faucet key only
    // installs the dripping key — fund that account with a transfer from
    // the dev account before dripping (same key everywhere).
    let spec = KanariChainSpec::devnet();
    let state_file = data_dir.join("state.json");
    let mut node = KanariNode::open(spec, &state_file)
        .unwrap_or_else(|e| fatal(&format!("failed to open node: {e}")));
    if let Some(secret) = faucet_key {
        node.set_faucet_key(secret)
            .unwrap_or_else(|e| fatal(&format!("failed to store faucet key: {e}")));
        let signer = PrivateKeySigner::from_bytes(&secret)
            .unwrap_or_else(|_| fatal("invalid --faucet-key secret"));
        let addr = signer.address();
        let funded = node.balance_of(addr).map(|b| !b.is_zero()).unwrap_or(false);
        if funded {
            println!("shared faucet enabled for {addr} (same key on all validators)");
        } else {
            println!(
                "shared faucet key installed for {addr} with ZERO balance — fund it with a transfer from the dev account {DEV_FUNDED_ACCOUNT} first"
            );
        }
    } else {
        println!("faucet disabled (validator mode never auto-creates one)");
    }

    let shared = Arc::new(Mutex::new(node));
    let validator = ValidatorNode::spawn(
        shared.clone(),
        ValidatorOpts {
            committee_path: committee,
            key_path: key,
            dag_wal_dir: Some(data_dir.join("dag-wal")),
            round_timeout: DEFAULT_ROUND_TIMEOUT,
        },
    )
    .await
    .unwrap_or_else(|e| fatal(&format!("failed to join DAG mesh: {e}")));

    println!("========================================");
    println!(
        "Kanari EVM Validator {} ({})",
        identity.id, identity.own_address
    );
    println!("========================================");
    println!("RPC URL:   http://{rpc_host}:{rpc_port}");
    println!("Chain ID:  {KANARI_EVM_DEV_CHAIN_ID}");
    println!("Data Dir:  {}", data_dir.display());
    println!("========================================");
    println!();

    serve_rpc(shared, &rpc_host, rpc_port).await;
    validator.shutdown().await;
    tracing::info!("validator stopped cleanly");
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
            log_level,
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
                log_level,
                chain_id_override: None,
                genesis_extra: ForkGenesisExtra::default(),
            })
            .await;
        }
        Commands::Local {
            rpc_port,
            log_level,
        } => {
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
                log_level,
                chain_id_override: None,
                genesis_extra: ForkGenesisExtra::default(),
            })
            .await;
        }
        Commands::Reset { data_dir, force } => cmd_reset(data_dir, force),
        Commands::Keygen {
            node_count,
            output_dir,
            host,
            base_dag_port,
        } => cmd_keygen(node_count, output_dir, &host, base_dag_port),
        Commands::Fork {
            rpc_url,
            block,
            account,
            slot,
            data_dir,
            rpc_port,
            rpc_host,
            faucet_key,
            log_level,
        } => {
            let accounts = account
                .iter()
                .map(|a| {
                    Address::from_str(a.trim())
                        .unwrap_or_else(|_| fatal(&format!("invalid --account address: {a}")))
                })
                .collect::<Vec<_>>();
            let slots = slot
                .iter()
                .map(|s| fork::parse_slot(s).unwrap_or_else(|e| fatal(&e)))
                .collect::<Vec<_>>();
            let data_dir = data_dir.unwrap_or_else(default_fork_dir);
            let state_file = data_dir.join("state.json");
            run_fork(ForkOptions {
                rpc_url,
                block,
                accounts,
                slots,
                data_dir,
                state_file,
                rpc_port,
                rpc_host,
                faucet_key: faucet_key.as_deref().map(parse_secret),
                log_level,
            })
            .await;
        }
        Commands::Validator {
            committee,
            key,
            data_dir,
            rpc_port,
            rpc_host,
            faucet_key,
            config,
            log_level,
        } => {
            // Precedence: CLI flag > TOML file > built-in default.
            let file = config
                .as_deref()
                .map(load_validator_file)
                .unwrap_or_default();
            let resolved = resolve_validator_config(
                ValidatorCli {
                    committee,
                    key,
                    data_dir,
                    rpc_port,
                    rpc_host,
                    faucet_key,
                    log_level,
                },
                file,
            )
            .unwrap_or_else(|e| fatal(&e));
            init_logging(&resolved.log_level).unwrap_or_else(|e| fatal(&e));
            let secret = resolved.faucet_key.as_deref().map(parse_secret);
            run_validator(
                resolved.committee,
                resolved.key,
                resolved.data_dir,
                resolved.rpc_port,
                resolved.rpc_host,
                secret,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_cli() -> ValidatorCli {
        ValidatorCli {
            committee: None,
            key: None,
            data_dir: None,
            rpc_port: None,
            rpc_host: None,
            faucet_key: None,
            log_level: None,
        }
    }

    #[test]
    fn config_file_fills_gaps_and_cli_wins() {
        let file: ValidatorFileConfig = toml::from_str(
            r#"
            committee = "./dag-keys/dag-committee.json"
            key = "./dag-keys/validator-1.key"
            data_dir = "./data/node1"
            rpc_host = "0.0.0.0"
            rpc_port = 3501
            log_level = "debug"
            "#,
        )
        .expect("sample parses");
        // File alone resolves (except optionals it omits).
        let resolved = resolve_validator_config(test_cli(), file.clone()).expect("resolves");
        assert_eq!(
            resolved.committee,
            PathBuf::from("./dag-keys/dag-committee.json")
        );
        assert_eq!(resolved.rpc_host, "0.0.0.0");
        assert_eq!(resolved.rpc_port, Some(3501));
        assert_eq!(resolved.log_level, "debug");
        assert!(resolved.faucet_key.is_none());

        // CLI flags override every file value.
        let cli = ValidatorCli {
            rpc_host: Some("127.0.0.1".to_string()),
            rpc_port: Some(9999),
            log_level: Some(LogLevel::Warn),
            faucet_key: Some("0xabc".to_string()),
            ..test_cli()
        };
        let resolved = resolve_validator_config(cli, file).expect("resolves");
        assert_eq!(resolved.rpc_host, "127.0.0.1");
        assert_eq!(resolved.rpc_port, Some(9999));
        assert_eq!(resolved.log_level, "warn");
        assert_eq!(resolved.faucet_key.as_deref(), Some("0xabc"));
    }

    #[test]
    fn config_defaults_and_missing_required() {
        // Empty file: defaults apply, required fields fail loudly.
        let err = resolve_validator_config(test_cli(), ValidatorFileConfig::default())
            .expect_err("missing committee must fail");
        assert!(err.contains("--committee"), "unexpected: {err}");

        let file = ValidatorFileConfig {
            committee: Some(PathBuf::from("c.json")),
            key: Some(PathBuf::from("k.key")),
            ..Default::default()
        };
        let resolved = resolve_validator_config(test_cli(), file).expect("resolves");
        assert_eq!(resolved.rpc_host, "127.0.0.1");
        assert!(
            resolved.rpc_port.is_none(),
            "rpc port defaults to DAG+1 later"
        );
        assert_eq!(resolved.log_level, "info");
    }
}
