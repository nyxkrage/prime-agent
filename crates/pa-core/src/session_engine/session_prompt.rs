//! The session's system prompt across model switches: the parts that depend
//! on the model (edit format, `model_prompts.toml` additions, vision) are
//! re-resolved on every switch, and the prompt is rebuilt only when one of
//! them changed, so ordinary switches keep the cached prefix.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use pa_types::ai::{EditFormat, ModelInput};

use super::turn_boundary::ModelInfo;
use crate::settings::ApplyPatchSetting;

/// The kernel import the `apply-patch` skill binds.
const APPLY_PATCH_IMPORT: &str = "apply_patch";

/// What the prompt reads from the session's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPromptFacts {
    pub(crate) edit_format: EditFormat,
    pub(crate) extras: Option<String>,
    pub(crate) vision_capable: bool,
}

/// Builds the whole prompt for one set of model facts; every other input is
/// fixed at session creation.
pub(crate) type PromptBuilder = Box<dyn Fn(&ModelPromptFacts) -> String + Send + Sync>;

pub(crate) struct SessionPrompt {
    build: PromptBuilder,
    apply_patch: ApplyPatchSetting,
    agent_dir: PathBuf,
    current: Mutex<(ModelPromptFacts, String)>,
    /// Serializes switches, so the agent ends on the prompt of the last one.
    switching: tokio::sync::Mutex<()>,
}

impl SessionPrompt {
    pub(crate) fn new(
        build: PromptBuilder,
        apply_patch: ApplyPatchSetting,
        agent_dir: PathBuf,
        facts: ModelPromptFacts,
    ) -> Self {
        let text = build(&facts);
        Self {
            build,
            apply_patch,
            agent_dir,
            current: Mutex::new((facts, text)),
            switching: tokio::sync::Mutex::new(()),
        }
    }

    fn current(&self) -> std::sync::MutexGuard<'_, (ModelPromptFacts, String)> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn text(&self) -> String {
        self.current().1.clone()
    }

    /// Follow a model switch: rebuild the prompt into `agent` only when the
    /// model facts changed. Returns the new edit format. The
    /// `model_prompts.toml` errors do not depend on the model and were
    /// reported when the session started.
    pub(crate) async fn switch(
        &self,
        agent: &pa_agent::agent::Agent,
        model: &ModelInfo,
        configured: Option<EditFormat>,
    ) -> EditFormat {
        let _switching = self.switching.lock().await;
        let (facts, _errors) = resolve(self.apply_patch, &self.agent_dir, model, configured);
        let edit_format = facts.edit_format;
        if self.current().0 != facts {
            let text = (self.build)(&facts);
            agent.set_system_prompt(text.clone()).await;
            *self.current() = (facts, text);
        }
        edit_format
    }
}

/// The facts for `model`, with the `model_prompts.toml` errors to report.
pub(crate) fn resolve(
    apply_patch: ApplyPatchSetting,
    agent_dir: &Path,
    model: &ModelInfo,
    configured: Option<EditFormat>,
) -> (ModelPromptFacts, Vec<String>) {
    let selector = format!("{}/{}", model.provider, model.id);
    let additions = crate::prompts::model_prompts::load_model_prompts(Some(&selector), agent_dir);
    let facts = ModelPromptFacts {
        edit_format: crate::models::resolve_edit_format(apply_patch, &model.id, configured),
        extras: additions.extras,
        vision_capable: model.input.contains(&ModelInput::Image),
    };
    (facts, additions.errors)
}

/// The kernel imports an edit format keeps out of the user namespace.
pub(crate) fn hidden_skills_for(edit_format: EditFormat) -> Vec<String> {
    match edit_format {
        EditFormat::Edit => vec![APPLY_PATCH_IMPORT.to_string()],
        EditFormat::ApplyPatch => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    fn model(provider: &str, id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            provider: provider.to_string(),
            input: vec![ModelInput::Text],
        }
    }

    /// A switch between models with the same facts keeps the prompt (and so
    /// the cached prefix) without rebuilding it; a switch that changes them
    /// rebuilds it into the agent.
    #[tokio::test]
    async fn a_switch_rebuilds_only_when_the_model_facts_change() {
        let agent_dir = tempfile::tempdir().unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&builds);
        let build: PromptBuilder = Box::new(move |facts| {
            counter.fetch_add(1, Ordering::SeqCst);
            format!("{:?}", facts.edit_format)
        });
        let first = model("openai", "gpt-5.1");
        let (facts, _errors) = resolve(
            ApplyPatchSetting::Auto,
            agent_dir.path(),
            &first,
            /*configured*/ None,
        );
        let prompt = SessionPrompt::new(
            build,
            ApplyPatchSetting::Auto,
            agent_dir.path().to_path_buf(),
            facts,
        );
        let agent = pa_agent::agent::Agent::new(pa_agent::agent::AgentOptions::default());
        let mut seen = Vec::new();
        for (provider, id) in [
            ("openai", "gpt-5.2"),
            ("anthropic", "claude-sonnet-4-5"),
            ("deepseek", "deepseek-v4-flash"),
            ("openai", "gpt-5.1"),
        ] {
            let edit_format = prompt
                .switch(&agent, &model(provider, id), /*configured*/ None)
                .await;
            seen.push((
                edit_format,
                prompt.text(),
                agent.state().await.system_prompt,
                builds.load(Ordering::SeqCst),
            ));
        }
        let apply_patch = || "ApplyPatch".to_string();
        let edit = || "Edit".to_string();
        assert_eq!(
            seen,
            vec![
                (EditFormat::ApplyPatch, apply_patch(), String::new(), 1),
                (EditFormat::Edit, edit(), edit(), 2),
                (EditFormat::Edit, edit(), edit(), 2),
                (EditFormat::ApplyPatch, apply_patch(), apply_patch(), 3),
            ]
        );
    }
}
