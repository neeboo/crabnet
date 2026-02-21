use crate::model::{
    current_ts, Bid, Claim, Envelope, EnvelopeCrypto, MessageKind, NodeIdentityInfo, Seed,
    SeedStatus, TaskResult, MSG_TTL_SECONDS,
};
use anyhow::{anyhow, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use pqcrypto_dilithium::dilithium2;
use pqcrypto_kyber::kyber768;
use pqcrypto_traits::kem::{
    Ciphertext as KyberCiphertext, PublicKey as KyberPublicKey, SecretKey as KyberSecretKey,
    SharedSecret,
};
use pqcrypto_traits::sign::{
    DetachedSignature as DilithiumDetachedSignature, PublicKey as DilithiumPublicKey,
    SecretKey as DilithiumSecretKey,
};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant, SystemTime};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, Duration};
use uuid::Uuid;
use x25519_dalek_ng::{EphemeralSecret, PublicKey, StaticSecret};

const DEFAULT_SEEN_MESSAGES_CAPACITY: usize = 10_000;
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);
const LOCK_STALE_AFTER: StdDuration = StdDuration::from_secs(30);

fn default_empty_string() -> String {
    String::new()
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SeenMessagesCompat {
    New(HashMap<String, u64>),
    Legacy(HashSet<String>),
}

fn deserialize_seen_messages<'de, D>(
    deserializer: D,
) -> std::result::Result<HashMap<String, u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<SeenMessagesCompat>::deserialize(deserializer)?;
    Ok(match value {
        Some(SeenMessagesCompat::New(entries)) => entries,
        Some(SeenMessagesCompat::Legacy(entries)) => {
            entries.into_iter().map(|id| (id, 0)).collect()
        }
        None => HashMap::new(),
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    pub alias: String,
    #[serde(default = "default_empty_string")]
    pub x25519_public: String,
    #[serde(default = "default_empty_string")]
    pub x25519_private: String,
    #[serde(default = "default_empty_string")]
    pub kyber_public: String,
    #[serde(default = "default_empty_string")]
    pub kyber_private: String,
    #[serde(default = "default_empty_string")]
    pub ed25519_public: String,
    #[serde(default = "default_empty_string")]
    pub ed25519_private: String,
    #[serde(default = "default_empty_string")]
    pub dilithium_public: String,
    #[serde(default = "default_empty_string")]
    pub dilithium_private: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub identity: NodeIdentity,
    pub seeds: HashMap<String, Seed>,
    pub bids: HashMap<String, Bid>,
    pub claims: HashMap<String, Claim>,
    pub results: HashMap<String, TaskResult>,
    #[serde(default, deserialize_with = "deserialize_seen_messages")]
    pub seen_messages: HashMap<String, u64>,
    #[serde(default)]
    pub peers: HashMap<String, NodeIdentityInfo>,
}

pub struct Store {
    data_dir: PathBuf,
    pub state: State,
    last_event: String,
    message_ttl_seconds: u64,
    seen_message_ttl_seconds: u64,
    seen_messages_capacity: usize,
    last_load_recovery: Option<StoreLoadRecoverySignal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLoadRecoverySignal {
    RecoveredFromBackup { primary_error: String },
    RepairedPrimaryWithoutChecksum { primary_error: String },
}

impl StoreLoadRecoverySignal {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RecoveredFromBackup { .. } => "recovered_from_backup",
            Self::RepairedPrimaryWithoutChecksum { .. } => "repaired_primary_without_checksum",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyRemoteNoopReason {
    Expired,
    SeenMessage,
    NotAddressedToThisNode,
    MissingPrivateCrypto,
    DuplicatePayload,
    DuplicateSeed,
    DuplicateBid,
    DuplicateClaim,
    DuplicateResult,
    DuplicateSettle,
    DuplicateHello,
    MalformedNodeHello,
    MalformedSeed,
    MalformedBid,
    MalformedClaim,
    MalformedResult,
    MalformedSettle,
}

impl ApplyRemoteNoopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::SeenMessage => "seen_message",
            Self::NotAddressedToThisNode => "not_addressed_to_this_node",
            Self::MissingPrivateCrypto => "missing_private_crypto",
            Self::DuplicatePayload => "duplicate_payload",
            Self::DuplicateSeed => "duplicate_seed",
            Self::DuplicateBid => "duplicate_bid",
            Self::DuplicateClaim => "duplicate_claim",
            Self::DuplicateResult => "duplicate_result",
            Self::DuplicateSettle => "duplicate_settle",
            Self::DuplicateHello => "duplicate_hello",
            Self::MalformedNodeHello => "malformed_node_hello",
            Self::MalformedSeed => "malformed_seed",
            Self::MalformedBid => "malformed_bid",
            Self::MalformedClaim => "malformed_claim",
            Self::MalformedResult => "malformed_result",
            Self::MalformedSettle => "malformed_settle",
        }
    }
}

