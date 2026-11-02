//! A live kernel follows the hidden Python-skill set without a restart (the
//! `apply_patch` binding across model switches). The kernel Python is ambient
//! (`PA_CORE_KERNEL_PYTHON`, else the default kernel venv); skipped if absent.

use std::path::PathBuf;

use pa_core::kernel::bootstrap::KernelPythonSkill;
use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::ExecuteOptions;
use pa_core::kernel::HiddenPythonSkills;

fn kernel_python() -> Option<PathBuf> {
    let python = std::env::var_os("PA_CORE_KERNEL_PYTHON")
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(".prime/agent/kernel-venv/bin/python"))
        })
        .filter(|python| python.exists());
    if python.is_none() {
        eprintln!("kernel python not found; skipping");
    }
    python
}

/// Hidden at boot, bound once the set clears, parked again, and a name the
/// user rebound while it was hidden is never overwritten.
#[tokio::test]
async fn the_live_kernel_follows_the_hidden_set() {
    let Some(python) = kernel_python() else {
        return;
    };
    let hidden = HiddenPythonSkills::default();
    hidden.set(vec!["json".to_string()]);
    let provisioner = IpythonKernelProvisioner::new(
        std::env::temp_dir(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            // A stdlib module stands in for a skill package.
            python_skills: vec![KernelPythonSkill {
                name: "json".to_string(),
                import_name: "json".to_string(),
                package_path: PathBuf::from("/nonexistent"),
                pyproject_path: PathBuf::from("/nonexistent/pyproject.toml"),
            }],
            hidden_skills: hidden.clone(),
            ..Default::default()
        },
    );
    let manager = provisioner.ensure(None, None).await.unwrap();
    // The `python_repl` executor syncs the set before every cell.
    let run = |code: &'static str| {
        let hidden = hidden.clone();
        let manager = manager.clone();
        async move {
            hidden.sync(&manager, None).await.unwrap();
            manager
                .execute(code, ExecuteOptions::default())
                .await
                .unwrap()
        }
    };
    let mut seen = Vec::new();
    for (names, user_code) in [
        (None, None),
        (Some(vec![]), None),
        (Some(vec!["json".to_string()]), Some("json = 'mine'")),
        (Some(vec![]), None),
    ] {
        if let Some(names) = names {
            hidden.set(names);
        }
        if let Some(code) = user_code {
            run(code).await;
        }
        let probe = run("type(globals().get('json')).__name__").await;
        seen.push(probe.result.unwrap_or_default());
    }
    assert_eq!(seen, ["'NoneType'", "'module'", "'str'", "'str'"]);
    provisioner.dispose(None).await;
}
