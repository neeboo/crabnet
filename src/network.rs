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
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

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
    _task: tokio::task::JoinHandle<()>,
}

#[derive(NetworkBehaviour)]
struct DhtBehaviour {
    gossipsub: Gossipsub,
    mdns: mdns::tokio::Behaviour,
}

const DHT_TOPIC: &str = "crabnet-mvp-v1";

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
        })
    }

    pub async fn new_listener(
        bind_addr: String,
        announce_addr: String,
        _bootstrap_peers: Vec<String>,
        monitor: MonitorHandle,
    ) -> Result<Self> {
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
        })
    }

    pub async fn broadcast(&self, msg: Envelope) -> Result<()> {
        let data = serde_json::to_string(&msg)?;
        let mut sent = false;
        let mut sent_count = 0usize;
        for addr in self.announce_addr.split(',') {
            let addr = addr.trim();
            if addr.is_empty() {
                continue;
            }
            let n = self.socket.send_to(data.as_bytes(), addr).await?;
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

    pub async fn next_message(&mut self) -> Result<Option<Envelope>> {
        let mut buf = vec![0u8; 65536];
        if let Ok((n, from)) = self.socket.recv_from(&mut buf).await {
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
            return Ok(serde_json::from_slice::<Envelope>(&buf[..n]).ok());
        }
        Ok(None)
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
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            let res = rx_socket.recv_from(&mut buf).await;
                            match res {
                                Ok((n, _)) => {
                                    if let Ok(env) = serde_json::from_slice::<Envelope>(&buf[..n]) {
                                        let _ = udp_msg_tx.send(env).await;
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
            _task: task,
        })
    }

    pub async fn broadcast(&self, msg: Envelope) -> Result<()> {
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
            if let Ok(n) = udp.send_to(payload, addr_for_send).await {
                if n > 0 {
                    sent = true;
                    let addr = addr.clone();
                    let _ = self
                        .monitor
                        .emit(
                            "dht_fallback_send",
                            EventSource::Network,
                            serde_json::json!({
                                "target": addr,
                                "bytes": n,
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
