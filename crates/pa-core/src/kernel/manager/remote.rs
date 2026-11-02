//! Remote kernel launch: the SSH transport for the existing process + stdio
//! protocol. The kernel process is `ssh <host> <remote-venv>/bin/python -m
//! rlm.repl`, and the local `ssh` process stands in for the kernel pid.
//!
//! The launcher's `exec env K=V ...` is visible in the remote `ps` and may
//! land in sshd logs, so it carries only non-secret wiring. The spawn's
//! `env=` values go as one `kernel_env` frame on the kernel's stdin, right
//! after the stdio sentinel and before `ready`.

use std::fmt::Write as _;
use std::time::Duration;

use super::{json, lock, Arc, Inner, KernelStartOptions};
use crate::kernel::bootstrap::{ensure_remote_kernel_python, sh_quote, RemoteHost};
use crate::kernel::env_overrides::ResolvedKernelEnv;
use crate::kernel::remote_policy::REMOTE_KERNEL_FORWARDED_ENV;
use crate::kernel::shared::RemoteKernelTarget;

/// The line the remote launcher prints right before `exec`ing the kernel:
/// the stdout reader discards everything before it (login-shell rc output)
/// so only protocol frames reach the parser.
pub(super) const REMOTE_KERNEL_STDIO_SENTINEL: &str = "__PRIME_AGENT_REMOTE_KERNEL_STDIO__";

/// How often the host sends a heartbeat frame to a remote kernel.
pub(super) const HOST_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
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
        let remote = RemoteHost::new(target.host(), &self.options.env)?;
        // Reused across restarts; a start that fails before `ready` drops it
        // (`startup.rs`), so a vanished or broken install is provisioned
        // again instead of relaunched forever.
        let cached = lock(&self.resolved_remote).clone();
        let install = if let Some(install) = cached {
            install
        } else {
            let install = ensure_remote_kernel_python(
                &remote,
                &self.options.python_skills,
                target.skill_dirs(),
                options.on_bootstrap_progress.as_ref(),
            )
            .await?;
            *lock(&self.resolved_remote) = Some(install.clone());
            install
        };
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
            ("RLM_REMOTE_SKILLS".to_string(), install.skill_ids.join(" ")),
        ];
        for key in REMOTE_KERNEL_FORWARDED_ENV {
            if let Some(value) = self.options.env.get(*key) {
                env.push(((*key).to_string(), value.clone()));
            }
        }
        // Run by `/bin/sh` whatever the login shell is; the `exec` makes the
        // remote sshd session the kernel's parent.
        let mut script = String::new();
        if let Some(cwd) = target.cwd() {
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
        // Teardown signals the ssh process; the remote runtime exits on stdin
        // EOF, its sshd parent's exit, or a missed heartbeat.
        command.kill_on_drop(false);
        Ok((command, format!("{} on {}", install.python, remote.label())))
    }
}