impl Store {
    pub fn new(data_dir: PathBuf) -> Self {
        let alias = std::env::var("USER").unwrap_or_else(|_| "node".into());
        let identity = generate_identity(alias);
        let message_ttl_seconds =
            read_env_u64("CRABNET_MESSAGE_TTL_SECONDS", MSG_TTL_SECONDS).max(1);
        let seen_message_ttl_seconds =
            read_env_u64("CRABNET_SEEN_MESSAGES_TTL_SECONDS", message_ttl_seconds).max(1);
        let seen_messages_capacity = read_env_usize(
            "CRABNET_SEEN_MESSAGES_CAPACITY",
            DEFAULT_SEEN_MESSAGES_CAPACITY,
        )
        .max(1);
        let state = State {
            identity,
            seeds: HashMap::new(),
            bids: HashMap::new(),
            claims: HashMap::new(),
            results: HashMap::new(),
            seen_messages: HashMap::new(),
            peers: HashMap::new(),
        };
        Self {
            data_dir,
            state,
            last_event: "init".into(),
            message_ttl_seconds,
            seen_message_ttl_seconds,
            seen_messages_capacity,
            last_load_recovery: None,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    fn state_checksum_path(&self) -> PathBuf {
        self.data_dir.join("state.json.sha256")
    }

    fn backup_state_path(&self) -> PathBuf {
        self.data_dir.join("state.json.bak")
    }

    fn backup_checksum_path(&self) -> PathBuf {
        self.data_dir.join("state.json.bak.sha256")
    }

    fn lock_path(&self) -> PathBuf {
        self.data_dir.join("state.json.lock")
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
        self.last_load_recovery = None;
        let _lock = self.acquire_state_lock().await?;
        let state_path = self.state_path();
        let state_checksum_path = self.state_checksum_path();
        let backup_state_path = self.backup_state_path();
        let backup_checksum_path = self.backup_checksum_path();
        let primary_exists = fs::try_exists(&state_path).await?;
        let backup_exists = fs::try_exists(&backup_state_path).await?;

        if primary_exists {
            match self
                .read_state_with_checksum(&state_path, &state_checksum_path)
                .await
            {
                Ok(state) => {
                    self.state = state;
                }
                Err(primary_err) => {
                    // If checksum validation fails but primary JSON is still parseable, prefer
                    // repairing primary over rolling back to an older backup snapshot.
                    if let Ok(primary_state) = self.read_state_without_checksum(&state_path).await {
                        self.state = primary_state;
                        self.last_load_recovery =
                            Some(StoreLoadRecoverySignal::RepairedPrimaryWithoutChecksum {
                                primary_error: primary_err.to_string(),
                            });
                        let raw = serde_json::to_string_pretty(&self.state)?;
                        // Write backup first so that if the primary write fails the backup
                        // is already consistent and can be used for the next recovery.
                        self.write_snapshot(&backup_state_path, &backup_checksum_path, &raw)
                            .await?;
                        self.write_snapshot(&state_path, &state_checksum_path, &raw)
                            .await?;
                    } else if backup_exists {
                        let backup_state = self
                            .read_state_with_checksum(&backup_state_path, &backup_checksum_path)
                            .await
                            .map_err(|backup_err| {
                                anyhow!(
                                    "state load failed: primary={} backup={}",
                                    primary_err,
                                    backup_err
                                )
                            })?;
                        self.state = backup_state;
                        self.last_load_recovery =
                            Some(StoreLoadRecoverySignal::RecoveredFromBackup {
                                primary_error: primary_err.to_string(),
                            });
                        self.write_snapshot(
                            &state_path,
                            &state_checksum_path,
                            &serde_json::to_string_pretty(&self.state)?,
                        )
                        .await?;
                    } else {
                        return Err(anyhow!("state load failed: {}", primary_err));
                    }
                }
            }
        } else if backup_exists {
            let backup_state = self
                .read_state_with_checksum(&backup_state_path, &backup_checksum_path)
                .await?;
            self.state = backup_state;
            self.last_load_recovery = Some(StoreLoadRecoverySignal::RecoveredFromBackup {
                primary_error: "state.json missing".into(),
            });
            self.write_snapshot(
                &state_path,
                &state_checksum_path,
                &serde_json::to_string_pretty(&self.state)?,
            )
            .await?;
        }
        if self.state.identity.x25519_public.is_empty()
            || self.state.identity.x25519_private.is_empty()
            || self.state.identity.kyber_public.is_empty()
            || self.state.identity.kyber_private.is_empty()
            || self.state.identity.ed25519_public.is_empty()
            || self.state.identity.ed25519_private.is_empty()
            || self.state.identity.dilithium_public.is_empty()
            || self.state.identity.dilithium_private.is_empty()
        {
            let alias = self.state.identity.alias.clone();
            self.state.identity = generate_identity(alias);
        }
        if self.state.peers.is_empty() {
            self.state.peers = HashMap::new();
        }
        let now = current_ts();
        for seen_at in self.state.seen_messages.values_mut() {
            if *seen_at == 0 {
                *seen_at = now;
            }
        }
        self.prune_seen_messages(now);
        Ok(primary_exists || backup_exists)
    }

    pub async fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).await?;
        let _lock = self.acquire_state_lock().await?;
        let raw = serde_json::to_string_pretty(&self.state)?;
        self.write_snapshot(&self.state_path(), &self.state_checksum_path(), &raw)
            .await?;
        self.write_snapshot(
            &self.backup_state_path(),
            &self.backup_checksum_path(),
            &raw,
        )
        .await?;
        Ok(())
    }

