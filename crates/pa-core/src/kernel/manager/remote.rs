//! Remote kernel launch: the SSH transport for the existing process + stdio
//! protocol. The kernel process is `ssh <host> <remote-venv>/bin/python -m
//! rlm.repl`, so requests, events, and host requests ride the SSH channel
//! unchanged, and the local `ssh` process stands in for the kernel pid.
//!
//! What crosses to the remote is governed by
//! [`crate::kernel::remote_policy`]: the forwarded env, the validated
//! `cwd`, and (in `host_requests.rs`) the host requests the remote kernel
//! may send back.
//!
//! The launcher's own env (`exec env K=V ...`) is visible in the remote
//! `ps` and may land in sshd logs, so it carries only non-secret wiring:
//! the scratch agent dir, the host label, the install dir, the heartbeat
//! timeout, and the session id. The spawn's `env=` overrides never touch
//! the command line: the host sends them as one `kernel_env` frame on the
//! kernel's stdin right after the stdio sentinel and before `ready`, and
//! the runtime applies them to `os.environ` before any user code runs.

use std::fmt::Write as _;
use std::time::Duration;

use super::{json, lock, Arc, Inner, KernelStartOptions, KernelState};
use crate::kernel::bootstrap::{
    ensure_remote_kernel_python, sh_quote, RemoteHost, RemoteKernelInstall,
};
use crate::kernel::env_overrides::ResolvedKernelEnv;
use crate::kernel::remote_policy::{validate_remote_path, REMOTE_KERNEL_FORWARDED_ENV};
use crate::kernel::shared::RemoteKernelTarget;

/// The line the remote launcher prints right before `exec`ing the kernel:
/// the stdout reader discards everything before it (login-shell rc output)
/// so only protocol frames reach the parser.
pub(super) const REMOTE_KERNEL_STDIO_SENTINEL: &str = "__PRIME_AGENT_REMOTE_KERNEL_STDIO__";

/// How often the host sends a heartbeat frame to a remote kernel.
const HOST_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// How long a remote kernel waits without any host frame before it exits
/// (and kills its process groups): a partition sshd never notices.
const HOST_HEARTBEAT_TIMEOUT_MS: u64 = 45_000;

impl Inner {
    /// Provision the remote host on first use, then build the `ssh` command
    /// that runs the kernel there. Returns the command and a launch label
    /// for error messages.
    pub(super) async fn remote_kernel_command(
        self: &Arc<Self>,
        target: &RemoteKernelTarget,
        options: &KernelStartOptions,
        resolved_env: &ResolvedKernelEnv,
    ) -> anyhow::Result<(tokio::process::Command, String)> {
        let remote = RemoteHost::new(&target.host, &self.options.env)?;
        if let Some(cwd) = &target.cwd {
            validate_remote_path(cwd, "rlm.spawn cwd")?;
        }
        let cached = lock(&self.resolved_remote).clone();
        let install: RemoteKernelInstall = if let Some(install) = cached {
            install
        } else {
            let install = ensure_remote_kernel_python(
                &remote,
                &self.options.python_skills,
                options.on_bootstrap_progress.as_ref(),
            )
            .await?;
            *lock(&self.resolved_remote) = Some(install.clone());
            install
        };
        // The remote kernel env: the forwarded allowlist plus the
        // launcher's own keys (see `REMOTE_KERNEL_FORWARDED_ENV`).
        let mut env: Vec<(String, String)> = vec![
            (
                "PRIME_AGENT_CODING_AGENT_DIR".to_string(),
                format!("{}/agent", install.root),
            ),
            ("RLM_REMOTE_HOST".to_string(), remote.label()),
            ("RLM_REMOTE_INSTALL_DIR".to_string(), install.dir.clone()),
            (
                "RLM_HOST_HEARTBEAT_TIMEOUT_MS".to_string(),
                HOST_HEARTBEAT_TIMEOUT_MS.to_string(),
            ),
        ];
        for key in REMOTE_KERNEL_FORWARDED_ENV {
            if let Some(value) = self.options.env.get(*key) {
                env.push(((*key).to_string(), value.clone()));
            }
        }
        // A POSIX script run by `/bin/sh` whatever the login shell is: it
        // changes into the cwd, prints the stdio sentinel, and execs the
        // kernel (so the kernel's parent is the remote sshd session).
        let mut script = String::new();
        if let Some(cwd) = &target.cwd {
            let cwd = match cwd.strip_prefix("~/") {
                Some(rest) => format!("~/{}", sh_quote(rest)),
                None if cwd == "~" => "~".to_string(),
                None => sh_quote(cwd),
            };
            let _ = write!(script, "cd -- {cwd} && ");
        }
        let _ = write!(
            script,
            "printf '\\n%s\\n' {REMOTE_KERNEL_STDIO_SENTINEL} && exec env"
        );
        for (key, value) in &env {
            script.push(' ');
            script.push_str(&sh_quote(&format!("{key}={value}")));
        }
        let _ = write!(script, " {} -m rlm.repl", sh_quote(&install.python));
        let mut command = remote.command(&format!("exec /bin/sh -c {}", sh_quote(&script)));
        // Always sent (an empty env too): the runtime of a remote kernel
        // reads exactly one `kernel_env` frame before it serves requests.
        let env_frame: serde_json::Map<String, serde_json::Value> = resolved_env
            .values()
            .iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect();
        *lock(&self.remote_env_frame) = Some(json!({ "type": "kernel_env", "env": env_frame }));
        // The kernel's teardown signals the ssh process; the remote runtime
        // sees stdin EOF (or its sshd parent exit, or the heartbeat stop)
        // and shuts down.
        command.kill_on_drop(false);
        Ok((command, format!("{} on {}", install.python, remote.label())))
    }

    /// Write this start's pending `kernel_env` frame (once) to the remote
    /// kernel's stdin. A local kernel has none.
    pub(super) async fn send_remote_env_frame(&self) -> anyhow::Result<()> {
        let Some(frame) = lock(&self.remote_env_frame).take() else {
            return Ok(());
        };
        self.write_line(&frame).await
    }

    /// Send a heartbeat frame to a remote kernel every
    /// [`HOST_HEARTBEAT_INTERVAL`] while this start generation lives.
    pub(super) fn start_remote_heartbeat(self: &Arc<Self>, generation: u64) {
        let inner = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HOST_HEARTBEAT_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                if inner.current_generation() != generation
                    || lock(&inner.guarded).state == KernelState::Shutdown
                {
                    return;
                }
                if inner
                    .write_line(&json!({ "type": "heartbeat" }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }
}
