//! Router construction: the `/api` surface (behind bearer auth + a CORS layer),
//! the `/ws` upgrade, a lightweight `/health` liveness probe, and the embedded
//! web UI as the fallback for everything else.

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Method, header::AUTHORIZATION},
    middleware::from_fn_with_state,
    routing::{any, delete, get, post, put},
};
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::{AllowOrigin, CorsLayer},
};
use tracing::warn;

use crate::auth::require_bearer;
use crate::handlers::{
    blobs, cascade, config, github, health, paste, projects, review, sessions, snapshot,
};
use crate::state::AppState;
use crate::{webui, ws};

/// The `/api` and `/ws` fallback: a JSON 404 in the shared error envelope.
async fn api_not_found() -> axum::response::Response {
    crate::error::error_response(
        axum::http::StatusCode::NOT_FOUND,
        "route",
        "no such endpoint",
    )
}

/// Build the CORS layer for the `/api` surface from the configured allowlist.
///
/// An empty allowlist denies all cross-origin requests (same-origin only): no
/// `Access-Control-Allow-Origin` header is ever emitted, so browsers block
/// cross-origin reads. A non-empty allowlist permits exactly those origins
/// (each parsed to a `HeaderValue`; unparseable entries are dropped with a
/// warning), plus the methods/headers our API actually uses.
fn cors_layer(allowed_origins: &[String]) -> CorsLayer {
    let origins: Vec<HeaderValue> = allowed_origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("ignoring invalid CORS origin {o:?}: {e}");
                None
            }
        })
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([AUTHORIZATION, axum::http::header::CONTENT_TYPE])
}

