//! `rlm.spawn(..., env={...})`: the per-child kernel env overrides.
//!
//! A spawn names each variable it wants in the child's kernel: a `str`
//! value is set as given (a literal), `None` copies the orchestrator's
//! value (a pass-through). The request carries only the names of
//! pass-throughs; their values are read here, host-side, from the same
//! environment a local kernel gets (the session worker's env overlaid with
//! the kernel env), so they never appear in the model's code, the host
//! request, the transcript, provider requests, or logs.
//!
//! A child never inherits its parent's overrides: each spawn names its
//! variables again. The name rules, reserved names, and size caps live in
//! [`crate::kernel::remote_policy`].
//!
//! Threat model: a value passed to a host is trusted to that host (root
//! there can read the kernel's environment), and the model can reach the
//! orchestrator's environment anyway (a local kernel inherits it).
//! Pass-through exists to keep values out of transcripts and logs, and
//! [`redact`] masks pass-through values the kernel prints back as best
//! effort hygiene, not as a security boundary.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::kernel::remote_policy::{
    kernel_env_name_refusal, KERNEL_ENV_MAX_ENTRIES, KERNEL_ENV_MAX_TOTAL_BYTES,
};

/// The global setting that turns pass-through to remote hosts off.
pub const REMOTE_ENV_PASSTHROUGH_SETTING: &str = "remoteEnvPassthrough";

/// Pass-through values shorter than this are not redacted from kernel
/// output: masking every `"1"` or `"true"` would mangle unrelated text.
pub const REDACTION_MIN_CHARS: usize = 8;

/// The mask that replaces a pass-through value in kernel output.
pub const REDACTION_MASK: &str = "***";

/// One `env=` entry.
#[derive(Clone, PartialEq, Eq)]
pub enum KernelEnvValue {
    /// Set to this value.
    Literal(String),
    /// Copy the orchestrator's value of the same name.
    Passthrough,
}

/// A refused `env=`: an invalid name or value, a payload over the caps, a
/// pass-through variable the orchestrator does not have, or a pass-through
/// to a remote host the `remoteEnvPassthrough` setting turns off. Typed so
/// the spawn seam counts refusals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelEnvRefusal(pub String);

impl std::fmt::Display for KernelEnvRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KernelEnvRefusal {}

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

    /// How many variables are set to a literal value.
    #[must_use]
    pub fn literal_count(&self) -> usize {
        self.entries
            .values()
            .filter(|value| matches!(value, KernelEnvValue::Literal(_)))
            .count()
    }

    /// How many variables pass the orchestrator's value through.
    #[must_use]
    pub fn passthrough_count(&self) -> usize {
        self.entries.len() - self.literal_count()
    }

    /// Refuse pass-throughs to a remote host when the global
    /// `remoteEnvPassthrough` setting turns them off; literal values stay
    /// allowed.
    ///
    /// # Errors
    ///
    /// Returns a refusal naming the first pass-through and the setting.
    pub fn check_remote_passthrough(
        &self,
        passthrough_allowed: bool,
    ) -> Result<(), KernelEnvRefusal> {
        if passthrough_allowed {
            return Ok(());
        }
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

    /// Fill in the pass-through values from `orchestrator_env` (the
    /// environment a local kernel gets).
    ///
    /// # Errors
    ///
    /// Returns a refusal naming a pass-through variable the orchestrator
    /// does not have, or the resolved payload's size over the cap.
    pub fn resolve(
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
pub struct ResolvedKernelEnv {
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
    /// The variables to set, by name.
    #[must_use]
    pub fn values(&self) -> &[(String, String)] {
        &self.values
    }

    /// The pass-through values long enough to mask in kernel output
    /// ([`REDACTION_MIN_CHARS`]), longest first.
    #[must_use]
    pub fn redactions(&self) -> &[String] {
        &self.redactions
    }
}

/// Mask every occurrence of `values` (longest first) in `text`. Best
/// effort: a value split across two output chunks passes unmasked.
#[must_use]
pub fn redact<'a>(text: &'a str, values: &[String]) -> Cow<'a, str> {
    let mut text = Cow::Borrowed(text);
    for value in values {
        if text.contains(value.as_str()) {
            text = Cow::Owned(text.replace(value.as_str(), REDACTION_MASK));
        }
    }
    text
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
        assert_eq!(overrides.to_json(), json!({ "EPOCHS": "3", "HF_TOKEN": null }));
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
        for (name, expected) in [
            ("PATH", "rlm.spawn env name PATH is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV"),
            ("HOME", "rlm.spawn env name HOME is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV"),
            ("VIRTUAL_ENV", "rlm.spawn env name VIRTUAL_ENV is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV"),
            ("RLM_REMOTE_HOST", "rlm.spawn env name RLM_REMOTE_HOST is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV"),
            ("PRIME_AGENT_KERNEL_OWNER_PID", "rlm.spawn env name PRIME_AGENT_KERNEL_OWNER_PID is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV"),
            ("PYTHONPATH", "rlm.spawn env name PYTHONPATH is reserved: the kernel launcher owns RLM_*, PRIME_AGENT_*, PYTHON*, PATH, HOME, and VIRTUAL_ENV"),
            ("lower", "rlm.spawn env name \"lower\" is invalid: names must match [A-Z_][A-Z0-9_]*"),
            ("1ABC", "rlm.spawn env name \"1ABC\" is invalid: names must match [A-Z_][A-Z0-9_]*"),
            ("A-B", "rlm.spawn env name \"A-B\" is invalid: names must match [A-Z_][A-Z0-9_]*"),
            ("", "rlm.spawn env name \"\" is invalid: names must match [A-Z_][A-Z0-9_]*"),
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
        assert_eq!(literal.check_remote_passthrough(false), Ok(()));
        let passthrough =
            KernelEnvOverrides::from_json(&json!({ "EPOCHS": "3", "HF_TOKEN": null })).unwrap();
        assert_eq!(passthrough.check_remote_passthrough(true), Ok(()));
        assert_eq!(
            passthrough.check_remote_passthrough(false).unwrap_err().0,
            "rlm.spawn env HF_TOKEN=None would pass the orchestrator's value to a remote host, and the global remoteEnvPassthrough setting is off (or unreadable); pass a literal value, or set remoteEnvPassthrough to true in the global settings"
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
