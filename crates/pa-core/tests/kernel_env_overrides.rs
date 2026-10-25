//! `rlm.spawn(env=...)` on a local kernel: literal values override the
//! inherited env, pass-throughs resolve from the environment the kernel
//! gets (the kernel env seam here, so no process env is mutated), a
//! missing pass-through fails the start naming it, and pass-through values
//! are masked in the output the model reads back. The remote delivery is
//! `remote_kernel.rs`'s.
#![cfg(unix)]
// Kernel manager futures follow the crate's existing integration-test disposition.
#![allow(clippy::large_futures)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};
use pa_core::kernel::KernelEnvOverrides;
use serde_json::json;

const START_TIMEOUT: Duration = Duration::from_mins(2);
const SECRET: &str = "passthrough-secret-4b7e19";

fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let candidate = PathBuf::from(format!(
        "{}/.prime/agent/kernel-venv/bin/python",
        std::env::var("HOME").unwrap_or_default()
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

fn manager(python: PathBuf, env_overrides: KernelEnvOverrides) -> ReplKernelManager {
    manager_with(python, env_overrides, HostRequestHandlers::new(), None)
}

fn manager_with(
    python: PathBuf,
    env_overrides: KernelEnvOverrides,
    host_handlers: HostRequestHandlers,
    stderr_log_path: Option<PathBuf>,
) -> ReplKernelManager {
    // The kernel env is the seam the pass-throughs resolve from (it
    // overlays this process's env, exactly as the kernel inherits it).
    let env: HashMap<String, String> = [
        ("PA_TEST_PASSTHROUGH", SECRET),
        ("PA_TEST_LITERAL", "inherited"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    ReplKernelManager::new(KernelManagerOptions {
        remote: None,
        env_overrides,
        python: Some(python),
        cwd: Some(std::env::temp_dir()),
        env,
        session_id: Some("env-overrides-test".to_string()),
        host_handlers,
        python_skills: Vec::new(),
        snapshot: None,
        bootstrap_code: None,
        stderr_log_path,
        on_background_work_settled: None,
    })
}

#[tokio::test]
async fn a_local_kernel_gets_the_spawn_env_and_masks_passthrough_values() {
    let Some(python) = kernel_python() else {
        return;
    };
    let overrides = KernelEnvOverrides::from_json(&json!({
        "PA_TEST_LITERAL": "literal-wins",
        "PA_TEST_PASSTHROUGH": null,
    }))
    .unwrap();
    let manager = manager(python, overrides);
    tokio::time::timeout(START_TIMEOUT, manager.start(KernelStartOptions::default()))
        .await
        .expect("kernel starts in time")
        .expect("kernel starts");
    let result = manager
        .execute(
            "import os, sys\nprint(os.environ['PA_TEST_LITERAL'])\nprint(os.environ['PA_TEST_PASSTHROUGH'][::-1])\nprint(os.environ['PA_TEST_PASSTHROUGH'])\nprint('err', os.environ['PA_TEST_PASSTHROUGH'], file=sys.stderr)",
            ExecuteOptions::default(),
        )
        .await
        .expect("cell runs");
    assert_eq!(result.status, ExecuteStatus::Ok, "{}", result.stderr);
    // The literal overrides the inherited value; the pass-through arrived
    // (its reversal is not masked) and is masked wherever it is printed.
    let reversed: String = SECRET.chars().rev().collect();
    assert_eq!(result.stdout, format!("literal-wins\n{reversed}\n***\n"));
    assert_eq!(result.stderr, "err ***\n");
    let failed = manager
        .execute(
            "raise RuntimeError(os.environ['PA_TEST_PASSTHROUGH'])",
            ExecuteOptions::default(),
        )
        .await
        .expect("cell runs");
    let error = failed.error.expect("the cell raised");
    assert_eq!(error.evalue, "***");
    assert!(
        error.traceback.iter().all(|line| !line.contains(SECRET)),
        "{:?}",
        error.traceback
    );
    let result = manager
        .execute(
            "os.environ['PA_TEST_PASSTHROUGH']",
            ExecuteOptions::default(),
        )
        .await
        .expect("cell runs");
    assert_eq!(result.result.as_deref(), Some("'***'"));
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

#[tokio::test]
async fn a_missing_passthrough_fails_the_start_naming_it() {
    let Some(python) = kernel_python() else {
        return;
    };
    let overrides = KernelEnvOverrides::from_json(&json!({ "PA_TEST_MISSING_VAR": null })).unwrap();
    let error = manager(python, overrides)
        .start(KernelStartOptions::default())
        .await
        .expect_err("the start is refused");
    assert_eq!(
        format!("{error:#}"),
        "PA_TEST_MISSING_VAR is not set on the orchestrator (rlm.spawn env PA_TEST_MISSING_VAR=None passes the orchestrator's value through)"
    );
}

/// The masking gaps: a value split across stream chunks, a diff display
/// payload (persisted as tool details), and `agent_message.send` text are
/// masked; and the stderr log's byte budget counts the masked bytes it
/// writes, so masking never truncates the log early.
#[tokio::test]
async fn split_chunks_diffs_agent_messages_and_the_stderr_log_are_masked() {
    let Some(python) = kernel_python() else {
        return;
    };
    let overrides = KernelEnvOverrides::from_json(&json!({ "PA_TEST_PASSTHROUGH": null })).unwrap();
    let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let mut handlers = HostRequestHandlers::new();
    {
        let sent = std::sync::Arc::clone(&sent);
        handlers.register(
            "agent_message.send",
            std::sync::Arc::new(
                move |payload: pa_core::kernel::shared::HostRequestPayload| {
                    sent.lock().unwrap().push(payload.data);
                    Box::pin(async { Ok(json!({ "ok": true })) })
                },
            ),
        );
    }
    let log_dir = tempfile::TempDir::new().unwrap();
    let log_path = log_dir.path().join("kernel-stderr.log");
    let manager = manager_with(python, overrides, handlers, Some(log_path.clone()));
    tokio::time::timeout(START_TIMEOUT, manager.start(KernelStartOptions::default()))
        .await
        .expect("kernel starts in time")
        .expect("kernel starts");
    let result = manager
        .execute(
            r"
import os, sys, time
from rlm.repl import emit, host_request
secret = os.environ['PA_TEST_PASSTHROUGH']
sys.stdout.write('token=' + secret[:9]); sys.stdout.flush(); time.sleep(0.3)
sys.stdout.write(secret[9:] + ' end\n'); sys.stdout.flush()
emit({'application/vnd.prime-agent.diff+json': {'path': 'cfg.env', 'old_str': 'TOKEN=', 'new_str': 'TOKEN=' + secret}})
await host_request({'type': 'agent_message.send', 'message': 'the token is ' + secret})
# The kernel process's own stderr (the log's source; cell stderr is a
# stream event): over the 5 MiB log budget raw, far under it masked.
import rlm.repl as _repl
data = memoryview(((secret + ' ') * 250000 + 'tail-marker\n').encode())
while data:
    data = data[os.write(_repl._host_stderr_fd, data):]
",
            ExecuteOptions::default(),
        )
        .await
        .expect("cell runs");
    assert_eq!(result.status, ExecuteStatus::Ok, "{}", result.stderr);
    assert_eq!(result.stdout, "token=*** end\n");
    let diffs = result.diffs.expect("a diff display");
    assert_eq!(
        (diffs[0].old_str.as_str(), diffs[0].new_str.as_str()),
        ("TOKEN=", "TOKEN=***")
    );
    assert_eq!(
        *sent.lock().unwrap(),
        [json!({ "type": "agent_message.send", "message": "the token is ***" })]
    );
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
    let log = std::fs::read_to_string(&log_path).expect("stderr log");
    assert!(!log.contains(SECRET), "the stderr log carries the value");
    assert!(
        log.contains("tail-marker"),
        "the masked stderr fits the budget and is logged whole"
    );
}
