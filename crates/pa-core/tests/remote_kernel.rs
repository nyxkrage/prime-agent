//! Remote (SSH) kernels against a realistic fake `ssh`: provisioning, the
//! ssh hardening options and env allowlist, the remote kernel's env, the
//! install cache and its lock, garbage collection, refused host requests,
//! interrupts, login-shell noise, a dropped connection, and teardown.
//!
//! The fake `ssh` (`KernelManagerOptions::env["PRIME_AGENT_SSH"]`, so no
//! process env is mutated) parses its options like ssh, records its argv
//! and its own env, and runs the remote command as a child under
//! `env -i HOME=... PATH=...`: nothing of this process's env (PYTHONPATH,
//! credentials) reaches the "remote", so the uploaded runtime must work on
//! its own, and the fake ssh is a real intermediate process between the
//! manager and the remote Python, like sshd. A destination's `HOME` is
//! keyed by its host part (`user@host` shares `host`'s machine), so tests
//! running in parallel on one machine also exercise the install lock.
#![cfg(unix)]
// Kernel manager futures follow the crate's existing integration-test disposition.
#![allow(clippy::large_futures)]

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteResult, ExecuteStatus, KernelManagerOptions, KernelShutdownOptions,
    RemoteKernelTarget,
};
use pa_core::kernel::{AbortSignal, KernelPythonSkill};

/// Provisioning builds a venv with uv (from its warm cache), so the bound
/// covers a cold install, not a fixed wait.
const START_TIMEOUT: Duration = Duration::from_mins(10);
/// How long a torn-down kernel's processes may take to disappear.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

const FAKE_SSH: &str = r#"#!/bin/sh
# Fake ssh: parse the options like ssh, record argv and this process's env,
# then run the remote command as a child under a clean env (like sshd).
base="__BASE__"
argv="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -o|-p) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest="$1"; shift
printf '%s\n' "$argv" >> "$base/argv-$dest"
env | sort > "$base/ssh-env-$dest"
printf '%s\n' "$*" >> "$base/log-$dest"
machine="${dest##*@}"
home="$base/home-$machine"
mkdir -p "$home"
cd "$home" || exit 255
path="__PATH__"
case "$dest" in
  oldpython) path="$base/oldpython-bin:$path" ;;
  # Login-shell rc noise, including a line that parses as a protocol frame.
  noisy@*) echo 'Welcome to the box'; echo '{"event": "ready", "protocol": 99}' ;;
esac
env -i HOME="$home" PATH="$path" UV_CACHE_DIR="__UV_CACHE__" /bin/sh -c "$*"
"#;

const OLD_PYTHON: &str = r#"#!/bin/sh
echo '{"version": [3, 8, 10], "executable": "/usr/bin/python3", "venv": true, "uv": null, "system": "Linux", "machine": "x86_64", "root": "/r", "dir": "/r/k", "installed": false, "stale_skills": []}'
"#;

/// A PATH entry holding `name`, from this process's PATH.
fn dir_of(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find(|dir| dir.join(name).is_file())
}

