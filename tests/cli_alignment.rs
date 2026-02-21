use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use serde_json::json;
use tokio::fs;
use tokio::process::Command;
use uuid::Uuid;

fn make_tmp_dir(prefix: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.push(format!(
        "crabnet-cli-alignment-{}-{}-{}",
        prefix,
        Uuid::new_v4(),
        ts
    ));
    dir
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

    candidates
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(fallback))
        .to_string_lossy()
        .to_string()
}

async fn run_cmd(bin: &str, args: &[&str]) -> Result<(String, String)> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("failed to spawn {}: {e}", bin))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        return Err(anyhow!("command failed: {} {}", stdout, stderr));
    }
    Ok((stdout, stderr))
}

#[tokio::test]
async fn cli_verify_config_flag_succeeds_without_subcommand() -> Result<()> {
    let bin = binary_path();
    let data_dir = make_tmp_dir("verify");

    let (stdout, _) = run_cmd(
        &bin,
        &[
            "--data-dir",
            data_dir.to_string_lossy().as_ref(),
            "--verify-config",
        ],
    )
    .await?;

    assert!(stdout.contains("verify-config: ok"), "stdout={stdout}");
    Ok(())
}

#[tokio::test]
async fn cli_verify_config_allows_empty_announce_for_dht() -> Result<()> {
    let bin = binary_path();
    let data_dir = make_tmp_dir("verify-dht");

    let (stdout, _) = run_cmd(
        &bin,
        &[
            "--data-dir",
            data_dir.to_string_lossy().as_ref(),
            "--network",
            "dht",
            "--announce-addr",
            "",
            "--verify-config",
        ],
    )
    .await?;

    assert!(stdout.contains("verify-config: ok"), "stdout={stdout}");
    Ok(())
}

#[tokio::test]
async fn cli_dump_topology_flag_reads_events_file() -> Result<()> {
    let bin = binary_path();
    let data_dir = make_tmp_dir("topology");
    fs::create_dir_all(&data_dir).await?;

    let events = vec![
        json!({
            "ts": 100,
            "node_id": "node-a",
            "kind": "dht_connection_established",
            "source": "network",
            "payload": {"peer": "node-b"}
        }),
        json!({
            "ts": 101,
            "node_id": "node-a",
            "kind": "udp_broadcast",
            "source": "network",
            "payload": {"peer": "node-b", "target": "node-c"}
        }),
    ];
    let content = events
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");

    fs::write(data_dir.join("events.ndjson"), format!("{content}\n")).await?;

    let (stdout, _) = run_cmd(
        &bin,
        &[
            "--data-dir",
            data_dir.to_string_lossy().as_ref(),
            "--dump-topology",
        ],
    )
    .await?;

    assert!(stdout.contains("\"events\": 2"), "stdout={stdout}");
    assert!(stdout.contains("\"edges\":"), "stdout={stdout}");
    Ok(())
}
