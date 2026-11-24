//! A real private S3 endpoint, ephemeral fixture identity, and the same locked SigV4 library used by OpenDAL.
//! Signed URLs are never emitted into logs or test receipts.
use axum::http::{HeaderMap, Method, Request};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqsign_aws_v4::{Credential, RequestSigner, StaticCredentialProvider};
use reqsign_core::{Context, Signer};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{env, time::Duration};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureIdentity {
    access_key: String,
    secret_key: String,
}

struct Capability {
    url: Url,
    headers: HeaderMap,
}
async fn sign(
    signer: &Signer<Credential>,
    method: Method,
    url: Url,
    ttl: u64,
    checksum: Option<&str>,
) -> Capability {
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    let mut request = Request::builder().method(method).uri(url.as_str());
    if let Some(sum) = checksum {
        request = request.header("x-amz-checksum-sha256", sum);
    }
    let (mut parts, _) = request.body(()).expect("fixture URI").into_parts();
    signer
        .sign(&mut parts, Some(Duration::from_secs(ttl)))
        .await
        .expect("local request signer");
    let signed = Url::parse(&parts.uri.to_string()).expect("signed URI");
    let number = signed
        .query_pairs()
        .filter(|(k, v)| k.eq_ignore_ascii_case("x-amz-expires") && v == ttl.to_string().as_str())
        .count();
    assert_eq!(number, 1);
    let fields = signed
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-amz-signedheaders"))
        .expect("signed headers")
        .1;
    assert!(fields.split(';').any(|h| h == "host"));
    if checksum.is_some() {
        assert!(fields.split(';').any(|h| h == "x-amz-checksum-sha256"));
    }
    Capability {
        url: signed,
        headers: parts.headers,
    }
}

async fn invoke(
    client: &Client,
    cap: &Capability,
    method: Method,
    data: Option<&'static [u8]>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = client
        .request(method, cap.url.clone())
        .headers(cap.headers.clone());
    if let Some(bytes) = data {
        req = req.body(bytes);
    }
    let response = req
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .expect("private fixture transport");
    let status = response.status();
    let headers = response.headers().clone();
    let data = response
        .bytes()
        .await
        .map_err(reqwest::Error::without_url)
        .expect("fixture bytes")
        .to_vec();
    (status, headers, data)
}

#[tokio::test]
async fn fixed_artifact_get_and_staging_put_expire_on_the_real_backend() {
    let port: u16 = env::var("ORANGE_ARTIFACT_FIXTURE_PORT")
        .expect("fixture port")
        .parse()
        .expect("valid port");
    let identity: FixtureIdentity = serde_json::from_slice(
        &std::fs::read(env::var_os("ORANGE_ARTIFACT_FIXTURE_IDENTITY").expect("fixture identity"))
            .expect("private credential file"),
    )
    .expect("fixture credentials");
    let version = env::var("ORANGE_ARTIFACT_FIXTURE_VERSION").expect("published version");
    let signer = Signer::new(
        Context::new(),
        StaticCredentialProvider::new(&identity.access_key, &identity.secret_key),
        RequestSigner::new("s3", "us-east-1"),
    );
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("fixture client");
    let root =
        Url::parse(&format!("http://127.0.0.1:{port}/registered-data/")).expect("local endpoint");
    let mut fixed = root.join("published/probe").expect("fixed path");
    fixed.query_pairs_mut().append_pair("versionId", &version);
    let begin = tokio::time::Instant::now();
    let get = sign(&signer, Method::GET, fixed, 60, None).await;
    assert!(
        get.url
            .query_pairs()
            .any(|(k, v)| k == "versionId" && v == version)
    );
    let data = b"signed artifact staging upload";
    let hash = STANDARD.encode(Sha256::digest(data));
    let put = sign(
        &signer,
        Method::PUT,
        root.join("staging/signed-probe").expect("stage path"),
        300,
        Some(&hash),
    )
    .await;
    let (status, header, _) = invoke(&client, &put, Method::PUT, Some(data)).await;
    assert_eq!(status, StatusCode::OK);
    let first = header
        .get("x-amz-version-id")
        .expect("versioned upload")
        .clone();
    let (status, header, _) = invoke(&client, &put, Method::PUT, Some(data)).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        first,
        header
            .get("x-amz-version-id")
            .expect("distinct reused PUT version")
    );
    let (status, _, body) = invoke(&client, &put, Method::PUT, Some(b"changed body")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("BadDigest"));
    for _ in 0..2 {
        let (status, headers, body) = invoke(&client, &get, Method::GET, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("x-amz-version-id").expect("pinned"),
            version.as_str()
        );
        assert_eq!(body, b"first immutable Orange artifact\n");
    }
    assert_eq!(
        invoke(&client, &get, Method::DELETE, None).await.0,
        StatusCode::FORBIDDEN
    );
    let mut other = get.url.clone();
    other.set_path("/registered-data/");
    assert_eq!(
        invoke(
            &client,
            &Capability {
                url: other,
                headers: get.headers.clone()
            },
            Method::GET,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    println!(
        "PASS fixed-version GET, repeatable staging PUT, signed checksum and method/path binding on a real private S3 service"
    );
    tokio::time::sleep_until(begin + Duration::from_secs(63)).await;
    assert_eq!(
        invoke(&client, &get, Method::GET, None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        invoke(&client, &put, Method::PUT, Some(data)).await.0,
        StatusCode::OK
    );
    println!(
        "PASS actual 60-second GET expiry while independent 300-second staging PUT remains valid"
    );
    tokio::time::sleep_until(begin + Duration::from_secs(303)).await;
    assert_eq!(
        invoke(&client, &put, Method::PUT, Some(data)).await.0,
        StatusCode::FORBIDDEN
    );
    println!("PASS actual five-minute staging expiry; no signed URLs retained");
}
