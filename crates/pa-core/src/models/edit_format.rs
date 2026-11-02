//! The effective file-edit format for a model: the one decision the prompt
//! and the kernel's `apply_patch` binding both follow.

use std::path::Path;

use pa_types::ai::{EditFormat, Model};

use crate::settings::{ApplyPatchSetting, SettingsManager};

/// The first GPT version trained on `apply_patch` as an official tool.
const FIRST_APPLY_PATCH_GPT: (u64, u64) = (5, 1);

/// Resolve the edit format: `applyPatch: "off"` wins, then the model's
/// configured `editFormat` (`models.json`), then the ID default (GPT-5.1 and
/// newer get `apply_patch`, everything else `edit`).
///
/// The default reads the model name through provider spellings (`openai/`
/// prefixes, `:nitro` variants, `ft:` fine-tunes, Bedrock-style
/// `us.openai.` prefixes): a re-hosted or fine-tuned model was trained the same.
#[must_use]
pub(crate) fn resolve_edit_format(
    setting: ApplyPatchSetting,
    model_id: &str,
    configured: Option<EditFormat>,
) -> EditFormat {
    match setting {
        ApplyPatchSetting::Off => EditFormat::Edit,
        ApplyPatchSetting::Auto => configured.unwrap_or_else(|| {
            let name = model_id.rsplit('/').next().unwrap_or(model_id);
            let name = name.to_ascii_lowercase();
            let name = name.strip_prefix("ft:").unwrap_or(&name);
            let name = name.split(':').next().unwrap_or_default();
            // `gpt-` at the start or right after a dotted vendor prefix.
            let gpt = name
                .match_indices("gpt-")
                .find(|(index, _)| *index == 0 || name.as_bytes()[index - 1] == b'.')
                .map(|(index, _)| &name[index + "gpt-".len()..]);
            // `<major>[.<minor>]` before the first `-`; a missing minor reads
            // as 0. Digits only: `parse` alone would accept a leading `+`.
            let version = gpt.and_then(|rest| {
                let version = rest.split('-').next().unwrap_or_default();
                let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
                let number = |part: &str| {
                    part.parse::<u64>()
                        .ok()
                        .filter(|_| part.bytes().all(|byte| byte.is_ascii_digit()))
                };
                Some((number(major)?, number(minor)?))
            });
            if version.is_some_and(|version| version >= FIRST_APPLY_PATCH_GPT) {
                EditFormat::ApplyPatch
            } else {
                EditFormat::Edit
            }
        }),
    }
}

/// The edit format a new session on `model` (a CLI model reference; `None`:
/// the startup model) would get, for `prime-agent prompt`. A reference the
/// catalog does not know is judged by its ID alone.
#[must_use]
pub fn edit_format_for_model_reference(
    settings: &SettingsManager,
    agent_dir: &Path,
    model: Option<&str>,
) -> EditFormat {
    let registry = super::ModelRegistry::create(
        crate::auth::AuthStorage::create(agent_dir),
        agent_dir.join("models.json"),
    );
    let all = registry.get_all();
    let resolved: Option<Model> = if let Some(reference) = model {
        super::resolve_cli_model(None, reference, all).model
    } else {
        let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
        super::find_initial_model(&super::InitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &[],
            is_continuing: false,
            default_provider: settings.get_default_provider(),
            default_model_id: settings.get_default_model(),
            all_models: all,
            available_models: &available,
        })
    };
    match (resolved, model) {
        (Some(resolved), _) => resolve_edit_format(
            settings.get_apply_patch(),
            &resolved.id,
            resolved.edit_format,
        ),
        (None, Some(reference)) => resolve_edit_format(
            settings.get_apply_patch(),
            reference.split_once('/').map_or(reference, |(_, id)| id),
            /*configured*/ None,
        ),
        (None, None) => EditFormat::Edit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_default_starts_at_gpt_5_1() {
        let ids = [
            "gpt-5.1",
            "gpt-5.1-codex-mini",
            "gpt-6-luna",
            "openai/gpt-5.6-luna",
            "gpt-5.10-codex",
            "openai/gpt-5.1:nitro",
            "ft:gpt-5.1:acme::abc123",
            "us.openai.gpt-5.2",
            "ft:gpt-4.1-mini:acme::abc123",
            "openai.gpt-oss-120b-1:0",
            "openai/gpt-5:free",
            "chatgpt-4o-latest",
            "my-gpt-5.1",
            "gpt-5-codex",
            "gpt-4.1",
            "gpt-oss-120b",
            "claude-sonnet-4-5",
        ];
        let resolved: Vec<(&str, EditFormat)> = ids
            .iter()
            .map(|id| {
                (
                    *id,
                    resolve_edit_format(ApplyPatchSetting::Auto, id, /*configured*/ None),
                )
            })
            .collect();
        let expected: Vec<(&str, EditFormat)> = vec![
            ("gpt-5.1", EditFormat::ApplyPatch),
            ("gpt-5.1-codex-mini", EditFormat::ApplyPatch),
            ("gpt-6-luna", EditFormat::ApplyPatch),
            ("openai/gpt-5.6-luna", EditFormat::ApplyPatch),
            ("gpt-5.10-codex", EditFormat::ApplyPatch),
            ("openai/gpt-5.1:nitro", EditFormat::ApplyPatch),
            ("ft:gpt-5.1:acme::abc123", EditFormat::ApplyPatch),
            ("us.openai.gpt-5.2", EditFormat::ApplyPatch),
            ("ft:gpt-4.1-mini:acme::abc123", EditFormat::Edit),
            ("openai.gpt-oss-120b-1:0", EditFormat::Edit),
            ("openai/gpt-5:free", EditFormat::Edit),
            ("chatgpt-4o-latest", EditFormat::Edit),
            ("my-gpt-5.1", EditFormat::Edit),
            ("gpt-5-codex", EditFormat::Edit),
            ("gpt-4.1", EditFormat::Edit),
            ("gpt-oss-120b", EditFormat::Edit),
            ("claude-sonnet-4-5", EditFormat::Edit),
        ];
        assert_eq!(resolved, expected);
    }

    #[test]
    fn the_setting_beats_the_configured_format_beats_the_default() {
        let cases = [
            (ApplyPatchSetting::Auto, "gpt-5.1", Some(EditFormat::Edit)),
            (
                ApplyPatchSetting::Auto,
                "gpt-5",
                Some(EditFormat::ApplyPatch),
            ),
            (
                ApplyPatchSetting::Auto,
                "my-model",
                Some(EditFormat::ApplyPatch),
            ),
            (ApplyPatchSetting::Off, "gpt-5.1", /*configured*/ None),
            (
                ApplyPatchSetting::Off,
                "gpt-5",
                Some(EditFormat::ApplyPatch),
            ),
        ];
        let resolved: Vec<EditFormat> = cases
            .iter()
            .map(|(setting, id, configured)| resolve_edit_format(*setting, id, *configured))
            .collect();
        assert_eq!(
            resolved,
            vec![
                EditFormat::Edit,
                EditFormat::ApplyPatch,
                EditFormat::ApplyPatch,
                EditFormat::Edit,
                EditFormat::Edit,
            ]
        );
    }
}
