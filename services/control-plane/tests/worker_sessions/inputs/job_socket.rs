//! Real WebSocket connection to the live Axum router, using an isolated loopback test origin.
use super::*;
use futures_util::{SinkExt, StreamExt};
use orange_protocol::job_events::{JobEventCursor, JobHistory, JobStreamControl, JobStreamFrame};
use std::time::Duration;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn open(
    url: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    protocol: &str,
) -> Result<Client, u16> {
    let mut req = url.into_client_request().expect("upgrade request");
    if let Some(v) = cookie {
        req.headers_mut()
            .insert("cookie", v.parse().expect("fixture"));
    }
    if let Some(v) = origin {
        req.headers_mut()
            .insert("origin", v.parse().expect("fixture"));
    }
    req.headers_mut()
        .insert("sec-websocket-protocol", protocol.parse().expect("fixture"));
    match connect_async(req).await {
        Ok((c, r)) => {
            assert_eq!(
                r.headers()["sec-websocket-protocol"],
                "orange.job-events.v1"
            );
            Ok(c)
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(error) => panic!("unexpected WebSocket error {error}"),
    }
}
async fn frame(client: &mut Client) -> JobStreamFrame {
    let msg = tokio::time::timeout(Duration::from_secs(4), client.next())
        .await
        .expect("event deadline")
        .expect("frame")
        .expect("socket message");
    serde_json::from_str(msg.to_text().expect("metadata text")).expect("typed event")
}
async fn closed(client: &mut Client) -> u16 {
    match tokio::time::timeout(Duration::from_secs(8), client.next())
        .await
        .expect("close deadline")
        .expect("close frame")
        .expect("socket receipt")
    {
        Message::Close(Some(reason)) => reason.code.into(),
        other => panic!("unexpected data in place of close: {other:?}"),
    }
}
async fn ack(client: &mut Client, generation: String, cursor: Option<JobEventCursor>) {
    let text = serde_json::to_string(&JobStreamControl::Ack {
        connection_generation: generation,
        cursor,
    })
    .expect("JSON");
    client.send(Message::Text(text.into())).await.expect("ack");
}
#[tokio::test]
async fn socket_checks_origin_ack_credit_cursor_generation_and_current_session_without_job_effects()
{
    let f = setup("client-ay").await;
    let cookie = f.headers["cookie"].to_str().expect("cookie");
    let origin = f.identity.config.user_origin.as_str();
    let app = orange_control_plane::artifacts::http::router(f.identity.clone(), f.objects.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!(
        "ws://{}/api/v1/owners/{}/jobs/{}/events/ws",
        listener.local_addr().expect("address"),
        f.owner,
        f.job
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let facts = || {
        sqlx::query_as::<_,(String,i64,i64)>("SELECT j.execution_state,j.state_revision,(SELECT COUNT(*) FROM ledger_transactions t WHERE t.owner_id=j.owner_id) FROM jobs j WHERE j.job_id=$1")
        .bind(&f.job).fetch_one(f.d.store.pool())
    };
    let original = facts()
        .await
        .expect("initial financial and execution state");
    const P: &str = "orange.job-events.v1";
    assert_eq!(open(&url, Some(cookie), None, P).await.err(), Some(403));
    assert_eq!(
        open(&url, Some(cookie), Some("https://untrusted.invalid"), P)
            .await
            .err(),
        Some(403)
    );
    assert_eq!(open(&url, None, Some(origin), P).await.err(), Some(401));
    assert_eq!(
        open(&url, Some(cookie), Some(origin), "unknown")
            .await
            .err(),
        Some(422)
    );
    assert_eq!(
        open(
            &(url.clone() + "?access_token=no"),
            Some(cookie),
            Some(origin),
            P
        )
        .await
        .err(),
        Some(422)
    );
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut c = open(&url, Some(cookie), Some(origin), P)
            .await
            .expect("within per-user quota");
        frame(&mut c).await;
        held.push(c);
    }
    assert_eq!(
        open(&url, Some(cookie), Some(origin), P).await.err(),
        Some(429)
    );
    for mut c in held {
        c.close(None).await.expect("release held connection");
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match open(&url, Some(cookie), Some(origin), P).await {
                Ok(mut c) => {
                    c.close(None).await.expect("probe close");
                    break;
                }
                Err(429) => tokio::time::sleep(Duration::from_millis(30)).await,
                Err(s) => panic!("unexpected quota probe status {s}"),
            }
        }
    })
    .await
    .expect("socket slots recovered");
    let mut first = open(&url, Some(cookie), Some(origin), P)
        .await
        .expect("current origin");
    let snapshot = frame(&mut first).await;
    assert_eq!(snapshot.protocol_version, 1);
    let cursor = match snapshot.history {
        JobHistory::Snapshot {
            job,
            cursor: Some(c),
            ..
        } => {
            assert_eq!(job.job_id, f.job);
            c
        }
        x => panic!("expected snapshot: {x:?}"),
    };
    first.close(None).await.expect("disconnect");
    let resume = format!("{}?stream_id={}&after=0&limit=1", url, cursor.stream_id);
    let mut second = open(&resume, Some(cookie), Some(origin), P)
        .await
        .expect("reconnect from durable sequence");
    let replay = frame(&mut second).await;
    assert_ne!(snapshot.connection_generation, replay.connection_generation);
    let next = match replay.history {
        JobHistory::Page { events, cursor, .. } => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].seq, "1");
            cursor
        }
        _ => panic!("expected replay metadata"),
    };
    ack(&mut second, snapshot.connection_generation, Some(next)).await;
    assert_eq!(closed(&mut second).await, 1008);
    let mut binary = open(&url, Some(cookie), Some(origin), P)
        .await
        .expect("binary negative");
    frame(&mut binary).await;
    binary
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .expect("send");
    assert_eq!(closed(&mut binary).await, 1008);
    let mut slow = open(&resume, Some(cookie), Some(origin), P)
        .await
        .expect("slow client");
    frame(&mut slow).await;
    assert_eq!(closed(&mut slow).await, 1008);
    sqlx::query("UPDATE outbox o SET delivered_at=clock_timestamp() FROM durable_events e WHERE e.event_id=o.domain_event_id AND e.stream_id=$1 AND e.seq=1")
        .bind(&cursor.stream_id).execute(f.d.store.pool()).await.expect("acknowledged retained event fixture");
    orange_control_plane::events::EventStore::new(f.d.store.pool().clone())
        .prune_delivered(&cursor.stream_id, 1)
        .await
        .expect("prune acknowledged sequence");
    let mut pruned = open(&resume, Some(cookie), Some(origin), P)
        .await
        .expect("resume with obsolete cursor");
    let reset = frame(&mut pruned).await;
    let JobHistory::Reset {
        job,
        cursor: Some(cover),
        ..
    } = reset.history
    else {
        panic!("expected authoritative reset")
    };
    assert_eq!(job.job_id, f.job);
    assert_eq!(job.execution_state, "queued");
    assert!(job.charge.is_none());
    assert_eq!(cover, cursor);
    ack(&mut pruned, reset.connection_generation, Some(cover)).await;
    pruned.close(None).await.expect("close reset subscriber");
    let mut live = open(
        &format!(
            "{}?stream_id={}&after={}",
            url, cursor.stream_id, cursor.after
        ),
        Some(cookie),
        Some(origin),
        P,
    )
    .await
    .expect("resume at head");
    let idle = frame(&mut live).await;
    let JobHistory::Page { events, cursor, .. } = idle.history else {
        panic!("expected initial empty page")
    };
    assert!(events.is_empty());
    ack(&mut live, idle.connection_generation, Some(cursor)).await;
    f.identity
        .revoke_self(&f.headers)
        .await
        .expect("revoke actual session");
    assert_eq!(closed(&mut live).await, 1008);
    assert_eq!(facts().await.expect("final facts"), original);
    server.abort();
}
