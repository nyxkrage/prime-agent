//! Opt-in: a remote kernel against a real SSH host. Set
//! `PA_REMOTE_TEST_HOST` to an SSH destination with non-interactive key
//! auth and python3 >= 3.11; the test is a no-op when it is unset. The fake
//! `ssh` verifiers live in `remote_kernel.rs` and are the default gate.
//!
//! Against the real host it checks a cold and a cached start, that no
//! local-only kernel env reaches the remote kernel, that a refused host
//! request is refused, and that teardown leaves no remote processes.
#![cfg(unix)]
// Kernel manager futures follow the crate's existing integration-test disposition.
#![allow(clippy::large_futures)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, KernelManagerOptions, KernelShutdownOptions, RemoteKernelTarget,
};
use pa_core::kernel::KernelPythonSkill;

/// A cold remote install downloads the runtime's dependencies there.
const START_TIMEOUT: Duration = Duration::from_mins(15);

const LIVE_CELL: &str = r#"
import os, platform, sys, edit, rlm
from rlm import bash
h = bash("sleep 600")
try:
    await rlm.host_request("mcp.config")
    refused = "allowed"
except Exception as error:
    refused = str(error)
print(platform.node(), sys.version.split()[0], sys.executable)
print(os.getpid(), h.pid)
print(" ".join(sorted(os.environ)))
print(refused)
"#;

struct LiveRun {
    progress: Vec<String>,
    lines: Vec<String>,
}

async fn start_and_run(host: &str) -> LiveRun {
    let package_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills/edit")
        .canonicalize()
        .expect("edit skill");
    let env: HashMap<String, String> = [
        ("RLM_SESSION_ID", "live-session"),
        ("SERPER_API_KEY", "local-secret"),
        ("PRIME_AGENT_BASH_COMMAND_PREFIX", "local-prefix"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python_skills: vec![KernelPythonSkill {
            name: "edit".to_string(),
            import_name: "edit".to_string(),
            pyproject_path: package_path.join("pyproject.toml"),
            package_path,
        }],
        env,
        remote: Some(RemoteKernelTarget::validated(host.to_string(), None).expect("valid host")),
        ..KernelManagerOptions::default()
    });
    let progress = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&progress);
    let started = Instant::now();
    tokio::time::timeout(
        START_TIMEOUT,
        manager.start(KernelStartOptions {
            signal: None,
            on_bootstrap_progress: Some(Arc::new(move |message: &str| {
                sink.lock().expect("progress").push(message.to_string());
            })),
        }),
    )
    .await
    .expect("remote kernel starts in time")
    .expect("remote kernel starts");
    eprintln!("start took {:?}", started.elapsed());
    let result = manager
        .execute(LIVE_CELL, ExecuteOptions::default())
        .await
        .expect("cell runs");
    assert_eq!(result.status, ExecuteStatus::Ok, "{}", result.stderr);
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
    let progress = progress.lock().expect("progress").clone();
    LiveRun {
        progress,
        lines: result.stdout.lines().map(str::to_string).collect(),
    }
}

/// Whether any of `pids` is still alive on `host` (one ssh round trip).
fn remote_alive(host: &str, pids: &str) -> bool {
    let status = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            host,
            &format!("kill -0 {pids} 2>/dev/null"),
        ])
        .status()
        .expect("ssh runs");
    status.success()
}

#[tokio::test]
async fn live_remote_host_runs_cells_caches_the_install_and_cleans_up() {
    let Ok(host) = std::env::var("PA_REMOTE_TEST_HOST") else {
        eprintln!("PA_REMOTE_TEST_HOST is unset; skipping the live remote kernel test");
        return;
    };
    let first = start_and_run(&host).await;
    eprintln!("first start: {:?}\n{:?}", first.progress, first.lines);
    assert!(
        first.lines[0].contains("/.prime/agent/remote/"),
        "{:?}",
        first.lines
    );
    let env: Vec<&str> = first.lines[2].split(' ').collect();
    for local_only in [
        "SERPER_API_KEY",
        "PRIME_AGENT_BASH_COMMAND_PREFIX",
        "PYTHONPATH",
    ] {
        assert!(
            !env.contains(&local_only),
            "{local_only} reached the remote: {env:?}"
        );
    }
    assert!(env.contains(&"RLM_SESSION_ID"), "{env:?}");
    assert!(
        first.lines[3]
            .starts_with("host request \"mcp.config\" is not available on remote kernels yet"),
        "{:?}",
        first.lines
    );
    // Teardown leaves neither the kernel nor its bash group running there.
    let pids = &first.lines[1];
    let deadline = Instant::now() + Duration::from_secs(60);
    while remote_alive(&host, pids) {
        assert!(
            Instant::now() < deadline,
            "remote processes {pids} outlived teardown"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    let second = start_and_run(&host).await;
    eprintln!("second start: {:?}", second.progress);
    assert!(
        !second
            .progress
            .iter()
            .any(|message| message.starts_with("Installing")),
        "the second start reuses the install: {:?}",
        second.progress
    );
}
