use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;

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
    sender: Option<mpsc::SyncSender<MonitorEvent>>,
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
            let _ = tx.try_send(event);
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

        const MONITOR_CHANNEL_CAP: usize = 8192;
        let (tx, rx) = mpsc::sync_channel::<MonitorEvent>(MONITOR_CHANNEL_CAP);
        std::thread::spawn(move || {
            let file = OpenOptions::new().create(true).append(true).open(path);
            let mut file = match file {
                Ok(file) => file,
                Err(_) => {
                    while rx.recv().is_ok() {}
                    return;
                }
            };

            while let Ok(event) = rx.recv() {
                if let Ok(raw) = serde_json::to_string(&event) {
                    let line = format!("{raw}\n");
                    let _ = file.write_all(line.as_bytes());
                    let _ = file.flush();
                }
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
