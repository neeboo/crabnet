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
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use x25519_dalek_ng::{EphemeralSecret, PublicKey, StaticSecret};

fn default_empty_string() -> String {
    String::new()
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
    #[serde(default)]
    pub seen_messages: HashSet<String>,
    #[serde(default)]
    pub peers: HashMap<String, NodeIdentityInfo>,
}

pub struct Store {
    data_dir: PathBuf,
    pub state: State,
    last_event: String,
}

#[derive(Debug, Clone, Copy)]
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
        }
    }
}

impl Store {
    pub fn new(data_dir: PathBuf) -> Self {
        let alias = std::env::var("USER").unwrap_or_else(|_| "node".into());
        let identity = generate_identity(alias);
        let state = State {
            identity,
            seeds: HashMap::new(),
            bids: HashMap::new(),
            claims: HashMap::new(),
            results: HashMap::new(),
            seen_messages: HashSet::new(),
            peers: HashMap::new(),
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
        }
        if self.state.seen_messages.is_empty() {
            self.state.seen_messages = HashSet::new();
        }
        if self.state.peers.is_empty() {
            self.state.peers = HashMap::new();
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
        if now.saturating_sub(message.meta.ts) > MSG_TTL_SECONDS {
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

        if !self.state.seen_messages.insert(message.meta.id.clone()) {
            return Ok((false, Some(ApplyRemoteNoopReason::SeenMessage)));
        }

        let value = message.payload;
        let changed = match message.meta.kind {
            MessageKind::NodeHello => {
                let identity = value
                    .get("identity")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed node hello"))?;
                let identity: crate::model::NodeIdentityInfo = serde_json::from_value(identity)?;
                if self.upsert_peer_identity(identity) {
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateHello)));
                }
            }
            MessageKind::SeedCreated => {
                let seed = value
                    .get("seed")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed seed message"))?;
                let seed: Seed = serde_json::from_value(seed)?;
                if !self.state.seeds.contains_key(&seed.id) {
                    self.state.seeds.insert(seed.id.clone(), seed);
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateSeed)));
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
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateBid)));
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
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateClaim)));
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
                    true
                } else {
                    return Ok((false, Some(ApplyRemoteNoopReason::DuplicateResult)));
                }
            }
            MessageKind::TaskSettle => {
                let seed = value
                    .get("seed")
                    .cloned()
                    .ok_or_else(|| anyhow!("malformed settle message"))?;
                let remote: Seed = serde_json::from_value(seed)?;
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

#[cfg(test)]
mod tests {
    use super::{current_ts, Envelope, Seed, SeedStatus, Store};
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
}
