mod model;
mod monitor;
mod network;
mod runner;
mod store;
mod web;

use crate::model::{current_ts, Envelope, Seed, SeedStatus, TaskResult};
use crate::monitor::{EventSource, MonitorHandle};
use crate::network::{BroadcastListener, MeshClient, NetworkBackend};
use crate::runner::run_bash_task;
use crate::store::Store;
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "crabnet")]
#[command(about = "Rust MVP for p2p shell-task network", long_about = None)]
struct Cli {
    /// Data directory for local state
    #[arg(long, default_value = ".crabnet")]
    data_dir: PathBuf,
    /// Announce UDP target (ip:port), supports unicast for e2e tests.
    /// Comma-separated list is supported, e.g. `127.0.0.1:9012,127.0.0.1:9013`.
    #[arg(long, default_value = "255.255.255.255:9012")]
    announce_addr: String,
    /// Listen UDP bind address (ip:port)
    #[arg(long, default_value = "0.0.0.0:9012")]
    listen_addr: String,
    /// Seed bootstrap peers used in dht mode (repeat or comma-separated list)
    #[arg(long, value_delimiter = ',')]
    bootstrap_peers: Vec<String>,
    /// Network backend for transport (udp keeps MVP, dht planned)
    #[arg(long, default_value = "udp")]
    network: NetworkBackend,
    /// HTTP monitor interface address
    #[arg(long, default_value = "127.0.0.1:3000")]
    web_addr: String,
    /// Output NDJSON event log file
    #[arg(long)]
    monitor_events: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show node identity and local counts
    Status,
    /// Start receive loop and print remote events
    Listen,
    /// Seed operations
    Seed(SeedCmds),
}

#[derive(Args)]
struct SeedCmds {
    #[command(subcommand)]
    cmd: SeedCommand,
}

