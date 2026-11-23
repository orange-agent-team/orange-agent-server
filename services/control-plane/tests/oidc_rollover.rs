//! The issuer is a loopback protocol fixture with ephemeral RSA keys. The relying party is the real Rust client.
use orange_control_plane::auth::{
    AuthError,
    oidc::{OidcProvider, ProviderConfig},
};
use serde_json::json;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn fresh_key_set_verifies_multi_key_lifetimes_without_repeating_code_exchange() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rotating-issuer.mjs");
    let mut child =
        tokio::process::Command::new(std::env::var("ORANGE_TEST_NODE").expect("pinned Node path"))
            .arg(path)
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("loopback fixture");
    let mut lines = BufReader::new(child.stdout.take().expect("fixture stdout")).lines();
    let issuer = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .expect("issuer readiness deadline")
        .expect("read issuer origin")
        .expect("issuer provided its URL");
    let provider = OidcProvider::discover(ProviderConfig {
        issuer: issuer.clone(),
        development: true,
        client_id: "orange-key-rollover-test".into(),
        client_secret: "development-only-fixture-secret-00000".into(),
        origin: "https://127.0.0.1:43122".into(),
        callback_path: "/api/v1/auth/callback".into(),
    })
    .await
    .expect("fixed trusted issuer");
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("local HTTP transport");
    for index in 1..=9 {
        let flow = provider.start("fixture").expect("real PKCE challenge");
        let redirect = http
            .get(flow.authorization_url)
            .send()
            .await
            .expect("authorize fixture login");
        assert_eq!(redirect.status(), reqwest::StatusCode::FOUND);
        let target = reqwest::Url::parse(
            redirect.headers()["location"]
                .to_str()
                .expect("redirect is text"),
        )
        .expect("parse callback");
        let pairs: std::collections::HashMap<_, _> = target.query_pairs().collect();
        assert_eq!(pairs["state"], flow.state);
        let result = provider
            .exchange(&pairs["code"], flow.verifier, flow.nonce)
            .await;
        if index == 3 {
            assert!(matches!(result, Err(AuthError::Unavailable)));
        } else if (5..=8).contains(&index) {
            assert!(matches!(result, Err(AuthError::Unauthenticated)));
        } else {
            let identity = result.expect("verified by the currently published key");
            assert_eq!(identity.issuer(), issuer);
            assert_eq!(identity.subject(), "fixture-subject");
        }
    }
    let counts: serde_json::Value = http
        .get(format!("{issuer}/counts"))
        .send()
        .await
        .expect("get issuer counters")
        .json()
        .await
        .expect("decode counter evidence");
    assert_eq!(counts, json!({"exchanges":9,"reads":10}));
    child.kill().await.expect("stop test issuer");
}
