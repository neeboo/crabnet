mod model;
mod monitor;
mod network;
mod runner;
mod store;
mod web;

use crate::model::{current_ts, Envelope, Seed, SeedStatus, TaskResult};
use crate::monitor::{EventSource, MonitorEvent, MonitorHandle};
use crate::network::{BroadcastListener, MeshClient, NetworkBackend};
use crate::runner::run_bash_task;
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use libp2p::Multiaddr;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
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

    /// Validate network/listen/data-dir/monitor configuration and exit
    #[arg(long, default_value_t = false)]
    verify_config: bool,
    /// Dump topology summary from the monitor events file and exit
    #[arg(long, default_value_t = false)]
    dump_topology: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Clone)]
enum Commands {
    /// Show node identity and local counts
    Status,
    /// Start receive loop and print remote events
    Listen,
    /// Seed operations
    Seed(SeedCmds),
}

#[derive(Args, Clone)]
struct SeedCmds {
    #[command(subcommand)]
    cmd: SeedCommand,
}

#[derive(Subcommand, Clone)]
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

#[derive(Debug, Serialize, Default)]
struct TopologyDump {
    monitor_path: String,
    events: usize,
    nodes: usize,
    edges: usize,
    node_list: Vec<TopologyNodeDump>,
    edge_list: Vec<TopologyEdgeDump>,
}

#[derive(Debug, Serialize)]
struct TopologyNodeDump {
    id: String,
    last_seen: u64,
    degree: usize,
    sources: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TopologyEdgeDump {
    from: String,
    to: String,
    kind: String,
    total: usize,
    last_seen: u64,
}

#[derive(Debug, Default)]
struct TopologyNodeAgg {
    last_seen: u64,
    degree: usize,
    sources: HashSet<String>,
}

#[derive(Debug, Default)]
struct TopologyEdgeAgg {
    total: usize,
    last_seen: u64,
}

fn resolve_monitor_path(data_dir: &Path, monitor_events: Option<&PathBuf>) -> PathBuf {
    match monitor_events {
        Some(path) if path.is_relative() => data_dir.join(path),
        Some(path) => path.clone(),
        None => data_dir.join("events.ndjson"),
    }
}

fn parse_network_endpoint(raw: &str) -> Result<()> {
    if raw.contains('/') {
        Multiaddr::from_str(raw).with_context(|| format!("invalid multiaddr: {raw}"))?;
        Ok(())
    } else {
        raw.parse::<SocketAddr>()
            .with_context(|| format!("invalid socket address: {raw}"))?;
        Ok(())
    }
}

fn parse_udp_targets(raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        part.parse::<SocketAddr>()
            .with_context(|| format!("invalid announce target: {part}"))?;
        out.push(part.to_string());
    }
    if out.is_empty() {
        return Err(anyhow!("announce address list is empty"));
    }
    Ok(out)
}

async fn ensure_writable_dir(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("create data dir {}", path.display()))?;
    let probe = path.join(format!(".verify-write-{}", Uuid::new_v4()));
    tokio::fs::write(&probe, b"ok")
        .await
        .with_context(|| format!("write probe in {}", path.display()))?;
    tokio::fs::remove_file(&probe)
        .await
        .with_context(|| format!("remove probe {}", probe.display()))?;
    Ok(())
}

async fn ensure_writable_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create monitor parent {}", parent.display()))?;
    }
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open monitor path {}", path.display()))?;
    Ok(())
}

