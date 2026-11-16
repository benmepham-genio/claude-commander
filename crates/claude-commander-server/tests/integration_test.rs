//! End-to-end integration tests for `claude-commander-server`.
//!
//! These boot the real router on an ephemeral loopback port and drive it with
//! real HTTP and WebSocket clients, through real tmux + git. They require tmux
//! and use a **runtime** `tmux_available()` self-skip (an early `return`), *not*
//! `#[ignore]` — `#[ignore]` would make `cargo test` skip them everywhere, so
//! they'd never execute in CI. With the runtime check, `cargo test --workspace`
//! in the Nix dev shell (tmux present) actually runs them; on a tmux-less box
//! they self-skip.
//!
//! The hermetic-server + git/tmux fixtures live in the shared
//! `claude-commander-test-support` crate (also used by the Flutter cdylib's
//! `client/rust/tests`). All disk access goes through `tempfile::TempDir`;
//! nothing touches the real filesystem. The listener binds `127.0.0.1:0` so the
//! OS assigns a free port.

use std::time::Duration;

use claude_commander_core::tmux::TmuxExecutor;
use claude_commander_test_support::{create_test_repo, spawn_server, test_state, tmux_available};
use futures::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

/// HTTP round-trip through real tmux + git: register a project, create a
/// session, then list/detail/pane/kill it over the wire.
#[tokio::test]
async fn http_session_lifecycle_round_trip() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    // Hold a service clone so we can clean tmux up at the end regardless of the
    // HTTP path taken.
    let service = state.service.clone();
    let addr = spawn_server(state).await;
    let base = format!("http://{addr}/api");
    let client = reqwest::Client::new();

    // -- register the project --
    let resp = client
        .post(format!("{base}/projects"))
        .json(&serde_json::json!({ "path": repo_path }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "POST /projects should succeed, got {}",
        resp.status()
    );

    // -- create a session -> 201 { id } --
    let resp = client
        .post(format!("{base}/sessions"))
        .json(&serde_json::json!({
            "project_path": repo_path,
            "title": "http-roundtrip",
            "program": "bash",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "POST /sessions should be 201"
    );
    let created: serde_json::Value = resp.json().await.unwrap();
    let id = created["id"]
        .as_str()
        .expect("id should be a string")
        .to_string();

    // -- GET /sessions shows it --
    let resp = client.get(format!("{base}/sessions")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let sessions: serde_json::Value = resp.json().await.unwrap();
    let arr = sessions.as_array().expect("sessions should be an array");
    assert!(
        arr.iter().any(|s| s["id"] == serde_json::json!(id)),
        "created session {id} should appear in the list: {sessions}"
    );

    // -- GET /sessions/{q}/detail -> 200 --
    // Query with the FULL UUID (exactly what `POST /sessions` returned), proving
    // detail/pane resolve the full id a real client holds (B1).
    let resp = client
        .get(format!("{base}/sessions/{id}/detail"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "detail should be 200");

    // -- GET /sessions/{q}/pane -> 200 --
    let resp = client
        .get(format!("{base}/sessions/{id}/pane"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "pane should be 200");

    // -- POST /sessions/{id}/kill -> 204 --
    let resp = client
        .post(format!("{base}/sessions/{id}/kill"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NO_CONTENT,
        "kill should be 204"
    );

    // -- cleanup: ensure the tmux session is gone --
    let id =
        claude_commander_core::session::SessionId::from_uuid(uuid::Uuid::parse_str(&id).unwrap());
    let _ = service.kill_session(&id).await;

    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// WebSocket attach: handshake (`auth` → `attach` → `resize`), assert `ready`,
/// send a keystroke as a BINARY frame, assert PTY output arrives as binary
/// frames, then `detach` and assert the tmux session **still exists** (detach ≠
/// kill). Finally kill it to clean up.
#[tokio::test]
async fn ws_attach_streams_and_detach_keeps_session_alive() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;

    // Register project + create a session directly through the service (the HTTP
    // path is covered by the other test; here we focus on the WS contract).
    service.add_project(repo_path.clone(), None).await.unwrap();
    let session_id = service
        .create_session(claude_commander_core::api::CreateSessionOpts {
            project_path: repo_path.clone(),
            title: "ws-attach".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        })
        .await
        .unwrap();

    // The tmux session name we expect to survive a detach.
    let tmux_name = service
        .resolve_tmux_session(&session_id.to_string())
        .await
        .unwrap()
        .expect("session should resolve to a tmux name");

    // -- open the WebSocket --
    let url = format!("ws://{addr}/ws/attach");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();

    // -- handshake: auth (token is ignored under AuthConfig::Disabled, but the
    //    handler still expects an auth frame first), attach, resize --
    ws.send(Message::Text(
        r#"{"type":"auth","token":"unused"}"#.to_string().into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        serde_json::json!({"type":"attach","session_id": session_id.to_string()})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        r#"{"type":"resize","cols":100,"rows":30}"#.to_string().into(),
    ))
    .await
    .unwrap();

    // -- assert a `ready` server control frame (text/JSON) arrives --
    let ready = next_text_frame(&mut ws)
        .await
        .expect("should receive a control frame after handshake");
    let parsed: serde_json::Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(
        parsed["type"], "ready",
        "first control frame should be `ready`, got: {ready}"
    );

    // -- type a keystroke (BINARY frame) and assert PTY output arrives as
    //    BINARY frames. A bare newline makes bash echo a prompt line. --
    ws.send(Message::Binary(b"echo cc_ws_marker\n".to_vec().into()))
        .await
        .unwrap();

    let got_binary = wait_for_binary_output(&mut ws, Duration::from_secs(5)).await;
    assert!(
        got_binary,
        "should receive PTY output as a binary frame after typing"
    );

    // -- detach: should leave the tmux session running --
    ws.send(Message::Text(r#"{"type":"detach"}"#.to_string().into()))
        .await
        .unwrap();
    // Drain until the socket closes so the server completes its teardown
    // (kills only the `tmux attach-session` child, not the session).
    drain_until_close(&mut ws, Duration::from_secs(5)).await;

    // -- the tmux session must STILL EXIST (detach ≠ kill) --
    // Pin this probe onto the same isolated socket dir the harness put the
    // session on (via `tmux_tmpdir`), or it would query the developer's real
    // tmux server and never see the session.
    let tmux = TmuxExecutor::new().with_tmux_tmpdir(service.read_config().tmux_tmpdir);
    let mut exists = false;
    for _ in 0..50 {
        if tmux.session_exists(&tmux_name).await.unwrap_or(false) {
            exists = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        exists,
        "tmux session {tmux_name} must survive a WS detach (detach is not a kill)"
    );

    // -- now actually kill it to clean up --
    service.kill_session(&session_id).await.unwrap();

    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// An empty `session_id` must be rejected with `WS_ERR_NO_SESSION`, not
/// resolved. `""` prefixes every session ID, so before the lookup rejected
/// blank queries this attached to an arbitrary live session.
#[tokio::test]
async fn ws_attach_with_empty_session_id_is_rejected() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;

    // A live session the empty query could otherwise have matched.
    service.add_project(repo_path.clone(), None).await.unwrap();
    let session_id = service
        .create_session(claude_commander_core::api::CreateSessionOpts {
            project_path: repo_path.clone(),
            title: "ws-empty-id".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        })
        .await
        .unwrap();

    let url = format!("ws://{addr}/ws/attach");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Text(
        r#"{"type":"auth","token":"unused"}"#.to_string().into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        r#"{"type":"attach","session_id":""}"#.to_string().into(),
    ))
    .await
    .unwrap();

    let frame = next_text_frame(&mut ws)
        .await
        .expect("should receive a control frame after attach");
    let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(
        parsed["type"], "error",
        "an empty session_id must not attach, got: {frame}"
    );
    assert_eq!(
        parsed["message"],
        claude_commander_protocol::ws::WS_ERR_NO_SESSION,
        "an empty session_id must report no-such-session, got: {frame}"
    );

    service.kill_session(&session_id).await.unwrap();
    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// WS agent-attach must revive a session whose tmux died server-side (parity
/// with `LocalBackend::attach`, which routes through `ensure_attachable`). Kill
/// the tmux session out from under the server, then WS-attach to the agent pane:
/// the handshake must still reach `ready` and the tmux session must be alive
/// again. Against a handler that resolves the pane with a bare store lookup this
/// is red — the attach spawns against a dead session name and never readies.
#[tokio::test]
async fn ws_agent_attach_revives_dead_tmux_session() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;

    service.add_project(repo_path.clone(), None).await.unwrap();
    let session_id = service
        .create_session(claude_commander_core::api::CreateSessionOpts {
            project_path: repo_path.clone(),
            title: "ws-revive".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        })
        .await
        .unwrap();

    let tmux_name = service
        .resolve_tmux_session(&session_id.to_string())
        .await
        .unwrap()
        .expect("session should resolve to a tmux name");

    // Kill the tmux session out from under the server (pin the probe to the
    // isolated socket dir the session lives on).
    let tmux = TmuxExecutor::new().with_tmux_tmpdir(service.read_config().tmux_tmpdir);
    tmux.kill_session(&tmux_name).await.unwrap();
    assert!(
        !tmux.session_exists(&tmux_name).await.unwrap(),
        "precondition: tmux session should be dead before the attach"
    );

    // WS-attach to the AGENT pane; the handshake must still reach `ready`.
    let url = format!("ws://{addr}/ws/attach");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Text(
        r#"{"type":"auth","token":"unused"}"#.to_string().into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        serde_json::json!({"type":"attach","session_id": session_id.to_string()})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let ready = next_text_frame(&mut ws)
        .await
        .expect("should receive a control frame after handshake");
    let parsed: serde_json::Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(
        parsed["type"], "ready",
        "agent attach to a dead session must revive it and reach `ready`, got: {ready}"
    );

    // The tmux session must be back.
    let mut revived = false;
    for _ in 0..50 {
        if tmux.session_exists(&tmux_name).await.unwrap_or(false) {
            revived = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        revived,
        "agent attach must recreate the dead tmux session {tmux_name}"
    );

    ws.send(Message::Text(r#"{"type":"detach"}"#.to_string().into()))
        .await
        .unwrap();
    drain_until_close(&mut ws, Duration::from_secs(5)).await;
    service.kill_session(&session_id).await.unwrap();

    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// The `attach` frame's `cols`/`rows` must size the PTY *before*
/// `tmux attach-session` is spawned — proven here by never sending a `resize`
/// frame at all, so the only way tmux can end up at the requested size is the
/// handshake.
///
/// Why it has to happen this early: tmux paints a full screen into the socket
/// the moment the attach starts, and every later repaint is incremental with no
/// full-screen clear. A client that could only announce its size afterwards
/// therefore always received one paint at the server's fallback 80x24, wrapped
/// it at its own (narrower) width, and had no way back — the pane stayed
/// desynchronised for the life of the attach, and reconnecting merely replayed
/// it. Red against a handler that spawns at `DEFAULT_COLS`/`DEFAULT_ROWS` and
/// waits for a `resize`.
#[tokio::test]
async fn ws_attach_handshake_size_reaches_tmux_without_any_resize() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;

    service.add_project(repo_path.clone(), None).await.unwrap();
    let session_id = service
        .create_session(claude_commander_core::api::CreateSessionOpts {
            project_path: repo_path.clone(),
            title: "ws-attach-size".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        })
        .await
        .unwrap();
    let tmux_name = service
        .resolve_tmux_session(&session_id.to_string())
        .await
        .unwrap()
        .expect("session should resolve to a tmux name");

    // A phone-shaped viewport, deliberately nothing like the 80x24 fallback.
    const COLS: u16 = 39;
    const ROWS: u16 = 40;

    let url = format!("ws://{addr}/ws/attach");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Text(
        r#"{"type":"auth","token":"unused"}"#.to_string().into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        serde_json::json!({
            "type": "attach",
            "session_id": session_id.to_string(),
            "cols": COLS,
            "rows": ROWS,
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let ready = next_text_frame(&mut ws)
        .await
        .expect("should receive a control frame after handshake");
    assert!(ready.contains("ready"), "expected `ready`, got: {ready}");

    // Poll: the size is set on the PTY before the child is spawned, but tmux
    // still has to start and register the client.
    let tmux = TmuxExecutor::new().with_tmux_tmpdir(service.read_config().tmux_tmpdir);
    let mut seen = String::new();
    let mut matched = false;
    for _ in 0..60 {
        seen = tmux
            .execute(&[
                "list-clients",
                "-t",
                &tmux_name,
                "-F",
                "#{client_width}x#{client_height}",
            ])
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        if seen == format!("{COLS}x{ROWS}") {
            matched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        matched,
        "the attach frame's {COLS}x{ROWS} must size the PTY with no `resize` \
         frame ever sent; tmux reported the client as {seen:?}"
    );

    ws.send(Message::Text(r#"{"type":"detach"}"#.to_string().into()))
        .await
        .unwrap();
    drain_until_close(&mut ws, Duration::from_secs(5)).await;
    service.kill_session(&session_id).await.unwrap();

    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// A WS attach must stamp `last_attached_at` (server-side MRU), matching
/// `LocalBackend::attach`'s `mark_attached`. Freshly created it is `None`; after
/// a successful WS attach it must be `Some`. Red against a handler that resolves
/// the pane without stamping.
#[tokio::test]
async fn ws_attach_stamps_last_attached_at() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;

    service.add_project(repo_path.clone(), None).await.unwrap();
    let session_id = service
        .create_session(claude_commander_core::api::CreateSessionOpts {
            project_path: repo_path.clone(),
            title: "ws-mru".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        })
        .await
        .unwrap();

    assert!(
        service
            .store()
            .read()
            .await
            .get_session(&session_id)
            .unwrap()
            .last_attached_at
            .is_none(),
        "precondition: a freshly created session has no last_attached_at"
    );

    let url = format!("ws://{addr}/ws/attach");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Text(
        r#"{"type":"auth","token":"unused"}"#.to_string().into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        serde_json::json!({"type":"attach","session_id": session_id.to_string()})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let ready = next_text_frame(&mut ws)
        .await
        .expect("should receive a control frame after handshake");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ready).unwrap()["type"],
        "ready"
    );

    assert!(
        service
            .store()
            .read()
            .await
            .get_session(&session_id)
            .unwrap()
            .last_attached_at
            .is_some(),
        "a WS attach must stamp last_attached_at (MRU parity with LocalBackend)"
    );

    ws.send(Message::Text(r#"{"type":"detach"}"#.to_string().into()))
        .await
        .unwrap();
    drain_until_close(&mut ws, Duration::from_secs(5)).await;
    service.kill_session(&session_id).await.unwrap();

    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// `POST /sessions/{id}/paste-image` end to end: a real session, a real upload,
/// and the written file plus the path injected into the pane.
///
/// The handler's own tests cover only the reject paths (400/404), which never
/// touch disk or tmux; this is the happy path, so it needs the real fixture.
/// Writes land under the harness's `agent_temp_dir` (inside `data_dir`), never
/// the real OS temp dir.
#[tokio::test]
async fn http_paste_image_writes_file_and_injects_path() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    // A minimal but valid 1×1 PNG — the allow-list sniffs content, so this is
    // the smallest thing the route will accept.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;
    let base = format!("http://{addr}/api");
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/projects"))
        .json(&serde_json::json!({ "path": repo_path }))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base}/sessions"))
        .json(&serde_json::json!({
            "project_path": repo_path,
            "title": "paste-image",
            "program": "bash",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let created: serde_json::Value = resp.json().await.unwrap();
    let id = created["id"].as_str().expect("id").to_string();

    // -- upload the image --
    let resp = client
        .post(format!("{base}/sessions/{id}/paste-image"))
        .header("content-type", "image/png")
        .body(TINY_PNG.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a valid image upload should be 200");
    let body: serde_json::Value = resp.json().await.unwrap();
    let written = body["path"].as_str().expect("path should be a string");

    // The server writes the bytes verbatim under its own generated name, with
    // the extension sniffed from the content (never a client-supplied one).
    assert!(
        written.ends_with(".png"),
        "extension should come from the magic bytes: {written}"
    );
    assert!(
        std::path::Path::new(written).starts_with(data_dir.path()),
        "test writes must stay inside the temp data dir, got {written}"
    );
    assert_eq!(
        tokio::fs::read(written).await.unwrap(),
        TINY_PNG,
        "stored file should be byte-identical to the upload"
    );

    // -- the path is typed into the pane (send-keys -l, no Enter) --
    let session_id =
        claude_commander_core::session::SessionId::from_uuid(uuid::Uuid::parse_str(&id).unwrap());
    let file_name = std::path::Path::new(written)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let mut injected = false;
    for _ in 0..20 {
        if let Ok(Some(content)) = service.get_pane_content(&id, None).await
            && content.contains(&file_name)
        {
            injected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        injected,
        "the stored image path ({file_name}) should be injected into the pane"
    );

    // -- a non-image is refused for an existing session too --
    let resp = client
        .post(format!("{base}/sessions/{id}/paste-image"))
        .body(b"not an image".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a non-image body should be 400 even for a live session"
    );

    service.kill_session(&session_id).await.unwrap();
    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// Two viewers of one session — a laptop's remote TUI and the host's — is how
/// the agent kept getting fresh-restarted. The second attach's `-d` detaches
/// the first's tmux client; the pump saw its PTY close and reported
/// `session_ended`, which a TUI answers by fresh-restarting the agent, killing
/// it under the viewer who just opened it. The session is alive, so the first
/// viewer must be told it was detached, never that the session ended.
#[tokio::test]
async fn ws_attach_displaced_by_a_second_viewer_is_not_session_ended() {
    if !tmux_available().await {
        eprintln!("Skipping test: tmux not available");
        return;
    }

    let (repo_temp_dir, repo_path) = create_test_repo().await;
    let data_dir = TempDir::new().unwrap();
    let worktrees_dir = TempDir::new().unwrap();
    let state = test_state(&data_dir, &worktrees_dir);
    let service = state.service.clone();
    let addr = spawn_server(state).await;

    service.add_project(repo_path.clone(), None).await.unwrap();
    let session_id = service
        .create_session(claude_commander_core::api::CreateSessionOpts {
            project_path: repo_path.clone(),
            title: "two-viewers".to_string(),
            program: Some("bash".to_string()),
            initial_prompt: None,
            effort: None,
            mode: None,
            model: None,
            base_branch: None,
            section: None,
            stack_parent: None,
        })
        .await
        .unwrap();

    let mut first = open_attach(addr, &session_id.to_string()).await;
    assert!(wait_for_binary_output(&mut first, Duration::from_secs(5)).await);
    let mut second = open_attach(addr, &session_id.to_string()).await;

    let frame = next_text_frame(&mut first)
        .await
        .expect("the displaced viewer should be told why its attach ended");
    let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(parsed["type"], "detached", "got: {frame}");
    assert_eq!(
        parsed["reason"], "client_detached",
        "a displaced viewer's session is still alive: {frame}"
    );

    second
        .send(Message::Text(r#"{"type":"detach"}"#.to_string().into()))
        .await
        .unwrap();
    drain_until_close(&mut second, Duration::from_secs(5)).await;
    service.kill_session(&session_id).await.unwrap();

    drop(repo_temp_dir);
    drop(data_dir);
    drop(worktrees_dir);
}

/// Open `/ws/attach`, complete the handshake for `session_id`, and wait for the
/// `ready` frame.
async fn open_attach(
    addr: std::net::SocketAddr,
    session_id: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/attach"))
        .await
        .unwrap();
    for frame in [
        r#"{"type":"auth","token":"unused"}"#.to_string(),
        serde_json::json!({"type":"attach","session_id": session_id}).to_string(),
    ] {
        ws.send(Message::Text(frame.into())).await.unwrap();
    }
    let ready = next_text_frame(&mut ws).await.expect("ready frame");
    assert!(ready.contains(r#""ready""#), "expected ready, got: {ready}");
    ws
}

/// Receive frames until the next TEXT frame, returning its payload. `None` on
/// close/error/timeout.
async fn next_text_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<String> {
    let fut = async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Text(t)) => return Some(t.to_string()),
                Ok(Message::Close(_)) | Err(_) => return None,
                // skip binary / ping / pong while waiting for a control frame
                Ok(_) => continue,
            }
        }
        None
    };
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .ok()
        .flatten()
}

/// Wait up to `timeout` for at least one non-empty BINARY frame (PTY output).
async fn wait_for_binary_output(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    timeout: Duration,
) -> bool {
    let fut = async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Binary(b)) if !b.is_empty() => return true,
                Ok(Message::Close(_)) | Err(_) => return false,
                Ok(_) => continue,
            }
        }
        false
    };
    tokio::time::timeout(timeout, fut).await.unwrap_or(false)
}

/// Drain frames until the socket closes (or `timeout` elapses).
async fn drain_until_close(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    timeout: Duration,
) {
    let fut = async {
        while let Some(msg) = ws.next().await {
            if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(timeout, fut).await;
}
