//! Exercise the production Range parser and HTTP transport on an owned loopback origin.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const DATA: &[u8] = b"GGUF changed origin fixture";
struct Reply {
    path: &'static str,
    offset: Option<usize>,
    status: u16,
    etag: &'static str,
    count: usize,
}
async fn serve(listener: TcpListener, replies: Vec<Reply>) {
    for reply in replies {
        let (socket, _) = listener.accept().await.expect("accept");
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("request line");
        assert_eq!(line.split_whitespace().nth(1), Some(reply.path));
        let mut range = None;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).await.expect("read header") > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((key, val)) = line.split_once(':')
                && key.eq_ignore_ascii_case("range")
            {
                range = Some(val.trim().to_owned());
            }
        }
        assert_eq!(range, reply.offset.map(|n| format!("bytes={n}-")));
        let start = if reply.status == 206 {
            reply.offset.expect("partial request")
        } else {
            0
        };
        let available = if reply.status >= 400 {
            0
        } else {
            DATA.len() - start
        };
        let mut headers = format!(
            "HTTP/1.1 {} Fixture\r\nContent-Length: {available}\r\nConnection: close\r\nETag: {}\r\n",
            reply.status, reply.etag
        );
        if reply.status == 206 {
            headers += &format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                DATA.len() - 1,
                DATA.len()
            );
        }
        let socket = reader.get_mut();
        headers += "\r\n";
        socket.write_all(headers.as_bytes()).await.expect("headers");
        if reply.count != 0 {
            socket
                .write_all(&DATA[start..start + reply.count])
                .await
                .expect("body");
        }
        socket.shutdown().await.expect("disconnect");
    }
}
fn network_spec(url: Url) -> ArtifactSpec {
    // Only this test module supplies a loopback URL. Public descriptor validation still requires HTTPS.
    let mut spec = ArtifactSpec::local_only(
        &hex::encode(Sha256::digest(DATA)),
        DATA.len() as u64,
        b"GGUF",
    )
    .expect("fixture");
    spec.url = Some(url);
    spec
}
#[tokio::test]
async fn changed_response_etag_discards_ranged_bytes_before_publication() {
    let dir = tempfile::tempdir().expect("cache");
    let mut cache = ModelCache::new(&dir.path().join("cache"), 1024).expect("cache");
    cache.retry_delay = Duration::from_millis(1);
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("owned server");
    let spec = network_spec(
        Url::parse(&format!(
            "http://{}/immutable",
            listener.local_addr().expect("port")
        ))
        .expect("url"),
    );
    let fixture = tokio::spawn(serve(
        listener,
        vec![
            Reply {
                path: "/immutable",
                offset: None,
                status: 200,
                etag: "\"first\"",
                count: 4,
            },
            Reply {
                path: "/immutable",
                offset: Some(4),
                status: 206,
                etag: "\"second\"",
                count: DATA.len() - 4,
            },
            Reply {
                path: "/immutable",
                offset: None,
                status: 200,
                etag: "\"second\"",
                count: DATA.len(),
            },
        ],
    ));
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let item = cache
        .ensure_with(&spec, &HttpTransport { client })
        .await
        .expect("verified after restart");
    fixture.await.expect("expected wire requests");
    assert_eq!(fs::read(item.path()).expect("published"), DATA);
    assert!(!item.path().with_extension("resume").exists());
}
#[tokio::test]
async fn expired_http_object_survives_restart_and_new_source_never_receives_its_prefix() {
    let dir = tempfile::tempdir().expect("cache");
    let root = dir.path().join("private");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("owned server");
    let origin = format!("http://{}", listener.local_addr().expect("port"));
    let old = network_spec(Url::parse(&(origin.clone() + "/one?token=fixture")).expect("old"));
    let fresh = network_spec(Url::parse(&(origin + "/two?token=other")).expect("new"));
    let fixture = tokio::spawn(serve(
        listener,
        vec![
            Reply {
                path: "/one?token=fixture",
                offset: None,
                status: 200,
                etag: "\"first\"",
                count: 5,
            },
            Reply {
                path: "/one?token=fixture",
                offset: Some(5),
                status: 403,
                etag: "\"expired\"",
                count: 0,
            },
            Reply {
                path: "/two?token=other",
                offset: None,
                status: 200,
                etag: "\"first\"",
                count: DATA.len(),
            },
        ],
    ));
    let transport = HttpTransport {
        client: Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client"),
    };
    {
        let mut cache = ModelCache::new(&root, 1024).expect("cache");
        cache.retry_delay = Duration::from_millis(1);
        assert!(matches!(
            cache.ensure_with(&old, &transport).await,
            Err(CacheError::Http(403))
        ));
    }
    let part = root.join(format!("{}.part", old.digest));
    assert_eq!(fs::read(&part).expect("recoverable prefix"), &DATA[..5]);
    let marker = fs::read_to_string(part.with_extension("resume")).expect("source binding");
    assert_eq!(marker.len(), 129);
    assert!(marker.split('\n').all(orange_protocol::valid_digest));
    assert!(!part.with_extension("gguf").exists());
    let cache = ModelCache::new(&root, 1024).expect("reopened cache");
    assert_eq!(
        fs::read(
            cache
                .ensure_with(&fresh, &transport)
                .await
                .expect("fresh source")
                .path()
        )
        .expect("verified artifact"),
        DATA
    );
    fixture.await.expect("expected wire requests");
}

