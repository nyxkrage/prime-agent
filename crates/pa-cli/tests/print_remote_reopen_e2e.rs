//! Print mode honors a spawned child's kernel placement: a session file
//! whose `kernel-placement.json` names a remote host reopens with
//! `prime-agent -p --resume` on that host, in the recorded remote cwd, with
//! the recorded `env=` (never silently with a local kernel), and a damaged
//! sidecar refuses the open. `PRIME_AGENT_SSH` points at a fake `ssh` that
//! runs the remote command under `env -i` in a per-destination `HOME` (see
//! pa-daemon's `remote_spawn_e2e.rs`).
// Pedantic-gate disposition (fleet-uniform ruling; see kernel_dispose_e2e).
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const FAKE_SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -p|-o) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest="$1"; shift
base="__BASE__"
printf '%s\n' "$*" >> "$base/log-$dest"
home="$base/home-$dest"
mkdir -p "$home"
cd "$home" || exit 255
env -i HOME="$home" PATH="__PATH__" UV_CACHE_DIR="__UV_CACHE__" /bin/sh -c "$*"
"#;

/// A PATH entry holding `name`, from this process's PATH.
fn dir_of(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find(|dir| dir.join(name).is_file())
}

fn run_print(args: &[&str], env: &[(&str, String)]) -> (String, String, i32) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("binary present");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn print_mode_reopens_a_remote_child_on_its_host_and_refuses_a_damaged_placement() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let base = dir.path().join("fake-ssh-base");
    let remote_cwd = dir.path().join("remote-work");
    for path in [&base, &remote_cwd] {
        std::fs::create_dir_all(path).expect("dir");
    }
    let ssh = base.join("fake-ssh");
    let mut remote_path = vec![dir_of("python3").expect("python3 on PATH for the fake remote")];
    remote_path.extend(dir_of("uv"));
    remote_path.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    let uv_cache = std::env::var("UV_CACHE_DIR")
        .unwrap_or_else(|_| format!("{}/.cache/uv", std::env::var("HOME").expect("HOME")));
    std::fs::write(
        &ssh,
        FAKE_SSH
            .replace("__BASE__", &base.to_string_lossy())
            .replace(
                "__PATH__",
                &std::env::join_paths(remote_path)
                    .expect("remote PATH")
                    .to_string_lossy(),
            )
            .replace("__UV_CACHE__", &uv_cache),
    )
    .expect("ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let env = |script: &Value| {
        vec![
            ("HOME", dir.path().to_string_lossy().to_string()),
            (
                "PRIME_AGENT_CODING_AGENT_DIR",
                agent_dir.to_string_lossy().to_string(),
            ),
            ("PRIME_AGENT_FAUX_SCRIPT", script.to_string()),
            ("PRIME_AGENT_SSH", ssh.to_string_lossy().to_string()),
            ("PA_E2E_PASSTHROUGH", "passthrough-e2e-value".to_string()),
        ]
    };
    let socket = dir.path().join("no-daemon.sock");
    let socket = socket.to_string_lossy();

    // One persisted print session, then the placement a remote spawn
    // records beside a child's session.
    let (_, stderr, code) = run_print(
        &["-p", "--daemon-socket", &socket, "seed the session"],
        &env(&json!({ "responses": [{ "text": "seeded" }] })),
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let session = std::fs::read_dir(&sessions)
        .expect("sessions")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("a persisted session");
    let placement = session.with_file_name("kernel-placement.json");
    std::fs::write(
        &placement,
        json!({
            "type": "kernel_placement",
            "host": "box",
            "cwd": remote_cwd.to_string_lossy(),
            "env": { "PA_E2E_LITERAL": "literal-1", "PA_E2E_PASSTHROUGH": null },
        })
        .to_string(),
    )
    .expect("placement");

    let receipt = dir.path().join("receipt.json");
    let cell = format!(
        "import json, os\nwith open({:?}, 'w') as handle:\n    json.dump({{'home': os.environ.get('HOME'), 'cwd': os.getcwd(), 'literal': os.environ.get('PA_E2E_LITERAL'), 'passthrough': os.environ.get('PA_E2E_PASSTHROUGH')}}, handle)",
        receipt.to_string_lossy()
    );
    let script = json!({
        "responses": [
            { "content": [
                { "type": "toolCall", "name": "python_repl", "arguments": { "code": cell } },
            ] },
            { "text": "reopened" },
        ],
    });
    let session_arg = session.to_string_lossy();
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket,
            "--resume",
            &session_arg,
            "run the cell",
        ],
        &env(&script),
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "reopened\n", "stderr: {stderr}");
    // The kernel ran on the fake host, in the recorded cwd, with the
    // literal and the pass-through resolved again from this process's env.
    let canonical = |path: &Path| {
        path.canonicalize()
            .expect("canonical")
            .display()
            .to_string()
    };
    let ran: Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap_or_else(|error| {
            panic!(
                "the cell left no receipt ({error}); ssh log:\n{}\nstderr:\n{stderr}",
                std::fs::read_to_string(base.join("log-box")).unwrap_or_default()
            )
        }))
        .expect("receipt json");
    assert_eq!(
        ran,
        json!({
            "home": base.join("home-box").to_string_lossy(),
            "cwd": canonical(&remote_cwd),
            "literal": "literal-1",
            "passthrough": "passthrough-e2e-value",
        })
    );

    // A damaged placement refuses the open instead of running locally.
    std::fs::write(&placement, "{broken").expect("damage");
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket,
            "--resume",
            &session_arg,
            "run the cell",
        ],
        &env(&script),
    );
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(
        stderr.contains(
            "this session's kernel placement (remote host, spawn env) is unreadable; it will not reopen without it"
        ),
        "{stderr}"
    );
}
