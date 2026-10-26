//! Port of the TS regression `6009-opencode-maintenance-session.test.ts`: every request a
//! session makes to the `opencode`/`opencode-go` providers carries the session file's id in
//! `x-opencode-session` and identifies as `prime-agent`: main turns, manual (turn-prefix,
//! split-turn, history-only) and automatic compaction, the auto-refine review, `/refine`,
//! tree branch summaries and side questions. Each split-turn summary has its own idempotency
//! key. Resume, reload and model switches keep the id; `/new`, fork and other sessions don't.
//!
//! Not ported: TS "retains session and request identity when retrying a summary" (Rust has
//! no summary retry), and "forwards refineHarness identity with explicit headers" (the Rust
//! refiner seam carries no request headers; the caller-wins precedence is unit-tested in
//! pa-ai's `opencode_session`).
//!
//! Like opencode-go, the mock refuses a request without the header (`MissingSessionID`
//! 400), so a dropped header also fails the operation.
#![allow(clippy::large_futures)]
// Fn length is a style gate, not correctness.
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SESSION_HEADER: &str = "x-opencode-session";
const PROVIDERS: [&str; 2] = ["opencode", "opencode-go"];
const PROPOSAL: &str =
    r#"{"summary":"No change","rationale":"Fixture","expectedOutcome":"No change","edits":[]}"#;

/// One request the mock received, labelled with the operation the test was driving.
#[derive(Debug, Clone)]
struct Recorded {
    operation: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Default)]
struct MockState {
    operation: String,
    reply: String,
    /// Report a usage that crosses the auto-compaction threshold on the next reply.
    crossing_usage_next: bool,
    requests: Vec<Recorded>,
}

/// An opencode-shaped chat-completions endpoint that records every request.
struct OpencodeMock {
    state: Arc<Mutex<MockState>>,
    port: u16,
}