#[tokio::test]
async fn durable_marker_allows_same_representation_and_recovers_from_range_416() {
    for reject_range in [false, true] {
        let dir = tempfile::tempdir().expect("private folder");
        let root = dir.path().join("cache");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("owned origin");
        let spec = network_spec(
            Url::parse(&format!(
                "http://{}/stable",
                listener.local_addr().expect("port")
            ))
            .expect("address"),
        );
        let mut steps = vec![
            Reply {
                path: "/stable",
                offset: None,
                status: 200,
                etag: "\"pinned\"",
                count: 6,
            },
            Reply {
                path: "/stable",
                offset: Some(6),
                status: 403,
                etag: "\"expired\"",
                count: 0,
            },
        ];
        if reject_range {
            steps.push(Reply {
                path: "/stable",
                offset: Some(6),
                status: 416,
                etag: "\"new\"",
                count: 0,
            });
            steps.push(Reply {
                path: "/stable",
                offset: None,
                status: 200,
                etag: "\"new\"",
                count: DATA.len(),
            });
        } else {
            steps.push(Reply {
                path: "/stable",
                offset: Some(6),
                status: 206,
                etag: "\"pinned\"",
                count: DATA.len() - 6,
            });
        }
        let server = tokio::spawn(serve(listener, steps));
        let transport = HttpTransport {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client"),
        };
        let open = || {
            let mut cache = ModelCache::new(&root, 1024).expect("open cache");
            cache.retry_delay = Duration::from_millis(1);
            cache
        };
        assert!(matches!(
            open().ensure_with(&spec, &transport).await,
            Err(CacheError::Http(403))
        ));
        let got = open()
            .ensure_with(&spec, &transport)
            .await
            .expect("same origin recovery");
        assert_eq!(fs::read(got.path()).expect("final data"), DATA);
        server.await.expect("wire witness");
    }
}

#[test]
fn response_binding_allows_rotating_signed_query_only_with_the_same_strong_tag() {
    let one = Url::parse("https://cdn.example/model?expires=123").expect("url");
    let two = Url::parse("https://cdn.example/model?expires=234").expect("url");
    let strong = reqwest::header::HeaderValue::from_static("\"revision\"");
    let weak = reqwest::header::HeaderValue::from_static("W/\"revision\"");
    assert_eq!(
        resume::representation(&one, Some(&strong)),
        resume::representation(&two, Some(&strong))
    );
    for etag in [None, Some(&weak)] {
        assert_ne!(
            resume::representation(&one, etag),
            resume::representation(&two, etag)
        );
    }
}

