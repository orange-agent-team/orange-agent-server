//! Bounded metadata-only projections; snapshots and replayed durable events have separate tags.
use crate::JobView;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobEventCursor {
    pub stream_id: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobEvent {
    pub event_id: String,
    pub seq: String,
    pub event_type: String,
    pub attempt_id: Option<String>,
    pub occurred_at: String,
    pub payload_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobHistory {
    Snapshot {
        job: JobView,
        job_version: String,
        cursor: Option<JobEventCursor>,
    },
    Reset {
        job: JobView,
        job_version: String,
        cursor: Option<JobEventCursor>,
    },
    Page {
        job_id: String,
        cursor: JobEventCursor,
        events: Vec<JobEvent>,
    },
}
