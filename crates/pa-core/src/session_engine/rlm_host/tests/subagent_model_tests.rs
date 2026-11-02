//! The subagent-model policy at the RLM bridge.
use super::*;

/// `subagentModels` narrows `rlm.find_models` to exactly the filtered set;
/// an alias as a pattern matches nothing; the query matches the display
/// name and the alias; `enabledModels` never filters.
#[tokio::test]
async fn find_models_applies_the_subagent_filter() {
    let dir = tempfile::TempDir::new().unwrap();
    let wiring = wired(dir.path(), None);
    let glm = json!({
        "provider": "test-provider",
        "id": "glm-5.3",
        "name": "GLM 5.3",
        "selector": "test-provider/glm-5.3",
    });
    let turbo = json!({
        "provider": "test-provider",
        "id": "glm-5.3-turbo",
        "name": "GLM Turbo",
        "selector": "test-provider/glm-5.3-turbo",
        "alias": "turbo",
    });
    let cases = [
        (
            r#"{ "subagentModels": ["test-provider/glm-5.3-turbo", "test-provider/glm-5.3"] }"#,
            "",
            vec![glm.clone(), turbo.clone()],
        ),
        (
            r#"{ "subagentModels": ["*turbo"] }"#,
            "glm",
            vec![turbo.clone()],
        ),
        (r#"{ "subagentModels": ["turbo"] }"#, "", Vec::new()),
        (
            r#"{ "subagentModels": ["test-provider/*"] }"#,
            "GLM Turbo",
            vec![turbo.clone()],
        ),
        (
            r#"{ "subagentModels": ["test-provider/*"] }"#,
            "turbo",
            vec![turbo.clone()],
        ),
        (
            r#"{ "enabledModels": ["test-provider/glm-5.3"] }"#,
            "test-provider/",
            vec![glm, turbo],
        ),
    ];
    for (settings, query, expected) in cases {
        std::fs::write(dir.path().join("settings.json"), settings).unwrap();
        let response = call(
            &wiring,
            "rlm.find_models",
            json!({ "type": "rlm.find_models", "query": query }),
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            json!({ "models": expected }),
            "{settings} {query:?}"
        );
    }
}

/// Counts filtered finds, refusals, and admitted alias spawns; a refused
/// alias spawn counts as a refusal only.
#[tokio::test]
async fn the_bridge_counts_refusals_and_admitted_alias_spawns() {
    let dir = tempfile::TempDir::new().unwrap();
    let host = RecordingHost::new();
    *host.refused_selector.lock().unwrap() = Some("test-provider/glm-5.3-turbo".to_string());
    let wiring = wired(
        dir.path(),
        Some(Arc::clone(&host) as Arc<dyn RlmSubagentHost>),
    );
    let telemetry = Arc::new(crate::session_engine::telemetry::SessionTelemetry::counting_only());
    assert!(wiring.rlm.telemetry.set(Arc::clone(&telemetry)).is_ok());
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "subagentModels": ["test-provider/glm-5.3"] }"#,
    )
    .unwrap();
    call(
        &wiring,
        "rlm.find_models",
        json!({ "type": "rlm.find_models", "query": "" }),
    )
    .await
    .unwrap();
    let spawn = json!({ "type": "rlm.run", "prompt": "go", "kwargs": { "model": "turbo" } });
    let create = json!({
        "type": "rlm.create_session", "prompt": "go", "kwargs": { "model": "turbo" }
    });
    for (request_type, data) in [("rlm.run", &spawn), ("rlm.create_session", &create)] {
        let error = call(&wiring, request_type, data.clone()).await.unwrap_err();
        assert!(error
            .downcast_ref::<crate::models::SubagentModelRefusal>()
            .is_some());
    }
    assert_eq!(telemetry.subagent_model_policy_counts(), [1, 0, 2]);
    *host.refused_selector.lock().unwrap() = None;
    call(&wiring, "rlm.run", spawn).await.unwrap();
    call(&wiring, "rlm.create_session", create).await.unwrap();
    call(
        &wiring,
        "rlm.run",
        json!({ "type": "rlm.run", "prompt": "go", "kwargs": { "model": "test-provider/glm-5.3" } }),
    )
    .await
    .unwrap();
    assert_eq!(telemetry.subagent_model_policy_counts(), [1, 2, 2]);
}

/// The lenient settings load would list every model for a malformed value.
#[tokio::test]
async fn find_models_fails_closed_on_a_malformed_subagent_filter() {
    let dir = tempfile::TempDir::new().unwrap();
    let wiring = wired(dir.path(), None);
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "subagentModels": "turbo" }"#,
    )
    .unwrap();
    let error = call(
        &wiring,
        "rlm.find_models",
        json!({ "type": "rlm.find_models", "query": "" }),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "rlm.find_models: the subagent model filter (settings \"subagentModels\") could not be \
         read (subagentModels is present but is not an array of strings); no model is permitted \
         for subagents until the global settings.json is fixed."
    );
}

/// An alias in `rlm.spawn(model=...)` reaches the host as its selector.
#[tokio::test]
async fn spawn_resolves_a_model_alias_to_its_selector() {
    let dir = tempfile::TempDir::new().unwrap();
    let host = RecordingHost::new();
    let spawn_requests = Arc::clone(&host.spawn_requests);
    let wiring = wired(dir.path(), Some(host));
    call(
        &wiring,
        "rlm.run",
        json!({ "type": "rlm.run", "prompt": "go", "kwargs": { "model": "TURBO" } }),
    )
    .await
    .unwrap();
    call(
        &wiring,
        "rlm.run",
        json!({ "type": "rlm.run", "prompt": "go", "kwargs": { "model": "p/m" } }),
    )
    .await
    .unwrap();
    let models: Vec<Option<String>> = spawn_requests
        .lock()
        .await
        .iter()
        .map(|request| request.model.clone())
        .collect();
    assert_eq!(
        models,
        vec![
            Some("test-provider/glm-5.3-turbo".to_string()),
            Some("p/m".to_string())
        ]
    );
}
