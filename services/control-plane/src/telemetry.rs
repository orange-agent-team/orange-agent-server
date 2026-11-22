//! Fixed-cardinality HTTP metrics, exported only on a separate loopback listener.
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram, MeterProvider, UpDownCounter},
};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider};
use prometheus::{Registry, TextEncoder};
use std::{env, error::Error, net::SocketAddr, time::Instant};
use tokio::{net::TcpListener, task::JoinHandle};

pub enum ApiRole {
    User,
    Admin,
    Development,
}

impl ApiRole {
    fn name(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
            Self::Development => "development",
        }
    }
}

#[derive(Clone)]
struct HttpMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
    active: UpDownCounter<i64>,
    role: KeyValue,
}

/// The API owns the listener and instruments for its lifetime, including startup failure.
pub struct MetricsEndpoint {
    task: JoinHandle<()>,
    _provider: SdkMeterProvider,
}

impl Drop for MetricsEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn configure(
    app: Router,
    role: ApiRole,
) -> Result<(Router, Option<MetricsEndpoint>), Box<dyn Error>> {
    let address = match env::var("ORANGE_METRICS_BIND") {
        Ok(value) => value.parse::<SocketAddr>()?,
        Err(env::VarError::NotPresent) => return Ok((app, None)),
        Err(error) => return Err(error.into()),
    };
    let (app, endpoint, _) = bind(app, role, address).await?;
    Ok((app, Some(endpoint)))
}

async fn bind(
    app: Router,
    role: ApiRole,
    address: SocketAddr,
) -> Result<(Router, MetricsEndpoint, SocketAddr), Box<dyn Error>> {
    if !address.ip().is_loopback() {
        return Err("HTTP metrics require a dedicated loopback listener".into());
    }
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let registry = Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .without_target_info()
        .scope_info_enabled(false)
        .build()?;
    let provider = SdkMeterProvider::builder()
        .with_resource(Resource::builder_empty().build())
        .with_reader(exporter)
        .build();
    let meter = provider.meter("orange-control-plane");
    let metrics = HttpMetrics {
        requests: meter
            .u64_counter("orange.http.responses")
            .with_description("HTTP responses with completed headers")
            .build(),
        duration: meter
            .f64_histogram("orange.http.response.duration")
            .with_unit("s")
            .with_description("Time until response headers are available")
            .with_boundaries(vec![0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0, 120.0])
            .build(),
        active: meter
            .i64_up_down_counter("orange.http.active")
            .with_description("HTTP requests awaiting response headers")
            .build(),
        role: KeyValue::new("api_role", role.name()),
    };
    let observer = Router::new()
        .route("/metrics", get(scrape))
        .with_state(registry);
    let task = tokio::spawn(async move {
        if axum::serve(listener, observer).await.is_err() {
            eprintln!("Local HTTP metrics listener stopped unexpectedly");
        }
    });
    eprintln!(
        "Local {} HTTP metrics listening on {}",
        role.name(),
        address
    );
    Ok((
        app.layer(middleware::from_fn_with_state(metrics, record)),
        MetricsEndpoint {
            task,
            _provider: provider,
        },
        address,
    ))
}

async fn scrape(State(registry): State<Registry>) -> Response {
    match TextEncoder::new().encode_to_string(&registry.gather()) {
        Ok(text) => (
            [
                (
                    header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                ),
                (header::CACHE_CONTROL, "no-store"),
            ],
            text,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

struct ActiveRequest<'a>(&'a HttpMetrics);
impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.0.active.add(-1, std::slice::from_ref(&self.0.role));
    }
}

