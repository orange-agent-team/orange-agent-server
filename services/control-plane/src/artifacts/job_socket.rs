//! One acknowledged metadata frame at a time. Disconnecting has no Job effects.
use super::super::job_events::HistoryQuery;
use super::*;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use orange_protocol::job_events::{JobEventCursor, JobHistory, JobStreamControl, JobStreamFrame};
use std::time::Duration;
use tokio::time::{Instant, timeout, timeout_at};

const PROTOCOL: &str = "orange.job-events.v1";
const FRAME_BYTES: usize = 65536;
const CLIENT_BYTES: usize = 2048;
const ACK_TIME: Duration = Duration::from_secs(5);
const POLL_TIME: Duration = Duration::from_millis(750);

pub(super) async fn upgrade(
    State(api): State<Api>,
    Path((owner, job)): Path<(String, String)>,
    headers: HeaderMap,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Result<Response, Error> {
    let actor = api.identity.artifact_actor(&headers, false).await?;
    if headers.get_all("origin").iter().count() != 1
        || headers.get("origin").and_then(|v| v.to_str().ok())
            != Some(api.identity.config.user_origin.as_str())
        || headers.contains_key("authorization")
    {
        return Err(ArtifactError::Forbidden.into());
    }
    if headers.get_all("sec-websocket-protocol").iter().count() != 1
        || headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            != Some(PROTOCOL)
    {
        return Err(ArtifactError::Invalid.into());
    }
    let Query(query) = query.map_err(|_| ArtifactError::Invalid)?;
    let ws = ws.map_err(|_| ArtifactError::Invalid)?;
    let permit = api
        .event_connections
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error(StatusCode::TOO_MANY_REQUESTS, "EVENT_STREAM_CAPACITY"))?;
    api.store.job_history(&actor, &owner, &job, &query).await?;
    Ok(ws
        .protocols([PROTOCOL])
        .max_message_size(CLIENT_BYTES)
        .max_frame_size(CLIENT_BYTES)
        .write_buffer_size(8192)
        .max_write_buffer_size(2 * FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            let _owned = permit;
            serve(socket, api.store, actor, owner, job, query).await;
        }))
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = timeout(
        Duration::from_secs(1),
        socket.send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))),
    )
    .await;
}

enum Wait {
    Control(JobStreamControl),
    Closed,
    Due,
    Invalid,
}
async fn receive_until(socket: &mut WebSocket, deadline: Instant) -> Wait {
    loop {
        if Instant::now() >= deadline {
            return Wait::Due;
        }
        match timeout_at(deadline, socket.recv()).await {
            Err(_) => return Wait::Due,
            Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return Wait::Closed,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Binary(_)))) => return Wait::Invalid,
            Ok(Some(Ok(Message::Text(text)))) => {
                return serde_json::from_str(text.as_str())
                    .map(Wait::Control)
                    .unwrap_or(Wait::Invalid);
            }
        }
    }
}
async fn serve(
    mut socket: WebSocket,
    store: ArtifactStore,
    actor: ArtifactActor,
    owner: String,
    job: String,
    mut query: HistoryQuery,
) {
    let generation = uuid::Uuid::new_v4().to_string();
    let mut no_stream_version = None;
    let mut initial = true;
    loop {
        // Never authorize a new frame from a connection's original authentication alone.
        let history = match store.job_history(&actor, &owner, &job, &query).await {
            Ok(value) => value,
            Err(ArtifactError::Forbidden | ArtifactError::NotFound | ArtifactError::Deleted) => {
                close(&mut socket, 1008, "access changed").await;
                return;
            }
            Err(ArtifactError::Invalid) => {
                close(&mut socket, 1008, "invalid cursor").await;
                return;
            }
            Err(_) => {
                close(&mut socket, 1011, "event store unavailable").await;
                return;
            }
        };
        let (cursor, send) = match &history {
            JobHistory::Page { cursor, events, .. } => (Some(cursor.clone()), !events.is_empty()),
            JobHistory::Reset { cursor, .. } => (cursor.clone(), true),
            JobHistory::Snapshot {
                job_version,
                cursor,
                ..
            } => {
                let changed = cursor.is_some() || no_stream_version.as_ref() != Some(job_version);
                no_stream_version = Some(job_version.clone());
                (cursor.clone(), changed)
            }
        };
        if send || initial {
            initial = false;
            let frame = JobStreamFrame {
                protocol_version: 1,
                connection_generation: generation.clone(),
                history,
            };
            let Ok(text) = serde_json::to_string(&frame) else {
                close(&mut socket, 1011, "invalid stored event").await;
                return;
            };
            if text.len() > FRAME_BYTES {
                close(&mut socket, 1009, "batch too large").await;
                return;
            }
            if !matches!(
                timeout(ACK_TIME, socket.send(Message::Text(text.into()))).await,
                Ok(Ok(()))
            ) {
                return;
            }
            match receive_until(&mut socket, Instant::now() + ACK_TIME).await {
                Wait::Control(JobStreamControl::Ack {
                    connection_generation,
                    cursor: ack,
                }) if connection_generation == generation && ack == cursor => {}
                Wait::Closed => return,
                Wait::Due => {
                    close(&mut socket, 1008, "acknowledgment expired").await;
                    return;
                }
                _ => {
                    close(&mut socket, 1008, "invalid acknowledgment").await;
                    return;
                }
            }
            if let Some(JobEventCursor { stream_id, after }) = cursor {
                query.stream_id = Some(stream_id);
                query.after = Some(after);
            } else {
                query.stream_id = None;
                query.after = None;
            }
            continue;
        }
        match receive_until(&mut socket, Instant::now() + POLL_TIME).await {
            Wait::Due => {}
            Wait::Closed => return,
            _ => {
                close(&mut socket, 1008, "unexpected control").await;
                return;
            }
        }
    }
}
