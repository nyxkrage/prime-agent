//! `rlm.spawn(..., env={...})`: the per-child kernel env overrides.
//!
//! A `str` value is set as given (a literal); `None` copies the
//! orchestrator's value (a pass-through). The request carries only the
//! names of pass-throughs; their values are resolved host-side from the
//! environment a local kernel gets, so they never appear in the model's
//! code, the host request, or the transcript. A child never inherits its
//! parent's overrides. Name rules and caps live in
//! [`crate::kernel::remote_policy`].
//!
//! Threat model: a value passed to a host is trusted to that host, and the
//! model can reach the orchestrator's environment anyway. [`redact`] masks
//! pass-through values in kernel output as best-effort hygiene, not a
//! security boundary: a transformed value (reversed, encoded) passes.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::kernel::remote_policy::{
    kernel_env_name_refusal, KERNEL_ENV_MAX_ENTRIES, KERNEL_ENV_MAX_TOTAL_BYTES,
};

/// The global setting that turns pass-through to remote hosts off.
pub(crate) const REMOTE_ENV_PASSTHROUGH_SETTING: &str = "remoteEnvPassthrough";

/// Pass-through values shorter than this are not redacted from kernel
/// output: masking every `"1"` or `"true"` would mangle unrelated text.
const REDACTION_MIN_CHARS: usize = 8;

const REDACTION_MASK: &str = "***";

/// One `env=` entry.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum KernelEnvValue {
    /// Set to this value.
    Literal(String),
    /// Copy the orchestrator's value of the same name.
    Passthrough,
}

/// A refused `env=`. Typed so the spawn seam counts refusals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct KernelEnvRefusal(pub(crate) String);

/// The validated `env=` of one spawn: literal values and pass-through
/// names. It never holds a pass-through value, so it may be persisted (the
/// durable create replays it) and serialized (`null` for a pass-through).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct KernelEnvOverrides {
    entries: BTreeMap<String, KernelEnvValue>,
}

impl std::fmt::Debug for KernelEnvOverrides {
    /// Names and kinds only: a literal value may still be sensitive.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.entries.iter().map(|(name, value)| {
                let kind = match value {
                    KernelEnvValue::Literal(_) => "literal",
                    KernelEnvValue::Passthrough => "passthrough",
                };
                (name, kind)
            }))
            .finish()
    }
}

