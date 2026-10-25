//! The subagent-model policy at the RLM bridge: the `subagentModels`
//! filter on `rlm.find_models`, alias resolution on spawns, and the policy
//! counters.
use super::*;

/// `subagentModels` (global settings, canonical selectors) narrows
/// `rlm.find_models` to exactly the filtered set, the aliased model
/// carrying its alias; an alias as a pattern matches nothing
/// (patterns match canonical names only).
#[tokio::test]
async fn find_models_returns_only_the_subagent_filter() {
    let dir = tempfile::TempDir::new().unwrap();
    let wiring = wired(dir.path(), None);
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "subagentModels": ["test-provider/glm-5.3-turbo", "test-provider/glm-5.3"] }"#,
    )
    .unwrap();
    let response = call(
        &wiring,
        "rlm.find_models",
        json!({ "type": "rlm.find_models", "query": "" }),
    )
    .await
    .unwrap();
    assert_eq!(
        response,
        json!({ "models": [
            {
                "provider": "test-provider",
                "id": "glm-5.3",
                "name": "GLM 5.3",
                "selector": "test-provider/glm-5.3",
            },
            {
                "provider": "test-provider",
                "id": "glm-5.3-turbo",
                "name": "GLM Turbo",
                "selector": "test-provider/glm-5.3-turbo",
                "alias": "turbo",
            },
        ] })
    );
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "subagentModels": ["*turbo"] }"#,
    )
    .unwrap();
    let response = call(
        &wiring,
        "rlm.find_models",
        json!({ "type": "rlm.find_models", "query": "glm" }),
    )
    .await
    .unwrap();
    assert_eq!(
        response,
        json!({ "models": [{
            "provider": "test-provider",
            "id": "glm-5.3-turbo",
            "name": "GLM Turbo",
            "selector": "test-provider/glm-5.3-turbo",
            "alias": "turbo",
        }] })
    );
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "subagentModels": ["turbo"] }"#,
    )
    .unwrap();
    let response = call(
        &wiring,
        "rlm.find_models",
        json!({ "type": "rlm.find_models", "query": "" }),
    )
    .await
    .unwrap();
    assert_eq!(response, json!({ "models": [] }));
}

/// The display name stays searchable beside the alias and selector.
#[tokio::test]
async fn find_models_matches_the_display_name_and_the_alias() {
    let dir = tempfile::TempDir::new().unwrap();
    let wiring = wired(dir.path(), None);
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "subagentModels": ["test-provider/*"] }"#,
    )
    .unwrap();
    let turbo = json!({ "models": [{
        "provider": "test-provider",
        "id": "glm-5.3-turbo",
        "name": "GLM Turbo",
        "selector": "test-provider/glm-5.3-turbo",
        "alias": "turbo",
    }] });
    for query in ["GLM Turbo", "turbo"] {
        let response = call(
            &wiring,
            "rlm.find_models",
            json!({ "type": "rlm.find_models", "query": query }),
        )
        .await
        .unwrap();
        assert_eq!(response, turbo, "query {query:?}");
    }
}

/// The bridge's policy counters: a filtered `rlm.find_models`, a
/// `subagentModels` refusal from the host, and an alias spawn the host
/// admitted; an alias spawn the host refused counts as a refusal only.
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

/// A malformed `subagentModels` (a string, not a list) fails closed:
/// `rlm.find_models` errors naming the problem instead of listing every
/// model as the lenient settings load would.
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

/// `enabledModels` never filters subagents: with it set and
/// `subagentModels` unset, `rlm.find_models` lists every searchable model.
#[tokio::test]
async fn enabled_models_never_filter_find_models() {
    let dir = tempfile::TempDir::new().unwrap();
    let wiring = wired(dir.path(), None);
    let query = json!({ "type": "rlm.find_models", "query": "test-provider/" });
    let unfiltered = call(&wiring, "rlm.find_models", query.clone())
        .await
        .unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{ "enabledModels": ["test-provider/glm-5.3"] }"#,
    )
    .unwrap();
    let with_enabled = call(&wiring, "rlm.find_models", query).await.unwrap();
    assert_eq!(with_enabled, unfiltered);
    assert_eq!(with_enabled["models"].as_array().unwrap().len(), 2);
}

/// A models.json alias in `rlm.spawn(model=...)` and
/// `rlm.create_session(model=...)` reaches the host as its selector.
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
