use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::time::SystemTime;
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;

use anyhow::{anyhow, Result};
use serde_json::Value;
use uuid::Uuid;

use crabnet_mvp::{model, store::Store};

fn make_tmp_dir(prefix: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.push(format!(
        "crabnet-cli-test-{}-{}-{}",
        prefix,
        Uuid::new_v4(),
        ts
    ));
    dir
}

static PORT_TICK: AtomicUsize = AtomicUsize::new(0);

fn unique_port_pair(base: u16) -> (String, String) {
    let tick = PORT_TICK.fetch_add(1, Ordering::SeqCst) as u16;
    let start = base + (tick.wrapping_mul(17) % 1200) * 2;
    let p1 = format!("127.0.0.1:{start}");
    let p2 = format!("127.0.0.1:{}", start + 1);
    (p1, p2)
}

fn binary_path() -> String {
    let bin_name = if cfg!(windows) {
        "crabnet-mvp.exe"
    } else {
        "crabnet-mvp"
    };
    let fallback = if cfg!(windows) {
        "target\\debug\\crabnet-mvp.exe"
    } else {
        "target/debug/crabnet-mvp"
    };

    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_crabnet_mvp") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let mut path = PathBuf::from(manifest_dir);
        path.push("target");
        path.push("debug");
        path.push(bin_name);
        candidates.push(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent().and_then(|p| p.parent()) {
            let mut candidate = bin_dir.to_path_buf();
            candidate.push(bin_name);
            candidates.push(candidate);
        }
    }
    let mut chosen = PathBuf::from(fallback);
    if let Some(found) = candidates.into_iter().find(|path| path.exists()) {
        chosen = found;
    }
    let chosen = chosen.to_string_lossy().to_string();
    if std::env::var("CRABNET_CLI_E2E_DEBUG").is_ok() {
        eprintln!("[cli_e2e] using bin: {}", chosen);
    }
    chosen
}

async fn run_cmd(bin: &str, args: &[String], _cwd: &PathBuf) -> Result<String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("failed to spawn {}: {e}", bin))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(anyhow!("command failed: {stderr} {stdout}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn first_seed_id(dir: &PathBuf) -> Result<String> {
    let mut store = Store::new(dir.clone());
    store.load().await?;
    store
        .state
        .seeds
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| anyhow!("no seed found in {dir:?}"))
}

async fn first_bid_id(dir: &PathBuf, seed_id: &str) -> Result<String> {
    let mut store = Store::new(dir.clone());
    store.load().await?;
    store
        .state
        .bids
        .values()
        .filter(|bid| bid.seed_id == seed_id)
        .max_by_key(|bid| bid.created_at)
        .map(|bid| bid.id.clone())
        .ok_or_else(|| anyhow!("no bid found for seed {seed_id} in {dir:?}"))
}

async fn read_seed_status(dir: &PathBuf, seed_id: &str) -> Result<Option<model::SeedStatus>> {
    let mut store = Store::new(dir.clone());
    store.load().await?;
    Ok(store.seed(seed_id).map(|s| s.status.clone()))
}

async fn wait_for_condition<F, Fut>(
    mut check: F,
    label: &str,
    max_retries: usize,
    interval: Duration,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    for _ in 0..max_retries {
        if check().await? {
            return Ok(());
        }
        sleep(interval).await;
    }
    Err(anyhow!("timeout waiting for {label}"))
}

async fn read_seed_bid_count(dir: &PathBuf, seed_id: &str) -> Result<usize> {
    let mut store = Store::new(dir.clone());
    store.load().await?;
    let count = store
        .state
        .bids
        .values()
        .filter(|b| b.seed_id == seed_id)
        .count();
    Ok(count)
}

async fn wait_for_status(dir: &PathBuf, seed_id: &str, expect: model::SeedStatus) -> Result<()> {
    let expected = expect.clone();
    wait_for_condition(
        || async {
            let status = read_seed_status(dir, seed_id).await?;
            Ok(status == Some(expected.clone()))
        },
        &format!("status {expect:?} for seed {seed_id} in {dir:?}"),
        80,
        Duration::from_millis(80),
    )
    .await
}

