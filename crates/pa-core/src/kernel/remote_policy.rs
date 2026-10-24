//! Remote (SSH) kernel policy: every allowlist that governs what a remote
//! kernel (`rlm.spawn(..., host=...)`) may reach, in one place.
//!
//! The rule is strict by default: anything not listed here is refused
//! with an error that names what was refused and why. Widening a policy
//! is a one-line change to the table plus a test (`tests/remote_kernel.rs`
//! and the unit tests below pin every table).
//!
//! | Policy | Table |
//! | --- | --- |
//! | ssh options forced on every connection | [`SSH_HARDENING_OPTIONS`] |
//! | local env the `ssh` process inherits | [`SSH_ENV_ALLOWLIST`], [`SSH_ENV_ALLOWED_PREFIXES`] |
//! | characters accepted in `host` | [`SSH_HOST_CHARS`] |
//! | characters accepted in a remote `cwd` | [`REMOTE_PATH_CHARS`] |
//! | local kernel env forwarded to the remote kernel | [`REMOTE_KERNEL_FORWARDED_ENV`] |
//! | host requests a remote kernel may send | [`REMOTE_KERNEL_HOST_REQUESTS`] |
//!
//! Settings and state read remotely: none. The remote kernel's agent dir is
//! an empty scratch dir under the install root; the runtime refuses
//! `rlm.harness` (and the refinements switch it reads from settings) and
//! `bash(persistent=True)` on a remote kernel (`rlm/remote.py`).

/// `-o` options passed on every ssh invocation. Command-line options win
/// over `~/.ssh/config`, so a user config that forwards the agent, X11, or
/// ports, runs a `LocalCommand`, or opens a new control master never
/// applies to a kernel connection. `BatchMode` fails a missing key instead
/// of prompting; the keepalives turn a dead connection into an ssh exit.
/// An existing control master (`ControlPath`) is still reused.
pub(crate) const SSH_HARDENING_OPTIONS: &[&str] = &[
    "BatchMode=yes",
    "ForwardAgent=no",
    "ForwardX11=no",
    "ClearAllForwardings=yes",
    "PermitLocalCommand=no",
    "ControlMaster=no",
    "ConnectTimeout=20",
    "ServerAliveInterval=15",
    "ServerAliveCountMax=3",
];

/// Local env vars the `ssh` process keeps; its env is cleared first.
/// `SSH_AUTH_SOCK` lets ssh authenticate with the local agent (never
/// forwarded: see `ForwardAgent=no`).
pub(crate) const SSH_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "SSH_AUTH_SOCK", "LANG"];

/// Env var name prefixes the `ssh` process keeps (locale categories).
pub(crate) const SSH_ENV_ALLOWED_PREFIXES: &[&str] = &["LC_"];

/// Characters accepted in a spawn `host` (`[user@]host[:port]`, a config
/// alias, bracketed IPv6, or `ssh://...`). Anything else (shell
/// metacharacters, `%` tokens, whitespace) is refused: an OpenSSH before
/// 9.6 expands the destination into `ProxyCommand`/`Match exec` shells
/// (CVE-2023-51385).
pub(crate) const SSH_HOST_CHARS: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._@[]:-";

/// Characters accepted in a remote path (`cwd=`, the install root). A
/// leading `~` (alone or as `~/`) is accepted on top of these. Spaces,
/// quotes, and shell metacharacters are refused.
pub(crate) const REMOTE_PATH_CHARS: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._/+-";

/// Local kernel env keys forwarded to the remote kernel. Everything else
/// in the local kernel env (agent and session dirs, the shell, the bash
/// command prefix, credentials such as `SERPER_API_KEY`, the worker's own
/// env) stays local. The remote launcher adds only its own generated keys:
/// `PRIME_AGENT_CODING_AGENT_DIR` (a remote scratch dir),
/// `PRIME_AGENT_KERNEL_OWNER_PID` (the remote sshd session), the remote
/// orphan journal, `RLM_REMOTE_HOST`, `RLM_REMOTE_INSTALL_DIR`, and
/// `RLM_HOST_HEARTBEAT_TIMEOUT_MS`.
pub(crate) const REMOTE_KERNEL_FORWARDED_ENV: &[&str] = &["RLM_SESSION_ID"];

