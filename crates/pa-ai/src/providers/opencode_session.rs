//! Routing headers for the `opencode` (Zen) and `opencode-go` providers (TS `withOpenCodeHeaders`).

use std::sync::OnceLock;

use crate::types::Model;

/// The header the opencode gateway routes requests by. `opencode-go` rejects a request that carries
/// no session (`MissingSessionID`), so every request to either opencode provider sends one.
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";
/// The client identity the opencode gateway sees (TS `withOpenCodeHeaders` defaults `User-Agent`).
const OPENCODE_USER_AGENT: (&str, &str) = ("user-agent", "prime-agent");

/// Append the opencode headers for the opencode providers: the `prime-agent` user agent and the
/// session routing header, each unless the model or caller headers already set it (names compare
/// case-insensitively, like the TS lowercase merge, so a caller's header always wins). The session
/// is the caller's when it has one, else one id per process, so genuinely sessionless calls stay
/// routable; an empty session id counts as none. Other providers are left untouched.
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
    if !has(headers, OPENCODE_USER_AGENT.0) {
        headers.push((OPENCODE_USER_AGENT.0.into(), OPENCODE_USER_AGENT.1.into()));
    }
    if has(headers, OPENCODE_SESSION_HEADER) {
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

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn opencode_providers_carry_the_user_agent_and_the_callers_session() {
        for provider in ["opencode", "opencode-go"] {
            let mut headers = Vec::new();
            push_opencode_headers(&mut headers, &model(provider), Some("s-1"));
            assert_eq!(
                headers,
                pairs(&[("user-agent", "prime-agent"), ("x-opencode-session", "s-1")])
            );
        }
    }

    #[test]
    fn a_sessionless_call_gets_one_stable_process_session() {
        let mut first = Vec::new();
        push_opencode_headers(&mut first, &model("opencode-go"), /*session_id*/ None);
        let mut second = Vec::new();
        push_opencode_headers(&mut second, &model("opencode-go"), /*session_id*/ None);
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert!(first[1].1.starts_with("pa-"));
    }

    #[test]
    fn an_empty_session_counts_as_sessionless() {
        let mut empty = Vec::new();
        push_opencode_headers(&mut empty, &model("opencode-go"), Some(""));
        let mut sessionless = Vec::new();
        push_opencode_headers(
            &mut sessionless,
            &model("opencode-go"),
            /*session_id*/ None,
        );
        assert_eq!(empty, sessionless);
    }

    /// TS lowercases every name and lets the caller's headers win: a caller's user agent and
    /// session header (in any casing) are kept, never duplicated.
    #[test]
    fn caller_set_headers_are_kept() {
        let mut headers = pairs(&[("User-Agent", "custom/1"), ("X-OpenCode-Session", "mine")]);
        push_opencode_headers(&mut headers, &model("opencode-go"), Some("s-1"));
        assert_eq!(
            headers,
            pairs(&[("User-Agent", "custom/1"), ("X-OpenCode-Session", "mine")])
        );
    }

    #[test]
    fn other_providers_get_no_header() {
        let mut headers = Vec::new();
        push_opencode_headers(&mut headers, &model("openai"), Some("s-1"));
        assert_eq!(headers, Vec::new());
    }
}
