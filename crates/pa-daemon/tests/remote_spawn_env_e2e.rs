//! `rlm.spawn(..., env={...})` through the real supervisor. The daemon's
//! own env carries a pass-through secret (the orchestrator's env; no test
//! process env is mutated): a remote child sees the literal and the
//! passed-through value, its nested child inherits neither, a local child
//! gets its literal, refusals (reserved and malformed names, non-str
//! values, an oversize payload, a missing pass-through) name what failed,
//! the secret is masked in the transcript and appears in no file the
//! daemon writes (sessions, descriptors, logs) nor in the ssh argv or the
//! remote command, and the global `remoteEnvPassthrough` kill switch
//! refuses pass-throughs to a host (a project setting cannot flip it)
//! while literal values stay allowed. `PRIME_AGENT_SSH` points at a fake
//! `ssh` that records its argv and runs the remote command under `env -i`
//! with a per-destination `HOME` (see `remote_spawn_e2e.rs`).
// Pedantic-gate dispositions (fleet-uniform ruling; see kernel_dispose_e2e).
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

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
# Fake ssh: record the argv, parse the options like ssh, then run the
# remote command as a child under a clean env (like sshd), in a
# per-destination HOME.
base="__BASE__"
printf '%s\n' "$*" >> "$base/argv"
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -p|-o) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest="$1"; shift
printf '%s\n' "$*" >> "$base/log-$dest"
home="$base/home-$dest"
mkdir -p "$home"
cd "$home" || exit 255
env -i HOME="$home" PATH="__PATH__" UV_CACHE_DIR="__UV_CACHE__" /bin/sh -c "$*"
"#;

/// One cell for every session in the tree (`childScript` rides the
/// identity down the recursion). The role comes from where the kernel runs
/// (the fake host's `HOME`) and an atomic `mkdir` claim.
const CELL: &str = r#"
import json, os, rlm
shared = __SHARED__
remote = os.environ.get("HOME", "").endswith("/home-box")
def claim(name):
    try:
        os.mkdir(os.path.join(shared, name))
        return True
    except FileExistsError:
        return False
def receipt(role, **extra):
    secret = os.environ.get("PA_E2E_PASSTHROUGH")
    with open(os.path.join(shared, "receipt-" + role + ".json"), "w") as handle:
        json.dump({
            "literal": os.environ.get("PA_E2E_LITERAL"),
            "secret_reversed": secret[::-1] if secret is not None else None,
            **extra,
        }, handle)
async def error_of(call):
    try:
        await call()
        return "allowed"
    except Exception as error:
        return str(error)
if not remote and claim("root"):
    refused = []
    for env in [{"PATH": "/evil"}, {"lower": "x"}, {"PA_E2E_MISSING_VAR": None}]:
        refused.append(await error_of(lambda env=env: rlm.spawn("x", name="bad", host="box", env=env)))
    # Past the kernel wrapper: the host validates on its own.
    for env in [{"EPOCHS": 3}, {"BIG": "x" * 70000}]:
        refused.append(await error_of(lambda env=env: rlm.host_request(
            "rlm.run", {"prompt": "x", "kwargs": {"name": "bad", "host": "box", "env": env}})))
    receipt("root", refused=refused)
    print(await rlm.spawn("work remotely", name="vps", host="box",
                          env={"PA_E2E_LITERAL": __LITERAL__, "PA_E2E_PASSTHROUGH": None}))
    print(await rlm.spawn("work locally", name="loc", env={"PA_E2E_LITERAL": "local-literal"}))
elif remote and claim("vps"):
    receipt("vps")
    print("secret:", os.environ["PA_E2E_PASSTHROUGH"])
    print(await rlm.spawn("stay on the host", name="nested"))
elif remote:
    receipt("nested")
else:
    receipt("loc")
"#;

/// The kill-switch cell: only the root acts.
const KILL_SWITCH_CELL: &str = r#"
import json, os, rlm
shared = __SHARED__
async def error_of(call):
    try:
        await call()
        return "allowed"
    except Exception as error:
        return str(error)
def claim(name):
    try:
        os.mkdir(os.path.join(shared, name))
        return True
    except FileExistsError:
        return False
