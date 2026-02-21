use anyhow::Result;
use std::path::PathBuf;
use uuid::Uuid;

use crabnet_mvp::{model, run_bash_task, store::Store};

fn make_tmp_dir(prefix: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("crabnet-test-{}-{}", prefix, Uuid::new_v4()));
    dir
}

#[tokio::test]
async fn e2e_publish_bid_claim_run_settle_sync() -> Result<()> {
    let publisher_dir = make_tmp_dir("publisher");
    let worker_dir = make_tmp_dir("worker");

    let mut publisher = Store::new(publisher_dir.clone());
    let mut worker = Store::new(worker_dir.clone());
    publisher.load().await?;
    worker.load().await?;
    worker.upsert_peer_identity(publisher.self_identity_info());
    publisher.upsert_peer_identity(worker.self_identity_info());

    let now = model::current_ts();
    let seed = model::Seed {
        id: Uuid::new_v4().to_string(),
        title: "demo task".into(),
        description: "run a quick shell command".into(),
        command: "echo ok".into(),
        timeout_ms: 5000,
        bid_deadline_ts: now + 600,
        exec_deadline_ms: 5000,
        min_price: 1,
        max_bids: 4,
        reward: 0,
        rules: "e2e".into(),
        status: model::SeedStatus::Open,
        created_by: publisher.node_id().to_string(),
        created_at: now,
        claimed_by: None,
        claimed_at: None,
        claim_id: None,
        result_id: None,
    };
    publisher.add_seed(seed.clone());

    let mut seed_msg = model::Envelope::seed_created(publisher.node_id(), seed.clone());
    publisher.sign_envelope(&mut seed_msg)?;
    assert!(worker.apply_remote(seed_msg).await?);
    assert!(worker.seed(&seed.id).is_some());

    let bid = worker.add_bid(&seed.id, 1, "worker bid".into())?;
    let mut bid_msg = model::Envelope::bid_submitted(worker.node_id(), bid.clone());
    worker.sign_envelope(&mut bid_msg)?;
    assert!(publisher.apply_remote(bid_msg).await?);

    let claim = publisher.claim_seed(&seed.id, &bid.id)?;
    let mut claim_msg = model::Envelope::claim_created(publisher.node_id(), claim.clone());
    publisher.sign_envelope(&mut claim_msg)?;
    assert!(worker.apply_remote(claim_msg).await?);

    let (command, timeout_ms) = {
        let publisher_seed = publisher.seed(&seed.id).expect("publisher seed exists");
        (publisher_seed.command.clone(), publisher_seed.timeout_ms)
    };
    publisher.mark_running(&seed.id)?;
    let result = run_bash_task(&command, timeout_ms).await?;
    let stored_result = publisher.add_result(&seed.id, result)?;
    let mut result_msg = model::Envelope::task_result(publisher.node_id(), stored_result.clone());
    publisher.sign_envelope(&mut result_msg)?;
    assert!(worker.apply_remote(result_msg).await?);

    let settled = publisher.settle_seed(&seed.id, true, "accepted by publisher".into())?;
    let mut settle_msg = model::Envelope::task_settle(publisher.node_id(), settled.clone());
    publisher.sign_envelope(&mut settle_msg)?;
    assert!(worker.apply_remote(settle_msg).await?);

    let w_seed = worker
        .seed(&seed.id)
        .expect("worker has synced seed after settle");

    assert_eq!(w_seed.status, model::SeedStatus::Accepted);
    assert_eq!(w_seed.result_id, Some(stored_result.id.clone()));
    assert_eq!(
        publisher.seed(&seed.id).unwrap().status,
        model::SeedStatus::Accepted
    );

    publisher.save().await?;
    worker.save().await?;

    let mut publisher_after = Store::new(publisher_dir);
    let mut worker_after = Store::new(worker_dir);
    publisher_after.load().await?;
    worker_after.load().await?;

    assert!(publisher_after.seed(&seed.id).is_some());
    assert_eq!(
        publisher_after.seed(&seed.id).unwrap().status,
        model::SeedStatus::Accepted
    );
    assert!(worker_after.seed(&seed.id).is_some());
    assert_eq!(
        worker_after.seed(&seed.id).unwrap().result_id,
        Some(stored_result.id)
    );

    Ok(())
}

#[tokio::test]
async fn e2e_reject_low_and_over_bid_limit() -> Result<()> {
    let dir = make_tmp_dir("limits");
    let mut node = Store::new(dir);
    node.load().await?;

    let now = model::current_ts();
    let seed = model::Seed {
        id: Uuid::new_v4().to_string(),
        title: "limited bid task".into(),
        description: "bid rule test".into(),
        command: "echo ok".into(),
        timeout_ms: 1000,
        bid_deadline_ts: now + 600,
        exec_deadline_ms: 1000,
        min_price: 10,
        max_bids: 1,
        reward: 0,
        rules: "rules".into(),
        status: model::SeedStatus::Open,
        created_by: node.node_id().to_string(),
        created_at: now,
        claimed_by: None,
        claimed_at: None,
        claim_id: None,
        result_id: None,
    };
    node.add_seed(seed.clone());

    assert!(node.add_bid(&seed.id, 5, "too cheap".into()).is_err());
    node.add_bid(&seed.id, 10, "ok".into())?;
    assert!(node.add_bid(&seed.id, 20, "too many".into()).is_err());

    Ok(())
}
