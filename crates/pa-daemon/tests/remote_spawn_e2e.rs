//! `rlm.spawn(..., host=...)` through the real supervisor: a root session
//! spawns a remote child, the remote child's own children inherit its host
//! unless they spawn with `host="local"`, and the roster names the host.
//! `PRIME_AGENT_SSH` points at a fake `ssh` that runs the remote command
//! locally under a per-destination `HOME`, so the destination behaves like
//! a separate machine; the opt-in live check against a real host is
//! `pa-core`'s `remote_kernel_live.rs`.
// Pedantic-gate dispositions (fleet-uniform ruling; see kernel_dispose_e2e).
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The first remote spawn provisions the fake host's venv (uv, from its
/// warm cache): the bound covers a cold install, not a fixed wait.
const RECEIPT_TIMEOUT: Duration = Duration::from_mins(10);

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
exec sh -c "$*"
"#;

/// One cell for every session in the tree (the create's `childScript`
/// rides the identity down the recursion, so all sessions replay it). The
/// role comes from where the kernel runs (the fake host's `HOME`) and an
/// atomic `mkdir` claim: the first local kernel is the root, the first
/// remote one is the remote child.
const CELL: &str = r#"
import json, os
shared = __SHARED__
remote = os.environ.get("HOME", "").endswith("/home-box")
def claim(name):
    try:
        os.mkdir(os.path.join(shared, name))
        return True
    except FileExistsError:
        return False
def receipt(role):
    with open(os.path.join(shared, "receipt-" + role + ".json"), "w") as handle:
        json.dump({"home": os.environ.get("HOME"), "cwd": os.getcwd()}, handle)
if not remote and claim("root"):
    receipt("root")
    print(await rlm.spawn("work remotely", name="vps", host="box", cwd=__REMOTE_CWD__))
elif remote and claim("vps"):
    receipt("vps")
    print(await rlm.spawn("stay on the host", name="inherit"))
    print(await rlm.spawn("come back", name="escape", host="local"))
elif remote:
    receipt("inherit")
else:
    receipt("escape")