    pub fn message_ttl_seconds(&self) -> u64 {
        self.message_ttl_seconds
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_message_ttl_seconds(&mut self, ttl_seconds: u64) {
        self.message_ttl_seconds = ttl_seconds.max(1);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_seen_message_policy(&mut self, ttl_seconds: u64, capacity: usize) {
        self.seen_message_ttl_seconds = ttl_seconds.max(1);
        self.seen_messages_capacity = capacity.max(1);
        self.prune_seen_messages(current_ts());
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn last_load_recovery(&self) -> Option<&StoreLoadRecoverySignal> {
        self.last_load_recovery.as_ref()
    }

    pub fn take_load_recovery(&mut self) -> Option<StoreLoadRecoverySignal> {
        self.last_load_recovery.take()
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

    #[allow(dead_code)]
    pub async fn apply_remote(&mut self, message: Envelope) -> Result<bool> {
        let (changed, _) = self.apply_remote_with_reason(message).await?;
        Ok(changed)
    }

    pub async fn apply_remote_with_reason(
        &mut self,
        message: Envelope,
    ) -> Result<(bool, Option<ApplyRemoteNoopReason>)> {
        let now = current_ts();
        if now.saturating_sub(message.meta.ts) > self.message_ttl_seconds {
            return Ok((false, Some(ApplyRemoteNoopReason::Expired)));
        }
        let msg_id = message.meta.id.clone();

        if let Err(err) = self.verify_envelope(&message) {
            return Err(anyhow!("invalid envelope signature {msg_id}: {err}"));
        }

        let message = match self.unwrap_encrypted(message) {
            Ok(msg) => msg,
            Err(err) => return Err(anyhow!("unable to unwrap message {msg_id}: {err}")),
        };
        let message = match message {
            Some(msg) => msg,
            None => {
                if self.state.identity.x25519_private.is_empty()
                    || self.state.identity.kyber_private.is_empty()
                {
                    return Ok((false, Some(ApplyRemoteNoopReason::MissingPrivateCrypto)));
                }
                return Ok((false, Some(ApplyRemoteNoopReason::NotAddressedToThisNode)));
            }
        };

        if !self.record_seen_message(message.meta.id.clone(), now) {
            return Ok((false, Some(ApplyRemoteNoopReason::SeenMessage)));
        }

        let value = message.payload;
        let changed = match message.meta.kind {
            MessageKind::NodeHello => {
                let identity = match payload_value::<crate::model::NodeIdentityInfo>(
                    &value,
                    "identity",
                    ApplyRemoteNoopReason::MalformedNodeHello,
                ) {
                    Ok(identity) => identity,
                    Err(reason) => return Ok((false, Some(reason))),
                };
                if self.upsert_peer_identity(identity) {
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateHello)));
                }
            }
            MessageKind::SeedCreated => {
                let seed = match payload_value::<Seed>(
                    &value,
                    "seed",
                    ApplyRemoteNoopReason::MalformedSeed,
                ) {
                    Ok(seed) => seed,
                    Err(reason) => return Ok((false, Some(reason))),
                };
                if !self.state.seeds.contains_key(&seed.id) {
                    self.state.seeds.insert(seed.id.clone(), seed);
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateSeed)));
                }
            }
            MessageKind::BidSubmitted => {
                let bid = match payload_value::<Bid>(
                    &value,
                    "bid",
                    ApplyRemoteNoopReason::MalformedBid,
                ) {
                    Ok(bid) => bid,
                    Err(reason) => return Ok((false, Some(reason))),
                };
                if !self.state.bids.contains_key(&bid.id) {
                    self.state.bids.insert(bid.id.clone(), bid);
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateBid)));
                }
            }
            MessageKind::ClaimCreated => {
                let claim = match payload_value::<Claim>(
                    &value,
                    "claim",
                    ApplyRemoteNoopReason::MalformedClaim,
                ) {
                    Ok(claim) => claim,
                    Err(reason) => return Ok((false, Some(reason))),
                };
                if !self.state.claims.contains_key(&claim.id) {
                    self.state.claims.insert(claim.id.clone(), claim.clone());
                    if let Some(seed) = self.state.seeds.get_mut(&claim.seed_id) {
                        seed.claim_id = Some(claim.id.clone());
                        seed.claimed_by = Some(claim.worker);
                        seed.status = SeedStatus::Claimed;
                    }
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateClaim)));
                }
            }
            MessageKind::TaskResult => {
                let result = match payload_value::<TaskResult>(
                    &value,
                    "result",
                    ApplyRemoteNoopReason::MalformedResult,
                ) {
                    Ok(result) => result,
                    Err(reason) => return Ok((false, Some(reason))),
                };
                if !self.state.results.contains_key(&result.id) {
                    self.state.results.insert(result.id.clone(), result.clone());
                    if let Some(seed) = self.state.seeds.get_mut(&result.seed_id) {
                        seed.result_id = Some(result.id);
                        seed.status = SeedStatus::Done;
                    }
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateResult)));
                }
            }
            MessageKind::TaskSettle => {
                let remote = match payload_value::<Seed>(
                    &value,
                    "seed",
                    ApplyRemoteNoopReason::MalformedSettle,
                ) {
                    Ok(seed) => seed,
                    Err(reason) => return Ok((false, Some(reason))),
                };
                if let Some(local) = self.state.seeds.get_mut(&remote.id) {
                    let before_status = local.status.clone();
                    let before_claim_id = local.claim_id.clone();
                    let before_claimed_by = local.claimed_by.clone();
                    let before_result_id = local.result_id.clone();
                    if local.status != remote.status {
                        local.status = remote.status;
                    }
                    if remote.result_id.is_some() && local.result_id != remote.result_id {
                        local.result_id = remote.result_id;
                    }
                    if remote.claim_id.is_some() && local.claim_id != remote.claim_id {
                        local.claim_id = remote.claim_id;
                    }
                    if remote.claimed_by.is_some() && local.claimed_by != remote.claimed_by {
                        local.claimed_by = remote.claimed_by;
                    }
                    if local.status != before_status
                        || local.claim_id != before_claim_id
                        || local.claimed_by != before_claimed_by
                        || local.result_id != before_result_id
                    {
                        true
                    } else {
                        return Ok((false, Some(ApplyRemoteNoopReason::DuplicateSettle)));
                    }
                } else {
                    self.state.seeds.insert(remote.id.clone(), remote);
                    true
                }
            }
        };

        if changed {
            self.last_event = format!("{:?} {}", message.meta.kind, message.meta.id);
            return Ok((true, None));
        }
        Ok((false, Some(ApplyRemoteNoopReason::DuplicatePayload)))
    }

    pub fn sign_envelope(&self, message: &mut Envelope) -> Result<()> {
        if message.meta.sender_identity.is_none() {
            message.meta.sender_identity = Some(self.self_identity_info());
        }
        let data = signed_payload(message)?;
        let ed25519_signature = self.sign_ed25519(&data)?;
        let dilithium_signature = self.sign_dilithium2(&data)?;
        message.meta.ed25519_signature = ed25519_signature.clone();
        message.meta.dilithium_signature = dilithium_signature;
        message.meta.signature = ed25519_signature;
        Ok(())
    }

    pub fn verify_envelope(&mut self, message: &Envelope) -> Result<()> {
        if message.meta.from == self.node_id() {
            return Ok(());
        }
        let data = signed_payload(message)?;
        match message.meta.kind {
            MessageKind::NodeHello => {
                let value = message
                    .payload
                    .get("identity")
                    .cloned()
                    .ok_or_else(|| anyhow!("node hello missing identity"))?;
                let declared: NodeIdentityInfo = serde_json::from_value(value)?;
                if declared.node_id != message.meta.from {
                    return Err(anyhow!(
                        "node hello sender mismatch: {} != {}",
                        message.meta.from,
                        declared.node_id
                    ));
                }
                let signer = message
                    .meta
                    .sender_identity
                    .clone()
                    .unwrap_or_else(|| declared.clone());
                self.verify_with_identity(&data, &signer, message)?;
                if declared.ed25519_public != signer.ed25519_public
                    || declared.dilithium_public != signer.dilithium_public
                    || declared.node_id != signer.node_id
                    || declared.x25519_public != signer.x25519_public
                    || declared.kyber_public != signer.kyber_public
                {
                    return Err(anyhow!("node hello identity mismatch"));
                }
            }
            _ => {
                let peer = self
                    .state
                    .peers
                    .get(&message.meta.from)
                    .cloned()
                    .or_else(|| {
                        message
                            .meta
                            .sender_identity
                            .clone()
                            .filter(|id| id.node_id == message.meta.from)
                    });
                let identity = peer.ok_or_else(|| {
                    anyhow!(
                        "unknown peer {}: missing sender_identity in message",
                        message.meta.from
                    )
                })?;
                self.verify_with_identity(&data, &identity, message)?;
                let _ = self.upsert_peer_identity(identity);
            }
        }
        Ok(())
    }

    pub fn last_event_desc(&self) -> &str {
        &self.last_event
    }

    async fn acquire_state_lock(&self) -> Result<StateLockGuard> {
        let path = self.lock_path();
        let start = Instant::now();
        loop {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    let note = format!("pid={} ts={}\n", std::process::id(), current_ts());
                    file.write_all(note.as_bytes()).await?;
                    file.sync_all().await?;
                    return Ok(StateLockGuard { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path).await? {
                        let _ = fs::remove_file(&path).await;
                        continue;
                    }
                    if start.elapsed() > LOCK_WAIT_TIMEOUT {
                        return Err(anyhow!(
                            "timed out waiting for store lock {}",
                            path.display()
                        ));
                    }
                    sleep(LOCK_RETRY_DELAY).await;
                }
                Err(err) => {
                    return Err(anyhow!("create lock file {}: {}", path.display(), err));
                }
            }
        }
    }

    async fn write_snapshot(&self, path: &Path, checksum_path: &Path, raw: &str) -> Result<()> {
        let temp_path =
            path.with_file_name(format!(".{}.{}.tmp", display_name(path), Uuid::new_v4()));
        let temp_checksum_path = checksum_path.with_file_name(format!(
            ".{}.{}.tmp",
            display_name(checksum_path),
            Uuid::new_v4()
        ));
        let checksum = sha256_hex(raw.as_bytes());

        {
            let mut file = File::create(&temp_path)
                .await
                .map_err(|e| anyhow!("create temp state file {}: {}", temp_path.display(), e))?;
            file.write_all(raw.as_bytes()).await?;
            file.sync_all()
                .await
                .map_err(|e| anyhow!("flush temp state file {}: {}", temp_path.display(), e))?;
        }

        if let Err(e) = fs::rename(&temp_path, path).await {
            // Best-effort cleanup: ignore removal errors to avoid masking the original rename error.
            let _ = fs::remove_file(&temp_path).await;
            return Err(anyhow!(
                "rename temp state file {} -> {}: {}",
                temp_path.display(),
                path.display(),
                e
            ));
        }

        let persisted = fs::read_to_string(path).await?;
        let persisted_checksum = sha256_hex(persisted.as_bytes());
        if persisted_checksum != checksum {
            return Err(anyhow!(
                "state durability check failed for {}: expected {} got {}",
                path.display(),
                checksum,
                persisted_checksum
            ));
        }

        {
            let mut file = File::create(&temp_checksum_path).await.map_err(|e| {
                anyhow!(
                    "create temp checksum file {}: {}",
                    temp_checksum_path.display(),
                    e
                )
            })?;
            file.write_all(format!("{checksum}\n").as_bytes()).await?;
            file.sync_all().await.map_err(|e| {
                anyhow!(
                    "flush temp checksum file {}: {}",
                    temp_checksum_path.display(),
                    e
                )
            })?;
        }
        if let Err(e) = fs::rename(&temp_checksum_path, checksum_path).await {
            // Best-effort cleanup: ignore removal errors to avoid masking the original rename error.
            let _ = fs::remove_file(&temp_checksum_path).await;
            return Err(anyhow!(
                "rename temp checksum file {} -> {}: {}",
                temp_checksum_path.display(),
                checksum_path.display(),
                e
            ));
        }

        Ok(())
    }

    async fn read_state_with_checksum(&self, path: &Path, checksum_path: &Path) -> Result<State> {
        let raw = fs::read_to_string(path)
            .await
            .map_err(|err| anyhow!("read {}: {}", path.display(), err))?;
        if fs::try_exists(checksum_path).await? {
            let expected = fs::read_to_string(checksum_path)
                .await
                .map_err(|err| anyhow!("read {}: {}", checksum_path.display(), err))?;
            let expected = expected.trim();
            if expected.is_empty() {
                return Err(anyhow!("empty checksum file {}", checksum_path.display()));
            }
            let actual = sha256_hex(raw.as_bytes());
            if actual != expected {
                return Err(anyhow!(
                    "checksum mismatch for {}: expected {} got {}",
                    path.display(),
                    expected,
                    actual
                ));
            }
        }
        serde_json::from_str(&raw).map_err(|err| anyhow!("parse {}: {}", path.display(), err))
    }

    async fn read_state_without_checksum(&self, path: &Path) -> Result<State> {
        let raw = fs::read_to_string(path)
            .await
            .map_err(|err| anyhow!("read {}: {}", path.display(), err))?;
        serde_json::from_str(&raw).map_err(|err| anyhow!("parse {}: {}", path.display(), err))
    }

    fn record_seen_message(&mut self, message_id: String, now: u64) -> bool {
        self.prune_seen_messages(now);
        if self.state.seen_messages.contains_key(&message_id) {
            return false;
        }
        self.state.seen_messages.insert(message_id, now);
        self.prune_seen_messages(now);
        true
    }

    fn prune_seen_messages(&mut self, now: u64) {
        let cutoff = now.saturating_sub(self.seen_message_ttl_seconds);
        self.state
            .seen_messages
            .retain(|_, seen_at| *seen_at >= cutoff);

        if self.state.seen_messages.len() <= self.seen_messages_capacity {
            return;
        }

        let mut oldest: Vec<(String, u64)> = self
            .state
            .seen_messages
            .iter()
            .map(|(id, seen_at)| (id.clone(), *seen_at))
            .collect();
        oldest.sort_by_key(|(_, seen_at)| *seen_at);
        let overflow = oldest.len().saturating_sub(self.seen_messages_capacity);
        for (id, _) in oldest.into_iter().take(overflow) {
            self.state.seen_messages.remove(&id);
        }
    }

    fn sign_ed25519(&self, data: &[u8]) -> Result<String> {
        let private = hex_decode_exact::<{ ed25519_dalek::SECRET_KEY_LENGTH }>(
            self.state.identity.ed25519_private.as_bytes(),
        )?;
        let signing_key = SigningKey::from_bytes(&private);
        let signature: Signature = signing_key.sign(data);
        Ok(hex_encode(&signature.to_bytes()))
    }

    fn sign_dilithium2(&self, data: &[u8]) -> Result<String> {
        let secret = DilithiumSecretKey::from_bytes(
            hex_decode_bytes(self.state.identity.dilithium_private.as_bytes())?.as_slice(),
        )?;
        let signature = dilithium2::detached_sign(data, &secret);
        Ok(hex_encode(signature.as_bytes()))
    }

    fn verify_with_identity(
        &self,
        data: &[u8],
        identity: &NodeIdentityInfo,
        message: &Envelope,
    ) -> Result<()> {
        let ed25519_signature = if !message.meta.ed25519_signature.is_empty() {
            message.meta.ed25519_signature.as_str()
        } else if !message.meta.signature.is_empty() {
            message.meta.signature.as_str()
        } else {
            ""
        };
        if ed25519_signature.is_empty() {
            return Err(anyhow!("missing ed25519 signature"));
        }
        if message.meta.dilithium_signature.is_empty() {
            return Err(anyhow!("missing dilithium2 signature"));
        }
        verify_ed25519_signature(data, identity, ed25519_signature)?;
        verify_dilithium2_signature(data, identity, &message.meta.dilithium_signature)?;
        Ok(())
    }

    fn seed_bid_count(&self, seed_id: &str) -> usize {
        self.state
            .bids
            .values()
            .filter(|b| b.seed_id == seed_id)
            .count()
    }

    pub fn self_identity_info(&self) -> NodeIdentityInfo {
        NodeIdentityInfo {
            node_id: self.state.identity.node_id.clone(),
            alias: self.state.identity.alias.clone(),
            x25519_public: self.state.identity.x25519_public.clone(),
            kyber_public: self.state.identity.kyber_public.clone(),
            ed25519_public: self.state.identity.ed25519_public.clone(),
            dilithium_public: self.state.identity.dilithium_public.clone(),
        }
    }

    pub fn upsert_peer_identity(&mut self, identity: NodeIdentityInfo) -> bool {
        if identity.node_id == self.state.identity.node_id {
            return false;
        }
        if self
            .state
            .peers
            .get(&identity.node_id)
            .is_some_and(|existing| {
                existing.x25519_public == identity.x25519_public
                    && existing.kyber_public == identity.kyber_public
                    && existing.alias == identity.alias
                    && existing.ed25519_public == identity.ed25519_public
                    && existing.dilithium_public == identity.dilithium_public
            })
        {
            return false;
        }
        self.state.peers.insert(identity.node_id.clone(), identity);
        true
    }

    #[allow(dead_code)]
    pub fn clear_peer_identity(&mut self, node_id: &str) {
        self.state.peers.remove(node_id);
    }

    pub fn encrypt_for_peers(&self, mut msg: Envelope) -> Result<Envelope> {
        if self.state.peers.is_empty() || !msg.crypto.is_empty() {
            return Ok(msg);
        }

        let payload = serde_json::to_vec(&msg.payload)?;
        let mut recipients = Vec::new();
        if self.state.identity.kyber_private.is_empty()
            || self.state.identity.x25519_private.is_empty()
        {
            return Ok(msg);
        }

        for (peer_id, peer_identity) in self.state.peers.iter() {
            if peer_id == &msg.meta.from {
                continue;
            }
            if let Ok(recipient) =
                build_recipient_envelope(msg.meta.id.clone(), peer_id, peer_identity, &payload)
            {
                recipients.push(recipient);
            }
        }

        if recipients.is_empty() {
            return Ok(msg);
        }
        msg.crypto = recipients;
        msg.payload = serde_json::Value::Null;
        Ok(msg)
    }

    pub fn unwrap_encrypted(&mut self, mut msg: Envelope) -> Result<Option<Envelope>> {
        if msg.crypto.is_empty() {
            return Ok(Some(msg));
        }

        let own_node = self.node_id().to_string();
        let recipient = msg.crypto.iter().find(|item| item.to == own_node).cloned();
        if recipient.is_none() {
            return Ok(None);
        }
        let recipient = recipient.unwrap();
        if self.state.identity.x25519_private.is_empty()
            || self.state.identity.kyber_private.is_empty()
        {
            return Ok(None);
        }

        let plaintext = decrypt_payload(
            &msg.meta,
            recipient,
            &self.state.identity.x25519_private,
            &self.state.identity.kyber_private,
        )?;
        msg.crypto = Vec::new();
        msg.payload = serde_json::from_slice(&plaintext)?;
        Ok(Some(msg))
    }
}