/// Whether a remote kernel may send a host request kind, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteHostRequest {
    /// Handled locally, exactly as for a local kernel.
    Allowed,
    /// Refused with this reason.
    Refused(&'static str),
}

/// The audited host request kinds. A kind missing from this table is
/// refused too (deny by default), so a new handler stays local-only until
/// someone audits it here.
pub(crate) const REMOTE_KERNEL_HOST_REQUESTS: &[(&str, RemoteHostRequest)] = {
    use RemoteHostRequest::{Allowed, Refused};
    &[
        // The basic RLM surface: spawn (children inherit the host),
        // collect, progress, the roster, model lookup, and renames carry
        // no local paths or secrets.
        ("rlm.run", Allowed),
        ("rlm.collect", Allowed),
        ("rlm.progress.note", Allowed),
        ("rlm.list_subagents", Allowed),
        ("rlm.delete_subagent", Allowed),
        ("rlm.find_models", Allowed),
        ("rlm.rename", Allowed),
        // The child's reply channel to its parent (the prompt mandates it).
        ("agent_message.send", Allowed),
        ("agent_message.list_agents", Allowed),
        // `bash()` completion notices wake the local agent loop.
        ("bash.completed", Allowed),
        ("bash.consumed", Allowed),
        // Model facts (`attach_image` checks vision support).
        ("model.info", Allowed),
        // Compaction acts on the local transcript only.
        ("compact.status", Allowed),
        ("compact.run", Allowed),
        (
            "rlm.create_session",
            Refused("it creates a local top-level session from a kernel-supplied cwd"),
        ),
        (
            "mcp.config",
            Refused("MCP server configs carry local credentials (env, headers, OAuth)"),
        ),
        (
            "mcp.refresh",
            Refused("MCP server configs carry local credentials (env, headers, OAuth)"),
        ),
        (
            "mcp.begin_login",
            Refused("MCP logins store credentials on this machine"),
        ),
        (
            "mcp.list_plugins",
            Refused("the MCP inventory describes this machine's servers"),
        ),
        (
            "mcp.search_plugins",
            Refused("the MCP inventory describes this machine's servers"),
        ),
        (
            "mcp.list_connections",
            Refused("the MCP inventory describes this machine's servers"),
        ),
        (
            "refine.status",
            Refused("harness refinement state lives on this machine"),
        ),
        (
            "refine.run",
            Refused("harness refinement state lives on this machine"),
        ),
        (
            "goal.get",
            Refused("goal state is not audited for remote kernels"),
        ),
        (
            "goal.create",
            Refused("goal state is not audited for remote kernels"),
        ),
        (
            "goal.complete",
            Refused("goal state is not audited for remote kernels"),
        ),
        (
            "rlm_heartbeat.list",
            Refused("heartbeat jobs schedule wake-ups on this machine"),
        ),
        (
            "rlm_heartbeat.create",
            Refused("heartbeat jobs schedule wake-ups on this machine"),
        ),
        (
            "rlm_heartbeat.update",
            Refused("heartbeat jobs schedule wake-ups on this machine"),
        ),
        (
            "rlm_heartbeat.delete",
            Refused("heartbeat jobs schedule wake-ups on this machine"),
        ),
        (
            "agent_observe.list",
            Refused("it reads other agents' local transcripts"),
        ),
        (
            "agent_observe.get",
            Refused("it reads other agents' local transcripts"),
        ),
        (
            "agent_observe.recent",
            Refused("it reads other agents' local transcripts"),
        ),
        (
            "factory.progress",
            Refused("factories run on harness state, which lives on this machine"),
        ),
    ]
};

