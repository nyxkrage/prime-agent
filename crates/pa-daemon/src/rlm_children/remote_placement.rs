//! The remote kernel placement sidecar (`remote-kernel.json` in a child's
//! session dir): an `rlm.spawn(..., host=...)` child records where its
//! kernel runs, so every later open of that session (a respawned worker, a
//! resume or switch, a fork in the same dir, the parent's ledger reseed)
//! runs the kernel on the same host. A session whose sidecar cannot be
//! read refuses to open rather than silently running its kernel locally.

use std::path::Path;

use anyhow::{bail, Context as _, Result};
use pa_core::kernel::RemoteKernelTarget;
use serde_json::{json, Value};

/// The sidecar file name inside a child's session dir.
pub(crate) const REMOTE_PLACEMENT_FILE: &str = "remote-kernel.json";

/// Record `target` as the placement of the session(s) in `session_dir`.
pub(crate) fn write_remote_placement(
    session_dir: &Path,
    target: &RemoteKernelTarget,
) -> Result<()> {
    std::fs::create_dir_all(session_dir)
        .with_context(|| format!("create session dir {}", session_dir.display()))?;
    let mut record = json!({ "type": "remote_kernel", "host": target.host });
    if let Some(cwd) = &target.cwd {
        record["cwd"] = json!(cwd);
    }
    let path = session_dir.join(REMOTE_PLACEMENT_FILE);
    let temp = session_dir.join(format!(
        "{REMOTE_PLACEMENT_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temp, format!("{record}\n"))
        .with_context(|| format!("write {}", temp.display()))?;
    pa_core::platform::rename_onto(&temp, &path)
        .with_context(|| format!("persist the remote kernel placement at {}", path.display()))
}

/// The placement recorded in `session_dir`: `None` for a local session (no
/// sidecar); an error for a sidecar that exists but is unreadable or names
/// a host or cwd outside the remote policy.
pub(crate) fn read_remote_placement(session_dir: &Path) -> Result<Option<RemoteKernelTarget>> {
    let path = session_dir.join(REMOTE_PLACEMENT_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read the remote kernel placement {}", path.display()))
        }
    };
    let record: Value = serde_json::from_str(text.trim())
        .with_context(|| format!("parse the remote kernel placement {}", path.display()))?;
    let Some(host) = record.get("host").and_then(Value::as_str) else {
        bail!(
            "the remote kernel placement {} names no host",
            path.display()
        );
    };
    let cwd = record
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    RemoteKernelTarget::validated(host.to_string(), cwd)
        .map(Some)
        .with_context(|| format!("the remote kernel placement {}", path.display()))
}

/// The placement of the session stored at `session_file` (its dir's
/// sidecar), with an error that says the session will not open locally.
pub(crate) fn placement_for_session_file(
    session_file: &Path,
) -> Result<Option<RemoteKernelTarget>> {
    let Some(dir) = session_file.parent() else {
        return Ok(None);
    };
    read_remote_placement(dir).context(
        "this session's kernel ran on a remote host; it will not reopen with a local kernel",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placement_round_trips_and_a_damaged_one_refuses() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(read_remote_placement(dir.path()).unwrap(), None);
        let target = RemoteKernelTarget {
            host: "user@box:2222".to_string(),
            cwd: Some("~/work".to_string()),
        };
        write_remote_placement(dir.path(), &target).unwrap();
        assert_eq!(read_remote_placement(dir.path()).unwrap(), Some(target));

        std::fs::write(dir.path().join(REMOTE_PLACEMENT_FILE), "{not json").unwrap();
        let error = placement_for_session_file(&dir.path().join("s.jsonl")).unwrap_err();
        assert!(
            format!("{error:#}").starts_with(
                "this session's kernel ran on a remote host; it will not reopen with a local kernel: parse the remote kernel placement"
            ),
            "{error:#}"
        );
        std::fs::write(
            dir.path().join(REMOTE_PLACEMENT_FILE),
            r#"{"type":"remote_kernel","host":"box$(id)"}"#,
        )
        .unwrap();
        assert!(read_remote_placement(dir.path()).is_err());
    }
}