/// The fake-ssh base dir, installed once per test binary under cargo's
/// target tmp dir. A static never drops, so each run first removes the
/// bases of earlier runs whose process is gone (they hold whole venvs).
fn fake_ssh_base() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        for entry in std::fs::read_dir(tmp).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(pid) = name
                .strip_prefix("remote-kernel-")
                .and_then(|pid| pid.parse::<i32>().ok())
            {
                if !alive(pid) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let base = tmp.join(format!("remote-kernel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("fake ssh base");
        let python = dir_of("python3").expect("python3 on PATH for the fake remote");
        let mut path = vec![python];
        // A uv on the fake remote installs from the warm cache.
        path.extend(dir_of("uv"));
        path.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        let uv_cache = std::env::var("UV_CACHE_DIR")
            .unwrap_or_else(|_| format!("{}/.cache/uv", std::env::var("HOME").expect("HOME")));
        let ssh = base.join("fake-ssh");
        std::fs::write(
            &ssh,
            FAKE_SSH
                .replace("__BASE__", &base.to_string_lossy())
                .replace(
                    "__PATH__",
                    &std::env::join_paths(path).unwrap().to_string_lossy(),
                )
                .replace("__UV_CACHE__", &uv_cache),
        )
        .expect("write fake ssh");
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let old_bin = base.join("oldpython-bin");
        std::fs::create_dir_all(&old_bin).expect("old python dir");
        std::fs::write(old_bin.join("python3"), OLD_PYTHON).expect("write old python");
        std::fs::set_permissions(
            old_bin.join("python3"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        base
    })
}

fn remote_root(machine: &str) -> PathBuf {
    fake_ssh_base()
        .join(format!("home-{machine}"))
        .join(".prime/agent/remote")
}

fn base_builds(machine: &str) -> usize {
    std::fs::read_to_string(remote_root(machine).join("install-history.log"))
        .unwrap_or_default()
        .lines()
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

/// Local kernel env the remote kernel must never see.
const LOCAL_ONLY_ENV: [(&str, &str); 4] = [
    ("PRIME_AGENT_BASH_COMMAND_PREFIX", "local-prefix"),
    ("SERPER_API_KEY", "local-secret"),
    ("RLM_HARNESS_STATE_DIR", "/local/harness"),
    ("PRIME_AGENT_CODING_AGENT_DIR", "/local/agent"),
];

fn remote_manager(host: &str, cwd: Option<String>) -> ReplKernelManager {
    ReplKernelManager::new(remote_manager_options(host, cwd))
}

fn remote_manager_options(host: &str, cwd: Option<String>) -> KernelManagerOptions {
    let mut env: HashMap<String, String> = LOCAL_ONLY_ENV
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    env.insert("RLM_SESSION_ID".to_string(), "session-1".to_string());
    env.insert(
        "PRIME_AGENT_SSH".to_string(),
        fake_ssh_base()
            .join("fake-ssh")
            .to_string_lossy()
            .to_string(),
    );
    KernelManagerOptions {
        python_skills: vec![edit_skill()],
        env,
        remote: Some(RemoteKernelTarget::validated(host.to_string(), cwd).expect("valid target")),
        ..KernelManagerOptions::default()
    }
}

async fn start(manager: &ReplKernelManager) {
    tokio::time::timeout(START_TIMEOUT, manager.start(KernelStartOptions::default()))
        .await
        .expect("remote kernel starts in time")
        .expect("remote kernel starts");
}

async fn shutdown(manager: &ReplKernelManager) {
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

/// Start a cell that prints a marker and sleeps; returns once it runs.
async fn sleeping_cell(
    manager: &ReplKernelManager,
    signal: Option<AbortSignal>,
) -> tokio::task::JoinHandle<anyhow::Result<ExecuteResult>> {
    let running = Arc::new(tokio::sync::Notify::new());
    let on_stream = {
        let running = Arc::clone(&running);
        Arc::new(move |chunk: &str, _stream| {
            if chunk.contains("cell-running") {
                running.notify_one();
            }
        })
    };
    let manager = manager.clone();
    let cell = tokio::spawn(async move {
        manager
            .execute(
                "import time\nprint('cell-running', flush=True)\ntime.sleep(600)",
                ExecuteOptions {
                    on_stream: Some(on_stream),
                    signal,
                    ..ExecuteOptions::default()
                },
            )
            .await
    });
    running.notified().await;
    cell
}

async fn run(manager: &ReplKernelManager, code: &str) -> String {
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .expect("cell runs");
    assert_eq!(result.status, ExecuteStatus::Ok, "{}", result.stderr);
    result.stdout
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Wait (bounded) until none of `pids` is alive; returns the survivors.
async fn wait_gone(pids: &[i32]) -> Vec<i32> {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        let survivors: Vec<i32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
        if survivors.is_empty() || Instant::now() > deadline {
            return survivors;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The kernel pid and a live `bash()` child's pid (its own process group).
async fn kernel_and_bash_pids(manager: &ReplKernelManager) -> (i32, i32) {
    let stdout = run(
        manager,
        "import os\nfrom rlm import bash\nh = bash('sleep 600')\nprint(os.getpid(), h.pid)",
    )
    .await;
    let pids: Vec<i32> = stdout
        .split_whitespace()
        .map(|pid| pid.parse().expect("pid"))
        .collect();
    (pids[0], pids[1])
}

#[tokio::test]
async fn a_remote_kernel_gets_hardened_ssh_and_only_allowlisted_env() {
    let manager = remote_manager("box", None);
    start(&manager).await;
    let home = fake_ssh_base().join("home-box");
    let stdout = run(
        &manager,
        "import os, sys, edit\nprint(os.getcwd())\nprint(sys.executable)\nprint(' '.join(sorted(os.environ)))\nprint(os.environ['PRIME_AGENT_CODING_AGENT_DIR'])",
    )
    .await;
    let lines: Vec<&str> = stdout.lines().collect();
    let root = remote_root("box");
    assert_eq!(
        lines[0],
        home.to_string_lossy(),
        "the login dir is the default cwd"
    );
    assert!(
        Path::new(lines[1]).starts_with(&root),
        "the kernel runs the remote venv: {}",
        lines[1]
    );
    // The remote kernel's env: the fake remote's own (HOME, PATH,
    // UV_CACHE_DIR, what /bin/sh adds, and Python's C-locale coercion), the
    // one forwarded key, and the launcher's keys. No local-only key arrives.
    let keys: BTreeSet<&str> = lines[2].split(' ').collect();
    let shell_added: BTreeSet<&str> = ["PWD", "SHLVL", "_", "OLDPWD", "LC_CTYPE"].into();
    let keys: BTreeSet<&str> = keys.difference(&shell_added).copied().collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "HOME",
            "PATH",
            "UV_CACHE_DIR",
            "RLM_SESSION_ID",
            "PRIME_AGENT_CODING_AGENT_DIR",
            "PRIME_AGENT_KERNEL_OWNER_PID",
            "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL",
        ]),
        "the launcher's RLM_REMOTE_* markers stay out of the kernel's os.environ"
    );
    assert_eq!(
        lines[3],
        root.join("agent").to_string_lossy(),
        "a remote scratch agent dir"
    );
    shutdown(&manager).await;

    // Every ssh invocation carries the hardening options before `--`.
    let argv = std::fs::read_to_string(fake_ssh_base().join("argv-box")).expect("argv log");
    for line in argv.lines() {
        assert!(
            line.starts_with(
                "-T -o BatchMode=yes -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o ControlMaster=no -o ConnectTimeout=20 -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -- box "
            ),
            "{line}"
        );
    }
    // The ssh process itself only inherits the env allowlist (plus what
    // /bin/sh adds to its own env).
    let ssh_env = std::fs::read_to_string(fake_ssh_base().join("ssh-env-box")).expect("ssh env");
    for line in ssh_env.lines() {
        let key = line.split('=').next().unwrap_or_default();
        assert!(
            [
                "PATH",
                "HOME",
                "USER",
                "SSH_AUTH_SOCK",
                "LANG",
                "KRB5CCNAME",
                "PWD",
                "SHLVL",
                "_",
                "OLDPWD"
            ]
            .contains(&key)
                || key.starts_with("LC_"),
            "the ssh process inherited {key}"
        );
    }

    // A second kernel on the same machine skips the base install and
    // honors cwd; a `user@` destination is the same machine.
    let work = home.join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    let second = remote_manager("again@box", Some(work.to_string_lossy().to_string()));
    start(&second).await;
    assert_eq!(
        run(&second, "import os\nprint(os.getcwd())").await.trim(),
        work.to_string_lossy()
    );
    shutdown(&second).await;
    assert_eq!(base_builds("box"), 1, "one base build per machine");
    let probe_only = std::fs::read_to_string(fake_ssh_base().join("log-again@box")).expect("log");
    assert_eq!(
        probe_only.lines().count(),
        2,
        "the second kernel only probes and launches:\n{probe_only}"
    );
}

/// `rlm.spawn(env=...)` on a remote kernel: the literal and the
/// passed-through value (resolved from the kernel env seam, never this
/// process's env) reach the remote `os.environ` over stdin, appear nowhere
/// in the ssh argv, the remote command, or the kernel's own command line
/// (what the remote `ps` shows), and the pass-through is masked when the
/// kernel prints it.
#[tokio::test]
async fn spawn_env_reaches_the_remote_kernel_over_stdin_and_never_the_command_line() {
    const LITERAL: &str = "literal-value-5d1f7a";
    const SECRET: &str = "passthrough-secret-9c2e44";
    let mut options = remote_manager_options("env@box", None);
    options
        .env
        .insert("PA_TEST_PASSTHROUGH".to_string(), SECRET.to_string());
    options.env_overrides = pa_core::kernel::KernelEnvOverrides::from_json(&serde_json::json!({
        "PA_TEST_LITERAL": LITERAL,
        "PA_TEST_PASSTHROUGH": null,
    }))
    .expect("valid overrides");
    let manager = ReplKernelManager::new(options);
    start(&manager).await;
    let stdout = run(
        &manager,
        "import os\nprint(os.getpid())\nprint(os.environ['PA_TEST_LITERAL'])\nprint(os.environ['PA_TEST_PASSTHROUGH'][::-1])\nprint(os.environ['PA_TEST_PASSTHROUGH'])",
    )
    .await;
    let lines: Vec<&str> = stdout.lines().collect();
    let reversed: String = SECRET.chars().rev().collect();
    assert_eq!(lines[1..], [LITERAL, reversed.as_str(), "***"]);
    let kernel_pid: i32 = lines[0].parse().expect("kernel pid");
    // The fake remote runs on this machine: the remote kernel's command
    // line is readable like the remote `ps` would show it.
    let cmdline = std::fs::read(format!("/proc/{kernel_pid}/cmdline")).expect("kernel cmdline");
    let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    assert!(cmdline.contains("rlm.repl"), "{cmdline}");
    shutdown(&manager).await;
    let argv = std::fs::read_to_string(fake_ssh_base().join("argv-env@box")).expect("argv log");
    let remote_commands =
        std::fs::read_to_string(fake_ssh_base().join("log-env@box")).expect("remote command log");
    assert!(remote_commands.contains("-m rlm.repl"), "{remote_commands}");
    for (what, text) in [
        ("ssh argv", &argv),
        ("remote command", &remote_commands),
        ("kernel cmdline", &cmdline),
    ] {
        for value in [LITERAL, SECRET] {
            assert!(
                !text.contains(value),
                "{value:?} leaked into the {what}: {text}"
            );
        }
    }
}

#[tokio::test]
async fn teardown_leaves_no_remote_processes() {
    let manager = remote_manager("teardown@box", None);
    start(&manager).await;
    let (kernel, bash) = kernel_and_bash_pids(&manager).await;
    assert!(alive(kernel) && alive(bash));
    shutdown(&manager).await;
    assert_eq!(wait_gone(&[kernel, bash]).await, Vec::<i32>::new());
    assert!(
        !remote_root("box")
            .join(format!("run/{kernel}.kernel"))
            .exists(),
        "a clean exit drops its run record"
    );
}

#[tokio::test]
async fn a_remote_kernel_refuses_host_requests_and_features_outside_the_allowlist() {
    let manager = remote_manager("refuse@box", None);
    start(&manager).await;
    let stdout = run(
        &manager,
        r#"
import rlm
async def attempt(label, call):
    try:
        await call()
        print(label, "-> allowed")
    except Exception as error:
        print(label, "->", error)
for kind in ["mcp.config", "mcp.refresh", "refine.status", "goal.get", "rlm_heartbeat.list", "agent_observe.list", "rlm.create_session", "brand.new", "rlm.find_models"]:
    await attempt(kind, lambda kind=kind: rlm.host_request(kind))
async def harness():
    rlm.harness.list()
await attempt("rlm.harness", harness)
async def persistent():
    rlm.bash("true", persistent=True)
await attempt("bash(persistent=True)", persistent)
"#,
    )
    .await;
    let refused = |kind: &str, reason: &str| {
        format!("{kind} -> host request \"{kind}\" is not available on remote kernels yet (this kernel runs on refuse@box): {reason}")
    };
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            refused("mcp.config", "MCP server configs carry local credentials (env, headers, OAuth)"),
            refused("mcp.refresh", "MCP server configs carry local credentials (env, headers, OAuth)"),
            refused("refine.status", "harness refinement state lives on this machine"),
            refused("goal.get", "goal state is not audited for remote kernels"),
            refused("rlm_heartbeat.list", "heartbeat jobs schedule wake-ups on this machine"),
            refused("agent_observe.list", "it reads other agents' local transcripts"),
            refused("rlm.create_session", "it creates a local top-level session from a kernel-supplied cwd"),
            refused("brand.new", "it has not been audited for remote kernels"),
            // Allowed by policy; this bare manager registers no handler.
            "rlm.find_models -> host request type \"rlm.find_models\" is not available in this session".to_string(),
            "rlm.harness -> rlm.harness is not available on remote kernels yet (this kernel runs on refuse@box): harness state lives on the orchestrator's machine".to_string(),
            "bash(persistent=True) -> bash(persistent=True) is not available on remote kernels yet (this kernel runs on refuse@box): a persistent shell outlives its kernel, and nothing on the orchestrator's machine can reattach or reap it there".to_string(),
        ]
    );
    shutdown(&manager).await;
}

/// Also behind a noisy login shell: rc output (even a line that parses as
/// a protocol frame) before the launcher's sentinel is ignored.
#[tokio::test]
async fn an_interrupt_reaches_the_remote_kernel_behind_login_shell_noise() {
    let manager = remote_manager("noisy@box", None);
    start(&manager).await;
    let signal = AbortSignal::new();
    let cell = sleeping_cell(&manager, Some(signal.clone())).await;
    signal.abort();
    let result = tokio::time::timeout(Duration::from_secs(30), cell)
        .await
        .expect("the interrupted cell settles")
        .expect("cell task")
        .expect("cell result");
    assert_eq!(result.status, ExecuteStatus::Aborted, "{result:?}");
    assert_eq!(
        run(&manager, "print('still alive')").await.trim(),
        "still alive"
    );
    shutdown(&manager).await;
}

/// A target is only built through `RemoteKernelTarget::validated`, so a
/// host or cwd outside the allowlist never reaches a manager, let alone ssh.
#[test]
fn hosts_and_cwds_outside_the_allowlist_are_refused() {
    for (host, cwd, expected) in [
        (
            "box$(touch${IFS}/tmp/pwned)",
            None,
            "rlm.spawn host \"box$(touch${IFS}/tmp/pwned)\" contains '$'; an SSH destination may only use letters, digits, and . _ @ [ ] : -",
        ),
        (
            "-oProxyCommand=id",
            None,
            "rlm.spawn host \"-oProxyCommand=id\" contains '='; an SSH destination may only use letters, digits, and . _ @ [ ] : -",
        ),
        (
            "cwd@box",
            Some("/tmp/a b"),
            "rlm.spawn cwd \"/tmp/a b\" contains ' '; remote paths may only use letters, digits, and . _ / + - (with an optional leading ~)",
        ),
        (
            "cwd@box",
            Some("/tmp/it's"),
            "rlm.spawn cwd \"/tmp/it's\" contains '\\''; remote paths may only use letters, digits, and . _ / + - (with an optional leading ~)",
        ),
        (
            "cwd@box",
            Some("work"),
            "rlm.spawn cwd must be an absolute or ~-relative remote path, got \"work\"",
        ),
    ] {
        let error = RemoteKernelTarget::validated(host.to_string(), cwd.map(str::to_string))
            .expect_err("refused");
        assert_eq!(format!("{error:#}"), expected);
    }
}

#[tokio::test]
async fn concurrent_installs_build_once_recover_and_collect_garbage() {
    let root = remote_root("twin");
    // Old installs: one this tool created and nobody used for 8 days, one
    // without the marker (not ours), and one recent install of ours.
    let old = root.join("v2-0000000000000000");
    let foreign = root.join("v2-1111111111111111");
    let recent = root.join("v2-2222222222222222");
    for dir in [&old, &foreign, &recent] {
        std::fs::create_dir_all(dir).expect("old install");
    }
    for dir in [&old, &recent] {
        std::fs::write(
            dir.join(".prime-agent-remote"),
            dir.file_name().unwrap().to_string_lossy().as_bytes(),
        )
        .expect("marker");
    }
    let eight_days_ago = std::time::SystemTime::now() - Duration::from_hours(8 * 24);
    for dir in [&old, &foreign] {
        let marker = dir.join(".prime-agent-remote");
        if marker.exists() {
            std::fs::File::options()
                .write(true)
                .open(&marker)
                .unwrap()
                .set_modified(eight_days_ago)
                .unwrap();
        }
    }

    // Two kernels start at once on a fresh machine: one builds, the other
    // waits on the install lock and reuses it.
    let (first, second) = (
        remote_manager("one@twin", None),
        remote_manager("two@twin", None),
    );
    tokio::join!(start(&first), start(&second));
    assert_eq!(base_builds("twin"), 1);
    assert_eq!(run(&first, "print(1)").await.trim(), "1");
    assert_eq!(run(&second, "print(2)").await.trim(), "2");
    let install_dir = run(
        &first,
        "import os, sys\nprint(os.path.dirname(os.path.dirname(os.path.dirname(sys.executable))))",
    )
    .await;
    let install_dir = PathBuf::from(install_dir.trim());
    for manager in [&first, &second] {
        shutdown(manager).await;
    }
    assert!(!old.exists(), "an idle install of ours is collected");
    assert!(
        foreign.exists(),
        "a dir without our marker is never touched"
    );
    assert!(recent.exists(), "a recently used install is kept");

    // An install interrupted before its stamp (and a leftover, unheld
    // lock file) is rebuilt by the next start.
    std::fs::remove_file(install_dir.join(".installed")).expect("drop the stamp");
    std::fs::remove_file(install_dir.join("venv/bin/python")).expect("break the venv");
    assert!(PathBuf::from(format!("{}.lock", install_dir.display())).exists());
    let third = remote_manager("three@twin", None);
    start(&third).await;
    assert_eq!(base_builds("twin"), 2);
    assert_eq!(run(&third, "import edit\nprint('ok')").await.trim(), "ok");
    shutdown(&third).await;
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
    assert_eq!(
        std::fs::read_to_string(fake_ssh_base().join("log-oldpython"))
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "only the probe ran"
    );
}

#[tokio::test]
async fn a_dropped_connection_fails_the_cell_and_the_remote_kernel_exits() {
    let manager = remote_manager("drop@box", None);
    start(&manager).await;
    let (kernel, bash) = kernel_and_bash_pids(&manager).await;
    let cell = sleeping_cell(&manager, None).await;
    // The local ssh process dies (the connection drops): its remote child
    // loses its parent, like a kernel whose sshd session ended.
    let ssh = manager.process_id().expect("ssh pid");
    assert_ne!(
        ssh, kernel,
        "a real process sits between ssh and the kernel"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(ssh),
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
        "Remote kernel on drop@box exited (code=null): the SSH connection closed or the remote Python died"
    );
    assert!(manager.is_defunct());
    assert_eq!(
        wait_gone(&[kernel, bash]).await,
        Vec::<i32>::new(),
        "the remote kernel and its bash group exit with the connection"
    );
}

/// A minimal Python skill package `pa_pinned` whose `VERSION` is `version`,
/// in a dir named `pinned` (both versions share the skill dir name).
fn pinned_skill(parent: &Path, version: &str) -> KernelPythonSkill {
    let package_path = parent.join("pinned");
    std::fs::create_dir_all(package_path.join("src/pa_pinned")).expect("skill dir");
    std::fs::write(
        package_path.join("pyproject.toml"),
        "[project]\nname = \"pa-pinned\"\nversion = \"0.1.0\"\ndependencies = []\n\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n\n[tool.hatch.build.targets.wheel]\npackages = [\"src/pa_pinned\"]\n",
    )
    .expect("pyproject");
    std::fs::write(
        package_path.join("src/pa_pinned/__init__.py"),
        format!("VERSION = {version:?}\n"),
    )
    .expect("module");
    std::fs::write(package_path.join("SKILL.md"), "---\nname: pinned\n---\n").expect("skill");
    KernelPythonSkill {
        name: "pinned".to_string(),
        import_name: "pa_pinned".to_string(),
        pyproject_path: package_path.join("pyproject.toml"),
        package_path,
    }
}

/// Two sessions on one host run two versions of the same skill: the second
/// install never touches the first kernel's code (each version has its own
/// pinned dir on its kernel's `sys.path`), and both versions' dirs stay
/// for garbage collection instead of being removed inline.
#[tokio::test]
async fn concurrent_sessions_keep_their_own_skill_versions() {
    let sources = tempfile::TempDir::new().expect("skill sources");
    let (one, two) = (sources.path().join("one"), sources.path().join("two"));
    let mut first = remote_manager_options("a@pins", None);
    first.python_skills = vec![edit_skill(), pinned_skill(&one, "one")];
    let first = ReplKernelManager::new(first);
    start(&first).await;
    assert_eq!(
        run(&first, "import pa_pinned\nprint(pa_pinned.VERSION)")
            .await
            .trim(),
        "one"
    );
    let mut second = remote_manager_options("b@pins", None);
    second.python_skills = vec![edit_skill(), pinned_skill(&two, "two")];
    let second = ReplKernelManager::new(second);
    start(&second).await;
    assert_eq!(
        run(&second, "import pa_pinned\nprint(pa_pinned.VERSION)")
            .await
            .trim(),
        "two"
    );
    // The first kernel re-reads its skill from disk: still its own version.
    assert_eq!(
        run(
            &first,
            "import importlib\nimportlib.reload(pa_pinned)\nprint(pa_pinned.VERSION)"
        )
        .await
        .trim(),
        "one"
    );
    let stores: BTreeSet<String> = std::fs::read_dir(remote_root("pins").join("skills"))
        .expect("skill store")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("pinned-") && !name.contains('.'))
        .collect();
    assert_eq!(stores.len(), 2, "both versions stay: {stores:?}");
    // The uploaded skill dirs carry the skill's files but never its
    // dotfiles (the edit skill ships a .gitignore).
    let edit_store = std::fs::read_dir(remote_root("pins").join("skills"))
        .expect("skill store")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("edit-")
        })
        .expect("the edit skill is uploaded");
    assert!(edit_store.join("SKILL.md").is_file());
    assert!(
        !edit_store.join(".gitignore").exists(),
        "a dotfile was uploaded"
    );
    for manager in [&first, &second] {
        shutdown(manager).await;
    }
}

