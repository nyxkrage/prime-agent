//! Opt-in: a remote kernel against a real SSH host. Set
//! `PA_REMOTE_TEST_HOST` to an SSH destination with non-interactive key
//! auth and python3 >= 3.11; the test is a no-op when it is unset. The fake
//! `ssh` verifiers live in `remote_kernel.rs` and are the default gate.
#![cfg(unix)]
// Kernel manager futures follow the crate's existing integration-test disposition.
#![allow(clippy::large_futures)]

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

async fn start_and_run(host: &str, code: &str) -> (Vec<String>, String, Duration) {
    let package_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills/edit")
        .canonicalize()
        .expect("edit skill");
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python_skills: vec![KernelPythonSkill {
            name: "edit".to_string(),
            import_name: "edit".to_string(),
            pyproject_path: package_path.join("pyproject.toml"),
            package_path,
        }],
        remote: Some(RemoteKernelTarget {
            host: host.to_string(),
            cwd: None,
        }),
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
    let elapsed = started.elapsed();
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .expect("cell runs");
    assert_eq!(result.status, ExecuteStatus::Ok, "{}", result.stderr);
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
    let progress = progress.lock().expect("progress").clone();
    (progress, result.stdout, elapsed)
}

#[tokio::test]
async fn live_remote_host_runs_cells_and_caches_the_install() {
    let Ok(host) = std::env::var("PA_REMOTE_TEST_HOST") else {
        eprintln!("PA_REMOTE_TEST_HOST is unset; skipping the live remote kernel test");
        return;
    };
    let code = "import os, platform, sys, edit\nprint(platform.node(), sys.version.split()[0], os.getcwd(), sys.executable)";
    let (first_progress, stdout, first) = start_and_run(&host, code).await;
    eprintln!("first start {first:?}: {first_progress:?}\n{stdout}");
    assert!(stdout.contains("/.prime/agent/remote/"), "{stdout}");
    let (second_progress, _, second) = start_and_run(&host, code).await;
    eprintln!("second start {second:?}: {second_progress:?}");
    assert!(
        !second_progress
            .iter()
            .any(|message| message.starts_with("Installing")),
        "the second start reuses the install: {second_progress:?}"
    );
}
