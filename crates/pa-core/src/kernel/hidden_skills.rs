//! Python skills the kernel imports but keeps out of the user namespace.

use std::sync::{Arc, Mutex, PoisonError};

use crate::kernel::manager::ReplKernelManager;
use crate::kernel::shared::{ExecuteOptions, ExecuteStatus};

/// Import names of bootstrapped Python skills that stay out of the kernel's
/// user namespace (the session's edit format hides `apply_patch` from `edit`
/// models). The session engine changes the set on a model switch without
/// restarting the kernel: every bootstrap applies the set current at boot, and
/// the next user cell first re-applies it when it changed since, or when the
/// kernel process was replaced (a protocol repair re-runs the boot-time code).
/// A hidden skill stays importable; its global is parked, not deleted.
#[derive(Clone, Default)]
pub struct HiddenPythonSkills(Arc<Mutex<HiddenState>>);

#[derive(Default)]
struct HiddenState {
    names: Vec<String>,
    generation: u64,
    /// Kernel process and generation the namespace last matched.
    applied: Option<(Option<i32>, u64)>,
}

impl HiddenPythonSkills {
    fn state(&self) -> std::sync::MutexGuard<'_, HiddenState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the hidden set; the live kernel catches up before its next user cell.
    pub fn set(&self, names: Vec<String>) {
        let mut state = self.state();
        if state.names != names {
            state.names = names;
            state.generation += 1;
        }
    }

    /// The current hidden set.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.state().names.clone()
    }

    /// Code that parks every hidden skill's global and restores every parked
    /// global no longer hidden (never over a name the user rebound since),
    /// with the generation it applies.
    pub(crate) fn binding_code(&self) -> (String, u64) {
        let state = self.state();
        let hidden = serde_json::to_string(&state.names).unwrap_or_else(|_| "[]".to_string());
        let code = format!(
            r#"
_prime_agent_parked_skills = globals().setdefault("_prime_agent_parked_skills", {{}})
for _prime_agent_name in {hidden}:
    if _prime_agent_name in globals():
        _prime_agent_parked_skills[_prime_agent_name] = globals().pop(_prime_agent_name)
for _prime_agent_name in [_n for _n in _prime_agent_parked_skills if _n not in {hidden}]:
    globals().setdefault(_prime_agent_name, _prime_agent_parked_skills.pop(_prime_agent_name))
"#
        );
        (code, state.generation)
    }

    pub(crate) fn mark_applied(&self, process_id: Option<i32>, generation: u64) {
        self.state().applied = Some((process_id, generation));
    }

    /// Bring the kernel namespace up to date; runs before every user cell.
    ///
    /// # Errors
    ///
    /// Returns an error when the binding cell fails to run.
    pub async fn sync(
        &self,
        manager: &ReplKernelManager,
        signal: Option<crate::kernel::cancellation::AbortSignal>,
    ) -> anyhow::Result<()> {
        let process_id = manager.process_id();
        let (code, generation) = self.binding_code();
        if self.state().applied == Some((process_id, generation)) {
            return Ok(());
        }
        let result = manager
            .execute(
                &code,
                ExecuteOptions {
                    internal: true,
                    signal,
                    ..ExecuteOptions::default()
                },
            )
            .await?;
        if result.status != ExecuteStatus::Ok {
            anyhow::bail!(
                "rebinding the Python skills failed: {}",
                crate::kernel::manager::describe_failure(&result)
            );
        }
        self.mark_applied(process_id, generation);
        Ok(())
    }
}