impl OpencodeMock {
    fn start() -> OpencodeMock {
        let state = Arc::new(Mutex::new(MockState {
            operation: "ordinary".to_string(),
            reply: "OK".to_string(),
            ..MockState::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("mock addr").port();
        let thread_state = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&thread_state);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        OpencodeMock { state, port }
    }

    fn set_operation(&self, operation: &str, reply: &str) {
        let mut state = self.state.lock().expect("mock lock");
        state.operation = operation.to_string();
        state.reply = reply.to_string();
    }

    fn requests_for(&self, operation: &str) -> Vec<Recorded> {
        let state = self.state.lock().expect("mock lock");
        state
            .requests
            .iter()
            .filter(|request| request.operation == operation)
            .cloned()
            .collect()
    }

    /// Wait (bounded) for `count` requests under `operation`: the post-compaction calls
    /// settle after their command's response.
    fn wait_for(&self, operation: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.requests_for(operation).len() < count {
            assert!(Instant::now() < deadline, "no {operation} request arrived");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Exactly `count` requests ran under `operation`, each routed under `session_id` and
    /// identifying as `prime-agent`.
    fn assert_identity(&self, operation: &str, session_id: &str, count: usize) {
        let requests = self.requests_for(operation);
        let identities: Vec<(Option<&str>, Option<&str>)> = requests
            .iter()
            .map(|request| {
                let header = |name: &str| request.headers.get(name).map(String::as_str);
                (header(SESSION_HEADER), header("user-agent"))
            })
            .collect();
        assert_eq!(
            identities,
            vec![(Some(session_id), Some("prime-agent")); count],
            "{operation}: {requests:#?}"
        );
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<MockState>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut headers = BTreeMap::new();
    let mut line = String::new();
    reader.read_line(&mut line)?; // request line
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let (reply, prompt_tokens) = {
        let mut state = state.lock().expect("mock lock");
        let operation = state.operation.clone();
        state.requests.push(Recorded {
            operation,
            headers: headers.clone(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        let crossing = std::mem::take(&mut state.crossing_usage_next);
        (state.reply.clone(), if crossing { 126_000 } else { 10 })
    };
    if !headers.contains_key(SESSION_HEADER) {
        let error = json!({"error": {"type": "invalid_request_error", "message":
            "Error from provider (Console Go): Request is missing x-opencode-session and cannot be routed efficiently."}})
        .to_string();
        let len = error.len();
        return write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {len}\r\n\r\n{error}");
    }
    let usage = json!({
        "prompt_tokens": prompt_tokens, "completion_tokens": 5, "total_tokens": prompt_tokens + 5,
        "prompt_tokens_details": {"cached_tokens": 0},
    });
    let chunk = |delta: Value, finish: Option<&str>| {
        json!({
            "id": "chatcmpl-fixture", "object": "chat.completion.chunk", "created": 1_750_000_000,
            "model": "routing-fixture", "usage": usage,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let text = chunk(json!({"role": "assistant", "content": reply}), None);
    let stop = chunk(json!({}), Some("stop"));
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {text}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One live session's ids as the daemon reports them.
struct Session {
    active_id: String,
    durable_id: String,
    file: PathBuf,
}

/// A supervisor over an agent dir whose only provider is `provider`, served by the mock.
struct Harness {
    provider: &'static str,
    mock: OpencodeMock,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// Every non-response line read while waiting for something else.
    frames: Vec<Value>,
    next_id: u64,
    cwd: PathBuf,
    _supervisor: Supervisor,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start(provider: &'static str, settings: &Value) -> Harness {
        let dir = tempfile::tempdir().expect("temp dir");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(agent_dir.join("sessions")).expect("session dir");
        let mock = OpencodeMock::start();
        let model = |id: &str| {
            json!({
                "id": id, "name": id, "api": "openai-completions", "reasoning": false,
                "contextWindow": 128_000, "maxTokens": 4096,
            })
        };
        let models = json!({"providers": {provider: {
            "api": "openai-completions",
            "baseUrl": format!("http://127.0.0.1:{}/v1", mock.port),
            "apiKey": "fixture-not-a-real-key",
            "models": [model("routing-fixture"), model("routing-fixture-2")],
        }}});
        std::fs::write(agent_dir.join("models.json"), models.to_string()).expect("models.json");
        std::fs::write(agent_dir.join("settings.json"), settings.to_string())
            .expect("settings.json");
        let socket = dir.path().join("daemon.sock");
        #[allow(clippy::zombie_processes)] // reaped by `Supervisor::drop`
        let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
            .arg("supervisor")
            .arg("--socket")
            .arg(&socket)
            .arg("--agent-dir")
            .arg(&agent_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("PRIME_API_KEY")
            .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
            .env_remove("OPENCODE_API_KEY")
            .env(
                pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
                "15000",
            )
            .spawn()
            .expect("spawn pa-daemon supervisor");
        let supervisor = Supervisor(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "supervisor socket never appeared"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let stream = UnixStream::connect(&socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        let mut harness = Harness {
            provider,
            mock,
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
            frames: Vec::new(),
            next_id: 0,
            cwd: dir.path().to_path_buf(),
            _supervisor: supervisor,
            _dir: dir,
        };
        assert_eq!(harness.read_line()["type"], "daemon_hello");
        harness
    }

    fn read_line(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_mins(1);
        let mut line = String::new();
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
                Err(error) => assert!(Instant::now() < deadline, "no supervisor line: {error}"),
            }
        }
    }

    /// Send `command` and return its successful response's `data`.
    fn request(&mut self, command: &Value) -> Value {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        let envelope = json!({
            "type": "command", "id": id, "command": command,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
        });
        writeln!(self.writer, "{envelope}").expect("send");
        loop {
            let frame = self.read_line();
            if frame["id"] == id.as_str() {
                assert_eq!(frame["success"], true, "{command} failed: {frame}");
                return frame["data"].clone();
            }
            self.frames.push(frame);
        }
    }

    /// Wait for a frame matching `matches` (already-buffered frames count).
    fn wait_for_frame(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.frames.iter().position(&matches) {
            return self.frames.remove(index);
        }
        loop {
            let frame = self.read_line();
            if matches(&frame) {
                return frame;
            }
            self.frames.push(frame);
        }
    }

    /// Send `{"type": kind, "activeSessionId": ..., ...fields}`.
    fn command(&mut self, kind: &str, session: &Session, fields: &Value) -> Value {
        let mut command = json!({"type": kind, "activeSessionId": session.active_id});
        if let Some(fields) = fields.as_object() {
            command.as_object_mut().unwrap().extend(fields.clone());
        }
        self.request(&command)
    }

    fn create(&mut self, session_path: Option<&Path>) -> Session {
        let mut create = json!({"type": "create", "config": {
            "cwd": self.cwd.to_string_lossy(),
            "sessionDir": self.cwd.join("agent/sessions").to_string_lossy(),
            "provider": self.provider, "model": "routing-fixture",
        }});
        if let Some(path) = session_path {
            create["sessionPath"] = json!(path.to_string_lossy());
        }
        let created = self.request(&create);
        let active_id = created["id"].as_str().expect("active id").to_string();
        let session = session_of(active_id, &created);
        self.command("attach", &session, &json!({}));
        session
    }

    /// The live session's ids after a replacement inside the same worker.
    fn replaced(&mut self, previous: &Session) -> Session {
        let state = self.command("get_state", previous, &json!({}));
        session_of(previous.active_id.clone(), &state)
    }

    /// A main turn: exactly one request, routed under the session's durable id.
    fn ordinary(&mut self, session: &Session, text: &str) {
        self.mock.set_operation("ordinary", "OK");
        let before = self.mock.requests_for("ordinary").len();
        // `followUp` resumes the queued-input suspension a manual compaction leaves
        // (TS `_resumeSessionInputAdmission`); on an idle session it is a plain prompt.
        self.command(
            "prompt_and_wait",
            session,
            &json!({"message": text, "streamingBehavior": "followUp"}),
        );
        let turn: Vec<Option<String>> = self.mock.requests_for("ordinary")[before..]
            .iter()
            .map(|request| request.headers.get(SESSION_HEADER).cloned())
            .collect();
        assert_eq!(
            turn,
            vec![Some(session.durable_id.clone())],
            "{}",
            self.provider
        );
    }

    /// A manual compaction under `operation`, asserted to route `count` requests.
    fn compact(&mut self, session: &Session, operation: &str, count: usize) -> Value {
        self.mock.set_operation(operation, "OK");
        let compacted = self.command("compact", session, &json!({}));
        assert!(
            compacted.get("summary").is_some() || compacted.get("result").is_some(),
            "{operation} ran ({}): {compacted}",
            self.provider
        );
        self.mock
            .assert_identity(operation, &session.durable_id, count);
        compacted
    }
}

fn session_of(active_id: String, summary: &Value) -> Session {
    Session {
        active_id,
        durable_id: summary["sessionId"]
            .as_str()
            .expect("session id")
            .to_string(),
        file: PathBuf::from(summary["sessionFile"].as_str().expect("session file")),
    }
}

/// The first assistant message entry id in the session file.
fn first_assistant_entry(session: &Session) -> String {
    std::fs::read_to_string(&session.file)
        .expect("read session file")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
        .and_then(|entry| entry["id"].as_str().map(str::to_string))
        .expect("an assistant entry")
}

fn base_settings() -> Value {
    json!({
        "compaction": {"enabled": false, "keepRecentTokens": 1},
        "autoRefine": {"enabled": false},
        "retry": {"enabled": true, "maxRetries": 1, "baseDelayMs": 1},
    })
}

#[test]
fn every_maintenance_call_routes_under_the_owning_session() {
    for provider in PROVIDERS {
        let mut h = Harness::start(provider, &base_settings());
        let session = h.create(None);
        let id = session.durable_id.clone();
        h.ordinary(&session, "remember this request");

        // One turn, so the cut splits it: a turn-prefix compaction.
        h.compact(&session, "turn-prefix", 1);

        // History plus the turn prefix, each slice with its own idempotency key.
        h.ordinary(&session, "second request");
        h.ordinary(&session, "third request");
        h.compact(&session, "split-compaction", 2);
        let keys: BTreeSet<Option<String>> = h
            .mock
            .requests_for("split-compaction")
            .iter()
            .map(|request| request.headers.get("idempotency-key").cloned())
            .collect();
        assert!(keys.len() == 2 && !keys.contains(&None), "{keys:?}");

        h.mock.set_operation("refinement-plan", PROPOSAL);
        h.command("refine", &session, &json!({}));
        h.mock.assert_identity("refinement-plan", &id, 1);

        let target = first_assistant_entry(&session);
        h.ordinary(&session, "explore a branch");
        h.mock.set_operation("branch-summary", "OK");
        let navigated = h.command(
            "navigate_tree",
            &session,
            &json!({"targetId": target, "summarize": true}),
        );
        assert_eq!(navigated["cancelled"], false, "{navigated}");
        h.mock.assert_identity("branch-summary", &id, 1);

        h.mock.set_operation("side-question", "OK");
        h.command(
            "start_side_question",
            &session,
            &json!({"sideQuestionId": "sq-1", "question": "what did I ask?"}),
        );
        let settled = h.wait_for_frame(|frame| {
            frame["type"] == "side_question_event" && frame["event"]["status"] != "running"
        });
        assert_eq!(settled["event"]["status"], "complete", "{settled}");
        h.mock.assert_identity("side-question", &id, 1);

        // A model switch and a reload keep the session's id.
        h.command(
            "set_model",
            &session,
            &json!({"provider": provider, "modelId": "routing-fixture-2"}),
        );
        h.ordinary(&session, "after the model switch");
        h.command("reload", &session, &json!({}));
        h.ordinary(&session, "after the reload");

        // A user bash row after the turn is a turn start, so the cut lands on it and only
        // the history is summarized.
        let history = h.create(None);
        h.ordinary(&history, "remember this request");
        h.command(
            "execute_bash_and_wait",
            &history,
            &json!({"command": "echo retained"}),
        );
        let compacted = h.compact(&history, "history", 1);
        assert_eq!(compacted["summary"], "OK", "{compacted}");
    }
}

#[test]
fn automatic_compaction_and_the_auto_refine_review_route_under_the_owning_session() {
    let settings = json!({
        // A tiny reserve: the crossing usage below fires the threshold compaction.
        "compaction": {"enabled": true, "reserveTokens": 500, "keepRecentTokens": 1},
        "autoRefine": {"enabled": true},
        "retry": {"enabled": true, "maxRetries": 1, "baseDelayMs": 1},
    });
    for provider in PROVIDERS {
        let mut h = Harness::start(provider, &settings);
        let session = h.create(None);
        h.ordinary(&session, "remember this request");

        h.mock.state.lock().unwrap().crossing_usage_next = true;
        h.mock.set_operation("automatic-compaction", "OK");
        h.command(
            "prompt_and_wait",
            &session,
            &json!({"message": "crossing request"}),
        );
        h.wait_for_frame(|frame| {
            frame["type"] == "session_event" && frame["event"]["type"] == "compaction_end"
        });
        // The crossing turn plus both split-turn summaries.
        h.mock.wait_for("automatic-compaction", 3);
        h.mock
            .assert_identity("automatic-compaction", &session.durable_id, 3);

        // The compaction armed the compact-trigger review; the next prompt's admission
        // runs it (declined) ahead of its own turn, under the same operation label.
        h.mock.set_operation(
            "refinement-review",
            r#"{"shouldRefine":false,"rationale":"No lesson"}"#,
        );
        h.command(
            "prompt_and_wait",
            &session,
            &json!({"message": "another request", "streamingBehavior": "followUp"}),
        );
        let review = h.mock.requests_for("refinement-review");
        assert!(
            review
                .iter()
                .any(|request| request.body.to_string().contains("<trigger>")),
            "{review:#?}"
        );
        h.mock
            .assert_identity("refinement-review", &session.durable_id, 2);
    }
}

#[test]
fn resume_keeps_and_replacement_changes_the_routing_identity() {
    for provider in PROVIDERS {
        let mut h = Harness::start(provider, &base_settings());
        let original = h.create(None);
        h.ordinary(&original, "remember this request");
        h.compact(&original, "first-compaction", 1);

        // `/new` replaces the session inside the same worker.
        let replaced = h.command("new_session", &original, &json!({}));
        assert_eq!(replaced["cancelled"], false, "{replaced}");
        let replacement = h.replaced(&original);
        assert_ne!(replacement.durable_id, original.durable_id);
        h.ordinary(&replacement, "a fresh conversation");
        h.compact(&replacement, "replacement-compaction", 1);

        // Switching back to the original file restores its id.
        h.command(
            "switch_session",
            &original,
            &json!({"sessionPath": original.file.to_string_lossy()}),
        );
        h.ordinary(&original, "back on the original");

        // A fork is a replacement too; its copied history plus the new turn split.
        let fork_point = first_assistant_entry(&original);
        let forked = h.command(
            "fork",
            &original,
            &json!({"entryId": fork_point, "position": "at"}),
        );
        assert_eq!(forked["cancelled"], false, "{forked}");
        let fork = h.replaced(&original);
        assert_ne!(fork.durable_id, original.durable_id);
        assert_ne!(fork.durable_id, replacement.durable_id);
        h.ordinary(&fork, "continue on the fork");
        h.compact(&fork, "fork-compaction", 2);

        let other = h.create(None);
        assert_ne!(other.durable_id, original.durable_id);
        h.ordinary(&other, "an unrelated session");
        h.compact(&other, "other-compaction", 1);

        // Resume: a new worker over the original file keeps its id.
        h.command("kill", &original, &json!({}));
        let resumed = h.create(Some(&original.file));
        assert_eq!(resumed.durable_id, original.durable_id);
        h.ordinary(&resumed, "continue resumed session");
        h.compact(&resumed, "resumed-compaction", 1);
        h.mock.set_operation("resumed-refinement", PROPOSAL);
        h.command("refine", &resumed, &json!({}));
        h.mock
            .assert_identity("resumed-refinement", &original.durable_id, 1);
    }
}
