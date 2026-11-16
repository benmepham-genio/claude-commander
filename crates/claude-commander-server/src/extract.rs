//! Request extractors that do not echo the request back.
//!
//! [`SafeJson`] exists because axum's own [`Json`] extractor answers a
//! wrong-shaped body by rendering serde's message, and serde quotes the offending
//! **value**. For a body carrying a credential that is a disclosure that happens
//! *below* every redaction the rest of the codebase performs: an extractor
//! rejection short-circuits in `FromRequest`, so no handler, no
//! `redact_credentials`, and no [`ApiError`](crate::error::ApiError) ever runs.

use axum::{
    extract::{FromRequest, Request, rejection::JsonRejection},
    http::StatusCode,
    response::Response,
};
use serde::de::DeserializeOwned;

use crate::error::error_response;

/// A [`Json`](axum::Json) extractor whose rejection never quotes the request.
///
/// **Any route whose request body can carry a credential must use this instead of
/// `Json`.** Today that includes `POST /api/projects/clone`, whose `source` is
/// routinely `https://user:token@host/owner/repo`, and `PATCH /api/config`, whose
/// optional GitLab hostname must reject userinfo without reflecting it.
///
/// The hazard is not hypothetical and not in our code. Verified against axum
/// 0.8.9: posting `{"source": "https://sizeak:ghp_TOKEN@github.com/o/r"}` to the
/// clone route — valid JSON, wrong shape, an ordinary client mistake — returned
///
/// ```text
/// 422 Unprocessable Entity
/// Failed to deserialize the JSON body into the target type: source: invalid
/// type: string "https://sizeak:ghp_TOKEN@github.com/o/r", expected internally
/// tagged enum CloneSource at line 1 column 57
/// ```
///
/// `Json`'s `FromRequest` returns that via `JsonRejection`'s own `IntoResponse`,
/// so the whole redaction chain this codebase builds — redacting at the
/// *construction* site of every `CloneRejection`, precisely so no later hop has
/// to remember — is bypassed before the handler is entered. Pinned by
/// `handlers::github::tests::a_malformed_body_is_400_without_the_token`.
///
/// What comes back instead names the *category* of the problem and nothing else:
/// no field values, no serde detail, no line/column. Category alone is enough to
/// act on ("my JSON is malformed" vs "my JSON does not match the schema" vs "I
/// forgot the content-type"), and it is derived from the rejection variant rather
/// than from `body_text()`, which is what carries the value.
///
/// Rejections are normalised to **400**, not axum's 422. The API documents 400 as
/// "your request was unusable", and a client should not have to learn a second
/// status for the same class of mistake depending on whether the body failed in
/// the extractor or in a validator.
///
/// This is deliberately a named type rather than an inline `map_err` on the one
/// handler that needs it: it gives the next person adding a secret-bearing route
/// something to reach for, and gives a reviewer something to grep for.
///
/// **Not applied to every JSON route, and that is a checked decision rather than
/// a default.** Serde's detail is genuinely useful on a body that cannot hold a
/// secret (a path, a bool, a comment anchor), so stripping it everywhere would
/// cost debuggability for no gain.
#[derive(Debug, Clone, Copy, Default)]
pub struct SafeJson<T>(pub T);

impl<S, T> FromRequest<S> for SafeJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_response(&rejection)),
        }
    }
}

/// Render a [`JsonRejection`] as a valueless 400 in the shared error envelope.
///
/// Matches on the *variant* and never calls `body_text()`/`Display`, which is
/// where the echoed value lives. `JsonRejection` is `#[non_exhaustive]`, so the
/// catch-all is required — and it is also the safe default: a variant added by a
/// future axum gets the generic message rather than silently starting to echo.
fn rejection_response(rejection: &JsonRejection) -> Response {
    let message = match rejection {
        JsonRejection::JsonSyntaxError(_) => "request body is not valid JSON",
        JsonRejection::JsonDataError(_) => {
            "request body does not match the expected shape for this endpoint"
        }
        JsonRejection::MissingJsonContentType(_) => {
            "expected a JSON body with content-type: application/json"
        }
        JsonRejection::BytesRejection(_) => "request body could not be read",
        _ => "request body was rejected",
    };
    error_response(StatusCode::BAD_REQUEST, "request", message)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::{Router, routing::post};
    use serde::Deserialize;

    use super::SafeJson;
    use crate::handlers::test_support::{json, send};

    #[derive(Debug, Deserialize)]
    struct TestBody {
        #[allow(dead_code)]
        flag: bool,
    }

    async fn handler(SafeJson(_): SafeJson<TestBody>) -> &'static str {
        "ok"
    }

    fn router() -> Router {
        Router::new().route("/t", post(handler))
    }

    fn post_raw(body: &str, content_type: Option<&str>) -> Request<Body> {
        let mut req = Request::post("/t");
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    /// A well-formed body still reaches the handler — the extractor must not be a
    /// blanket reject.
    #[tokio::test]
    async fn a_valid_body_reaches_the_handler() {
        let (status, body) = send(
            router(),
            post_raw(r#"{"flag":true}"#, Some("application/json")),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(String::from_utf8_lossy(&body), "ok");
    }

    /// Every rejection is a 400 in the shared envelope, and none of them quotes
    /// the value that caused it.
    ///
    /// The `secret` here sits in a field of the *wrong type*, which is exactly the
    /// shape that makes `Json` echo it: serde renders `invalid type: string
    /// "…", expected a boolean`.
    #[tokio::test]
    async fn no_rejection_echoes_the_body() {
        const SECRET: &str = "ghp_s3cr3tt0ken";
        let cases = [
            // Valid JSON, wrong type for `flag` — the echoing case.
            (
                format!(r#"{{"flag":"{SECRET}"}}"#),
                Some("application/json"),
                "shape",
            ),
            // Not JSON at all, with the secret in the malformed text.
            (
                format!(r#"{{"flag": {SECRET}"#),
                Some("application/json"),
                "JSON",
            ),
            // Right body, missing content-type.
            (format!(r#"{{"flag":"{SECRET}"}}"#), None, "content-type"),
        ];

        for (body, content_type, expect_hint) in cases {
            let (status, raw) = send(router(), post_raw(&body, content_type)).await;
            let text = String::from_utf8_lossy(&raw);
            assert_eq!(status, 400, "body={body} response={text}");
            assert!(!text.contains(SECRET), "value echoed: {text}");

            // Still actionable: the envelope parses and names the category.
            let parsed: serde_json::Value = json(&raw);
            let message = parsed["error"]["message"].as_str().unwrap();
            assert_eq!(parsed["error"]["kind"], "request");
            assert!(
                message.contains(expect_hint),
                "message {message:?} does not hint at {expect_hint:?}"
            );
        }
    }
}
