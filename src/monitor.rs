use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Node,
    Network,
}

impl std::fmt::Display for EventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Node => write!(f, "node"),
            Self::Network => write!(f, "network"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorEvent {
    pub ts: u64,
    pub node_id: String,
    pub kind: String,
    pub source: EventSource,
    pub payload: Value,
}

#[derive(Clone)]
pub struct MonitorHandle {
    node_id: String,
    sender: Option<mpsc::UnboundedSender<MonitorEvent>>,
}

impl MonitorHandle {
    pub async fn emit<V>(&self, kind: &str, source: EventSource, payload: V)
    where
        V: Serialize,
    {
        if let Some(tx) = &self.sender {
            let event = MonitorEvent {
                ts: now_unix_secs(),
                node_id: self.node_id.clone(),
                kind: kind.to_string(),
                source,
                payload: serde_json::to_value(payload).unwrap_or(Value::Null),
            };
            let _ = tx.send(event);
        }
    }

    pub fn with_path(path: Option<PathBuf>, node_id: impl Into<String>) -> Self {
        let node_id = node_id.into();
        let Some(path) = path else {
            return Self {
                node_id,
                sender: None,
            };
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<MonitorEvent>();
        tokio::spawn(async move {
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
            {
                Ok(mut file) => {
                    while let Some(event) = rx.recv().await {
                        match serde_json::to_string(&event) {
                            Ok(raw) => {
                                let line = format!("{}\n", raw);
                                let _ = file.write_all(line.as_bytes()).await;
                                let _ = file.flush().await;
                            }
                            Err(_) => {}
                        }
                    }
                }
                Err(_) => while rx.recv().await.is_some() {},
            }
        });

        Self {
            node_id,
            sender: Some(tx),
        }
    }
}

fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
