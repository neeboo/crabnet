use anyhow::Result;
use crabnet_mvp::run_bash_task;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const ENV_ALLOWED_COMMANDS: &str = "CRABNET_RUNNER_ALLOWED_COMMANDS";
const ENV_MAX_COMMAND_LEN: &str = "CRABNET_RUNNER_MAX_COMMAND_LEN";
const ENV_WORKDIR: &str = "CRABNET_RUNNER_WORKDIR";
const ENV_WORKDIR_ROOT: &str = "CRABNET_RUNNER_WORKDIR_ROOT";
const ENV_MAX_OUTPUT_BYTES: &str = "CRABNET_RUNNER_MAX_OUTPUT_BYTES";
const ENV_TIMEOUT_KILL_WAIT_MS: &str = "CRABNET_RUNNER_TIMEOUT_KILL_WAIT_MS";

const ENV_KEYS: [&str; 6] = [
    ENV_ALLOWED_COMMANDS,
    ENV_MAX_COMMAND_LEN,
    ENV_WORKDIR,
    ENV_WORKDIR_ROOT,
    ENV_MAX_OUTPUT_BYTES,
    ENV_TIMEOUT_KILL_WAIT_MS,
];

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn with_runner_env<F>(pairs: &[(&str, String)], f: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let _guard = env_lock().lock().expect("env lock");
    let saved: Vec<(String, Option<String>)> = ENV_KEYS
        .iter()
        .map(|key| ((*key).to_string(), std::env::var(key).ok()))
        .collect();

    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
    for (key, value) in pairs {
        std::env::set_var(key, value);
    }

    let result = f();

    for (key, value) in saved {
        match value {
            Some(existing) => std::env::set_var(key, existing),
            None => std::env::remove_var(key),
        }
    }

    result
}

fn run(command: &str, timeout_ms: u64) -> Result<crabnet_mvp::model::TaskResult> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_bash_task(command, timeout_ms))
}

fn make_tmp_dir(prefix: &str) -> Result<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("crabnet-runner-test-{prefix}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[test]
fn rejects_command_outside_allowlist() -> Result<()> {
    with_runner_env(&[(ENV_ALLOWED_COMMANDS, "echo".to_string())], || {
        let result = run("pwd", 500)?;
        assert_eq!(result.exit_code, -1);
        assert!(!result.timed_out);
        assert!(result.stderr.contains("not in allowlist"));
        Ok(())
    })
}

#[test]
fn rejects_overlong_command() -> Result<()> {
    with_runner_env(
        &[
            (ENV_ALLOWED_COMMANDS, "*".to_string()),
            (ENV_MAX_COMMAND_LEN, "8".to_string()),
        ],
        || {
            let result = run("echo this-is-too-long", 500)?;
            assert_eq!(result.exit_code, -1);
            assert!(!result.timed_out);
            assert!(result.stderr.contains("command length"));
            Ok(())
        },
    )
}

#[test]
fn enforces_workdir_root_restriction() -> Result<()> {
    let root = make_tmp_dir("root")?;
    let outside = make_tmp_dir("outside")?;

    with_runner_env(
        &[
            (ENV_ALLOWED_COMMANDS, "*".to_string()),
            (ENV_WORKDIR, outside.to_string_lossy().to_string()),
            (ENV_WORKDIR_ROOT, root.to_string_lossy().to_string()),
        ],
        || {
            let result = run("echo blocked", 500)?;
            assert_eq!(result.exit_code, -1);
            assert!(result.stderr.contains("outside allowed root"));
            Ok(())
        },
    )
}

#[test]
fn runs_inside_configured_workdir() -> Result<()> {
    let root = make_tmp_dir("allowed-root")?;
    let workdir = root.join("runner");
    std::fs::create_dir_all(&workdir)?;
    let expected = std::fs::canonicalize(&workdir)?;

    let cmd = if cfg!(target_os = "windows") {
        "cd"
    } else {
        "pwd"
    };
    let allow = if cfg!(target_os = "windows") {
        "cd"
    } else {
        "pwd"
    };

    with_runner_env(
        &[
            (ENV_ALLOWED_COMMANDS, allow.to_string()),
            (ENV_WORKDIR, workdir.to_string_lossy().to_string()),
            (ENV_WORKDIR_ROOT, root.to_string_lossy().to_string()),
        ],
        || {
            let result = run(cmd, 1000)?;
            assert_eq!(result.exit_code, 0);
            assert_eq!(result.stdout.trim(), expected.to_string_lossy());
            Ok(())
        },
    )
}

#[test]
fn timeout_kills_process_and_records_metadata() -> Result<()> {
    let (cmd, allow) = if cfg!(target_os = "windows") {
        ("ping -n 6 127.0.0.1 > NUL", "ping")
    } else {
        ("sleep 5", "sleep")
    };

    with_runner_env(
        &[
            (ENV_ALLOWED_COMMANDS, allow.to_string()),
            (ENV_TIMEOUT_KILL_WAIT_MS, "300".to_string()),
        ],
        || {
            let result = run(cmd, 80)?;
            assert!(result.timed_out);
            assert!(result.stderr.contains("[runner_timeout"));
            assert!(result.stderr.contains("terminate_invoked="));
            Ok(())
        },
    )
}

#[cfg(unix)]
#[test]
fn truncates_stdout_and_stderr_by_configured_cap() -> Result<()> {
    with_runner_env(
        &[
            (ENV_ALLOWED_COMMANDS, "printf".to_string()),
            (ENV_MAX_OUTPUT_BYTES, "64".to_string()),
        ],
        || {
            let result = run(
                "printf 'a%.0s' {1..300}; printf 'b%.0s' {1..300} 1>&2",
                1000,
            )?;
            assert_eq!(result.exit_code, 0);
            assert!(result.stdout.contains("[runner_output_truncated"));
            assert!(result.stderr.contains("[runner_output_truncated"));
            assert!(result.stdout.contains("cap_bytes=64"));
            assert!(result.stderr.contains("cap_bytes=64"));
            Ok(())
        },
    )
}
