use crate::model::TaskResult;
use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const ENV_ALLOWED_COMMANDS: &str = "CRABNET_RUNNER_ALLOWED_COMMANDS";
const ENV_MAX_COMMAND_LEN: &str = "CRABNET_RUNNER_MAX_COMMAND_LEN";
const ENV_WORKDIR: &str = "CRABNET_RUNNER_WORKDIR";
const ENV_WORKDIR_ROOT: &str = "CRABNET_RUNNER_WORKDIR_ROOT";
const ENV_MAX_OUTPUT_BYTES: &str = "CRABNET_RUNNER_MAX_OUTPUT_BYTES";
const ENV_TIMEOUT_KILL_WAIT_MS: &str = "CRABNET_RUNNER_TIMEOUT_KILL_WAIT_MS";

const DEFAULT_ALLOWED_COMMANDS: &[&str] = &[
    "echo", "printf", "pwd", "ls", "cat", "sleep", "true", "false", "cd",
];
const DEFAULT_MAX_COMMAND_LEN: usize = 4096;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 128 * 1024;
const DEFAULT_TIMEOUT_KILL_WAIT_MS: u64 = 1000;

#[derive(Debug, Clone)]
struct RunnerConfig {
    allowlist: Vec<String>,
    allow_all: bool,
    max_command_len: usize,
    max_output_bytes: usize,
    workdir: PathBuf,
    allowed_root: PathBuf,
    timeout_kill_wait_ms: u64,
}

#[derive(Debug)]
struct StreamCapture {
    bytes: Vec<u8>,
    total_bytes: usize,
    truncated: bool,
}

fn command_builder(cmd: &str) -> (String, Vec<String>) {
    if cfg!(target_os = "windows") {
        ("cmd".to_string(), vec!["/C".to_string(), cmd.to_string()])
    } else {
        ("bash".to_string(), vec!["-lc".to_string(), cmd.to_string()])
    }
}

fn read_env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn read_env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn canonical_dir(raw: &str, env_name: &str) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(raw)
        .map_err(|err| anyhow!("{env_name} path `{raw}` is invalid: {err}"))?;
    if !canonical.is_dir() {
        return Err(anyhow!("{env_name} path `{}` is not a directory", raw));
    }
    Ok(canonical)
}

fn load_runner_config() -> Result<RunnerConfig> {
    let raw_allowlist =
        std::env::var(ENV_ALLOWED_COMMANDS).unwrap_or_else(|_| DEFAULT_ALLOWED_COMMANDS.join(","));
    let allowlist: Vec<String> = raw_allowlist
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.to_ascii_lowercase())
        .collect();
    let allow_all = allowlist.iter().any(|entry| entry == "*");

    let raw_workdir = std::env::var(ENV_WORKDIR).unwrap_or_else(|_| ".".to_string());
    let raw_allowed_root = std::env::var(ENV_WORKDIR_ROOT).unwrap_or_else(|_| raw_workdir.clone());
    let workdir = canonical_dir(&raw_workdir, ENV_WORKDIR)?;
    let allowed_root = canonical_dir(&raw_allowed_root, ENV_WORKDIR_ROOT)?;
    if !workdir.starts_with(&allowed_root) {
        return Err(anyhow!(
            "configured workdir `{}` is outside allowed root `{}`",
            workdir.display(),
            allowed_root.display()
        ));
    }

    Ok(RunnerConfig {
        allowlist,
        allow_all,
        max_command_len: read_env_usize(ENV_MAX_COMMAND_LEN, DEFAULT_MAX_COMMAND_LEN),
        max_output_bytes: read_env_usize(ENV_MAX_OUTPUT_BYTES, DEFAULT_MAX_OUTPUT_BYTES),
        workdir,
        allowed_root,
        timeout_kill_wait_ms: read_env_u64(ENV_TIMEOUT_KILL_WAIT_MS, DEFAULT_TIMEOUT_KILL_WAIT_MS),
    })
}

fn extract_command_token(command: &str) -> Option<&str> {
    command.split_whitespace().find(|token| {
        // Allow common `FOO=bar cmd` command prefixes.
        !(token.contains('=') && !token.contains('/') && !token.contains('\\'))
    })
}

fn normalized_token(token: &str) -> String {
    let cleaned = token.trim_matches(|c| c == '\'' || c == '"');
    Path::new(cleaned)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(cleaned)
        .to_ascii_lowercase()
}

fn is_command_allowed(command: &str, config: &RunnerConfig) -> bool {
    if config.allow_all {
        return true;
    }
    let Some(token) = extract_command_token(command) else {
        return false;
    };
    let token = normalized_token(token);
    config.allowlist.iter().any(|allowed| allowed == &token)
}