/// A start that fails before `ready` drops the manager's cached install, so
/// the next start probes the host again (and rebuilds a broken install)
/// instead of relaunching the cached one forever. (An exit before `ready`
/// retires the manager anyway; a start that times out keeps it.)
#[tokio::test]
async fn a_failed_start_reprovisions_a_broken_install() {
    let manager = remote_manager("again@rebuild", None);
    start(&manager).await;
    let install_dir = run(
        &manager,
        "import os, sys\nprint(os.path.dirname(os.path.dirname(os.path.dirname(sys.executable))))",
    )
    .await;
    let install_dir = PathBuf::from(install_dir.trim());
    // The install breaks on the host: its interpreter hangs and its stamp is
    // gone. (The venv python is a symlink: unlink it, never write through.)
    let python = install_dir.join("venv/bin/python");
    std::fs::remove_file(&python).expect("unlink the venv python");
    std::fs::write(&python, "#!/bin/sh\nexec sleep 45\n").expect("hanging python");
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::remove_file(install_dir.join(".installed")).expect("drop the stamp");
    // The cached install never reaches `ready`: this start times out...
    assert!(manager.restart().await.is_err());
    // ...and the next one probes again, rebuilds, and runs.
    start(&manager).await;
    assert_eq!(run(&manager, "print('rebuilt')").await.trim(), "rebuilt");
    assert_eq!(base_builds("rebuild"), 2);
    shutdown(&manager).await;
}
