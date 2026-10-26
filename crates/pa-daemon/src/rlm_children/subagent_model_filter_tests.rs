use super::*;
use crate::protocol::response_failure;
use pa_core::models::{AliasPattern, SubagentModelRefusal};
use pa_types::platform::transport::bind_transport;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// A fake supervisor that records and refuses every command, so a child
/// that slips past the filter fails differently instead of hanging.
async fn spawn_recording_supervisor(
    socket: std::path::PathBuf,
    seen_tx: mpsc::UnboundedSender<String>,
) {
    let listener = bind_transport(&socket).await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok(stream) = listener.accept().await else {
                return;
            };
            let seen_tx = seen_tx.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.split();
                let mut reader = BufReader::new(reader);
                writer
                    .write_all(
                        b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n",
                    )
                    .await
                    .unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    let value: Value = serde_json::from_str(line.trim()).unwrap();
                    let id = value["id"].as_str().unwrap_or_default().to_string();
                    let command_type = value["command"]["type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let _ = seen_tx.send(command_type.clone());
                    let response = response_failure(
                        Some(&id),
                        &command_type,
                        "the recording supervisor refuses every command",
                        None,
                    );
                    let mut line = serde_json::to_string(&response).unwrap();
                    line.push('\n');
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

/// The supervisor host enforces `subagentModels` on real `spawn` and
/// `create_session` calls, for an explicit and an inherited model, before
/// anything reaches the supervisor.
#[tokio::test]
async fn the_supervisor_host_refuses_children_outside_the_subagent_filter() {
    let refused = |permitted: &[&str], alias_patterns| SubagentModelRefusal::OutsideFilter {
        selector: "test-provider/glm-5.3".to_string(),
        permitted: permitted.iter().map(ToString::to_string).collect(),
        alias_patterns,
    };
    let cases = [
        (
            r#"{ "subagentModels": ["test-provider/glm-5.3-turbo"] }"#,
            refused(&["turbo (test-provider/glm-5.3-turbo)"], Vec::new()),
        ),
        // An alias pattern is never expanded: it permits nothing.
        (
            r#"{ "subagentModels": ["turbo"] }"#,
            refused(
                &[],
                vec![AliasPattern {
                    pattern: "turbo".to_string(),
                    selector: "test-provider/glm-5.3-turbo".to_string(),
                }],
            ),
        ),
        (
            r#"{ "subagentModels": "test-provider/glm-5.3" }"#,
            SubagentModelRefusal::UnreadableFilter {
                selector: "test-provider/glm-5.3".to_string(),
                error: "subagentModels is present but is not an array of strings".to_string(),
            },
        ),
    ];
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path();
    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "test-provider": {
                    "baseUrl": "http://localhost:9",
                    "apiKey": "test-key",
                    "api": "openai-completions",
                    "models": [
                        { "id": "glm-5.3", "contextWindow": 1000, "maxTokens": 100 },
                        { "id": "glm-5.3-turbo", "alias": "turbo", "contextWindow": 1000, "maxTokens": 100 }
                    ]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let socket = agent_dir.join("supervisor.sock");
    spawn_recording_supervisor(socket.clone(), seen_tx).await;
    let sessions = SupervisorChildSessions::new(
        Arc::new(crate::supervisor_link::SupervisorLink::new(socket)),
        agent_dir.to_path_buf(),
        "parent-live".to_string(),
        Arc::new(crate::model_allowlist::ModelRefusalTelemetry::new(
            agent_dir.to_path_buf(),
            /*telemetry_disabled*/ true,
        )),
        String::new(),
    );
    sessions.set_identity(ParentIdentity {
        model: Some("test-provider/glm-5.3".to_string()),
        cwd: Some(agent_dir.to_string_lossy().to_string()),
        ..ParentIdentity::with_default_depth()
    });
    for (settings, expected) in cases {
        std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
        for model in [Some("test-provider/glm-5.3"), None] {
            let model = model.map(str::to_string);
            let spawned = sessions
                .spawn(RlmSpawnRequest {
                    prompt: "go".to_string(),
                    name: None,
                    model: model.clone(),
                    thinking: None,
                    spawned_by_request_id: None,
                })
                .await
                .expect_err("the spawn must be refused");
            assert_eq!(
                spawned.downcast_ref::<SubagentModelRefusal>(),
                Some(&expected),
                "{settings} spawn(model={model:?}): {spawned:#}"
            );
            let created = sessions
                .create_session(RlmCreateSessionRequest {
                    prompt: "go".to_string(),
                    name: None,
                    model: model.clone(),
                    thinking: None,
                    cwd: None,
                })
                .await
                .expect_err("create_session must be refused");
            assert_eq!(
                created.downcast_ref::<SubagentModelRefusal>(),
                Some(&expected),
                "{settings} create_session(model={model:?}): {created:#}"
            );
        }
    }
    assert!(
        seen_rx.try_recv().is_err(),
        "a refused child must never reach the supervisor"
    );
}
