//! Remote (SSH) kernels: provisioning, cells, the install cache, the Python
//! probe, and a dropped connection. `PRIME_AGENT_SSH` points at a fake `ssh`
//! that runs the remote command locally with a per-destination `HOME`, so
//! each destination behaves like a separate machine.
#![cfg(unix)]
// Kernel manager futures follow the crate's existing integration-test disposition.
#![allow(clippy::large_futures)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, KernelManagerOptions, KernelShutdownOptions, RemoteKernelTarget,
};
use pa_core::kernel::KernelPythonSkill;

/// Provisioning builds a venv with uv (from its warm cache), so the bound
/// covers a cold install, not a fixed wait.
const START_TIMEOUT: Duration = Duration::from_mins(10);

const FAKE_SSH: &str = r#"#!/bin/sh
# Fake ssh: skip the options, run the remote command locally.
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -p|-o) shift 2 ;;
    *) shift ;;
  esac
done
dest="$1"; shift
base="__BASE__"
printf '%s\n' "$*" >> "$base/log-$dest"
export UV_CACHE_DIR="${UV_CACHE_DIR:-$HOME/.cache/uv}"
export HOME="$base/home-$dest"
mkdir -p "$HOME"
cd "$HOME"
case "$dest" in
  oldpython) PATH="$base/oldpython-bin:$PATH"; export PATH ;;
esac
exec sh -c "$*"
"#;

const OLD_PYTHON: &str = r#"#!/bin/sh
echo '{"version": [3, 8, 10], "executable": "/usr/bin/python3", "venv": true, "uv": null, "system": "Linux", "machine": "x86_64", "root": "/r", "dir": "/r/k", "installed": false}'
"#;

/// The fake-ssh base dir, installed once per test binary.
fn fake_ssh_base() -> &'static Path {
    static BASE: OnceLock<tempfile::TempDir> = OnceLock::new();
    BASE.get_or_init(|| {
        let base = tempfile::TempDir::new().expect("temp dir");
        let ssh = base.path().join("fake-ssh");
        std::fs::write(
            &ssh,
            FAKE_SSH.replace("__BASE__", &base.path().to_string_lossy()),
        )
        .expect("write fake ssh");
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let old_bin = base.path().join("oldpython-bin");
        std::fs::create_dir_all(&old_bin).expect("old python dir");
        std::fs::write(old_bin.join("python3"), OLD_PYTHON).expect("write old python");
        std::fs::set_permissions(
            old_bin.join("python3"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        // Set once, before any manager exists; every test reads the same value.
        std::env::set_var("PRIME_AGENT_SSH", &ssh);
        base
    })
    .path()
}

fn install_count(destination: &str) -> usize {
    std::fs::read_to_string(fake_ssh_base().join(format!("log-{destination}")))
        .unwrap_or_default()
        .matches("import io, json, os, shutil, subprocess, sys, tarfile, time")
        .count()
}

fn edit_skill() -> KernelPythonSkill {
    let package_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills/edit");
    let package_path = package_path.canonicalize().expect("edit skill");
    KernelPythonSkill {
        name: "edit".to_string(),
        import_name: "edit".to_string(),
        pyproject_path: package_path.join("pyproject.toml"),
        package_path,
    }
}

fn remote_manager(host: &str, cwd: Option<String>) -> ReplKernelManager {
    fake_ssh_base();
    ReplKernelManager::new(KernelManagerOptions {
        python_skills: vec![edit_skill()],
        remote: Some(RemoteKernelTarget {
            host: host.to_string(),
            cwd,
        }),
        ..KernelManagerOptions::default()
    })
}

async fn run(manager: &ReplKernelManager, code: &str) -> String {
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .expect("cell runs");
    assert_eq!(result.status, ExecuteStatus::Ok, "{}", result.stderr);
    result.stdout
}

#[tokio::test]
async fn remote_kernel_runs_on_the_remote_and_reuses_the_install() {
    let manager = remote_manager("box", None);
    tokio::time::timeout(START_TIMEOUT, manager.start(KernelStartOptions::default()))
        .await
        .expect("remote kernel starts in time")
        .expect("remote kernel starts");
    let home = fake_ssh_base().join("home-box");
    let stdout = run(
        &manager,
        "import os, sys, edit\nprint(os.getcwd())\nprint(sys.executable)\nprint(os.environ['PRIME_AGENT_CODING_AGENT_DIR'])",
    )
    .await;
    let lines: Vec<&str> = stdout.lines().collect();
    let remote_root = home.join(".prime/agent/remote");
    assert_eq!(
        lines[0],
        home.to_string_lossy(),
        "the login dir is the default cwd"
    );
    assert!(
        Path::new(lines[1]).starts_with(&remote_root),
        "the kernel runs the remote venv: {}",
        lines[1]
    );
    assert_eq!(lines[2], remote_root.join("agent").to_string_lossy());
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
    assert_eq!(install_count("box"), 1);

    // A second kernel on the same host skips the upload and honors cwd.
    let work = home.join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    let second = remote_manager("box", Some(work.to_string_lossy().to_string()));
    tokio::time::timeout(START_TIMEOUT, second.start(KernelStartOptions::default()))
        .await
        .expect("second kernel starts in time")
        .expect("second kernel starts");
    let stdout = run(&second, "import os\nprint(os.getcwd())").await;
    assert_eq!(stdout.trim(), work.to_string_lossy());
    second
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
    assert_eq!(install_count("box"), 1, "the cached install is reused");
}

#[tokio::test]
async fn an_unsupported_remote_python_fails_the_start_clearly() {
    let manager = remote_manager("oldpython", None);
    let error = manager
        .start(KernelStartOptions::default())
        .await
        .expect_err("python 3.8 is rejected");
    assert_eq!(
        format!("{error:#}"),
        "Remote host oldpython has Python 3.8.10 at /usr/bin/python3; the Prime Agent kernel needs Python 3.11 or newer as python3."
    );
    assert_eq!(install_count("oldpython"), 0);
}

#[tokio::test]
async fn a_dropped_connection_fails_the_running_cell_naming_the_host() {
    let manager = remote_manager("dropbox", None);
    tokio::time::timeout(START_TIMEOUT, manager.start(KernelStartOptions::default()))
        .await
        .expect("remote kernel starts in time")
        .expect("remote kernel starts");
    let running = Arc::new(tokio::sync::Notify::new());
    let on_stream = {
        let running = Arc::clone(&running);
        Arc::new(move |chunk: &str, _stream| {
            if chunk.contains("cell-running") {
                running.notify_one();
            }
        })
    };
    let cell = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    "import time\nprint('cell-running', flush=True)\ntime.sleep(600)",
                    ExecuteOptions {
                        on_stream: Some(on_stream),
                        ..ExecuteOptions::default()
                    },
                )
                .await
        })
    };
    running.notified().await;
    let pid = manager.process_id().expect("kernel pid");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("drop the connection");
    let error = tokio::time::timeout(Duration::from_secs(30), cell)
        .await
        .expect("the cell settles instead of hanging")
        .expect("cell task")
        .expect_err("the cell fails");
    assert_eq!(
        format!("{error:#}"),
        "Remote kernel on dropbox exited (code=null): the SSH connection closed or the remote Python died"
    );
    assert!(manager.is_defunct());
}