impl KernelEnvOverrides {
    /// Validate an `env=` payload: an object of names to `str` or `null`
    /// (`null`, or an absent payload, is no overrides).
    ///
    /// # Errors
    ///
    /// Returns a refusal naming the bad name or value, or the cap exceeded.
    pub fn from_json(value: &Value) -> Result<Self, KernelEnvRefusal> {
        let object = match value {
            Value::Null => return Ok(Self::default()),
            Value::Object(object) => object,
            Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
                return Err(KernelEnvRefusal(format!(
                    "rlm.spawn env must be a dict of str to str | None, got {}",
                    json_type_name(value)
                )));
            }
        };
        if object.len() > KERNEL_ENV_MAX_ENTRIES {
            return Err(KernelEnvRefusal(format!(
                "rlm.spawn env sets {} variables; at most {KERNEL_ENV_MAX_ENTRIES} are allowed",
                object.len()
            )));
        }
        let mut entries = BTreeMap::new();
        let mut total = 0usize;
        for (name, value) in object {
            if let Some(refusal) = kernel_env_name_refusal(name) {
                return Err(KernelEnvRefusal(refusal));
            }
            total += name.len();
            let value = match value {
                Value::Null => KernelEnvValue::Passthrough,
                Value::String(literal) => {
                    if literal.contains('\0') {
                        return Err(KernelEnvRefusal(format!(
                            "rlm.spawn env value for {name} contains a NUL byte"
                        )));
                    }
                    total += literal.len();
                    KernelEnvValue::Literal(literal.clone())
                }
                Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
                    return Err(KernelEnvRefusal(format!(
                        "rlm.spawn env value for {name} must be a str or None, got {}",
                        json_type_name(value)
                    )));
                }
            };
            entries.insert(name.clone(), value);
        }
        if total > KERNEL_ENV_MAX_TOTAL_BYTES {
            return Err(KernelEnvRefusal(format!(
                "rlm.spawn env carries {total} bytes; at most {KERNEL_ENV_MAX_TOTAL_BYTES} are allowed"
            )));
        }
        Ok(Self { entries })
    }

    /// The wire form [`Self::from_json`] reads: literals as strings,
    /// pass-throughs as `null`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        for (name, value) in &self.entries {
            let value = match value {
                KernelEnvValue::Literal(literal) => Value::String(literal.clone()),
                KernelEnvValue::Passthrough => Value::Null,
            };
            object.insert(name.clone(), value);
        }
        Value::Object(object)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn literal_count(&self) -> usize {
        self.entries
            .values()
            .filter(|value| matches!(value, KernelEnvValue::Literal(_)))
            .count()
    }

    pub(crate) fn passthrough_count(&self) -> usize {
        self.entries.len() - self.literal_count()
    }

    /// Refuse any pass-through: called for a remote host when the global
    /// `remoteEnvPassthrough` setting is off. Literal values stay allowed.
    ///
    /// # Errors
    ///
    /// Returns a refusal naming the first pass-through and the setting.
    pub fn refuse_passthroughs(&self) -> Result<(), KernelEnvRefusal> {
        match self
            .entries
            .iter()
            .find(|(_, value)| matches!(value, KernelEnvValue::Passthrough))
        {
            Some((name, _)) => Err(KernelEnvRefusal(format!(
                "rlm.spawn env {name}=None would pass the orchestrator's value to a remote host, and the global {REMOTE_ENV_PASSTHROUGH_SETTING} setting is off (or unreadable); pass a literal value, or set {REMOTE_ENV_PASSTHROUGH_SETTING} to true in the global settings"
            ))),
            None => Ok(()),
        }
    }

    /// Check that every pass-through resolves against `orchestrator_env`
    /// (and the resolved payload fits the cap), so a spawn fails naming the
    /// missing variable instead of the child's first kernel start.
    ///
    /// # Errors
    ///
    /// Returns the refusal [`Self::resolve`] would.
    pub fn check_resolvable(
        &self,
        orchestrator_env: impl Fn(&str) -> Option<String>,
    ) -> Result<(), KernelEnvRefusal> {
        self.resolve(orchestrator_env).map(drop)
    }

    /// Fill in the pass-through values from `orchestrator_env` (the
    /// environment a local kernel gets).
    ///
    /// # Errors
    ///
    /// Returns a refusal naming a pass-through variable the orchestrator
    /// does not have, or the resolved payload's size over the cap.
    pub(crate) fn resolve(
        &self,
        orchestrator_env: impl Fn(&str) -> Option<String>,
    ) -> Result<ResolvedKernelEnv, KernelEnvRefusal> {
        let mut values = Vec::with_capacity(self.entries.len());
        let mut redactions = Vec::new();
        let mut total = 0usize;
        for (name, value) in &self.entries {
            let value = match value {
                KernelEnvValue::Literal(literal) => literal.clone(),
                KernelEnvValue::Passthrough => {
                    let Some(value) = orchestrator_env(name) else {
                        return Err(KernelEnvRefusal(format!(
                            "{name} is not set on the orchestrator (rlm.spawn env {name}=None passes the orchestrator's value through)"
                        )));
                    };
                    if value.chars().count() >= REDACTION_MIN_CHARS {
                        redactions.push(value.clone());
                    }
                    value
                }
            };
            total += name.len() + value.len();
            values.push((name.clone(), value));
        }
        if total > KERNEL_ENV_MAX_TOTAL_BYTES {
            return Err(KernelEnvRefusal(format!(
                "rlm.spawn env resolves to {total} bytes; at most {KERNEL_ENV_MAX_TOTAL_BYTES} are allowed"
            )));
        }
        // Longest first, so a value that contains another is masked whole.
        redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
        redactions.dedup();
        Ok(ResolvedKernelEnv { values, redactions })
    }
}

/// The overrides with their values filled in: what a kernel launcher
/// applies. Its `Debug` never prints a value.
#[derive(Clone, Default)]
pub(crate) struct ResolvedKernelEnv {
    values: Vec<(String, String)>,
    redactions: Vec<String>,
}

impl std::fmt::Debug for ResolvedKernelEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.values.iter().map(|(name, _)| name))
            .finish()
    }
}

impl ResolvedKernelEnv {
    pub(crate) fn values(&self) -> &[(String, String)] {
        &self.values
    }

    /// The pass-through values long enough to mask, longest first.
    pub(crate) fn redactions(&self) -> &[String] {
        &self.redactions
    }
}

/// Mask every occurrence of `values` (longest first) in `text`. One chunk
/// at a time; a stream's chunks go through [`redact_stream_chunk`].
#[must_use]
pub(crate) fn redact<'a>(text: &'a str, values: &[String]) -> Cow<'a, str> {
    let mut text = Cow::Borrowed(text);
    for value in values {
        if text.contains(value.as_str()) {
            text = Cow::Owned(text.replace(value.as_str(), REDACTION_MASK));
        }
    }
    text
}

