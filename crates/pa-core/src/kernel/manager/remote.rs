//! Remote kernel launch: the SSH transport for the existing process + stdio
//! protocol. The kernel process is `ssh <host> <remote-venv>/bin/python -m
//! rlm.repl`, so requests, events, and host requests ride the SSH channel
//! unchanged, and the local `ssh` process stands in for the kernel pid.

use std::fmt::Write as _;

use super::{lock, Arc, Inner, KernelStartOptions};
use crate::kernel::bootstrap::{
    ensure_remote_kernel_python, sh_quote, RemoteKernelInstall, SshDestination,
};
use crate::kernel::shared::RemoteKernelTarget;

/// Kernel env keys forwarded verbatim to the remote kernel. Everything
/// else in the local kernel env is a local path (agent dir, session dir,
/// shell) or a credential that must not ride the remote command line.
const FORWARDED_REMOTE_ENV: [&str; 2] = ["RLM_SESSION_ID", "PRIME_AGENT_BASH_COMMAND_PREFIX"];

impl Inner {
    /// Provision the remote host on first use, then build the `ssh` command
    /// that runs the kernel there. Returns the command and a launch label
    /// for error messages.
    pub(super) async fn remote_kernel_command(
        self: &Arc<Self>,
        target: &RemoteKernelTarget,
        options: &KernelStartOptions,
    ) -> anyhow::Result<(tokio::process::Command, String)> {
        let destination = SshDestination::parse(&target.host)?;
        let cached = lock(&self.resolved_remote).clone();
        let install: RemoteKernelInstall = if let Some(install) = cached {
            install
        } else {
            let install = ensure_remote_kernel_python(
                &destination,
                &self.options.python_skills,
                options.on_bootstrap_progress.as_ref(),
            )
            .await?;
            *lock(&self.resolved_remote) = Some(install.clone());
            install
        };
        let mut env: Vec<(String, String)> = vec![(
            "PRIME_AGENT_CODING_AGENT_DIR".to_string(),
            format!("{}/agent", install.root),
        )];
        for key in FORWARDED_REMOTE_ENV {
            if let Some(value) = self.options.env.get(key) {
                env.push((key.to_string(), value.clone()));
            }
        }
        // Kernel-side session state (local harness entries) lives on the
        // remote, keyed by the durable session id like the local artifact dir.
        if let Some(session_id) = self.options.env.get("RLM_SESSION_ID") {
            if !session_id.is_empty() {
                env.push((
                    "RLM_SESSION_DIR".to_string(),
                    format!("{}/sessions/{session_id}", install.root),
                ));
            }
        }
        let mut script = String::new();
        if let Some(cwd) = &target.cwd {
            let cwd = match cwd.strip_prefix("~/") {
                Some(rest) => format!("~/{}", sh_quote(rest)),
                None if cwd == "~" => "~".to_string(),
                None => sh_quote(cwd),
            };
            let _ = write!(script, "cd {cwd} && ");
        }
        script.push_str("exec env");
        for (key, value) in &env {
            script.push(' ');
            script.push_str(&sh_quote(&format!("{key}={value}")));
        }
        let _ = write!(script, " {} -m rlm.repl", sh_quote(&install.python));
        let mut command = destination.command(&script);
        // The kernel's teardown signals the ssh process; the remote runtime
        // sees stdin EOF (or its sshd parent exit) and shuts down.
        command.kill_on_drop(false);
        command
            .env_clear()
            .envs(super::startup::kernel_spawn_env(&self.options.env));
        Ok((
            command,
            format!("{} on {}", install.python, destination.label()),
        ))
    }
}