async fn assert_no_not_addressed_noops(dir: &PathBuf) -> Result<()> {
    let path = dir.join("events.ndjson");
    let raw = fs::read_to_string(path).await?;
    let mut not_addressed = HashSet::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line)?;
        if event.get("kind").and_then(Value::as_str) != Some("store_sync_noop") {
            continue;
        }
        let payload = event.get("payload");
        if payload
            .and_then(|payload| payload.get("reason"))
            .and_then(Value::as_str)
            == Some("not_addressed_to_this_node")
        {
            let node_id = event.get("node_id").and_then(Value::as_str).unwrap_or("");
            let from = payload
                .and_then(|payload| payload.get("from"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if node_id == from {
                continue;
            }
            let message_id = payload
                .and_then(|payload| payload.get("message_id"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            not_addressed.insert(format!("{node_id}->{from}:{message_id}"));
        }
    }

    if !not_addressed.is_empty() {
        anyhow::bail!(
            "found not_addressed_to_this_node noop events in {}: {:?}",
            dir.display(),
            not_addressed
        );
    }

    Ok(())
}

async fn wait_for_publisher_bid_sync(publisher_dir: &PathBuf, seed_id: &str) -> Result<()> {
    wait_for_condition(
        || async {
            let count = read_seed_bid_count(publisher_dir, seed_id).await?;
            Ok(count > 0)
        },
        &format!("publisher bid sync for seed {seed_id} in {publisher_dir:?}"),
        80,
        Duration::from_millis(80),
    )
    .await?;
    Ok(())
}

async fn wait_for_seed_sync(target_dir: &PathBuf, seed_id: &str) -> Result<()> {
    wait_for_condition(
        || async {
            let status = read_seed_status(target_dir, seed_id).await?;
            Ok(status.is_some())
        },
        &format!("seed sync for {seed_id} in {target_dir:?}"),
        180,
        Duration::from_millis(80),
    )
    .await?;
    Ok(())
}

fn listener_cmd(
    bin: &str,
    dir: &PathBuf,
    listen_addr: &str,
    announce_addr: &str,
    web_addr: &str,
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--data-dir")
        .arg(dir)
        .arg("--listen-addr")
        .arg(listen_addr)
        .arg("--announce-addr")
        .arg(announce_addr)
        .arg("--web-addr")
        .arg(web_addr);
    cmd.arg("listen");
    cmd
}

fn listener_cmd_network(
    bin: &str,
    dir: &PathBuf,
    listen_addr: &str,
    bootstrap_peers: &str,
    web_addr: &str,
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--data-dir")
        .arg(dir)
        .arg("--listen-addr")
        .arg(listen_addr)
        .arg("--network")
        .arg("dht")
        .arg("--bootstrap-peers")
        .arg(bootstrap_peers)
        .arg("--web-addr")
        .arg(web_addr);
    cmd.arg("listen");
    cmd
}

#[tokio::test]
async fn cli_e2e_dual_node_sync_publish_bid_claim_run_settle() -> Result<()> {
    let bin = binary_path();
    let publisher_dir = make_tmp_dir("publisher");
    let worker_dir = make_tmp_dir("worker");

    let (publisher_listen, worker_listen) = unique_port_pair(12020);
    let both_announces = format!("{publisher_listen},{worker_listen}");

    let mut publisher_listener = listener_cmd(
        &bin,
        &publisher_dir,
        &publisher_listen,
        &both_announces,
        "127.0.0.1:3101",
    )
    .spawn()?;
    let mut worker_listener = listener_cmd(
        &bin,
        &worker_dir,
        &worker_listen,
        &both_announces,
        "127.0.0.1:3102",
    )
    .spawn()?;
    sleep(Duration::from_millis(900)).await;

    let result: Result<()> = async {
        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                publisher_dir.to_string_lossy().to_string(),
                "--announce-addr".into(),
                both_announces.clone(),
                "seed".into(),
                "publish".into(),
                "--title".into(),
                "cli e2e".into(),
                "--cmd".into(),
                "echo cli-ok".into(),
                "--timeout-ms".into(),
                "5000".into(),
                "--bid-window-ms".into(),
                "12000".into(),
                "--min-price".into(),
                "1".into(),
                "--max-bids".into(),
                "2".into(),
                "--announce".into(),
            ],
            &publisher_dir,
        )
        .await?;

        let seed_id = first_seed_id(&publisher_dir).await?;
        sleep(Duration::from_millis(200)).await;
        wait_for_seed_sync(&worker_dir, &seed_id).await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                worker_dir.to_string_lossy().to_string(),
                "--announce-addr".into(),
                both_announces.clone(),
                "seed".into(),
                "bid".into(),
                seed_id.clone(),
                "--price".into(),
                "1".into(),
                "--announce".into(),
            ],
            &worker_dir,
        )
        .await?;

        let bid_id = first_bid_id(&worker_dir, &seed_id).await?;
        wait_for_publisher_bid_sync(&publisher_dir, &seed_id).await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                publisher_dir.to_string_lossy().to_string(),
                "--announce-addr".into(),
                both_announces.clone(),
                "seed".into(),
                "claim".into(),
                seed_id.clone(),
                bid_id,
                "--announce".into(),
            ],
            &publisher_dir,
        )
        .await?;

        wait_for_status(&worker_dir, &seed_id, model::SeedStatus::Claimed).await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                worker_dir.to_string_lossy().to_string(),
                "--announce-addr".into(),
                both_announces.clone(),
                "seed".into(),
                "run".into(),
                seed_id.clone(),
            ],
            &worker_dir,
        )
        .await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                publisher_dir.to_string_lossy().to_string(),
                "--announce-addr".into(),
                both_announces.clone(),
                "seed".into(),
                "settle".into(),
                seed_id.clone(),
                "--accepted".into(),
                "--note".into(),
                "ok".into(),
                "--announce".into(),
            ],
            &publisher_dir,
        )
        .await?;

        wait_for_status(&publisher_dir, &seed_id, model::SeedStatus::Accepted).await?;
        wait_for_status(&worker_dir, &seed_id, model::SeedStatus::Accepted).await?;
        assert_no_not_addressed_noops(&publisher_dir).await?;
        assert_no_not_addressed_noops(&worker_dir).await?;

        let mut publisher_store = Store::new(publisher_dir.clone());
        let mut worker_store = Store::new(worker_dir.clone());
        publisher_store.load().await?;
        worker_store.load().await?;

        let p = publisher_store
            .seed(&seed_id)
            .expect("publisher final seed");
        let w = worker_store.seed(&seed_id).expect("worker final seed");
        assert_eq!(p.status, model::SeedStatus::Accepted);
        assert_eq!(w.status, model::SeedStatus::Accepted);
        assert_eq!(p.result_id, w.result_id);
        Ok(())
    }
    .await;

    publisher_listener.kill().await.ok();
    worker_listener.kill().await.ok();
    publisher_listener.wait().await.ok();
    worker_listener.wait().await.ok();

    result
}