async fn run_verify_config(cli: &Cli, monitor_path: &Path) -> Result<()> {
    let mut checks: Vec<(&str, Result<()>)> = Vec::new();
    let announce_required =
        cli.network != NetworkBackend::Dht || !cli.announce_addr.trim().is_empty();
    if announce_required {
        checks.push((
            "announce_addr",
            parse_udp_targets(&cli.announce_addr).map(|_| ()),
        ));
    }
    checks.push(("listen_addr", parse_network_endpoint(&cli.listen_addr)));
    checks.push(("web_addr", parse_network_endpoint(&cli.web_addr)));

    if cli.network == NetworkBackend::Dht {
        for peer in &cli.bootstrap_peers {
            for item in peer.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                checks.push((
                    "bootstrap_peer",
                    parse_network_endpoint(item).with_context(|| format!("peer={item}")),
                ));
            }
        }
    }

    let mut failed = Vec::new();
    for (name, result) in checks {
        if let Err(err) = result {
            failed.push(format!("{name}: {err}"));
        }
    }

    if let Err(err) = ensure_writable_dir(&cli.data_dir).await {
        failed.push(format!("data_dir: {err}"));
    }
    if let Err(err) = ensure_writable_file(monitor_path).await {
        failed.push(format!("monitor_events: {err}"));
    }

    if failed.is_empty() {
        println!("verify-config: ok");
        println!("network: {:?}", cli.network);
        println!("listen_addr: {}", cli.listen_addr);
        println!(
            "announce_targets: {}",
            if announce_required {
                parse_udp_targets(&cli.announce_addr)?.len()
            } else {
                0
            }
        );
        println!("data_dir: {}", cli.data_dir.display());
        println!("monitor_path: {}", monitor_path.display());
        return Ok(());
    }

    eprintln!("verify-config: failed");
    for err in failed {
        eprintln!("- {err}");
    }
    Err(anyhow!("verify-config failed"))
}

fn relation_from_event(event: &MonitorEvent) -> Option<(String, String, String)> {
    if event.source != EventSource::Network {
        return None;
    }
    let payload = event.payload.as_object()?;
    let peer = payload
        .get("peer")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let target = payload
        .get("target")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);

    let (from, to) = match event.kind.as_str() {
        "dht_message_to" | "dht_fallback_send" | "udp_broadcast" => (
            event.node_id.clone(),
            target.unwrap_or_else(|| event.node_id.clone()),
        ),
        "dht_connection_established" | "dht_connection_closed" | "dht_message_received" => {
            let peer = peer?;
            (event.node_id.clone(), peer)
        }
        _ => {
            let peer = peer?;
            (event.node_id.clone(), peer)
        }
    };
    Some((from, to, event.kind.clone()))
}

fn build_topology_dump(events: &[MonitorEvent], monitor_path: &Path) -> TopologyDump {
    let mut nodes = HashMap::<String, TopologyNodeAgg>::new();
    let mut edges = HashMap::<(String, String, String), TopologyEdgeAgg>::new();

    for event in events {
        let source = event.source.to_string();
        nodes
            .entry(event.node_id.clone())
            .and_modify(|node| {
                node.last_seen = node.last_seen.max(event.ts);
                node.sources.insert(source.clone());
            })
            .or_insert_with(|| TopologyNodeAgg {
                last_seen: event.ts,
                degree: 0,
                sources: HashSet::from([source]),
            });

        if let Some((from, to, kind)) = relation_from_event(event) {
            nodes
                .entry(from.clone())
                .and_modify(|node| node.last_seen = node.last_seen.max(event.ts))
                .or_insert_with(|| TopologyNodeAgg {
                    last_seen: event.ts,
                    degree: 0,
                    sources: HashSet::new(),
                });
            nodes
                .entry(to.clone())
                .and_modify(|node| node.last_seen = node.last_seen.max(event.ts))
                .or_insert_with(|| TopologyNodeAgg {
                    last_seen: event.ts,
                    degree: 0,
                    sources: HashSet::new(),
                });
            edges
                .entry((from, to, kind))
                .and_modify(|edge| {
                    edge.total = edge.total.saturating_add(1);
                    edge.last_seen = edge.last_seen.max(event.ts);
                })
                .or_insert(TopologyEdgeAgg {
                    total: 1,
                    last_seen: event.ts,
                });
        }
    }

    for ((from, to, _), edge) in &edges {
        if let Some(node) = nodes.get_mut(from) {
            node.degree = node.degree.saturating_add(edge.total);
        }
        if let Some(node) = nodes.get_mut(to) {
            node.degree = node.degree.saturating_add(edge.total);
        }
    }

    let mut node_list = nodes
        .into_iter()
        .map(|(id, mut node)| {
            let mut sources = node.sources.drain().collect::<Vec<_>>();
            sources.sort();
            TopologyNodeDump {
                id,
                last_seen: node.last_seen,
                degree: node.degree,
                sources,
            }
        })
        .collect::<Vec<_>>();
    node_list.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

    let mut edge_list = edges
        .into_iter()
        .map(|((from, to, kind), agg)| TopologyEdgeDump {
            from,
            to,
            kind,
            total: agg.total,
            last_seen: agg.last_seen,
        })
        .collect::<Vec<_>>();
    edge_list.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

    TopologyDump {
        monitor_path: monitor_path.display().to_string(),
        events: events.len(),
        nodes: node_list.len(),
        edges: edge_list.len(),
        node_list,
        edge_list,
    }
}

