use crate::model::{
    current_ts, Bid, Claim, Envelope, MessageKind, Seed, SeedStatus, TaskResult, MSG_TTL_SECONDS,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub alias: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub identity: NodeIdentity,
    pub seeds: HashMap<String, Seed>,
    pub bids: HashMap<String, Bid>,
    pub claims: HashMap<String, Claim>,
    pub results: HashMap<String, TaskResult>,
    #[serde(default)]
    pub seen_messages: HashSet<String>,
}

pub struct Store {
    data_dir: PathBuf,
    pub state: State,
    last_event: String,
}

impl Store {
    pub fn new(data_dir: PathBuf) -> Self {
        let alias = std::env::var("USER").unwrap_or_else(|_| "node".into());
        let state = State {
            identity: NodeIdentity {
                node_id: format!("node-{}", uuid::Uuid::new_v4()),
                alias,
            },
            seeds: HashMap::new(),
            bids: HashMap::new(),
            claims: HashMap::new(),
            results: HashMap::new(),
            seen_messages: HashSet::new(),
        };
        Self {
            data_dir,
            state,
            last_event: "init".into(),
        }
    }

    fn state_path(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn node_id(&self) -> &str {
        &self.state.identity.node_id
    }

    pub fn seeds_len(&self) -> usize {
        self.state.seeds.len()
    }
    pub fn bids_len(&self) -> usize {
        self.state.bids.len()
    }
    pub fn claims_len(&self) -> usize {
        self.state.claims.len()
    }
    pub fn results_len(&self) -> usize {
        self.state.results.len()
    }

    pub fn seeds_sorted(&self) -> Vec<&Seed> {
        let mut arr: Vec<&Seed> = self.state.seeds.values().collect();
        arr.sort_by_key(|s| s.created_at);
        arr
    }

    pub fn seed(&self, seed_id: &str) -> Option<&Seed> {
        self.state.seeds.get(seed_id)
    }

    pub async fn load(&mut self) -> Result<bool> {
        fs::create_dir_all(&self.data_dir).await?;
        let path = self.state_path();
        let has_state = fs::try_exists(&path).await?;
        if has_state {
            let raw = fs::read_to_string(&path).await?;
            self.state = serde_json::from_str(&raw)?;
        }
        if self.state.seen_messages.is_empty() {
            self.state.seen_messages = HashSet::new();
        }
        Ok(has_state)
    }

    pub async fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).await?;
        let temp_path = self
            .state_path()
            .with_file_name(format!(".state.json.{}.tmp", Uuid::new_v4()));
        let raw = serde_json::to_string_pretty(&self.state)?;
        {
            let mut file = File::create(&temp_path)
                .await
                .map_err(|e| anyhow!("create temp state file {}: {}", temp_path.display(), e))?;
            file.write_all(raw.as_bytes()).await?;
            file.sync_all()
                .await
                .map_err(|e| anyhow!("flush temp state file {}: {}", temp_path.display(), e))?;
        }
        fs::rename(&temp_path, self.state_path())
            .await
            .map_err(|e| {
                anyhow!(
                    "rename temp state file {} -> {}: {}",
                    temp_path.display(),
                    self.state_path().display(),
                    e
                )
            })?;
        Ok(())
    }

    pub fn add_seed(&mut self, seed: Seed) {
        self.state.seeds.insert(seed.id.clone(), seed);
    }

    pub fn add_bid(&mut self, seed_id: &str, price: u64, notes: String) -> Result<Bid> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let seed = self
            .state
            .seeds
            .get(seed_id)
            .ok_or_else(|| anyhow!("seed not found: {seed_id}"))?;

        if !matches!(seed.status, SeedStatus::Open) {
            return Err(anyhow!("seed not open"));
        }
        if now > seed.bid_deadline_ts {
            return Err(anyhow!("seed bidding closed"));
        }
        if price < seed.min_price {
            return Err(anyhow!("bid too low"));
        }

        let current_bids = self.seed_bid_count(seed_id);
        if seed.max_bids > 0 && current_bids >= seed.max_bids {
            return Err(anyhow!("max bids reached"));
        }

        let bid = Bid {
            id: uuid::Uuid::new_v4().to_string(),
            seed_id: seed_id.to_string(),
            bidder: self.state.identity.node_id.clone(),
            price,
            notes,
            created_at: now,
        };
        self.state.bids.insert(bid.id.clone(), bid.clone());
        Ok(bid)
    }

    pub fn claim_seed(&mut self, seed_id: &str, bid_id: &str) -> Result<Claim> {
        let now = current_ts();
        let seed = self
            .state
            .seeds
            .get_mut(seed_id)
            .ok_or_else(|| anyhow!("seed not found: {seed_id}"))?;
        if !matches!(seed.status, SeedStatus::Open) {
            return Err(anyhow!("seed not open"));
        }

        let bid = self
            .state
            .bids
            .get(bid_id)
            .ok_or_else(|| anyhow!("bid not found: {bid_id}"))?;
        if bid.seed_id != seed_id {
            return Err(anyhow!("bid does not belong to seed"));
        }

        let claim = Claim {
            id: uuid::Uuid::new_v4().to_string(),
            seed_id: seed_id.to_string(),
            bid_id: bid_id.to_string(),
            worker: bid.bidder.clone(),
            created_at: now,
        };
        seed.status = SeedStatus::Claimed;
        seed.claim_id = Some(claim.id.clone());
        seed.claimed_by = Some(claim.worker.clone());
        seed.claimed_at = Some(now);
        self.state.claims.insert(claim.id.clone(), claim.clone());
        Ok(claim)
    }

    pub fn mark_running(&mut self, seed_id: &str) -> Result<()> {
        let seed = self
            .state
            .seeds
            .get_mut(seed_id)
            .ok_or_else(|| anyhow!("seed not found"))?;
        if !matches!(seed.status, SeedStatus::Claimed) {
            return Err(anyhow!("seed not claimed"));
        }
        seed.status = SeedStatus::Running;
        Ok(())
    }

    pub fn add_result(&mut self, seed_id: &str, mut result: TaskResult) -> Result<TaskResult> {
        let seed = self
            .state
            .seeds
            .get_mut(seed_id)
            .ok_or_else(|| anyhow!("seed not found"))?;
        if !matches!(seed.status, SeedStatus::Claimed | SeedStatus::Running) {
            return Err(anyhow!("seed not running"));
        }
        result.seed_id = seed_id.to_string();
        result.worker = seed.claimed_by.clone().unwrap_or_else(|| "unknown".into());
        seed.status = SeedStatus::Done;
        seed.result_id = Some(result.id.clone());
        self.state.results.insert(result.id.clone(), result.clone());
        Ok(result)
    }

    pub fn settle_seed(&mut self, seed_id: &str, accepted: bool, note: String) -> Result<Seed> {
        let seed = self
            .state
            .seeds
            .get_mut(seed_id)
            .ok_or_else(|| anyhow!("seed not found"))?;
        seed.status = if accepted {
            SeedStatus::Accepted
        } else {
            SeedStatus::Rejected
        };
        if !note.is_empty() {
            seed.rules = note;
        }
        Ok(seed.clone())
    }

    pub async fn apply_remote(&mut self, message: Envelope) -> Result<bool> {
        let now = current_ts();
        if now.saturating_sub(message.meta.ts) > MSG_TTL_SECONDS {
            return Ok(false);
        }
        if !self.state.seen_messages.insert(message.meta.id.clone()) {
            return Ok(false);
        }

        let mut changed = false;
        let value = message.payload;
        match message.meta.kind {
            MessageKind::SeedCreated => {
                let seed = value
                    .get("seed")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed seed message"))?;
                let seed: Seed = serde_json::from_value(seed)?;
                if !self.state.seeds.contains_key(&seed.id) {
                    self.state.seeds.insert(seed.id.clone(), seed);
                    changed = true;
                }
            }
            MessageKind::BidSubmitted => {
                let bid = value
                    .get("bid")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed bid message"))?;
                let bid: Bid = serde_json::from_value(bid)?;
                if !self.state.bids.contains_key(&bid.id) {
                    self.state.bids.insert(bid.id.clone(), bid);
                    changed = true;
                }
            }
            MessageKind::ClaimCreated => {
                let claim = value
                    .get("claim")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed claim message"))?;
                let claim: Claim = serde_json::from_value(claim)?;
                if !self.state.claims.contains_key(&claim.id) {
                    self.state.claims.insert(claim.id.clone(), claim.clone());
                    if let Some(seed) = self.state.seeds.get_mut(&claim.seed_id) {
                        seed.claim_id = Some(claim.id.clone());
                        seed.claimed_by = Some(claim.worker);
                        seed.status = SeedStatus::Claimed;
                    }
                    changed = true;
                }
            }
            MessageKind::TaskResult => {
                let result = value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed result message"))?;
                let result: TaskResult = serde_json::from_value(result)?;
                if !self.state.results.contains_key(&result.id) {
                    self.state.results.insert(result.id.clone(), result.clone());
                    if let Some(seed) = self.state.seeds.get_mut(&result.seed_id) {
                        seed.result_id = Some(result.id);
                        seed.status = SeedStatus::Done;
                    }
                    changed = true;
                }
            }
            MessageKind::TaskSettle => {
                let seed = value
                    .get("seed")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed settle message"))?;
                let remote: Seed = serde_json::from_value(seed)?;
                if let Some(local) = self.state.seeds.get_mut(&remote.id) {
                    local.status = remote.status;
                    if remote.result_id.is_some() {
                        local.result_id = remote.result_id;
                    }
                    if remote.claim_id.is_some() {
                        local.claim_id = remote.claim_id;
                    }
                    if remote.claimed_by.is_some() {
                        local.claimed_by = remote.claimed_by;
                    }
                    changed = true;
                } else {
                    self.state.seeds.insert(remote.id.clone(), remote);
                    changed = true;
                }
            }
        }

        if changed {
            self.last_event = format!("{:?} {}", message.meta.kind, message.meta.id);
        }
        Ok(changed)
    }

    pub fn last_event_desc(&self) -> &str {
        &self.last_event
    }

    fn seed_bid_count(&self, seed_id: &str) -> usize {
        self.state
            .bids
            .values()
            .filter(|b| b.seed_id == seed_id)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::{current_ts, Seed, SeedStatus, Store};
    use anyhow::Result;
    use tokio::time::{sleep, Duration};
    use uuid::Uuid;

    #[tokio::test]
    async fn concurrent_saves_keep_state_json_parsable() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-store-race-{}", Uuid::new_v4().to_string()));

        let mut left = Store::new(dir.clone());
        let mut right = Store::new(dir.clone());
        left.load().await?;
        right.load().await?;

        left.add_seed(Seed {
            id: "left-seed".into(),
            title: "left".into(),
            description: "".into(),
            command: "echo left".into(),
            timeout_ms: 1,
            bid_deadline_ts: current_ts() + 10,
            exec_deadline_ms: 100,
            min_price: 1,
            max_bids: 1,
            reward: 0,
            rules: "".into(),
            status: SeedStatus::Open,
            created_by: "left".into(),
            created_at: current_ts(),
            claimed_by: None,
            claimed_at: None,
            claim_id: None,
            result_id: None,
        });
        right.add_seed(Seed {
            id: "right-seed".into(),
            title: "right".into(),
            description: "".into(),
            command: "echo right".into(),
            timeout_ms: 1,
            bid_deadline_ts: current_ts() + 10,
            exec_deadline_ms: 100,
            min_price: 1,
            max_bids: 1,
            reward: 0,
            rules: "".into(),
            status: SeedStatus::Open,
            created_by: "right".into(),
            created_at: current_ts(),
            claimed_by: None,
            claimed_at: None,
            claim_id: None,
            result_id: None,
        });

        let left_save = tokio::spawn(async move { left.save().await });
        let right_save = tokio::spawn(async move { right.save().await });
        left_save.await??;
        right_save.await??;
        // Give FS a tiny settling window on some platforms.
        sleep(Duration::from_millis(50)).await;

        let mut viewer = Store::new(dir);
        viewer.load().await?;
        assert!(viewer.state.seeds.len() <= 1);
        assert!(
            viewer.state.seeds.get("left-seed").is_some()
                || viewer.state.seeds.get("right-seed").is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn save_overwrites_and_loads_without_corruption() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-store-save-{}", Uuid::new_v4().to_string()));

        let mut store = Store::new(dir.clone());
        store.load().await?;
        store.state.seen_messages.insert("msg-1".into());
        store.add_seed(Seed {
            id: "seed-a".into(),
            title: "seed-a".into(),
            description: "".into(),
            command: "echo ok".into(),
            timeout_ms: 1000,
            bid_deadline_ts: current_ts() + 60,
            exec_deadline_ms: 3000,
            min_price: 1,
            max_bids: 2,
            reward: 0,
            rules: "".into(),
            status: SeedStatus::Open,
            created_by: "tester".into(),
            created_at: current_ts(),
            claimed_by: None,
            claimed_at: None,
            claim_id: None,
            result_id: None,
        });
        store.save().await?;
        store.add_seed(Seed {
            id: "seed-b".into(),
            title: "seed-b".into(),
            description: "".into(),
            command: "echo ok".into(),
            timeout_ms: 1000,
            bid_deadline_ts: current_ts() + 60,
            exec_deadline_ms: 3000,
            min_price: 1,
            max_bids: 2,
            reward: 0,
            rules: "".into(),
            status: SeedStatus::Open,
            created_by: "tester".into(),
            created_at: current_ts(),
            claimed_by: None,
            claimed_at: None,
            claim_id: None,
            result_id: None,
        });
        store.save().await?;

        let mut reader = Store::new(dir);
        reader.load().await?;
        assert!(reader.state.seeds.contains_key("seed-b"));
        Ok(())
    }
}