#[tokio::test]
async fn cancelling_download_preserves_other_subscribers_and_cleans_only_the_last_staging() {
    for handoff in [false, true] {
        let dir = tempfile::tempdir().expect("private folder");
        let root = dir.path().join("cache");
        let cache = ModelCache::new(&root, 4096).expect("cache");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("owned server");
        let spec = network_spec(
            Url::parse(&format!(
                "http://{}/model",
                listener.local_addr().expect("port")
            ))
            .expect("url"),
        );
        let other = root.join(format!("{}.part", "a".repeat(64)));
        fs::write(&other, b"unrelated").expect("other request");
        let ready_other = root.join(format!("{}.gguf", "b".repeat(64)));
        fs::write(&ready_other, b"another model").expect("unrelated publication");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("first connection");
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).await.expect("headers") > 0);
                if line == "\r\n" {
                    break;
                }
            }
            let socket = reader.get_mut();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"same\"\r\nConnection: close\r\n\r\n",DATA.len()).as_bytes()).await.expect("headers");
            socket.write_all(&DATA[..4]).await.expect("prefix");
            let mut one = [0];
            let end = tokio::time::timeout(Duration::from_secs(4), socket.read(&mut one))
                .await
                .expect("cancel closed connection");
            assert!(
                matches!(end, Ok(0) | Err(_)),
                "cancelled connection received another request"
            );
            if handoff {
                serve(
                    listener,
                    vec![Reply {
                        path: "/model",
                        offset: Some(4),
                        status: 206,
                        etag: "\"same\"",
                        count: DATA.len() - 4,
                    }],
                )
                .await;
            }
        });
        let transport = HttpTransport {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(6))
                .build()
                .expect("client"),
        };
        let part = root.join(format!("{}.part", spec.digest));
        let marker = part.with_extension("resume");
        let (first_stop, first_rx) = tokio::sync::watch::channel(false);
        let first = cache.subscribed(&spec, &transport, first_rx);
        tokio::pin!(first);
        let have_prefix = async {
            while !fs::metadata(&part).is_ok_and(|m| m.len() == 4) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::select! {_=&mut first=>panic!("transfer finished early"),result=tokio::time::timeout(Duration::from_secs(4),have_prefix)=>result.expect("durable staged data")};
        assert_eq!(
            cache.discard_staging(&spec).expect("observe subscriptions"),
            StagingCleanup::RetainedForSubscribers
        );
        let (other_stop, other_rx) = tokio::sync::watch::channel(false);
        let waiter = cache.subscribed(&spec, &transport, other_rx);
        tokio::pin!(waiter);
        // Poll once: the independent subscriber obtains its shared file lock before it waits for the writer.
        tokio::select! {biased;_=&mut waiter=>panic!("writer was already occupied"),_=std::future::ready(())=>{}}
        if handoff {
            first_stop.send_replace(true);
            assert!(matches!(first.await, Err(CacheError::Cancelled)));
            assert_eq!(
                fs::read(&part).expect("remaining subscriber needs this prefix"),
                &DATA[..4]
            );
            let verified = waiter
                .await
                .expect("remaining subscriber acquired the bytes");
            assert_eq!(fs::read(verified.path()).expect("published content"), DATA);
            assert_eq!(
                cache.discard_staging(&spec).expect("delete staging"),
                StagingCleanup::Released {
                    download_bytes: 0,
                    import_bytes: 0
                }
            );
            assert!(matches!(
                ModelCache::observe(&root, &spec)
                    .await
                    .expect("active read lease")
                    .read_lease_observed,
                Some(true)
            ));
        } else {
            other_stop.send_replace(true);
            assert!(matches!(waiter.await, Err(CacheError::Cancelled)));
            assert!(marker.is_file() && part.is_file());
            first_stop.send_replace(true);
            assert!(matches!(first.await, Err(CacheError::Cancelled)));
            assert!(!part.with_extension("gguf").exists());
        }
        server.await.expect("server observed original cancellation");
        assert!(!part.exists() && !marker.exists());
        assert_eq!(
            fs::read(other).expect("unrelated staging retained"),
            b"unrelated"
        );
        assert_eq!(
            fs::read(ready_other).expect("unrelated publication retained"),
            b"another model"
        );
    }
}
