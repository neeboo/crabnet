use crate::monitor::{EventSource, MonitorEvent};
use anyhow::{Context, Result};
use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::fs;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct WebState {
    pub monitor_path: PathBuf,
}

#[derive(Serialize)]
struct EventListResponse {
    events: Vec<MonitorEvent>,
    total: usize,
}

#[derive(Deserialize)]
struct EventQuery {
    limit: Option<usize>,
    kind: Option<String>,
    source: Option<String>,
}

#[derive(Serialize, Clone)]
struct TopologyNode {
    id: String,
    kind: String,
    last_seen: u64,
    degree: usize,
}

#[derive(Serialize, Clone)]
struct TopologyEdge {
    from: String,
    to: String,
    kind: String,
    total: usize,
    last_seen: u64,
}

#[derive(Serialize)]
struct TopologyResponse {
    nodes: Vec<TopologyNode>,
    edges: Vec<TopologyEdge>,
}

#[derive(Serialize)]
struct OverviewResponse {
    nodes: usize,
    edges: usize,
    events: usize,
    last_event_ts: Option<u64>,
    node_events: u64,
    top_kinds: Vec<(String, usize)>,
    top_sources: Vec<(String, usize)>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

const CRABNET_WEB_API_AUTH_REQUIRED: &str = "CRABNET_WEB_API_AUTH_REQUIRED";
const CRABNET_WEB_API_TOKEN: &str = "CRABNET_WEB_API_TOKEN";
const X_API_TOKEN_HEADER: &str = "x-api-token";

#[derive(Clone, Debug, PartialEq, Eq)]
struct ApiAuthConfig {
    required: bool,
    token: Option<String>,
}

impl ApiAuthConfig {
    fn from_env() -> Self {
        Self::from_raw(
            env::var(CRABNET_WEB_API_AUTH_REQUIRED).ok(),
            env::var(CRABNET_WEB_API_TOKEN).ok(),
        )
    }

    fn from_raw(required: Option<String>, token: Option<String>) -> Self {
        let required = required
            .as_deref()
            .map(parse_bool)
            .unwrap_or(Some(true))
            .unwrap_or_else(|| {
                if let Some(raw) = required {
                    eprintln!(
                        "invalid {} value `{raw}`, defaulting to protected API mode",
                        CRABNET_WEB_API_AUTH_REQUIRED
                    );
                }
                true
            });

        let token = token.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        Self { required, token }
    }

