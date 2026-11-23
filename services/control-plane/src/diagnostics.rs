//! Operational correlation only. Never enqueue headers, URLs, bodies or arbitrary log values.
use crate::telemetry::ApiRole;
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderValue, Method},
    middleware::{self, Next},
    response::Response,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
};
use uuid::Uuid;

static SINK: OnceLock<Option<SyncSender<Event>>> = OnceLock::new();
static LOST: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
struct Event {
    event: &'static str,
    role: &'static str,
    request_id: UuidText,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    dropped_before: u64,
}

#[derive(Serialize)]
#[serde(transparent)]
struct UuidText(String);

pub fn link(kind: &'static str, id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"orange.diagnostics.v1\0");
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update(id.as_bytes());
    hex::encode(hash.finalize())
}

fn emit(event: Event) {
    let channel = SINK.get_or_init(|| {
        let (sender, receiver) = sync_channel::<Event>(256);
        std::thread::Builder::new()
            .name("orange-diagnostics".into())
            .spawn(move || {
                for mut event in receiver {
                    event.dropped_before = LOST.swap(0, Ordering::Relaxed);
                    let Ok(mut bytes) = serde_json::to_vec(&event) else {
                        continue;
                    };
                    bytes.push(b'\n');
                    if std::io::stderr().lock().write_all(&bytes).is_err() {
                        break;
                    }
                }
            })
            .ok()
            .map(|_| sender)
    });
    if channel
        .as_ref()
        .is_none_or(|tx| tx.try_send(event).is_err())
    {
        LOST.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct JobLink(String);
pub(crate) fn job_response(mut response: Response, id: &str) -> Response {
    response.extensions_mut().insert(JobLink(link("job", id)));
    response
}

pub(crate) fn assignment_committed(job: &str, attempt: &str, node: &str) {
    if SINK.get().is_some() || std::env::var("ORANGE_DIAGNOSTIC_LOG").as_deref() == Ok("on") {
        emit(Event {
            event: "assignment_committed",
            role: "worker_gateway",
            request_id: UuidText(Uuid::new_v4().to_string()),
            status: None,
            job: Some(link("job", job)),
            attempt: Some(link("attempt", attempt)),
            node: Some(link("node", node)),
            dropped_before: 0,
        });
    }
}

pub fn observe(router: Router, role: ApiRole, log: bool) -> Router {
    router.layer(middleware::from_fn_with_state((role, log), completed))
}

fn classification(status: u16, read: bool) -> Option<(&'static str, &'static str)> {
    let retry = if read { "backoff" } else { "query_original" };
    match status {
        401 | 403 => Some(("permission", "never")),
        402 | 507 => Some(("quota", "never")),
        429 => Some(("quota", retry)),
        409 => Some(("conflict", "query_original")),
        408 | 502 | 503 | 504 => Some(("unavailable", retry)),
        400..=499 => Some(("input", "never")),
        500..=599 => Some(("internal", "never")),
        _ => None,
    }
}

// Preserve the retry policy chosen by the handler; many conflict responses must never be replayed.
pub(crate) fn typed_failure(
    status: axum::http::StatusCode,
    error: orange_protocol::ApiError,
) -> Response {
    use axum::response::IntoResponse;
    let override_class = match error.code.as_str() {
        "INSUFFICIENT_BALANCE" | "BUDGET_EXCEEDED" | "STORAGE_CAPACITY" => Some("quota"),
        "SPENDING_PAUSED" => Some("permission"),
        _ => None,
    };
    let retry = match error.retry_class.as_str() {
        "after_reauth" => "after_reauth",
        "after_reconciliation" => "after_reconciliation",
        "query_original" => "query_original",
        "backoff" => "backoff",
        _ => "never",
    };
    let mut response = (status, axum::Json(error)).into_response();
    response
        .headers_mut()
        .insert("x-orange-retry-class", HeaderValue::from_static(retry));
    if let Some(class) = override_class {
        response
            .headers_mut()
            .insert("x-orange-error-class", HeaderValue::from_static(class));
    }
    response
}

async fn completed(
    State((role, log)): State<(ApiRole, bool)>,
    req: Request,
    next: Next,
) -> Response {
    let id = Uuid::new_v4().to_string();
    let read = matches!(*req.method(), Method::GET | Method::HEAD);
    let mut response = next.run(req).await;
    let status = response.status().as_u16();
    if let Ok(value) = HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .insert("x-orange-diagnostic-id", value);
    }
    if let Some((class, retry)) = classification(status, read) {
        response
            .headers_mut()
            .entry("x-orange-error-class")
            .or_insert(HeaderValue::from_static(class));
        response
            .headers_mut()
            .entry("x-orange-retry-class")
            .or_insert(HeaderValue::from_static(retry));
    }
    if log {
        emit(Event {
            event: "http_completed",
            role: role.name(),
            request_id: UuidText(id),
            status: Some(status),
            job: response
                .extensions()
                .get::<JobLink>()
                .map(|job| job.0.clone()),
            attempt: None,
            node: None,
            dropped_before: 0,
        });
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn structured_retry_survives_the_http_middleware_and_diagnostic_id_is_server_owned() {
        let app = observe(
            Router::new().route(
                "/quota",
                get(|| async {
                    typed_failure(
                        StatusCode::CONFLICT,
                        orange_protocol::ApiError {
                            code: "BUDGET_EXCEEDED".into(),
                            message: "BUDGET_EXCEEDED".into(),
                            request_id: "original".into(),
                            retry_class: "never".into(),
                        },
                    )
                }),
            ),
            ApiRole::Development,
            false,
        );
        let response = app
            .oneshot(
                HttpRequest::get("/quota")
                    .header("x-orange-diagnostic-id", "private-token")
                    .body(Body::empty())
                    .expect("fixture"),
            )
            .await
            .expect("router");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()["x-orange-error-class"], "quota");
        assert_eq!(response.headers()["x-orange-retry-class"], "never");
        assert!(
            Uuid::parse_str(
                response.headers()["x-orange-diagnostic-id"]
                    .to_str()
                    .expect("ascii")
            )
            .is_ok()
        );
    }
    #[test]
    fn typed_failure_categories_do_not_authorize_replaying_unknown_writes() {
        for (status, expected) in [
            (400, "input"),
            (401, "permission"),
            (403, "permission"),
            (429, "quota"),
            (503, "unavailable"),
            (500, "internal"),
        ] {
            assert_eq!(classification(status, true).expect("failure").0, expected);
        }
        assert_eq!(
            classification(503, false),
            Some(("unavailable", "query_original"))
        );
        assert_eq!(classification(503, true), Some(("unavailable", "backoff")));
        assert_ne!(link("job", "marker"), link("node", "marker"));
        assert!(!link("node", "marker").contains("marker"));
    }
}
