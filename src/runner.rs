use crate::model::TaskResult;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::time::Instant;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

fn command_builder(cmd: &str) -> (String, Vec<String>) {
    if cfg!(target_os = "windows") {
        ("cmd".to_string(), vec!["/C".to_string(), cmd.to_string()])
    } else {
        ("bash".to_string(), vec!["-lc".to_string(), cmd.to_string()])
    }
}

pub async fn run_bash_task(command: &str, timeout_ms: u64) -> Result<TaskResult> {
    let (program, args) = command_builder(command);
    let start = Instant::now();
    let mut child = Command::new(program);
    child.args(&args);
    child.stdout(std::process::Stdio::piped());
    child.stderr(std::process::Stdio::piped());

    let timed_out;
    let output = match timeout(Duration::from_millis(timeout_ms), child.output()).await {
        Ok(inner) => {
            timed_out = false;
            inner?
        }
        Err(_) => {
            timed_out = true;
            std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: b"task timeout".to_vec(),
            }
        }
    };
    let elapsed = start.elapsed().as_millis() as u64;
    let mut hasher = Sha256::new();
    hasher.update(&output.stdout);
    hasher.update(&output.stderr);
    let output_hash = hex::encode(hasher.finalize());

    Ok(TaskResult {
        id: uuid::Uuid::new_v4().to_string(),
        seed_id: String::new(),
        worker: String::new(),
        exit_code: output.status.code().unwrap_or(-1),
        timed_out,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        output_hash,
        duration_ms: elapsed,
    })
}