/// The refusal for one host request kind from a remote kernel on `host`,
/// or `None` when the kind is allowed.
pub(crate) fn remote_host_request_refusal(request_type: &str, host: &str) -> Option<String> {
    let reason = match REMOTE_KERNEL_HOST_REQUESTS
        .iter()
        .find(|(kind, _)| *kind == request_type)
    {
        Some((_, RemoteHostRequest::Allowed)) => return None,
        Some((_, RemoteHostRequest::Refused(reason))) => reason,
        None => "it has not been audited for remote kernels",
    };
    Some(format!(
        "host request \"{request_type}\" is not available on remote kernels yet (this kernel runs on {host}): {reason}"
    ))
}

/// The `ssh` process env: only the allowlisted keys of `source`.
pub(crate) fn ssh_process_env(
    source: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = source
        .into_iter()
        .filter(|(key, _)| {
            SSH_ENV_ALLOWLIST.contains(&key.as_str())
                || SSH_ENV_ALLOWED_PREFIXES
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
        })
        .collect();
    env.sort();
    env
}

/// Validate a remote path (`cwd=`, the install root): absolute, `~`, or
/// `~/`-relative, over [`REMOTE_PATH_CHARS`].
pub(crate) fn validate_remote_path(path: &str, what: &str) -> anyhow::Result<()> {
    let rest = if path == "~" {
        ""
    } else if let Some(rest) = path.strip_prefix("~/") {
        rest
    } else if path.starts_with('/') {
        path
    } else {
        anyhow::bail!("{what} must be an absolute or ~-relative remote path, got {path:?}");
    };
    if path.len() > 4096 {
        anyhow::bail!("{what} is longer than 4096 bytes");
    }
    if let Some(bad) = rest.chars().find(|c| !REMOTE_PATH_CHARS.contains(*c)) {
        anyhow::bail!(
            "{what} {path:?} contains {bad:?}; remote paths may only use letters, digits, and . _ / + - (with an optional leading ~)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlisted_and_refused_host_requests_name_the_kind_and_reason() {
        assert_eq!(remote_host_request_refusal("rlm.run", "box"), None);
        assert_eq!(
            remote_host_request_refusal("mcp.config", "box").as_deref(),
            Some("host request \"mcp.config\" is not available on remote kernels yet (this kernel runs on box): MCP server configs carry local credentials (env, headers, OAuth)")
        );
        assert_eq!(
            remote_host_request_refusal("future.kind", "box").as_deref(),
            Some("host request \"future.kind\" is not available on remote kernels yet (this kernel runs on box): it has not been audited for remote kernels")
        );
    }

    #[test]
    fn the_ssh_env_keeps_only_the_allowlist() {
        let env = ssh_process_env(
            [
                ("PATH", "/bin"),
                ("HOME", "/h"),
                ("USER", "u"),
                ("SSH_AUTH_SOCK", "/s"),
                ("LANG", "C.UTF-8"),
                ("LC_ALL", "C"),
                ("OPENAI_API_KEY", "secret"),
                ("PRIME_AGENT_BASH_COMMAND_PREFIX", "x"),
                ("PYTHONPATH", "/p"),
                ("SSH_ASKPASS", "/a"),
            ]
            .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        assert_eq!(
            env,
            [
                ("HOME", "/h"),
                ("LANG", "C.UTF-8"),
                ("LC_ALL", "C"),
                ("PATH", "/bin"),
                ("SSH_AUTH_SOCK", "/s"),
                ("USER", "u"),
            ]
            .map(|(key, value)| (key.to_string(), value.to_string()))
        );
    }

    #[test]
    fn remote_paths_accept_only_the_conservative_charset() {
        for good in ["/", "/srv/work", "~", "~/work/a-b_c.d+e", "/home/u/x/../y"] {
            assert!(validate_remote_path(good, "cwd").is_ok(), "{good:?}");
        }
        for bad in [
            "",
            "work",
            "./work",
            "~user/work",
            "/a b",
            "/a'b",
            "/a\"b",
            "/a;b",
            "/a$(touch x)",
            "/a`x`",
            "/a\nb",
            "/a*",
            "~/a|b",
            "/a\\b",
        ] {
            assert!(validate_remote_path(bad, "cwd").is_err(), "{bad:?}");
        }
    }
}