#[derive(Subcommand)]
enum SeedCommand {
    /// Create task seed (publisher)
    Publish {
        #[arg(long)]
        title: String,
        #[arg(long)]
        cmd: String,
        #[arg(long, default_value = "")]
        desc: String,
        #[arg(long, default_value_t = 120000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 60000)]
        bid_window_ms: u64,
        #[arg(long, default_value_t = 0)]
        min_price: u64,
        #[arg(long, default_value_t = 1)]
        max_bids: usize,
        #[arg(long, default_value_t = 0)]
        reward: u64,
        #[arg(long, default_value = "")]
        rules: String,
        #[arg(long, default_value_t = false)]
        announce: bool,
    },
    /// List all local seeds
    List,
    /// Bid for a seed
    Bid {
        seed_id: String,
        #[arg(long, default_value_t = 0)]
        price: u64,
        #[arg(long, default_value = "")]
        notes: String,
        #[arg(long, default_value_t = false)]
        announce: bool,
    },
    /// Claim a bid for seed
    Claim {
        seed_id: String,
        bid_id: String,
        #[arg(long, default_value_t = false)]
        announce: bool,
    },
    /// Run local bash task for claimed seed
    Run { seed_id: String },
    /// Settle seed result
    Settle {
        seed_id: String,
        #[arg(long, default_value_t = true)]
        accepted: bool,
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long, default_value_t = false)]
        announce: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.clone();
    let mut store = Store::new(data_dir.clone());
    let has_persisted_state = store.load().await?;
    if !has_persisted_state {
        if let Err(err) = store.save().await {
            eprintln!("persist initial state failed: {err}");
        }
    }
    let monitor_path = match &cli.monitor_events {
        Some(path) => {
            if path.is_relative() {
                data_dir.join(path)
            } else {
                path.clone()
            }
        }
        None => data_dir.join("events.ndjson"),
    };
    let monitor = MonitorHandle::with_path(Some(monitor_path.clone()), store.node_id().to_string());
    monitor
        .emit(
            "node_start",
            EventSource::Node,
            serde_json::json!({ "network": format!("{:?}", cli.network) }),
        )
        .await;
    let mesh = {
        let backend = cli.network;
        MeshClient::new(
            store.node_id().to_string(),
            cli.announce_addr.clone(),
            cli.listen_addr.clone(),
            cli.bootstrap_peers.clone(),
            backend,
            monitor.clone(),
        )
        .await?
    };

    match cli.command {
        Commands::Status => {
            print_status(&store);
        }
        Commands::Listen => {
            let mut listener = BroadcastListener::new(
                cli.listen_addr.clone(),
                store.node_id().to_string(),
                cli.network,
                cli.bootstrap_peers.clone(),
                cli.announce_addr.clone(),
                monitor.clone(),
            )
            .await?;
            println!(
                "listen started on {:?}:{}",
                cli.network,
                listener.bind_addr()
            );
            let mut hello = Envelope::node_hello(store.node_id(), store.self_identity_info());
            if let Err(err) = store.sign_envelope(&mut hello) {
                monitor
                    .emit(
                        "node_hello_send_failed",
                        EventSource::Node,
                        serde_json::json!({ "reason": err.to_string() }),
                    )
                    .await;
            } else {
                for attempt in 1..=3 {
                    if let Err(err) = mesh.broadcast(hello.clone()).await {
                        monitor
                            .emit(
                                "node_hello_send_failed",
                                EventSource::Node,
                                serde_json::json!({
                                    "reason": err.to_string(),
                                    "attempt": attempt,
                                }),
                            )
                            .await;
                    }
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            let web_state = web::WebState {
                monitor_path: monitor_path.clone(),
            };
            let web_addr = cli.web_addr.clone();
            let _web_handle: JoinHandle<()> = tokio::spawn(async move {
                if let Err(err) = crate::web::run(&web_addr, web_state).await {
                    eprintln!("web server failed: {err}");
                }
            });
            loop {
                if let Some(msg) = listener.next_message().await? {
                    let msg_kind = format!("{:?}", msg.meta.kind);
                    let msg_id = msg.meta.id.clone();
                    let msg_from = msg.meta.from.clone();
                    let _ = monitor
                        .emit(
                            "listener_message_received",
                            EventSource::Node,
                            serde_json::json!({
                                "kind": msg_kind,
                                "message_id": msg_id,
                                "from": msg_from,
                            }),
                        )
                        .await;
                    match store.apply_remote_with_reason(msg.clone()).await {
                        Ok((changed, reason)) => {
                            if changed {
                                store.save().await?;
                                let _ = monitor
                                    .emit(
                                        "store_sync",
                                        EventSource::Node,
                                        serde_json::json!({
                                            "kind": msg_kind,
                                            "message_id": msg_id,
                                            "from": msg_from,
                                        }),
                                    )
                                    .await;
                                println!("> synced:{}", store.last_event_desc());
                                if cli.network == NetworkBackend::Dht && msg.crypto.is_empty() {
                                    mesh.broadcast(msg).await?;
                                }
                            }
                            if !changed {
                                if !msg_from.eq(store.node_id())
                                    || !matches!(
                                        reason,
                                        Some(store::ApplyRemoteNoopReason::NotAddressedToThisNode)
                                    )
                                {
                                    let _ = monitor
                                        .emit(
                                            "store_sync_noop",
                                            EventSource::Node,
                                            serde_json::json!({
                                                "kind": msg_kind,
                                                "message_id": msg_id,
                                                "from": msg_from,
                                                "reason": reason
                                                    .map(|reason| reason.as_str())
                                                    .unwrap_or("unknown"),
                                            }),
                                        )
                                        .await;
                                }
                            }
                        }
                        Err(err) => {
                            let _ = monitor
                                .emit(
                                    "store_sync_rejected",
                                    EventSource::Node,
                                    serde_json::json!({
                                        "kind": format!("{:?}", msg.meta.kind),
                                        "seed_id": msg.meta.id,
                                        "from": msg.meta.from,
                                        "error": err.to_string(),
                                    }),
                                )
                                .await;
                        }
                    }
                }
            }
        }
        Commands::Seed(seed_cmd) => match seed_cmd.cmd {
            SeedCommand::Publish {
                title,
                cmd,
                desc,
                timeout_ms,
                bid_window_ms,
                min_price,
                max_bids,
                reward,
                rules,
                announce,
            } => {
                let seed = Seed {
                    id: Uuid::new_v4().to_string(),
                    title,
                    description: desc,
                    command: cmd,
                    timeout_ms,
                    bid_deadline_ts: current_ts() + (bid_window_ms / 1000),
                    exec_deadline_ms: timeout_ms,
                    min_price,
                    max_bids,
                    reward,
                    rules,
                    status: SeedStatus::Open,
                    created_by: store.node_id().to_string(),
                    created_at: current_ts(),
                    claimed_by: None,
                    claimed_at: None,
                    claim_id: None,
                    result_id: None,
                };
                store.add_seed(seed.clone());
                monitor
                    .emit(
                        "seed_published",
                        EventSource::Node,
                        serde_json::json!({
                            "seed_id": seed.id,
                            "title": seed.title,
                            "timeout_ms": timeout_ms,
                            "min_price": min_price,
                            "max_bids": max_bids,
                        }),
                    )
                    .await;
                let mut msg = Envelope::seed_created(store.node_id(), seed.clone());
                if announce {
                    if cli.network != NetworkBackend::Dht {
                        msg = store.encrypt_for_peers(msg)?;
                    }
                    store.sign_envelope(&mut msg)?;
                    mesh.broadcast(msg).await?;
                }
                println!("seed published {}", seed.id);
            }
            SeedCommand::List => {
                for seed in store.seeds_sorted() {
                    println!(
                        "{} [{}] cmd={} timeout={}ms reward={}",
                        seed.id, seed.status, seed.command, seed.timeout_ms, seed.reward
                    );
                }
            }
            SeedCommand::Bid {
                seed_id,
                price,
                notes,
                announce,
            } => {
                let bid = store.add_bid(&seed_id, price, notes)?;
                monitor
                    .emit(
                        "bid_submitted",
                        EventSource::Node,
                        serde_json::json!({
                            "seed_id": seed_id,
                            "bid_id": bid.id,
                            "price": bid.price,
                        }),
                    )
                    .await;
                println!("bid submitted {}", bid.id);
                if announce {
                    let mut msg = Envelope::bid_submitted(store.node_id(), bid);
                    if cli.network != NetworkBackend::Dht {
                        msg = store.encrypt_for_peers(msg)?;
                    }
                    store.sign_envelope(&mut msg)?;
                    mesh.broadcast(msg).await?;
                }
            }
            SeedCommand::Claim {
                seed_id,
                bid_id,
                announce,
            } => {
                let claim = store.claim_seed(&seed_id, &bid_id)?;
                monitor
                    .emit(
                        "claim_created",
                        EventSource::Node,
                        serde_json::json!({
                            "seed_id": seed_id,
                            "claim_id": claim.id,
                        }),
                    )
                    .await;
                println!("claim created {}", claim.id);
                if announce {
                    let mut msg = Envelope::claim_created(store.node_id(), claim);
                    if cli.network != NetworkBackend::Dht {
                        msg = store.encrypt_for_peers(msg)?;
                    }
                    store.sign_envelope(&mut msg)?;
                    mesh.broadcast(msg).await?;
                }
            }
            SeedCommand::Run { seed_id } => {
                let (command, timeout_ms) = {
                    let seed = store
                        .seed(&seed_id)
                        .ok_or_else(|| anyhow::anyhow!("seed not found"))?;
                    (seed.command.clone(), seed.timeout_ms)
                };
                store.mark_running(&seed_id)?;
                let result: TaskResult = run_bash_task(&command, timeout_ms).await?;
                let stored = store.add_result(&seed_id, result.clone())?;
                monitor
                    .emit(
                        "task_result_generated",
                        EventSource::Node,
                        serde_json::json!({
                            "seed_id": seed_id,
                            "result_id": stored.id,
                            "duration_ms": stored.duration_ms,
                            "exit_code": stored.exit_code,
                        }),
                    )
                    .await;
                println!("task result {} hash={}", stored.id, stored.output_hash);
                let mut msg = Envelope::task_result(store.node_id(), result);
                if cli.network != NetworkBackend::Dht {
                    msg = store.encrypt_for_peers(msg)?;
                }
                store.sign_envelope(&mut msg)?;
                mesh.broadcast(msg).await?;
            }
            SeedCommand::Settle {
                seed_id,
                accepted,
                note,
                announce,
            } => {
                let settled = store.settle_seed(&seed_id, accepted, note)?;
                monitor
                    .emit(
                        "seed_settled",
                        EventSource::Node,
                        serde_json::json!({
                            "seed_id": seed_id,
                            "accepted": accepted,
                            "status": format!("{:?}", settled.status),
                        }),
                    )
                    .await;
                println!("seed {} settled as {:?}", settled.id, settled.status);
                if announce {
                    let mut msg = Envelope::task_settle(store.node_id(), settled.clone());
                    if cli.network != NetworkBackend::Dht {
                        msg = store.encrypt_for_peers(msg)?;
                    }
                    store.sign_envelope(&mut msg)?;
                    mesh.broadcast(msg).await?;
                }
            }
        },
    }

    store.save().await?;
    Ok(())
}

fn print_status(store: &Store) {
    println!("node-id: {}", store.node_id());
    println!("seeds: {}", store.seeds_len());
    println!("bids: {}", store.bids_len());
    println!("claims: {}", store.claims_len());
    println!("results: {}", store.results_len());
}
