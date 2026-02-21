use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SeedStatus {
    Open,
    Expired,
    Claimed,
    Running,
    Done,
    Accepted,
    Rejected,
    Forfeited,
}

impl std::fmt::Display for SeedStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            SeedStatus::Open => "OPEN",
            SeedStatus::Expired => "EXPIRED",
            SeedStatus::Claimed => "CLAIMED",
            SeedStatus::Running => "RUNNING",
            SeedStatus::Done => "DONE",
            SeedStatus::Accepted => "ACCEPTED",
            SeedStatus::Rejected => "REJECTED",
            SeedStatus::Forfeited => "FORFEITED",
        };
        write!(f, "{}", text)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seed {
    pub id: String,
    pub title: String,
    pub description: String,
    pub command: String,
    pub timeout_ms: u64,
    pub bid_deadline_ts: u64,
    pub exec_deadline_ms: u64,
    pub min_price: u64,
    pub max_bids: usize,
    pub reward: u64,
    pub rules: String,
    pub status: SeedStatus,
    pub created_by: String,
    pub created_at: u64,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<u64>,
    pub claim_id: Option<String>,
    pub result_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bid {
    pub id: String,
    pub seed_id: String,
    pub bidder: String,
    pub price: u64,
    pub notes: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub seed_id: String,
    pub bid_id: String,
    pub worker: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub seed_id: String,
    pub worker: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub output_hash: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeMeta {
    pub id: String,
    pub from: String,
    pub kind: MessageKind,
    pub ts: u64,
    pub nonce: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageKind {
    SeedCreated,
    BidSubmitted,
    ClaimCreated,
    TaskResult,
    TaskSettle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub meta: EnvelopeMeta,
    pub payload: Value,
}

impl Envelope {
    pub fn seed_created(from: &str, seed: Seed) -> Self {
        Self::build(
            from,
            MessageKind::SeedCreated,
            serde_json::json!({ "seed": seed }),
        )
    }

    pub fn bid_submitted(from: &str, bid: Bid) -> Self {
        Self::build(
            from,
            MessageKind::BidSubmitted,
            serde_json::json!({ "bid": bid }),
        )
    }

    pub fn claim_created(from: &str, claim: Claim) -> Self {
        Self::build(
            from,
            MessageKind::ClaimCreated,
            serde_json::json!({ "claim": claim }),
        )
    }

    pub fn task_result(from: &str, result: TaskResult) -> Self {
        Self::build(
            from,
            MessageKind::TaskResult,
            serde_json::json!({ "result": result }),
        )
    }

    pub fn task_settle(from: &str, seed: Seed) -> Self {
        Self::build(
            from,
            MessageKind::TaskSettle,
            serde_json::json!({ "seed": seed }),
        )
    }

    fn build(from: &str, kind: MessageKind, payload: Value) -> Self {
        let ts = current_ts();
        Self {
            meta: EnvelopeMeta {
                id: Uuid::new_v4().to_string(),
                from: from.to_string(),
                kind,
                ts,
                nonce: ts,
                signature: String::new(),
            },
            payload,
        }
    }
}

pub const MSG_TTL_SECONDS: u64 = 12 * 3600;

pub fn current_ts() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
