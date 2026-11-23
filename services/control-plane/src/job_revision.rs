//! Optimistic concurrency for Job status. Existing clients can omit the request header.
use axum::{
    Json,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use orange_protocol::JobView;

pub(crate) const EXPECTED: &str = "x-orange-expected-job-version";
pub(crate) const CURRENT: &str = "x-orange-job-version";

#[derive(Clone, Copy)]
pub(crate) struct Expected(i64);
impl Expected {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Result<Option<Self>, ()> {
        let mut values = headers.get_all(EXPECTED).iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        let raw = value.to_str().map_err(|_| ())?;
        let revision = raw.parse::<i64>().map_err(|_| ())?;
        if values.next().is_some() || revision <= 0 || revision.to_string() != raw {
            return Err(());
        }
        Ok(Some(Self(revision)))
    }
    pub(crate) fn get(self) -> i64 {
        self.0
    }
}
pub(crate) struct Status {
    pub(crate) view: JobView,
    pub(crate) revision: i64,
}
impl IntoResponse for Status {
    fn into_response(self) -> Response {
        let mut response = Json(self.view).into_response();
        response
            .headers_mut()
            .insert(CURRENT, HeaderValue::from(self.revision));
        response
    }
}