async fn read_monitor_events(path: &Path) -> Result<Vec<MonitorEvent>> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(anyhow!("read monitor file {}: {err}", path.display())),
    };
    let mut events = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<MonitorEvent>(line) {
            events.push(event);
        }
    }
    Ok(events)
}

async fn run_dump_topology(monitor_path: &Path) -> Result<()> {
    let events = read_monitor_events(monitor_path).await?;
    let dump = build_topology_dump(&events, monitor_path);
    println!("{}", serde_json::to_string_pretty(&dump)?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.clone();
    let monitor_path = resolve_monitor_path(&data_dir, cli.monitor_events.as_ref());

    if cli.verify_config {
        run_verify_config(&cli, &monitor_path).await?;
        if !cli.dump_topology {
            return Ok(());
        }
    }
    if cli.dump_topology {
        run_dump_topology(&monitor_path).await?;
        return Ok(());
    }
    if cli.command.is_none() {
        return Err(anyhow!(
            "missing command: use status|listen|seed, or --verify-config/--dump-topology"
        ));
    }

    let mut store = Store::new(data_dir.clone());
    let has_persisted_state = store.load().await?;
    let load_recovery = store.take_load_recovery();
    if !has_persisted_state {
        if let Err(err) = store.save().await {
            eprintln!("persist initial state failed: {err}");
        }
    }

    let monitor = MonitorHandle::with_path(Some(monitor_path.clone()), store.node_id().to_string());
    let ttl_seconds = store.message_ttl_seconds();
    eprintln!("effective message ttl: {ttl_seconds}s");
    monitor
        .emit(
            "config_effective",
            EventSource::Node,
            serde_json::json!({
                "msg_ttl_seconds": ttl_seconds,
            }),
        )
        .await;

    if let Some(recovery) = load_recovery {
        let details = match &recovery {
            store::StoreLoadRecoverySignal::RecoveredFromBackup { primary_error } => {
                serde_json::json!({
                    "type": recovery.as_str(),
                    "primary_error": primary_error,
                })
            }
            store::StoreLoadRecoverySignal::RepairedPrimaryWithoutChecksum { primary_error } => {
                serde_json::json!({
                    "type": recovery.as_str(),
                    "primary_error": primary_error,
                })
            }
        };
        monitor
            .emit(
                "store_load_recovery_fallback",
                EventSource::Node,
                details.clone(),
            )
            .await;
        eprintln!("store load recovery fallback detected: {details}");
    }

    monitor
        .emit(
            "node_start",
            EventSource::Node,
            serde_json::json!({
                "network": format!("{:?}", cli.network),
                "msg_ttl_seconds": ttl_seconds,
            }),
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

    let command = cli.command.clone().expect("checked command above");
    match command {
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