fn build_recipient_envelope(
    msg_id: String,
    peer_id: &str,
    peer_identity: &NodeIdentityInfo,
    plaintext: &[u8],
) -> Result<EnvelopeCrypto> {
    let peer_x25519_public = {
        let bytes = hex_decode_exact::<32>(peer_identity.x25519_public.as_bytes())?;
        PublicKey::from(bytes)
    };
    let peer_kyber_public_bytes = hex_decode_bytes(peer_identity.kyber_public.as_bytes())?;
    let peer_kyber_public = KyberPublicKey::from_bytes(&peer_kyber_public_bytes)
        .map_err(|err| anyhow!("invalid peer kyber public: {err:?}"))?;
    let sender_ephemeral = EphemeralSecret::new(OsRng);
    let sender_ephemeral_public = PublicKey::from(&sender_ephemeral).to_bytes();
    let x25519_shared = sender_ephemeral.diffie_hellman(&peer_x25519_public);
    let sender_x25519_pk = hex_encode(&sender_ephemeral_public);

    let (kyber_shared, kyber_ciphertext) = kyber768::encapsulate(&peer_kyber_public);

    let session_key = derive_session_key(
        msg_id.as_bytes(),
        &x25519_shared.to_bytes(),
        &kyber_shared.as_bytes(),
    )?;
    let cipher = ChaCha20Poly1305::new_from_slice(&session_key)?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            &nonce.into(),
            Payload {
                msg: plaintext,
                aad: msg_id.as_bytes(),
            },
        )
        .map_err(|err| anyhow!("encrypt payload failed: {err}"))?;
    Ok(EnvelopeCrypto {
        to: peer_id.to_string(),
        x25519_ephemeral_pk: sender_x25519_pk,
        kyber_ct: hex_encode(kyber_ciphertext.as_bytes()),
        nonce: hex_encode(&nonce),
        ciphertext: hex_encode(&ciphertext),
    })
}

