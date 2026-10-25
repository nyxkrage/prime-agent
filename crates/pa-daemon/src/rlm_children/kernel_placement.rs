//! The kernel placement sidecar (`kernel-placement.json` in a child's
//! session dir): an `rlm.spawn(..., host=..., env=...)` child records where
//! its kernel runs and its `env=` overrides, so every later open of that
//! session (a respawned worker, a resume or switch, a fork or new session in
//! the same dir, the parent's ledger reseed, a print-mode reopen) runs the
//! kernel on the same host with the same env. A session whose sidecar
//! cannot be read refuses to open rather than silently running its kernel
//! locally or without its env.
//!
//! The env is stored as the spawn validated it: literal values and
//! pass-through names (`null`), never a pass-through's value. Each kernel
//! start resolves the pass-throughs again, and the session build re-checks
//! the global `remoteEnvPassthrough` kill switch for a remote placement.

use std::path::Path;

use anyhow::{Context as _, Result};
use pa_core::kernel::{KernelEnvOverrides, RemoteKernelTarget};
use serde_json::{json, Value};

/// The sidecar file name inside a child's session dir.
pub(crate) const KERNEL_PLACEMENT_FILE: &str = "kernel-placement.json";

/// A session's kernel placement: the remote host (`None` runs locally) and
/// the spawn's `env=` overrides (empty when none were given).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KernelPlacement {
    /// Where the kernel runs; `None` runs it locally.
    pub remote: Option<RemoteKernelTarget>,
    /// The spawn's `env=` overrides (literals and pass-through names).
    pub env: KernelEnvOverrides,
}

/// Record `placement` for the session(s) in `session_dir`. A local
/// placement without env writes nothing.
pub(crate) fn write_kernel_placement(
    session_dir: &Path,
    placement: &KernelPlacement,
) -> Result<()> {
    if placement.remote.is_none() && placement.env.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(session_dir)
        .with_context(|| format!("create session dir {}", session_dir.display()))?;
    let mut record = json!({ "type": "kernel_placement" });
    if let Some(remote) = &placement.remote {
        record["host"] = json!(remote.host());
        if let Some(cwd) = remote.cwd() {
            record["cwd"] = json!(cwd);
        }
    }
    if !placement.env.is_empty() {
        record["env"] = placement.env.to_json();
    }
    let path = session_dir.join(KERNEL_PLACEMENT_FILE);
    let temp = session_dir.join(format!(
        "{KERNEL_PLACEMENT_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temp, format!("{record}\n"))
        .with_context(|| format!("write {}", temp.display()))?;
    pa_core::platform::rename_onto(&temp, &path)
        .with_context(|| format!("persist the kernel placement at {}", path.display()))
}

/// The placement recorded in `session_dir`: the default (local, no env)
/// without a sidecar; an error for a sidecar that exists but is unreadable,
/// names a host or cwd outside the remote policy, or carries an invalid env.
pub(crate) fn read_kernel_placement(session_dir: &Path) -> Result<KernelPlacement> {
    let path = session_dir.join(KERNEL_PLACEMENT_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(KernelPlacement::default())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read the kernel placement {}", path.display()))
        }
    };
    let record: Value = serde_json::from_str(text.trim())
        .with_context(|| format!("parse the kernel placement {}", path.display()))?;
    let remote = record
        .get("host")
        .map(|host| {
            let host = host.as_str().context("its host is not a string")?;
            let cwd = record
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
            RemoteKernelTarget::validated(host.to_string(), cwd)
        })
        .transpose()
        .with_context(|| format!("the kernel placement {}", path.display()))?;
    let env = KernelEnvOverrides::from_json(record.get("env").unwrap_or(&Value::Null))
        .with_context(|| format!("the kernel placement {}", path.display()))?;
    Ok(KernelPlacement { remote, env })
}

/// The placement of the session stored at `session_file` (its dir's
/// sidecar), with an error that says the session will not open without it.
///
/// # Errors
///
/// Returns an error when the sidecar exists but cannot be read or names a
/// host, cwd, or env outside the policy.
pub fn placement_for_session_file(session_file: &Path) -> Result<KernelPlacement> {
    let Some(dir) = session_file.parent() else {
        return Ok(KernelPlacement::default());
    };
    read_kernel_placement(dir).context(
        "this session's kernel placement (remote host, spawn env) is unreadable; it will not reopen without it",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placement_round_trips_and_a_damaged_one_refuses() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            read_kernel_placement(dir.path()).unwrap(),
            KernelPlacement::default()
        );
        // A local placement without env leaves no sidecar.
        write_kernel_placement(dir.path(), &KernelPlacement::default()).unwrap();
        assert!(!dir.path().join(KERNEL_PLACEMENT_FILE).exists());
        let placement = KernelPlacement {
            remote: Some(
                RemoteKernelTarget::validated(
                    "user@box:2222".to_string(),
                    Some("~/work".to_string()),
                )
                .unwrap(),
            ),
            env: KernelEnvOverrides::from_json(&json!({ "EPOCHS": "3", "HF_TOKEN": null }))
                .unwrap(),
        };
        write_kernel_placement(dir.path(), &placement).unwrap();
        assert_eq!(read_kernel_placement(dir.path()).unwrap(), placement);
        // A pass-through is stored by name only.
        let stored = std::fs::read_to_string(dir.path().join(KERNEL_PLACEMENT_FILE)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap(),
            json!({
                "type": "kernel_placement",
                "host": "user@box:2222",
                "cwd": "~/work",
                "env": { "EPOCHS": "3", "HF_TOKEN": null },
            })
        );
        // A local child with env keeps its env too.
        let local = KernelPlacement {
            remote: None,
            env: KernelEnvOverrides::from_json(&json!({ "EPOCHS": "3" })).unwrap(),
        };
        write_kernel_placement(dir.path(), &local).unwrap();
        assert_eq!(read_kernel_placement(dir.path()).unwrap(), local);

        std::fs::write(dir.path().join(KERNEL_PLACEMENT_FILE), "{not json").unwrap();
        let error = placement_for_session_file(&dir.path().join("s.jsonl")).unwrap_err();
        assert!(
            format!("{error:#}").starts_with(
                "this session's kernel placement (remote host, spawn env) is unreadable; it will not reopen without it: parse the kernel placement"
            ),
            "{error:#}"
        );
        for damaged in [
            r#"{"type":"kernel_placement","host":"box$(id)"}"#,
            r#"{"type":"kernel_placement","host":"box","env":{"PATH":"/evil"}}"#,
        ] {
            std::fs::write(dir.path().join(KERNEL_PLACEMENT_FILE), damaged).unwrap();
            assert!(read_kernel_placement(dir.path()).is_err(), "{damaged}");
        }
    }
}