/// Mask one chunk of a stream whose chunks may split a value: `carry` (the
/// previous chunk's held tail) is prepended, and the masked text's longest
/// tail that could begin a value is held back in `carry` for the next
/// chunk. Returns the text to emit now; the caller emits a leftover
/// `carry` as-is when the stream ends (or another stream interleaves).
pub(crate) fn redact_stream_chunk(carry: &mut String, chunk: &str, values: &[String]) -> String {
    let text = std::mem::take(carry) + chunk;
    let masked = redact(&text, values).into_owned();
    let held = masked
        .char_indices()
        .map(|(start, _)| start)
        .find(|start| {
            let tail = &masked[*start..];
            values
                .iter()
                .any(|value| value.len() > tail.len() && value.starts_with(tail))
        })
        .unwrap_or(masked.len());
    *carry = masked[held..].to_string();
    masked[..held].to_string()
}

/// Mask `values` in every string of a JSON payload (a display bundle, a host
/// request), in place.
pub(crate) fn redact_json(payload: &mut Value, values: &[String]) {
    match payload {
        Value::String(text) => {
            if let Cow::Owned(masked) = redact(text, values) {
                *text = masked;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item, values);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                redact_json(item, values);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "None",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refusal(value: &Value) -> String {
        KernelEnvOverrides::from_json(value).unwrap_err().0
    }

    #[test]
    fn literals_and_passthroughs_round_trip_without_values() {
        let overrides =
            KernelEnvOverrides::from_json(&json!({ "EPOCHS": "3", "HF_TOKEN": null })).unwrap();
        assert_eq!(
            (overrides.literal_count(), overrides.passthrough_count()),
            (1, 1)
        );
        assert_eq!(
            overrides.to_json(),
            json!({ "EPOCHS": "3", "HF_TOKEN": null })
        );
        assert_eq!(
            format!("{overrides:?}"),
            r#"{"EPOCHS": "literal", "HF_TOKEN": "passthrough"}"#
        );
        assert_eq!(
            KernelEnvOverrides::from_json(&Value::Null).unwrap(),
            KernelEnvOverrides::default()
        );
    }

    #[test]
    fn reserved_and_malformed_names_bad_values_and_oversize_payloads_are_refused() {
        let reserved = |name: &str| {
            format!("rlm.spawn env name {name} is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV")
        };
        let invalid = |name: &str| {
            format!("rlm.spawn env name {name:?} is invalid: names must match [A-Z_][A-Z0-9_]*")
        };
        for (name, expected) in [
            ("PATH", reserved("PATH")),
            ("HOME", reserved("HOME")),
            ("VIRTUAL_ENV", reserved("VIRTUAL_ENV")),
            ("RLM_REMOTE_HOST", reserved("RLM_REMOTE_HOST")),
            (
                "PRIME_AGENT_KERNEL_OWNER_PID",
                reserved("PRIME_AGENT_KERNEL_OWNER_PID"),
            ),
            ("PYTHONPATH", reserved("PYTHONPATH")),
            ("lower", invalid("lower")),
            ("1ABC", invalid("1ABC")),
            ("A-B", invalid("A-B")),
            ("", invalid("")),
        ] {
            // Reserved names are refused for a literal and a pass-through alike.
            assert_eq!(refusal(&json!({ name: "x" })), expected);
            assert_eq!(refusal(&json!({ name: null })), expected);
        }
        let long = "A".repeat(129);
        assert_eq!(
            refusal(&json!({ long.clone(): "x" })),
            format!("rlm.spawn env name {long:?} is longer than 128 bytes")
        );
        assert_eq!(
            refusal(&json!({ "EPOCHS": 3 })),
            "rlm.spawn env value for EPOCHS must be a str or None, got int"
        );
        assert_eq!(
            refusal(&json!({ "EPOCHS": ["3"] })),
            "rlm.spawn env value for EPOCHS must be a str or None, got list"
        );
        assert_eq!(
            refusal(&json!(["EPOCHS"])),
            "rlm.spawn env must be a dict of str to str | None, got list"
        );
        assert_eq!(
            refusal(&json!({ "A": "x\u{0}y" })),
            "rlm.spawn env value for A contains a NUL byte"
        );
        let big = "x".repeat(40 * 1024);
        assert_eq!(
            refusal(&json!({ "A": big, "B": big })),
            "rlm.spawn env carries 81922 bytes; at most 65536 are allowed"
        );
        let many: Map<String, Value> = (0..65).map(|i| (format!("V{i}"), json!("x"))).collect();
        assert_eq!(
            refusal(&Value::Object(many)),
            "rlm.spawn env sets 65 variables; at most 64 are allowed"
        );
    }

    #[test]
    fn passthroughs_resolve_from_the_orchestrator_env_and_a_missing_one_is_named() {
        let overrides = KernelEnvOverrides::from_json(
            &json!({ "EPOCHS": "3", "HF_TOKEN": null, "SHORT": null }),
        )
        .unwrap();
        let orchestrator = |name: &str| match name {
            "HF_TOKEN" => Some("hf_secret_value".to_string()),
            "SHORT" => Some("abc".to_string()),
            _ => None,
        };
        let resolved = overrides.resolve(orchestrator).unwrap();
        assert_eq!(
            resolved.values(),
            [
                ("EPOCHS".to_string(), "3".to_string()),
                ("HF_TOKEN".to_string(), "hf_secret_value".to_string()),
                ("SHORT".to_string(), "abc".to_string()),
            ]
        );
        // Values under REDACTION_MIN_CHARS are never masked.
        assert_eq!(resolved.redactions(), ["hf_secret_value".to_string()]);
        assert_eq!(
            format!("{resolved:?}"),
            r#"["EPOCHS", "HF_TOKEN", "SHORT"]"#
        );

        let missing = KernelEnvOverrides::from_json(&json!({ "HF_TOKEN": null }))
            .unwrap()
            .resolve(|_| None)
            .unwrap_err();
        assert_eq!(
            missing.0,
            "HF_TOKEN is not set on the orchestrator (rlm.spawn env HF_TOKEN=None passes the orchestrator's value through)"
        );
        let oversize = KernelEnvOverrides::from_json(&json!({ "BIG": null }))
            .unwrap()
            .resolve(|_| Some("x".repeat(70 * 1024)))
            .unwrap_err();
        assert_eq!(
            oversize.0,
            "rlm.spawn env resolves to 71683 bytes; at most 65536 are allowed"
        );
    }

    #[test]
    fn the_kill_switch_refuses_passthroughs_but_not_literals() {
        let literal = KernelEnvOverrides::from_json(&json!({ "EPOCHS": "3" })).unwrap();
        assert_eq!(literal.refuse_passthroughs(), Ok(()));
        let passthrough =
            KernelEnvOverrides::from_json(&json!({ "EPOCHS": "3", "HF_TOKEN": null })).unwrap();
        assert_eq!(
            passthrough.refuse_passthroughs().unwrap_err().0,
            "rlm.spawn env HF_TOKEN=None would pass the orchestrator's value to a remote host, and the global remoteEnvPassthrough setting is off (or unreadable); pass a literal value, or set remoteEnvPassthrough to true in the global settings"
        );
    }

    /// A value split across stream chunks is held back until it can be
    /// masked whole; text that only looks like a value's start is emitted
    /// once the next chunk shows it is not one.
    #[test]
    fn stream_chunks_that_split_a_value_are_masked_whole() {
        let values = vec!["hf_secret_value".to_string()];
        let mut carry = String::new();
        let emitted: Vec<String> = ["token: hf_sec", "ret_value done hf_", "x end"]
            .iter()
            .map(|chunk| redact_stream_chunk(&mut carry, chunk, &values))
            .collect();
        assert_eq!(emitted, ["token: ", "*** done ", "hf_x end"]);
        assert_eq!(carry, "");
        // A held tail stays in the carry until the stream ends.
        assert_eq!(
            redact_stream_chunk(&mut carry, "ends in hf_s", &values),
            "ends in "
        );
        assert_eq!(carry, "hf_s");
    }

    #[test]
    fn json_payloads_are_masked_in_every_string() {
        let values = vec!["hf_secret_value".to_string()];
        let mut payload = json!({
            "path": "/srv/.env",
            "diff": ["-TOKEN=old", "+TOKEN=hf_secret_value"],
            "nested": { "message": "use hf_secret_value", "count": 3 },
        });
        redact_json(&mut payload, &values);
        assert_eq!(
            payload,
            json!({
                "path": "/srv/.env",
                "diff": ["-TOKEN=old", "+TOKEN=***"],
                "nested": { "message": "use ***", "count": 3 },
            })
        );
    }

    #[test]
    fn redaction_masks_long_values_longest_first() {
        let values = vec!["secret-value-long".to_string(), "secret-value".to_string()];
        assert_eq!(
            redact("a secret-value-long and secret-value b", &values),
            "a *** and *** b"
        );
        assert!(matches!(redact("nothing here", &values), Cow::Borrowed(_)));
    }
}