fn decrypt_payload(
    meta: &crate::model::EnvelopeMeta,
    recipient: EnvelopeCrypto,
    self_x25519_private: &str,
    self_kyber_private: &str,
) -> Result<Vec<u8>> {
    let x25519_secret = StaticSecret::from(hex_decode_exact::<32>(self_x25519_private.as_bytes())?);
    let peer_ephemeral_pk = PublicKey::from(hex_decode_exact::<32>(
        recipient.x25519_ephemeral_pk.as_bytes(),
    )?);
    let x25519_shared = x25519_secret.diffie_hellman(&peer_ephemeral_pk).to_bytes();

    let kyber_secret = {
        let kyber_secret_bytes = hex_decode_bytes(self_kyber_private.as_bytes())?;
        KyberSecretKey::from_bytes(&kyber_secret_bytes)
            .map_err(|err| anyhow!("invalid local kyber private: {err:?}"))?
    };
    let kyber_ciphertext =
        KyberCiphertext::from_bytes(&hex_decode_bytes(recipient.kyber_ct.as_bytes())?)?;
    let kyber_shared = kyber768::decapsulate(&kyber_ciphertext, &kyber_secret);

    let session_key =
        derive_session_key(meta.id.as_bytes(), &x25519_shared, &kyber_shared.as_bytes())?;

    let cipher = ChaCha20Poly1305::new_from_slice(&session_key)?;
    let nonce = nonce_from_hex(&recipient.nonce)?;
    let plaintext = cipher
        .decrypt(
            nonce.as_slice().into(),
            Payload {
                msg: &hex_decode_bytes(recipient.ciphertext.as_bytes())?,
                aad: meta.id.as_bytes(),
            },
        )
        .map_err(|err| anyhow!("decrypt failed: {err}"))?;
    Ok(plaintext)
}