if not os.environ.get("HOME", "").endswith("/home-box") and claim("root"):
    refused = await error_of(lambda: rlm.spawn("x", name="pass", host="box", env={"PA_E2E_PASSTHROUGH": None}))
    allowed = await error_of(lambda: rlm.spawn("x", name="lit", host="box", env={"PA_E2E_LITERAL": "lit"}))
    local = await error_of(lambda: rlm.spawn("x", name="loc", env={"PA_E2E_PASSTHROUGH": None}))
    with open(os.path.join(shared, "receipt-root.json"), "w") as handle:
        json.dump({"refused": refused, "allowed": allowed, "local": local}, handle)
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

/// The local kernel Python (the resolution the other kernel e2es use). A
/// missing interpreter fails the test: a silent skip would let the remote
/// spawn path go unverified.
fn kernel_python() -> PathBuf {
    if let Some(explicit) = std::env::var_os("PA_E2E_KERNEL_PYTHON") {
        return PathBuf::from(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(".prime/agent/kernel-venv/bin/python");
    assert!(
        candidate.exists(),
        "kernel python {} not found: set PA_E2E_KERNEL_PYTHON to a Python 3.11+ with the kernel runtime's dependencies",
        candidate.display()
    );
    candidate
}

/// A PATH entry holding `name`, from this process's PATH.
fn dir_of(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find(|dir| dir.join(name).is_file())
}

fn walk_all(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            files.extend(walk_all(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    walk_all(dir)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect()
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

fn read_receipt(shared: &Path, role: &str) -> Option<Value> {
    let bytes = std::fs::read_to_string(shared.join(format!("receipt-{role}.json"))).ok()?;
    serde_json::from_str(&bytes).ok()
}

/// A temp tree with the fake ssh, a daemon running with `secret` in its
/// env, and a root session whose script (and every child's) runs `cell`.
struct Fixture {
    dir: tempfile::TempDir,
    base: PathBuf,
    shared: PathBuf,
    log_path: PathBuf,
    _daemon: Daemon,
    client: Client,
}

impl Fixture {
    fn start(cell: &str, secret: &str, global_settings: Option<&str>) -> Self {
        let kernel_python = kernel_python();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let base = dir.path().join("fake-ssh-base");
        let shared = dir.path().join("shared");
        for path in [&base, &shared] {
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

        let cell = cell.replace("__SHARED__", &format!("{:?}", shared.to_string_lossy()));
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
        if let Some(settings) = global_settings {
            std::fs::write(agent_dir.join("settings.json"), settings).expect("global settings");
        }
        // A project setting never re-enables the global kill switch.
        let project = dir.path().join(".prime/agent");
        std::fs::create_dir_all(&project).expect("project settings dir");
        std::fs::write(
            project.join("settings.json"),
            r#"{ "remoteEnvPassthrough": true }"#,
        )
        .expect("project settings");
        let log_path = dir.path().join("daemon.log");
        let log = std::fs::File::create(&log_path).expect("log");
        let daemon = Daemon {
            child: Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
                .arg("supervisor")
                .arg("--socket")
                .arg(&socket)
                .arg("--agent-dir")
                .arg(&agent_dir)
                .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
                .env("PRIME_AGENT_KERNEL_PYTHON", &kernel_python)
                .env("PRIME_AGENT_SSH", &ssh)
                // The orchestrator's env: the pass-through source.
                .env("PA_E2E_PASSTHROUGH", secret)
                .env_remove("PA_E2E_LITERAL")
                .env_remove("PA_E2E_MISSING_VAR")
                // Local kernels import this checkout's `rlm`; remote ones
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
        Self {
            dir,
            base,
            shared,
            log_path,
            _daemon: daemon,
            client,
        }
    }

    /// Wait (bounded) for `ready` over the tree; the panic shows the state.
    fn wait_for<T>(&self, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + RECEIPT_TIMEOUT;
        loop {
            if let Some(value) = ready() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "{what} never happened; ssh log:\n{}\ndaemon log:\n{}",
                std::fs::read_to_string(self.base.join("log-box")).unwrap_or_default(),
                std::fs::read_to_string(&self.log_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn spawn_env_passes_literals_and_passthroughs_and_never_leaks_the_value() {
    let secret = format!("pa-e2e-secret-{}", uuid::Uuid::new_v4().simple());
    let literal = format!("pa-e2e-literal-{}", uuid::Uuid::new_v4().simple());
    let cell = CELL.replace("__LITERAL__", &format!("{literal:?}"));
    let fixture = Fixture::start(&cell, &secret, None);
    let roles = ["root", "vps", "nested", "loc"];
    let receipts: Vec<Value> = fixture.wait_for("every receipt", || {
        roles
            .iter()
            .map(|role| read_receipt(&fixture.shared, role))
            .collect()
    });
    let reversed: String = secret.chars().rev().collect();
    let mut receipts = receipts;
    let refused = receipts[0]
        .as_object_mut()
        .expect("root receipt")
        .remove("refused")
        .expect("refusals");
    assert_eq!(
        receipts,
        vec![
            // The root inherits the daemon's env (the orchestrator's).
            json!({ "literal": null, "secret_reversed": reversed }),
            // The remote child: the literal and the passed-through value.
            json!({ "literal": literal, "secret_reversed": reversed }),
            // Its nested child inherits the host, never the env overrides.
            json!({ "literal": null, "secret_reversed": null }),
            // A local child gets its literal.
            json!({ "literal": "local-literal", "secret_reversed": reversed }),
        ]
    );
    assert_eq!(
        refused,
        json!([
            "rlm.spawn env name PATH is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV",
            "rlm.spawn env name \"lower\" is invalid: names must match [A-Z_][A-Z0-9_]*",
            "PA_E2E_MISSING_VAR is not set on the orchestrator (rlm.spawn env PA_E2E_MISSING_VAR=None passes the orchestrator's value through)",
            "rlm.spawn env value for EPOCHS must be a str or None, got int",
            "rlm.spawn env carries 70003 bytes; at most 65536 are allowed",
        ])
    );
    // The remote child printed the secret: its transcript shows the mask.
    fixture.wait_for("the masked secret in the remote child's transcript", || {
        session_files(fixture.dir.path())
            .iter()
            .any(|path| {
                std::fs::read_to_string(path).is_ok_and(|text| text.contains("secret: ***"))
            })
            .then_some(())
    });
    // Neither value reaches the ssh argv or the remote command, and the
    // secret is in no file the daemon tree wrote (sessions, descriptors,
    // the durable create, logs, the fake host), only the test's receipts.
    let argv = std::fs::read_to_string(fixture.base.join("argv")).expect("ssh argv");
    let remote_commands =
        std::fs::read_to_string(fixture.base.join("log-box")).expect("remote commands");
    assert!(remote_commands.contains("-m rlm.repl"), "{remote_commands}");
    for value in [&literal, &secret] {
        assert!(!argv.contains(value.as_str()), "{value} in the ssh argv");
        assert!(
            !remote_commands.contains(value.as_str()),
            "{value} in the remote command"
        );
    }
    let leaks: Vec<PathBuf> = walk_all(fixture.dir.path())
        .into_iter()
        .filter(|path| !path.starts_with(&fixture.shared))
        .filter(|path| {
            std::fs::read(path).is_ok_and(|bytes| {
                bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
            })
        })
        .collect();
    assert_eq!(leaks, Vec::<PathBuf>::new(), "the pass-through value leaked");
    drop(fixture.client);
}

#[test]
fn the_global_kill_switch_refuses_remote_passthroughs_but_not_literals() {
    let secret = format!("pa-e2e-secret-{}", uuid::Uuid::new_v4().simple());
    let fixture = Fixture::start(
        KILL_SWITCH_CELL,
        &secret,
        Some(r#"{ "remoteEnvPassthrough": false }"#),
    );
    let receipt = fixture.wait_for("the root receipt", || {
        read_receipt(&fixture.shared, "root")
    });
    assert_eq!(
        receipt,
        json!({
            "refused": "rlm.spawn env PA_E2E_PASSTHROUGH=None would pass the orchestrator's value to a remote host, and the global remoteEnvPassthrough setting is off (or unreadable); pass a literal value, or set remoteEnvPassthrough to true in the global settings",
            "allowed": "allowed",
            // The switch governs remote hosts only.
            "local": "allowed",
        })
    );
    drop(fixture.client);
}
