//! Routing headers for the `opencode` (Zen) and `opencode-go` providers (TS `withOpenCodeHeaders`).

use std::sync::OnceLock;

use crate::types::Model;

/// Append `user-agent: prime-agent` and `x-opencode-session` for the opencode providers, each
/// unless already set (case-insensitive, like the TS lowercase merge, so a caller's header wins).
/// `opencode-go` rejects a request without a session (`MissingSessionID`), so a sessionless (or
/// empty-session) call falls back to one id per process.
pub(crate) fn push_opencode_headers(
    headers: &mut Vec<(String, String)>,
    model: &Model,
    session_id: Option<&str>,
) {
    static PROCESS_SESSION_ID: OnceLock<String> = OnceLock::new();
    if model.provider != "opencode" && model.provider != "opencode-go" {
        return;
    }
    let has = |headers: &[(String, String)], wanted: &str| {
        headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(wanted))
    };
    if !has(headers, "user-agent") {
        headers.push(("user-agent".into(), "prime-agent".into()));
    }
    if has(headers, "x-opencode-session") {
        return;
    }
    let value = session_id.filter(|id| !id.is_empty()).map_or_else(
        || {
            PROCESS_SESSION_ID
                .get_or_init(|| format!("pa-{:032x}", rand::random::<u128>()))
                .clone()
        },
        str::to_string,
    );
    headers.push(("x-opencode-session".into(), value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-responses", "provider": provider,
            "baseUrl": "https://opencode.ai/zen/go/v1", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn opencode_headers_by_provider_and_caller_headers() {
        let caller = [("User-Agent", "custom/1"), ("X-OpenCode-Session", "mine")];
        let opencode = [("user-agent", "prime-agent"), ("x-opencode-session", "s-1")];
        let none: &[(&str, &str)] = &[];
        let cases = [
            ("opencode", none, &opencode[..]),
            ("opencode-go", none, &opencode[..]),
            ("opencode-go", &caller[..], &caller[..]),
            ("openai", none, none),
        ];
        for (provider, initial, expected) in cases {
            let mut headers = pairs(initial);
            push_opencode_headers(&mut headers, &model(provider), Some("s-1"));
            assert_eq!(headers, pairs(expected), "{provider} {initial:?}");
        }
    }

    #[test]
    fn sessionless_and_empty_session_calls_share_one_process_session() {
        let headers = |session_id| {
            let mut headers = Vec::new();
            push_opencode_headers(&mut headers, &model("opencode-go"), session_id);
            headers
        };
        let sessionless = headers(None);
        assert_eq!(sessionless, headers(None));
        assert_eq!(sessionless, headers(Some("")));
        assert!(sessionless[1].1.starts_with("pa-"), "{sessionless:?}");
    }
}
