//! Session routing header for the `opencode` (Zen) and `opencode-go` providers.

use std::sync::OnceLock;

use crate::types::Model;

/// The header the opencode gateway routes requests by. `opencode-go` rejects a request that carries
/// no session (`MissingSessionID`), so every request to either opencode provider sends one.
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Append the opencode session header for the opencode providers, unless the model or caller headers
/// already set it. The value is the caller's session when it has one, else one id per process, so
/// genuinely sessionless calls stay routable; an empty session id counts as none. Other providers
/// are left untouched.
pub(crate) fn push_opencode_session_header(
    headers: &mut Vec<(String, String)>,
    model: &Model,
    session_id: Option<&str>,
) {
    static PROCESS_SESSION_ID: OnceLock<String> = OnceLock::new();
    if model.provider != "opencode" && model.provider != "opencode-go" {
        return;
    }
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(OPENCODE_SESSION_HEADER))
    {
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
    headers.push((OPENCODE_SESSION_HEADER.into(), value));
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

    #[test]
    fn opencode_providers_carry_the_callers_session() {
        for provider in ["opencode", "opencode-go"] {
            let mut headers = Vec::new();
            push_opencode_session_header(&mut headers, &model(provider), Some("s-1"));
            assert_eq!(
                headers,
                vec![(OPENCODE_SESSION_HEADER.to_string(), "s-1".to_string())]
            );
        }
    }

    #[test]
    fn a_sessionless_call_gets_one_stable_process_session() {
        let mut first = Vec::new();
        push_opencode_session_header(&mut first, &model("opencode-go"), None);
        let mut second = Vec::new();
        push_opencode_session_header(&mut second, &model("opencode-go"), None);
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert!(first[0].1.starts_with("pa-"));
    }

    #[test]
    fn an_empty_session_counts_as_sessionless() {
        let mut empty = Vec::new();
        push_opencode_session_header(&mut empty, &model("opencode-go"), Some(""));
        let mut sessionless = Vec::new();
        push_opencode_session_header(&mut sessionless, &model("opencode-go"), None);
        assert_eq!(empty, sessionless);
    }

    #[test]
    fn a_caller_set_header_is_kept() {
        let mut headers = vec![("X-OpenCode-Session".to_string(), "mine".to_string())];
        push_opencode_session_header(&mut headers, &model("opencode-go"), Some("s-1"));
        assert_eq!(
            headers,
            vec![("X-OpenCode-Session".to_string(), "mine".to_string())]
        );
    }

    #[test]
    fn other_providers_get_no_header() {
        let mut headers = Vec::new();
        push_opencode_session_header(&mut headers, &model("openai"), Some("s-1"));
        assert_eq!(headers, Vec::new());
    }
}