async fn record(State(metrics): State<HttpMetrics>, request: Request, next: Next) -> Response {
    let method = match *request.method() {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PATCH => "PATCH",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    };
    metrics.active.add(1, std::slice::from_ref(&metrics.role));
    let _active = ActiveRequest(&metrics);
    let start = Instant::now();
    let response = next.run(request).await;
    let status = match response.status().as_u16() / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    };
    let attributes = [
        metrics.role.clone(),
        KeyValue::new("method", method),
        KeyValue::new("status_class", status),
    ];
    metrics.requests.add(1, &attributes);
    metrics
        .duration
        .record(start.elapsed().as_secs_f64(), &attributes);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use std::{sync::Arc, time::Duration};
    use tokio::{sync::Notify, time::timeout};
    use tower::ServiceExt;

    #[tokio::test]
    async fn actual_scrape_separates_roles_and_never_exports_request_data() {
        let refused = bind(
            Router::new(),
            ApiRole::User,
            "0.0.0.0:0".parse().expect("address"),
        )
        .await;
        assert!(refused.is_err());
        let app = Router::new().route("/objects/{id}", get(|| async { StatusCode::FORBIDDEN }));
        let (api, _endpoint, address) =
            bind(app, ApiRole::User, "127.0.0.1:0".parse().expect("address"))
                .await
                .expect("private observer");
        let (admin, _admin_endpoint, admin_address) = bind(
            Router::new(),
            ApiRole::Admin,
            "127.0.0.1:0".parse().expect("address"),
        )
        .await
        .expect("separate registry");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let api_listener = TcpListener::bind("127.0.0.1:0").await.expect("api");
        let api_addr = api_listener.local_addr().expect("bound");
        let task = tokio::spawn(async move { axum::serve(api_listener, api).await });
        assert_eq!(
            client
                .get(format!(
                    "http://{api_addr}/objects/private-record?value=private-query"
                ))
                .bearer_auth("private-token")
                .send()
                .await
                .expect("invoke")
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .request(
                    reqwest::Method::from_bytes(b"PRIVATEVERB").expect("method"),
                    format!("http://{api_addr}/objects/private-method")
                )
                .send()
                .await
                .expect("custom method")
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            client
                .get(format!("http://{api_addr}/metrics"))
                .send()
                .await
                .expect("user route")
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .get(format!("http://{address}/objects/private-record"))
                .send()
                .await
                .expect("observer route")
                .status(),
            StatusCode::NOT_FOUND
        );
        let response = client
            .get(format!("http://{address}/metrics"))
            .send()
            .await
            .expect("metrics response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .expect("content type")
                .starts_with("text/plain; version=0.0.4")
        );
        let text = response.text().await.expect("scrape");
        for want in [
            "orange_http_responses_total",
            "orange_http_response_duration_seconds_bucket",
            "api_role=\"user\"",
            "method=\"OTHER\"",
            "status_class=\"4xx\"",
        ] {
            assert!(text.contains(want), "Missing metric {want}: {text}");
        }
        for hidden in [
            "private-",
            "PRIVATEVERB",
            "objects/",
            "target_info",
            "otel_scope",
            "Authorization",
        ] {
            assert!(
                !text.contains(hidden),
                "Unexpected request or resource attribute: {hidden}"
            );
        }
        admin
            .oneshot(Request::get("/").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let separate = client
            .get(format!("http://{admin_address}/metrics"))
            .send()
            .await
            .expect("admin scrape")
            .text()
            .await
            .expect("text");
        assert!(separate.contains("api_role=\"admin\"") && !separate.contains("api_role=\"user\""));
        task.abort();
    }

    #[tokio::test]
    async fn cancelled_request_releases_active_gauge_without_inventing_a_response() {
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        let app = Router::new().route(
            "/pending",
            get(move || async move {
                signal.notify_one();
                std::future::pending::<StatusCode>().await
            }),
        );
        let (app, endpoint, addr) = bind(
            app,
            ApiRole::Development,
            "127.0.0.1:0".parse().expect("address"),
        )
        .await
        .expect("observer");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let url = format!("http://{addr}/metrics");
        let request = tokio::spawn(
            app.oneshot(
                Request::get("/pending")
                    .body(Body::empty())
                    .expect("request"),
            ),
        );
        timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("handler entered");
        let during = client
            .get(&url)
            .send()
            .await
            .expect("response")
            .text()
            .await
            .expect("metrics");
        assert!(
            during
                .lines()
                .any(|l| l.starts_with("orange_http_active{") && l.ends_with(" 1"))
        );
        request.abort();
        assert!(request.await.expect_err("aborted").is_cancelled());
        let after = client
            .get(&url)
            .send()
            .await
            .expect("response")
            .text()
            .await
            .expect("metrics");
        assert!(
            after
                .lines()
                .any(|l| l.starts_with("orange_http_active{") && l.ends_with(" 0"))
        );
        assert!(
            !after
                .lines()
                .any(|l| l.starts_with("orange_http_responses_total{"))
        );
        drop(endpoint);
        timeout(Duration::from_secs(3), async {
            while tokio::net::TcpStream::connect(addr).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("observer closed with owner");
    }
}