"#;

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The local kernel Python (the resolution the other kernel e2es use).
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_E2E_KERNEL_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let candidate =
        PathBuf::from(std::env::var("HOME").ok()?).join(".prime/agent/kernel-venv/bin/python");
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping the remote spawn e2e",
        candidate.display()
    );
    None
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(15);
        let writer = loop {
            if let Ok(stream) = UnixStream::connect(socket) {
                break stream;
            }
            assert!(Instant::now() < deadline, "supervisor socket never came up");
            std::thread::sleep(Duration::from_millis(20));
        };
        let reader = BufReader::new(writer.try_clone().expect("clone"));
        let mut client = Self { reader, writer };
        assert_eq!(client.read_line()["type"], "daemon_hello");
        client
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => panic!("daemon closed the socket"),
            Ok(_) => serde_json::from_str(line.trim()).expect("line json"),
            Err(error) => panic!("read failed: {error}"),
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": {"name": "prime-agent.daemon", "version": 7},
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("write");
        self.writer.flush().expect("flush");
        loop {
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// Every session file under `dir` with its tool results: a stalled tree
/// shows which spawn or cell failed and why.
fn session_tails(dir: &Path) -> String {
    let mut out = String::new();
    for entry in walk(dir) {
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        let _ = writeln!(out, "== {}", entry.display());
        for line in text.lines().filter(|line| line.contains("toolResult")) {
            let clipped: String = line.chars().take(1500).collect();
            out.push_str(&clipped);
            out.push('\n');
        }
    }
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
    files
}

fn read_receipt(shared: &Path, role: &str) -> Option<Value> {
    let bytes = std::fs::read_to_string(shared.join(format!("receipt-{role}.json"))).ok()?;
    serde_json::from_str(&bytes).ok()
}

#[test]
fn a_remote_child_runs_remotely_its_children_inherit_and_local_escapes() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp dir");
    let base = dir.path().join("fake-ssh-base");
    let shared = dir.path().join("shared");
    let remote_cwd = dir.path().join("remote-work");
    for path in [&base, &shared, &remote_cwd] {
        std::fs::create_dir_all(path).expect("dir");
    }
    let ssh = base.join("fake-ssh");
    std::fs::write(&ssh, FAKE_SSH.replace("__BASE__", &base.to_string_lossy())).expect("ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let cell = CELL
        .replace("__SHARED__", &format!("{:?}", shared.to_string_lossy()))
        .replace(
            "__REMOTE_CWD__",
            &format!("{:?}", remote_cwd.to_string_lossy()),
        );
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "python_repl", "arguments": { "code": cell } },
                ] },
                { "text": "done" },
            ],
        })
        .to_string(),
    )
    .expect("write script");

    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let log_path = dir.path().join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("log");
    let _daemon = Daemon {
        child: Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
            .arg("supervisor")
            .arg("--socket")
            .arg(&socket)
            .arg("--agent-dir")
            .arg(&agent_dir)
            .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
            .env("PRIME_AGENT_KERNEL_PYTHON", &kernel_python)
            .env("PRIME_AGENT_SSH", &ssh)
            // Local kernels import this checkout's `rlm` (the installed
            // kernel venv may predate `rlm.spawn(host=...)`); remote ones
            // run the uploaded checkout runtime.
            .env(
                "PYTHONPATH",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime/src"),
            )
            .env_remove("PRIME_API_KEY")
            .env_remove("RLM_DEPTH")
            .env(
                pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
                "15000",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn pa-daemon supervisor"),
    };
    let mut client = Client::connect(&socket);

    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let created = client.request(
        "create",
        &json!({
            "type": "create",
            "name": "root",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions.to_string_lossy(),
                "script": script.to_string_lossy(),
                // Every `rlm.spawn` child (and theirs) replays the same script.
                "childScript": script.to_string_lossy(),
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    let root = created["data"]["activeSessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let prompted = client.request(
        "prompt",
        &json!({ "type": "prompt_and_wait", "activeSessionId": root, "message": "go" }),
    );
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");

    let roles = ["root", "vps", "inherit", "escape"];
    let deadline = Instant::now() + RECEIPT_TIMEOUT;
    let receipts = loop {
        let receipts: Vec<Option<Value>> = roles
            .iter()
            .map(|role| read_receipt(&shared, role))
            .collect();
        if receipts.iter().all(Option::is_some) {
            break receipts.into_iter().map(Option::unwrap).collect::<Vec<_>>();
        }
        assert!(
            Instant::now() < deadline,
            "receipts {:?} incomplete; ssh log:\n{}\nsession files:\n{}\ndaemon log:\n{}",
            roles
                .iter()
                .zip(&receipts)
                .map(|(role, receipt)| format!("{role}={}", receipt.is_some()))
                .collect::<Vec<_>>(),
            std::fs::read_to_string(base.join("log-box")).unwrap_or_default(),
            session_tails(dir.path()),
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    let remote_home = base.join("home-box").to_string_lossy().to_string();
    let local_home = std::env::var("HOME").unwrap_or_default();
    let canonical = |path: &Path| {
        path.canonicalize()
            .expect("canonical")
            .display()
            .to_string()
    };
    assert_eq!(
        receipts,
        vec![
            // The root runs locally in its own cwd.
            json!({ "home": local_home, "cwd": canonical(dir.path()) }),
            // The remote child runs on the host, in the requested cwd.
            json!({ "home": remote_home, "cwd": canonical(&remote_cwd) }),
            // Its plain child inherits the host (the remote login dir).
            json!({ "home": remote_home, "cwd": canonical(Path::new(&remote_home)) }),
            // `host="local"` brings a grandchild back to this machine.
            json!({ "home": local_home, "cwd": canonical(dir.path()) }),
        ]
    );
    // Exactly two kernels launched over ssh (the remote child and its
    // inheriting child); the escaped grandchild never touched the host.
    let ssh_log = std::fs::read_to_string(base.join("log-box")).expect("ssh log");
    assert_eq!(ssh_log.matches("-m rlm.repl").count(), 2, "{ssh_log}");

    // The roster names the remote child's host.
    let children = client.request(
        "children",
        &json!({ "type": "get_rlm_children", "activeSessionId": root }),
    );
    assert_eq!(children["success"], true, "{children}");
    // The root's only direct child is the remote one (its own children
    // belong to its roster, not the root's).
    let hosts: Vec<(Value, Value)> = children["data"]["children"]
        .as_array()
        .expect("children rows")
        .iter()
        .map(|row| (row["sessionName"].clone(), row["host"].clone()))
        .collect();
    assert_eq!(hosts, vec![(json!("vps"), json!("box"))], "{children}");
}