/// Build the full application router.
pub fn build_router(state: AppState) -> Router {
    let auth = state.auth.clone();
    let auth_for_slash = auth.clone();
    let cors = cors_layer(&state.cors_allowed_origins);

    let api = Router::new()
        // -- workspace surface --
        .route("/workspace", get(snapshot::snapshot))
        .route("/agent-states", get(snapshot::agent_states))
        .route("/pr-refresh", post(snapshot::pr_refresh))
        .route("/create-options", get(snapshot::create_options))
        .route("/comments/pending", get(review::pending))
        // -- cascade / push-stack --
        .route("/cascade/resume", post(cascade::resume))
        .route("/cascade/abandon", post(cascade::abandon))
        // -- sessions --
        .route("/sessions", get(sessions::list).post(sessions::create))
        .route("/sessions/find", get(sessions::find))
        .route("/sessions/unread", post(sessions::unread))
        .route("/sessions/{q}/detail", get(sessions::detail))
        .route("/sessions/{q}/pane", get(sessions::pane))
        .route("/sessions/{id}/kill", post(sessions::kill))
        .route("/sessions/{id}/restart", post(sessions::restart))
        .route(
            "/sessions/{id}/restart-fresh",
            post(sessions::restart_fresh),
        )
        .route(
            "/sessions/{id}",
            delete(sessions::delete).patch(sessions::patch),
        )
        .route("/sessions/{id}/preview", get(sessions::preview))
        .route("/sessions/{id}/branch-diff", get(sessions::branch_diff))
        .route("/sessions/{id}/read", post(sessions::read))
        .route("/sessions/{id}/keep-alive", post(sessions::keep_alive))
        .route("/sessions/{id}/base", post(sessions::set_base))
        .route("/sessions/{id}/cascade", post(cascade::cascade))
        .route("/sessions/{id}/push-stack", post(cascade::push_stack))
        // -- review + comments --
        .route("/sessions/{id}/review", get(review::open))
        .route("/sessions/{id}/review/refresh", get(review::refresh))
        .route(
            "/sessions/{id}/comments",
            get(review::list_comments).post(review::create_comment),
        )
        .route(
            "/sessions/{id}/comments/{cid}",
            delete(review::delete_comment),
        )
        .route("/sessions/{id}/comments/apply", post(review::apply))
        .route(
            "/sessions/{id}/files/reviewed",
            post(review::toggle_reviewed).get(review::reviewed_marks),
        )
        // -- blobs --
        .route("/sessions/{id}/blob", get(blobs::fetch))
        // -- pasted-image upload (raw image body) --
        // The route carries its own body-size limit sized to the paste-image cap
        // (the global axum default is 2 MiB, too small for a screenshot); the
        // service re-checks the length as defence in depth.
        .route(
            "/sessions/{id}/paste-image",
            post(paste::paste_image).route_layer(DefaultBodyLimit::max(
                claude_commander_protocol::paste::MAX_IMAGE_BYTES,
            )),
        )
        // -- projects --
        .route("/projects", get(projects::list).post(projects::add))
        .route("/projects/scan", post(projects::scan))
        .route("/projects/ensure", post(projects::ensure))
        .route("/projects/{id}", delete(projects::delete))
        .route("/projects/{id}/workspace", put(projects::set_workspace))
        .route("/projects/{id}/branches", get(projects::branches))
        .route("/projects/{id}/preview", get(projects::preview))
        // -- repo picker + clone --
        // `/projects/clone` sits alongside `/projects/{id}`, and it **does** rely
        // on the router preferring a static segment over a capture: without that,
        // "clone" binds to `{id}` and a POST here reaches a path that only
        // registers DELETE, i.e. a 405 rather than this handler. The differing
        // methods do not save it — they are what the failure would look like.
        // `routes_reach_the_clone_handlers` is the receipt: it drives the real
        // router and identifies the handler by a message only it produces, so a
        // capture match shows up as a wrong status/body rather than passing
        // quietly.
        .route("/github/repos", get(github::repos))
        .route("/projects/clone", post(github::clone))
        .route("/projects/clone/{job}", get(github::clone_status))
        // -- config + health --
        .route("/config", get(config::read).patch(config::update))
        .route("/config/programs", put(config::put_programs))
        // -- workspaces (definitions; project tags go via /projects/{id}/workspace) --
        .route("/config/workspaces", put(config::put_workspaces))
        .route("/config/workspaces/rename", post(config::rename_workspace))
        .route("/config/workspaces/delete", post(config::delete_workspace))
        .route("/config/reload", post(config::reload))
        .route("/health/tmux", get(config::health_tmux))
        // An `/api` miss is an API 404, never the web UI's `index.html`: without
        // its own fallback a nested miss would reach the root fallback below
        // and answer 200 HTML to a client expecting JSON. Declared *before* the
        // layers so it sits behind auth too — an unauthenticated probe gets the
        // same 401 as a real route and learns nothing about which paths exist.
        .fallback(api_not_found)
        // Bearer auth guards the whole `/api` surface; the CORS layer sits
        // outside auth so browser preflight (OPTIONS, unauthenticated) is
        // answered correctly.
        .layer(from_fn_with_state(auth, require_bearer))
        .layer(cors);

    // The WS handshake authenticates in-band (browsers can't set headers on the
    // upgrade), so `/ws` sits outside the `/api` bearer layer.
    let ws = Router::new()
        .route("/attach", get(ws::attach))
        .fallback(api_not_found);

    // `nest("/api")` matches `/api` and `/api/…` but not `/api/` itself, which
    // would otherwise fall through to the page. Answer it as the API would —
    // behind the same bearer check — rather than with `index.html`.
    let api_slash = any(api_not_found).layer(from_fn_with_state(auth_for_slash, require_bearer));

    Router::new()
        .nest("/api", api)
        .route("/api/", api_slash)
        .nest("/ws", ws)
        .route("/ws/", any(api_not_found))
        // Lightweight liveness probe, outside the auth layer.
        .route("/health", get(health::live))
        // Everything else is the embedded web UI (unknown paths → index.html),
        // outside the auth layer like `/health`: the page holds no data.
        .fallback(webui::serve)
        // Defense-in-depth: a panicking handler returns 500 instead of dropping
        // the connection (complements `run_local`'s explicit 500 mapping).
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::handlers::test_support::test_state;

    const ALLOWED: &str = "https://app.example.com";
    const OTHER: &str = "https://evil.example.com";

    /// An allowed origin gets an `Access-Control-Allow-Origin` header echoing it.
    #[tokio::test]
    async fn allowed_origin_gets_cors_header() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir).with_cors(vec![ALLOWED.to_string()]);
        let app = super::build_router(state);

        let req = Request::get("/api/config")
            .header(header::ORIGIN, ALLOWED)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let allow = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok());
        assert_eq!(allow, Some(ALLOWED));
    }

    /// An origin not on the allowlist gets no `Access-Control-Allow-Origin`
    /// header, so a browser blocks the cross-origin read.
    #[tokio::test]
    async fn unlisted_origin_gets_no_cors_header() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir).with_cors(vec![ALLOWED.to_string()]);
        let app = super::build_router(state);

        let req = Request::get("/api/config")
            .header(header::ORIGIN, OTHER)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "unlisted origin must not receive a CORS allow header"
        );
    }

    /// The clone routes resolve to *their own* handlers through the real router.
    ///
    /// `/projects/clone` sits next to `/projects/{id}`, so the literal "clone"
    /// could be captured as a project id and `/projects/clone/{job}` could be read
    /// as some `/projects/{id}/…` sub-route. Both assertions below identify the
    /// handler by a message only it produces, so a mis-resolution shows up as a
    /// wrong body rather than passing on a coincidentally equal status.
    #[tokio::test]
    async fn routes_reach_the_clone_handlers() {
        let dir = TempDir::new().unwrap();
        let app = super::build_router(test_state(&dir));

        // A malformed job id → 400 from `parse_clone_job_id`. Routed to
        // `/projects/{id}/…` instead this would be a 404 or a project-id message.
        let req = Request::get("/api/projects/clone/not-a-uuid")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("not a valid clone job id"),
            "not the clone-status handler: {body}"
        );

        // A rejected source → 400 from the clone handler. A routing miss would be
        // a 404 (no such path) or 405 (wrong method on `/projects/{id}`).
        let req = Request::post("/api/projects/clone")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "source": { "kind": "github", "full_name": "nope" } })
                    .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("owner/name repository slug"),
            "not the clone handler: {body}"
        );
    }

    /// With an empty allowlist (default), even a syntactically valid origin is
    /// denied — same-origin only.
    #[tokio::test]
    async fn empty_allowlist_denies_cross_origin() {
        let dir = TempDir::new().unwrap();
        let state = test_state(&dir); // no CORS configured
        let app = super::build_router(state);

        let req = Request::get("/api/config")
            .header(header::ORIGIN, ALLOWED)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "empty allowlist must not emit a CORS allow header"
        );
    }

    /// The snapshot is served at `/api/workspace` with a fixed top-level key
    /// set. The Rust type is named `Snapshot`, but the URL and the JSON shape
    /// are the wire contract every client (TUI remote, Flutter) builds against,
    /// so a rename must not move either.
    #[tokio::test]
    async fn snapshot_is_served_at_api_workspace_with_a_stable_shape() {
        let dir = TempDir::new().unwrap();
        let app = super::build_router(test_state(&dir));

        let req = Request::get("/api/workspace").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("snapshot is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "cascade_paused",
                "main_workspace",
                "operations",
                "pending_comment_sessions",
                "project_pull",
                "projects",
                "server",
                "sessions",
                "startup_workspace",
                "workspaces",
            ]
        );
    }

    /// A router whose `/api` surface demands a bearer token, for the asset
    /// tests that must prove the fallback does not shadow auth.
    fn token_router(dir: &TempDir) -> axum::Router {
        let state = crate::state::AppState::new(
            test_state(dir).service,
            crate::auth::AuthConfig::Token("secret".into()),
        );
        super::build_router(state)
    }

    async fn get_resp(app: axum::Router, uri: &str) -> axum::response::Response {
        app.oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn content_type(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The web UI is served from the root, unauthenticated (the page itself
    /// holds no data; every call it makes goes through `/api`'s bearer layer).
    #[tokio::test]
    async fn root_serves_the_embedded_index_html() {
        let dir = TempDir::new().unwrap();
        let resp = get_resp(token_router(&dir), "/").await;
        assert_eq!(resp.status(), 200);
        assert!(content_type(&resp).starts_with("text/html"), "{resp:?}");
        let body = body_string(resp).await;
        assert!(body.contains("<title>Claude Commander</title>"), "{body}");
    }

    /// A browser reload of a client-side route still loads the app.
    #[tokio::test]
    async fn unknown_page_path_falls_back_to_index_html() {
        let dir = TempDir::new().unwrap();
        let resp = get_resp(token_router(&dir), "/sessions/some-client-route").await;
        assert_eq!(resp.status(), 200);
        assert!(content_type(&resp).starts_with("text/html"), "{resp:?}");
        assert!(
            body_string(resp)
                .await
                .contains("<title>Claude Commander</title>")
        );
    }

    /// Scripts carry a JavaScript MIME type (a browser refuses to execute a
    /// script served as `text/html` under `nosniff`), plus the caching headers:
    /// revalidate every time, keyed on a content hash.
    #[tokio::test]
    async fn app_js_is_served_as_javascript_with_an_etag() {
        let dir = TempDir::new().unwrap();
        let resp = get_resp(token_router(&dir), "/app.js").await;
        assert_eq!(resp.status(), 200);
        assert!(content_type(&resp).contains("javascript"), "{resp:?}");
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert!(resp.headers().get(header::ETAG).is_some(), "{resp:?}");
    }

    /// A matching `If-None-Match` answers 304 with no body.
    #[tokio::test]
    async fn matching_if_none_match_is_not_modified() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);
        let first = get_resp(app.clone(), "/app.js").await;
        let etag = first.headers().get(header::ETAG).unwrap().clone();
        let req = Request::get("/app.js")
            .header(header::IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 304);
        assert!(body_string(resp).await.is_empty());
    }

    /// The terminal is embedded too — without it the page loads but can never
    /// attach. web/build.mjs bundles xterm.js (+ its fit addon) into `app.js`,
    /// with a licence banner naming each package, and inlines xterm's
    /// stylesheet into `style.css`.
    #[tokio::test]
    async fn bundled_xterm_is_embedded() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);

        let js = get_resp(app.clone(), "/app.js").await;
        assert_eq!(js.status(), 200);
        let js = body_string(js).await;
        for pkg in ["@xterm/xterm@", "@xterm/addon-fit@"] {
            assert!(js.contains(pkg), "app.js lacks {pkg}");
        }

        let css = get_resp(app, "/style.css").await;
        assert_eq!(css.status(), 200);
        assert!(content_type(&css).starts_with("text/css"), "{css:?}");
        assert!(body_string(css).await.contains(".xterm-viewport"));
    }

    /// Every page response carries the hardening headers: no MIME sniffing, no
    /// framing (clickjacking a page that holds a bearer token), no referrer,
    /// and a CSP that allows only same-origin scripts and connections.
    #[tokio::test]
    async fn page_responses_carry_security_headers() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);
        for path in ["/", "/app.js", "/style.css", "/some-client-route"] {
            let resp = get_resp(app.clone(), path).await;
            assert_eq!(resp.status(), 200, "{path}");
            let h = resp.headers();
            let get = |name: &str| {
                h.get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_else(|| panic!("{path}: no {name}"))
                    .to_owned()
            };
            assert_eq!(get("x-content-type-options"), "nosniff", "{path}");
            assert_eq!(get("x-frame-options"), "DENY", "{path}");
            assert_eq!(get("referrer-policy"), "no-referrer", "{path}");
            let csp = get("content-security-policy");
            for directive in [
                "default-src 'self'",
                "script-src 'self'",
                "connect-src 'self' ws: wss:",
                "frame-ancestors 'none'",
            ] {
                assert!(csp.contains(directive), "{path}: {csp} lacks {directive}");
            }
            assert!(!csp.contains("unsafe-eval"), "{path}: {csp}");
        }
    }

    /// The page is read-only: anything but GET/HEAD is a 405, not the page.
    #[tokio::test]
    async fn non_get_page_requests_are_method_not_allowed() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);
        for (method, path) in [("POST", "/"), ("PUT", "/app.js"), ("DELETE", "/x")] {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 405, "{method} {path}");
            assert_eq!(resp.headers().get(header::ALLOW).unwrap(), "GET, HEAD");
        }
        let head = Request::head("/").body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(head).await.unwrap().status(), 200);
    }

    /// A missing *file* (the last segment has an extension) is a 404, not
    /// `index.html`: HTML served as a source map or a script is a confusing
    /// failure, and a 200 hides the missing asset. Extension-less paths are
    /// client-side routes and still get the page.
    #[tokio::test]
    async fn missing_asset_paths_are_404_not_the_page() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);
        for path in ["/app.js.map", "/vendor/xterm.js", "/nope.css"] {
            let resp = get_resp(app.clone(), path).await;
            assert_eq!(resp.status(), 404, "{path}");
            assert!(!content_type(&resp).starts_with("text/html"), "{path}");
        }
    }

    /// The favicon is the real SVG, not a Git LFS pointer (which is what a
    /// checkout without LFS smudging would embed).
    #[tokio::test]
    async fn favicon_is_a_real_svg() {
        let dir = TempDir::new().unwrap();
        let resp = get_resp(token_router(&dir), "/favicon.svg").await;
        assert_eq!(resp.status(), 200);
        assert!(content_type(&resp).starts_with("image/svg"), "{resp:?}");
        assert!(body_string(resp).await.starts_with("<svg"));
    }

    /// The fallback sits outside the bearer layer but must not swallow `/api`:
    /// a real route still demands the token.
    #[tokio::test]
    async fn api_routes_still_require_the_bearer_token() {
        let dir = TempDir::new().unwrap();
        let resp = get_resp(token_router(&dir), "/api/sessions").await;
        assert_eq!(resp.status(), 401);
        assert!(
            content_type(&resp).starts_with("application/json"),
            "{resp:?}"
        );
    }

    /// An unknown `/api` path is an API miss — 401 without a token, a JSON 404
    /// with one — never the page. Serving `index.html` there would hand a
    /// client a 200 it then tries to parse as JSON.
    #[tokio::test]
    async fn unknown_api_paths_are_not_the_page() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);
        for path in ["/api", "/api/", "/api/nope", "/api/sessions/x/nope/deeper"] {
            let resp = get_resp(app.clone(), path).await;
            assert_eq!(resp.status(), 401, "{path} without a token");
            assert!(!content_type(&resp).starts_with("text/html"), "{path}");

            let req = Request::get(path)
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 404, "{path} with a token");
            assert!(
                content_type(&resp).starts_with("application/json"),
                "{path}"
            );
        }
    }

    /// `/health` keeps its plain liveness body; the fallback does not replace it.
    #[tokio::test]
    async fn health_is_unchanged_by_the_fallback() {
        let dir = TempDir::new().unwrap();
        let resp = get_resp(token_router(&dir), "/health").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body_string(resp).await, "ok");
    }

    /// `/ws/attach` is still the WS handler (a plain GET is refused as a
    /// non-upgrade, not answered with the page), and a `/ws` miss is a 404.
    #[tokio::test]
    async fn ws_attach_is_not_shadowed_by_the_fallback() {
        let dir = TempDir::new().unwrap();
        let app = token_router(&dir);
        let resp = get_resp(app.clone(), "/ws/attach").await;
        assert_ne!(resp.status(), 200);
        assert!(!content_type(&resp).starts_with("text/html"), "{resp:?}");

        for path in ["/ws", "/ws/", "/ws/nope"] {
            let resp = get_resp(app.clone(), path).await;
            assert_eq!(resp.status(), 404, "{path}");
            assert!(!content_type(&resp).starts_with("text/html"), "{path}");
        }
    }

    /// The workspace routes are reachable through the real router (and so
    /// behind auth, with the static `rename`/`delete` segments not shadowed).
    #[tokio::test]
    async fn workspace_routes_are_mounted() {
        let dir = TempDir::new().unwrap();
        let app = super::build_router(test_state(&dir));
        for (method, uri, body) in [
            (
                "PUT",
                "/api/config/workspaces",
                r#"{"workspaces":[{"name":"Work"}]}"#,
            ),
            (
                "POST",
                "/api/config/workspaces/rename",
                r#"{"from":"Work","to":"Job"}"#,
            ),
            ("POST", "/api/config/workspaces/delete", r#"{"name":"Job"}"#),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 204, "{method} {uri}");
        }
        let req = Request::put(format!("/api/projects/{}/workspace", uuid::Uuid::new_v4()))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"workspace":"Work"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            404,
            "an unknown project, not an unmounted route"
        );
    }
}
