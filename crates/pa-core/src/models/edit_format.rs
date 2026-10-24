//! The effective file-edit format for a session's model: the one decision
//! the prompt layers and the kernel skill binding both read, so the prompt
//! never documents `apply_patch` when the kernel does not bind it.

use pa_types::ai::EditFormat;

use crate::settings::ApplyPatchSetting;

/// The first GPT version trained on `apply_patch` as an official tool.
const FIRST_APPLY_PATCH_GPT: (u64, u64) = (5, 1);

/// Resolve the edit format: `applyPatch: "off"` wins, then the model's
/// configured `editFormat` (`models.json`), then the ID default (GPT-5.1 and
/// newer get `apply_patch`, everything else `edit`).
///
/// The default judges the model name inside `model_id`, whatever spelling
/// the provider uses: the last `/` segment (`openai/gpt-5.1` on `OpenRouter`
/// or Prime Inference), without a `:` variant suffix (`gpt-5.1:nitro`,
/// `:free`), the base model of an `OpenAI` fine-tune (`ft:gpt-5.1:org::id`),
/// and the name after a dotted vendor prefix (Bedrock-style
/// `openai.gpt-5.1`, `us.openai.gpt-5.1`). A fine-tune or a re-hosted model is
/// the same trained model, so it gets the same format.
#[must_use]
pub fn resolve_edit_format(
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
                    (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                        .then(|| part.parse::<u64>().ok())
                        .flatten()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The default by model ID, with and without provider-style prefixes,
    /// variant suffixes, fine-tune wrappers, and dotted vendor prefixes.
    #[test]
    fn the_id_default_starts_at_gpt_5_1() {
        let ids = [
            "gpt-5.1",
            "gpt-5.1-codex-mini",
            "gpt-5.2",
            "gpt-5.5",
            "gpt-6-luna",
            "openai/gpt-5.1",
            "openai/gpt-5.6-luna",
            "gpt-5.10",
            "gpt-5.10-codex",
            "openai/gpt-5.1:nitro",
            "openai/gpt-5.2:free",
            "gpt-5.1:online",
            "ft:gpt-5.1:acme::abc123",
            "ft:gpt-4.1-mini:acme::abc123",
            "openai.gpt-5.1",
            "us.openai.gpt-5.2",
            "openai.gpt-oss-120b-1:0",
            "openai/gpt-5:free",
            "chatgpt-4o-latest",
            "my-gpt-5.1",
            "gpt-5",
            "gpt-5-codex",
            "gpt-4.1",
            "gpt-oss-120b",
            "openai/gpt-oss-20b",
            "gpt-4o",
            "claude-sonnet-4-5",
            "deepseek-v4-flash",
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
            ("gpt-5.2", EditFormat::ApplyPatch),
            ("gpt-5.5", EditFormat::ApplyPatch),
            ("gpt-6-luna", EditFormat::ApplyPatch),
            ("openai/gpt-5.1", EditFormat::ApplyPatch),
            ("openai/gpt-5.6-luna", EditFormat::ApplyPatch),
            ("gpt-5.10", EditFormat::ApplyPatch),
            ("gpt-5.10-codex", EditFormat::ApplyPatch),
            ("openai/gpt-5.1:nitro", EditFormat::ApplyPatch),
            ("openai/gpt-5.2:free", EditFormat::ApplyPatch),
            ("gpt-5.1:online", EditFormat::ApplyPatch),
            ("ft:gpt-5.1:acme::abc123", EditFormat::ApplyPatch),
            ("ft:gpt-4.1-mini:acme::abc123", EditFormat::Edit),
            ("openai.gpt-5.1", EditFormat::ApplyPatch),
            ("us.openai.gpt-5.2", EditFormat::ApplyPatch),
            ("openai.gpt-oss-120b-1:0", EditFormat::Edit),
            ("openai/gpt-5:free", EditFormat::Edit),
            ("chatgpt-4o-latest", EditFormat::Edit),
            ("my-gpt-5.1", EditFormat::Edit),
            ("gpt-5", EditFormat::Edit),
            ("gpt-5-codex", EditFormat::Edit),
            ("gpt-4.1", EditFormat::Edit),
            ("gpt-oss-120b", EditFormat::Edit),
            ("openai/gpt-oss-20b", EditFormat::Edit),
            ("gpt-4o", EditFormat::Edit),
            ("claude-sonnet-4-5", EditFormat::Edit),
            ("deepseek-v4-flash", EditFormat::Edit),
        ];
        assert_eq!(resolved, expected);
    }

    /// An explicit `editFormat` wins over the ID default in both directions,
    /// and `applyPatch: "off"` wins over an explicit `apply_patch`.
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
