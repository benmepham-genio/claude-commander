//! Loopback-only fixture for request-count and change-feed integration tests.
use crate::{RemoteClient, RemoteServerSpec};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::watch;

/// A client wired to an ad-hoc `app` on a loopback port; the server task is
/// aborted when this is dropped.
pub struct Served {
    pub client: RemoteClient,
    pub addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub async fn serve(app: Router) -> Served {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = RemoteClient::new(RemoteServerSpec {
        name: "test".into(),
        base_url: format!("http://{addr}"),
        token: None,
    })
    .unwrap();
    Served { client, addr, task }
}

#[derive(Clone)]
pub struct Data {
    pub generation: watch::Sender<u64>,
    pub waits: watch::Sender<usize>,
    pub workspace: Arc<AtomicUsize>,
    pub agents: Arc<AtomicUsize>,
    pub fail_wait: Arc<AtomicBool>,
    pub fail_workspace: Arc<AtomicBool>,
    pub supported: bool,
}
pub struct Server {
    pub client: Arc<RemoteClient>,
    pub data: Data,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(supported: bool) -> Self {
        let data = Data {
            generation: watch::channel(0).0,
            waits: watch::channel(0).0,
            workspace: Arc::new(AtomicUsize::new(0)),
            agents: Arc::new(AtomicUsize::new(0)),
            fail_wait: Arc::new(AtomicBool::new(false)),
            fail_workspace: Arc::new(AtomicBool::new(false)),
            supported,
        };
        let app = Router::new()
            .route("/api/changes", get(changes))
            .route("/api/workspace", get(workspace))
            .route("/api/agent-states", get(agents))
            .route("/api/pr-refresh", post(|| async { StatusCode::ACCEPTED }))
            .with_state(data.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Arc::new(
            RemoteClient::new(RemoteServerSpec {
                name: "test".into(),
                base_url: format!("http://{addr}"),
                token: None,
            })
            .unwrap(),
        );
        Self { client, data, task }
    }
    pub async fn wait_for_requests(&self, count: usize) {
        let mut rx = self.data.waits.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while *rx.borrow_and_update() < count {
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
}
async fn changes(
    State(data): State<Data>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<u64>, StatusCode> {
    data.waits.send_modify(|n| *n += 1);
    if !data.supported {
        return Err(StatusCode::NOT_FOUND);
    }
    // Fail before parking, so the failure arrives without a generation bump:
    // a poller that retried with its old `since` would park instead of recovering.
    if data.fail_wait.swap(false, Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let mut rx = data.generation.subscribe();
    let current = *rx.borrow_and_update();
    if q.get("since").and_then(|s| s.parse().ok()) == Some(current) {
        rx.changed().await.unwrap();
    }
    let next = *rx.borrow();
    Ok(Json(next))
}
async fn workspace(State(data): State<Data>) -> Result<Json<serde_json::Value>, StatusCode> {
    data.workspace.fetch_add(1, Ordering::SeqCst);
    if data.fail_workspace.swap(false, Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    Ok(Json(
        serde_json::json!({"projects":[],"sessions":[],"server":{"gh_available":false,"tmux_ok":false,"version":"test"}}),
    ))
}
async fn agents(State(data): State<Data>) -> Json<serde_json::Value> {
    data.agents.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"states":{},"commander_running":false}))
}