fn derive_session_key(
    msg_id: &[u8],
    x25519_shared: &[u8],
    kyber_shared: &[u8],
) -> Result<[u8; 32]> {
    let mut ikm = Vec::new();
    ikm.extend_from_slice(x25519_shared);
    ikm.extend_from_slice(kyber_shared);
    let hkdf = Hkdf::<Sha256>::new(Some(b"crabnet-hybrid-v1"), &ikm);
    let mut out = [0u8; 32];
    hkdf.expand(msg_id, &mut out)
        .map_err(|err| anyhow!("hkdf expand: {err}"))?;
    Ok(out)
}

fn nonce_from_hex(hex: &str) -> Result<[u8; 12]> {
    let decoded = hex::decode(hex).map_err(|err| anyhow!("invalid nonce: {err}"))?;
    if decoded.len() != 12 {
        return Err(anyhow!("invalid nonce length: {}", decoded.len()));
    }
    let mut out = [0u8; 12];
    out.copy_from_slice(&decoded);
    Ok(out)
}

fn hex_decode_exact<const N: usize>(value: &[u8]) -> Result<[u8; N]> {
    let decoded = hex::decode(value).map_err(|err| anyhow!("invalid hex: {err}"))?;
    if decoded.len() != N {
        return Err(anyhow!(
            "invalid key length {} expected {}",
            decoded.len(),
            N
        ));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&decoded);
    Ok(out)
}

fn hex_decode_bytes(value: &[u8]) -> Result<Vec<u8>> {
    let decoded = hex::decode(value).map_err(|err| anyhow!("invalid hex: {err}"))?;
    Ok(decoded)
}

fn hex_encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

fn signed_payload(message: &Envelope) -> Result<Vec<u8>> {
    let mut unsigned = message.clone();
    clear_signature_fields(&mut unsigned);
    serde_json::to_vec(&unsigned).map_err(|err| anyhow!("serialize signed payload: {err}"))
}

fn clear_signature_fields(message: &mut Envelope) {
    message.meta.signature.clear();
    message.meta.ed25519_signature.clear();
    message.meta.dilithium_signature.clear();
}

fn verify_ed25519_signature(
    data: &[u8],
    identity: &NodeIdentityInfo,
    signature: &str,
) -> Result<()> {
    let sig_bytes = hex_decode_bytes(signature.as_bytes())?;
    if sig_bytes.len() != ed25519_dalek::SIGNATURE_LENGTH {
        return Err(anyhow!(
            "invalid ed25519 signature length: {}",
            sig_bytes.len()
        ));
    }
    let mut signature_bytes = [0u8; ed25519_dalek::SIGNATURE_LENGTH];
    signature_bytes.copy_from_slice(&sig_bytes);
    let signature = ed25519_dalek::Signature::from_slice(&signature_bytes)
        .map_err(|_| anyhow!("invalid ed25519 signature"))?;
    let public = VerifyingKey::from_bytes(&hex_decode_exact::<
        { ed25519_dalek::PUBLIC_KEY_LENGTH },
    >(identity.ed25519_public.as_bytes())?)?;
    public
        .verify(data, &signature)
        .map_err(|err| anyhow!("ed25519 signature verify failed: {err}"))?;
    Ok(())
}

fn verify_dilithium2_signature(
    data: &[u8],
    identity: &NodeIdentityInfo,
    signature: &str,
) -> Result<()> {
    let signature = hex_decode_bytes(signature.as_bytes())?;
    let signature = DilithiumDetachedSignature::from_bytes(&signature)
        .map_err(|err| anyhow!("invalid dilithium signature: {err:?}"))?;
    let public =
        DilithiumPublicKey::from_bytes(&hex_decode_bytes(identity.dilithium_public.as_bytes())?)
            .map_err(|err| anyhow!("invalid peer dilithium public: {err:?}"))?;
    dilithium2::verify_detached_signature(&signature, data, &public)
        .map_err(|err| anyhow!("dilithium signature verify failed: {err:?}"))?;
    Ok(())
}

fn generate_identity(alias: String) -> NodeIdentity {
    let x25519_private = StaticSecret::new(OsRng);
    let x25519_public = PublicKey::from(&x25519_private).to_bytes();

    let (kyber_public, kyber_private) = {
        let (public, private) = kyber768::keypair();
        (
            hex_encode(public.as_bytes()),
            hex_encode(private.as_bytes()),
        )
    };

    let mut ed25519_private = [0u8; ed25519_dalek::SECRET_KEY_LENGTH];
    OsRng.fill_bytes(&mut ed25519_private);
    let ed25519_signing = SigningKey::from_bytes(&ed25519_private);
    let ed25519_public = ed25519_signing.verifying_key().to_bytes();

    let (dilithium_public, dilithium_private) = {
        let (public, private) = dilithium2::keypair();
        (public.as_bytes().to_vec(), private.as_bytes().to_vec())
    };

    NodeIdentity {
        node_id: format!("node-{}", Uuid::new_v4()),
        alias,
        x25519_public: hex_encode(x25519_public.as_ref()),
        x25519_private: hex_encode(&x25519_private.to_bytes()),
        kyber_public,
        kyber_private,
        ed25519_public: hex_encode(&ed25519_public),
        ed25519_private: hex_encode(&ed25519_private),
        dilithium_public: hex_encode(&dilithium_public),
        dilithium_private: hex_encode(&dilithium_private),
    }
}