fn build_task_result(
    started_at: Instant,
    exit_code: i32,
    timed_out: bool,
    stdout: String,
    stderr: String,
) -> TaskResult {
    let mut hasher = Sha256::new();
    hasher.update(stdout.as_bytes());
    hasher.update(stderr.as_bytes());
    let output_hash = hex::encode(hasher.finalize());
    TaskResult {
        id: uuid::Uuid::new_v4().to_string(),
        seed_id: String::new(),
        worker: String::new(),
        exit_code,
        timed_out,
        stdout,
        stderr,
        output_hash,
        duration_ms: started_at.elapsed().as_millis() as u64,
    }
}

fn rejected_result(started_at: Instant, reason: &str) -> TaskResult {
    build_task_result(
        started_at,
        -1,
        false,
        String::new(),
        format!("[runner_rejected] {reason}"),
    )
}

fn render_capture(capture: StreamCapture, cap: usize) -> String {
    let mut rendered = String::from_utf8_lossy(&capture.bytes).to_string();
    if capture.truncated {
        if !rendered.is_empty() && !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str(&format!(
            "[runner_output_truncated captured_bytes={} total_bytes={} cap_bytes={cap}]",
            capture.bytes.len(),
            capture.total_bytes,
        ));
    }
    rendered
}

async fn read_stream_limited<R>(mut reader: R, cap: usize) -> std::io::Result<StreamCapture>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut captured = Vec::with_capacity(cap.min(8192));
    let mut total_bytes = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        total_bytes += n;
        if captured.len() < cap {
            let room = cap - captured.len();
            let take = room.min(n);
            captured.extend_from_slice(&buffer[..take]);
        }
    }
    Ok(StreamCapture {
        truncated: total_bytes > captured.len(),
        bytes: captured,
        total_bytes,
    })
}

pub async fn run_bash_task(command: &str, timeout_ms: u64) -> Result<TaskResult> {
    let config = match load_runner_config() {
        Ok(config) => config,
        Err(err) => {
            return Ok(rejected_result(
                Instant::now(),
                &format!("invalid runner config: {err}"),
            ))
        }
    };
    let start = Instant::now();
    let command = command.trim();
    if command.is_empty() {
        return Ok(rejected_result(start, "empty command"));
    }
    if command.len() > config.max_command_len {
        return Ok(rejected_result(
            start,
            &format!(
                "command length {} exceeds max {}",
                command.len(),
                config.max_command_len
            ),
        ));
    }
    if !is_command_allowed(command, &config) {
        return Ok(rejected_result(
            start,
            &format!(
                "command `{}` is not in allowlist (workdir={}, root={})",
                extract_command_token(command).unwrap_or("<none>"),
                config.workdir.display(),
                config.allowed_root.display(),
            ),
        ));
    }

    let (program, args) = command_builder(command);
    let mut child = Command::new(program);
    child.args(&args);
    child.current_dir(&config.workdir);
    child.stdout(Stdio::piped());
    child.stderr(Stdio::piped());
    child.kill_on_drop(true);
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Ok(rejected_result(start, &format!("spawn failed: {err}")));
        }
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture stderr"))?;
    let max_output = config.max_output_bytes;
    let stdout_handle = tokio::spawn(async move { read_stream_limited(stdout, max_output).await });
    let stderr_handle = tokio::spawn(async move { read_stream_limited(stderr, max_output).await });

    let mut timed_out = false;
    let mut timeout_metadata: Option<String> = None;
    let mut status_code = -1;
    match timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(wait_result) => {
            let status = wait_result?;
            status_code = status.code().unwrap_or(-1);
        }
        Err(_) => {
            timed_out = true;
            let terminate_invoked = child.start_kill().is_ok();
            let waited_after_kill = match timeout(
                Duration::from_millis(config.timeout_kill_wait_ms),
                child.wait(),
            )
            .await
            {
                Ok(Ok(status)) => {
                    status_code = status.code().unwrap_or(-1);
                    true
                }
                Ok(Err(err)) => {
                    timeout_metadata = Some(format!(
                        "[runner_timeout timeout_ms={timeout_ms} terminate_invoked={terminate_invoked} wait_error={err}]"
                    ));
                    false
                }
                Err(_) => {
                    let _ = child.kill().await;
                    if let Ok(status) = child.wait().await {
                        status_code = status.code().unwrap_or(-1);
                    }
                    false
                }
            };
            if timeout_metadata.is_none() {
                timeout_metadata = Some(format!(
                    "[runner_timeout timeout_ms={timeout_ms} terminate_invoked={terminate_invoked} kill_wait_ms={} waited_after_kill={waited_after_kill}]",
                    config.timeout_kill_wait_ms
                ));
            }
        }
    }

    let stdout_capture = stdout_handle
        .await
        .map_err(|err| anyhow!("stdout capture join failed: {err}"))??;
    let stderr_capture = stderr_handle
        .await
        .map_err(|err| anyhow!("stderr capture join failed: {err}"))??;
    let stdout = render_capture(stdout_capture, config.max_output_bytes);
    let mut stderr = render_capture(stderr_capture, config.max_output_bytes);
    if let Some(metadata) = timeout_metadata {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&metadata);
    }
    Ok(build_task_result(
        start,
        status_code,
        timed_out,
        stdout,
        stderr,
    ))
}
