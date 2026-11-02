//! The kernel placement sidecar (`kernel-placement.json` in a child's
//! session dir): a spawned child's remote host and `env=` overrides, so
//! every later open of that session (respawn, resume, switch, fork, new
//! session, ledger reseed, print-mode reopen) keeps both. An unreadable
//! sidecar refuses the open rather than silently running locally.
//!
//! Pass-throughs are stored by name (`null`), never by value.

use std::path::Path;

use anyhow::{Context as _, Result};
use pa_core::kernel::{KernelEnvOverrides, RemoteKernelTarget};
use serde_json::{json, Value};

/// The sidecar file name inside a child's session dir.
pub(crate) const KERNEL_PLACEMENT_FILE: &str = "kernel-placement.json";

/// A session's kernel placement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KernelPlacement {
    /// `None` runs the kernel locally.
    pub remote: Option<RemoteKernelTarget>,
    pub env: KernelEnvOverrides,
}

impl KernelPlacement {
    /// The JSON record (the sidecar line and the durable create's
    /// `kernelPlacement`); `None` for a local placement without env.
    pub(crate) fn to_record(&self) -> Option<Value> {
        if self.remote.is_none() && self.env.is_empty() {
            return None;
        }
        let mut record = json!({ "type": "kernel_placement" });
        if let Some(remote) = &self.remote {
            record["host"] = json!(remote.host());
            if let Some(cwd) = remote.cwd() {
                record["cwd"] = json!(cwd);
            }
        }
        if !self.env.is_empty() {
            record["env"] = self.env.to_json();
        }
        Some(record)
    }

    /// Parse [`Self::to_record`]'s form, validating host, cwd, and env again.
    pub(crate) fn from_record(record: &Value) -> Result<Self> {
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
            .transpose()?;
        let env = KernelEnvOverrides::from_json(record.get("env").unwrap_or(&Value::Null))?;
        Ok(Self { remote, env })
    }
}

/// Record `placement` for the session(s) in `session_dir`. A local
/// placement without env writes nothing.
pub(crate) fn write_kernel_placement(
    session_dir: &Path,
    placement: &KernelPlacement,
) -> Result<()> {
    let Some(record) = placement.to_record() else {
        return Ok(());
    };
    std::fs::create_dir_all(session_dir)
        .with_context(|| format!("create session dir {}", session_dir.display()))?;
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

/// The placement recorded in `session_dir` (the default without a sidecar);
/// an error for a sidecar that is unreadable or outside the policy.
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
    KernelPlacement::from_record(&record)
        .with_context(|| format!("the kernel placement {}", path.display()))
}

/// The placement of the session stored at `session_file`.
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