struct StateLockGuard {
    path: PathBuf,
}

impl Drop for StateLockGuard {
    fn drop(&mut self) {
        // Intentional synchronous best-effort cleanup: Drop cannot be async, so
        // std::fs::remove_file is used directly. Failures are silently ignored by design.
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn lock_is_stale(path: &Path) -> Result<bool> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(anyhow!("stat lock file {}: {}", path.display(), err)),
    };
    let modified = metadata
        .modified()
        .map_err(|err| anyhow!("read lock mtime {}: {}", path.display(), err))?;
    let age = match SystemTime::now().duration_since(modified) {
        Ok(age) => age,
        Err(_) => return Ok(false),
    };
    Ok(age > LOCK_STALE_AFTER)
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn payload_value<T: serde::de::DeserializeOwned>(
    payload: &serde_json::Value,
    key: &str,
    malformed_reason: ApplyRemoteNoopReason,
) -> std::result::Result<T, ApplyRemoteNoopReason> {
    let value = payload.get(key).cloned().ok_or(malformed_reason)?;
    serde_json::from_value(value).map_err(|_| malformed_reason)
}

fn read_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default)
}

fn read_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{current_ts, ApplyRemoteNoopReason, Envelope, Seed, SeedStatus, Store};
    use anyhow::Result;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tokio::fs;
    use tokio::time::{sleep, Duration};
    use uuid::Uuid;

    fn seed_with_id(id: &str) -> Seed {
        Seed {
            id: id.into(),
            title: id.into(),
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
        }
    }

    fn checksum_hex(raw: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        hex::encode(hasher.finalize())
    }

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

        let mut viewer = Store::new(dir.clone());
        viewer.load().await?;
        assert!(viewer.state.seeds.len() <= 1);
        assert!(
            viewer.state.seeds.get("left-seed").is_some()
                || viewer.state.seeds.get("right-seed").is_some()
        );
        assert!(!fs::try_exists(dir.join("state.json.lock")).await?);
        Ok(())
    }

    #[tokio::test]
    async fn save_overwrites_and_loads_without_corruption() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-store-save-{}", Uuid::new_v4().to_string()));

        let mut store = Store::new(dir.clone());
        store.load().await?;
        store
            .state
            .seen_messages
            .insert("msg-1".into(), current_ts());
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

    #[tokio::test]
    async fn node_hello_verify_does_not_mutate_peer_state_before_apply() -> Result<()> {
        let mut receiver_dir = std::env::temp_dir();
        receiver_dir.push(format!(
            "crabnet-node-hello-verify-{}",
            Uuid::new_v4().to_string()
        ));
        let mut sender_dir = std::env::temp_dir();
        sender_dir.push(format!(
            "crabnet-node-hello-sender-{}",
            Uuid::new_v4().to_string()
        ));

        let mut receiver = Store::new(receiver_dir);
        let mut sender = Store::new(sender_dir);
        receiver.load().await?;
        sender.load().await?;
        let mut seed_msg = Envelope::node_hello(sender.node_id(), sender.self_identity_info());
        sender.sign_envelope(&mut seed_msg)?;
        let sender_id = sender.node_id().to_string();

        assert!(receiver.state.peers.get(&sender_id).is_none());

        receiver.verify_envelope(&seed_msg)?;
        assert!(receiver.state.peers.get(&sender_id).is_none());

        let (changed, noop) = receiver.apply_remote_with_reason(seed_msg).await?;
        assert!(changed);
        assert!(noop.is_none());
        assert!(receiver.state.peers.get(&sender_id).is_some());

        Ok(())
    }

    #[tokio::test]
    async fn node_hello_verify_rejects_missing_signature_parts() -> Result<()> {
        let mut receiver_dir = std::env::temp_dir();
        receiver_dir.push(format!(
            "crabnet-node-hello-verify-missing-{}",
            Uuid::new_v4().to_string()
        ));
        let mut sender_dir = std::env::temp_dir();
        sender_dir.push(format!(
            "crabnet-node-hello-sender-missing-{}",
            Uuid::new_v4().to_string()
        ));
        let mut sender = Store::new(sender_dir);
        let mut receiver = Store::new(receiver_dir);
        sender.load().await?;
        receiver.load().await?;

        let mut hello = Envelope::node_hello(sender.node_id(), sender.self_identity_info());
        sender.sign_envelope(&mut hello)?;
        hello.meta.ed25519_signature.clear();
        hello.meta.signature.clear();

        assert!(receiver.verify_envelope(&hello).is_err());

        Ok(())
    }

    #[tokio::test]
    async fn save_persists_checksum_and_backup_files() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-store-checksum-{}", Uuid::new_v4()));

        let mut store = Store::new(dir.clone());
        store.load().await?;
        store.add_seed(seed_with_id("seed-checksum"));
        store.save().await?;

        let state_path = dir.join("state.json");
        let checksum_path = dir.join("state.json.sha256");
        let backup_path = dir.join("state.json.bak");
        let backup_checksum_path = dir.join("state.json.bak.sha256");
        assert!(fs::try_exists(&state_path).await?);
        assert!(fs::try_exists(&checksum_path).await?);
        assert!(fs::try_exists(&backup_path).await?);
        assert!(fs::try_exists(&backup_checksum_path).await?);

        let state_raw = fs::read_to_string(&state_path).await?;
        let checksum = fs::read_to_string(&checksum_path).await?;
        assert_eq!(checksum.trim(), checksum_hex(&state_raw));

        let backup_raw = fs::read_to_string(&backup_path).await?;
        let backup_checksum = fs::read_to_string(&backup_checksum_path).await?;
        assert_eq!(backup_checksum.trim(), checksum_hex(&backup_raw));

        Ok(())
    }

    #[tokio::test]
    async fn load_recovers_from_backup_when_primary_corrupt() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-store-recovery-{}", Uuid::new_v4()));

        let mut writer = Store::new(dir.clone());
        writer.load().await?;
        writer.add_seed(seed_with_id("seed-recover"));
        writer.save().await?;

        fs::write(dir.join("state.json"), "{\"broken\":true}").await?;

        let mut reader = Store::new(dir);
        let loaded = reader.load().await?;
        assert!(loaded);
        assert_eq!(
            reader.last_load_recovery().map(|signal| signal.as_str()),
            Some("recovered_from_backup")
        );
        assert!(reader.state.seeds.contains_key("seed-recover"));
        Ok(())
    }

    #[tokio::test]
    async fn apply_remote_uses_configurable_message_ttl() -> Result<()> {
        let mut receiver_dir = std::env::temp_dir();
        receiver_dir.push(format!("crabnet-ttl-receiver-{}", Uuid::new_v4()));
        let mut sender_dir = std::env::temp_dir();
        sender_dir.push(format!("crabnet-ttl-sender-{}", Uuid::new_v4()));

        let mut receiver = Store::new(receiver_dir);
        let mut sender = Store::new(sender_dir);
        receiver.load().await?;
        sender.load().await?;
        receiver.set_message_ttl_seconds(1);

        let mut msg = Envelope::seed_created(sender.node_id(), seed_with_id("seed-ttl"));
        sender.sign_envelope(&mut msg)?;
        msg.meta.ts = current_ts().saturating_sub(5);
        let (changed, noop) = receiver.apply_remote_with_reason(msg).await?;
        assert!(!changed);
        assert_eq!(noop, Some(ApplyRemoteNoopReason::Expired));

        Ok(())
    }

    #[tokio::test]
    async fn apply_remote_reports_explicit_malformed_reason() -> Result<()> {
        let mut receiver_dir = std::env::temp_dir();
        receiver_dir.push(format!("crabnet-malformed-receiver-{}", Uuid::new_v4()));
        let mut sender_dir = std::env::temp_dir();
        sender_dir.push(format!("crabnet-malformed-sender-{}", Uuid::new_v4()));

        let mut receiver = Store::new(receiver_dir);
        let mut sender = Store::new(sender_dir);
        receiver.load().await?;
        sender.load().await?;

        let mut msg = Envelope::seed_created(sender.node_id(), seed_with_id("seed-malformed"));
        msg.payload = json!({"not_seed": true});
        sender.sign_envelope(&mut msg)?;
        let (changed, noop) = receiver.apply_remote_with_reason(msg).await?;
        assert!(!changed);
        assert_eq!(noop, Some(ApplyRemoteNoopReason::MalformedSeed));
        Ok(())
    }

    #[tokio::test]
    async fn seen_messages_prunes_by_ttl_and_capacity() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-seen-window-{}", Uuid::new_v4()));
        let mut store = Store::new(dir);
        store.load().await?;
        store.set_seen_message_policy(10, 3);

        assert!(store.record_seen_message("msg-a".into(), 100));
        assert!(store.record_seen_message("msg-b".into(), 101));
        assert!(store.record_seen_message("msg-c".into(), 102));
        assert!(store.record_seen_message("msg-d".into(), 103));
        assert_eq!(store.state.seen_messages.len(), 3);
        assert!(!store.state.seen_messages.contains_key("msg-a"));
        assert!(store.state.seen_messages.contains_key("msg-d"));

        store.state.seen_messages.insert("stale".into(), 10);
        store.prune_seen_messages(30);
        assert!(!store.state.seen_messages.contains_key("stale"));

        Ok(())
    }

    #[tokio::test]
    async fn write_snapshot_cleans_temp_files_on_rename_failure() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-temp-cleanup-{}", Uuid::new_v4()));

        let mut store = Store::new(dir.clone());
        store.load().await?;
        store.add_seed(seed_with_id("seed-cleanup"));

        // Create a read-only directory to force rename failure
        let readonly_dir = dir.join("readonly");
        fs::create_dir(&readonly_dir).await?;

        // First save succeeds (creates the files)
        let mut writable_store = Store::new(readonly_dir.clone());
        writable_store.load().await?;
        writable_store.add_seed(seed_with_id("seed-readonly"));
        writable_store.save().await?;

        // Make directory read-only (Unix-only, but test will skip gracefully on Windows)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&readonly_dir).await?.permissions();
            perms.set_readonly(true);
            fs::set_permissions(&readonly_dir, perms).await?;

            // Attempt to save again - should fail to rename due to read-only directory
            let result = writable_store.save().await;

            // Restore permissions for cleanup
            let mut perms = fs::metadata(&readonly_dir).await?.permissions();
            perms.set_readonly(false);
            fs::set_permissions(&readonly_dir, perms).await?;

            assert!(result.is_err(), "save should fail with read-only directory");

            // Verify no orphaned .tmp files remain
            let mut entries = fs::read_dir(&readonly_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                assert!(
                    !name.to_string_lossy().contains(".tmp"),
                    "found orphaned tmp file: {:?}",
                    name
                );
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn recovery_write_order_preserves_backup_on_primary_failure() -> Result<()> {
        let mut dir = std::env::temp_dir();
        dir.push(format!("crabnet-recovery-order-{}", Uuid::new_v4()));

        let mut writer = Store::new(dir.clone());
        writer.load().await?;
        writer.add_seed(seed_with_id("seed-initial"));
        writer.save().await?;

        // Verify initial backup exists
        let initial_backup = fs::read_to_string(dir.join("state.json.bak")).await?;
        assert!(initial_backup.contains("seed-initial"));

        // Update state with new content
        writer.add_seed(seed_with_id("seed-updated"));
        let updated_state = serde_json::to_string_pretty(&writer.state)?;

        // Trigger RepairedPrimaryWithoutChecksum path by writing valid JSON with wrong checksum
        fs::write(dir.join("state.json"), updated_state).await?;
        fs::write(dir.join("state.json.checksum"), "intentionally_wrong_checksum").await?;

        // Load will trigger the repair path which writes backup FIRST, then primary
        let loaded = writer.load().await?;
        assert!(loaded, "load should trigger recovery path");

        // Verify backup was updated with the new state
        let updated_backup = fs::read_to_string(dir.join("state.json.bak")).await?;
        assert!(
            updated_backup.contains("seed-updated"),
            "backup should be updated during recovery repair path"
        );

        // Verify the recovery signal is set correctly
        assert_eq!(
            writer.last_load_recovery().map(|s| s.as_str()),
            Some("repaired_primary_without_checksum")
        );

        Ok(())
    }
}
