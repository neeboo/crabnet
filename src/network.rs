use crate::model::Envelope;
use crate::monitor::{EventSource, MonitorHandle};
use anyhow::{anyhow, Result};
use clap::ValueEnum;
use futures::StreamExt;
use libp2p::{
    gossipsub::{
        self, Behaviour as Gossipsub, ConfigBuilder as GossipsubConfigBuilder,
        Event as GossipsubEvent, IdentTopic, MessageAuthenticity,
    },
    identity,
    mdns::{self, Config as MdnsConfig, Event as MdnsEvent},
    noise,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    tcp::tokio::Transport as TokioTcpTransport,
    yamux, Multiaddr, PeerId, Transport,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NetworkBackend {
    Udp,
    Dht,
}

impl Default for NetworkBackend {
    fn default() -> Self {
        Self::Udp
    }
}

enum TransportBackend {
    Udp(UdpTransport),
    Dht(DhtTransport),
}

pub struct MeshClient {
    transport: TransportBackend,
}

pub struct BroadcastListener {
    receiver: mpsc::Receiver<Envelope>,
    bind_addr: String,
}

struct UdpTransport {
    socket: UdpSocket,
    announce_addr: String,
    monitor: MonitorHandle,
    fragments: HashMap<String, UdpFragmentState>,
    outbound_rate_limiter: Option<BroadcastRateLimiter>,
    inbound_source_guard: Option<UdpSourceFastFailGuard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UdpFragment {
    envelope_id: String,
    total: u16,
    index: u16,
    payload: String,
}

#[derive(Debug)]
struct UdpFragmentState {
    total: usize,
    received: Vec<Option<Vec<u8>>>,
    last_seen: Instant,
}

enum DhtCommand {
    Broadcast {
        msg: Envelope,
        reply: oneshot::Sender<Result<()>>,
    },
}

struct DhtTransport {
    cmd_tx: mpsc::Sender<DhtCommand>,
    msg_rx: mpsc::Receiver<Envelope>,
    announce_addr: String,
    fallback_udp: Option<UdpSocket>,
    monitor: MonitorHandle,
    outbound_rate_limiter: Option<BroadcastRateLimiter>,
    _task: tokio::task::JoinHandle<()>,
}

#[derive(NetworkBehaviour)]
struct DhtBehaviour {
    gossipsub: Gossipsub,
    mdns: mdns::tokio::Behaviour,
}

const DHT_TOPIC: &str = "crabnet-mvp-v1";
const UDP_FRAGMENT_SIZE: usize = 512;
const UDP_MAX_DATAGRAM: usize = 1200;
const UDP_FRAGMENT_TTL_SECONDS: u64 = 30;
const UDP_FRAGMENT_SEND_ATTEMPTS: usize = 3;
const UDP_FRAGMENT_RESEND_DELAY_MS: u64 = 10;
const BROADCAST_WINDOW_MILLIS: u64 = 1_000;
const UDP_RATE_LIMIT_ENV: &str = "CRABNET_UDP_BROADCASTS_PER_SEC";
const DHT_RATE_LIMIT_ENV: &str = "CRABNET_DHT_BROADCASTS_PER_SEC";
const UDP_SOURCE_FAST_FAIL_THRESHOLD_ENV: &str = "CRABNET_UDP_SOURCE_FAST_FAIL_THRESHOLD_PER_SEC";
const UDP_SOURCE_BLOCK_MS_ENV: &str = "CRABNET_UDP_SOURCE_BLOCK_MS";
const UDP_SOURCE_TRACK_MAX: usize = 4_096;

#[derive(Clone, Copy, Debug)]
struct AbusePreventionConfig {
    udp_broadcasts_per_sec: usize,
    dht_broadcasts_per_sec: usize,
    udp_source_fast_fail_threshold_per_sec: usize,
    udp_source_block_ms: u64,
}

impl AbusePreventionConfig {
    fn from_env() -> Self {
        Self {
            // 0 means disabled for backwards compatibility.
            udp_broadcasts_per_sec: parse_env_usize(UDP_RATE_LIMIT_ENV, 0),
            // 0 means disabled for backwards compatibility.
            dht_broadcasts_per_sec: parse_env_usize(DHT_RATE_LIMIT_ENV, 0),
            // 0 means disabled for backwards compatibility.
            udp_source_fast_fail_threshold_per_sec: parse_env_usize(
                UDP_SOURCE_FAST_FAIL_THRESHOLD_ENV,
                0,
            ),
            // Keep block duration short by default in case users enable threshold.
            udp_source_block_ms: parse_env_u64(UDP_SOURCE_BLOCK_MS_ENV, 3_000),
        }
    }
}

#[derive(Debug)]
struct BroadcastRateLimiter {
    limit: usize,
    window: Duration,
    state: Mutex<BroadcastRateLimiterState>,
}

#[derive(Debug)]
struct BroadcastRateLimiterState {
    window_start: Instant,
    seen: usize,
}

#[derive(Debug, Clone, Copy)]
enum BroadcastRateDecision {
    Allow,
    Block { retry_after: Duration },
}

impl BroadcastRateLimiter {
    fn new(limit: usize, window: Duration) -> Option<Self> {
        if limit == 0 {
            return None;
        }

        Some(Self {
            limit,
            window,
            state: Mutex::new(BroadcastRateLimiterState {
                window_start: Instant::now(),
                seen: 0,
            }),
        })
    }

    fn check(&self, now: Instant) -> BroadcastRateDecision {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.duration_since(state.window_start) >= self.window {
            state.window_start = now;
            state.seen = 0;
        }

        if state.seen < self.limit {
            state.seen += 1;
            BroadcastRateDecision::Allow
        } else {
            let elapsed = now.duration_since(state.window_start);
            let retry_after = self.window.saturating_sub(elapsed);
            BroadcastRateDecision::Block { retry_after }
        }
    }
}

#[derive(Debug)]
struct UdpSourceFastFailGuard {
    threshold_per_window: usize,
    window: Duration,
    block_for: Duration,
    states: HashMap<String, UdpSourceFastFailState>,
}

#[derive(Debug)]
struct UdpSourceFastFailState {
    window_start: Instant,
    seen: usize,
    blocked_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
enum UdpSourceDecision {
    Allow,
    DropBlocked {
        retry_after: Duration,
    },
    DropThresholdExceeded {
        threshold_per_window: usize,
        block_for: Duration,
    },
}

impl UdpSourceFastFailGuard {
    fn from_config(config: AbusePreventionConfig) -> Option<Self> {
        if config.udp_source_fast_fail_threshold_per_sec == 0 {
            return None;
        }

        Some(Self {
            threshold_per_window: config.udp_source_fast_fail_threshold_per_sec,
            window: Duration::from_millis(BROADCAST_WINDOW_MILLIS),
            block_for: Duration::from_millis(config.udp_source_block_ms),
            states: HashMap::new(),
        })
    }

    fn check(&mut self, source: &SocketAddr, now: Instant) -> UdpSourceDecision {
        if self.states.len() > UDP_SOURCE_TRACK_MAX {
            self.states.retain(|_, state| {
                state
                    .blocked_until
                    .map(|until| until > now)
                    .unwrap_or_else(|| now.duration_since(state.window_start) <= self.window)
            });
        }

        let key = source.ip().to_string();
        let state = self
            .states
            .entry(key)
            .or_insert_with(|| UdpSourceFastFailState {
                window_start: now,
                seen: 0,
                blocked_until: None,
            });

        if let Some(until) = state.blocked_until {
            if until > now {
                return UdpSourceDecision::DropBlocked {
                    retry_after: until.duration_since(now),
                };
            }
            state.blocked_until = None;
        }

        if now.duration_since(state.window_start) >= self.window {
            state.window_start = now;
            state.seen = 0;
        }

        state.seen = state.seen.saturating_add(1);
        if state.seen > self.threshold_per_window {
            state.blocked_until = Some(now + self.block_for);
            state.seen = 0;
            return UdpSourceDecision::DropThresholdExceeded {
                threshold_per_window: self.threshold_per_window,
                block_for: self.block_for,
            };
        }

        UdpSourceDecision::Allow
    }
}

fn parse_env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn parse_env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

impl MeshClient {
    pub async fn new(
        node_id: String,
        announce_addr: String,
        listen_addr: String,
        bootstrap_peers: Vec<String>,
        backend: NetworkBackend,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        let node_id = node_id;
        let transport = match backend {
            NetworkBackend::Udp => TransportBackend::Udp(
                UdpTransport::new(node_id.clone(), announce_addr.clone(), monitor.clone()).await?,
            ),
            NetworkBackend::Dht => TransportBackend::Dht(
                DhtTransport::new(
                    node_id.clone(),
                    listen_addr,
                    announce_addr,
                    bootstrap_peers,
                    monitor,
                )
                .await?,
            ),
        };

        Ok(Self { transport })
    }

    pub async fn broadcast(&self, msg: Envelope) -> Result<()> {
        match &self.transport {
            TransportBackend::Udp(udp) => udp.broadcast(msg).await,
            TransportBackend::Dht(dht) => dht.broadcast(msg).await,
        }
    }
}

impl BroadcastListener {
    pub async fn new(
        bind_addr: String,
        node_id: String,
        backend: NetworkBackend,
        bootstrap_peers: Vec<String>,
        announce_addr: String,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        let monitor_for_udp = monitor.clone();
        let monitor_for_dht = monitor;
        match backend {
            NetworkBackend::Udp => {
                let mut transport = UdpTransport::new_listener(
                    bind_addr.clone(),
                    announce_addr,
                    bootstrap_peers,
                    monitor_for_udp,
                )
                .await?;
                let (tx, rx) = mpsc::channel(64);
                tokio::spawn(async move {
                    loop {
                        match transport.next_message().await {
                            Ok(Some(msg)) => {
                                if tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                });

                Ok(Self {
                    receiver: rx,
                    bind_addr,
                })
            }
            NetworkBackend::Dht => {
                let mut transport = DhtTransport::new_listener(
                    node_id,
                    bind_addr.clone(),
                    announce_addr,
                    bootstrap_peers,
                    monitor_for_dht,
                )
                .await?;
                let (tx, rx) = mpsc::channel(64);
                tokio::spawn(async move {
                    loop {
                        match transport.next_message().await {
                            Ok(Some(msg)) => {
                                if tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                });

                Ok(Self {
                    receiver: rx,
                    bind_addr,
                })
            }
        }
    }

    pub fn bind_addr(&self) -> &str {
        &self.bind_addr
    }

    pub async fn next_message(&mut self) -> Result<Option<Envelope>> {
        Ok(self.receiver.recv().await)
    }
}

fn parse_addr(raw: &str) -> Result<Multiaddr> {
    if raw.contains('/') {
        Multiaddr::from_str(raw).map_err(|e| anyhow!("invalid multiaddr {raw}: {e}"))
    } else {
        let addr: SocketAddr = raw
            .parse()
            .map_err(|e| anyhow!("invalid socket addr {raw}: {e}"))?;
        let ip = addr.ip();
        let port = addr.port();
        match ip {
            std::net::IpAddr::V4(v4) => Ok(format!("/ip4/{}/tcp/{port}", v4).parse::<Multiaddr>()?),
            std::net::IpAddr::V6(v6) => Ok(format!("/ip6/{}/tcp/{port}", v6).parse::<Multiaddr>()?),
        }
    }
}

fn parse_udp_targets(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .filter_map(|item| {
            let _ = SocketAddr::from_str(item).ok()?;
            Some(item.to_string())
        })
        .collect()
}

fn parse_bootstrap_peers(raw: Vec<String>) -> Vec<Multiaddr> {
    let mut peers = Vec::new();
    for item in raw {
        for addr in item.split(',') {
            let addr = addr.trim();
            if addr.is_empty() {
                continue;
            }
            if let Ok(ma) = parse_addr(addr) {
                peers.push(ma);
            }
        }
    }
    peers
}

fn dedupe_multiaddr(input: Vec<Multiaddr>) -> Vec<Multiaddr> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(input.len());
    for addr in input {
        let key = addr.to_string();
        if seen.insert(key) {
            out.push(addr);
        }
    }
    out
}

fn build_dht_components(local_key: &identity::Keypair, topic: &str) -> Result<Gossipsub> {
    let gossipsub_config = GossipsubConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(gossipsub::ValidationMode::None)
        .build()
        .map_err(|e| anyhow!("gossipsub config: {e}"))?;

    let mut gossipsub = Gossipsub::new(
        MessageAuthenticity::Signed(local_key.clone()),
        gossipsub_config,
    )
    .map_err(|e| anyhow!("gossipsub init: {e}"))?;
    gossipsub.subscribe(&IdentTopic::new(topic.to_string()))?;

    Ok(gossipsub)
}

impl UdpTransport {
    pub async fn new(
        _node_id: String,
        announce_addr: String,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        let abuse_config = AbusePreventionConfig::from_env();
        monitor
            .emit(
                "udp_sender_init",
                EventSource::Network,
                serde_json::json!({ "announce_addr": announce_addr, "mode": "unicast_listener" }),
            )
            .await;
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        socket
            .set_broadcast(true)
            .map_err(|e| anyhow!(e.to_string()))?;

        Ok(Self {
            socket,
            announce_addr,
            monitor,
            fragments: HashMap::new(),
            outbound_rate_limiter: BroadcastRateLimiter::new(
                abuse_config.udp_broadcasts_per_sec,
                Duration::from_millis(BROADCAST_WINDOW_MILLIS),
            ),
            inbound_source_guard: None,
        })
    }

    pub async fn new_listener(
        bind_addr: String,
        announce_addr: String,
        _bootstrap_peers: Vec<String>,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        let abuse_config = AbusePreventionConfig::from_env();
        monitor
            .emit(
                "udp_listener_init",
                EventSource::Network,
                serde_json::json!({ "bind_addr": bind_addr, "announce_addr": announce_addr }),
            )
            .await;
        let socket = UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        socket
            .set_broadcast(true)
            .map_err(|e| anyhow!(e.to_string()))?;
        let _ = announce_addr;

        Ok(Self {
            socket,
            announce_addr: String::new(),
            monitor,
            fragments: HashMap::new(),
            outbound_rate_limiter: BroadcastRateLimiter::new(
                abuse_config.udp_broadcasts_per_sec,
                Duration::from_millis(BROADCAST_WINDOW_MILLIS),
            ),
            inbound_source_guard: UdpSourceFastFailGuard::from_config(abuse_config),
        })
    }

    pub async fn broadcast(&self, msg: Envelope) -> Result<()> {
        if let Some(limiter) = &self.outbound_rate_limiter {
            if let BroadcastRateDecision::Block { retry_after } = limiter.check(Instant::now()) {
                self.monitor
                    .emit(
                        "udp_rate_limited",
                        EventSource::Network,
                        serde_json::json!({
                            "limit_per_sec": limiter.limit,
                            "retry_after_ms": retry_after.as_millis(),
                            "announce_addr": self.announce_addr,
                        }),
                    )
                    .await;
                return Err(anyhow!("udp broadcast rate limited"));
            }
        }

        let data = serde_json::to_string(&msg)?;
        let mut sent = false;
        let mut sent_count = 0usize;
        for addr in self.announce_addr.split(',') {
            let addr = addr.trim();
            if addr.is_empty() {
                continue;
            }
            let n = self.send_payload(addr, data.as_bytes()).await?;
            if n > 0 {
                sent = true;
                sent_count = sent_count.saturating_add(1);
            }
        }
        self.monitor
            .emit(
                "udp_broadcast",
                EventSource::Network,
                serde_json::json!({
                    "targets": sent_count,
                    "bytes": data.len(),
                }),
            )
            .await;
        if !sent {
            return Err(anyhow!("no bytes sent"));
        }
        Ok(())
    }

    async fn send_payload(&self, target: &str, payload: &[u8]) -> Result<usize> {
        self.send_payload_to_target(target, payload).await
    }

    async fn send_payload_to_target(&self, addr: &str, payload: &[u8]) -> Result<usize> {
        let (_, bytes) = send_udp_payload(&self.socket, addr, payload).await?;
        Ok(bytes)
    }

    pub async fn next_message(&mut self) -> Result<Option<Envelope>> {
        let mut buf = vec![0u8; 65536];
        loop {
            if let Ok((n, from)) = self.socket.recv_from(&mut buf).await {
                if let Some(guard) = self.inbound_source_guard.as_mut() {
                    match guard.check(&from, Instant::now()) {
                        UdpSourceDecision::Allow => {}
                        UdpSourceDecision::DropBlocked { retry_after } => {
                            self.monitor
                                .emit(
                                    "udp_inbound_fast_fail_drop",
                                    EventSource::Network,
                                    serde_json::json!({
                                        "from": from.to_string(),
                                        "source": from.ip().to_string(),
                                        "reason": "blocked",
                                        "retry_after_ms": retry_after.as_millis(),
                                    }),
                                )
                                .await;
                            continue;
                        }
                        UdpSourceDecision::DropThresholdExceeded {
                            threshold_per_window,
                            block_for,
                        } => {
                            self.monitor
                                .emit(
                                    "udp_inbound_fast_fail_drop",
                                    EventSource::Network,
                                    serde_json::json!({
                                        "from": from.to_string(),
                                        "source": from.ip().to_string(),
                                        "reason": "threshold_exceeded",
                                        "threshold_per_sec": threshold_per_window,
                                        "block_for_ms": block_for.as_millis(),
                                    }),
                                )
                                .await;
                            continue;
                        }
                    }
                }

                self.monitor
                    .emit(
                        "udp_receive",
                        EventSource::Network,
                        serde_json::json!({
                            "from": from.to_string(),
                            "bytes": n,
                        }),
                    )
                    .await;

                if let Ok(msg) = serde_json::from_slice::<Envelope>(&buf[..n]) {
                    return Ok(Some(msg));
                }

                if let Ok(mut fragment) = serde_json::from_slice::<UdpFragment>(&buf[..n]) {
                    match self.handle_fragment(&mut fragment).await {
                        Ok(Some(msg)) => {
                            return Ok(Some(msg));
                        }
                        Ok(None) => continue,
                        Err(err) => {
                            let _ = self
                                .monitor
                                .emit(
                                    "udp_fragment_failed",
                                    EventSource::Network,
                                    serde_json::json!({
                                        "from": from.to_string(),
                                        "envelope_id": fragment.envelope_id,
                                        "error": err.to_string(),
                                    }),
                                )
                                .await;
                        }
                    }
                    continue;
                }

                let _ = self
                    .monitor
                    .emit(
                        "udp_receive_unparseable",
                        EventSource::Network,
                        serde_json::json!({
                            "from": from.to_string(),
                            "bytes": n,
                        }),
                    )
                    .await;
            }
            return Ok(None);
        }
    }

    async fn handle_fragment(&mut self, fragment: &mut UdpFragment) -> Result<Option<Envelope>> {
        maybe_reassemble_fragment(
            &mut self.fragments,
            fragment,
            UDP_FRAGMENT_TTL_SECONDS,
            &self.monitor,
        )
        .await
    }
}

impl DhtTransport {
    pub async fn new(
        node_id: String,
        _listen_addr: String,
        announce_addr: String,
        bootstrap_peers: Vec<String>,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        DhtTransport::build(
            node_id,
            listen_from_str("0.0.0.0:0"),
            announce_addr,
            bootstrap_peers,
            monitor,
        )
        .await
    }

    pub async fn new_listener(
        node_id: String,
        bind_addr: String,
        announce_addr: String,
        bootstrap_peers: Vec<String>,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        DhtTransport::build(node_id, bind_addr, announce_addr, bootstrap_peers, monitor).await
    }

    async fn build(
        node_id: String,
        listen_addr: String,
        announce_addr: String,
        bootstrap_peers: Vec<String>,
        monitor: MonitorHandle,
    ) -> Result<Self> {
        let abuse_config = AbusePreventionConfig::from_env();
        monitor
            .emit(
                "dht_init",
                EventSource::Network,
                serde_json::json!({ "listen_addr": listen_addr, "announce_addr": announce_addr }),
            )
            .await;

        let local_key = identity::Keypair::generate_ed25519();
        let peer_id = PeerId::from(local_key.public());
        let topic = IdentTopic::new(DHT_TOPIC);

        let gossipsub = build_dht_components(&local_key, DHT_TOPIC)?;
        let mdns = mdns::tokio::Behaviour::new(MdnsConfig::default(), peer_id)
            .map_err(|e| anyhow!("mdns init: {e}"))?;

        let behaviour = DhtBehaviour { gossipsub, mdns };
        let transport = TokioTcpTransport::new(libp2p::tcp::Config::default());
        let noise = noise::Config::new(&local_key)?;
        let transport = transport
            .upgrade(libp2p::core::upgrade::Version::V1)
            .authenticate(noise)
            .multiplex(yamux::Config::default())
            .boxed();
        let mut swarm = Swarm::new(
            transport,
            behaviour,
            peer_id,
            libp2p::swarm::Config::with_tokio_executor(),
        );

        let listen = parse_addr(&listen_addr)?;
        swarm.listen_on(listen)?;

        let mut peers = parse_bootstrap_peers(bootstrap_peers);
        peers = dedupe_multiaddr(peers);
        monitor
            .emit(
                "dht_bootstrap_peers",
                EventSource::Network,
                serde_json::json!({ "count": peers.len() }),
            )
            .await;

        for addr in peers {
            if let Err(err) = swarm.dial(addr.clone()) {
                monitor
                    .emit(
                        "dht_bootstrap_dial",
                        EventSource::Network,
                        serde_json::json!({ "target": addr.to_string(), "ok": false, "error": err.to_string(), "node_id": node_id }),
                    )
                    .await;
            } else {
                monitor
                    .emit(
                        "dht_bootstrap_dial",
                        EventSource::Network,
                        serde_json::json!({ "target": addr.to_string(), "ok": true, "node_id": node_id }),
                    )
                    .await;
            }
        }

        let (fallback_udp, fallback_udp_msg_rx) = if announce_addr.trim().is_empty() {
            monitor
                .emit(
                    "dht_fallback_udp",
                    EventSource::Network,
                    serde_json::json!({ "enabled": false, "reason": "announce not set" }),
                )
                .await;
            (None, None)
        } else {
            match UdpSocket::bind(&listen_addr).await {
                Ok(rx_socket) => {
                    let _ = rx_socket.set_broadcast(true);
                    let send_socket = UdpSocket::bind("0.0.0.0:0").await.ok();
                    if send_socket.is_none() {
                        monitor
                            .emit(
                                "dht_fallback_udp",
                                EventSource::Network,
                                serde_json::json!({
                                    "enabled": false,
                                    "reason": "sender unavailable",
                                    "node_id": node_id,
                                }),
                            )
                            .await;
                    } else {
                        monitor
                            .emit(
                                "dht_fallback_udp",
                                EventSource::Network,
                                serde_json::json!({
                                    "enabled": true,
                                    "node_id": node_id,
                                }),
                            )
                            .await;
                    }

                    let (udp_msg_tx, udp_msg_rx) = mpsc::channel::<Envelope>(64);
                    let monitor_for_fallback = monitor.clone();
                    let mut udp_fragments: HashMap<String, UdpFragmentState> = HashMap::new();
                    let mut source_guard = UdpSourceFastFailGuard::from_config(abuse_config);
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            let res = rx_socket.recv_from(&mut buf).await;
                            match res {
                                Ok((n, from)) => {
                                    if let Some(guard) = source_guard.as_mut() {
                                        match guard.check(&from, Instant::now()) {
                                            UdpSourceDecision::Allow => {}
                                            UdpSourceDecision::DropBlocked { retry_after } => {
                                                let _ = monitor_for_fallback
                                                    .emit(
                                                        "udp_inbound_fast_fail_drop",
                                                        EventSource::Network,
                                                        serde_json::json!({
                                                            "from": from.to_string(),
                                                            "source": from.ip().to_string(),
                                                            "reason": "blocked",
                                                            "retry_after_ms": retry_after.as_millis(),
                                                            "from_dht": true,
                                                        }),
                                                    )
                                                    .await;
                                                continue;
                                            }
                                            UdpSourceDecision::DropThresholdExceeded {
                                                threshold_per_window,
                                                block_for,
                                            } => {
                                                let _ = monitor_for_fallback
                                                    .emit(
                                                        "udp_inbound_fast_fail_drop",
                                                        EventSource::Network,
                                                        serde_json::json!({
                                                            "from": from.to_string(),
                                                            "source": from.ip().to_string(),
                                                            "reason": "threshold_exceeded",
                                                            "threshold_per_sec": threshold_per_window,
                                                            "block_for_ms": block_for.as_millis(),
                                                            "from_dht": true,
                                                        }),
                                                    )
                                                    .await;
                                                continue;
                                            }
                                        }
                                    }
                                    if let Ok(env) = serde_json::from_slice::<Envelope>(&buf[..n]) {
                                        let _ = udp_msg_tx.send(env).await;
                                        continue;
                                    }
                                    if let Ok(mut fragment) =
                                        serde_json::from_slice::<UdpFragment>(&buf[..n])
                                    {
                                        match maybe_reassemble_fragment(
                                            &mut udp_fragments,
                                            &mut fragment,
                                            UDP_FRAGMENT_TTL_SECONDS,
                                            &monitor_for_fallback,
                                        )
                                        .await
                                        {
                                            Ok(Some(env)) => {
                                                let _ = udp_msg_tx.send(env).await;
                                            }
                                            Ok(None) => {}
                                            Err(err) => {
                                                let _ = monitor_for_fallback
                                                    .emit(
                                                        "udp_fragment_failed",
                                                        EventSource::Network,
                                                        serde_json::json!({
                                                            "from_dht": true,
                                                            "envelope_id": fragment.envelope_id,
                                                            "error": err.to_string(),
                                                        }),
                                                    )
                                                    .await;
                                            }
                                        }
                                    } else {
                                        let _ = monitor_for_fallback
                                            .emit(
                                                "udp_receive_unparseable",
                                                EventSource::Network,
                                                serde_json::json!({ "from_dht": true, "bytes": n }),
                                            )
                                            .await;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });

                    (send_socket, Some(udp_msg_rx))
                }
                Err(err) => {
                    monitor
                        .emit(
                            "dht_fallback_udp",
                            EventSource::Network,
                            serde_json::json!({
                                "enabled": false,
                                "reason": err.to_string(),
                                "node_id": node_id,
                            }),
                        )
                        .await;
                    (None, None)
                }
            }
        };
        let mut fallback_udp_msg_rx = match fallback_udp_msg_rx {
            Some(rx) => rx,
            None => {
                let (_tx, rx) = mpsc::channel::<Envelope>(1);
                rx
            }
        };

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<DhtCommand>(64);
        let (msg_tx, msg_rx) = mpsc::channel::<Envelope>(64);
        let topic_for_loop = topic.clone();
        let monitor_for_loop = monitor.clone();

        let local_node_id = node_id.clone();
        let task = tokio::spawn(async move {
            let topic = topic_for_loop;
            let monitor = monitor_for_loop;
            let local_node = local_node_id;
            loop {
                tokio::select! {
                    Some(cmd) = cmd_rx.recv() => match cmd {
                        DhtCommand::Broadcast { msg, reply } => {
                            let mut outcome = Ok(());
                            if let Ok(payload) = serde_json::to_vec(&msg) {
                                let mut attempts = 0u8;
                                loop {
                                    match swarm
                                        .behaviour_mut()
                                        .gossipsub
                                        .publish(topic.clone(), payload.clone())
                                    {
                                        Ok(_) => break,
                                        Err(err) => {
                                            attempts += 1;
                                            outcome = Err(anyhow!("publish failed: {err}"));
                                            let _ = monitor.emit(
                                                "dht_publish_failed",
                                                EventSource::Network,
                                                serde_json::json!({
                                                    "attempt": attempts,
                                                    "error": err.to_string(),
                                                    "bytes": payload.len(),
                                                    "from": local_node,
                                                }),
                                            ).await;
                                            if attempts > 12 {
                                                break;
                                            }
                                            tokio::time::sleep(Duration::from_millis(300)).await;
                                        }
                                    }
                                }
                            } else {
                                outcome = Err(anyhow!("failed to serialize envelope"));
                            }
                            let ok = outcome.is_ok();
                            if ok {
                                let _ = monitor.emit(
                                    "dht_publish_success",
                                    EventSource::Network,
                                    serde_json::json!({ "from": local_node }),
                                )
                                .await;
                            }
                            let _ = reply.send(outcome);
                        }
                    }
                    ,
                    Some(udp_msg) = fallback_udp_msg_rx.recv() => {
                        let _ = monitor.emit(
                            "dht_udp_message",
                            EventSource::Network,
                            serde_json::json!({ "from": local_node }),
                        )
                        .await;
                        let _ = msg_tx.send(udp_msg).await;
                    }
                    ,
                    event = swarm.select_next_some() => {
                        match event {
                            SwarmEvent::NewListenAddr { address, .. } => {
                                let _ = monitor.emit(
                                    "dht_listening",
                                    EventSource::Network,
                                    serde_json::json!({
                                        "address": address.to_string(),
                                        "from": local_node,
                                    }),
                                )
                                .await;
                            }
                            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                                let _ = monitor.emit(
                                    "dht_connection_established",
                                    EventSource::Network,
                                    serde_json::json!({ "from": local_node, "peer": peer_id.to_string() }),
                                )
                                .await;
                            }
                            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                                let _ = monitor.emit(
                                    "dht_connection_error",
                                    EventSource::Network,
                                    serde_json::json!({
                                        "from": local_node,
                                        "peer": peer_id.map(|p| p.to_string()),
                                        "direction": "outgoing",
                                        "error": error.to_string(),
                                    }),
                                )
                                .await;
                            }
                            SwarmEvent::IncomingConnectionError { error, .. } => {
                                let _ = monitor.emit(
                                    "dht_connection_error",
                                    EventSource::Network,
                                    serde_json::json!({
                                        "from": local_node,
                                        "direction": "incoming",
                                        "error": error.to_string(),
                                    }),
                                )
                                .await;
                            }
                            SwarmEvent::Behaviour(DhtBehaviourEvent::Mdns(MdnsEvent::Discovered(peers))) => {
                                for (_peer, addr) in peers {
                                    if let Err(err) = swarm.dial(addr.clone()) {
                                        let _ = monitor.emit(
                                            "dht_mdns_dial_failed",
                                            EventSource::Network,
                                            serde_json::json!({
                                                "from": local_node,
                                                "addr": addr.to_string(),
                                                "error": err.to_string(),
                                            }),
                                        )
                                        .await;
                                    }
                                    swarm.behaviour_mut().gossipsub.add_explicit_peer(&_peer);
                                }
                            }
                            SwarmEvent::Behaviour(DhtBehaviourEvent::Mdns(MdnsEvent::Expired(peers))) => {
                                for (peer, _addr) in peers {
                                    swarm
                                        .behaviour_mut()
                                        .gossipsub
                                        .remove_explicit_peer(&peer);
                                }
                            }
                            SwarmEvent::Behaviour(DhtBehaviourEvent::Gossipsub(
                                GossipsubEvent::Message {
                                    message, ..
                                },
                            )) => {
                                if let Ok(env) = serde_json::from_slice::<Envelope>(&message.data) {
                                    let peer = message.source.as_ref().map(|x| x.to_string());
                                    let _ = monitor.emit(
                                        "dht_message_received",
                                        EventSource::Network,
                                        serde_json::json!({
                                            "from": local_node,
                                            "peer": peer
                                        }),
                                    )
                                    .await;
                                    let _ = msg_tx.send(env).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self {
            cmd_tx,
            announce_addr,
            fallback_udp,
            msg_rx,
            monitor,
            outbound_rate_limiter: BroadcastRateLimiter::new(
                abuse_config.dht_broadcasts_per_sec,
                Duration::from_millis(BROADCAST_WINDOW_MILLIS),
            ),
            _task: task,
        })
    }

    pub async fn broadcast(&self, msg: Envelope) -> Result<()> {
        if let Some(limiter) = &self.outbound_rate_limiter {
            if let BroadcastRateDecision::Block { retry_after } = limiter.check(Instant::now()) {
                self.monitor
                    .emit(
                        "dht_rate_limited",
                        EventSource::Network,
                        serde_json::json!({
                            "limit_per_sec": limiter.limit,
                            "retry_after_ms": retry_after.as_millis(),
                            "announce_addr": self.announce_addr,
                        }),
                    )
                    .await;
                return Err(anyhow!("dht broadcast rate limited"));
            }
        }

        let payload = serde_json::to_vec(&msg)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg_for_worker = msg.clone();
        self.cmd_tx
            .send(DhtCommand::Broadcast {
                msg: msg_for_worker,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("dht transport closed"))?;

        let publish_result = reply_rx
            .await
            .map_err(|_| anyhow!("dht worker terminated"))?;

        let fallback_sent = self.fallback_send(&payload).await?;

        if publish_result.is_ok() {
            if !fallback_sent {
                let _ = self
                    .monitor
                    .emit(
                        "dht_publish_no_fallback",
                        EventSource::Network,
                        serde_json::json!({
                            "announce_addr": self.announce_addr,
                            "reason": "fallback_not_sent"
                        }),
                    )
                    .await;
            }
            return Ok(());
        }

        if fallback_sent {
            return Ok(());
        }

        publish_result
    }

    async fn fallback_send(&self, payload: &[u8]) -> Result<bool> {
        let udp = match &self.fallback_udp {
            Some(udp) => udp,
            None => return Ok(false),
        };

        let mut sent = false;
        for addr in parse_udp_targets(&self.announce_addr) {
            let addr_for_send = addr.clone();
            if let Ok((is_sent, bytes)) = send_udp_payload(udp, &addr_for_send, payload).await {
                if is_sent && bytes > 0 {
                    sent = true;
                    let addr = addr.clone();
                    let _ = self
                        .monitor
                        .emit(
                            "dht_fallback_send",
                            EventSource::Network,
                            serde_json::json!({
                                "target": addr,
                                "bytes": bytes,
                                "announce_addr": self.announce_addr,
                            }),
                        )
                        .await;
                }
            }
        }

        Ok(sent)
    }

    pub async fn next_message(&mut self) -> Result<Option<Envelope>> {
        Ok(self.msg_rx.recv().await)
    }
}

fn listen_from_str(s: &str) -> String {
    s.to_string()
}

async fn maybe_reassemble_fragment(
    fragments: &mut HashMap<String, UdpFragmentState>,
    fragment: &mut UdpFragment,
    ttl_seconds: u64,
    monitor: &MonitorHandle,
) -> Result<Option<Envelope>> {
    let now = Instant::now();
    let mut dropped = 0usize;
    fragments.retain(|_, state| {
        let keep = now.duration_since(state.last_seen).as_secs() <= ttl_seconds;
        if !keep {
            dropped += 1;
        }
        keep
    });
    if dropped > 0 {
        let _ = monitor
            .emit(
                "udp_fragment_dropped",
                EventSource::Network,
                serde_json::json!({ "count": dropped }),
            )
            .await;
    }

    let total = usize::from(fragment.total);
    if total == 0 {
        return Ok(None);
    }

    let index = usize::from(fragment.index);
    if index >= total {
        return Ok(None);
    }

    let chunk = hex::decode(&fragment.payload)?;
    let state = fragments
        .entry(fragment.envelope_id.clone())
        .or_insert_with(|| UdpFragmentState {
            total,
            received: vec![None; total],
            last_seen: Instant::now(),
        });

    if state.total != total {
        state.total = total;
        state.received.resize_with(total, || None);
    }
    state.last_seen = Instant::now();
    if state.received[index].is_none() {
        state.received[index] = Some(chunk);
    }

    if state.received.iter().all(std::option::Option::is_some) {
        let mut payload = Vec::new();
        for part in state.received.iter() {
            if let Some(chunk) = part {
                payload.extend_from_slice(chunk);
            } else {
                return Ok(None);
            }
        }
        fragments.remove(&fragment.envelope_id);
        let _ = monitor
            .emit(
                "udp_fragment_reassembled",
                EventSource::Network,
                serde_json::json!({
                    "envelope_id": fragment.envelope_id,
                    "total": total,
                    "from": fragment.envelope_id,
                }),
            )
            .await;
        let envelope = serde_json::from_slice(&payload)?;
        Ok(Some(envelope))
    } else {
        Ok(None)
    }
}

async fn send_udp_payload(socket: &UdpSocket, addr: &str, payload: &[u8]) -> Result<(bool, usize)> {
    if payload.len() <= UDP_MAX_DATAGRAM {
        let n = socket.send_to(payload, addr).await?;
        return Ok((n > 0, n));
    }

    let envelope_id = Uuid::new_v4().to_string();
    let total = u16::try_from((payload.len() + UDP_FRAGMENT_SIZE - 1) / UDP_FRAGMENT_SIZE)
        .map_err(|_| anyhow!("udp payload too large for fragmentation"))?;

    let mut sent = false;
    let mut bytes = 0usize;
    let fragments: Vec<Vec<u8>> = payload
        .chunks(UDP_FRAGMENT_SIZE)
        .map(|chunk| chunk.to_vec())
        .collect();
    for attempt in 1..=UDP_FRAGMENT_SEND_ATTEMPTS {
        for (index, chunk) in fragments.iter().enumerate() {
            let index = u16::try_from(index).map_err(|_| anyhow!("udp fragment index overflow"))?;
            let fragment = UdpFragment {
                envelope_id: envelope_id.clone(),
                total,
                index,
                payload: hex::encode(chunk),
            };
            let encoded = serde_json::to_vec(&fragment)?;
            let n = socket.send_to(&encoded, addr).await?;
            if n > 0 {
                sent = true;
                bytes = bytes.saturating_add(n);
            }
        }
        if attempt < UDP_FRAGMENT_SEND_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(UDP_FRAGMENT_RESEND_DELAY_MS)).await;
        }
    }

    Ok((sent, bytes))
}

#[cfg(test)]
mod tests {
    use super::{
        BroadcastRateDecision, BroadcastRateLimiter, UdpSourceDecision, UdpSourceFastFailGuard,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    #[test]
    fn broadcast_rate_limiter_blocks_when_limit_is_exceeded() {
        let limiter =
            BroadcastRateLimiter::new(2, Duration::from_millis(1_000)).expect("limiter enabled");
        let now = Instant::now();

        assert!(matches!(limiter.check(now), BroadcastRateDecision::Allow));
        assert!(matches!(limiter.check(now), BroadcastRateDecision::Allow));
        assert!(matches!(
            limiter.check(now),
            BroadcastRateDecision::Block { .. }
        ));

        assert!(matches!(
            limiter.check(now + Duration::from_millis(1_001)),
            BroadcastRateDecision::Allow
        ));
    }

    #[test]
    fn source_fast_fail_blocks_and_recovers_after_block_window() {
        let mut guard = UdpSourceFastFailGuard {
            threshold_per_window: 3,
            window: Duration::from_millis(1_000),
            block_for: Duration::from_millis(500),
            states: std::collections::HashMap::new(),
        };
        let source = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12_345);
        let now = Instant::now();

        assert!(matches!(
            guard.check(&source, now),
            UdpSourceDecision::Allow
        ));
        assert!(matches!(
            guard.check(&source, now),
            UdpSourceDecision::Allow
        ));
        assert!(matches!(
            guard.check(&source, now),
            UdpSourceDecision::Allow
        ));
        assert!(matches!(
            guard.check(&source, now),
            UdpSourceDecision::DropThresholdExceeded { .. }
        ));

        assert!(matches!(
            guard.check(&source, now + Duration::from_millis(200)),
            UdpSourceDecision::DropBlocked { .. }
        ));
        assert!(matches!(
            guard.check(&source, now + Duration::from_millis(501)),
            UdpSourceDecision::Allow
        ));
    }

    #[test]
    fn source_fast_fail_applies_to_source_ip_not_port() {
        let mut guard = UdpSourceFastFailGuard {
            threshold_per_window: 1,
            window: Duration::from_millis(1_000),
            block_for: Duration::from_millis(500),
            states: std::collections::HashMap::new(),
        };
        let source_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20_000);
        let source_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20_001);
        let now = Instant::now();

        assert!(matches!(
            guard.check(&source_a, now),
            UdpSourceDecision::Allow
        ));
        assert!(matches!(
            guard.check(&source_b, now),
            UdpSourceDecision::DropThresholdExceeded { .. }
        ));
    }
}