    fn is_authorized(&self, headers: &HeaderMap) -> bool {
        if !self.required {
            return true;
        }

        let Some(expected) = self.token.as_deref() else {
            return false;
        };

        bearer_token(headers).is_some_and(|token| token == expected)
            || headers
                .get(X_API_TOKEN_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|token| token == expected)
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") || parts.next().is_some() {
        return None;
    }
    Some(token)
}

pub async fn run(listen: &str, state: WebState) -> Result<()> {
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid web listen addr: {listen}"))?;
    let app = make_router(state);

    axum::serve(
        tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind web server at {listen}"))?,
        app,
    )
    .await
    .with_context(|| format!("serve web api at {listen}"))?;

    Ok(())
}

fn make_router(state: WebState) -> Router {
    make_router_with_auth(state, ApiAuthConfig::from_env())
}

fn make_router_with_auth(state: WebState, auth_config: ApiAuthConfig) -> Router {
    let static_root = resolve_static_root();
    eprintln!("web static directory: {}", static_root.display());

    let api_router = Router::new()
        .route("/events", get(api_events))
        .route("/topology", get(api_topology))
        .route("/overview", get(api_overview))
        .route_layer(middleware::from_fn_with_state(auth_config, api_auth_guard));

    let app = Router::new()
        .nest("/api", api_router)
        .route("/health", get(health))
        .with_state(state);

    let index_file = static_root.join("index.html");
    if static_root.exists() && static_root.is_dir() && index_file.exists() {
        let serve_dir = ServeDir::new(static_root).not_found_service(ServeFile::new(index_file));
        app.fallback_service(serve_dir)
    } else {
        eprintln!(
            "web static directory or index.html missing, fallback to built-in status page: {}",
            static_root.display()
        );
        app.route("/", get(index_page))
    }
}

fn resolve_static_root() -> PathBuf {
    let configured = env::var("CRABNET_WEB_DIST").ok();
    let fallback = PathBuf::from("web/app/dist");
    let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let add_candidate = |candidates: &mut Vec<PathBuf>, candidate: PathBuf| {
        if !candidates.iter().any(|p| p == &candidate) {
            candidates.push(candidate);
        }
    };

    let mut candidates = Vec::<PathBuf>::new();
    if let Some(value) = configured {
        let configured_path = PathBuf::from(value);
        add_candidate(&mut candidates, configured_path.clone());
        if configured_path.is_relative() {
            add_candidate(&mut candidates, current_dir.join(&configured_path));
            if let Some(exe_dir) = &exe_dir {
                add_candidate(&mut candidates, exe_dir.join(&configured_path));
                if let Some(parent) = exe_dir.parent() {
                    add_candidate(&mut candidates, parent.join(&configured_path));
                    if let Some(grand_parent) = parent.parent() {
                        add_candidate(&mut candidates, grand_parent.join(&configured_path));
                    }
                }
            }
        }
    } else {
        add_candidate(&mut candidates, fallback.clone());
        add_candidate(&mut candidates, current_dir.join(&fallback));
        if let Some(exe_dir) = &exe_dir {
            add_candidate(&mut candidates, exe_dir.join(&fallback));
            if let Some(parent) = exe_dir.parent() {
                add_candidate(&mut candidates, parent.join(&fallback));
                if let Some(grand_parent) = parent.parent() {
                    add_candidate(&mut candidates, grand_parent.join(&fallback));
                }
            }
        }
    }

    for candidate in &candidates {
        if candidate.exists() && candidate.is_dir() {
            return candidate.clone();
        }
    }

    if let Some(candidate) = candidates.first() {
        eprintln!(
            "web static directory not found, fallback to configured candidate: {}",
            candidate.display()
        );
        candidate.clone()
    } else {
        fallback
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn index_page() -> impl IntoResponse {
    (StatusCode::OK, include_str!("../web/index.html"))
}

async fn api_auth_guard(
    State(auth): State<ApiAuthConfig>,
    request: Request,
    next: Next,
) -> Response {
    if auth.is_authorized(request.headers()) {
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError {
            error: "unauthorized".to_string(),
        }),
    )
        .into_response()
}

async fn api_events(State(state): State<WebState>, Query(query): Query<EventQuery>) -> Response {
    match read_events(state.monitor_path.clone(), query).await {
        Ok(events) => {
            let total = events.len();
            (StatusCode::OK, Json(EventListResponse { events, total })).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn api_topology(State(state): State<WebState>) -> Response {
    match parse_topology(state).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn api_overview(State(state): State<WebState>) -> Response {
    match build_overview(state).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn parse_topology(state: WebState) -> Result<TopologyResponse> {
    let events = read_events(
        state.monitor_path,
        EventQuery {
            limit: Some(10_000),
            kind: None,
            source: None,
        },
    )
    .await?;

    let mut nodes = HashMap::<String, TopologyNode>::new();
    let mut edge_map = HashMap::<(String, String, String), TopologyEdge>::new();

    let mut register_node = |id: &str, kind: &str, ts: u64| {
        nodes
            .entry(id.to_string())
            .and_modify(|n| {
                n.last_seen = n.last_seen.max(ts);
            })
            .or_insert(TopologyNode {
                id: id.to_string(),
                kind: kind.to_string(),
                last_seen: ts,
                degree: 0,
            });
    };

    for e in events.iter() {
        register_node(&e.node_id, &e.source.to_string(), e.ts);
        if let Some(rel) = relation_from_payload(e) {
            let (from, to) = rel;
            register_node(&from, "network-node", e.ts);
            register_node(&to, "network-node", e.ts);
            let key = (from.clone(), to.clone(), e.kind.clone());
            edge_map
                .entry(key.clone())
                .and_modify(|edge| {
                    edge.total = edge.total.saturating_add(1);
                    edge.last_seen = edge.last_seen.max(e.ts);
                    edge.from = from.clone();
                })
                .or_insert(TopologyEdge {
                    from,
                    to,
                    kind: e.kind.clone(),
                    total: 1,
                    last_seen: e.ts,
                });
        }
    }

    for edge in edge_map.values() {
        if let Some(a) = nodes.get_mut(&edge.from) {
            a.degree = a.degree.saturating_add(edge.total);
        }
        if let Some(b) = nodes.get_mut(&edge.to) {
            b.degree = b.degree.saturating_add(edge.total);
        }
    }

    let mut node_list = nodes.into_values().collect::<Vec<_>>();
    node_list.sort_by_key(|n| n.last_seen);
    let mut edge_list = edge_map.into_values().collect::<Vec<_>>();
    edge_list.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

    Ok(TopologyResponse {
        nodes: node_list,
        edges: edge_list,
    })
}

async fn build_overview(state: WebState) -> Result<OverviewResponse> {
    let mut events = read_events(
        state.monitor_path,
        EventQuery {
            limit: None,
            kind: None,
            source: None,
        },
    )
    .await?;

    let topo =
        parse_events_to_topology(events.iter().map(|v| v as &MonitorEvent).collect()).await?;

    let mut kind_count = HashMap::<String, usize>::new();
    let mut source_count = HashMap::<String, usize>::new();
    let mut node_count = 0u64;
    let mut last_ts = None;

    for event in &events {
        kind_count
            .entry(event.kind.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        source_count
            .entry(event.source.to_string())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        if event.source == EventSource::Network {
            node_count = node_count.saturating_add(1);
        }
        last_ts = Some(event.ts);
    }

    let total_events = events.len();
    events.clear();

    let mut top_kinds = kind_count.into_iter().collect::<Vec<_>>();
    top_kinds.sort_by(|a, b| b.1.cmp(&a.1));
    if top_kinds.len() > 6 {
        top_kinds.truncate(6);
    }

    let mut top_sources = source_count.into_iter().collect::<Vec<_>>();
    top_sources.sort_by(|a, b| b.1.cmp(&a.1));
    if top_sources.len() > 6 {
        top_sources.truncate(6);
    }

    let edges = topo.edges.len();

    Ok(OverviewResponse {
        nodes: topo.nodes.len(),
        edges,
        events: total_events,
        last_event_ts: last_ts,
        node_events: node_count,
        top_kinds,
        top_sources,
    })
}

async fn parse_events_to_topology(events: Vec<&MonitorEvent>) -> Result<TopologyResponse> {
    let mut nodes = HashMap::<String, TopologyNode>::new();
    let mut edge_map = HashMap::<(String, String, String), TopologyEdge>::new();

    let mut register_node = |id: &str, kind: &str, ts: u64| {
        nodes
            .entry(id.to_string())
            .and_modify(|n| {
                n.last_seen = n.last_seen.max(ts);
            })
            .or_insert(TopologyNode {
                id: id.to_string(),
                kind: kind.to_string(),
                last_seen: ts,
                degree: 0,
            });
    };

    for e in events.iter() {
        register_node(&e.node_id, &e.source.to_string(), e.ts);
        if let Some(rel) = relation_from_payload(e) {
            let (from, to) = rel;
            register_node(&from, "network-node", e.ts);
            register_node(&to, "network-node", e.ts);
            let key = (from.clone(), to.clone(), e.kind.clone());
            edge_map
                .entry(key.clone())
                .and_modify(|edge| {
                    edge.total = edge.total.saturating_add(1);
                    edge.last_seen = edge.last_seen.max(e.ts);
                    edge.from = from.clone();
                })
                .or_insert(TopologyEdge {
                    from,
                    to,
                    kind: e.kind.clone(),
                    total: 1,
                    last_seen: e.ts,
                });
        }
    }

    for edge in edge_map.values() {
        if let Some(a) = nodes.get_mut(&edge.from) {
            a.degree = a.degree.saturating_add(edge.total);
        }
        if let Some(b) = nodes.get_mut(&edge.to) {
            b.degree = b.degree.saturating_add(edge.total);
        }
    }

    let mut node_list = nodes.into_values().collect::<Vec<_>>();
    node_list.sort_by_key(|n| n.last_seen);
    let mut edge_list = edge_map.into_values().collect::<Vec<_>>();
    edge_list.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

    Ok(TopologyResponse {
        nodes: node_list,
        edges: edge_list,
    })
}

fn relation_from_payload(event: &MonitorEvent) -> Option<(String, String)> {
    if event.source != EventSource::Network {
        return None;
    }

    let payload = event.payload.as_object()?;
    let peer = payload
        .get("peer")
        .and_then(Value::as_str)
        .map(ToString::to_string)?;
    let (kind, maybe_target) = (event.kind.as_str(), payload.get("target"));
    let target = match maybe_target.and_then(Value::as_str) {
        Some(v) => v.to_string(),
        None => event.node_id.clone(),
    };

    match kind {
        "dht_connection_established" | "dht_connection_closed" | "dht_message_received" => {
            Some((event.node_id.clone(), peer))
        }
        "dht_message_to" | "dht_fallback_send" | "udp_broadcast" => {
            Some((event.node_id.clone(), target))
        }
        _ => Some((event.node_id.clone(), peer)),
    }
}

async fn read_events(path: PathBuf, query: EventQuery) -> Result<Vec<MonitorEvent>> {
    let raw = match fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(err) => {
            return Err(anyhow::anyhow!(
                "read monitor file {}: {err}",
                path.display()
            ))
        }
    };

    let mut events = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<MonitorEvent>(line) {
            if let Some(kind) = &query.kind {
                if event.kind != *kind {
                    continue;
                }
            }
            if let Some(source) = &query.source {
                if event.source.to_string() != *source {
                    continue;
                }
            }
            events.push(event);
        }
    }

    if let Some(limit) = query.limit {
        let limit = limit.min(events.len());
        if events.len() > limit {
            events.drain(0..events.len() - limit);
        }
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::{make_router_with_auth, parse_topology, ApiAuthConfig, EventQuery, WebState};
    use crate::monitor::{EventSource, MonitorEvent};
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    }

    fn make_event(kind: &str, source: EventSource, node_id: &str, peer: &str) -> MonitorEvent {
        MonitorEvent {
            ts: ts(),
            node_id: node_id.to_string(),
            kind: kind.to_string(),
            source,
            payload: json!({ "peer": peer, "target": format!("to-{peer}") }),
        }
    }

    #[tokio::test]
    async fn topology_from_events() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-web-topology-{}", uuid::Uuid::new_v4()));
        let file = dir.join("events.ndjson");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create tmp dir");

        let events = vec![
            make_event(
                "dht_connection_established",
                EventSource::Network,
                "node-a",
                "node-b",
            ),
            make_event(
                "dht_message_received",
                EventSource::Network,
                "node-a",
                "node-c",
            ),
            make_event("seed_published", EventSource::Node, "node-a", "node-z"),
        ];
        let content = events
            .into_iter()
            .map(|event| serde_json::to_string(&event))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        tokio::fs::write(&file, format!("{content}\n"))
            .await
            .expect("write events");

        let topo = parse_topology(WebState { monitor_path: file })
            .await
            .expect("parse topology");

        assert_eq!(topo.nodes.len(), 3);
        assert!(topo
            .edges
            .iter()
            .any(|e| e.from == "node-a" && e.to == "node-b"));
        assert!(topo
            .edges
            .iter()
            .any(|e| e.from == "node-a" && e.to == "node-c"));
    }

    #[tokio::test]
    async fn query_limit_keeps_last_events() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-web-query-{}", uuid::Uuid::new_v4()));
        let file = dir.join("events.ndjson");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create tmp dir");

        let events = (0..4)
            .map(|i| MonitorEvent {
                ts: 100 + i,
                node_id: "node-a".into(),
                kind: if i % 2 == 0 {
                    "dht_bootstrap".into()
                } else {
                    "seed".into()
                },
                source: EventSource::Node,
                payload: json!({}),
            })
            .collect::<Vec<_>>();
        let content = events
            .into_iter()
            .map(|event| serde_json::to_string(&event))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        tokio::fs::write(&file, format!("{content}\n"))
            .await
            .expect("write events");

        let resp = super::read_events(
            file,
            EventQuery {
                limit: Some(2),
                kind: None,
                source: None,
            },
        )
        .await
        .expect("read limited");
        assert_eq!(resp.len(), 2);
        assert_eq!(resp[0].ts, 102);
        assert_eq!(resp[1].ts, 103);
    }

    #[test]
    fn auth_config_defaults_to_protected() {
        let config = ApiAuthConfig::from_raw(None, None);
        assert!(config.required);
        assert_eq!(config.token, None);
    }

    #[test]
    fn api_rejects_unauthorized_by_default() {
        let config = ApiAuthConfig::from_raw(None, None);
        let headers = HeaderMap::new();
        assert!(!config.is_authorized(&headers));
    }

    #[test]
    fn api_accepts_bearer_token() {
        let config = ApiAuthConfig::from_raw(None, Some("secret-token".into()));
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer secret-token"),
        );
        assert!(config.is_authorized(&headers));
    }

    #[test]
    fn api_accepts_x_api_token() {
        let config = ApiAuthConfig::from_raw(None, Some("secret-token".into()));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-token", HeaderValue::from_static("secret-token"));
        assert!(config.is_authorized(&headers));
    }

    #[test]
    fn api_can_be_configured_open_via_env_flag() {
        let config = ApiAuthConfig::from_raw(Some("false".into()), None);
        let headers = HeaderMap::new();
        assert!(config.is_authorized(&headers));
    }

    #[tokio::test]
    async fn health_is_public_when_api_is_protected() {
        let _ = make_router_with_auth(
            WebState {
                monitor_path: PathBuf::from("unused.ndjson"),
            },
            ApiAuthConfig::from_raw(None, None),
        );
        assert_eq!(super::health().await, "ok");
    }
}
