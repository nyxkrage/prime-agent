//! The effective file-edit format for a session's model: the one decision
//! the prompt layers and the kernel skill binding both read, so the prompt
//! never documents `apply_patch` when the kernel does not bind it.

use pa_types::ai::EditFormat;

use crate::settings::ApplyPatchSetting;

/// The first GPT version trained on `apply_patch` as an official tool.
const FIRST_APPLY_PATCH_GPT: (u64, u64) = (5, 1);

/// Resolve the edit format: `applyPatch: "off"` wins, then the model's
/// configured `editFormat` (`models.json`), then the ID default (GPT-5.1 and
/// newer get `apply_patch`, everything else `edit`). `model_id` is the bare
/// model ID; a slash-carrying ID (`openai/gpt-5.1` on `OpenRouter` or Prime
/// Inference) is judged by its last segment.
#[must_use]
pub fn resolve_edit_format(
    setting: ApplyPatchSetting,
    model_id: &str,
    configured: Option<EditFormat>,
) -> EditFormat {
    match setting {
        ApplyPatchSetting::Off => EditFormat::Edit,
        ApplyPatchSetting::Auto => configured.unwrap_or_else(|| {
            let bare = model_id.rsplit('/').next().unwrap_or(model_id);
            if gpt_version(bare).is_some_and(|version| version >= FIRST_APPLY_PATCH_GPT) {
                EditFormat::ApplyPatch
            } else {
                EditFormat::Edit
            }
        }),
    }
}

/// `(major, minor)` of a `gpt-<major>[.<minor>][-suffix]` ID; a missing minor
/// reads as 0. Anything else (`gpt-oss-120b`, `gpt-4o`) is not versioned.
fn gpt_version(model_id: &str) -> Option<(u64, u64)> {
    let rest = model_id.to_ascii_lowercase();
    let rest = rest.strip_prefix("gpt-")?;
    let version = rest.split('-').next().unwrap_or_default();
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    let parse = |part: &str| {
        // Digits only: `parse` alone would accept a leading `+`.
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse::<u64>().ok())
            .flatten()
    };
    Some((parse(major)?, parse(minor)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default by model ID, with and without provider-style prefixes.
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
            .map(|id| (*id, resolve_edit_format(ApplyPatchSetting::Auto, id, None)))
            .collect();
        let expected: Vec<(&str, EditFormat)> = vec![
            ("gpt-5.1", EditFormat::ApplyPatch),
            ("gpt-5.1-codex-mini", EditFormat::ApplyPatch),
            ("gpt-5.2", EditFormat::ApplyPatch),
            ("gpt-5.5", EditFormat::ApplyPatch),
            ("gpt-6-luna", EditFormat::ApplyPatch),
            ("openai/gpt-5.1", EditFormat::ApplyPatch),
            ("openai/gpt-5.6-luna", EditFormat::ApplyPatch),
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
            (ApplyPatchSetting::Off, "gpt-5.1", None),
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