#[tokio::test]
async fn cli_e2e_dual_node_dht_sync_publish_bid_claim_run_settle() -> Result<()> {
    let bin = binary_path();
    let publisher_dir = make_tmp_dir("publisher-dht");
    let worker_dir = make_tmp_dir("worker-dht");

    let (publisher_listen, worker_listen) = unique_port_pair(13200);
    let bootstrap = format!("{publisher_listen},{worker_listen}");

    let mut publisher_listener = listener_cmd_network(
        &bin,
        &publisher_dir,
        &publisher_listen,
        &bootstrap,
        "127.0.0.1:3111",
    )
    .spawn()?;
    let mut worker_listener = listener_cmd_network(
        &bin,
        &worker_dir,
        &worker_listen,
        &bootstrap,
        "127.0.0.1:3112",
    )
    .spawn()?;
    sleep(Duration::from_millis(900)).await;

    let result: Result<()> = async {
        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                publisher_dir.to_string_lossy().to_string(),
                "--network".into(),
                "dht".into(),
                "--bootstrap-peers".into(),
                bootstrap.clone(),
                "--announce-addr".into(),
                bootstrap.clone(),
                "seed".into(),
                "publish".into(),
                "--title".into(),
                "cli dht".into(),
                "--cmd".into(),
                "echo cli-ok".into(),
                "--timeout-ms".into(),
                "5000".into(),
                "--bid-window-ms".into(),
                "12000".into(),
                "--min-price".into(),
                "1".into(),
                "--max-bids".into(),
                "2".into(),
                "--announce".into(),
            ],
            &publisher_dir,
        )
        .await?;

        let seed_id = first_seed_id(&publisher_dir).await?;
        sleep(Duration::from_millis(200)).await;
        wait_for_seed_sync(&worker_dir, &seed_id).await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                worker_dir.to_string_lossy().to_string(),
                "--network".into(),
                "dht".into(),
                "--bootstrap-peers".into(),
                bootstrap.clone(),
                "--announce-addr".into(),
                bootstrap.clone(),
                "seed".into(),
                "bid".into(),
                seed_id.clone(),
                "--price".into(),
                "1".into(),
                "--announce".into(),
            ],
            &worker_dir,
        )
        .await?;

        let bid_id = first_bid_id(&worker_dir, &seed_id).await?;
        wait_for_publisher_bid_sync(&publisher_dir, &seed_id).await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                publisher_dir.to_string_lossy().to_string(),
                "--network".into(),
                "dht".into(),
                "--bootstrap-peers".into(),
                bootstrap.clone(),
                "--announce-addr".into(),
                bootstrap.clone(),
                "seed".into(),
                "claim".into(),
                seed_id.clone(),
                bid_id,
                "--announce".into(),
            ],
            &publisher_dir,
        )
        .await?;

        wait_for_status(&worker_dir, &seed_id, model::SeedStatus::Claimed).await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                worker_dir.to_string_lossy().to_string(),
                "--network".into(),
                "dht".into(),
                "--bootstrap-peers".into(),
                bootstrap.clone(),
                "--announce-addr".into(),
                bootstrap.clone(),
                "seed".into(),
                "run".into(),
                seed_id.clone(),
            ],
            &worker_dir,
        )
        .await?;

        run_cmd(
            &bin,
            &[
                "--data-dir".into(),
                publisher_dir.to_string_lossy().to_string(),
                "--network".into(),
                "dht".into(),
                "--bootstrap-peers".into(),
                bootstrap.clone(),
                "--announce-addr".into(),
                bootstrap.clone(),
                "seed".into(),
                "settle".into(),
                seed_id.clone(),
                "--accepted".into(),
                "--note".into(),
                "ok".into(),
                "--announce".into(),
            ],
            &publisher_dir,
        )
        .await?;

        wait_for_status(&publisher_dir, &seed_id, model::SeedStatus::Accepted).await?;
        wait_for_status(&worker_dir, &seed_id, model::SeedStatus::Accepted).await?;
        assert_no_not_addressed_noops(&publisher_dir).await?;
        assert_no_not_addressed_noops(&worker_dir).await?;

        let mut publisher_store = Store::new(publisher_dir.clone());
        let mut worker_store = Store::new(worker_dir.clone());
        publisher_store.load().await?;
        worker_store.load().await?;

        let p = publisher_store
            .seed(&seed_id)
            .expect("publisher final seed");
        let w = worker_store.seed(&seed_id).expect("worker final seed");
        assert_eq!(p.status, model::SeedStatus::Accepted);
        assert_eq!(w.status, model::SeedStatus::Accepted);
        assert_eq!(p.result_id, w.result_id);
        Ok(())
    }
    .await;

    publisher_listener.kill().await.ok();
    worker_listener.kill().await.ok();
    publisher_listener.wait().await.ok();
    worker_listener.wait().await.ok();

    result
}
